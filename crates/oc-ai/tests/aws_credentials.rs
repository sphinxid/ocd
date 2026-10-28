//! The AWS default credential chain (`fromNodeProviderChain` of `@aws-sdk/credential-providers`
//! 3.1057.0) against local stand-ins for STS, SSO, SSO-OIDC, sign-in, IMDS and the container
//! endpoint. Every case runs with its own environment: `HOME`, `AWS_CONFIG_FILE` and
//! `AWS_SHARED_CREDENTIALS_FILE` in a temporary directory, and instance metadata disabled unless
//! the case points it at a mock.

#[path = "common/http_mock.rs"]
mod http_mock;

use http_mock::{Req, Resp, closed_port, mock};
use oc_ai::protocols::bedrock_converse::aws::{default_chain_with, dpop_proof};
use oc_ai::protocols::bedrock_converse::sigv4::AwsCredentials;
use oc_ai::protocols::cloud_env::Env;
use oc_ai::route::auth::AuthFailure;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

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
            ("AWS_CONFIG_FILE".to_string(), format!("{home}/config")),
            ("AWS_SHARED_CREDENTIALS_FILE".to_string(), format!("{home}/credentials")),
            ("AWS_EC2_METADATA_DISABLED".to_string(), "true".to_string()),
        ];
        Fixture { dir, vars }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
    fn write(&self, name: &str, text: &str) -> &Fixture {
        let path = self.path(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        self
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
    fn env(&self) -> Env {
        Env::from_map(self.vars.clone())
    }
}

async fn resolve(env: &Env, profile: Option<&str>) -> Result<AwsCredentials, String> {
    default_chain_with(env, "us-east-1", profile).await.map_err(|e| match e {
        AuthFailure::Error(e) => e.message.clone(),
        AuthFailure::Missing { message, .. } => message,
    })
}

fn keys(c: &AwsCredentials) -> (&str, &str, Option<&str>) {
    (c.access_key_id.as_str(), c.secret_access_key.as_str(), c.session_token.as_deref())
}

const NO_CREDENTIALS: &str = "AWS default credential chain failed: Could not load credentials from any providers";

fn sts_credentials(action: &str, n: usize) -> Resp {
    Resp::xml(
        200,
        format!(
            "<{action}Response xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><{action}Result><Credentials><AccessKeyId>ASIA{n}</AccessKeyId><SecretAccessKey>secret{n}</SecretAccessKey><SessionToken>token{n}</SessionToken><Expiration>2099-01-01T00:00:00Z</Expiration></Credentials><AssumedRoleUser><Arn>arn:aws:sts::123456789012:assumed-role/r/s</Arn><AssumedRoleId>AROA:s</AssumedRoleId></AssumedRoleUser></{action}Result></{action}Response>"
        ),
    )
}

/// `Credential=<key>/<date>/<region>/<service>/aws4_request` of a SigV4 `authorization` header.
fn credential_scope(request: &Req) -> Option<(String, String, String)> {
    let auth = request.header("authorization")?;
    let credential = auth.split("Credential=").nth(1)?.split(',').next()?;
    let parts: Vec<&str> = credential.split('/').collect();
    Some((parts[0].to_string(), parts[2].to_string(), parts[3].to_string()))
}

#[tokio::test]
async fn environment_variables_unless_a_profile_is_set() {
    let mut f = Fixture::new();
    f.set("AWS_ACCESS_KEY_ID", "AKENV").set("AWS_SECRET_ACCESS_KEY", "SKENV").set("AWS_SESSION_TOKEN", "TENV");
    let c = resolve(&f.env(), None).await.unwrap();
    assert_eq!(keys(&c), ("AKENV", "SKENV", Some("TENV")));
    assert_eq!(c.region, "us-east-1");
    // A profile (option or AWS_PROFILE) skips the environment provider.
    f.write("credentials", "[work]\naws_access_key_id = AKWORK\naws_secret_access_key = SKWORK\n");
    assert_eq!(keys(&resolve(&f.env(), Some("work")).await.unwrap()), ("AKWORK", "SKWORK", None));
    f.set("AWS_PROFILE", "work");
    assert_eq!(keys(&resolve(&f.env(), None).await.unwrap()), ("AKWORK", "SKWORK", None));
    // An empty AWS_PROFILE does not count.
    f.set("AWS_PROFILE", "");
    assert_eq!(keys(&resolve(&f.env(), None).await.unwrap()), ("AKENV", "SKENV", Some("TENV")));
}

#[tokio::test]
async fn shared_files_static_keys() {
    let f = Fixture::new();
    f.write("config", "[default]\naws_access_key_id = AKCONFIG\naws_secret_access_key = SKCONFIG\n[profile dev] ; comment\naws_access_key_id=AKDEV\naws_secret_access_key=SKDEV\naws_session_token=TDEV\n");
    f.write("credentials", "[default]\naws_access_key_id = AKCRED\n");
    // The credentials file wins key by key over the config file.
    assert_eq!(keys(&resolve(&f.env(), None).await.unwrap()), ("AKCRED", "SKCONFIG", None));
    assert_eq!(keys(&resolve(&f.env(), Some("dev")).await.unwrap()), ("AKDEV", "SKDEV", Some("TDEV")));
}

#[tokio::test]
async fn nothing_configured_fails_without_touching_the_network() {
    let f = Fixture::new();
    let started = std::time::Instant::now();
    assert_eq!(resolve(&f.env(), None).await.unwrap_err(), NO_CREDENTIALS);
    // Instance metadata at a closed port: the IMDSv2 token and the v1 fallback both fail fast.
    let mut f = Fixture::new();
    f.unset("AWS_EC2_METADATA_DISABLED").set("AWS_EC2_METADATA_SERVICE_ENDPOINT", format!("http://127.0.0.1:{}", closed_port()));
    assert_eq!(resolve(&f.env(), None).await.unwrap_err(), NO_CREDENTIALS);
    // A profile that does not exist ends the same way.
    assert_eq!(resolve(&f.env(), Some("missing")).await.unwrap_err(), NO_CREDENTIALS);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[tokio::test]
async fn credential_process() {
    let f = Fixture::new();
    let script = f.path("creds.sh");
    std::fs::write(&script, "#!/bin/sh\necho '{\"Version\": 1, \"AccessKeyId\": \"AKPROC\", \"SecretAccessKey\": \"SKPROC\", \"SessionToken\": \"TPROC\", \"Expiration\": \"2099-01-01T00:00:00Z\"}'\n").unwrap();
    let bad = f.path("bad.sh");
    std::fs::write(&bad, "#!/bin/sh\necho '{\"Version\": 2, \"AccessKeyId\": \"A\", \"SecretAccessKey\": \"S\"}'\n").unwrap();
    let expired = f.path("expired.sh");
    std::fs::write(
        &expired,
        "#!/bin/sh\necho '{\"Version\": 1, \"AccessKeyId\": \"A\", \"SecretAccessKey\": \"S\", \"Expiration\": \"2001-01-01T00:00:00Z\"}'\n",
    )
    .unwrap();
    f.write(
        "config",
        &format!(
            "[profile proc]\ncredential_process = sh {}\n[profile bad]\ncredential_process = sh {}\n[profile expired]\ncredential_process = sh {}\n[profile failing]\ncredential_process = sh -c 'exit 3'\n",
            script.display(),
            bad.display(),
            expired.display()
        ),
    );
    assert_eq!(keys(&resolve(&f.env(), Some("proc")).await.unwrap()), ("AKPROC", "SKPROC", Some("TPROC")));
    // Invalid output is a provider error: the chain moves on and ends with the generic failure.
    for profile in ["bad", "expired", "failing"] {
        assert_eq!(resolve(&f.env(), Some(profile)).await.unwrap_err(), NO_CREDENTIALS, "{profile}");
    }
}

#[tokio::test]
async fn assume_role_with_source_profile_and_chaining() {
    let sts = mock(|request, n| {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/");
        assert_eq!(request.header("content-type"), Some("application/x-www-form-urlencoded"));
        sts_credentials("AssumeRole", n)
    })
    .await;
    let mut f = Fixture::new();
    f.set("AWS_ENDPOINT_URL_STS", &sts.base);
    f.write(
        "config",
        "[profile top]\nrole_arn = arn:aws:iam::123456789012:role/top\nsource_profile = middle\nregion = eu-west-2\nexternal_id = ext-1\nrole_session_name = my-session\nduration_seconds = 900\n\
         [profile middle]\nrole_arn = arn:aws:iam::123456789012:role/middle\nsource_profile = base\nregion = ap-south-1\n\
         [profile base]\naws_access_key_id = AKBASE\naws_secret_access_key = SKBASE\n",
    );
    let c = resolve(&f.env(), Some("top")).await.unwrap();
    assert_eq!(keys(&c), ("ASIA1", "secret1", Some("token1")));
    let calls = sts.requests();
    assert_eq!(calls.len(), 2);
    // The innermost role is assumed first, with the source profile's keys; both calls use the STS
    // client the outermost role profile created (its region).
    assert_eq!(calls[0].field("Action").as_deref(), Some("AssumeRole"));
    assert_eq!(calls[0].field("Version").as_deref(), Some("2011-06-15"));
    assert_eq!(calls[0].field("RoleArn").as_deref(), Some("arn:aws:iam::123456789012:role/middle"));
    assert!(calls[0].field("RoleSessionName").unwrap().starts_with("aws-sdk-js-"));
    assert_eq!(calls[0].field("DurationSeconds").as_deref(), Some("3600"));
    assert_eq!(calls[0].field("ExternalId"), None);
    assert_eq!(credential_scope(&calls[0]), Some(("AKBASE".into(), "eu-west-2".into(), "sts".into())));
    assert_eq!(calls[0].header("x-amz-security-token"), None);
    assert_eq!(calls[1].field("RoleArn").as_deref(), Some("arn:aws:iam::123456789012:role/top"));
    assert_eq!(calls[1].field("RoleSessionName").as_deref(), Some("my-session"));
    assert_eq!(calls[1].field("ExternalId").as_deref(), Some("ext-1"));
    assert_eq!(calls[1].field("DurationSeconds").as_deref(), Some("900"));
    assert_eq!(credential_scope(&calls[1]), Some(("ASIA0".into(), "eu-west-2".into(), "sts".into())));
    assert_eq!(calls[1].header("x-amz-security-token"), Some("token0"));
}

#[tokio::test]
async fn assume_role_with_credential_source() {
    let sts = mock(|_, n| sts_credentials("AssumeRole", n)).await;
    let imds = mock(|request, _| match (request.method.as_str(), request.path.as_str()) {
        ("PUT", "/latest/api/token") => Resp::text(200, "imds-token"),
        ("GET", "/latest/meta-data/iam/security-credentials/") => Resp::text(200, "instance-role\n"),
        ("GET", "/latest/meta-data/iam/security-credentials/instance-role") => {
            Resp::json(200, json!({"AccessKeyId": "AKIMDS", "SecretAccessKey": "SKIMDS", "Token": "TIMDS", "Expiration": "2099-01-01T00:00:00Z"}))
        }
        _ => Resp::text(404, ""),
    })
    .await;
    let mut f = Fixture::new();
    f.set("AWS_ENDPOINT_URL_STS", &sts.base).set("AWS_REGION", "us-west-1");
    f.set("AWS_ACCESS_KEY_ID", "AKENV").set("AWS_SECRET_ACCESS_KEY", "SKENV");
    f.set("AWS_EC2_METADATA_SERVICE_ENDPOINT", &imds.base);
    f.write(
        "config",
        "[profile env]\nrole_arn = arn:aws:iam::1:role/env\ncredential_source = Environment\n\
         [profile ec2]\nrole_arn = arn:aws:iam::1:role/ec2\ncredential_source = Ec2InstanceMetadata\n\
         [profile other]\nrole_arn = arn:aws:iam::1:role/x\ncredential_source = Somewhere\n",
    );
    assert_eq!(keys(&resolve(&f.env(), Some("env")).await.unwrap()), ("ASIA0", "secret0", Some("token0")));
    // No region in the profile: STS uses AWS_REGION.
    assert_eq!(credential_scope(&sts.requests()[0]), Some(("AKENV".into(), "us-west-1".into(), "sts".into())));
    // Ec2InstanceMetadata reads the instance role even though IMDS is disabled for the chain itself.
    assert_eq!(keys(&resolve(&f.env(), Some("ec2")).await.unwrap()), ("ASIA1", "secret1", Some("token1")));
    assert_eq!(credential_scope(&sts.requests()[1]).unwrap().0, "AKIMDS");
    assert_eq!(sts.requests()[1].header("x-amz-security-token"), Some("TIMDS"));
    // An unknown source is a provider error.
    assert_eq!(resolve(&f.env(), Some("other")).await.unwrap_err(), NO_CREDENTIALS);
}

#[tokio::test]
async fn assume_role_errors() {
    let sts = mock(|_, _| {
        Resp::xml(
            403,
            "<ErrorResponse><Error><Type>Sender</Type><Code>AccessDenied</Code><Message>User is not authorized to perform: sts:AssumeRole</Message></Error></ErrorResponse>",
        )
    })
    .await;
    let mut f = Fixture::new();
    f.set("AWS_ENDPOINT_URL_STS", &sts.base);
    f.write(
        "config",
        "[profile denied]\nrole_arn = arn:aws:iam::1:role/r\nsource_profile = keys\n\
         [profile keys]\naws_access_key_id = A\naws_secret_access_key = S\n\
         [profile mfa]\nrole_arn = arn:aws:iam::1:role/r\nsource_profile = keys\nmfa_serial = arn:aws:iam::1:mfa/me\n\
         [profile loop-a]\nrole_arn = arn:aws:iam::1:role/a\nsource_profile = loop-b\n\
         [profile loop-b]\nrole_arn = arn:aws:iam::1:role/b\nsource_profile = loop-a\n",
    );
    // An STS error is not a provider error: it ends the chain with its message.
    assert_eq!(resolve(&f.env(), Some("denied")).await.unwrap_err(), "AWS default credential chain failed: User is not authorized to perform: sts:AssumeRole");
    assert_eq!(
        resolve(&f.env(), Some("mfa")).await.unwrap_err(),
        "AWS default credential chain failed: Profile mfa requires multi-factor authentication, but no MFA code callback was provided."
    );
    // A source_profile cycle is a provider error.
    assert_eq!(resolve(&f.env(), Some("loop-a")).await.unwrap_err(), NO_CREDENTIALS);
    assert_eq!(sts.requests().len(), 1);
}

#[tokio::test]
async fn web_identity_from_environment_and_profile() {
    let sts = mock(|_, n| sts_credentials("AssumeRoleWithWebIdentity", n)).await;
    let mut f = Fixture::new();
    f.write("token", "eyJ.web.token\n");
    let token_file = f.path("token").to_string_lossy().to_string();
    f.set("AWS_ENDPOINT_URL", &sts.base)
        .set("AWS_WEB_IDENTITY_TOKEN_FILE", &token_file)
        .set("AWS_ROLE_ARN", "arn:aws:iam::1:role/web")
        .set("AWS_ROLE_SESSION_NAME", "pod-session");
    assert_eq!(keys(&resolve(&f.env(), None).await.unwrap()), ("ASIA0", "secret0", Some("token0")));
    let call = &sts.requests()[0];
    assert_eq!(call.field("Action").as_deref(), Some("AssumeRoleWithWebIdentity"));
    assert_eq!(call.field("RoleArn").as_deref(), Some("arn:aws:iam::1:role/web"));
    assert_eq!(call.field("RoleSessionName").as_deref(), Some("pod-session"));
    // The token is sent as read (not trimmed) and the call is not signed.
    assert_eq!(call.field("WebIdentityToken").as_deref(), Some("eyJ.web.token\n"));
    assert_eq!(call.header("authorization"), None);
    // A web identity profile.
    let mut f2 = Fixture::new();
    f2.set("AWS_ENDPOINT_URL_STS", &sts.base);
    f2.write("config", &format!("[profile web]\nweb_identity_token_file = {token_file}\nrole_arn = arn:aws:iam::1:role/p\n"));
    assert_eq!(keys(&resolve(&f2.env(), Some("web")).await.unwrap()), ("ASIA1", "secret1", Some("token1")));
    assert!(sts.requests()[1].field("RoleSessionName").unwrap().starts_with("aws-sdk-js-session-"));
    // A missing token file is not a provider error.
    let mut f3 = Fixture::new();
    f3.set("AWS_WEB_IDENTITY_TOKEN_FILE", "/nonexistent/ocd-token").set("AWS_ROLE_ARN", "arn:aws:iam::1:role/web");
    assert_eq!(
        resolve(&f3.env(), None).await.unwrap_err(),
        "AWS default credential chain failed: ENOENT: no such file or directory, open '/nonexistent/ocd-token'"
    );
}

fn sso_cache_name(id: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, id.as_bytes());
    format!(".aws/sso/cache/{}.json", hex_string(digest.as_ref()))
}

fn iso_in(seconds: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(seconds)).format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn sso_mock_routes(request: &Req, _: usize) -> Resp {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/federation/credentials") => {
            let token = request.header("x-amz-sso_bearer_token").unwrap_or_default().to_string();
            if token == "bad" {
                return Resp::json(401, json!({"message": "Session token not found or invalid"})).with("x-amzn-errortype", "UnauthorizedException");
            }
            Resp::json(
                200,
                json!({"roleCredentials": {"accessKeyId": format!("AK-{token}"), "secretAccessKey": "SKSSO", "sessionToken": "TSSO", "expiration": 4102444800000u64}}),
            )
        }
        ("POST", "/token") => Resp::json(200, json!({"accessToken": "refreshed", "expiresIn": 3600, "refreshToken": "rotated", "tokenType": "Bearer"})),
        _ => Resp::text(404, ""),
    }
}

