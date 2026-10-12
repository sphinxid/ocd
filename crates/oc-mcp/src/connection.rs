//! One MCP server connection (`packages/core/src/mcp/client.ts`): transport selection,
//! transient retries, timeouts, session-expiry detection, teardown and result mapping.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use url::Url;

use crate::config::{ServerConfig, TimeoutConfig};
use crate::error::McpError;
use crate::oauth::Provider;
use crate::oauth::fetch::Fetcher;
use crate::protocol::{Client, ClientConfig, ElicitationHandler, RequestOptions};
use crate::transport::http::{HttpOptions, HttpTransport};
use crate::transport::stdio::{StdioOptions, StdioTransport};
use crate::transport::{Transport, TransportKind};
use crate::types::{CallToolResult, ClientInfo, to_call_tool_result};

const TERMINATE_TIMEOUT: Duration = Duration::from_millis(1_000);

/// Why a connect failed (`MCP.NeedsAuthError` / `MCP.ConnectError`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectError {
    #[error("{message}")]
    NeedsAuth { server: String, message: String },
    #[error("{message}")]
    Connect { server: String, message: String },
}

type Callback = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct SessionState {
    expired: Option<Callback>,
    reported: bool,
}

/// A connected MCP server. Dropping it does not close it; call [`Connection::close`].
pub struct Connection {
    server: String,
    client: Arc<Client>,
    last_error: Arc<Mutex<Option<String>>>,
    session: Arc<Mutex<SessionState>>,
    catalog: Duration,
    execution: Duration,
    closed: Mutex<bool>,
}

/// `file://` URL for a directory (`pathToFileURL(directory).href`).
pub fn file_url(directory: &Path) -> String {
    Url::from_file_path(directory).map(|u| u.to_string()).unwrap_or_else(|_| format!("file://{}", directory.display()))
}

