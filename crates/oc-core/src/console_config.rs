//! The OpenCode Console workspace configuration (`packages/core/src/plugin/provider/opencode.ts` `load`,
//! `publish`, `fetchConfig`, the web search registration and the polling): while a location is signed in to
//! OpenCode Console, the Console's `GET /api/v2/config` (providers, remote MCP servers, policies and the web search
//! selection) merges as an integration config document, loaded right after managed policies.

use oc_ai::credentials::flows::console::{credential_headers, server_url, SIGNED_OUT_MESSAGE};
use oc_ai::credentials::integration::{ConnectionInfo, ConnectionStatus, IntegrationError, Integrations};
use oc_ai::credentials::CredentialValue;
use oc_config::schema::{Entry, Info};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// The integration of the OpenCode Console sign-in.
pub const INTEGRATION_ID: &str = "opencode";
/// The Console web search provider id.
pub const WEBSEARCH_ID: &str = "opencode";
/// How often a signed-in location checks the Console config again.
pub const POLL: Duration = Duration::from_secs(60);

const SIGNED_OUT: &str = "Reconnect OpenCode Console to continue";

fn sso_message(organization: Option<&str>) -> String {
    format!("Sign in with SSO again to use {}", organization.unwrap_or("your OpenCode Console organization"))
}

/// What the plugin last loaded for a location.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// The Console's config (`ConfigIntegration.Remote`), when it has one.
    pub remote: Option<Info>,
    /// The active OpenCode Console connection.
    pub connection: Option<ConnectionInfo>,
    /// The organization behind the connection (`metadata.orgName`).
    pub organization: Option<String>,
}

impl Snapshot {
    /// The Console selected its hosted search and the location is signed in (`websearch.transform`).
    pub fn offers_websearch(&self) -> bool {
        self.connection.is_some()
            && matches!(
                self.remote.as_ref().and_then(|r| r.websearch.as_ref()),
                Some(oc_config::schema::WebSearch::Provider(id)) if id == WEBSEARCH_ID
            )
    }

    /// The integration document the snapshot contributes to config.
    pub fn document(&self) -> Option<Entry> {
        let (remote, connection) = (self.remote.clone()?, self.connection.as_ref()?);
        Some(oc_config::integration::make(INTEGRATION_ID, connection_json(connection), self.organization.clone(), remote))
    }
}

/// `Connection.Info` without its status.
fn connection_json(connection: &ConnectionInfo) -> Value {
    match connection {
        ConnectionInfo::Credential { id, label, method, .. } => json!({"type": "credential", "id": id, "label": label, "method": method}),
        ConnectionInfo::Env { name, .. } => json!({"type": "env", "name": name}),
    }
}

/// Integration documents by location key (`Config.integration`), read by every config load of the location.
static DOCUMENTS: once_cell::sync::Lazy<Mutex<HashMap<String, Entry>>> = once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));
/// The last snapshot by location key.
static SNAPSHOTS: once_cell::sync::Lazy<Mutex<HashMap<String, Snapshot>>> = once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

/// The integration documents of location `key`, in load order.
pub fn documents(key: &str) -> Vec<Entry> {
    DOCUMENTS.lock().get(key).cloned().into_iter().collect()
}

/// The last loaded snapshot of location `key`.
pub fn snapshot(key: &str) -> Snapshot {
    SNAPSHOTS.lock().get(key).cloned().unwrap_or_default()
}

/// Store a snapshot and its document; true when the document changed (config must reload).
pub fn publish(key: &str, next: Snapshot) -> bool {
    let document = next.document();
    SNAPSHOTS.lock().insert(key.to_string(), next);
    let mut documents = DOCUMENTS.lock();
    if documents.get(key) == document.as_ref() {
        return false;
    }
    match document {
        Some(document) => documents.insert(key.to_string(), document),
        None => documents.remove(key),
    };
    true
}

/// Forget a closed location.
pub fn forget(key: &str) {
    DOCUMENTS.lock().remove(key);
    SNAPSHOTS.lock().remove(key);
}