#[tokio::test]
async fn sso_session_profiles() {
    assert_eq!(sso_cache_name("abc"), ".aws/sso/cache/a9993e364706816aba3e25717850c26c9cd0d89d.json");
    let server = mock(sso_mock_routes).await;
    let mut f = Fixture::new();
    f.set("AWS_ENDPOINT_URL_SSO", &server.base).set("AWS_ENDPOINT_URL_SSO_OIDC", &server.base);
    f.write(
        "config",
        "[profile dev]\nsso_session = corp\nsso_account_id = 111122223333\nsso_role_name = Dev\n\
         [sso-session corp]\nsso_start_url = https://corp.awsapps.com/start\nsso_region = eu-central-1\nsso_registration_scopes = sso:account:access\n\
         [profile broken]\nsso_session = corp\nsso_account_id = 111122223333\n\
         [profile ghost]\nsso_session = nowhere\nsso_account_id = 1\nsso_role_name = R\n",
    );
    f.write(
        &sso_cache_name("corp"),
        &json!({"startUrl": "https://corp.awsapps.com/start", "region": "eu-central-1", "accessToken": "cached", "expiresAt": iso_in(3600)}).to_string(),
    );
    let c = resolve(&f.env(), Some("dev")).await.unwrap();
    assert_eq!(keys(&c), ("AK-cached", "SKSSO", Some("TSSO")));
    let call = &server.requests_to("/federation/credentials")[0];
    assert_eq!(call.param("account_id").as_deref(), Some("111122223333"));
    assert_eq!(call.param("role_name").as_deref(), Some("Dev"));
    assert!(server.requests_to("/token").is_empty());
    // Invalid SSO configuration is not a provider error.
    assert_eq!(
        resolve(&f.env(), Some("broken")).await.unwrap_err(),
        "AWS default credential chain failed: Profile is configured with invalid SSO credentials. Required parameters \"sso_account_id\", \"sso_region\", \"sso_role_name\", \"sso_start_url\". Got sso_session, sso_account_id, sso_region, sso_start_url\nReference: https://docs.aws.amazon.com/cli/latest/userguide/cli-configure-sso.html"
    );
    assert!(resolve(&f.env(), Some("ghost")).await.unwrap_err().starts_with("AWS default credential chain failed: undefined is not an object"));
    // A rejected token surfaces the service error.
    f.write(&sso_cache_name("corp"), &json!({"accessToken": "bad", "expiresAt": iso_in(3600)}).to_string());
    assert_eq!(
        resolve(&f.env(), Some("dev")).await.unwrap_err(),
        "AWS default credential chain failed: UnauthorizedException: Session token not found or invalid"
    );
}

