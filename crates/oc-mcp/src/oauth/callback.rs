//! Interactive OAuth authorization (`authorize`, `oauth.ts:330-532`) with its loopback callback
//! server on 127.0.0.1.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body as AxumBody;
use axum::extract::State;
use axum::http::{Response, StatusCode, Uri};
use base64::Engine;
use parking_lot::Mutex;
use rand::RngCore;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::discovery::{self, DiscoveryState, LATEST_PROTOCOL_VERSION, WwwAuthenticate};
use super::fetch::{Body, Fetcher, HttpRequest};
use super::page::{self, CallbackPageOptions};
use super::{
    AuthOptions, CLIENT_METADATA_URL, McpCredentialStore, MemoryStore, Provider, Redirect, TokenStore, auth,
    client_from_credential, configured_discovery, to_credential,
};
use crate::config::RemoteConfig;
use crate::error::McpError;

/// OAuth attempt lifetime (`integration.ts:261`).
pub const ATTEMPT_LIFETIME: Duration = Duration::from_secs(10 * 60);

pub struct AuthorizeOptions {
    pub name: String,
    pub config: RemoteConfig,
    pub integration_id: String,
    pub method_id: String,
    pub credentials: Option<Arc<dyn McpCredentialStore>>,
}

/// A started authorization: show `instructions` and open `url`; `wait()` resolves with the
/// credential JSON (see [`super::McpCredentialStore`]) once the browser completes the flow.
pub struct AuthorizeHandle {
    pub url: String,
    pub instructions: String,
    /// Always `"auto"`: the callback is received automatically.
    pub mode: &'static str,
    result: oneshot::Receiver<Result<Value, String>>,
    cancel: CancellationToken,
}

impl AuthorizeHandle {
    /// Waits for the callback and token exchange. Errors carry the failure message
    /// (`Authorization expired` when the attempt outlives its 10 minute lifetime).
    pub async fn wait(self) -> Result<Value, String> {
        let cancel = self.cancel.clone();
        let result = self.result.await.unwrap_or_else(|_| Err("Authorization cancelled".into()));
        cancel.cancel();
        result
    }

    /// Cancels the attempt and closes the callback server.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// The attempt's cancellation token: cancelling it closes the callback server; it is cancelled
    /// once [`AuthorizeHandle::wait`] resolves.
    pub fn canceller(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

struct CodeMessage {
    code: String,
    iss: Option<String>,
    respond: oneshot::Sender<(u16, String)>,
}

struct CallbackState {
    path: String,
    state: String,
    provider_name: String,
    code: Mutex<Option<oneshot::Sender<Result<CodeMessage, String>>>>,
    fields: String,
}

fn html_response(status: u16, html: String) -> Response<AxumBody> {
    Response::builder()
        .status(status)
        .header("Content-Type", "text/html")
        .body(AxumBody::from(html))
        .unwrap()
}

async fn handle_callback(State(state): State<Arc<CallbackState>>, uri: Uri) -> Response<AxumBody> {
    let url = Url::parse("http://127.0.0.1").unwrap().join(&uri.to_string()).unwrap_or_else(|_| Url::parse("http://127.0.0.1/").unwrap());
    if url.path() != state.path {
        return Response::builder().status(StatusCode::NOT_FOUND).body(AxumBody::from("Not found")).unwrap();
    }
    let param = |key: &str| url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned());
    let options = CallbackPageOptions::provider(state.provider_name.clone());
    let fail = |reason: String, failure: &str| {
        tracing::warn!(fields = %state.fields, reason = failure, "mcp oauth callback rejected");
        if let Some(sender) = state.code.lock().take() {
            let _ = sender.send(Err(reason.clone()));
        }
        html_response(400, page::error(&reason, &options))
    };
    if let Some(error) = param("error_description").or_else(|| param("error")) {
        return fail(error, "authorization_error");
    }
    let returned = param("state");
    // Browsers can reach the callback without any OAuth parameters (preflights, prefetches, reloads). Only a
    // redirect from the authorization server may end the login, so keep waiting for it.
    if returned.is_none() && param("code").is_none_or(|c| c.is_empty()) {
        return html_response(400, page::error("Missing authorization code", &options));
    }
    if returned.as_deref() != Some(state.state.as_str()) {
        return fail("OAuth state mismatch".into(), "state_mismatch");
    }
    let Some(code) = param("code").filter(|c| !c.is_empty()) else {
        return fail("Missing authorization code".into(), "missing_code");
    };
    let Some(sender) = state.code.lock().take() else {
        return Response::builder()
            .status(StatusCode::CONFLICT)
            .body(AxumBody::from("OAuth callback already received"))
            .unwrap();
    };
    // The page waits for the token exchange so the browser never reports success for a rejected code.
    let (respond, response) = oneshot::channel();
    if sender.send(Ok(CodeMessage { code, iss: param("iss"), respond })).is_err() {
        return Response::builder()
            .status(StatusCode::CONFLICT)
            .body(AxumBody::from("OAuth callback already received"))
            .unwrap();
    }
    match response.await {
        Ok((status, html)) => html_response(status, html),
        Err(_) => Response::builder().status(StatusCode::INTERNAL_SERVER_ERROR).body(AxumBody::empty()).unwrap(),
    }
}

/// Binds the loopback callback listener: `callback_port`, else the port pinned by
/// `redirect_uri`, else ephemeral.
pub async fn bind_callback(config: &RemoteConfig) -> Result<(tokio::net::TcpListener, u16, String), McpError> {
    let oauth = config.oauth_config();
    let redirect = oauth.and_then(|o| o.redirect_uri.as_deref()).and_then(|r| Url::parse(r).ok());
    let path = redirect.as_ref().map(|r| r.path().to_string()).unwrap_or_else(|| "/callback".to_string());
    let redirect_port = redirect.as_ref().and_then(|r| r.port()).filter(|p| *p != 0);
    let port = oauth.and_then(|o| o.callback_port).or(redirect_port).unwrap_or(0);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| McpError::other(e.to_string()))?;
    let port = listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|_| McpError::other("Could not determine MCP OAuth callback port"))?;
    Ok((listener, port, path))
}

