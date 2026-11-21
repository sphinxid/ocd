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
use crate::oauth::ConnectAuth;
use crate::oauth::fetch::Fetcher;
use crate::protocol::{Client, ClientConfig, ElicitationHandler, RequestOptions};
use crate::transport::http::{HttpOptions, HttpTransport};
use crate::transport::stdio::{StdioOptions, StdioTransport};
use crate::transport::{Transport, TransportKind};
use crate::types::{CallToolResult, ClientInfo, to_call_tool_result};

const TERMINATE_TIMEOUT: Duration = Duration::from_millis(1_000);
/// SEP-2243 HeaderMismatch; the SDK does not export the constant.
const HEADER_MISMATCH: i64 = -32020;

/// Why a connect failed (`MCP.NeedsAuthError` / `MCP.ConnectError`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectError {
    #[error("{message}")]
    NeedsAuth { server: String, message: String },
    /// `answered`: the server was reached but refused or broke the handshake, so connecting again, or differently,
    /// would not necessarily fail the same way.
    #[error("{message}")]
    Connect { server: String, message: String, answered: bool },
}

impl ConnectError {
    pub fn connect(server: &str, message: impl Into<String>) -> ConnectError {
        ConnectError::Connect { server: server.to_string(), message: message.into(), answered: false }
    }
}

/// `McpClient.ConnectOptions`.
pub struct ConnectOptions<'a> {
    pub server: &'a str,
    pub config: &'a ServerConfig,
    /// Resolves a relative stdio `cwd` and is the default one, and is the root `roots/list` answers with.
    pub directory: &'a Path,
    /// Remote only.
    pub auth_provider: Option<Arc<ConnectAuth>>,
    pub elicitation: Option<Arc<dyn ElicitationHandler>>,
    pub client_info: &'a ClientInfo,
    /// False declares no roots and leaves `roots/list` unanswered, for a connection no one directory owns.
    pub roots: bool,
    /// Experimental: declares MCP Events, a draft extension, and exposes [`Connection::events`] for a server that
    /// offers them.
    pub events: bool,
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

/// The opencode host's home (`Global.Path.home`): `OPENCODE_TEST_HOME`, else `os.homedir()`.
fn host_home() -> PathBuf {
    std::env::var_os("OPENCODE_TEST_HOME").map(PathBuf::from).unwrap_or_else(oc_util::global::os_home)
}

/// `McpStdio.resolveCwd` (`FileAccess.resolvePath`): a local server's configured `cwd` against the workspace
/// directory, expanding `~` to the opencode host's home (environments do not expose their own home).
pub fn resolve_cwd(directory: &Path, cwd: Option<&str>) -> PathBuf {
    match cwd {
        None | Some("") => directory.to_path_buf(),
        Some(cwd) => resolve_with_home(directory, cwd, &host_home()),
    }
}

fn resolve_with_home(directory: &Path, cwd: &str, home: &Path) -> PathBuf {
    if cwd == "~" {
        return resolve(directory, &home.to_string_lossy());
    }
    let rest = cwd.strip_prefix("~/").or_else(|| if cfg!(windows) { cwd.strip_prefix("~\\") } else { None });
    match rest {
        Some(rest) => resolve(directory, &home.join(rest).to_string_lossy()),
        None => resolve(directory, cwd),
    }
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
    options: &ConnectOptions<'_>,
    transport: Arc<dyn Transport>,
    events: mpsc::UnboundedReceiver<crate::transport::TransportEvent>,
    last_error: Arc<Mutex<Option<String>>>,
) -> Result<Opened, McpError> {
    let config = options.config;
    handshake(
        Handshake {
            server: options.server,
            directory: options.directory,
            elicitation: options.elicitation.clone(),
            client_info: options.client_info,
            roots: options.roots,
            events: options.events,
            negotiation: config.protocol(),
            startup: Duration::from_millis(config.startup_timeout_ms()),
        },
        transport,
        events,
        last_error,
    )
    .await
}

struct Handshake<'a> {
    server: &'a str,
    directory: &'a Path,
    elicitation: Option<Arc<dyn ElicitationHandler>>,
    client_info: &'a ClientInfo,
    roots: bool,
    events: bool,
    negotiation: Option<crate::config::Protocol>,
    startup: Duration,
}