#[tokio::test]
async fn sso_token_refresh() {
    let server = mock(sso_mock_routes).await;
    let mut f = Fixture::new();
    f.set("AWS_ENDPOINT_URL_SSO", &server.base).set("AWS_ENDPOINT_URL_SSO_OIDC", &server.base);
    f.write(
        "config",
        "[profile dev]\nsso_session = corp\nsso_account_id = 1\nsso_role_name = Dev\n[sso-session corp]\nsso_start_url = https://s\nsso_region = us-east-2\n",
    );
    let cache = sso_cache_name("corp");
    // Within the 5-minute window with refresh data: CreateToken, then the cache file is rewritten.
    f.write(
        &cache,
        &json!({"startUrl": "https://s", "region": "us-east-2", "accessToken": "old", "expiresAt": iso_in(60), "clientId": "cid", "clientSecret": "csecret", "registrationExpiresAt": "2099-01-01T00:00:00Z", "refreshToken": "rt"})
            .to_string(),
    );
    let env = f.env();
    assert_eq!(keys(&resolve(&env, Some("dev")).await.unwrap()).0, "AK-refreshed");
    let create = &server.requests_to("/token")[0];
    assert_eq!(create.json(), json!({"clientId": "cid", "clientSecret": "csecret", "grantType": "refresh_token", "refreshToken": "rt"}));
    let written: serde_json::Map<String, Value> = serde_json::from_str(&std::fs::read_to_string(f.path(&cache)).unwrap()).unwrap();
    assert_eq!(
        written.keys().collect::<Vec<_>>(),
        ["startUrl", "region", "accessToken", "expiresAt", "clientId", "clientSecret", "registrationExpiresAt", "refreshToken"]
    );
    assert_eq!(written["accessToken"], "refreshed");
    assert_eq!(written["refreshToken"], "rotated");
    assert!(std::fs::read_to_string(f.path(&cache)).unwrap().contains("\n  \"accessToken\""));
    // A second refresh within 30 s is not attempted: the still-valid old token is used.
    f.write(&cache, &json!({"accessToken": "old2", "expiresAt": iso_in(60), "clientId": "c", "clientSecret": "s", "refreshToken": "r"}).to_string());
    assert_eq!(keys(&resolve(&env, Some("dev")).await.unwrap()).0, "AK-old2");
    assert_eq!(server.requests_to("/token").len(), 1);
    // An expired token that cannot be refreshed.
    f.write(&cache, &json!({"accessToken": "old3", "expiresAt": iso_in(-60)}).to_string());
    assert_eq!(
        resolve(&f.env(), Some("dev")).await.unwrap_err(),
        "AWS default credential chain failed: Value not present for 'clientId' in SSO Token. Cannot refresh. To refresh this SSO session run 'aws sso login' with the corresponding profile."
    );
    std::fs::remove_file(f.path(&cache)).unwrap();
    assert_eq!(
        resolve(&f.env(), Some("dev")).await.unwrap_err(),
        "AWS default credential chain failed: The SSO session token associated with profile=dev was not found or is invalid. To refresh this SSO session run 'aws sso login' with the corresponding profile."
    );
}

