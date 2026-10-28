//! Google Application Default Credentials as `providers/google-vertex-shared.ts` loads them:
//! `google-auth-library` 10.5.0 `GoogleAuth({ projectId, scopes: [cloud-platform] }).getClient()`
//! with upstream's `CloudSDKAuth` subclass (the well-known file honours `CLOUDSDK_CONFIG`), then
//! `getAccessToken()` per request.
//!
//! Discovery: `GOOGLE_APPLICATION_CREDENTIALS`, the well-known file
//! (`$CLOUDSDK_CONFIG/application_default_credentials.json`, else
//! `$HOME/.config/gcloud/application_default_credentials.json`), then the GCE metadata server
//! (`gcp-metadata` 8.1.4 detection: residency, `METADATA_SERVER_DETECTION`, `GCE_METADATA_HOST`).
//! Credential types: `authorized_user`, `service_account`, `impersonated_service_account`,
//! `external_account` (workload / workforce identity federation, see [`external`]) and
//! `external_account_authorized_user`. Tokens are cached and refreshed 5 minutes before expiry.
//! The client is loaded once per auth instance (a failed load stays failed, as upstream caches the
//! rejected promise); every failure is `MissingCredentialError("Google Application Default
//! Credentials")`.

pub mod external;

use crate::protocols::cloud_env::Env;
use crate::route::auth::{self, AuthFailure};
use crate::route::{Auth, Credential};
use base64::Engine;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const SOURCE: &str = "Google Application Default Credentials";
/// `GOOGLE_TOKEN_URL` (gtoken) and `OAuth2Client` `oauth2TokenUrl`.
pub const OAUTH2_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const DEFAULT_UNIVERSE: &str = "googleapis.com";
/// `DEFAULT_EAGER_REFRESH_THRESHOLD_MILLIS` / `EXPIRATION_TIME_OFFSET`.
const EAGER_REFRESH_MS: f64 = 5.0 * 60.0 * 1000.0;
const NO_ADC: &str = "Could not load the default credentials. Browse to https://cloud.google.com/docs/authentication/getting-started for more information.";
/// `process.version` of Bun 1.4.4.
pub(crate) const NODE_VERSION: &str = "26.3.0";
pub(crate) const LIBRARY_VERSION: &str = "10.5.0";

fn missing() -> AuthFailure {
    AuthFailure::missing(SOURCE)
}

pub(crate) fn now_ms() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64
}

/// Fixed Google endpoints that tests point at local servers. Production uses the defaults.
#[derive(Clone, Debug, Default)]
pub struct Endpoints {
    /// `https://oauth2.googleapis.com/token`.
    pub oauth2_token: Option<String>,
    /// `https://iamcredentials.<universe>` for `impersonated_service_account`.
    pub iam_credentials: Option<String>,
    /// Trust for mTLS requests (certificate-sourced credentials); the process trust when unset.
    pub trust: Option<oc_util::http::Trust>,
}

impl Endpoints {
    fn oauth2_token(&self) -> String {
        self.oauth2_token.clone().unwrap_or_else(|| OAUTH2_TOKEN_URL.into())
    }
}

/// What every client needs to make requests.
#[derive(Clone)]
pub struct Ctx {
    pub env: Env,
    pub endpoints: Endpoints,
    pub http: reqwest::Client,
}

impl Ctx {
    pub fn new(env: Env, endpoints: Endpoints) -> Ctx {
        Ctx { env, endpoints, http: oc_util::http::client() }
    }
}

// ---------------------------------------------------------------------------------------------
// gaxios
// ---------------------------------------------------------------------------------------------

/// A gaxios request.
pub(crate) struct Request {
    pub method: reqwest::Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Body>,
    /// `RETRY_CONFIG` (`retry: true`, every method retried); otherwise only `no_response_retries`
    /// with gaxios' default methods (GET, HEAD, PUT, OPTIONS, DELETE).
    pub retry: bool,
    pub no_response_retries: Option<u32>,
    pub timeout: Option<Duration>,
}

pub(crate) enum Body {
    Form(Vec<(String, String)>),
    Json(Value),
}

