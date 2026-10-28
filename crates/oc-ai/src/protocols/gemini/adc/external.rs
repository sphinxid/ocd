//! `external_account` credentials (workload and workforce identity federation) as
//! `google-auth-library` 10.5.0 implements them: `BaseExternalAccountClient` (STS token exchange,
//! service account impersonation, `workforce_pool_user_project`) with the subject token from an
//! identity pool source (`IdentityPoolClient`: file, URL or X.509 certificate), AWS
//! (`AwsClient`: a signed `GetCallerIdentity` request) or an executable (`PluggableAuthClient`).

use super::{Cached, Ctx, Request, js_string, now_ms, oauth_error, opt, opt_str, send};
use crate::protocols::bedrock_converse::sigv4::encode_uri_component;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_SCOPE: &str = super::SCOPE;
const STS_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const STS_REQUEST_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
const WORKFORCE_AUDIENCE: &str = r"//iam\.googleapis\.com/locations/[^/]+/workforcePools/[^/]+/providers/.+";

/// The subject token source.
enum Source {
    File { path: String, format: Format },
    Url { url: String, headers: Option<serde_json::Map<String, Value>>, format: Format },
    Certificate(Certificate),
    Aws(Aws),
    Executable(Executable),
}

#[derive(Clone)]
enum Format {
    Text,
    Json(String),
}

pub struct External {
    audience: String,
    subject_token_type: Option<String>,
    token_url: String,
    client_auth: Option<(String, Option<String>)>,
    scopes: Vec<String>,
    impersonation_url: Option<String>,
    lifetime: Value,
    config_lifetime: bool,
    workforce_pool_user_project: Option<String>,
    quota_project: Option<String>,
    universe: String,
    source: Source,
    cached: Option<Cached>,
    /// The mTLS client of a certificate source (set when the subject token is read).
    mtls: Option<reqwest::Client>,
}

impl External {
    /// `ExternalAccountClient.fromJSON` and the client constructors' validation.
    pub fn from_json(json: &Value, ctx: &Ctx) -> Result<External, String> {
        let _ = ctx;
        let universe = opt_str(json, "universe_domain").unwrap_or_else(|| "googleapis.com".into());
        let token_url = opt(json, "token_url").map(js_string).unwrap_or_else(|| format!("https://sts.{universe}/v1/token"));
        let client_auth = opt_str(json, "client_id").map(|id| (id, opt(json, "client_secret").map(js_string)));
        let audience = opt(json, "audience").map(js_string).ok_or("Cannot read properties of undefined (reading 'match')")?;
        let workforce_pool_user_project = opt_str(json, "workforce_pool_user_project");
        if workforce_pool_user_project.is_some() && !regex::Regex::new(WORKFORCE_AUDIENCE).unwrap().is_match(&audience) {
            return Err("workforcePoolUserProject should not be set for non-workforce pool credentials.".into());
        }
        let impersonation = opt(json, "service_account_impersonation").cloned().unwrap_or(Value::Null);
        let requested_lifetime = opt(&impersonation, "token_lifetime_seconds").filter(|v| super::truthy(v)).cloned();
        let config_lifetime = requested_lifetime.is_some();
        let source = match json.get("credential_source") {
            Some(cs) if cs.get("environment_id").is_some_and(super::truthy) => Source::Aws(Aws::from_json(json)?),
            Some(cs) if cs.get("executable").is_some_and(super::truthy) => Source::Executable(Executable::from_json(cs)?),
            _ => identity_pool(json)?,
        };
        Ok(External {
            audience,
            subject_token_type: opt(json, "subject_token_type").map(js_string),
            token_url,
            client_auth,
            scopes: vec![DEFAULT_SCOPE.to_string()],
            impersonation_url: opt(json, "service_account_impersonation_url").map(js_string).filter(|s| !s.is_empty()),
            lifetime: requested_lifetime.unwrap_or_else(|| json!(3600)),
            config_lifetime,
            workforce_pool_user_project,
            quota_project: opt(json, "quota_project_id").map(js_string),
            universe,
            source,
            cached: None,
            mtls: None,
        })
    }

    pub fn universe(&self) -> String {
        self.universe.clone()
    }

    pub fn quota_project(&self) -> Option<String> {
        self.quota_project.clone()
    }