/// Starts an interactive authorization for a remote server.
pub async fn authorize(options: AuthorizeOptions) -> Result<AuthorizeHandle, McpError> {
    let attempt = uuid::Uuid::new_v4().to_string();
    let fields = format!("server={} methodID={} oauthAttemptID={}", options.name, options.method_id, attempt);
    let result = authorize_inner(options.clone_parts(), fields.clone()).await;
    if let Err(error) = &result {
        tracing::warn!(server = %options.name, methodID = %options.method_id, errors = %error, "mcp oauth authorization setup failed");
    }
    result
}

impl AuthorizeOptions {
    fn clone_parts(&self) -> AuthorizeOptions {
        AuthorizeOptions {
            name: self.name.clone(),
            config: self.config.clone(),
            integration_id: self.integration_id.clone(),
            method_id: self.method_id.clone(),
            credentials: self.credentials.clone(),
        }
    }
}

async fn authorize_inner(input: AuthorizeOptions, fields: String) -> Result<AuthorizeHandle, McpError> {
    let mut fetch = Fetcher::new(input.name.clone());
    fetch.fields.push(("attempt".into(), fields.clone()));
    tracing::info!(fields = %fields, "mcp oauth authorization started");
    let oauth = input.config.oauth_config().cloned();
    let store = Arc::new(MemoryStore::default());
    // Reuse the client registered by an earlier login.
    if let Some(credentials) = &input.credentials {
        let previous = credentials.list(&input.integration_id).await.map_err(|e| McpError::other(e.to_string()))?.pop();
        if let Some(previous) = previous.filter(|p| p.value.get("type").and_then(Value::as_str) == Some("oauth")) {
            if let Some(client) = client_from_credential(&previous.value) {
                store.save_client_information(client).await?;
            }
        }
    }
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let state = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);

    let (listener, port, redirect_path) = bind_callback(&input.config).await?;
    let cancel = CancellationToken::new();
    let (code_tx, code_rx) = oneshot::channel();
    let callback_state = Arc::new(CallbackState {
        path: redirect_path.clone(),
        state: state.clone(),
        provider_name: input.name.clone(),
        code: Mutex::new(Some(code_tx)),
        fields: fields.clone(),
    });
    let router = axum::Router::new().fallback(handle_callback).with_state(callback_state);
    let shutdown = cancel.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).with_graceful_shutdown(async move { shutdown.cancelled().await }).await;
    });
    let guard = cancel.clone().drop_guard();

    let server_url = Url::parse(&input.config.url).map_err(|_| McpError::other(format!("Invalid URL: {}", input.config.url)))?;
    // The server's 401 names where its resource metadata lives and which scopes it wants.
    let challenge = probe_challenge(&fetch, &input.config).await;
    let resource_metadata_url = challenge.resource_metadata_url.clone();
    let discovered = match configured_discovery(&input.config, &fetch).await? {
        Some(state) => state,
        None => {
            let info = discovery::discover_server_info(&fetch, &server_url, resource_metadata_url.as_ref()).await?;
            DiscoveryState {
                authorization_server_url: info.authorization_server_url,
                resource_metadata_url: None,
                resource_metadata: info.resource_metadata,
                authorization_server_metadata: info.authorization_server_metadata,
            }
        }
    };
    let cimd = oauth.as_ref().and_then(|o| o.client_id.as_ref()).is_none()
        && oauth.as_ref().and_then(|o| o.redirect_uri.as_ref()).is_none()
        && discovered.authorization_server_metadata.as_ref().is_some_and(|m| {
            m.client_id_metadata_document_supported()
                && m.token_endpoint_auth_methods_supported().is_some_and(|l| l.iter().any(|x| x == "none"))
        });
    let registration = if oauth.as_ref().and_then(|o| o.client_id.as_ref()).is_some() {
        "static"
    } else if cimd {
        "cimd"
    } else {
        "dcr"
    };
    tracing::info!(fields = %fields, registration, "mcp oauth client registration selected");

    let authorization_url: Arc<Mutex<Option<Url>>> = Arc::new(Mutex::new(None));
    let captured = authorization_url.clone();
    let open_fields = fields.clone();
    let redirect_url = oauth
        .as_ref()
        .and_then(|o| o.redirect_uri.clone())
        .unwrap_or_else(|| format!("http://127.0.0.1:{port}{redirect_path}"));
    let provider = Provider::new(
        input.config.clone(),
        store.clone(),
        Some(Redirect {
            url: redirect_url,
            state,
            open: Box::new(move |url| {
                *captured.lock() = Some(url);
                tracing::info!(fields = %open_fields, "mcp oauth awaiting authorization");
            }),
        }),
        cimd.then(|| CLIENT_METADATA_URL.to_string()),
        Some(DiscoveryState { resource_metadata_url: resource_metadata_url.as_ref().map(Url::to_string), ..discovered }),
        fetch.clone(),
    )?;
    let scope = oauth.as_ref().and_then(|o| o.scope.clone()).or(challenge.scope.clone());
    auth(&provider, &AuthOptions { server_url: Some(server_url.clone()), scope: scope.clone(), ..Default::default() }, &fetch).await?;
    let Some(url) = authorization_url.lock().clone() else {
        return Err(McpError::other(format!("MCP server \"{}\" did not provide an authorization URL", input.name)));
    };

    let (result_tx, result_rx) = oneshot::channel();
    let name = input.name.clone();
    let method_id = input.method_id.clone();
    let config_url = input.config.url.clone();
    let deadline = cancel.clone();
    let task_guard = guard;
    tokio::spawn(async move {
        let _guard = task_guard;
        let received = tokio::select! {
            r = code_rx => r.unwrap_or_else(|_| Err("Authorization cancelled".to_string())),
            _ = tokio::time::sleep(ATTEMPT_LIFETIME) => Err("Authorization expired".to_string()),
            _ = deadline.cancelled() => Err("Authorization cancelled".to_string()),
        };
        let outcome = match received {
            Err(reason) => Err(reason),
            Ok(message) => {
                let exchange = async {
                    auth(
                        &provider,
                        &AuthOptions {
                            server_url: Some(server_url.clone()),
                            authorization_code: Some(message.code.clone()),
                            iss: message.iss.clone(),
                            scope: scope.clone(),
                            ..Default::default()
                        },
                        &fetch,
                    )
                    .await?;
                    let Some(tokens) = store.tokens().await? else {
                        return Err(McpError::other(format!("MCP server \"{name}\" did not return OAuth tokens")));
                    };
                    let client = store.client_information().await?;
                    tracing::info!(fields = %fields, hasRefreshToken = tokens.refresh_token.is_some(), expiresIn = ?tokens.expires_in, "mcp oauth authorization completed");
                    Ok(to_credential(&method_id, &config_url, &tokens, client.as_ref()))
                }
                .await;
                let options = CallbackPageOptions::provider(name.clone());
                match &exchange {
                    Ok(_) => {
                        let _ = message.respond.send((200, page::success(&options)));
                    }
                    Err(error) => {
                        let _ = message.respond.send((400, page::error(&error.to_string(), &options)));
                    }
                }
                exchange.map_err(|e| e.to_string())
            }
        };
        if let Err(reason) = &outcome {
            tracing::warn!(fields = %fields, errors = %reason, "mcp oauth authorization failed");
        }
        let _ = result_tx.send(outcome);
        // Let the held browser response flush before the graceful shutdown starts.
        tokio::time::sleep(Duration::from_millis(50)).await;
    });

    Ok(AuthorizeHandle {
        url: url.to_string(),
        instructions: format!("Authorize {} in your browser. This window will close automatically.", input.name),
        mode: "auto",
        result: result_rx,
        cancel,
    })
}

/// The 401 challenge probe: POST an `initialize` and read `WWW-Authenticate`; 5 s timeout,
/// failures ignored.
async fn probe_challenge(fetch: &Fetcher, config: &RemoteConfig) -> WwwAuthenticate {
    let Ok(url) = Url::parse(&config.url) else { return WwwAuthenticate::default() };
    let mut headers: Vec<(String, String)> =
        config.headers.iter().flatten().map(|(k, v)| (k.clone(), v.clone())).collect();
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-type") && !k.eq_ignore_ascii_case("accept"));
    headers.push(("content-type".into(), "application/json".into()));
    headers.push(("accept".into(), "application/json, text/event-stream".into()));
    let body = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {"protocolVersion": LATEST_PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": "opencode"}}
    });
    let request = HttpRequest { method: reqwest::Method::POST, url, headers, body: Some(Body::Json(body.to_string())) };
    match tokio::time::timeout(Duration::from_secs(5), fetch.fetch(request)).await {
        Ok(Ok(response)) => discovery::parse_www_authenticate(&response.headers),
        _ => WwwAuthenticate::default(),
    }
}