impl Request {
    pub fn new(method: reqwest::Method, url: impl Into<String>) -> Request {
        Request { method, url: url.into(), headers: Vec::new(), body: None, retry: false, no_response_retries: None, timeout: None }
    }
    pub fn retry(mut self) -> Request {
        self.retry = true;
        self
    }
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Request {
        let name = name.to_ascii_lowercase();
        self.headers.retain(|(k, _)| *k != name);
        self.headers.push((name, value.into()));
        self
    }
    pub fn form(mut self, fields: Vec<(String, String)>) -> Request {
        self.body = Some(Body::Form(fields));
        self
    }
    pub fn json(mut self, value: Value) -> Request {
        self.body = Some(Body::Json(value));
        self
    }
}

/// A gaxios response: `data` is the parsed JSON body, else the text.
pub(crate) struct Response {
    pub headers: reqwest::header::HeaderMap,
    pub data: Value,
}

/// A `GaxiosError` (`status` and `data` when there was a response).
#[derive(Debug)]
pub(crate) struct GaxiosError {
    pub message: String,
    pub status: Option<u16>,
    pub data: Option<Value>,
}

impl std::fmt::Display for GaxiosError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn parse_data(text: String) -> Value {
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

fn form_encode(fields: &[(String, String)]) -> String {
    url::form_urlencoded::Serializer::new(String::new()).extend_pairs(fields).finish()
}

/// `transporter.request(opts)` with gaxios' retry rules (delays 100 ms, 500 ms, 1.5 s).
pub(crate) async fn send(http: &reqwest::Client, request: &Request) -> Result<Response, GaxiosError> {
    let retries = if request.retry || request.no_response_retries.is_some() {
        3
    } else {
        0
    };
    let no_response_retries = request.no_response_retries.unwrap_or(2);
    let method_retried = request.retry || ["GET", "HEAD", "PUT", "OPTIONS", "DELETE"].contains(&request.method.as_str());
    let mut attempt = 0u32;
    loop {
        let mut builder = http.request(request.method.clone(), &request.url);
        let mut has_user_agent = false;
        let mut has_api_client = false;
        for (k, v) in &request.headers {
            has_user_agent |= k == "user-agent";
            has_api_client |= k == "x-goog-api-client";
            builder = builder.header(k.as_str(), v.as_str());
        }
        if !has_api_client {
            builder = builder.header("x-goog-api-client", format!("gl-node/{NODE_VERSION}"));
        }
        if !has_user_agent {
            builder = builder.header("user-agent", format!("google-api-nodejs-client/{LIBRARY_VERSION}"));
        }
        match &request.body {
            Some(Body::Form(fields)) => builder = builder.header("content-type", "application/x-www-form-urlencoded").body(form_encode(fields)),
            Some(Body::Json(value)) => {
                if !request.headers.iter().any(|(k, _)| k == "content-type") {
                    builder = builder.header("content-type", "application/json");
                }
                builder = builder.body(value.to_string());
            }
            None => {}
        }
        if let Some(timeout) = request.timeout {
            builder = builder.timeout(timeout);
        }
        let error = match builder.send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                let headers = response.headers().clone();
                let text = response.text().await.unwrap_or_default();
                let data = parse_data(text);
                if (200..300).contains(&status) {
                    return Ok(Response { headers, data });
                }
                GaxiosError { message: format!("Request failed with status code {status}"), status: Some(status), data: Some(data) }
            }
            Err(e) => GaxiosError { message: e.to_string(), status: None, data: None },
        };
        let retryable = method_retried
            && attempt < retries
            && match error.status {
                Some(s) => (100..200).contains(&s) || s == 408 || s == 429 || (500..600).contains(&s),
                None => attempt < no_response_retries,
            };
        if !retryable {
            return Err(error);
        }
        let delay = if attempt == 0 {
            100.0
        } else {
            (2f64.powi(attempt as i32) - 1.0) / 2.0 * 1000.0
        };
        attempt += 1;
        tokio::time::sleep(Duration::from_millis(delay as u64)).await;
    }
}

/// `getErrorFromOAuthErrorResponse`.
pub(crate) fn oauth_error(error: GaxiosError) -> String {
    match error.data.as_ref().filter(|_| error.status.is_some()) {
        Some(data) => {
            let code = data.get("error").map(js_string).unwrap_or_else(|| "undefined".into());
            let mut message = format!("Error code {code}");
            if let Some(d) = data.get("error_description") {
                message.push_str(&format!(": {}", js_string(d)));
            }
            if let Some(u) = data.get("error_uri") {
                message.push_str(&format!(" - {}", js_string(u)));
            }
            message
        }
        None => error.message,
    }
}