/// Why the Console config could not be loaded.
#[derive(Debug, Clone, PartialEq)]
pub enum LoadError {
    /// The Console session or refresh token was revoked or expired; only a new device login recovers.
    SignedOut,
    /// The organization enforces SSO and the user's SSO proof is missing or expired.
    SsoRequired { organization: Option<String>, url: String },
    Other(String),
}

impl LoadError {
    /// The connection status a well-formed Console error reports (`connectionStatus`); anything else may be
    /// transient and changes nothing.
    fn status(&self) -> Option<ConnectionStatus> {
        match self {
            LoadError::SignedOut => Some(ConnectionStatus::needs_auth(SIGNED_OUT, None)),
            LoadError::SsoRequired { organization, url } => {
                Some(ConnectionStatus::needs_auth(sso_message(organization.as_deref()), Some(url.clone())))
            }
            LoadError::Other(_) => None,
        }
    }
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::SignedOut => write!(f, "{SIGNED_OUT}."),
            LoadError::SsoRequired { organization, .. } => write!(f, "{}.", sso_message(organization.as_deref())),
            LoadError::Other(message) => f.write_str(message),
        }
    }
}

/// `normalizeServer`: origin + pathname without trailing slashes; HTTP(S) only.
fn normalize_server(input: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(input).map_err(|_| "Invalid OpenCode server URL: Invalid URL".to_string())?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("Invalid OpenCode server URL: expected HTTP(S)".into());
    }
    Ok(url)
}

/// `ssoUrl(server, connectionID)`: Console only redirects to paths under its own base path after SSO.
pub fn sso_url(server: &str, connection_id: &str) -> Result<String, String> {
    let url = normalize_server(server)?;
    let path = url.path().trim_end_matches('/');
    let base = format!("{}{path}", url.origin().ascii_serialization());
    let redirect = format!("{path}/");
    let encode = |s: &str| percent_encode(s);
    Ok(format!("{base}/auth/sso/{}/start?redirectTo={}", encode(connection_id), encode(&redirect)))
}

