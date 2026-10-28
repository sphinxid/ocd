//! Google Application Default Credentials (`google-auth-library` 10.5.0 as
//! `google-vertex-shared.ts` uses it) against local stand-ins for the OAuth2 token endpoint, Google
//! STS, IAM credentials, the GCE metadata server and AWS metadata. Every case has its own
//! environment (`HOME`, `CLOUDSDK_CONFIG`, ...) in a temporary directory.

#[path = "common/http_mock.rs"]
mod http_mock;

use base64::Engine;
use http_mock::{Resp, closed_port, mock};
use oc_ai::protocols::cloud_env::Env;
use oc_ai::protocols::gemini::adc::{Adc, Endpoints, SCOPE};
use serde_json::{Value, json};
use std::path::PathBuf;

struct Fixture {
    dir: tempfile::TempDir,
    vars: Vec<(String, String)>,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();
        let vars = vec![
            ("HOME".to_string(), home.clone()),
            ("CLOUDSDK_CONFIG".to_string(), format!("{home}/gcloud")),
            ("METADATA_SERVER_DETECTION".to_string(), "none".to_string()),
        ];
        Fixture { dir, vars }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
    fn file(&self, name: &str, text: &str) -> String {
        let path = self.path(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path.to_string_lossy().to_string()
    }
    fn set(&mut self, name: &str, value: impl Into<String>) -> &mut Fixture {
        self.vars.retain(|(k, _)| k != name);
        self.vars.push((name.into(), value.into()));
        self
    }
    fn unset(&mut self, name: &str) -> &mut Fixture {
        self.vars.retain(|(k, _)| k != name);
        self
    }
    /// Writes the ADC file named by `GOOGLE_APPLICATION_CREDENTIALS`.
    fn adc(&mut self, value: Value) -> &mut Fixture {
        let path = self.file("adc.json", &value.to_string());
        self.set("GOOGLE_APPLICATION_CREDENTIALS", path)
    }
    fn auth(&self, endpoints: Endpoints) -> Adc {
        Adc::new(Env::from_map(self.vars.clone()), endpoints)
    }
}

fn oauth2(base: &str) -> Endpoints {
    Endpoints { oauth2_token: Some(format!("{base}/token")), ..Default::default() }
}

#[tokio::test]
async fn authorized_user_and_token_cache() {
    let server = mock(|request, n| {
        assert_eq!(request.path, "/token");
        Resp::json(200, json!({"access_token": format!("user-{n}"), "expires_in": 3599, "token_type": "Bearer"}))
    })
    .await;
    let mut f = Fixture::new();
    f.adc(json!({"type": "authorized_user", "client_id": "cid", "client_secret": "cs", "refresh_token": "rt", "quota_project_id": "q"}));
    let adc = f.auth(oauth2(&server.base));
    assert_eq!(adc.token().await.unwrap(), "user-0");
    assert_eq!(adc.token().await.unwrap(), "user-0");
    let calls = server.requests();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].form(),
        [("refresh_token", "rt"), ("client_id", "cid"), ("client_secret", "cs"), ("grant_type", "refresh_token")].map(|(k, v)| (k.to_string(), v.to_string()))
    );
    // A token that expires within 5 minutes is refreshed.
    let short = mock(|_, n| Resp::json(200, json!({"access_token": format!("short-{n}"), "expires_in": 120}))).await;
    let adc = f.auth(oauth2(&short.base));
    assert_eq!(adc.token().await.unwrap(), "short-0");
    assert_eq!(adc.token().await.unwrap(), "short-1");
}