fn resolve(directory: &Path, cwd: &str) -> PathBuf {
    let path = Path::new(cwd);
    let joined = if path.is_absolute() { path.to_path_buf() } else { directory.join(path) };
    // Normalize `.` and `..` like `path.resolve`.
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn explain(error: &McpError, last_error: &Mutex<Option<String>>) -> String {
    match (error.is_connection_closed(), last_error.lock().clone()) {
        (true, Some(last)) => format!("{}: {last}", error.describe()),
        _ => error.describe(),
    }
}

/// Retries transient failures twice (250 ms, then 1 s) while the deadline allows.
async fn retrying<T, F, Fut>(server: &str, what: &str, deadline: Option<Instant>, mut run: F) -> Result<T, McpError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, McpError>>,
{
    let delays = [Duration::from_millis(250), Duration::from_millis(1000)];
    let mut attempt = 0;
    loop {
        match run().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if attempt >= delays.len() || !error.is_transient() {
                    return Err(error);
                }
                let delay = delays[attempt];
                if deadline.is_some_and(|d| Instant::now() + delay >= d) {
                    return Err(error);
                }
                tracing::warn!(server = %server, error = %error.describe(), "retrying {what}");
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

struct Opened {
    client: Arc<Client>,
}

async fn initialize(
    server: &str,
    transport: Arc<dyn Transport>,
    events: mpsc::UnboundedReceiver<crate::transport::TransportEvent>,
    config: &ServerConfig,
    directory: &Path,
    elicitation: Option<Arc<dyn ElicitationHandler>>,
    client_info: &ClientInfo,
    last_error: Arc<Mutex<Option<String>>>,
) -> Result<Opened, McpError> {
    let client = Client::new(
        transport.clone(),
        events,
        ClientConfig {
            server: server.to_string(),
            client_info: client_info.clone(),
            negotiation: config.protocol(),
            elicitation,
            root_uri: file_url(directory),
        },
    );
    let name = server.to_string();
    // Background work such as the standalone SSE stream reports failures only here.
    client.set_on_error(move |error| {
        if matches!(error, McpError::Aborted(_)) {
            return;
        }
        let described = error.describe();
        *last_error.lock() = Some(described.clone());
        tracing::warn!(server = %name, error = %described, "mcp transport error");
    });
    let timeout = Duration::from_millis(config.startup_timeout_ms());
    match client.connect(timeout).await {
        Ok(()) => Ok(Opened { client }),
        Err(error) => {
            transport.close().await;
            Err(error)
        }
    }
}

impl Connection {
    /// Connects a server. `auth_provider` is used for remote servers only.
    pub async fn connect(
        server: &str,
        config: &ServerConfig,
        directory: &Path,
        auth_provider: Option<Arc<Provider>>,
        elicitation: Option<Arc<dyn ElicitationHandler>>,
        client_info: &ClientInfo,
    ) -> Result<Connection, ConnectError> {
        let last_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let session = Arc::new(Mutex::new(SessionState::default()));
        let opened: Result<Opened, McpError> = match config {
            ServerConfig::Local(local) => {
                let mut parts = local.command.iter();
                let command = parts.next().cloned().unwrap_or_default();
                let args: Vec<String> = parts.cloned().collect();
                let mut environment = IndexMap::new();
                if command == "opencode" {
                    environment.insert("BUN_BE_BUN".to_string(), "1".to_string());
                }
                for (k, v) in local.environment.iter().flatten() {
                    environment.insert(k.clone(), v.clone());
                }
                let cwd = match &local.cwd {
                    Some(cwd) => resolve(directory, cwd),
                    None => directory.to_path_buf(),
                };
                let (tx, rx) = mpsc::unbounded_channel();
                let transport: Arc<dyn Transport> = StdioTransport::new(
                    StdioOptions { server: server.to_string(), command, args, cwd, environment },
                    tx,
                );
                initialize(server, transport, rx, config, directory, elicitation, client_info, last_error.clone()).await
            }
            ServerConfig::Remote(remote) => {
                let Ok(configured) = Url::parse(&remote.url) else {
                    return Err(ConnectError::Connect {
                        server: server.to_string(),
                        message: format!("Invalid MCP URL for \"{server}\""),
                    });
                };
                let fetch = Fetcher::new(server.to_string());
                // Servers that bundle their own Code Mode expose raw tools with ?codemode=false.
                let mut url = configured.clone();
                let added_codemode = remote.codemode != Some(false) && !url.query_pairs().any(|(k, _)| k == "codemode");
                if added_codemode {
                    let mut pairs: Vec<(String, String)> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
                    pairs.push(("codemode".into(), "false".into()));
                    url.set_query(Some(&crate::oauth::fetch::encode_form(&pairs)));
                }
                let deadline = Instant::now() + Duration::from_millis(config.startup_timeout_ms());
                let open = |url: Url| {
                    let fetch = fetch.clone();
                    let auth_provider = auth_provider.clone();
                    let elicitation = elicitation.clone();
                    let last_error = last_error.clone();
                    async move {
                        let (tx, rx) = mpsc::unbounded_channel();
                        let transport: Arc<dyn Transport> = HttpTransport::new(
                            HttpOptions { url, headers: remote.headers.clone(), auth_provider, fetch },
                            tx,
                        );
                        initialize(server, transport, rx, config, directory, elicitation, client_info, last_error).await
                    }
                };
                retrying(server, "MCP connect", Some(deadline), || {
                    let open = &open;
                    let url = url.clone();
                    let configured = configured.clone();
                    async move {
                        match open(url).await {
                            Err(error)
                                if added_codemode && matches!(error.http_status(), Some(400) | Some(404)) =>
                            {
                                // Servers that reject unknown query params get one retry at the configured URL.
                                open(configured).await
                            }
                            other => other,
                        }
                    }
                })
                .await
            }
        };
        let opened = match opened {
            Ok(opened) => opened,
            Err(error) => {
                return Err(match &error {
                    McpError::Unauthorized(message) => {
                        ConnectError::NeedsAuth { server: server.to_string(), message: message.clone() }
                    }
                    McpError::UnsupportedProtocolVersion { message, supported, .. } => ConnectError::Connect {
                        server: server.to_string(),
                        message: format!(
                            "{message}; the server supports {}. Set \"protocol\" for this server to one of those or to \"legacy\".",
                            supported.join(", ")
                        ),
                    },
                    other => ConnectError::Connect { server: server.to_string(), message: explain(other, &last_error) },
                });
            }
        };
        Ok(Connection {
            server: server.to_string(),
            client: opened.client,
            last_error,
            session,
            catalog: Duration::from_millis(TimeoutConfig::catalog_ms(config.timeout())),
            execution: Duration::from_millis(TimeoutConfig::execution_ms(config.timeout())),
            closed: Mutex::new(false),
        })
    }

    pub fn client(&self) -> &Arc<Client> {
        &self.client
    }

    /// True when the connection negotiated revision 2026-07-28 or later.
    pub fn modern(&self) -> bool {
        self.client.is_modern()
    }

    /// `client.getInstructions()?.trim() || undefined`.
    pub fn instructions(&self) -> Option<String> {
        self.client.instructions().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }

    pub fn session_id(&self) -> Option<String> {
        self.client.transport().session_id()
    }

    /// Maps a request failure, detecting legacy session expiry.
    fn failure(&self, error: McpError) -> McpError {
        let status = error.http_status();
        let Some(status) = status else { return McpError::other(explain(&error, &self.last_error)) };
        if self.client.transport().session_id().is_none() || self.client.transport().kind() != TransportKind::Http {
            return McpError::other(explain(&error, &self.last_error));
        }
        let text = match &error {
            McpError::Http { text, .. } => text.clone().unwrap_or_default(),
            _ => String::new(),
        };
        let expired = status == 404 || (status == 400 && text.contains("Bad Request: Server not initialized"));
        if !expired {
            return McpError::other(explain(&error, &self.last_error));
        }
        let callback = {
            let mut session = self.session.lock();
            if session.reported {
                None
            } else {
                session.reported = true;
                session.expired.clone()
            }
        };
        if let Some(callback) = callback {
            callback();
        }
        McpError::SessionExpired { server: self.server.clone() }
    }

    async fn list<F, Fut>(&self, what: &str, run: F) -> Result<Vec<Value>, McpError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<Value>, McpError>>,
    {
        let result = retrying(&self.server, what, None, run).await.map_err(|e| self.failure(e));
        if let Err(error) = &result {
            tracing::warn!(server = %self.server, error = %error, "failed to {what}");
        }
        result
    }

    fn catalog_options(&self) -> RequestOptions {
        RequestOptions { timeout: Some(self.catalog), ..Default::default() }
    }

    pub async fn tools(&self) -> Result<Vec<Map<String, Value>>, McpError> {
        let options = self.catalog_options();
        let items = self.list("list MCP tools", || self.client.list_tools(&options)).await?;
        Ok(items.into_iter().filter_map(|v| v.as_object().cloned()).collect())
    }

    pub async fn prompts(&self) -> Result<Vec<Map<String, Value>>, McpError> {
        let options = self.catalog_options();
        let items = self.list("list MCP prompts", || self.client.list_prompts(&options)).await?;
        Ok(items.into_iter().filter_map(|v| v.as_object().cloned()).collect())
    }

    pub async fn resources(&self) -> Result<Vec<Map<String, Value>>, McpError> {
        let options = self.catalog_options();
        let items = self.list("list MCP resources", || self.client.list_resources(&options)).await?;
        Ok(items.into_iter().filter_map(|v| v.as_object().cloned()).collect())
    }

    pub async fn resource_templates(&self) -> Result<Vec<Map<String, Value>>, McpError> {
        let options = self.catalog_options();
        let items = self
            .list("list MCP resource templates", || self.client.list_resource_templates(&options))
            .await?;
        Ok(items.into_iter().filter_map(|v| v.as_object().cloned()).collect())
    }

    async fn request(&self, what: &str, method: &str, params: Value, progress: bool) -> Result<Value, McpError> {
        let options = RequestOptions { timeout: Some(self.execution), progress, ..Default::default() };
        let result = if method == "tools/call" {
            self.client.call_tool(params, &options).await
        } else {
            self.client.request(method, Some(params), &options).await
        };
        let result = result.map_err(|e| self.failure(e));
        if let Err(error) = &result {
            tracing::warn!(server = %self.server, error = %error, "failed to {what}");
        }
        result
    }

    /// `readResource`: `Ok(None)` when the server does not advertise resources.
    pub async fn read_resource(&self, uri: &str) -> Result<Option<Value>, McpError> {
        if !self.client.has_resources() {
            return Ok(None);
        }
        self.request("read MCP resource", "resources/read", json!({"uri": uri}), false).await.map(Some)
    }

    /// `prompts/get` with `arguments: args ?? {}`.
    pub async fn prompt(&self, name: &str, args: Option<&IndexMap<String, String>>) -> Result<Value, McpError> {
        let arguments = serde_json::to_value(args.cloned().unwrap_or_default()).unwrap_or(json!({}));
        self.request("get MCP prompt", "prompts/get", json!({"name": name, "arguments": arguments}), false).await
    }

    /// `tools/call` with `arguments: args ?? {}` and `_meta["ai.opencode/sessionID"]` when known.
    pub async fn call_tool(&self, name: &str, args: Option<Value>, session_id: Option<&str>) -> Result<CallToolResult, McpError> {
        let mut params = json!({"name": name, "arguments": args.unwrap_or_else(|| json!({}))});
        if let Some(session_id) = session_id {
            params["_meta"] = json!({"ai.opencode/sessionID": session_id});
        }
        // Requesting progress keeps long calls alive; execution is the hard wall.
        let result = self.request("call MCP tool", "tools/call", params, true).await?;
        Ok(to_call_tool_result(&result))
    }

    pub fn on_close(&self, callback: impl Fn(String) + Send + Sync + 'static) {
        let last_error = self.last_error.clone();
        self.client.set_on_close(move || {
            let reason = match last_error.lock().clone() {
                Some(last) => format!("Connection closed: {last}"),
                None => "Connection closed".to_string(),
            };
            callback(reason);
        });
    }

    pub fn on_session_expired(&self, callback: impl Fn() + Send + Sync + 'static) {
        self.session.lock().expired = Some(Arc::new(callback));
    }

    pub fn on_tools_changed(&self, callback: impl Fn() + Send + Sync + 'static) {
        self.client.set_on_tools_changed(callback);
    }

    pub fn on_prompts_changed(&self, callback: impl Fn() + Send + Sync + 'static) {
        self.client.set_on_prompts_changed(callback);
    }

    pub fn on_resources_changed(&self, callback: impl Fn() + Send + Sync + 'static) {
        self.client.set_on_resources_changed(callback);
    }

    /// Teardown: terminate the legacy HTTP session (1 s timeout) unless expiry was reported, then
    /// close the transport (killing a stdio process).
    pub async fn close(&self) {
        {
            let mut closed = self.closed.lock();
            if *closed {
                return;
            }
            *closed = true;
        }
        self.client.clear_on_error();
        let transport = self.client.transport().clone();
        let reported = self.session.lock().reported;
        if transport.session_id().is_some() && !reported {
            let result = match tokio::time::timeout(TERMINATE_TIMEOUT, transport.terminate_session()).await {
                Ok(result) => result,
                Err(_) => Err(McpError::other(format!("Timed out after {}ms", TERMINATE_TIMEOUT.as_millis()))),
            };
            if let Err(error) = result {
                tracing::warn!(server = %self.server, error = %error, "failed to terminate MCP session");
            }
        }
        self.client.close().await;
    }
}