    fn source_type(&self) -> &'static str {
        match self.source {
            Source::File { .. } => "file",
            Source::Url { .. } => "url",
            Source::Certificate(_) => "certificate",
            Source::Aws(_) => "aws",
            Source::Executable(_) => "executable",
        }
    }

    /// `getServiceAccountEmail`.
    fn service_account_email(&self) -> Result<Option<String>, String> {
        let Some(url) = &self.impersonation_url else { return Ok(None) };
        if url.chars().count() > 256 {
            return Err(format!("URL is too long: {url}"));
        }
        Ok(regex::Regex::new(r"serviceAccounts/([^:]+):generateAccessToken$").unwrap().captures(url).map(|c| c[1].to_string()))
    }

    /// `getAccessToken()`: the cached token until 5 minutes before it expires.
    pub async fn access_token(&mut self, ctx: &Ctx) -> Result<String, String> {
        if let Some(cached) = &self.cached
            && !cached.expiring()
        {
            return cached.access_token.clone().ok_or_else(|| "empty token".to_string());
        }
        let cached = self.refresh(ctx).await?;
        let token = cached.access_token.clone();
        self.cached = Some(cached);
        token.ok_or_else(|| "Google ADC returned an empty access token".to_string())
    }

    /// `#internalRefreshAccessTokenAsync`.
    async fn refresh(&mut self, ctx: &Ctx) -> Result<Cached, String> {
        let subject_token = self.subject_token(ctx).await?;
        let scope = if self.impersonation_url.is_some() {
            DEFAULT_SCOPE.to_string()
        } else {
            self.scopes.join(" ")
        };
        let mut fields: Vec<(String, String)> = vec![("grant_type".into(), STS_GRANT_TYPE.into()), ("audience".into(), self.audience.clone())];
        fields.push(("scope".into(), scope));
        fields.push(("requested_token_type".into(), STS_REQUEST_TOKEN_TYPE.into()));
        fields.push(("subject_token".into(), subject_token));
        if let Some(kind) = &self.subject_token_type {
            fields.push(("subject_token_type".into(), kind.clone()));
        }
        if self.client_auth.is_none()
            && let Some(project) = &self.workforce_pool_user_project
        {
            fields.push(("options".into(), json!({"userProject": project}).to_string()));
        }
        // `removeUndefinedValuesInObject` also drops the string "undefined".
        fields.retain(|(_, v)| v != "undefined");
        let metrics = format!(
            "gl-node/{} auth/{} google-byoid-sdk source/{} sa-impersonation/{} config-lifetime/{}",
            super::NODE_VERSION,
            super::LIBRARY_VERSION,
            self.source_type(),
            self.impersonation_url.is_some(),
            self.config_lifetime
        );
        let mut request = Request::new(reqwest::Method::POST, self.token_url.clone()).retry().form(fields).header("x-goog-api-client", metrics);
        if let Some((id, secret)) = &self.client_auth {
            let basic = base64::engine::general_purpose::STANDARD.encode(format!("{id}:{}", secret.clone().unwrap_or_default()));
            request = request.header("authorization", format!("Basic {basic}"));
        }
        let http = self.mtls.clone().unwrap_or_else(|| ctx.http.clone());
        let response = send(&http, &request).await.map_err(oauth_error)?;
        let sts_token = response.data.get("access_token").map(js_string).unwrap_or_else(|| "undefined".into());
        if let Some(url) = &self.impersonation_url {
            let request = Request::new(reqwest::Method::POST, url.clone())
                .retry()
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {sts_token}"))
                .json(json!({"scope": self.scopes, "lifetime": format!("{}s", js_string(&self.lifetime))}));
            let response = send(&http, &request).await.map_err(|e| e.message)?;
            let expiry = response.data.get("expireTime").and_then(Value::as_str).and_then(crate::js_date::parse).unwrap_or(f64::NAN);
            return Ok(Cached { access_token: response.data.get("accessToken").map(js_string), expiry_date: Some(expiry) });
        }
        let expires_in = response.data.get("expires_in").and_then(Value::as_f64).filter(|e| *e != 0.0);
        Ok(Cached { access_token: response.data.get("access_token").map(js_string), expiry_date: expires_in.map(|e| now_ms() + e * 1000.0) })
    }

    /// `retrieveSubjectToken`.
    async fn subject_token(&mut self, ctx: &Ctx) -> Result<String, String> {
        let email = self.service_account_email();
        let audience = self.audience.clone();
        let token_type = self.subject_token_type.clone();
        match &mut self.source {
            Source::File { path, format } => file_token(path, format),
            Source::Url { url, headers, format } => {
                let mut request = Request::new(reqwest::Method::GET, url.clone()).retry();
                for (k, v) in headers.iter().flatten() {
                    request = request.header(k, js_string(v));
                }
                let response = send(&ctx.http, &request).await.map_err(|e| e.message)?;
                let token = match format {
                    Format::Text => response.data,
                    Format::Json(field) => response.data.get(field.as_str()).cloned().unwrap_or(Value::Null),
                };
                if !super::truthy(&token) {
                    return Err("Unable to parse the subject_token from the credential_source URL".into());
                }
                Ok(js_string(&token))
            }
            Source::Certificate(certificate) => {
                let (token, client) = certificate.subject_token(ctx)?;
                self.mtls = Some(client);
                Ok(token)
            }
            Source::Aws(aws) => aws.subject_token(ctx, &audience).await,
            Source::Executable(executable) => executable.subject_token(ctx, &audience, token_type.as_deref(), email?.as_deref()).await,
        }
    }
}