#[tokio::test]
async fn well_known_file_and_cloudsdk_config() {
    let server = mock(|_, _| Resp::json(200, json!({"access_token": "from-gcloud", "expires_in": 3600}))).await;
    let mut f = Fixture::new();
    let user = json!({"type": "authorized_user", "client_id": "c", "client_secret": "s", "refresh_token": "r"}).to_string();
    // CLOUDSDK_CONFIG names the directory; HOME/.config/gcloud is not consulted then.
    f.file(".config/gcloud/application_default_credentials.json", &user);
    let missing = f.auth(oauth2(&server.base)).token().await.unwrap_err();
    assert!(missing.starts_with("Could not load the default credentials."), "{missing}");
    f.file("gcloud/application_default_credentials.json", &user);
    assert_eq!(f.auth(oauth2(&server.base)).token().await.unwrap(), "from-gcloud");
    // Without CLOUDSDK_CONFIG: $HOME/.config/gcloud.
    std::fs::remove_file(f.path("gcloud/application_default_credentials.json")).unwrap();
    f.unset("CLOUDSDK_CONFIG");
    assert_eq!(f.auth(oauth2(&server.base)).token().await.unwrap(), "from-gcloud");
    // A failed load stays failed for that auth instance, like upstream's cached client promise.
    let mut g = Fixture::new();
    g.set("GOOGLE_APPLICATION_CREDENTIALS", g.path("later.json").to_string_lossy().to_string());
    let adc = g.auth(oauth2(&server.base));
    assert!(adc.token().await.is_err());
    g.file("later.json", &user);
    assert!(adc.token().await.is_err());
    assert_eq!(g.auth(oauth2(&server.base)).token().await.unwrap(), "from-gcloud");
}

#[tokio::test]
async fn service_account_assertion() {
    use rsa::pkcs8::EncodePrivateKey;
    let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 1024).unwrap();
    let pem = key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap().to_string();
    let public = key.to_public_key();
    let server = mock(|_, _| Resp::json(200, json!({"access_token": "sa-token", "expires_in": 3600, "token_type": "Bearer"}))).await;
    let mut f = Fixture::new();
    f.adc(json!({"type": "service_account", "project_id": "p", "private_key_id": "kid", "private_key": pem, "client_email": "sa@p.iam.gserviceaccount.com", "token_uri": "https://ignored.example/token"}));
    assert_eq!(f.auth(oauth2(&server.base)).token().await.unwrap(), "sa-token");
    let call = &server.requests()[0];
    assert_eq!(call.field("grant_type").as_deref(), Some("urn:ietf:params:oauth:grant-type:jwt-bearer"));
    let assertion = call.field("assertion").unwrap();
    let parts: Vec<&str> = assertion.split('.').collect();
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    assert_eq!(b64.decode(parts[0]).unwrap(), br#"{"alg":"RS256"}"#);
    let claims: serde_json::Map<String, Value> = serde_json::from_slice(&b64.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(claims.keys().collect::<Vec<_>>(), ["iss", "scope", "aud", "exp", "iat"]);
    assert_eq!(claims["aud"], "https://oauth2.googleapis.com/token");
    assert_eq!(claims["scope"], SCOPE);
    use rsa::signature::Verifier;
    let verifier = rsa::pkcs1v15::VerifyingKey::<sha2::Sha256>::new(public);
    let signature = rsa::pkcs1v15::Signature::try_from(b64.decode(parts[2]).unwrap().as_slice()).unwrap();
    verifier.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature).unwrap();
}

fn sts_server_routes() -> impl Fn(&http_mock::Req, usize) -> Resp + Send + Sync + 'static {
    |request, n| match request.path.as_str() {
        "/v1/token" => Resp::json(
            200,
            json!({"access_token": format!("sts-{n}"), "issued_token_type": "urn:ietf:params:oauth:token-type:access_token", "token_type": "Bearer", "expires_in": 3600}),
        ),
        "/v1/projects/-/serviceAccounts/sa@p.iam.gserviceaccount.com:generateAccessToken" => {
            Resp::json(200, json!({"accessToken": format!("impersonated-{n}"), "expireTime": "2099-01-01T00:00:00Z"}))
        }
        "/subject" => Resp::json(200, json!({"value": "url-subject-token"})),
        _ => Resp::text(404, ""),
    }
}

const AUDIENCE: &str = "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/pool/providers/prov";