/// `encodeURIComponent`.
fn percent_encode(input: &str) -> String {
    let mut out = String::new();
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `fetchConfig`: the Console's config, `None` when it has none (404).
pub async fn fetch_config(
    http: &dyn crate::usage_limits::Http,
    user_agent: &str,
    credential: &CredentialValue,
) -> Result<Option<Info>, LoadError> {
    let server = server_url(credential);
    let url = format!("{server}/api/v2/config");
    let mut headers = vec![("user-agent".to_string(), user_agent.to_string()), ("accept".to_string(), "application/json".to_string())];
    headers.extend(credential_headers(credential));
    let response = http.get(&url, &headers).await.map_err(LoadError::Other)?;
    let body = || serde_json::from_str::<Value>(&response.body).ok();
    match response.status {
        404 => return Ok(None),
        401 if body().and_then(|b| b.get("_tag").cloned()) == Some(json!("Unauthorized")) => return Err(LoadError::SignedOut),
        403 => {
            if let Some(b) = body()
                && b.get("_tag") == Some(&json!("SsoRequired"))
                && b.get("orgId").is_some_and(Value::is_string)
                && let Some(connection) = b.get("connectionId").and_then(Value::as_str)
            {
                let organization = credential.metadata().and_then(|m| m.get("orgName")).and_then(Value::as_str).map(String::from);
                let url = sso_url(&server, connection).map_err(LoadError::Other)?;
                return Err(LoadError::SsoRequired { organization, url });
            }
        }
        _ => {}
    }
    if !(200..300).contains(&response.status) {
        return Err(LoadError::Other(format!("StatusCode: non 2xx status code ({} GET {url})", response.status)));
    }
    let body: Value = serde_json::from_str(&response.body).map_err(|e| LoadError::Other(e.to_string()))?;
    oc_config::integration::decode_remote(&body).map(Some).map_err(LoadError::Other)
}

fn organization_name(credential: &CredentialValue) -> Option<String> {
    credential.metadata().and_then(|m| m.get("orgName")).and_then(Value::as_str).map(String::from)
}

/// `load()`: the active OpenCode Console connection's config, reporting `needs_auth` statuses. A load that fails
/// for the connection already in place keeps its last config: dropping it would lift organization policy while
/// personal credentials keep working.
pub async fn load(
    integrations: &Integrations,
    http: &dyn crate::usage_limits::Http,
    user_agent: &str,
    previous: &Snapshot,
) -> Snapshot {
    let connection = integrations.active(INTEGRATION_ID).await.ok().flatten();
    let Some(connection) = connection else { return Snapshot::default() };
    let result: Result<Snapshot, LoadError> = async {
        let credential = integrations.resolve(&connection).await.map_err(|error| match error {
            IntegrationError::Authorization(message) if message == SIGNED_OUT_MESSAGE => LoadError::SignedOut,
            other => LoadError::Other(other.to_string()),
        })?;
        match credential {
            None | Some(CredentialValue::External { .. }) => {
                Ok(Snapshot { remote: None, connection: Some(connection.clone()), organization: None })
            }
            Some(credential) => {
                let remote = fetch_config(http, user_agent, &credential).await?;
                Ok(Snapshot { remote, connection: Some(connection.clone()), organization: organization_name(&credential) })
            }
        }
    }
    .await;
    match result {
        Ok(next) => {
            integrations.set_status(INTEGRATION_ID, &connection, None);
            next
        }
        Err(cause) => {
            if let Some(status) = cause.status() {
                integrations.set_status(INTEGRATION_ID, &connection, Some(status));
            }
            tracing::warn!(cause = %cause, "failed to load OpenCode provider config");
            let same = previous.connection.as_ref().map(ConnectionInfo::key) == Some(connection.key());
            if same {
                Snapshot { remote: previous.remote.clone(), connection: Some(connection), organization: previous.organization.clone() }
            } else {
                Snapshot { remote: None, connection: Some(connection), organization: None }
            }
        }
    }
}

/// The Console web search provider for a snapshot's connection (`search(providerID, connection)`): every search
/// checks that the same connection is still active.
pub fn websearch_provider_for(
    integrations: Arc<Integrations>,
    connection: ConnectionInfo,
) -> Arc<dyn oc_tools::websearch::SearchProvider> {
    use oc_tools::websearch::{ConsoleCredential, ConsoleProvider};
    let credential: oc_tools::websearch::ConsoleCredentialFn = Arc::new(move || {
        let (integrations, connection) = (integrations.clone(), connection.clone());
        Box::pin(async move {
            let active = integrations.active(INTEGRATION_ID).await.ok().flatten();
            if active.as_ref().map(ConnectionInfo::key) != Some(connection.key()) {
                return Err("OpenCode Console connection changed".to_string());
            }
            let credential = integrations.resolve(active.as_ref().unwrap()).await.map_err(|e| e.to_string())?;
            let credential = match credential {
                Some(c) if !matches!(c, CredentialValue::External { .. }) => c,
                _ => return Err("OpenCode Console is not connected".to_string()),
            };
            let token = match &credential {
                CredentialValue::Oauth { access, .. } => access.clone(),
                CredentialValue::Key { key, .. } => key.clone(),
                CredentialValue::External { .. } => String::new(),
            };
            let org_id = credential.metadata().and_then(|m| m.get("orgID")).and_then(Value::as_str).map(String::from);
            Ok(ConsoleCredential { token, org_id, server: Some(server_url(&credential)) })
        })
    });
    Arc::new(ConsoleProvider::new(WEBSEARCH_ID, credential))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sso_urls_stay_under_the_console_base_path() {
        assert_eq!(
            sso_url("https://opencode.ai/console", "conn 1").unwrap(),
            "https://opencode.ai/console/auth/sso/conn%201/start?redirectTo=%2Fconsole%2F"
        );
        assert_eq!(sso_url("https://dev.example/", "c").unwrap(), "https://dev.example/auth/sso/c/start?redirectTo=%2F");
    }
}