/// `IdentityPoolClient` construction.
fn identity_pool(json: &Value) -> Result<Source, String> {
    let source = opt(json, "credential_source").ok_or("A credential source or subject token supplier must be specified.")?;
    let format_opts = opt(source, "format").cloned().unwrap_or(Value::Null);
    let format_type = opt(&format_opts, "type").map(js_string).filter(|s| !s.is_empty()).unwrap_or_else(|| "text".into());
    let field = opt_str(&format_opts, "subject_token_field_name");
    let format = match format_type.as_str() {
        "text" => Format::Text,
        "json" => Format::Json(field.ok_or("Missing subject_token_field_name for JSON credential_source format")?),
        other => return Err(format!("Invalid credential_source format \"{other}\"")),
    };
    let file = opt(source, "file").filter(|v| super::truthy(v));
    let url = opt(source, "url").filter(|v| super::truthy(v));
    let certificate = opt(source, "certificate").filter(|v| super::truthy(v));
    let invalid = "No valid Identity Pool \"credential_source\" provided, must be either file, url, or certificate.";
    match (file, url, certificate) {
        (Some(file), None, None) => Ok(Source::File { path: js_string(file), format }),
        (None, Some(url), None) => Ok(Source::Url { url: js_string(url), headers: opt(source, "headers").and_then(|h| h.as_object().cloned()), format }),
        (None, None, Some(certificate)) => Ok(Source::Certificate(Certificate::from_json(certificate)?)),
        _ => Err(invalid.into()),
    }
}

/// `FileSubjectTokenSupplier.getSubjectToken`.
fn file_token(path: &str, format: &Format) -> Result<String, String> {
    let real = std::fs::canonicalize(path).map_err(|e| format!("The file at {path} does not exist, or it is not a file. {e}"))?;
    if !std::fs::symlink_metadata(&real).is_ok_and(|m| m.is_file()) {
        return Err(format!("The file at {} does not exist, or it is not a file. ", real.display()));
    }
    let raw = std::fs::read_to_string(&real).map_err(|e| e.to_string())?;
    let token = match format {
        Format::Text => Value::String(raw),
        Format::Json(field) => serde_json::from_str::<Value>(&raw).map_err(|e| e.to_string())?.get(field.as_str()).cloned().unwrap_or(Value::Null),
    };
    if !super::truthy(&token) {
        return Err("Unable to parse the subject_token from the credential_source file".into());
    }
    Ok(js_string(&token))
}

// ---------------------------------------------------------------------------------------------
// Certificate source
// ---------------------------------------------------------------------------------------------

/// `CertificateSubjectTokenSupplier`.
struct Certificate {
    config_path: String,
    trust_chain_path: Option<String>,
}

impl Certificate {
    fn from_json(certificate: &Value) -> Result<Certificate, String> {
        let use_default = certificate.get("use_default_certificate_config").is_some_and(super::truthy);
        let location = certificate.get("certificate_config_location").filter(|v| super::truthy(v)).map(js_string);
        if !use_default && location.is_none() {
            return Err("Either `useDefaultCertificateConfig` must be true or a `certificateConfigLocation` must be provided.".into());
        }
        if use_default && location.is_some() {
            return Err("Both `useDefaultCertificateConfig` and `certificateConfigLocation` cannot be provided.".into());
        }
        Ok(Certificate {
            config_path: location.unwrap_or_default(),
            trust_chain_path: certificate.get("trust_chain_path").filter(|v| super::truthy(v)).map(js_string),
        })
    }

    fn is_file(path: &str) -> bool {
        std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
    }