pub(crate) fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}

/// `originalOrCamelOptions(obj).get(key)`: the snake_case key, else its camelCase form (`??`).
pub(crate) fn opt<'a>(obj: &'a Value, key: &str) -> Option<&'a Value> {
    let get = |k: &str| obj.get(k).filter(|v| !v.is_null());
    get(key).or_else(|| {
        let mut camel = String::new();
        let mut upper = false;
        let chars: Vec<char> = key.chars().collect();
        for (i, c) in chars.iter().enumerate() {
            if *c == '_' && chars.get(i + 1).is_some_and(|n| *n != '_') {
                upper = true;
            } else if upper {
                camel.extend(c.to_uppercase());
                upper = false;
            } else {
                camel.push(*c);
            }
        }
        get(&camel)
    })
}

/// A JS truthy string value.
pub(crate) fn opt_str(obj: &Value, key: &str) -> Option<String> {
    opt(obj, key).map(js_string).filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------------------------
// Clients
// ---------------------------------------------------------------------------------------------

/// `credentials` of an `OAuth2Client`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Cached {
    pub access_token: Option<String>,
    pub expiry_date: Option<f64>,
}

impl Cached {
    /// `isTokenExpiring` (expiry within the eager threshold).
    fn expiring(&self) -> bool {
        self.expiry_date.is_some_and(|e| e != 0.0 && !e.is_nan() && e <= now_ms() + EAGER_REFRESH_MS)
    }
    fn usable(&self) -> Option<String> {
        self.access_token.clone().filter(|_| !self.expiring())
    }
}

/// An ADC client.
pub enum Client {
    User(UserRefresh),
    ServiceAccount(ServiceAccount),
    Compute(Compute),
    Impersonated(Box<Impersonated>),
    External(Box<external::External>),
    ExternalAuthorizedUser(ExternalAuthorizedUser),
}

impl Client {
    /// `getAccessToken().token`.
    pub fn access_token<'a>(&'a mut self, ctx: &'a Ctx) -> futures::future::BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            match self {
                Client::User(c) => c.access_token(ctx).await,
                Client::ServiceAccount(c) => c.access_token(ctx).await,
                Client::Compute(c) => c.access_token(ctx).await,
                Client::Impersonated(c) => c.access_token(ctx).await,
                Client::External(c) => c.access_token(ctx).await,
                Client::ExternalAuthorizedUser(c) => c.access_token(ctx).await,
            }
        })
    }

    /// `quotaProjectId` (sent as `x-goog-user-project` on requests made with this client).
    fn quota_project(&self) -> Option<String> {
        match self {
            Client::User(c) => c.quota_project.clone(),
            Client::ServiceAccount(c) => c.quota_project.clone(),
            Client::ExternalAuthorizedUser(c) => c.quota_project.clone(),
            Client::External(c) => c.quota_project(),
            _ => None,
        }
    }

    fn universe_domain(&self) -> String {
        match self {
            Client::User(c) => c.universe.clone(),
            Client::ServiceAccount(c) => c.universe.clone(),
            Client::Impersonated(c) => c.universe.clone(),
            Client::External(c) => c.universe(),
            Client::ExternalAuthorizedUser(c) => c.universe.clone(),
            Client::Compute(_) => DEFAULT_UNIVERSE.into(),
        }
    }
}

/// `UserRefreshClient` (`authorized_user`).
pub struct UserRefresh {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    quota_project: Option<String>,
    universe: String,
    cached: Cached,
}

impl UserRefresh {
    fn from_json(json: &Value) -> Result<UserRefresh, String> {
        let field = |k: &str| json.get(k).map(js_string).filter(|s| !s.is_empty());
        let client_id = field("client_id").ok_or("The incoming JSON object does not contain a client_id field")?;
        let client_secret = field("client_secret").ok_or("The incoming JSON object does not contain a client_secret field")?;
        let refresh_token = field("refresh_token").ok_or("The incoming JSON object does not contain a refresh_token field")?;
        Ok(UserRefresh {
            client_id,
            client_secret,
            refresh_token,
            quota_project: field("quota_project_id"),
            universe: field("universe_domain").unwrap_or_else(|| DEFAULT_UNIVERSE.into()),
            cached: Cached::default(),
        })
    }