#[tokio::test]
async fn legacy_sso_profiles() {
    let server = mock(sso_mock_routes).await;
    let mut f = Fixture::new();
    f.set("AWS_ENDPOINT_URL_SSO", &server.base);
    f.write("config", "[default]\nsso_start_url = https://legacy.awsapps.com/start\nsso_region = us-east-1\nsso_account_id = 9\nsso_role_name = Admin\n");
    let cache = sso_cache_name("https://legacy.awsapps.com/start");
    f.write(&cache, &json!({"accessToken": "legacy", "expiresAt": iso_in(600)}).to_string());
    assert_eq!(keys(&resolve(&f.env(), None).await.unwrap()).0, "AK-legacy");
    f.write(&cache, &json!({"accessToken": "legacy", "expiresAt": iso_in(-1)}).to_string());
    assert_eq!(
        resolve(&f.env(), None).await.unwrap_err(),
        "AWS default credential chain failed: The SSO session associated with this profile has expired. To refresh this SSO session run aws sso login with the corresponding profile."
    );
    std::fs::remove_file(f.path(&cache)).unwrap();
    assert_eq!(
        resolve(&f.env(), None).await.unwrap_err(),
        "AWS default credential chain failed: The SSO session associated with this profile is invalid. To refresh this SSO session run aws sso login with the corresponding profile."
    );
}