#[tokio::test]
async fn identity_pool_file_source() {
    let server = mock(sts_server_routes()).await;
    let mut f = Fixture::new();
    let subject = f.file("subject.txt", "file-subject-token");
    f.adc(json!({
        "type": "external_account",
        "audience": AUDIENCE,
        "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
        "token_url": format!("{}/v1/token", server.base),
        "credential_source": {"file": subject},
    }));
    let adc = f.auth(Endpoints::default());
    assert_eq!(adc.token().await.unwrap(), "sts-0");
    assert_eq!(adc.token().await.unwrap(), "sts-0");
    let call = &server.requests()[0];
    assert_eq!(
        call.form(),
        [
            ("grant_type", "urn:ietf:params:oauth:grant-type:token-exchange"),
            ("audience", AUDIENCE),
            ("scope", SCOPE),
            ("requested_token_type", "urn:ietf:params:oauth:token-type:access_token"),
            ("subject_token", "file-subject-token"),
            ("subject_token_type", "urn:ietf:params:oauth:token-type:jwt"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()))
    );
    assert_eq!(call.header("x-goog-api-client"), Some("gl-node/26.3.0 auth/10.5.0 google-byoid-sdk source/file sa-impersonation/false config-lifetime/false"));
    assert_eq!(call.header("authorization"), None);
}

#[tokio::test]
async fn identity_pool_url_source_with_impersonation() {
    let server = mock(sts_server_routes()).await;
    let mut f = Fixture::new();
    f.adc(json!({
        "type": "external_account",
        "audience": AUDIENCE,
        "subject_token_type": "urn:ietf:params:oauth:token-type:id_token",
        "token_url": format!("{}/v1/token", server.base),
        "service_account_impersonation_url": format!("{}/v1/projects/-/serviceAccounts/sa@p.iam.gserviceaccount.com:generateAccessToken", server.base),
        "service_account_impersonation": {"token_lifetime_seconds": 600},
        "credential_source": {"url": format!("{}/subject", server.base), "headers": {"Metadata": "True"}, "format": {"type": "json", "subject_token_field_name": "value"}},
    }));
    assert_eq!(f.auth(Endpoints::default()).token().await.unwrap(), "impersonated-0");
    let subject = &server.requests_to("/subject")[0];
    assert_eq!(subject.header("metadata"), Some("True"));
    let exchange = &server.requests_to("/v1/token")[0];
    assert_eq!(exchange.field("subject_token").as_deref(), Some("url-subject-token"));
    assert_eq!(exchange.field("scope").as_deref(), Some(SCOPE));
    assert!(exchange.header("x-goog-api-client").unwrap().ends_with("source/url sa-impersonation/true config-lifetime/true"));
    let impersonation = &server.requests_to("/v1/projects/-/serviceAccounts/sa@p.iam.gserviceaccount.com:generateAccessToken")[0];
    assert_eq!(impersonation.header("authorization"), Some("Bearer sts-0"));
    assert_eq!(impersonation.json(), json!({"scope": [SCOPE], "lifetime": "600s"}));
}