    async fn access_token(&mut self, ctx: &Ctx) -> Result<String, String> {
        if let Some(token) = self.cached.usable() {
            return Ok(token);
        }
        let request = Request::new(reqwest::Method::POST, ctx.endpoints.oauth2_token()).retry().form(vec![
            ("refresh_token".into(), self.refresh_token.clone()),
            ("client_id".into(), self.client_id.clone()),
            ("client_secret".into(), self.client_secret.clone()),
            ("grant_type".into(), "refresh_token".into()),
        ]);
        let response = send(&ctx.http, &request).await.map_err(|e| e.message)?;
        self.cached = tokens(&response.data);
        self.cached.access_token.clone().ok_or_else(|| "Could not refresh access token.".to_string())
    }
}

/// The `tokens` of a token response (`expires_in` → `expiry_date`).
fn tokens(data: &Value) -> Cached {
    let expires_in = data.get("expires_in").and_then(Value::as_f64).filter(|e| *e != 0.0);
    Cached { access_token: data.get("access_token").map(js_string).filter(|t| !t.is_empty()), expiry_date: expires_in.map(|e| now_ms() + e * 1000.0) }
}

/// `JWT` (`service_account`), whose `getAccessToken` goes through gtoken: an RS256 assertion
/// posted to `https://oauth2.googleapis.com/token` (the file's `token_uri` is not used).
pub struct ServiceAccount {
    email: String,
    key: String,
    quota_project: Option<String>,
    universe: String,
    cached: Cached,
}

impl ServiceAccount {
    fn from_json(json: &Value) -> Result<ServiceAccount, String> {
        let field = |k: &str| json.get(k).map(js_string).filter(|s| !s.is_empty());
        let email = field("client_email").ok_or("The incoming JSON object does not contain a client_email field")?;
        let key = field("private_key").ok_or("The incoming JSON object does not contain a private_key field")?;
        Ok(ServiceAccount {
            email,
            key,
            quota_project: field("quota_project_id"),
            universe: field("universe_domain").unwrap_or_else(|| DEFAULT_UNIVERSE.into()),
            cached: Cached::default(),
        })
    }

    async fn access_token(&mut self, ctx: &Ctx) -> Result<String, String> {
        if let Some(token) = self.cached.usable() {
            return Ok(token);
        }
        let now = chrono::Utc::now().timestamp() as u64;
        let assertion = service_account_assertion(&self.email, &self.key, OAUTH2_TOKEN_URL, now)?;
        let request = Request::new(reqwest::Method::POST, ctx.endpoints.oauth2_token())
            .form(vec![("grant_type".into(), "urn:ietf:params:oauth:grant-type:jwt-bearer".into()), ("assertion".into(), assertion)]);
        let response = send(&ctx.http, &request).await.map_err(|e| e.message)?;
        self.cached = tokens(&response.data);
        self.cached.access_token.clone().ok_or_else(|| "Could not refresh access token.".to_string())
    }
}

fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// gtoken's signed assertion: header `{"alg":"RS256"}`, claims `iss`, `scope`, `aud`, `exp`, `iat`.
pub fn service_account_assertion(client_email: &str, private_key_pem: &str, audience: &str, now: u64) -> Result<String, String> {
    use rsa::pkcs1::DecodeRsaPrivateKey;
    use rsa::pkcs1v15::SigningKey;
    use rsa::pkcs8::DecodePrivateKey;
    use rsa::signature::{SignatureEncoding, Signer};
    let header = b64url(br#"{"alg":"RS256"}"#);
    let claims = json!({"iss": client_email, "scope": SCOPE, "aud": audience, "exp": now + 3600, "iat": now});
    let payload = b64url(claims.to_string().as_bytes());
    let input = format!("{header}.{payload}");
    let key =
        rsa::RsaPrivateKey::from_pkcs8_pem(private_key_pem).or_else(|_| rsa::RsaPrivateKey::from_pkcs1_pem(private_key_pem)).map_err(|e| e.to_string())?;
    let signer = SigningKey::<sha2::Sha256>::new(key);
    let signature = signer.sign(input.as_bytes()).to_bytes();
    Ok(format!("{input}.{}", b64url(&signature)))
}

/// `Compute`: the metadata server's default service account token.
pub struct Compute {
    cached: Cached,
}

impl Compute {
    async fn access_token(&mut self, ctx: &Ctx) -> Result<String, String> {
        if let Some(token) = self.cached.usable() {
            return Ok(token);
        }
        let query = form_encode(&[("scopes".into(), SCOPE.into())]);
        let url = format!("{}/instance/service-accounts/default/token?{query}", metadata_base(&ctx.env));
        let data = metadata_get(ctx, &url, Some(3)).await.map_err(|e| format!("Could not refresh access token: {e}"))?;
        self.cached = tokens(&data);
        self.cached.access_token.clone().ok_or_else(|| "Could not refresh access token.".to_string())
    }
}

/// `getBaseUrl` of gcp-metadata: `GCE_METADATA_IP`, `GCE_METADATA_HOST`, else 169.254.169.254.
fn metadata_base(env: &Env) -> String {
    metadata_base_for(&env.non_empty("GCE_METADATA_IP").or_else(|| env.non_empty("GCE_METADATA_HOST")).unwrap_or_else(|| "http://169.254.169.254".into()))
}

fn metadata_base_for(host: &str) -> String {
    let base = if host.starts_with("http://") || host.starts_with("https://") {
        host.to_string()
    } else {
        format!("http://{host}")
    };
    match url::Url::parse(&base).and_then(|u| u.join("/computeMetadata/v1")) {
        Ok(url) => url.to_string(),
        Err(_) => format!("{}/computeMetadata/v1", base.trim_end_matches('/')),
    }
}

/// `requestTimeout()`: none on GCP, else 3 s.
fn metadata_timeout(env: &Env) -> Option<Duration> {
    if gcp_residency(env) {
        None
    } else {
        Some(Duration::from_secs(3))
    }
}

/// `metadataAccessor`: `Metadata-Flavor: Google` both ways; the body as JSON when it parses.
async fn metadata_get(ctx: &Ctx, url: &str, no_response_retries: Option<u32>) -> Result<Value, String> {
    let mut request = Request::new(reqwest::Method::GET, url).header("metadata-flavor", "Google");
    request.no_response_retries = no_response_retries;
    request.timeout = metadata_timeout(&ctx.env);
    let response = send(&ctx.http, &request).await.map_err(|e| e.message)?;
    match response.headers.get("metadata-flavor").and_then(|v| v.to_str().ok()) {
        Some("Google") => Ok(response.data),
        Some(other) => Err(format!("Invalid response from metadata service: incorrect Metadata-Flavor header. Expected 'Google', got '{other}'")),
        None => Err("Invalid response from metadata service: incorrect Metadata-Flavor header. Expected 'Google', got no header".into()),
    }
}

/// `detectGCPResidency`: serverless variables, a Google BIOS, or a `42:01` MAC address.
fn gcp_residency(env: &Env) -> bool {
    if let Some(cached) = *env.state().gcp_residency.lock() {
        return cached;
    }
    let serverless = env.non_empty("CLOUD_RUN_JOB").is_some() || env.non_empty("FUNCTION_NAME").is_some() || env.non_empty("K_SERVICE").is_some();
    let residency = serverless || gce_linux() || gce_mac();
    *env.state().gcp_residency.lock() = Some(residency);
    residency
}

fn gce_linux() -> bool {
    cfg!(target_os = "linux")
        && std::fs::metadata("/sys/class/dmi/id/bios_date").is_ok()
        && std::fs::read_to_string("/sys/class/dmi/id/bios_vendor").is_ok_and(|v| v.contains("Google"))
}

fn gce_mac() -> bool {
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else { return false };
    entries.flatten().any(|e| std::fs::read_to_string(e.path().join("address")).is_ok_and(|a| a.trim().starts_with("42:01")))
}

/// `_checkIsGCE`: residency, else `isAvailable()` (cached).
async fn is_gce(ctx: &Ctx) -> Result<bool, String> {
    if gcp_residency(&ctx.env) {
        return Ok(true);
    }
    if let Some(value) = ctx.env.non_empty("METADATA_SERVER_DETECTION") {
        match value.trim().to_lowercase().as_str() {
            "assume-present" => return Ok(true),
            "none" => return Ok(false),
            "bios-only" => return Ok(gcp_residency(&ctx.env)),
            "ping-only" => {}
            other => {
                return Err(format!(
                    "Unknown `METADATA_SERVER_DETECTION` env variable. Got `{other}`, but it should be `assume-present`, `none`, `bios-only`, `ping-only`, or unset"
                ));
            }
        }
    }
    let available = ctx
        .env
        .state()
        .gce_available
        .get_or_init(|| async {
            let explicit = ctx.env.non_empty("GCE_METADATA_IP").or_else(|| ctx.env.non_empty("GCE_METADATA_HOST")).is_some();
            let retries = ctx.env.non_empty("DETECT_GCP_RETRIES").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
            let primary = format!("{}/instance", metadata_base(&ctx.env));
            let ping = |url: String| async move {
                let mut request = Request::new(reqwest::Method::GET, url).header("metadata-flavor", "Google");
                request.no_response_retries = Some(retries);
                request.timeout = metadata_timeout(&ctx.env);
                let response = send(&ctx.http, &request).await.ok()?;
                (response.headers.get("metadata-flavor").and_then(|v| v.to_str().ok()) == Some("Google")).then_some(())
            };
            if explicit {
                ping(primary).await.is_some()
            } else {
                let secondary = format!("{}/instance", metadata_base_for("http://metadata.google.internal."));
                let a = Box::pin(ping(primary));
                let b = Box::pin(ping(secondary));
                // `Promise.any`: the first success wins.
                match futures::future::select(a, b).await {
                    futures::future::Either::Left((Some(()), _)) | futures::future::Either::Right((Some(()), _)) => true,
                    futures::future::Either::Left((None, other)) => other.await.is_some(),
                    futures::future::Either::Right((None, other)) => other.await.is_some(),
                }
            }
        })
        .await;
    Ok(*available)
}

/// `Impersonated` (`impersonated_service_account`).
pub struct Impersonated {
    source: Box<Client>,
    target_principal: String,
    delegates: Value,
    lifetime: Value,
    universe: String,
    cached: Cached,
}

impl Impersonated {
    fn from_json(json: &Value, ctx: &Ctx) -> Result<Impersonated, String> {
        let source_json = json.get("source_credentials").filter(|v| truthy(v)).ok_or("The incoming JSON object does not contain a source_credentials field")?;
        let url = json
            .get("service_account_impersonation_url")
            .map(js_string)
            .filter(|s| !s.is_empty())
            .ok_or("The incoming JSON object does not contain a service_account_impersonation_url field")?;
        let source = from_json(source_json, ctx)?;
        if url.chars().count() > 256 {
            return Err(format!("Target principal is too long: {url}"));
        }
        let target = regex::Regex::new(r"([^/]+):(generateAccessToken|generateIdToken)$")
            .unwrap()
            .captures(&url)
            .map(|c| c[1].to_string())
            .ok_or_else(|| format!("Cannot extract target principal from {url}"))?;
        let explicit = opt_str(json, "universe_domain");
        let universe = match explicit {
            None => source.universe_domain(),
            Some(u) if u != source.universe_domain() => {
                return Err(format!(
                    "Universe domain {} in source credentials does not match {u} universe domain set for impersonated credentials.",
                    source.universe_domain()
                ));
            }
            Some(u) => u,
        };
        Ok(Impersonated {
            source: Box::new(source),
            target_principal: target,
            delegates: json.get("delegates").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!([])),
            lifetime: json.get("lifetime").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!(3600)),
            universe,
            cached: Cached { access_token: None, expiry_date: Some(1.0) },
        })
    }

    async fn access_token(&mut self, ctx: &Ctx) -> Result<String, String> {
        if let Some(token) = self.cached.access_token.clone().filter(|_| !self.cached.expiring()) {
            return Ok(token);
        }
        let source_token = self.source.access_token(ctx).await.map_err(|e| format!("unable to impersonate: Error: {e}"))?;
        let endpoint = ctx.endpoints.iam_credentials.clone().unwrap_or_else(|| format!("https://iamcredentials.{}", self.universe));
        let url = format!("{endpoint}/v1/projects/-/serviceAccounts/{}:generateAccessToken", self.target_principal);
        let lifetime = format!("{}s", js_string(&self.lifetime));
        let mut request = Request::new(reqwest::Method::POST, url)
            .retry()
            .json(json!({"delegates": self.delegates, "scope": [SCOPE], "lifetime": lifetime}))
            .header("authorization", format!("Bearer {source_token}"));
        if let Some(project) = self.source.quota_project() {
            request = request.header("x-goog-user-project", project);
        }
        let response = send(&ctx.http, &request).await.map_err(|e| format!("unable to impersonate: {e}"))?;
        let token = response.data.get("accessToken").map(js_string);
        let expiry = response.data.get("expireTime").and_then(Value::as_str).and_then(crate::js_date::parse).unwrap_or(f64::NAN);
        self.cached = Cached { access_token: token.clone(), expiry_date: Some(expiry) };
        token.filter(|t| !t.is_empty()).ok_or_else(|| "Could not refresh access token.".to_string())
    }
}

