//! Refreshing a stored MCP OAuth credential one at a time (`Credential.refresh`, `core/src/credential.ts`) and the
//! locked refresh `connectProvider` sends `refresh_token` requests through (`core/src/mcp/oauth.ts`).
//!
//! Refresh tokens rotate and the row is shared by every connection, Location and process using the database, so a
//! refresh runs under the in-process share plus the file lock `credential-refresh:<id>`, reads the row again and
//! only spends the refresh token while the row still holds it. A temporary failure answers
//! `503 temporarily_unavailable`, which the SDK surfaces as is, so the connection fails and the login stays.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Map, Value, json};

use super::fetch::{HttpRequest, HttpResponse, send_buffered};
use super::{McpCredentialStore, Tokens, client_from_credential, to_credential};
use crate::error::McpError;

/// `Effect.timeout("30 seconds")` around the refresh, reading the body included.
pub const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a refresh produced no credential.
#[derive(Debug, Clone)]
pub enum RefreshError {
    /// `RefreshRejected`: the authorization server answered with an error or a body that is not tokens.
    Rejected { status: u16, status_text: String, headers: HeaderMap, body: Bytes },
    /// A network failure, the time limit or the lock: the message becomes the `temporarily_unavailable` reason.
    Failed(String),
    /// Reading or writing the store failed (an upstream defect).
    Defect(String),
}

type Pending = Shared<BoxFuture<'static, Result<Option<Value>, RefreshError>>>;