#[tokio::test]
async fn workforce_pools_and_client_auth() {
    let server = mock(sts_server_routes()).await;
    let workforce = "//iam.googleapis.com/locations/global/workforcePools/wf/providers/p";
    let mut f = Fixture::new();
    let subject = f.file("subject.json", r#"{"id_token": "json-subject"}"#);
    let base = json!({
        "type": "external_account",
        "audience": workforce,
        "subject_token_type": "urn:ietf:params:oauth:token-type:id_token",
        "token_url": format!("{}/v1/token", server.base),
        "workforce_pool_user_project": "user-project",
        "credential_source": {"file": subject, "format": {"type": "json", "subject_token_field_name": "id_token"}},
    });
    f.adc(base.clone());
    f.auth(Endpoints::default()).token().await.unwrap();
    let call = &server.requests()[0];
    assert_eq!(call.field("subject_token").as_deref(), Some("json-subject"));
    assert_eq!(call.field("options").as_deref(), Some(r#"{"userProject":"user-project"}"#));
    // With a client id: Basic auth and no options.
    let mut with_client = base.clone();
    with_client["client_id"] = json!("cid");
    with_client["client_secret"] = json!("csecret");
    f.adc(with_client);
    f.auth(Endpoints::default()).token().await.unwrap();
    let call = &server.requests()[1];
    assert_eq!(call.field("options"), None);
    assert_eq!(call.header("authorization"), Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("cid:csecret")).as_str()));
    // The workforce project is refused for a workload audience.
    let mut wrong = base.clone();
    wrong["audience"] = json!(AUDIENCE);
    f.adc(wrong);
    assert!(f.auth(Endpoints::default()).token().await.unwrap_err().contains("workforcePoolUserProject"));
}

#[tokio::test]
async fn aws_source() {
    let server = mock(|request, n| match (request.method.as_str(), request.path.as_str()) {
        ("PUT", "/latest/api/token") => Resp::text(200, "imds-v2"),
        ("GET", "/latest/meta-data/placement/availability-zone") => Resp::text(200, "us-east-2b"),
        ("GET", "/latest/meta-data/iam/security-credentials") => Resp::text(200, "the-role"),
        ("GET", "/latest/meta-data/iam/security-credentials/the-role") => {
            Resp::json(200, json!({"AccessKeyId": "AKROLE", "SecretAccessKey": "SKROLE", "Token": "TROLE"}))
        }
        ("POST", "/v1/token") => Resp::json(200, json!({"access_token": format!("aws-sts-{n}"), "expires_in": 3600})),
        _ => Resp::text(404, ""),
    })
    .await;
    let mut f = Fixture::new();
    let config = json!({
        "type": "external_account",
        "audience": AUDIENCE,
        "subject_token_type": "urn:ietf:params:aws:token-type:aws4_request",
        "token_url": format!("{}/v1/token", server.base),
        "credential_source": {
            "environment_id": "aws1",
            "region_url": format!("{}/latest/meta-data/placement/availability-zone", server.base),
            "url": format!("{}/latest/meta-data/iam/security-credentials", server.base),
            "regional_cred_verification_url": "https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15",
            "imdsv2_session_token_url": format!("{}/latest/api/token", server.base),
        },
    });
    f.adc(config.clone());
    assert_eq!(f.auth(Endpoints::default()).token().await.unwrap(), "aws-sts-0");
    let metadata: Vec<_> = server.requests().into_iter().filter(|r| r.path.starts_with("/latest/meta-data")).collect();
    assert!(metadata.iter().all(|r| r.header("x-aws-ec2-metadata-token") == Some("imds-v2")));
    let token = server.requests_to("/v1/token")[0].field("subject_token").unwrap();
    let decoded: Value = serde_json::from_str(&percent_encoding::percent_decode_str(&token).decode_utf8().unwrap()).unwrap();
    assert_eq!(decoded["url"], "https://sts.us-east-2.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15");
    assert_eq!(decoded["method"], "POST");
    let headers: Vec<(String, String)> =
        decoded["headers"].as_array().unwrap().iter().map(|h| (h["key"].as_str().unwrap().into(), h["value"].as_str().unwrap().into())).collect();
    let names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(names, ["authorization", "host", "x-amz-date", "x-amz-security-token", "x-goog-cloud-target-resource"]);
    assert!(headers[0].1.starts_with("AWS4-HMAC-SHA256 Credential=AKROLE/"));
    assert!(headers[0].1.contains("/us-east-2/sts/aws4_request, SignedHeaders=host;x-amz-date;x-amz-security-token, Signature="));
    assert_eq!(headers[1].1, "sts.us-east-2.amazonaws.com");
    assert_eq!(headers[3].1, "TROLE");
    assert_eq!(headers[4].1, AUDIENCE);
    // Region and keys from the environment: no metadata requests.
    let before = server.requests().len();
    f.set("AWS_REGION", "eu-west-1").set("AWS_ACCESS_KEY_ID", "AKENV").set("AWS_SECRET_ACCESS_KEY", "SKENV");
    assert_eq!(f.auth(Endpoints::default()).token().await.unwrap(), "aws-sts-1");
    let calls = server.requests();
    assert_eq!(calls.len(), before + 1);
    let token = calls.last().unwrap().field("subject_token").unwrap();
    let decoded: Value = serde_json::from_str(&percent_encoding::percent_decode_str(&token).decode_utf8().unwrap()).unwrap();
    let keys: Vec<&str> = decoded["headers"].as_array().unwrap().iter().map(|h| h["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["authorization", "host", "x-amz-date", "x-goog-cloud-target-resource"]);
    assert!(decoded["headers"][0]["value"].as_str().unwrap().contains("Credential=AKENV/") && decoded["url"].as_str().unwrap().contains("eu-west-1"));
}

#[tokio::test]
async fn executable_source() {
    let server = mock(sts_server_routes()).await;
    let mut f = Fixture::new();
    let seen = f.path("seen-env");
    let script = f.file(
        "exec.sh",
        &format!(
            "#!/bin/sh\nenv | grep GOOGLE_EXTERNAL_ACCOUNT | sort > '{}'\necho '{{\"version\": 1, \"success\": true, \"token_type\": \"urn:ietf:params:oauth:token-type:id_token\", \"id_token\": \"exec-token\", \"expiration_time\": 4102444800}}'\n",
            seen.display()
        ),
    );
    let output = f.path("exec-cache.json").to_string_lossy().to_string();
    let config = json!({
        "type": "external_account",
        "audience": AUDIENCE,
        "subject_token_type": "urn:ietf:params:oauth:token-type:id_token",
        "token_url": format!("{}/v1/token", server.base),
        "service_account_impersonation_url": format!("{}/v1/projects/-/serviceAccounts/sa@p.iam.gserviceaccount.com:generateAccessToken", server.base),
        "credential_source": {"executable": {"command": format!("/bin/sh {script}"), "timeout_millis": 5000, "output_file": output}},
    });
    f.adc(config.clone());
    // Executables must be allowed explicitly.
    assert!(f.auth(Endpoints::default()).token().await.unwrap_err().contains("GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES"));
    f.set("GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES", "1");
    assert_eq!(f.auth(Endpoints::default()).token().await.unwrap(), "impersonated-0");
    assert_eq!(server.requests_to("/v1/token")[0].field("subject_token").as_deref(), Some("exec-token"));
    let env = std::fs::read_to_string(&seen).unwrap();
    assert_eq!(
        env.lines().collect::<Vec<_>>(),
        [
            "GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES=1".to_string(),
            format!("GOOGLE_EXTERNAL_ACCOUNT_AUDIENCE={AUDIENCE}"),
            "GOOGLE_EXTERNAL_ACCOUNT_IMPERSONATED_EMAIL=sa@p.iam.gserviceaccount.com".to_string(),
            "GOOGLE_EXTERNAL_ACCOUNT_INTERACTIVE=0".to_string(),
            format!("GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE={output}"),
            "GOOGLE_EXTERNAL_ACCOUNT_TOKEN_TYPE=urn:ietf:params:oauth:token-type:id_token".to_string(),
        ]
    );
    // A valid response in the output file is used without running the executable.
    std::fs::remove_file(&seen).unwrap();
    std::fs::write(
        &output,
        json!({"version": 1, "success": true, "token_type": "urn:ietf:params:oauth:token-type:jwt", "id_token": "cached-token", "expiration_time": 4102444800u64}).to_string(),
    )
    .unwrap();
    f.auth(Endpoints::default()).token().await.unwrap();
    assert_eq!(server.requests_to("/v1/token")[1].field("subject_token").as_deref(), Some("cached-token"));
    assert!(!seen.exists());
    // A failure reported by the executable.
    std::fs::remove_file(&output).unwrap();
    f.file("exec.sh", "#!/bin/sh\necho '{\"version\": 1, \"success\": false, \"code\": \"401\", \"message\": \"denied\"}'\n");
    let error = f.auth(Endpoints::default()).token().await.unwrap_err();
    assert_eq!(error, "The executable failed with exit code: 401 and error message: denied.");
}

#[tokio::test]
async fn impersonated_service_account() {
    let server = mock(|request, _| match request.path.as_str() {
        "/token" => Resp::json(200, json!({"access_token": "source-token", "expires_in": 3600})),
        "/v1/projects/-/serviceAccounts/target@p.iam.gserviceaccount.com:generateAccessToken" => {
            Resp::json(200, json!({"accessToken": "target-token", "expireTime": "2099-01-01T00:00:00Z"}))
        }
        _ => Resp::text(404, ""),
    })
    .await;
    let mut f = Fixture::new();
    f.adc(json!({
        "type": "impersonated_service_account",
        "service_account_impersonation_url": "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/target@p.iam.gserviceaccount.com:generateAccessToken",
        "delegates": ["projects/-/serviceAccounts/d@p.iam.gserviceaccount.com"],
        "source_credentials": {"type": "authorized_user", "client_id": "c", "client_secret": "s", "refresh_token": "r", "quota_project_id": "billing"},
    }));
    let endpoints = Endpoints { oauth2_token: Some(format!("{}/token", server.base)), iam_credentials: Some(server.base.clone()), ..Default::default() };
    let adc = f.auth(endpoints);
    assert_eq!(adc.token().await.unwrap(), "target-token");
    assert_eq!(adc.token().await.unwrap(), "target-token");
    let calls = server.requests_to("/v1/projects/-/serviceAccounts/target@p.iam.gserviceaccount.com:generateAccessToken");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].header("authorization"), Some("Bearer source-token"));
    assert_eq!(calls[0].header("x-goog-user-project"), Some("billing"));
    assert_eq!(calls[0].json(), json!({"delegates": ["projects/-/serviceAccounts/d@p.iam.gserviceaccount.com"], "scope": [SCOPE], "lifetime": "3600s"}));
}

#[tokio::test]
async fn external_account_authorized_user() {
    let server = mock(|_, n| Resp::json(200, json!({"access_token": format!("eau-{n}"), "expires_in": 3600, "refresh_token": format!("rotated-{n}")}))).await;
    let mut f = Fixture::new();
    f.adc(json!({
        "type": "external_account_authorized_user",
        "audience": "//iam.googleapis.com/locations/global/workforcePools/wf/providers/p",
        "refresh_token": "first",
        "token_url": format!("{}/v1/oauthtoken", server.base),
        "client_id": "cid",
        "client_secret": "cs",
    }));
    assert_eq!(f.auth(Endpoints::default()).token().await.unwrap(), "eau-0");
    let call = &server.requests()[0];
    assert_eq!(call.path, "/v1/oauthtoken");
    assert_eq!(call.form(), [("grant_type", "refresh_token"), ("refresh_token", "first")].map(|(k, v)| (k.to_string(), v.to_string())));
    assert_eq!(call.header("authorization"), Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("cid:cs")).as_str()));
}