/// `ExternalAccountAuthorizedUserClient` (`external_account_authorized_user`).
pub struct ExternalAuthorizedUser {
    token_url: String,
    client_id: Option<String>,
    client_secret: Option<String>,
    refresh_token: Value,
    quota_project: Option<String>,
    universe: String,
    cached: Option<Cached>,
}

impl ExternalAuthorizedUser {
    fn from_json(json: &Value) -> ExternalAuthorizedUser {
        let universe = json.get("universe_domain").map(js_string).filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_UNIVERSE.into());
        ExternalAuthorizedUser {
            token_url: json.get("token_url").filter(|v| !v.is_null()).map(js_string).unwrap_or_else(|| format!("https://sts.{universe}/v1/oauthtoken")),
            client_id: json.get("client_id").filter(|v| !v.is_null()).map(js_string),
            client_secret: json.get("client_secret").filter(|v| !v.is_null()).map(js_string),
            refresh_token: json.get("refresh_token").cloned().unwrap_or(Value::Null),
            quota_project: json.get("quota_project_id").filter(|v| !v.is_null()).map(js_string),
            universe,
            cached: None,
        }
    }

    async fn access_token(&mut self, ctx: &Ctx) -> Result<String, String> {
        if let Some(cached) = &self.cached
            && !cached.expiring()
        {
            return cached.access_token.clone().ok_or_else(|| "empty token".to_string());
        }
        let refresh = match &self.refresh_token {
            Value::Null => "undefined".to_string(),
            other => js_string(other),
        };
        let basic = format!(
            "{}:{}",
            self.client_id.clone().unwrap_or_else(|| "undefined".into()),
            self.client_secret.clone().filter(|s| !s.is_empty()).unwrap_or_default()
        );
        let request = Request::new(reqwest::Method::POST, self.token_url.clone())
            .retry()
            .form(vec![("grant_type".into(), "refresh_token".into()), ("refresh_token".into(), refresh)])
            .header("authorization", format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(basic)));
        let response = send(&ctx.http, &request).await.map_err(oauth_error)?;
        let expires_in = response.data.get("expires_in").and_then(Value::as_f64).unwrap_or(f64::NAN);
        let cached = Cached { access_token: response.data.get("access_token").map(js_string), expiry_date: Some(now_ms() + expires_in * 1000.0) };
        if let Some(refresh) = response.data.get("refresh_token") {
            self.refresh_token = refresh.clone();
        }
        let token = cached.access_token.clone();
        self.cached = Some(cached);
        token.filter(|t| !t.is_empty()).ok_or_else(|| "Google ADC returned an empty access token".to_string())
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

