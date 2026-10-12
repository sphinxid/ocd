//! OAuth for remote MCP servers (`packages/core/src/mcp/oauth.ts` plus the SDK `auth()`
//! orchestrator it relies on).

pub mod callback;
pub mod discovery;
pub mod fetch;
pub mod page;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use parking_lot::Mutex;
use rand::Rng;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::config::RemoteConfig;
use crate::error::McpError;
pub use callback::{AuthorizeHandle, AuthorizeOptions, authorize};
use discovery::{AuthServerMetadata, DiscoveryState, ResourceMetadata};
use fetch::{Body, Fetcher, HttpRequest};

/// Client ID Metadata Document URL used as `client_id` when the AS supports CIMD.
pub const CLIENT_METADATA_URL: &str = "https://opencode.ai/oauth/opencode/client.json";

/// One row of the SQLite `credential` table as far as MCP cares.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredCredential {
    pub id: String,
    pub integration_id: String,
    pub label: String,
    /// The JSON `value` column (`Credential.Value`). For MCP OAuth it has the shape documented on
    /// [`McpCredentialStore`].
    pub value: Value,
}

/// Credential persistence that core implements over the global SQLite `credential` table.
///
/// The `value` stored for an MCP OAuth credential (`Credential.OAuth`, built by `toCredential`):
///
/// ```json
/// { "type": "oauth", "methodID": "mcp_<16hex>",
///   "access": "<access_token>", "refresh": "<refresh_token or ''>",
///   "expires": 1730000000000,            // epoch ms = now + expires_in*1000; 0 = non-expiring
///   "metadata": { "serverUrl": "<config url>", "tokenType": "Bearer",
///                 "scope": "...",          // optional
///                 "issuer": "...",         // optional, the authorization server issuer
///                 "client": { "client_id": "...", "client_secret": "...", "issuer": "...", ... } } }
/// ```
///
/// `metadata.client` is the (dynamically registered / CIMD) client information as returned by
/// the authorization server, stamped with `issuer`.
#[async_trait]
pub trait McpCredentialStore: Send + Sync {
    /// All credentials of an integration in list order; MCP uses the last one (`list().at(-1)`).
    async fn list(&self, integration_id: &str) -> anyhow::Result<Vec<StoredCredential>>;
    async fn get(&self, id: &str) -> anyhow::Result<Option<StoredCredential>>;
    /// Creates a credential row and returns its id.
    async fn create(&self, integration_id: &str, label: &str, value: Value) -> anyhow::Result<String>;
    async fn update(&self, id: &str, value: Value) -> anyhow::Result<()>;
    async fn remove(&self, id: &str) -> anyhow::Result<()>;
}

/// In-memory [`McpCredentialStore`] (tests and embedders without a database).
#[derive(Default)]
pub struct MemoryCredentialStore {
    rows: Mutex<Vec<StoredCredential>>,
}

#[async_trait]
impl McpCredentialStore for MemoryCredentialStore {
    async fn list(&self, integration_id: &str) -> anyhow::Result<Vec<StoredCredential>> {
        Ok(self.rows.lock().iter().filter(|r| r.integration_id == integration_id).cloned().collect())
    }
    async fn get(&self, id: &str) -> anyhow::Result<Option<StoredCredential>> {
        Ok(self.rows.lock().iter().find(|r| r.id == id).cloned())
    }
    async fn create(&self, integration_id: &str, label: &str, value: Value) -> anyhow::Result<String> {
        let id = format!("cred_{}", uuid::Uuid::new_v4().simple());
        self.rows.lock().push(StoredCredential {
            id: id.clone(),
            integration_id: integration_id.to_string(),
            label: label.to_string(),
            value,
        });
        Ok(id)
    }
    async fn update(&self, id: &str, value: Value) -> anyhow::Result<()> {
        if let Some(row) = self.rows.lock().iter_mut().find(|r| r.id == id) {
            row.value = value;
        }
        Ok(())
    }
    async fn remove(&self, id: &str) -> anyhow::Result<()> {
        self.rows.lock().retain(|r| r.id != id);
        Ok(())
    }
}

/// `integrationID = methodID = "mcp_" + sha1(name + "\0" + url).hex.slice(0,16)`.
pub fn integration_id(name: &str, url: &str) -> String {
    use sha1::Digest as _;
    let mut hasher = sha1::Sha1::new();
    hasher.update(name.as_bytes());
    hasher.update([0u8]);
    hasher.update(url.as_bytes());
    let hex = hex::encode(hasher.finalize());
    format!("mcp_{}", &hex[..16])
}

/// SDK `StoredOAuthTokens`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Tokens {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: Option<f64>,
    pub scope: Option<String>,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub issuer: Option<String>,
}