// Concurrent callers for the same credential share the running refresh, including its failure.
static PENDING: Lazy<Mutex<HashMap<String, Pending>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn is_oauth(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("oauth")
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `Credential.refresh(id, {stale, refresh})`: refreshes a stored OAuth credential and saves the result. The row is
/// read again under the lock: when `stale` says it no longer needs a refresh, another caller already refreshed it
/// and that row is returned. When the row changes while `refresh` runs, the stored row wins. `Ok(None)` when the
/// credential is gone. The operation runs in its own task, so it finishes even when every caller goes away.
pub async fn refresh_credential<S, R>(
    store: Arc<dyn McpCredentialStore>,
    id: &str,
    stale: S,
    refresh: R,
) -> Result<Option<Value>, RefreshError>
where
    S: FnOnce(&Value) -> bool + Send + 'static,
    R: FnOnce(Value) -> BoxFuture<'static, Result<Value, RefreshError>> + Send + 'static,
{
    let pending = {
        let mut pending = PENDING.lock();
        match pending.get(id) {
            Some(running) => running.clone(),
            None => {
                let key = id.to_string();
                let task = tokio::spawn(async move {
                    let root = store.lock_root();
                    let lease = match &root {
                        Some(root) => match oc_util::flock::acquire(
                            root,
                            &format!("credential-refresh:{key}"),
                            oc_util::flock::Options::default(),
                        )
                        .await
                        {
                            Ok(lease) => Some(lease),
                            Err(error) => {
                                PENDING.lock().remove(&key);
                                return Err(RefreshError::Failed(error.to_string()));
                            }
                        },
                        None => None,
                    };
                    let result = locked(&*store, &key, stale, refresh).await;
                    if let Some(lease) = lease {
                        let _ = lease.release();
                    }
                    PENDING.lock().remove(&key);
                    result
                });
                let shared = async move { task.await.unwrap_or_else(|e| Err(RefreshError::Defect(e.to_string()))) }
                    .boxed()
                    .shared();
                pending.insert(id.to_string(), shared.clone());
                shared
            }
        }
    };
    pending.await
}

async fn locked<S, R>(store: &dyn McpCredentialStore, id: &str, stale: S, refresh: R) -> Result<Option<Value>, RefreshError>
where
    S: FnOnce(&Value) -> bool,
    R: FnOnce(Value) -> BoxFuture<'static, Result<Value, RefreshError>>,
{
    let read = || async {
        let row = store.get(id).await.map_err(|e| RefreshError::Defect(e.to_string()))?;
        Ok::<_, RefreshError>(row.map(|r| r.value).filter(is_oauth))
    };
    let Some(current) = read().await? else { return Ok(None) };
    if !stale(&current) {
        tracing::info!(credentialID = %id, reason = "already_refreshed", "credential refresh skipped");
        return Ok(Some(current));
    }
    let value = match tokio::time::timeout(REFRESH_TIMEOUT, refresh(current.clone())).await {
        Ok(result) => result?,
        Err(_) => return Err(RefreshError::Failed("Operation timed out after '30s'".into())),
    };
    let Some(latest) = read().await? else { return Ok(None) };
    if text(&latest, "access") != text(&current, "access") || text(&latest, "refresh") != text(&current, "refresh") {
        tracing::warn!(credentialID = %id, reason = "changed_meanwhile", "credential refresh discarded");
        return Ok(Some(latest));
    }
    store.update(id, value.clone()).await.map_err(|e| RefreshError::Defect(e.to_string()))?;
    Ok(Some(value))
}

/// `toTokens` as the token endpoint's JSON body.
pub fn tokens_json(credential: &Value) -> Value {
    let tokens = super::to_tokens(credential);
    let mut body = Map::new();
    body.insert("access_token".into(), json!(tokens.access_token));
    body.insert("token_type".into(), json!(tokens.token_type));
    if let Some(refresh) = &tokens.refresh_token {
        body.insert("refresh_token".into(), json!(refresh));
    }
    if let Some(expires_in) = tokens.expires_in {
        body.insert("expires_in".into(), json!(expires_in as u64));
    }
    if let Some(scope) = &tokens.scope {
        body.insert("scope".into(), json!(scope));
    }
    if let Some(issuer) = &tokens.issuer {
        body.insert("issuer".into(), json!(issuer));
    }
    Value::Object(body)
}

fn json_response(status: u16, status_text: &str, body: &Value) -> HttpResponse {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    HttpResponse { status, status_text: status_text.into(), headers, body: Bytes::from(body.to_string()) }
}

/// `unavailable(reason)`: `temporarily_unavailable` is an OAuth error the SDK surfaces as is, so the attempt fails
/// and the credential stays for the next one (the SDK takes a failure it cannot classify as a lost login).
pub fn unavailable(reason: &str) -> HttpResponse {
    json_response(
        503,
        "Service Unavailable",
        &json!({"error": "temporarily_unavailable", "error_description": format!("Could not refresh the OAuth token: {reason}")}),
    )
}

/// What the locked refresh needs to rebuild the credential (`connectProvider`).
#[derive(Clone)]
pub struct LockedRefresh {
    pub credentials: Arc<dyn McpCredentialStore>,
    pub id: String,
    pub method_id: String,
    pub server_url: String,
}

impl LockedRefresh {
    /// Sends one refresh request whose body carries `token`, in place of the in-process share, and answers it the
    /// way the token endpoint would: the stored tokens, `invalid_grant` when the credential was signed out
    /// meanwhile, the server's own rejection, or `temporarily_unavailable` for a failure that may pass.
    pub async fn send(&self, request: HttpRequest, token: String) -> Result<HttpResponse, McpError> {
        let (method_id, server_url) = (self.method_id.clone(), self.server_url.clone());
        let presented = token.clone();
        let result = refresh_credential(
            self.credentials.clone(),
            &self.id,
            move |current| text(current, "refresh") == presented,
            move |current| {
                async move {
                    // The body is read here so the time limit also covers a server that stalls after its headers.
                    let response = send_buffered(request).await.map_err(|e| RefreshError::Failed(e.to_string()))?;
                    let parsed = if response.ok() {
                        serde_json::from_slice::<Value>(&response.body).ok().and_then(|v| Tokens::parse(&v).ok())
                    } else {
                        None
                    };
                    let Some(mut tokens) = parsed else {
                        return Err(RefreshError::Rejected {
                            status: response.status,
                            status_text: response.status_text,
                            headers: response.headers,
                            body: response.body,
                        });
                    };
                    if tokens.refresh_token.is_none() {
                        tokens.refresh_token = Some(token);
                    }
                    // The SDK ignores an issuer in the token response; keep the one this credential was stamped with.
                    tokens.issuer = current.get("metadata").and_then(|m| m.get("issuer")).and_then(Value::as_str).map(String::from);
                    Ok(to_credential(&method_id, &server_url, &tokens, client_from_credential(&current).as_ref()))
                }
                .boxed()
            },
        )
        .await;
        match result {
            Ok(Some(value)) => Ok(json_response(200, "OK", &tokens_json(&value))),
            // The credential was signed out meanwhile; the SDK drops it and reports needs_auth.
            Ok(None) => Ok(json_response(400, "Bad Request", &json!({"error": "invalid_grant"}))),
            Err(RefreshError::Rejected { status, .. }) if status == 429 || status >= 500 => {
                Ok(unavailable(&format!("HTTP {status}")))
            }
            Err(RefreshError::Rejected { status, status_text, headers, body }) => {
                Ok(HttpResponse { status, status_text, headers, body })
            }
            Err(RefreshError::Failed(reason)) => Ok(unavailable(&reason)),
            Err(RefreshError::Defect(message)) => Err(McpError::other(message)),
        }
    }
}