fn imds_routes(token_status: u16) -> impl Fn(&Req, usize) -> Resp + Send + Sync + 'static {
    move |request: &Req, _| match (request.method.as_str(), request.path.as_str()) {
        ("PUT", "/latest/api/token") => {
            assert_eq!(request.header("x-aws-ec2-metadata-token-ttl-seconds"), Some("21600"));
            if token_status == 200 {
                Resp::text(200, "imds-token")
            } else {
                Resp::text(token_status, "")
            }
        }
        ("GET", "/latest/meta-data/iam/security-credentials/") => Resp::text(200, "role-a"),
        ("GET", "/latest/meta-data/iam/security-credentials/role-a") => {
            Resp::json(200, json!({"Code": "Success", "AccessKeyId": "AKI", "SecretAccessKey": "SKI", "Token": "TI", "Expiration": "2001-01-01T00:00:00Z"}))
        }
        _ => Resp::text(404, ""),
    }
}

#[tokio::test]
async fn instance_metadata() {
    // IMDSv2: a session token on every metadata request; expired credentials are still used.
    let imds = mock(imds_routes(200)).await;
    let mut f = Fixture::new();
    f.unset("AWS_EC2_METADATA_DISABLED").set("AWS_EC2_METADATA_SERVICE_ENDPOINT", &imds.base);
    assert_eq!(keys(&resolve(&f.env(), None).await.unwrap()), ("AKI", "SKI", Some("TI")));
    let calls = imds.requests();
    assert_eq!(calls.len(), 3);
    assert!(calls[1..].iter().all(|r| r.header("x-aws-ec2-metadata-token") == Some("imds-token")));
    // The endpoint from the config file, and the v1 fallback when the token request is refused.
    let v1 = mock(imds_routes(403)).await;
    let mut f = Fixture::new();
    f.unset("AWS_EC2_METADATA_DISABLED");
    f.write("config", &format!("[default]\nec2_metadata_service_endpoint = {}\n", v1.base));
    assert_eq!(keys(&resolve(&f.env(), None).await.unwrap()).0, "AKI");
    assert!(v1.requests()[1..].iter().all(|r| r.header("x-aws-ec2-metadata-token").is_none()));
    // The fallback can be blocked.
    f.set("AWS_EC2_METADATA_V1_DISABLED", "true");
    assert_eq!(resolve(&f.env(), None).await.unwrap_err(), NO_CREDENTIALS);
    // A 400 on the token request is an error.
    let bad = mock(imds_routes(400)).await;
    let mut f = Fixture::new();
    f.unset("AWS_EC2_METADATA_DISABLED").set("AWS_EC2_METADATA_SERVICE_ENDPOINT", &bad.base);
    assert_eq!(resolve(&f.env(), None).await.unwrap_err(), NO_CREDENTIALS);
    assert_eq!(bad.requests().len(), 1);
    // An unknown endpoint mode is not a provider error.
    let mut f = Fixture::new();
    f.unset("AWS_EC2_METADATA_DISABLED").set("AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE", "IPv5");
    assert_eq!(resolve(&f.env(), None).await.unwrap_err(), "AWS default credential chain failed: Unsupported endpoint mode: IPv5. Select from IPv4,IPv6");
}