impl Tokens {
    /// `OAuthTokensSchema` (strips unknown keys; `expires_in` is coerced to a number).
    pub fn parse(value: &Value) -> Result<Self, McpError> {
        let obj = value.as_object().ok_or_else(|| McpError::other("Invalid OAuth tokens: expected object"))?;
        let s = |k: &str| obj.get(k).and_then(Value::as_str).map(String::from);
        let access_token = s("access_token").ok_or_else(|| McpError::other("Invalid OAuth tokens: access_token"))?;
        let token_type = s("token_type").ok_or_else(|| McpError::other("Invalid OAuth tokens: token_type"))?;
        let expires_in = match obj.get("expires_in") {
            None | Some(Value::Null) => None,
            Some(Value::Number(n)) => n.as_f64(),
            Some(Value::String(t)) => t.trim().parse::<f64>().ok(),
            Some(_) => None,
        };
        Ok(Self {
            access_token,
            token_type,
            expires_in,
            scope: s("scope"),
            refresh_token: s("refresh_token"),
            id_token: s("id_token"),
            issuer: None,
        })
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `toCredential` (`oauth.ts:238-258`).
pub fn to_credential(method_id: &str, server_url: &str, tokens: &Tokens, client: Option<&Map<String, Value>>) -> Value {
    let mut metadata = Map::new();
    metadata.insert("serverUrl".into(), json!(server_url));
    metadata.insert("tokenType".into(), json!(tokens.token_type));
    if let Some(scope) = tokens.scope.as_ref().filter(|s| !s.is_empty()) {
        metadata.insert("scope".into(), json!(scope));
    }
    if let Some(issuer) = tokens.issuer.as_ref().filter(|s| !s.is_empty()) {
        metadata.insert("issuer".into(), json!(issuer));
    }
    if let Some(client) = client {
        metadata.insert("client".into(), Value::Object(client.clone()));
    }
    let expires = match tokens.expires_in {
        Some(e) if e != 0.0 && !e.is_nan() => (now_ms() as f64 + e * 1000.0) as u64,
        _ => 0,
    };
    json!({
        "type": "oauth",
        "methodID": method_id,
        "access": tokens.access_token,
        "refresh": tokens.refresh_token.clone().unwrap_or_default(),
        "expires": expires,
        "metadata": Value::Object(metadata),
    })
}

/// `toTokens` (`oauth.ts:260-270`).
pub fn to_tokens(credential: &Value) -> Tokens {
    let metadata = credential.get("metadata").cloned().unwrap_or(Value::Null);
    let refresh = credential.get("refresh").and_then(Value::as_str).unwrap_or("");
    let expires = credential.get("expires").and_then(Value::as_u64).unwrap_or(0);
    Tokens {
        access_token: credential.get("access").and_then(Value::as_str).unwrap_or("").to_string(),
        token_type: metadata.get("tokenType").and_then(Value::as_str).unwrap_or("Bearer").to_string(),
        refresh_token: (!refresh.is_empty()).then(|| refresh.to_string()),
        expires_in: (expires != 0).then(|| ((expires as f64 - now_ms() as f64) / 1000.0).floor().max(0.0)),
        scope: metadata.get("scope").and_then(Value::as_str).map(String::from),
        issuer: metadata.get("issuer").and_then(Value::as_str).map(String::from),
        id_token: None,
    }
}

/// `clientFromCredential`.
pub fn client_from_credential(credential: &Value) -> Option<Map<String, Value>> {
    credential.get("metadata")?.get("client")?.as_object().cloned()
}

/// The provider's token/client/verifier storage (`McpOAuth.Store`).
#[async_trait]
pub trait TokenStore: Send + Sync {
    async fn tokens(&self) -> Result<Option<Tokens>, McpError>;
    async fn save_tokens(&self, tokens: Tokens) -> Result<(), McpError>;
    async fn client_information(&self) -> Result<Option<Map<String, Value>>, McpError>;
    async fn save_client_information(&self, info: Map<String, Value>) -> Result<(), McpError>;
    async fn code_verifier(&self) -> Result<Option<String>, McpError>;
    async fn save_code_verifier(&self, verifier: String) -> Result<(), McpError>;
    /// `invalidateCredentials`; `None` when the provider does not implement it.
    async fn invalidate(&self, _scope: &str) -> Option<Result<(), McpError>> {
        None
    }
}

/// `memoryStore()`.
#[derive(Default)]
pub struct MemoryStore {
    tokens: Mutex<Option<Tokens>>,
    client: Mutex<Option<Map<String, Value>>>,
    verifier: Mutex<Option<String>>,
}

#[async_trait]
impl TokenStore for MemoryStore {
    async fn tokens(&self) -> Result<Option<Tokens>, McpError> {
        Ok(self.tokens.lock().clone())
    }
    async fn save_tokens(&self, tokens: Tokens) -> Result<(), McpError> {
        *self.tokens.lock() = Some(tokens);
        Ok(())
    }
    async fn client_information(&self) -> Result<Option<Map<String, Value>>, McpError> {
        Ok(self.client.lock().clone())
    }
    async fn save_client_information(&self, info: Map<String, Value>) -> Result<(), McpError> {
        *self.client.lock() = Some(info);
        Ok(())
    }
    async fn code_verifier(&self) -> Result<Option<String>, McpError> {
        Ok(self.verifier.lock().clone())
    }
    async fn save_code_verifier(&self, verifier: String) -> Result<(), McpError> {
        *self.verifier.lock() = Some(verifier);
        Ok(())
    }
}

fn store_error(error: anyhow::Error) -> McpError {
    McpError::other(error.to_string())
}

/// The connect-time store over the latest credential row (`connectProvider`, `oauth.ts:272-328`).
pub struct CredentialStore {
    credentials: Arc<dyn McpCredentialStore>,
    id: String,
    method_id: String,
    server_url: String,
    presented: Mutex<String>,
}

impl CredentialStore {
    async fn read(&self) -> Result<Option<Value>, McpError> {
        let row = self.credentials.get(&self.id).await.map_err(store_error)?;
        Ok(row.map(|r| r.value).filter(|v| v.get("type").and_then(Value::as_str) == Some("oauth")))
    }
}

#[async_trait]
impl TokenStore for CredentialStore {
    async fn tokens(&self) -> Result<Option<Tokens>, McpError> {
        let Some(oauth) = self.read().await? else { return Ok(None) };
        *self.presented.lock() = oauth.get("refresh").and_then(Value::as_str).unwrap_or("").to_string();
        Ok(Some(to_tokens(&oauth)))
    }
    async fn save_tokens(&self, tokens: Tokens) -> Result<(), McpError> {
        let previous = self.read().await?;
        let client = previous.as_ref().and_then(client_from_credential);
        let value = to_credential(&self.method_id, &self.server_url, &tokens, client.as_ref());
        *self.presented.lock() = value["refresh"].as_str().unwrap_or("").to_string();
        self.credentials.update(&self.id, value).await.map_err(store_error)
    }
    async fn client_information(&self) -> Result<Option<Map<String, Value>>, McpError> {
        Ok(self.read().await?.as_ref().and_then(client_from_credential))
    }
    async fn save_client_information(&self, info: Map<String, Value>) -> Result<(), McpError> {
        let Some(mut oauth) = self.read().await? else { return Ok(()) };
        let mut metadata = oauth.get("metadata").and_then(Value::as_object).cloned().unwrap_or_default();
        metadata.insert("client".into(), Value::Object(info));
        oauth["metadata"] = Value::Object(metadata);
        self.credentials.update(&self.id, oauth).await.map_err(store_error)
    }
    async fn code_verifier(&self) -> Result<Option<String>, McpError> {
        Ok(None)
    }
    async fn save_code_verifier(&self, _verifier: String) -> Result<(), McpError> {
        Ok(())
    }
    async fn invalidate(&self, scope: &str) -> Option<Result<(), McpError>> {
        if scope == "verifier" || scope == "discovery" {
            return Some(Ok(()));
        }
        Some(
            async {
                let Some(oauth) = self.read().await? else { return Ok(()) };
                // Refresh tokens rotate and the row is shared across connections: only drop it while
                // it still holds ours.
                if oauth.get("refresh").and_then(Value::as_str).unwrap_or("") != *self.presented.lock() {
                    return Ok(());
                }
                tracing::warn!(credentialID = %self.id, scope = %scope, "mcp oauth credential invalidated");
                self.credentials.remove(&self.id).await.map_err(store_error)
            }
            .await,
        )
    }
}

/// Redirect settings for an interactive authorization attempt.
pub struct Redirect {
    pub url: String,
    pub state: String,
    /// Captures the authorization URL (`open`).
    pub open: Box<dyn Fn(Url) + Send + Sync>,
}

/// `McpOAuth.provider(...)`: the SDK `OAuthClientProvider` used for one server.
pub struct Provider {
    pub config: RemoteConfig,
    store: Arc<dyn TokenStore>,
    redirect: Option<Redirect>,
    client_metadata_url: Option<String>,
    discovery: tokio::sync::Mutex<Option<DiscoveryState>>,
    redirect_url: String,
    identity: Url,
    static_client: Option<Map<String, Value>>,
    fetch: Fetcher,
}

impl Provider {
    pub fn new(
        config: RemoteConfig,
        store: Arc<dyn TokenStore>,
        redirect: Option<Redirect>,
        client_metadata_url: Option<String>,
        discovery: Option<DiscoveryState>,
        fetch: Fetcher,
    ) -> Result<Self, McpError> {
        let oauth = config.oauth_config().cloned();
        let static_client = oauth.as_ref().and_then(|o| o.client_id.clone()).map(|id| {
            let mut m = Map::new();
            m.insert("client_id".into(), json!(id));
            if let Some(secret) = oauth.as_ref().and_then(|o| o.client_secret.clone()) {
                m.insert("client_secret".into(), json!(secret));
            }
            m
        });
        // A missing redirectUrl selects the client-credentials grant in the SDK, so connect still names one.
        let redirect_url = redirect
            .as_ref()
            .map(|r| r.url.clone())
            .or_else(|| oauth.as_ref().and_then(|o| o.redirect_uri.clone()))
            .unwrap_or_else(|| "http://127.0.0.1/callback".to_string());
        let mut identity =
            Url::parse(&config.url).map_err(|_| McpError::other(format!("Invalid URL: {}", config.url)))?;
        identity.set_fragment(None);
        Ok(Self {
            config,
            store,
            redirect,
            client_metadata_url,
            discovery: tokio::sync::Mutex::new(discovery),
            redirect_url,
            identity,
            static_client,
            fetch,
        })
    }

    pub fn redirect_url(&self) -> &str {
        &self.redirect_url
    }

    pub fn store(&self) -> &Arc<dyn TokenStore> {
        &self.store
    }

    fn refuse(&self, what: &str) -> McpError {
        McpError::Unauthorized(format!("MCP server \"{}\" requires {}", self.config.url, what))
    }

    pub async fn discovery_state(&self) -> Result<Option<DiscoveryState>, McpError> {
        let mut guard = self.discovery.lock().await;
        if guard.is_none() {
            *guard = configured_discovery(&self.config, &self.fetch).await?;
        }
        Ok(guard.clone())
    }

    pub async fn save_discovery_state(&self, state: DiscoveryState) {
        *self.discovery.lock().await = Some(state);
    }

    /// `validateResourceURL`.
    pub fn validate_resource_url(&self, resource: Option<&str>) -> Result<Url, McpError> {
        let Some(resource) = resource else { return Ok(self.identity.clone()) };
        let configured = Url::parse(resource).map_err(|_| McpError::other(format!("Invalid URL: {resource}")))?;
        if !discovery::check_resource_allowed(&self.identity, &configured) {
            return Err(McpError::other(format!("Protected resource {resource} does not cover {}", self.identity)));
        }
        if configured.origin() == self.identity.origin() && configured.path() == self.identity.path() {
            return Ok(self.identity.clone());
        }
        Ok(configured)
    }

    /// `clientMetadata` (RFC 7591 body before SDK defaults).
    pub fn client_metadata(&self) -> Map<String, Value> {
        let oauth = self.config.oauth_config();
        let mut m = Map::new();
        m.insert("redirect_uris".into(), json!([self.redirect_url]));
        m.insert("client_name".into(), json!("opencode"));
        m.insert("client_uri".into(), json!("https://opencode.ai"));
        m.insert("grant_types".into(), json!(["authorization_code", "refresh_token"]));
        m.insert("response_types".into(), json!(["code"]));
        let has_secret = self.static_client.as_ref().is_some_and(|c| c.contains_key("client_secret"));
        m.insert(
            "token_endpoint_auth_method".into(),
            json!(if has_secret { "client_secret_post" } else { "none" }),
        );
        if let Some(scope) = oauth.and_then(|o| o.scope.clone()).filter(|s| !s.is_empty()) {
            m.insert("scope".into(), json!(scope));
        }
        m
    }

    pub async fn client_information(&self) -> Result<Option<Map<String, Value>>, McpError> {
        if let Some(client) = &self.static_client {
            return Ok(Some(client.clone()));
        }
        let stored = self.store.client_information().await?;
        if stored.is_none() && self.redirect.is_none() {
            return Err(self.refuse("a login before it can register a client"));
        }
        Ok(stored)
    }

    pub async fn tokens(&self) -> Result<Option<Tokens>, McpError> {
        self.store.tokens().await
    }

    pub fn redirect_to_authorization(&self, url: Url) -> Result<(), McpError> {
        let Some(redirect) = &self.redirect else { return Err(self.refuse("user authorization")) };
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(McpError::other(format!(
                "MCP server \"{}\" returned a {}: authorization URL; only http and https are supported",
                self.config.url,
                url.scheme()
            )));
        }
        (redirect.open)(url);
        Ok(())
    }

    pub async fn code_verifier(&self) -> Result<String, McpError> {
        self.store
            .code_verifier()
            .await?
            .ok_or_else(|| McpError::other("Missing PKCE code verifier for MCP OAuth flow"))
    }

    fn state(&self) -> Option<String> {
        self.redirect.as_ref().map(|r| r.state.clone())
    }
}

/// `configuredDiscovery`: a configured `auth_server_metadata_url` stands in for RFC 9728 discovery.
pub async fn configured_discovery(config: &RemoteConfig, fetch: &Fetcher) -> Result<Option<DiscoveryState>, McpError> {
    let Some(url) = config.oauth_config().and_then(|o| o.auth_server_metadata_url.clone()) else {
        return Ok(None);
    };
    let url = Url::parse(&url).map_err(|_| McpError::other(format!("Invalid URL: {url}")))?;
    let response = fetch.fetch(HttpRequest::get(url).header("accept", "application/json")).await?;
    if !response.ok() {
        return Err(McpError::other(format!(
            "HTTP {} trying to load OAuth authorization server metadata",
            response.status
        )));
    }
    let body = response.json()?;
    let metadata = AuthServerMetadata::parse_oauth(body.clone()).or_else(|_| AuthServerMetadata::parse_oidc(body))?;
    let server_url = Url::parse(&config.url).map_err(|_| McpError::other(format!("Invalid URL: {}", config.url)))?;
    let mut resource = Map::new();
    resource.insert("resource".into(), json!(discovery::resource_url_from_server_url(&server_url).to_string()));
    Ok(Some(DiscoveryState {
        authorization_server_url: metadata.issuer().to_string(),
        resource_metadata_url: None,
        resource_metadata: Some(ResourceMetadata(resource)),
        authorization_server_metadata: Some(metadata),
    }))
}

/// Options for [`auth`].
#[derive(Clone, Default)]
pub struct AuthOptions {
    pub server_url: Option<Url>,
    pub authorization_code: Option<String>,
    pub iss: Option<String>,
    pub scope: Option<String>,
    pub resource_metadata_url: Option<Url>,
    pub force_reauthorization: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthResult {
    Authorized,
    Redirect,
}

/// SDK `auth()`: retries once after `invalid_client`/`unauthorized_client` (dropping client and
/// tokens) or `invalid_grant` (dropping tokens).
pub async fn auth(provider: &Provider, options: &AuthOptions, fetch: &Fetcher) -> Result<AuthResult, McpError> {
    match auth_internal(provider, options, fetch).await {
        Err(McpError::OAuth { code, .. }) if code == "invalid_client" || code == "unauthorized_client" => {
            if let Some(r) = provider.store.invalidate("client").await {
                r?;
            }
            if let Some(r) = provider.store.invalidate("tokens").await {
                r?;
            }
            auth_internal(provider, options, fetch).await
        }
        Err(McpError::OAuth { code, .. }) if code == "invalid_grant" => {
            if let Some(r) = provider.store.invalidate("tokens").await {
                r?;
            }
            auth_internal(provider, options, fetch).await
        }
        other => other,
    }
}

fn determine_scope(
    requested: Option<&str>,
    resource: Option<&ResourceMetadata>,
    metadata: Option<&AuthServerMetadata>,
    client_metadata: &Map<String, Value>,
) -> Option<String> {
    let mut scope = requested
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| resource.and_then(|r| r.scopes_supported()).map(|s| s.join(" ")).filter(|s| !s.is_empty()))
        .or_else(|| client_metadata.get("scope").and_then(Value::as_str).map(String::from));
    if let Some(s) = &scope {
        let offline = metadata.and_then(|m| m.scopes_supported()).is_some_and(|l| l.iter().any(|x| x == "offline_access"));
        let refresh_grant = client_metadata
            .get("grant_types")
            .and_then(Value::as_array)
            .is_some_and(|g| g.iter().any(|v| v == "refresh_token"));
        if !s.is_empty() && offline && !s.split(' ').any(|t| t == "offline_access") && refresh_grant {
            scope = Some(format!("{s} offline_access"));
        }
    }
    scope.filter(|s| !s.is_empty())
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

fn derive_application_type(redirect_uris: Option<&Value>) -> &'static str {
    for raw in redirect_uris.and_then(Value::as_array).into_iter().flatten() {
        let Some(Ok(url)) = raw.as_str().map(Url::parse) else { continue };
        if url.scheme() != "http" && url.scheme() != "https" {
            return "native";
        }
        if is_loopback(url.host_str().unwrap_or("")) {
            return "native";
        }
    }
    "web"
}

/// `resolveClientMetadata`.
fn resolve_client_metadata(provider: &Provider) -> Map<String, Value> {
    let mut m = provider.client_metadata();
    if !m.contains_key("application_type") {
        let app = derive_application_type(m.get("redirect_uris"));
        m.insert("application_type".into(), json!(app));
    }
    m
}

fn stamp_issuer(mut info: Map<String, Value>, issuer: &str) -> Map<String, Value> {
    info.insert("issuer".into(), json!(issuer));
    info
}

fn discard_if_issuer_mismatch(stored: Option<Map<String, Value>>, issuer: &str) -> Option<Map<String, Value>> {
    let stored = stored?;
    match stored.get("issuer").and_then(Value::as_str) {
        None => Some(stored),
        Some(s) if discovery::issuers_match(s, issuer) => Some(stored),
        Some(_) => None,
    }
}

fn parse_url(value: &str) -> Result<Url, McpError> {
    Url::parse(value).map_err(|_| McpError::Network { message: format!("Invalid URL: {value}"), code: Some("ERR_INVALID_URL".into()) })
}

async fn auth_internal(provider: &Provider, options: &AuthOptions, fetch: &Fetcher) -> Result<AuthResult, McpError> {
    let server_url = match &options.server_url {
        Some(u) => u.clone(),
        None => parse_url(&provider.config.url)?,
    };
    let client_metadata = resolve_client_metadata(provider);
    let cached = provider.discovery_state().await?;
    let mut resource_metadata_url = options.resource_metadata_url.clone();
    if resource_metadata_url.is_none() {
        resource_metadata_url = cached.as_ref().and_then(|c| c.resource_metadata_url.as_deref()).and_then(|u| Url::parse(u).ok());
    }
    let authorization_server_url: String;
    let metadata: Option<AuthServerMetadata>;
    let resource_metadata: Option<ResourceMetadata>;
    let mut fresh = None;
    match cached.as_ref().filter(|c| !c.authorization_server_url.is_empty()) {
        Some(state) => {
            authorization_server_url = state.authorization_server_url.clone();
            metadata = match &state.authorization_server_metadata {
                Some(m) => Some(m.clone()),
                None => discovery::discover_authorization_server(fetch, &authorization_server_url).await?,
            };
            resource_metadata = match &state.resource_metadata {
                Some(r) => Some(r.clone()),
                None => match discovery::discover_protected_resource(fetch, &server_url, resource_metadata_url.as_ref()).await {
                    Ok(r) => Some(r),
                    Err(e @ McpError::Network { .. }) => return Err(e),
                    Err(_) => None,
                },
            };
            if metadata != state.authorization_server_metadata || resource_metadata != state.resource_metadata {
                provider
                    .save_discovery_state(DiscoveryState {
                        authorization_server_url: authorization_server_url.clone(),
                        resource_metadata_url: resource_metadata_url.as_ref().map(Url::to_string),
                        resource_metadata: resource_metadata.clone(),
                        authorization_server_metadata: metadata.clone(),
                    })
                    .await;
            }
        }
        None => {
            let info = discovery::discover_server_info(fetch, &server_url, resource_metadata_url.as_ref()).await?;
            authorization_server_url = info.authorization_server_url;
            metadata = info.authorization_server_metadata;
            resource_metadata = info.resource_metadata;
            fresh = Some(DiscoveryState {
                authorization_server_url: authorization_server_url.clone(),
                resource_metadata_url: resource_metadata_url.as_ref().map(Url::to_string),
                resource_metadata: resource_metadata.clone(),
                authorization_server_metadata: metadata.clone(),
            });
        }
    }
    let issuer = metadata.as_ref().map(|m| m.issuer().to_string()).unwrap_or_else(|| authorization_server_url.clone());
    if options.authorization_code.is_some() {
        let recorded = cached.as_ref().map(|c| {
            c.authorization_server_metadata.as_ref().map(|m| m.issuer().to_string()).unwrap_or_else(|| c.authorization_server_url.clone())
        });
        match recorded {
            None => {
                return Err(McpError::other(format!(
                    "Authorization server changed between redirect and callback (redirected to {}, callback resolved {}); refusing to send authorization_code/code_verifier to a different token endpoint",
                    serde_json::to_string("discoveryState was not available on the callback leg; ensure your provider persists discoveryState alongside codeVerifier").unwrap(),
                    serde_json::to_string(&issuer).unwrap()
                )));
            }
            Some(recorded) if !discovery::issuers_match(&recorded, &issuer) => {
                return Err(McpError::other(format!(
                    "Authorization server changed between redirect and callback (redirected to {}, callback resolved {}); refusing to send authorization_code/code_verifier to a different token endpoint",
                    serde_json::to_string(&recorded).unwrap(),
                    serde_json::to_string(&issuer).unwrap()
                )));
            }
            _ => {}
        }
    }
    if let Some(state) = fresh {
        provider.save_discovery_state(state).await;
    }
    let resource = provider.validate_resource_url(resource_metadata.as_ref().map(|r| r.resource()))?;
    let scope = determine_scope(
        options.scope.as_deref(),
        resource_metadata.as_ref(),
        metadata.as_ref(),
        &provider.client_metadata(),
    );

    let raw_client = provider.client_information().await?;
    let mut client = discard_if_issuer_mismatch(raw_client, &issuer);
    if let Some(c) = &client {
        if !c.contains_key("issuer") {
            let stamped = stamp_issuer(c.clone(), &issuer);
            provider.store.save_client_information(stamped.clone()).await?;
            client = Some(stamped);
        }
    }
    let client = match client {
        Some(c) => c,
        None => {
            if options.authorization_code.is_some() {
                return Err(McpError::other(
                    "Existing OAuth client information is required when exchanging an authorization code",
                ));
            }
            let supports_cimd = metadata.as_ref().is_some_and(|m| m.client_id_metadata_document_supported());
            if let Some(url) = &provider.client_metadata_url {
                if !is_https_url(url) {
                    return Err(McpError::OAuth {
                        code: "invalid_client_metadata".into(),
                        message: format!("clientMetadataUrl must be a valid HTTPS URL with a non-root pathname, got: {url}"),
                    });
                }
            }
            let info = match (&provider.client_metadata_url, supports_cimd) {
                (Some(url), true) => {
                    let mut m = Map::new();
                    m.insert("client_id".into(), json!(url));
                    m.insert("issuer".into(), json!(issuer));
                    m
                }
                _ => {
                    let registered =
                        register_client(fetch, &authorization_server_url, metadata.as_ref(), &client_metadata, scope.as_deref())
                            .await?;
                    stamp_issuer(registered, &issuer)
                }
            };
            provider.store.save_client_information(info.clone()).await?;
            info
        }
    };

    if let Some(code) = &options.authorization_code {
        validate_iss(options.iss.as_deref(), metadata.as_ref())?;
        let verifier = provider.code_verifier().await?;
        let client_info = provider.client_information().await?.unwrap_or(client.clone());
        let params = vec![
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("code".to_string(), code.clone()),
            ("code_verifier".to_string(), verifier),
            ("redirect_uri".to_string(), provider.redirect_url.clone()),
        ];
        let mut tokens = token_request(fetch, &authorization_server_url, metadata.as_ref(), params, &client_info, &resource).await?;
        tokens.issuer = Some(issuer.clone());
        provider.store.save_tokens(tokens).await?;
        return Ok(AuthResult::Authorized);
    }

    let mut tokens = provider.store.tokens().await?;
    if let Some(t) = &tokens {
        match &t.issuer {
            Some(stored) if !discovery::issuers_match(stored, &issuer) => tokens = None,
            Some(_) => {}
            None => {
                let mut stamped = t.clone();
                stamped.issuer = Some(issuer.clone());
                provider.store.save_tokens(stamped.clone()).await?;
                tokens = Some(stamped);
            }
        }
    }
    if let Some(refresh) = tokens.as_ref().and_then(|t| t.refresh_token.clone()) {
        if !options.force_reauthorization {
            let params = vec![
                ("grant_type".to_string(), "refresh_token".to_string()),
                ("refresh_token".to_string(), refresh.clone()),
            ];
            match token_request(fetch, &authorization_server_url, metadata.as_ref(), params, &client, &resource).await {
                Ok(mut new_tokens) => {
                    if new_tokens.refresh_token.is_none() {
                        new_tokens.refresh_token = Some(refresh);
                    }
                    new_tokens.issuer = Some(issuer.clone());
                    provider.store.save_tokens(new_tokens).await?;
                    return Ok(AuthResult::Authorized);
                }
                Err(error @ McpError::Other(_)) if error.to_string().starts_with("Refusing to send credentials") => {
                    return Err(error);
                }
                Err(McpError::OAuth { code, message }) if code != "server_error" => {
                    return Err(McpError::OAuth { code, message });
                }
                Err(_) => {}
            }
        }
    }

    let (authorization_url, verifier) = start_authorization(
        &authorization_server_url,
        metadata.as_ref(),
        &client,
        &provider.redirect_url,
        scope.as_deref(),
        provider.state().as_deref(),
        &resource,
    )?;
    provider.store.save_code_verifier(verifier).await?;
    provider.redirect_to_authorization(authorization_url)?;
    Ok(AuthResult::Redirect)
}

fn is_https_url(value: &str) -> bool {
    Url::parse(value).is_ok_and(|u| u.scheme() == "https" && u.path() != "/")
}

fn validate_iss(iss: Option<&str>, metadata: Option<&AuthServerMetadata>) -> Result<(), McpError> {
    let Some(metadata) = metadata else { return Ok(()) };
    let expected = metadata.issuer();
    let mismatch = |received: Option<&str>| {
        McpError::other(format!(
            "Issuer mismatch in authorization response (RFC 9207): expected {}, received {}",
            serde_json::to_string(expected).unwrap(),
            received.map(|r| serde_json::to_string(r).unwrap()).unwrap_or_else(|| "undefined".into())
        ))
    };
    match iss {
        None if metadata.iss_parameter_supported() => Err(mismatch(None)),
        None => Ok(()),
        Some(iss) if iss != expected => Err(mismatch(Some(iss))),
        Some(_) => Ok(()),
    }
}

/// PKCE (`pkce-challenge`): 43-char verifier, S256 challenge.
pub fn pkce() -> (String, String) {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::rng();
    let verifier: String = (0..43).map(|_| CHARS[rng.random_range(0..CHARS.len())] as char).collect();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn set_param(pairs: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some(index) = pairs.iter().position(|(k, _)| k == key) {
        pairs[index].1 = value.to_string();
        let mut i = index + 1;
        while i < pairs.len() {
            if pairs[i].0 == key {
                pairs.remove(i);
            } else {
                i += 1;
            }
        }
    } else {
        pairs.push((key.to_string(), value.to_string()));
    }
}

/// `startAuthorization`.
pub fn start_authorization(
    authorization_server_url: &str,
    metadata: Option<&AuthServerMetadata>,
    client: &Map<String, Value>,
    redirect_url: &str,
    scope: Option<&str>,
    state: Option<&str>,
    resource: &Url,
) -> Result<(Url, String), McpError> {
    let mut url = match metadata {
        Some(m) => {
            let url = parse_url(m.authorization_endpoint())?;
            if !m.response_types_supported().iter().any(|r| r == "code") {
                return Err(McpError::other("Incompatible auth server: does not support response type code"));
            }
            if let Some(methods) = m.code_challenge_methods_supported() {
                if !methods.iter().any(|m| m == "S256") {
                    return Err(McpError::other("Incompatible auth server: does not support code challenge method S256"));
                }
            }
            url
        }
        None => parse_url(authorization_server_url)?.join("/authorize").map_err(|e| McpError::other(e.to_string()))?,
    };
    let (verifier, challenge) = pkce();
    let mut pairs: Vec<(String, String)> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    set_param(&mut pairs, "response_type", "code");
    set_param(&mut pairs, "client_id", client.get("client_id").and_then(Value::as_str).unwrap_or(""));
    set_param(&mut pairs, "code_challenge", &challenge);
    set_param(&mut pairs, "code_challenge_method", "S256");
    set_param(&mut pairs, "redirect_uri", redirect_url);
    if let Some(state) = state.filter(|s| !s.is_empty()) {
        set_param(&mut pairs, "state", state);
    }
    if let Some(scope) = scope.filter(|s| !s.is_empty()) {
        set_param(&mut pairs, "scope", scope);
    }
    set_param(&mut pairs, "resource", resource.as_str());
    url.set_query(Some(&fetch::encode_form(&pairs)));
    Ok((url, verifier))
}

fn select_auth_method(client: &Map<String, Value>, supported: &[String]) -> &'static str {
    let has_secret = client.get("client_secret").is_some_and(|v| !v.is_null());
    if let Some(method) = client.get("token_endpoint_auth_method").and_then(Value::as_str) {
        for candidate in ["client_secret_basic", "client_secret_post", "none"] {
            if method == candidate && (supported.is_empty() || supported.iter().any(|s| s == method)) {
                return candidate;
            }
        }
    }
    if supported.is_empty() {
        return if has_secret { "client_secret_basic" } else { "none" };
    }
    if has_secret && supported.iter().any(|s| s == "client_secret_basic") {
        return "client_secret_basic";
    }
    if has_secret && supported.iter().any(|s| s == "client_secret_post") {
        return "client_secret_post";
    }
    if supported.iter().any(|s| s == "none") {
        return "none";
    }
    if has_secret { "client_secret_post" } else { "none" }
}

/// `executeTokenRequest`.
async fn token_request(
    fetch: &Fetcher,
    authorization_server_url: &str,
    metadata: Option<&AuthServerMetadata>,
    mut params: Vec<(String, String)>,
    client: &Map<String, Value>,
    resource: &Url,
) -> Result<Tokens, McpError> {
    let token_url = match metadata {
        Some(m) => parse_url(m.token_endpoint())?,
        None => parse_url(authorization_server_url)?.join("/token").map_err(|e| McpError::other(e.to_string()))?,
    };
    if token_url.scheme() != "https" && !is_loopback(token_url.host_str().unwrap_or("")) {
        return Err(McpError::other(format!(
            "Refusing to send credentials to non-https token endpoint '{token_url}'. OAuth token requests MUST use TLS (localhost / 127.0.0.1 / ::1 are exempt)."
        )));
    }
    set_param(&mut params, "resource", resource.as_str());
    let mut request = HttpRequest {
        method: reqwest::Method::POST,
        url: token_url,
        headers: vec![
            ("Content-Type".into(), "application/x-www-form-urlencoded".into()),
            ("Accept".into(), "application/json".into()),
        ],
        body: None,
    };
    let supported = metadata.and_then(|m| m.token_endpoint_auth_methods_supported()).unwrap_or_default();
    let client_id = client.get("client_id").and_then(Value::as_str).unwrap_or("").to_string();
    let secret = client.get("client_secret").and_then(Value::as_str).map(String::from);
    match select_auth_method(client, &supported) {
        "client_secret_basic" => {
            let Some(secret) = secret else {
                return Err(McpError::other("client_secret_basic authentication requires a client_secret"));
            };
            let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{client_id}:{secret}"));
            request = request.header("Authorization", &format!("Basic {encoded}"));
        }
        "client_secret_post" => {
            set_param(&mut params, "client_id", &client_id);
            if let Some(secret) = secret.filter(|s| !s.is_empty()) {
                set_param(&mut params, "client_secret", &secret);
            }
        }
        _ => set_param(&mut params, "client_id", &client_id),
    }
    request.body = Some(Body::Form(params));
    let response = fetch.fetch(request).await?;
    if !response.ok() {
        return Err(fetch::parse_error_response(response.status, &response.text()));
    }
    let json = response.json()?;
    match Tokens::parse(&json) {
        Ok(tokens) => Ok(tokens),
        Err(error) => {
            if json.get("error").is_some() {
                Err(fetch::parse_error_response(response.status, &json.to_string()))
            } else {
                Err(error)
            }
        }
    }
}

/// `registerClient` (RFC 7591 dynamic client registration).
async fn register_client(
    fetch: &Fetcher,
    authorization_server_url: &str,
    metadata: Option<&AuthServerMetadata>,
    client_metadata: &Map<String, Value>,
    scope: Option<&str>,
) -> Result<Map<String, Value>, McpError> {
    let url = match metadata {
        Some(m) => match m.registration_endpoint() {
            Some(endpoint) => parse_url(endpoint)?,
            None => return Err(McpError::other("Incompatible auth server: does not support dynamic client registration")),
        },
        None => parse_url(authorization_server_url)?.join("/register").map_err(|e| McpError::other(e.to_string()))?,
    };
    let mut submitted = client_metadata.clone();
    if let Some(scope) = scope {
        submitted.insert("scope".into(), json!(scope));
    }
    let request = HttpRequest {
        method: reqwest::Method::POST,
        url,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: Some(Body::Json(Value::Object(submitted).to_string())),
    };
    let response = fetch.fetch(request).await?;
    if !response.ok() {
        return Err(McpError::other(format!(
            "Dynamic Client Registration rejected (HTTP {}): {}",
            response.status,
            response.text()
        )));
    }
    let json = response.json()?;
    match json {
        Value::Object(obj) if obj.get("client_id").is_some_and(Value::is_string) => Ok(obj),
        _ => Err(McpError::other("Invalid client registration response: client_id is required")),
    }
}

/// `connectProvider`: the provider used when connecting. With no stored OAuth credential it is a
/// provider over an empty memory store without a redirect, so a 401 ends in `needs_auth`.
pub async fn connect_provider(
    config: &RemoteConfig,
    integration_id: &str,
    credentials: Option<&Arc<dyn McpCredentialStore>>,
    fetch: Fetcher,
) -> Result<Provider, McpError> {
    let found = match credentials {
        Some(store) => store.list(integration_id).await.map_err(store_error)?.pop(),
        None => None,
    };
    let Some(found) = found.filter(|f| f.value.get("type").and_then(Value::as_str) == Some("oauth")) else {
        return Provider::new(config.clone(), Arc::new(MemoryStore::default()), None, None, None, fetch);
    };
    let store = CredentialStore {
        credentials: credentials.unwrap().clone(),
        id: found.id.clone(),
        method_id: found.value.get("methodID").and_then(Value::as_str).unwrap_or("").to_string(),
        server_url: config.url.clone(),
        presented: Mutex::new(found.value.get("refresh").and_then(Value::as_str).unwrap_or("").to_string()),
    };
    Provider::new(config.clone(), Arc::new(store), None, None, None, fetch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integration_ids() {
        let id = integration_id("srv", "https://x.test/mcp");
        assert!(id.starts_with("mcp_"));
        assert_eq!(id.len(), 20);
        assert_ne!(id, integration_id("srv2", "https://x.test/mcp"));
    }

    #[test]
    fn credential_round_trip() {
        let tokens = Tokens {
            access_token: "a".into(),
            token_type: "Bearer".into(),
            expires_in: Some(3600.0),
            scope: Some("read".into()),
            refresh_token: Some("r".into()),
            id_token: None,
            issuer: Some("https://as.test".into()),
        };
        let mut client = Map::new();
        client.insert("client_id".into(), json!("cid"));
        let value = to_credential("mcp_x", "https://x.test", &tokens, Some(&client));
        let keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, vec!["type", "methodID", "access", "refresh", "expires", "metadata"]);
        let meta_keys: Vec<_> = value["metadata"].as_object().unwrap().keys().cloned().collect();
        assert_eq!(meta_keys, vec!["serverUrl", "tokenType", "scope", "issuer", "client"]);
        assert!(value["expires"].as_u64().unwrap() > now_ms());
        let back = to_tokens(&value);
        assert_eq!(back.access_token, "a");
        assert_eq!(back.refresh_token.as_deref(), Some("r"));
        assert!(back.expires_in.unwrap() >= 3598.0);
        assert_eq!(client_from_credential(&value).unwrap()["client_id"], "cid");
        let non_expiring = to_credential("m", "u", &Tokens { access_token: "x".into(), token_type: "bearer".into(), ..Default::default() }, None);
        assert_eq!(non_expiring["expires"], 0);
        assert_eq!(non_expiring["refresh"], "");
        let t = to_tokens(&non_expiring);
        assert!(t.expires_in.is_none() && t.refresh_token.is_none());
    }

    #[test]
    fn pkce_and_scope() {
        let (verifier, challenge) = pkce();
        assert_eq!(verifier.len(), 43);
        assert_eq!(challenge.len(), 43);
        let mut cm = Map::new();
        cm.insert("grant_types".into(), json!(["authorization_code", "refresh_token"]));
        let meta = AuthServerMetadata(json!({"scopes_supported": ["offline_access"]}).as_object().unwrap().clone());
        assert_eq!(determine_scope(Some("a"), None, Some(&meta), &cm).as_deref(), Some("a offline_access"));
        assert_eq!(determine_scope(None, None, Some(&meta), &cm), None);
        assert_eq!(select_auth_method(&Map::new(), &[]), "none");
    }

    #[test]
    fn authorization_url() {
        let mut client = Map::new();
        client.insert("client_id".into(), json!("cid"));
        let meta = AuthServerMetadata(
            json!({"issuer":"https://as.test","authorization_endpoint":"https://as.test/authorize?x=1","token_endpoint":"https://as.test/token","response_types_supported":["code"]})
                .as_object()
                .unwrap()
                .clone(),
        );
        let (url, _) = start_authorization(
            "https://as.test",
            Some(&meta),
            &client,
            "http://127.0.0.1:5555/callback",
            Some("a b"),
            Some("st"),
            &Url::parse("https://x.test/mcp").unwrap(),
        )
        .unwrap();
        let s = url.as_str();
        assert!(s.starts_with("https://as.test/authorize?x=1&response_type=code&client_id=cid&code_challenge="));
        assert!(s.contains("&code_challenge_method=S256&redirect_uri=http%3A%2F%2F127.0.0.1%3A5555%2Fcallback&state=st&scope=a+b&resource=https%3A%2F%2Fx.test%2Fmcp"));
    }
}