// ---------------------------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------------------------

/// `GoogleAuth.fromJSON`.
pub fn from_json(json: &Value, ctx: &Ctx) -> Result<Client, String> {
    match json.get("type").and_then(Value::as_str) {
        Some("authorized_user") => UserRefresh::from_json(json).map(Client::User),
        Some("impersonated_service_account") => Impersonated::from_json(json, ctx).map(|c| Client::Impersonated(Box::new(c))),
        Some("external_account") => external::External::from_json(json, ctx).map(|c| Client::External(Box::new(c))),
        Some("external_account_authorized_user") => Ok(Client::ExternalAuthorizedUser(ExternalAuthorizedUser::from_json(json))),
        _ => ServiceAccount::from_json(json).map(Client::ServiceAccount),
    }
}

/// `_getApplicationCredentialsFromFilePath`.
fn from_file(path: &Path, ctx: &Ctx) -> Result<Client, String> {
    let real = std::fs::canonicalize(path).map_err(|e| format!("The file at {} does not exist, or it is not a file. {e}", path.display()))?;
    if !std::fs::symlink_metadata(&real).is_ok_and(|m| m.is_file()) {
        return Err(format!("The file at {} does not exist, or it is not a file. ", real.display()));
    }
    let text = std::fs::read_to_string(&real).map_err(|e| e.to_string())?;
    let json: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    from_json(&json, ctx)
}