#[tokio::test]
async fn container_credentials() {
    let server = mock(|request, _| {
        if request.header("authorization") == Some("container-token") {
            Resp::json(200, json!({"AccessKeyId": "AKC", "SecretAccessKey": "SKC", "Token": "TC", "Expiration": "2099-01-01T00:00:00Z"}))
        } else {
            Resp::text(401, "")
        }
    })
    .await;
    let mut f = Fixture::new();
    f.set("AWS_CONTAINER_CREDENTIALS_FULL_URI", format!("{}/v2/credentials?id=1", server.base)).set("AWS_CONTAINER_AUTHORIZATION_TOKEN", "container-token");
    assert_eq!(keys(&resolve(&f.env(), None).await.unwrap()), ("AKC", "SKC", Some("TC")));
    let call = &server.requests()[0];
    assert_eq!((call.path.as_str(), call.query.as_str()), ("/v2/credentials", "id=1"));
    // The token file wins over the token variable.
    f.write("ctoken", "container-token");
    let token_file = f.path("ctoken").to_string_lossy().to_string();
    f.set("AWS_CONTAINER_AUTHORIZATION_TOKEN", "wrong").set("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE", token_file);
    assert_eq!(keys(&resolve(&f.env(), None).await.unwrap()).0, "AKC");
    // Plain HTTP to a host that is neither loopback nor a container host is refused.
    let mut f = Fixture::new();
    f.set("AWS_CONTAINER_CREDENTIALS_FULL_URI", "http://192.0.2.1/creds");
    assert_eq!(resolve(&f.env(), None).await.unwrap_err(), NO_CREDENTIALS);
}