#[tokio::test]
async fn compute_metadata_server() {
    let server = mock(|request, _| {
        assert_eq!(request.header("metadata-flavor"), Some("Google"));
        match request.path.as_str() {
            "/computeMetadata/v1/instance" => Resp::text(200, "").with("Metadata-Flavor", "Google"),
            "/computeMetadata/v1/instance/service-accounts/default/token" => {
                Resp::json(200, json!({"access_token": "gce-token", "expires_in": 3599, "token_type": "Bearer"})).with("Metadata-Flavor", "Google")
            }
            _ => Resp::text(404, ""),
        }
    })
    .await;
    let mut f = Fixture::new();
    f.set("GCE_METADATA_HOST", format!("127.0.0.1:{}", server.port)).set("METADATA_SERVER_DETECTION", "ping-only");
    assert_eq!(f.auth(Endpoints::default()).token().await.unwrap(), "gce-token");
    let token = &server.requests_to("/computeMetadata/v1/instance/service-accounts/default/token")[0];
    assert_eq!(token.param("scopes").as_deref(), Some(SCOPE));
    // Nothing listening: not on GCE.
    let mut f = Fixture::new();
    f.set("GCE_METADATA_HOST", format!("127.0.0.1:{}", closed_port())).set("METADATA_SERVER_DETECTION", "ping-only");
    assert!(f.auth(Endpoints::default()).token().await.unwrap_err().starts_with("Could not load the default credentials."));
}