async fn handshake(
    options: Handshake<'_>,
    transport: Arc<dyn Transport>,
    events: mpsc::UnboundedReceiver<crate::transport::TransportEvent>,
    last_error: Arc<Mutex<Option<String>>>,
) -> Result<Opened, McpError> {
    let server = options.server;
    let client = Client::new(
        transport.clone(),
        events,
        ClientConfig {
            server: server.to_string(),
            client_info: options.client_info.clone(),
            negotiation: options.negotiation,
            elicitation: options.elicitation.clone(),
            root_uri: file_url(options.directory),
            without_roots: !options.roots,
            events: options.events,
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
    match client.connect(options.startup).await {
        Ok(()) => Ok(Opened { client }),
        Err(error) => {
            transport.close().await;
            Err(error)
        }
    }
}

/// The execution timeout of an attached client's tool calls (`ATTACHED_EXECUTION_TIMEOUT`).
pub const ATTACHED_EXECUTION_TIMEOUT: Duration = Duration::from_secs(10 * 60);

fn connect_error(server: &str, error: &McpError, last_error: &Mutex<Option<String>>) -> ConnectError {
    match error {
        McpError::Unauthorized(message) | McpError::InsufficientScope(message) => {
            ConnectError::NeedsAuth { server: server.to_string(), message: message.clone() }
        }
        McpError::UnsupportedProtocolVersion { message, supported, .. } => ConnectError::connect(
            server,
            format!(
                "{message}; the server supports {}. Set \"protocol\" for this server to one of those or to \"legacy\".",
                supported.join(", ")
            ),
        ),
        other => ConnectError::Connect { server: server.to_string(), message: explain(other, last_error), answered: !other.repeats() },
    }
}

impl Connection {
    /// Connects a server that answers `roots/list` and does not declare MCP Events. `auth_provider` is used for
    /// remote servers only.
    pub async fn connect(
        server: &str,
        config: &ServerConfig,
        directory: &Path,
        auth_provider: Option<Arc<ConnectAuth>>,
        elicitation: Option<Arc<dyn ElicitationHandler>>,
        client_info: &ClientInfo,
    ) -> Result<Connection, ConnectError> {
        Self::open(ConnectOptions { server, config, directory, auth_provider, elicitation, client_info, roots: true, events: false })
            .await
    }

    /// `McpClient.connect`.
    pub async fn open(options: ConnectOptions<'_>) -> Result<Connection, ConnectError> {
        let (server, config, directory) = (options.server, options.config, options.directory);
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
                let cwd = resolve_cwd(directory, local.cwd.as_deref());
                let (tx, rx) = mpsc::unbounded_channel();
                let transport: Arc<dyn Transport> = StdioTransport::new(
                    StdioOptions { server: server.to_string(), command, args, cwd, environment },
                    tx,
                );
                initialize(&options, transport, rx, last_error.clone()).await
            }
            ServerConfig::Remote(remote) => {
                let Ok(configured) = Url::parse(&remote.url) else {
                    return Err(ConnectError::connect(server, format!("Invalid MCP server URL: {}", remote.url)));
                };
                let fetch = Fetcher::new(server.to_string());
                // Servers that bundle their own Code Mode expose raw tools with ?codemode=false.
                let mut url = configured.clone();
                let added_codemode = remote.codemode != Some(false)
                    && !url.query_pairs().any(|(k, _)| k == "codemode")
                    && !rejects_unknown_query(&url);
                if added_codemode {
                    let mut pairs: Vec<(String, String)> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
                    pairs.push(("codemode".into(), "false".into()));
                    url.set_query(Some(&crate::oauth::fetch::encode_form(&pairs)));
                }
                let deadline = Instant::now() + Duration::from_millis(config.startup_timeout_ms());
                let open = |url: Url| {
                    let fetch = fetch.clone();
                    let auth_provider = options.auth_provider.clone();
                    let last_error = last_error.clone();
                    let options = &options;
                    async move {
                        let (tx, rx) = mpsc::unbounded_channel();
                        let transport: Arc<dyn Transport> = HttpTransport::new(
                            HttpOptions { url, headers: remote.headers.clone(), auth_provider, fetch },
                            tx,
                        );
                        initialize(options, transport, rx, last_error).await
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
        let opened = opened.map_err(|error| connect_error(server, &error, &last_error))?;
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

    /// `McpClient.connect` with an `attached` config: an MCP server a client hosts over a transport the client already
    /// opened, such as a socket (`mcp/socket-client.ts`). Attached clients run ordinary SDK servers over the legacy
    /// `initialize` handshake, with the default startup and catalog timeouts and a 10-minute execution timeout. The
    /// connection answers `roots/list` and does not declare MCP Events.
    pub async fn attach(
        server: &str,
        transport: Arc<dyn Transport>,
        events: mpsc::UnboundedReceiver<crate::transport::TransportEvent>,
        directory: &Path,
        elicitation: Option<Arc<dyn ElicitationHandler>>,
        client_info: &ClientInfo,
    ) -> Result<Connection, ConnectError> {
        let last_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let opened = handshake(
            Handshake {
                server,
                directory,
                elicitation,
                client_info,
                roots: true,
                events: false,
                negotiation: Some(crate::config::Protocol::Legacy),
                startup: Duration::from_millis(TimeoutConfig::startup_ms(None)),
            },
            transport,
            events,
            last_error.clone(),
        )
        .await
        .map_err(|error| connect_error(server, &error, &last_error))?;
        Ok(Connection {
            server: server.to_string(),
            client: opened.client,
            last_error,
            session: Arc::new(Mutex::new(SessionState::default())),
            catalog: Duration::from_millis(TimeoutConfig::catalog_ms(None)),
            execution: ATTACHED_EXECUTION_TIMEOUT,
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

    /// The protocol revision the connection negotiated, such as "2025-11-25".
    pub fn protocol_version(&self) -> Option<String> {
        self.client.negotiated_protocol_version()
    }

    pub fn server(&self) -> &str {
        &self.server
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
        // A tool call that relists first can fail with the list's already mapped expiry.
        if matches!(error, McpError::SessionExpired { .. }) {
            return error;
        }
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
        self.list_value(what, run).await
    }

    /// Listing is read-only, so a transient failure is safe to retry.
    pub(crate) async fn list_value<T, F, Fut>(&self, what: &str, run: F) -> Result<T, McpError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, McpError>>,
    {
        let result = retrying(&self.server, what, None, run).await.map_err(|e| self.failure(e));
        if let Err(error) = &result {
            tracing::warn!(server = %self.server, error = %error, "failed to {what}");
        }
        result
    }

    /// `failure(error).message`.
    pub(crate) fn failure_message(&self, error: McpError) -> String {
        self.failure(error).to_string()
    }

    pub(crate) fn catalog_options(&self) -> RequestOptions {
        RequestOptions { timeout: Some(self.catalog), ..Default::default() }
    }

    pub async fn tools(&self) -> Result<Vec<Map<String, Value>>, McpError> {
        let options = self.catalog_options();
        let items = self.list("list MCP tools", || self.client.list_tools(&options)).await?;
        Ok(crate::tool_check::check_tools(&self.server, items))
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

    async fn request(
        &self,
        what: &str,
        method: &str,
        params: Value,
        progress: bool,
        elicitation: Option<Arc<dyn ElicitationHandler>>,
    ) -> Result<Value, McpError> {
        let options = RequestOptions { timeout: Some(self.execution), progress, elicitation, ..Default::default() };
        let result = if method == "tools/call" {
            let result = self.client.call_tool(params.clone(), &options).await;
            match result {
                // A modern server rejects headers mirrored from a stale definition before running the tool, so
                // relist and retry once with the fresh definition.
                Err(McpError::Protocol { code: HEADER_MISMATCH, .. }) if self.modern() => match self.tools().await {
                    Ok(_) => self.client.call_tool(params, &options).await,
                    Err(error) => Err(error),
                },
                other => other,
            }
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
        self.read_resource_for(uri, None).await
    }

    /// `readResource` whose elicitations go to `elicitation` (`PerRequest`).
    pub async fn read_resource_for(&self, uri: &str, elicitation: Option<Arc<dyn ElicitationHandler>>) -> Result<Option<Value>, McpError> {
        if !self.client.has_resources() {
            return Ok(None);
        }
        self.request("read MCP resource", "resources/read", json!({"uri": uri}), false, elicitation).await.map(Some)
    }

    /// `prompts/get` with `arguments: args ?? {}`.
    pub async fn prompt(&self, name: &str, args: Option<&IndexMap<String, String>>) -> Result<Value, McpError> {
        self.prompt_for(name, args, None).await
    }

    /// `prompts/get` whose elicitations go to `elicitation` (`PerRequest`).
    pub async fn prompt_for(
        &self,
        name: &str,
        args: Option<&IndexMap<String, String>>,
        elicitation: Option<Arc<dyn ElicitationHandler>>,
    ) -> Result<Value, McpError> {
        let arguments = serde_json::to_value(args.cloned().unwrap_or_default()).unwrap_or(json!({}));
        self.request("get MCP prompt", "prompts/get", json!({"name": name, "arguments": arguments}), false, elicitation).await
    }

    /// `tools/call` with `arguments: args ?? {}` and `_meta["ai.opencode/sessionID"]` when known.
    pub async fn call_tool(&self, name: &str, args: Option<Value>, session_id: Option<&str>) -> Result<CallToolResult, McpError> {
        self.call_tool_for(name, args, session_id, None).await
    }

    /// `tools/call` whose elicitations go to `elicitation` (`PerRequest`).
    pub async fn call_tool_for(
        &self,
        name: &str,
        args: Option<Value>,
        session_id: Option<&str>,
        elicitation: Option<Arc<dyn ElicitationHandler>>,
    ) -> Result<CallToolResult, McpError> {
        let mut params = json!({"name": name, "arguments": args.unwrap_or_else(|| json!({}))});
        if let Some(session_id) = session_id {
            params["_meta"] = json!({"ai.opencode/sessionID": session_id});
        }
        // Requesting progress keeps long calls alive; execution is the hard wall.
        let result = self.request("call MCP tool", "tools/call", params, true, elicitation).await?;
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

/// Hosts that reject any unknown query parameter and have no Code Mode of their own, so adding
/// `?codemode=false` only costs a failed request before the retry at the configured URL
/// (`rejectsUnknownQuery`).
pub fn rejects_unknown_query(url: &Url) -> bool {
    let host = match url.host() {
        Some(url::Host::Domain(domain)) => domain.to_string(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => format!("[{ip}]"),
        None => String::new(),
    };
    // Google Workspace MCP (gmailmcp, drivemcp, calendarmcp, ...) returns 404.
    if host.ends_with(".googleapis.com") {
        return true;
    }
    // Zoho MCP gateways (zohomcp.com, zohomcp.eu, ...) return 400.
    static ZOHO: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(^|\.)zohomcp\.[a-z.]+$").unwrap());
    if ZOHO.is_match(&host) {
        return true;
    }
    // Loopback servers run on this machine (Figma Desktop's Dev Mode MCP returns 404), so they are not
    // hosted Code Mode servers.
    static LOOPBACK: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^127(\.\d+){3}$").unwrap());
    host == "localhost" || host == "[::1]" || LOOPBACK.is_match(&host)
}

#[cfg(test)]
mod query_tests {
    use super::*;

    #[test]
    fn stdio_cwd_expands_the_host_home() {
        let (dir, home) = (Path::new("/work/project"), Path::new("/home/me"));
        assert_eq!(resolve_with_home(dir, "~", home), PathBuf::from("/home/me"));
        assert_eq!(resolve_with_home(dir, "~/tools/../bin", home), PathBuf::from("/home/me/bin"));
        assert_eq!(resolve_with_home(dir, "sub/./x", home), PathBuf::from("/work/project/sub/x"));
        assert_eq!(resolve_with_home(dir, "../other", home), PathBuf::from("/work/other"));
        assert_eq!(resolve_with_home(dir, "/abs", home), PathBuf::from("/abs"));
        assert_eq!(resolve_with_home(dir, "~user/x", home), PathBuf::from("/work/project/~user/x"));
        assert_eq!(resolve_cwd(dir, None), PathBuf::from("/work/project"));
        assert_eq!(resolve_cwd(dir, Some("")), PathBuf::from("/work/project"));
    }

    #[test]
    fn hosts_that_reject_unknown_query_parameters() {
        for url in [
            "https://gmailmcp.googleapis.com/mcp/v1",
            "https://mcp.zohomcp.com/x",
            "https://zohomcp.eu/x",
            "http://localhost:3845/mcp",
            "http://127.0.0.1:3845/mcp",
            "http://[::1]:3845/mcp",
        ] {
            assert!(rejects_unknown_query(&Url::parse(url).unwrap()), "{url}");
        }
        for url in ["https://mcp.example.com/mcp", "https://googleapis.com.evil.example/x", "http://10.0.0.1/mcp", "https://notzohomcp.com/x"] {
            assert!(!rejects_unknown_query(&Url::parse(url).unwrap()), "{url}");
        }
    }
}