/// A SEC1 `EC PRIVATE KEY` PEM (with its public key) for a fresh P-256 key.
fn sec1_pem() -> (String, Vec<u8>) {
    use base64::Engine;
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(&ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let der = pkcs8.as_ref();
    // PrivateKeyInfo ::= SEQUENCE { version, algorithm, OCTET STRING { ECPrivateKey } }
    fn element(input: &[u8]) -> (u8, &[u8], &[u8]) {
        let len = input[1] as usize;
        if len < 0x80 {
            (input[0], &input[2..2 + len], &input[2 + len..])
        } else {
            let n = len & 0x7f;
            let len = input[2..2 + n].iter().fold(0usize, |a, &b| (a << 8) | b as usize);
            (input[0], &input[2 + n..2 + n + len], &input[2 + n + len..])
        }
    }
    let (_, info, _) = element(der);
    let (_, _, rest) = element(info);
    let (_, _, rest) = element(rest);
    let (_, sec1, _) = element(rest);
    let key = ring::signature::EcdsaKeyPair::from_pkcs8(&ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING, der, &rng).unwrap();
    use ring::signature::KeyPair;
    let public = key.public_key().as_ref().to_vec();
    let body = base64::engine::general_purpose::STANDARD.encode(sec1);
    let lines: Vec<String> = body.as_bytes().chunks(64).map(|c| String::from_utf8(c.to_vec()).unwrap()).collect();
    (format!("-----BEGIN EC PRIVATE KEY-----\n{}\n-----END EC PRIVATE KEY-----\n", lines.join("\n")), public)
}

fn verify_dpop(proof: &str, public: &[u8], url: &str) {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let parts: Vec<&str> = proof.split('.').collect();
    let header: Value = serde_json::from_slice(&b64.decode(parts[0]).unwrap()).unwrap();
    let payload: Value = serde_json::from_slice(&b64.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(header["alg"], "ES256");
    assert_eq!(header["typ"], "dpop+jwt");
    assert_eq!(header["jwk"]["x"], b64.encode(&public[1..33]));
    assert_eq!(payload["htm"], "POST");
    assert_eq!(payload["htu"], url);
    let key = ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, public);
    key.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &b64.decode(parts[2]).unwrap()).unwrap();
}