/// The well-known ADC file of upstream's `CloudSDKAuth`: `$CLOUDSDK_CONFIG` (with `~` expanded) when
/// set, else `$HOME/.config/gcloud` (`%APPDATA%\gcloud` on Windows). `None` when the file is absent.
pub fn well_known_file(env: &Env) -> Option<PathBuf> {
    if let Some(config) = env.non_empty("CLOUDSDK_CONFIG") {
        let dir = if config == "~" {
            env.os_home()
        } else if let Some(rest) = config.strip_prefix("~/") {
            env.os_home().join(rest)
        } else {
            PathBuf::from(config)
        };
        let file = dir.join("application_default_credentials.json");
        return file.exists().then_some(file);
    }
    let base = if cfg!(windows) {
        PathBuf::from(env.non_empty("APPDATA")?)
    } else {
        PathBuf::from(env.non_empty("HOME")?).join(".config")
    };
    let file = base.join("gcloud").join("application_default_credentials.json");
    file.exists().then_some(file)
}

/// Locate the ADC JSON file (`GOOGLE_APPLICATION_CREDENTIALS`, else the well-known file).
pub fn credentials_file() -> Option<PathBuf> {
    let env = Env::process();
    env.non_empty("GOOGLE_APPLICATION_CREDENTIALS")
        .or_else(|| env.non_empty("google_application_credentials"))
        .map(PathBuf::from)
        .or_else(|| well_known_file(&env))
}

/// `getApplicationDefaultAsync`.
pub async fn load(ctx: &Ctx) -> Result<Client, String> {
    let env = &ctx.env;
    if let Some(path) = env.non_empty("GOOGLE_APPLICATION_CREDENTIALS").or_else(|| env.non_empty("google_application_credentials")) {
        return from_file(Path::new(&path), ctx)
            .map_err(|e| format!("Unable to read the credential file specified by the GOOGLE_APPLICATION_CREDENTIALS environment variable: {e}"));
    }
    if let Some(file) = well_known_file(env) {
        return from_file(&file, ctx);
    }
    if is_gce(ctx).await? {
        return Ok(Client::Compute(Compute { cached: Cached { access_token: None, expiry_date: Some(1.0) } }));
    }
    Err(NO_ADC.into())
}

/// The ADC auth of one route: the client is loaded on first use and kept (with its token cache).
pub struct Adc {
    ctx: Ctx,
    client: tokio::sync::Mutex<Option<Result<Client, String>>>,
}

impl Adc {
    pub fn new(env: Env, endpoints: Endpoints) -> Adc {
        Adc { ctx: Ctx::new(env, endpoints), client: tokio::sync::Mutex::new(None) }
    }

    /// `(await loadClient()).getAccessToken()`, empty tokens rejected.
    pub async fn token(&self) -> Result<String, String> {
        let mut guard = self.client.lock().await;
        if guard.is_none() {
            *guard = Some(load(&self.ctx).await);
        }
        match guard.as_mut().unwrap() {
            Ok(client) => {
                let token = client.access_token(&self.ctx).await?;
                if token.is_empty() {
                    Err("Google ADC returned an empty access token".into())
                } else {
                    Ok(token)
                }
            }
            Err(e) => Err(e.clone()),
        }
    }
}

/// `Authorization: Bearer <ADC token>`.
pub fn google_adc_auth(_project: Option<String>) -> Arc<dyn Auth> {
    let adc = Arc::new(Adc::new(Env::process(), Endpoints::default()));
    auth::bearer(Credential::dynamic(move || {
        let adc = adc.clone();
        Box::pin(async move {
            adc.token().await.map_err(|e| {
                tracing::debug!("google adc: {e}");
                missing()
            })
        })
    }))
}