    /// The certificate config path: the override, `GOOGLE_API_CERTIFICATE_CONFIG`, then gcloud's.
    fn resolve_config(&mut self, ctx: &Ctx) -> Result<String, String> {
        if !self.config_path.is_empty() {
            if Self::is_file(&self.config_path) {
                return Ok(self.config_path.clone());
            }
            return Err(format!("Provided certificate config path is invalid: {}", self.config_path));
        }
        if let Some(path) = ctx.env.non_empty("GOOGLE_API_CERTIFICATE_CONFIG") {
            if Self::is_file(&path) {
                return Ok(path);
            }
            return Err(format!("Path from environment variable \"GOOGLE_API_CERTIFICATE_CONFIG\" is invalid: {path}"));
        }
        let dir = match ctx.env.non_empty("CLOUDSDK_CONFIG") {
            Some(dir) => std::path::PathBuf::from(dir),
            None if cfg!(windows) => std::path::PathBuf::from(ctx.env.var("APPDATA").unwrap_or_default()).join("gcloud"),
            None => std::path::PathBuf::from(ctx.env.var("HOME").unwrap_or_default()).join(".config").join("gcloud"),
        };
        let well_known = dir.join("certificate_config.json").to_string_lossy().to_string();
        if Self::is_file(&well_known) {
            return Ok(well_known);
        }
        Err(format!(
            "Could not find certificate configuration file. Searched override path, the \"GOOGLE_API_CERTIFICATE_CONFIG\" env var, and the gcloud path ({well_known})."
        ))
    }

    /// The subject token (the certificate chain as base64 DER, JSON-encoded) and the mTLS client.
    fn subject_token(&mut self, ctx: &Ctx) -> Result<(String, reqwest::Client), String> {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        self.config_path = self.resolve_config(ctx)?;
        let config_path = self.config_path.clone();
        let text = std::fs::read_to_string(&config_path).map_err(|_| format!("Failed to read certificate config file at: {config_path}"))?;
        let config: Value = serde_json::from_str(&text).map_err(|e| format!("Failed to parse certificate config from {config_path}: {e}"))?;
        let workload = config.get("cert_configs").and_then(|c| c.get("workload")).cloned().unwrap_or(Value::Null);
        let (Some(cert_path), Some(key_path)) = (workload.get("cert_path").filter(|v| super::truthy(v)), workload.get("key_path").filter(|v| super::truthy(v)))
        else {
            return Err(format!("Certificate config file ({config_path}) is missing required \"cert_path\" or \"key_path\" in the workload config."));
        };
        let (cert_path, key_path) = (js_string(cert_path), js_string(key_path));
        let cert_pem = std::fs::read(&cert_path).map_err(|e| format!("Failed to read certificate file at {cert_path}: {e}"))?;
        let leaf = CertificateDer::pem_slice_iter(&cert_pem)
            .next()
            .and_then(|c| c.ok())
            .ok_or_else(|| format!("Failed to read certificate file at {cert_path}: invalid certificate"))?;
        let key_pem = std::fs::read(&key_path).map_err(|e| format!("Failed to read private key file at {key_path}: {e}"))?;
        let key = PrivateKeyDer::from_pem_slice(&key_pem).map_err(|e| format!("Failed to read private key file at {key_path}: {e}"))?;
        let chain: Vec<CertificateDer<'static>> = match &self.trust_chain_path {
            None => vec![leaf.clone()],
            Some(path) => {
                let pems = std::fs::read(path).map_err(|e| format!("Failed to process certificate chain from {path}: {e}"))?;
                let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&pems).filter_map(|c| c.ok()).collect();
                match certs.iter().position(|c| c.as_ref() == leaf.as_ref()) {
                    None => std::iter::once(leaf.clone()).chain(certs).collect(),
                    Some(0) => certs,
                    Some(i) => return Err(format!("Leaf certificate exists in the trust chain but is not the first entry (found at index {i}).")),
                }
            }
        };
        let token =
            serde_json::to_string(&chain.iter().map(|c| base64::engine::general_purpose::STANDARD.encode(c.as_ref())).collect::<Vec<_>>()).unwrap_or_default();
        let client = mtls_client(ctx, vec![leaf], key)?;
        Ok((token, client))
    }
}

#[derive(Debug)]
struct ClientCert(Arc<rustls::sign::CertifiedKey>);

impl rustls::client::ResolvesClientCert for ClientCert {
    fn resolve(&self, _hints: &[&[u8]], _schemes: &[rustls::SignatureScheme]) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(self.0.clone())
    }
    fn has_certs(&self) -> bool {
        true
    }
}