#[tokio::test]
async fn login_sessions() {
    let (pem, public) = sec1_pem();
    let proof = dpop_proof(&pem, "POST", "https://x/v1/token").unwrap();
    verify_dpop(&proof, &public, "https://x/v1/token");
    let signin = mock(|request, _| {
        assert_eq!(request.path, "/v1/token");
        Resp::json(
            200,
            json!({"accessToken": {"accessKeyId": "AKNEW", "secretAccessKey": "SKNEW", "sessionToken": "TNEW"}, "tokenType": "aws_sigv4", "expiresIn": 900, "refreshToken": "rt2"}),
        )
    })
    .await;
    let mut f = Fixture::new();
    let cache = f.path("login-cache");
    f.set("AWS_LOGIN_CACHE_DIRECTORY", cache.to_string_lossy().to_string()).set("AWS_ENDPOINT_URL_SIGNIN", &signin.base);
    f.write("config", "[profile console]\nlogin_session = arn:aws:sts::123:assumed-role/Admin/me\nregion = us-west-2\n");
    let digest = ring::digest::digest(&ring::digest::SHA256, b"arn:aws:sts::123:assumed-role/Admin/me");
    let file = cache.join(format!("{}.json", hex_string(digest.as_ref())));
    let token = |expires: String| json!({"accessToken": {"accessKeyId": "AKOLD", "secretAccessKey": "SKOLD", "sessionToken": "TOLD", "accountId": "123", "expiresAt": expires}, "tokenType": "aws_sigv4", "clientId": "arn:aws:signin:::devtools/same-device", "refreshToken": "rt1", "idToken": "id", "dpopKey": pem});
    write_json(&file, &token(iso_in(3600)));
    assert_eq!(keys(&resolve(&f.env(), Some("console")).await.unwrap()), ("AKOLD", "SKOLD", Some("TOLD")));
    assert!(signin.requests().is_empty());
    // Close to expiry: CreateOAuth2Token with a DPoP proof, then the cache is rewritten.
    write_json(&file, &token(iso_in(60)));
    assert_eq!(keys(&resolve(&f.env(), Some("console")).await.unwrap()), ("AKNEW", "SKNEW", Some("TNEW")));
    let call = &signin.requests()[0];
    assert_eq!(call.json(), json!({"clientId": "arn:aws:signin:::devtools/same-device", "grantType": "refresh_token", "refreshToken": "rt1"}));
    verify_dpop(call.header("dpop").unwrap(), &public, &format!("{}/v1/token", signin.base));
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(written["refreshToken"], "rt2");
    assert_eq!(written["accessToken"]["accessKeyId"], "AKNEW");
    assert_eq!(written["accessToken"]["accountId"], "123");
    // A token file without the required fields.
    write_json(&file, &json!({"accessToken": {"accessKeyId": "A"}}));
    let error = resolve(&f.env(), Some("console")).await.unwrap_err();
    assert!(error.ends_with("CredentialsProviderError: Token validation failed, missing fields: clientId, refreshToken, dpopKey, accountId"), "{error}");
}

fn write_json(path: &Path, value: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
}

fn hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