/// A TLS server that requires a client certificate signed by `ca` and answers STS exchanges.
fn mtls_sts(
    server_cert: rustls::pki_types::CertificateDer<'static>,
    server_key: rustls::pki_types::PrivateKeyDer<'static>,
    client_ca: rustls::pki_types::CertificateDer<'static>,
) -> (u16, std::sync::Arc<parking_lot::Mutex<Vec<String>>>) {
    use std::io::{Read, Write};
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.add(client_ca).unwrap();
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(std::sync::Arc::new(roots), provider.clone()).build().unwrap();
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![server_cert], server_key)
        .unwrap();
    let config = std::sync::Arc::new(config);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let recorded = bodies.clone();
    std::thread::spawn(move || {
        for socket in listener.incoming().flatten() {
            let config = config.clone();
            let recorded = recorded.clone();
            std::thread::spawn(move || {
                let connection = rustls::ServerConnection::new(config).unwrap();
                let mut stream = rustls::StreamOwned::new(connection, socket);
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let Ok(n) = stream.read(&mut chunk) else { return };
                    if n == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buffer).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                            .unwrap_or(0);
                        if buffer.len() >= end + 4 + length {
                            recorded.lock().push(text[end + 4..].to_string());
                            break;
                        }
                    }
                }
                let body = r#"{"access_token":"mtls-token","expires_in":3600}"#;
                let _ =
                    write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = stream.flush();
                stream.conn.send_close_notify();
                let _ = stream.flush();
            });
        }
    });
    (port, bodies)
}