/// An HTTPS client presenting the workload certificate (`new https.Agent({ key, cert })`).
fn mtls_client(
    ctx: &Ctx,
    certs: Vec<rustls::pki_types::CertificateDer<'static>>,
    key: rustls::pki_types::PrivateKeyDer<'static>,
) -> Result<reqwest::Client, String> {
    let provider = rustls::crypto::ring::default_provider();
    let signing = provider.key_provider.load_private_key(key).map_err(|e| e.to_string())?;
    let trust = ctx.endpoints.trust.clone().unwrap_or_else(|| oc_util::http::Trust::global().clone());
    let mut config = trust.client_config(&[b"h2", b"http/1.1"]);
    config.client_auth_cert_resolver = Arc::new(ClientCert(Arc::new(rustls::sign::CertifiedKey::new(certs, signing))));
    oc_util::http::client_builder().use_preconfigured_tls(config).build().map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------------------------
// AWS source
// ---------------------------------------------------------------------------------------------

/// `AwsClient` with `DefaultAwsSecurityCredentialsSupplier`.
struct Aws {
    region_url: Option<String>,
    url: Option<String>,
    imdsv2_url: Option<String>,
    verification_url: String,
    /// The region, read once (`awsRequestSigner` is created on the first exchange).
    region: Option<String>,
}

impl Aws {
    fn from_json(json: &Value) -> Result<Aws, String> {
        let source = opt(json, "credential_source").cloned().unwrap_or(Value::Null);
        let environment_id = opt(&source, "environment_id").map(js_string).unwrap_or_default();
        let verification_url = opt(&source, "regional_cred_verification_url").map(js_string).filter(|s| !s.is_empty());
        let caps = regex::Regex::new(r"^(aws)(\d+)$").unwrap().captures(&environment_id).map(|c| c[2].to_string());
        let (Some(version), Some(verification_url)) = (caps, verification_url) else {
            return Err("No valid AWS \"credential_source\" provided".into());
        };
        if version.parse::<u64>().ok() != Some(1) {
            return Err(format!("aws version \"{version}\" is not supported in the current build."));
        }
        Ok(Aws {
            region_url: opt(&source, "region_url").map(js_string).filter(|s| !s.is_empty()),
            url: opt(&source, "url").map(js_string).filter(|s| !s.is_empty()),
            imdsv2_url: opt(&source, "imdsv2_session_token_url").map(js_string).filter(|s| !s.is_empty()),
            verification_url,
            region: None,
        })
    }

    async fn session_token(&self, ctx: &Ctx) -> Result<String, String> {
        let url = self.imdsv2_url.clone().unwrap_or_default();
        let request = Request::new(reqwest::Method::PUT, url).header("x-aws-ec2-metadata-token-ttl-seconds", "300");
        Ok(js_string(&send(&ctx.http, &request).await.map_err(|e| e.message)?.data))
    }

    /// `getAwsRegion`: `AWS_REGION` / `AWS_DEFAULT_REGION`, else the region URL minus its last
    /// character (the availability zone letter).
    async fn region(&self, ctx: &Ctx) -> Result<String, String> {
        if let Some(region) = ctx.env.non_empty("AWS_REGION").or_else(|| ctx.env.non_empty("AWS_DEFAULT_REGION")) {
            return Ok(region);
        }
        let mut request = Request::new(reqwest::Method::GET, String::new());
        if self.imdsv2_url.is_some() {
            request = request.header("x-aws-ec2-metadata-token", self.session_token(ctx).await?);
        }
        let Some(url) = &self.region_url else {
            return Err("Unable to determine AWS region due to missing \"options.credential_source.region_url\"".into());
        };
        request.url = url.clone();
        let data = js_string(&send(&ctx.http, &request).await.map_err(|e| e.message)?.data);
        let mut chars: Vec<char> = data.chars().collect();
        chars.pop();
        Ok(chars.into_iter().collect())
    }

    /// `getAwsSecurityCredentials`: the environment, else the metadata role's credentials.
    async fn credentials(&self, ctx: &Ctx) -> Result<(String, String, Option<String>), String> {
        if let (Some(id), Some(secret)) = (ctx.env.non_empty("AWS_ACCESS_KEY_ID"), ctx.env.non_empty("AWS_SECRET_ACCESS_KEY")) {
            return Ok((id, secret, ctx.env.var("AWS_SESSION_TOKEN")));
        }
        let mut headers = Vec::new();
        if self.imdsv2_url.is_some() {
            headers.push(("x-aws-ec2-metadata-token", self.session_token(ctx).await?));
        }
        let Some(url) = &self.url else {
            return Err("Unable to determine AWS role name due to missing \"options.credential_source.url\"".into());
        };
        let with_headers = |mut request: Request| {
            for (k, v) in &headers {
                request = request.header(k, v.clone());
            }
            request
        };
        let role = js_string(&send(&ctx.http, &with_headers(Request::new(reqwest::Method::GET, url.clone()))).await.map_err(|e| e.message)?.data);
        let creds = send(&ctx.http, &with_headers(Request::new(reqwest::Method::GET, format!("{url}/{role}")))).await.map_err(|e| e.message)?.data;
        let field = |k: &str| creds.get(k).filter(|v| !v.is_null()).map(js_string);
        Ok((field("AccessKeyId").unwrap_or_else(|| "undefined".into()), field("SecretAccessKey").unwrap_or_else(|| "undefined".into()), field("Token")))
    }

    /// `AwsClient.retrieveSubjectToken`: the serialized signed `GetCallerIdentity` request.
    async fn subject_token(&mut self, ctx: &Ctx, audience: &str) -> Result<String, String> {
        if self.region.is_none() {
            self.region = Some(self.region(ctx).await?);
        }
        let region = self.region.clone().unwrap_or_default();
        let url = self.verification_url.replace("{region}", &region);
        let (access_key_id, secret, token) = self.credentials(ctx).await?;
        let signed = sign_get_caller_identity(&url, &region, &access_key_id, &secret, token.as_deref().filter(|t| !t.is_empty()), chrono::Utc::now())?;
        let mut headers: Vec<(String, String)> = signed;
        headers.push(("x-goog-cloud-target-resource".into(), audience.to_string()));
        // `Headers` iterate in name order.
        headers.sort_by(|a, b| a.0.cmp(&b.0));
        let request = json!({
            "url": url,
            "method": "POST",
            "headers": headers.iter().map(|(k, v)| json!({"key": k, "value": v})).collect::<Vec<_>>(),
        });
        Ok(encode_uri_component(&request.to_string()))
    }
}

/// `AwsRequestSigner.getRequestOptions` for a body-less POST: `x-amz-date`, `authorization`,
/// `host` and (with a session token) `x-amz-security-token`.
fn sign_get_caller_identity(
    url: &str,
    region: &str,
    access_key_id: &str,
    secret: &str,
    token: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<(String, String)>, String> {
    let parsed = url::Url::parse(url).map_err(|e| e.to_string())?;
    let host = match parsed.port() {
        Some(port) => format!("{}:{port}", parsed.host_str().unwrap_or_default()),
        None => parsed.host_str().unwrap_or_default().to_string(),
    };
    let service = host.split('.').next().unwrap_or_default().to_string();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = &amz_date[..8];
    let mut signed: Vec<(&str, String)> = vec![("host", host.clone()), ("x-amz-date", amz_date.clone())];
    if let Some(token) = token {
        signed.push(("x-amz-security-token", token.to_string()));
    }
    signed.sort_by(|a, b| a.0.cmp(b.0));
    let canonical_headers: String = signed.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = signed.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(";");
    let payload_hash = hex::encode(Sha256::digest(b""));
    let canonical_request = format!("POST\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{payload_hash}", parsed.path(), parsed.query().unwrap_or_default());
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex::encode(Sha256::digest(canonical_request.as_bytes())));
    let hmac = |key: &[u8], data: &str| {
        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
        mac.update(data.as_bytes());
        mac.finalize().into_bytes().to_vec()
    };
    let k = hmac(format!("AWS4{secret}").as_bytes(), date);
    let k = hmac(&k, region);
    let k = hmac(&k, &service);
    let k = hmac(&k, "aws4_request");
    let signature = hex::encode(hmac(&k, &string_to_sign));
    let mut out = vec![
        ("x-amz-date".to_string(), amz_date),
        ("authorization".to_string(), format!("AWS4-HMAC-SHA256 Credential={access_key_id}/{scope}, SignedHeaders={signed_headers}, Signature={signature}")),
        ("host".to_string(), host),
    ];
    if let Some(token) = token {
        out.push(("x-amz-security-token".to_string(), token.to_string()));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Executable source
// ---------------------------------------------------------------------------------------------

const SAML: &str = "urn:ietf:params:oauth:token-type:saml2";
const OIDC_ID_TOKEN: &str = "urn:ietf:params:oauth:token-type:id_token";
const OIDC_JWT: &str = "urn:ietf:params:oauth:token-type:jwt";

/// `PluggableAuthClient` / `PluggableAuthHandler`.
struct Executable {
    command: Vec<String>,
    timeout: Duration,
    output_file: Option<String>,
}

/// `ExecutableResponse`.
struct ExecutableResponse {
    version: f64,
    success: bool,
    expiration_time: Option<f64>,
    subject_token: Option<String>,
    error_code: Option<String>,
    error_message: Option<String>,
}

impl ExecutableResponse {
    fn parse(json: &Value) -> Result<ExecutableResponse, String> {
        let version = json.get("version").filter(|v| super::truthy(v)).ok_or("Executable response must contain a 'version' field.")?;
        let success = json.get("success").ok_or("Executable response must contain a 'success' field.")?;
        let mut response = ExecutableResponse {
            version: version.as_f64().unwrap_or(f64::NAN),
            success: super::truthy(success),
            expiration_time: None,
            subject_token: None,
            error_code: None,
            error_message: None,
        };
        if response.success {
            response.expiration_time = json.get("expiration_time").and_then(Value::as_f64);
            let token_type = json.get("token_type").and_then(Value::as_str).unwrap_or_default();
            if ![SAML, OIDC_ID_TOKEN, OIDC_JWT].contains(&token_type) {
                return Err(format!(
                    "Executable response must contain a 'token_type' field when successful and it must be one of {OIDC_ID_TOKEN}, {OIDC_JWT}, or {SAML}."
                ));
            }
            let (key, message) = if token_type == SAML {
                ("saml_response", format!("Executable response must contain a 'saml_response' field when token_type={SAML}."))
            } else {
                ("id_token", format!("Executable response must contain a 'id_token' field when token_type={OIDC_ID_TOKEN} or {OIDC_JWT}."))
            };
            response.subject_token = Some(json.get(key).filter(|v| super::truthy(v)).map(js_string).ok_or(message)?);
        } else {
            response.error_code =
                Some(json.get("code").filter(|v| super::truthy(v)).map(js_string).ok_or("Executable response must contain a 'code' field when unsuccessful.")?);
            response.error_message = Some(
                json.get("message")
                    .filter(|v| super::truthy(v))
                    .map(js_string)
                    .ok_or("Executable response must contain a 'message' field when unsuccessful.")?,
            );
        }
        Ok(response)
    }

    fn expired(&self) -> bool {
        self.expiration_time.is_some_and(|t| t < (now_ms() / 1000.0).round())
    }
}

impl Executable {
    fn from_json(source: &Value) -> Result<Executable, String> {
        let executable = source.get("executable").cloned().unwrap_or(Value::Null);
        let invalid = "No valid Pluggable Auth \"credential_source\" provided.";
        let command = executable.get("command").filter(|v| super::truthy(v)).map(js_string).ok_or(invalid)?;
        let timeout = match executable.get("timeout_millis") {
            None => 30_000.0,
            Some(v) => {
                // `null` compares as 0; other non-numbers as NaN, which passes the range check.
                let ms = v.as_f64().unwrap_or(if v.is_null() { 0.0 } else { f64::NAN });
                if ms < 5000.0 || ms > 120_000.0 {
                    return Err("Timeout must be between 5000 and 120000 milliseconds.".into());
                }
                if ms.is_nan() { 1.0 } else { ms }
            }
        };
        let parts: Vec<String> = regex::Regex::new(r#"(?:[^\s"]+|"[^"]*")+"#)
            .unwrap()
            .find_iter(&command)
            .map(|m| {
                let s = m.as_str();
                if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
                    s[1..s.len() - 1].to_string()
                } else {
                    s.to_string()
                }
            })
            .collect();
        if parts.is_empty() {
            return Err(format!("Provided command: \"{command}\" could not be parsed."));
        }
        Ok(Executable {
            command: parts,
            timeout: Duration::from_millis(timeout as u64),
            output_file: executable.get("output_file").filter(|v| !v.is_null()).map(js_string),
        })
    }

    /// `retrieveCachedResponse`.
    fn cached(&self) -> Result<Option<ExecutableResponse>, String> {
        let Some(file) = self.output_file.as_ref().filter(|f| !f.is_empty()) else { return Ok(None) };
        let Ok(real) = std::fs::canonicalize(file) else { return Ok(None) };
        if !std::fs::symlink_metadata(&real).is_ok_and(|m| m.is_file()) {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&real).map_err(|e| e.to_string())?;
        if text.is_empty() {
            return Ok(None);
        }
        let json: Value = serde_json::from_str(&text).map_err(|_| format!("The output file contained an invalid response: {text}"))?;
        let response = ExecutableResponse::parse(&json)?;
        Ok((!response.expired() && response.success).then_some(response))
    }

    /// `retrieveResponseFromExecutable`.
    async fn run(&self, ctx: &Ctx, vars: Vec<(&str, String)>) -> Result<ExecutableResponse, String> {
        let mut command = tokio::process::Command::new(&self.command[0]);
        command.args(&self.command[1..]);
        ctx.env.apply(&mut command);
        command.envs(vars);
        command.stdin(std::process::Stdio::null()).kill_on_drop(true);
        let output = tokio::time::timeout(self.timeout, command.output())
            .await
            .map_err(|_| "The executable failed to finish within the timeout specified.".to_string())?
            .map_err(|e| e.to_string())?;
        let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        match output.status.code() {
            Some(0) => {
                let json: Value = serde_json::from_str(&text).map_err(|_| format!("The executable returned an invalid response: {text}"))?;
                ExecutableResponse::parse(&json)
            }
            code => Err(format!(
                "The executable failed with exit code: {} and error message: {text}.",
                code.map(|c| c.to_string()).unwrap_or_else(|| "null".into())
            )),
        }
    }

    /// `PluggableAuthClient.retrieveSubjectToken`.
    async fn subject_token(&self, ctx: &Ctx, audience: &str, token_type: Option<&str>, email: Option<&str>) -> Result<String, String> {
        if ctx.env.var("GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES").as_deref() != Some("1") {
            return Err("Pluggable Auth executables need to be explicitly allowed to run by setting the GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES environment Variable to 1.".into());
        }
        let mut response = if self.output_file.is_some() {
            self.cached()?
        } else {
            None
        };
        if response.is_none() {
            // `spawn` leaves out variables whose value is `undefined`.
            let mut vars = vec![("GOOGLE_EXTERNAL_ACCOUNT_AUDIENCE", audience.to_string())];
            if let Some(kind) = token_type {
                vars.push(("GOOGLE_EXTERNAL_ACCOUNT_TOKEN_TYPE", kind.to_string()));
            }
            vars.push(("GOOGLE_EXTERNAL_ACCOUNT_INTERACTIVE", "0".to_string()));
            if let Some(file) = &self.output_file {
                vars.push(("GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE", file.clone()));
            }
            if let Some(email) = email {
                vars.push(("GOOGLE_EXTERNAL_ACCOUNT_IMPERSONATED_EMAIL", email.to_string()));
            }
            response = Some(self.run(ctx, vars).await?);
        }
        let response = response.unwrap();
        if response.version > 1.0 {
            return Err("Version of executable is not currently supported, maximum supported version is 1.".into());
        }
        if !response.success {
            return Err(format!(
                "The executable failed with exit code: {} and error message: {}.",
                response.error_code.unwrap_or_default(),
                response.error_message.unwrap_or_default()
            ));
        }
        if self.output_file.is_some() && response.expiration_time.is_none() {
            return Err("The executable response must contain the `expiration_time` field for successful responses when an output_file has been specified in the configuration.".into());
        }
        if response.expired() {
            return Err("Executable response is expired.".into());
        }
        Ok(response.subject_token.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_caller_identity_signature() {
        // The AWS SigV4 test-suite style check: same inputs, same signature, signed headers sorted.
        let now = chrono::DateTime::parse_from_rfc3339("2020-08-11T06:55:22Z").unwrap().with_timezone(&chrono::Utc);
        let headers = sign_get_caller_identity(
            "https://sts.us-east-2.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15",
            "us-east-2",
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            Some("TOKEN"),
            now,
        )
        .unwrap();
        let auth = &headers.iter().find(|(k, _)| k == "authorization").unwrap().1;
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20200811/us-east-2/sts/aws4_request, SignedHeaders=host;x-amz-date;x-amz-security-token, Signature="
        ));
        assert_eq!(headers.iter().find(|(k, _)| k == "host").unwrap().1, "sts.us-east-2.amazonaws.com");
    }

    #[test]
    fn executable_command_parsing() {
        let e = Executable::from_json(&json!({"executable": {"command": "/bin/run \"a b\" c"}})).unwrap();
        assert_eq!(e.command, ["/bin/run", "a b", "c"]);
        assert!(Executable::from_json(&json!({"executable": {"command": "x", "timeout_millis": 10}})).is_err());
    }
}