#[tokio::test]
async fn certificate_source_uses_mutual_tls() {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.distinguished_name.push(DnType::CommonName, "ocd test CA");
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca = ca.self_signed(&ca_key).unwrap();
    let server_key = KeyPair::generate().unwrap();
    let server = CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap().signed_by(&server_key, &ca, &ca_key).unwrap();
    let client_key = KeyPair::generate().unwrap();
    let mut client_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    client_params.distinguished_name.push(DnType::CommonName, "workload");
    let client = client_params.signed_by(&client_key, &ca, &ca_key).unwrap();
    let (port, bodies) = mtls_sts(server.der().clone(), PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())), ca.der().clone());

    let mut f = Fixture::new();
    let ca_file = f.file("ca.pem", &ca.pem());
    let cert_file = f.file("workload.pem", &client.pem());
    let key_file = f.file("workload.key", &client_key.serialize_pem());
    let config = f.file("certificate_config.json", &json!({"cert_configs": {"workload": {"cert_path": cert_file, "key_path": key_file}}}).to_string());
    f.adc(json!({
        "type": "external_account",
        "audience": AUDIENCE,
        "subject_token_type": "urn:ietf:params:oauth:token-type:mtls",
        "token_url": format!("https://127.0.0.1:{port}/v1/token"),
        "credential_source": {"certificate": {"certificate_config_location": config}},
    }));
    let trust = oc_util::http::Trust::load_with(
        |name| {
            if name == "SSL_CERT_FILE" {
                Some(std::ffi::OsString::from(&ca_file))
            } else {
                None
            }
        },
        &[],
        &[],
    );
    let endpoints = Endpoints { trust: Some(trust), ..Default::default() };
    assert_eq!(f.auth(endpoints).token().await.unwrap(), "mtls-token");
    let body = bodies.lock()[0].clone();
    let subject: String = url::form_urlencoded::parse(body.as_bytes()).find(|(k, _)| k == "subject_token").unwrap().1.into_owned();
    assert_eq!(subject, json!([base64::engine::general_purpose::STANDARD.encode(client.der())]).to_string());
}
