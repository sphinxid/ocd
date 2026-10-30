//! `McpManager`: the per-Location server registry and lifecycle
//! (`packages/core/src/mcp/index.ts`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use indexmap::IndexMap;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};

use crate::config::{ServerConfig, apply_codemode_defaults};
use crate::connection::{ConnectError, Connection};
use crate::error::McpError;
use crate::oauth::fetch::Fetcher;
use crate::oauth::{self, AuthorizeHandle, AuthorizeOptions, McpCredentialStore};
use crate::protocol::ElicitationHandler;
use crate::types::{
    ClientInfo, McpEvent, Prompt, PromptResult, Resource, ResourceCatalog, ResourceContent, ResourceContentPart,
    ResourceTemplate, ServerInfo, Status, Tool, ToolResult,
};

/// Errors surfaced by the registry (`MCP.NotFoundError`, `MCP.ToolCallError`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManagerError {
    #[error("MCP server not found: {0}")]
    NotFound(String),
    #[error("{message}")]
    ToolCall { server: String, tool: String, message: String },
    #[error("{0}")]
    Other(String),
}

/// `unavailable()` (`index.ts:65-76`).
pub fn unavailable_message(server: &str, status: &Status) -> String {
    match status {
        Status::Failed { error } => format!("MCP server \"{server}\" is not connected: {error}. Reconnect it from /mcps."),
        Status::NeedsAuth { error } => format!("MCP server \"{server}\" needs authentication: {error}. Sign in from /mcps."),
        Status::Disabled => format!("MCP server \"{server}\" is disabled."),
        _ => format!("MCP server \"{server}\" is not connected."),
    }
}

/// `String.prototype.localeCompare` approximation for ASCII names: case-insensitive first,
/// lowercase before uppercase on ties.
pub fn locale_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let la = a.to_lowercase();
    let lb = b.to_lowercase();
    la.cmp(&lb).then_with(|| {
        for (x, y) in a.chars().zip(b.chars()) {
            if x != y {
                return match (x.is_lowercase(), y.is_lowercase()) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => x.cmp(&y),
                };
            }
        }
        a.len().cmp(&b.len())
    })
}

pub struct ManagerOptions {
    /// The Location directory (stdio cwd base, `roots/list`).
    pub directory: PathBuf,
    /// `{name:"opencode", version: app version}`.
    pub client_info: ClientInfo,
    pub credentials: Option<Arc<dyn McpCredentialStore>>,
    pub elicitation: Option<Arc<dyn ElicitationHandler>>,
}

#[derive(Default)]
struct KeyedMutex {
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl KeyedMutex {
    async fn lock(&self, key: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self.locks.lock().entry(key.to_string()).or_default().clone();
        lock.lock_owned().await
    }
}

// Concurrent startups to the same remote URL are serialized across all Locations.
static ENDPOINT_LOADS: Lazy<KeyedMutex> = Lazy::new(KeyedMutex::default);

#[derive(Default)]
struct EntryState {
    status: Option<Status>,
    connection: Option<Arc<Connection>>,
    tools: Option<Vec<Tool>>,
    prompts: Option<Vec<Prompt>>,
}

struct Entry {
    config: ServerConfig,
    integration_id: Option<String>,
    state: Mutex<EntryState>,
    startup: watch::Sender<bool>,
}

impl Entry {
    fn new(name: &str, config: ServerConfig) -> Arc<Self> {
        let integration_id = match &config {
            ServerConfig::Remote(remote) if !remote.oauth_disabled() => Some(oauth::integration_id(name, &remote.url)),
            _ => None,
        };
        Arc::new(Self {
            config,
            integration_id,
            state: Mutex::new(EntryState { status: Some(Status::Pending), ..Default::default() }),
            startup: watch::channel(false).0,
        })
    }

    fn status(&self) -> Status {
        self.state.lock().status.clone().unwrap_or(Status::Pending)
    }

    fn set_status(&self, status: Status) {
        self.state.lock().status = Some(status);
    }

    fn connection(&self) -> Option<Arc<Connection>> {
        self.state.lock().connection.clone()
    }

    fn open_startup(&self) {
        self.startup.send_replace(true);
    }

    async fn await_startup(&self) {
        let mut rx = self.startup.subscribe();
        let _ = rx.wait_for(|open| *open).await;
    }

    fn is_live(&self, connection: &Arc<Connection>) -> bool {
        self.connection().is_some_and(|c| Arc::ptr_eq(&c, connection))
    }
}

struct Inner {
    options: ManagerOptions,
    entries: Mutex<IndexMap<String, Arc<Entry>>>,
    locks: KeyedMutex,
    events: broadcast::Sender<McpEvent>,
    base: Mutex<IndexMap<String, ServerConfig>>,
    overrides: Mutex<IndexMap<String, Option<ServerConfig>>>,
    applied: Mutex<Option<IndexMap<String, ServerConfig>>>,
    reconcile_lock: tokio::sync::Mutex<()>,
    sink: Mutex<Option<EventSink>>,
}

/// Receives every `mcp.*.changed` event synchronously, in publication order (see [`McpManager::on_event`]).
pub type EventSink = Arc<dyn Fn(&McpEvent) + Send + Sync>;

/// The MCP server registry for one Location. Cheap to clone.
#[derive(Clone)]
pub struct McpManager {
    inner: Arc<Inner>,
}

impl McpManager {
    pub fn new(options: ManagerOptions) -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            inner: Arc::new(Inner {
                options,
                entries: Mutex::new(IndexMap::new()),
                locks: KeyedMutex::default(),
                events,
                base: Mutex::new(IndexMap::new()),
                overrides: Mutex::new(IndexMap::new()),
                applied: Mutex::new(None),
                reconcile_lock: tokio::sync::Mutex::new(()),
                sink: Mutex::new(None),
            }),
        }
    }

    /// Subscribes to `mcp.*.changed` events.
    pub fn subscribe(&self) -> broadcast::Receiver<McpEvent> {
        self.inner.events.subscribe()
    }

    /// Delivers every event to `sink` as it is published, before the broadcast subscribers see it. Upstream
    /// publishes on the bus directly, so the startup `pending` statuses precede whatever the caller
    /// publishes after [`McpManager::set_servers`] returns (the location boot's `*.updated` burst).
    pub fn on_event(&self, sink: EventSink) {
        *self.inner.sink.lock() = Some(sink);
    }

    fn publish(&self, event: McpEvent) {
        let sink = self.inner.sink.lock().clone();
        if let Some(sink) = sink {
            sink(&event);
        }
        let _ = self.inner.events.send(event);
    }

    fn entry(&self, name: &str) -> Option<Arc<Entry>> {
        self.inner.entries.lock().get(name).cloned()
    }

    fn require(&self, name: &str) -> Result<Arc<Entry>, ManagerError> {
        self.entry(name).ok_or_else(|| ManagerError::NotFound(name.to_string()))
    }

    /// The effective server map: runtime overrides win, removed names are ignored, then the
    /// built-in codemode defaults apply.
    fn effective(&self) -> IndexMap<String, ServerConfig> {
        let overrides = self.inner.overrides.lock().clone();
        let mut servers: IndexMap<String, ServerConfig> =
            overrides.iter().filter_map(|(k, v)| v.clone().map(|v| (k.clone(), v))).collect();
        for (name, config) in self.inner.base.lock().iter() {
            if servers.contains_key(name) || overrides.get(name).is_some_and(Option::is_none) {
                continue;
            }
            servers.insert(name.clone(), config.clone());
        }
        apply_codemode_defaults(&mut servers);
        servers
    }

    /// Loads (or reloads after `config.updated`) the configured servers — the output of
    /// [`crate::config::merge_documents`] after policy filtering — and reconciles.
    ///
    /// The first call creates every entry as `pending`, marks disabled ones and starts the rest
    /// in the background. Later calls replace servers whose config changed (deep equality),
    /// remove missing ones and leave unchanged ones alone.
    pub async fn set_servers(&self, servers: IndexMap<String, ServerConfig>) {
        *self.inner.base.lock() = servers;
        self.reconcile().await;
    }

    async fn reconcile(&self) {
        let _permit = self.inner.reconcile_lock.lock().await;
        let servers = self.effective();
        let initial = self.inner.applied.lock().is_none() && self.inner.entries.lock().is_empty();
        if initial {
            let entries: Vec<(String, Arc<Entry>)> =
                servers.iter().map(|(name, config)| (name.clone(), Entry::new(name, config.clone()))).collect();
            {
                let mut map = self.inner.entries.lock();
                for (name, entry) in &entries {
                    map.insert(name.clone(), entry.clone());
                }
            }
            *self.inner.applied.lock() = Some(servers);
            // Initial connections stay asynchronous so one slow server does not block startup.
            let mut starting = Vec::new();
            for (name, entry) in entries {
                if entry.config.disabled() {
                    entry.set_status(Status::Disabled);
                    entry.open_startup();
                    self.publish(McpEvent::StatusChanged { server: name });
                    continue;
                }
                starting.push((name, entry));
            }
            // Upstream forks each startup, and the forked fibers announce `pending` before the State
            // notification that started them returns; announcing here keeps that order.
            for (name, entry) in &starting {
                entry.set_status(Status::Pending);
                self.publish(McpEvent::StatusChanged { server: name.clone() });
            }
            for (name, entry) in starting {
                let this = self.clone();
                tokio::spawn(async move {
                    let _guard = this.inner.locks.lock(&name).await;
                    // connect() or disconnect() may have settled the server while this startup waited to run.
                    if entry.status() != Status::Pending {
                        entry.open_startup();
                        return;
                    }
                    this.connect_server(&name, &entry).await;
                });
            }
            return;
        }
        let previous = self.inner.applied.lock().clone().unwrap_or_default();
        let mut names: Vec<String> = previous.keys().cloned().collect();
        for name in servers.keys() {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        for name in names {
            let before = previous.get(&name);
            let after = servers.get(&name);
            if before == after {
                continue;
            }
            let _guard = self.inner.locks.lock(&name).await;
            match after {
                None => self.remove_server(&name).await,
                Some(config) => self.replace_server(&name, config.clone()).await,
            }
        }
        *self.inner.applied.lock() = Some(servers);
    }

    async fn replace_server(&self, name: &str, config: ServerConfig) {
        if let Some(previous) = self.entry(name) {
            self.stop_server(name, &previous).await;
        }
        let entry = Entry::new(name, config);
        self.inner.entries.lock().insert(name.to_string(), entry.clone());
        if entry.config.disabled() {
            entry.set_status(Status::Disabled);
            self.publish(McpEvent::StatusChanged { server: name.to_string() });
        } else {
            self.start_server(name, &entry).await;
        }
        entry.open_startup();
    }

    async fn remove_server(&self, name: &str) {
        let Some(entry) = self.entry(name) else { return };
        self.stop_server(name, &entry).await;
        // Credentials are keyed by name + URL and intentionally survive removal.
        self.inner.entries.lock().shift_remove(name);
        self.publish(McpEvent::StatusChanged { server: name.to_string() });
    }

    fn start_server<'a>(&'a self, name: &'a str, entry: &'a Arc<Entry>) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            // Announce the handshake so connect() and credential reconnects don't show a stale
            // disabled/failed status for the duration of the connection attempt.
            entry.set_status(Status::Pending);
            self.publish(McpEvent::StatusChanged { server: name.to_string() });
            self.connect_server(name, entry).await;
        })
    }

    /// `startServer` after its `pending` announcement.
    fn connect_server<'a>(&'a self, name: &'a str, entry: &'a Arc<Entry>) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            let connection_id = uuid::Uuid::new_v4().to_string();
            let directory = self.inner.options.directory.display().to_string();
            let result = async {
                let provider = match (&entry.config, &entry.integration_id) {
                    (ServerConfig::Remote(remote), Some(integration_id)) => Some(Arc::new(
                        oauth::connect_provider(
                            remote,
                            integration_id,
                            self.inner.options.credentials.as_ref(),
                            Fetcher::new(name.to_string()).with_base_headers(remote.headers.clone()),
                        )
                        .await
                        .map_err(|e| ConnectError::Connect { server: name.to_string(), message: e.to_string() })?,
                    )),
                    _ => None,
                };
                let _endpoint = match &entry.config {
                    ServerConfig::Remote(remote) => Some(ENDPOINT_LOADS.lock(&remote.url).await),
                    _ => None,
                };
                let connection = Connection::connect(
                    name,
                    &entry.config,
                    &self.inner.options.directory,
                    provider,
                    self.inner.options.elicitation.clone(),
                    &self.inner.options.client_info,
                )
                .await?;
                // List tools as part of connect so a failure marks the server failed.
                match connection.tools().await {
                    Ok(tools) => Ok((connection, tools)),
                    Err(error) => {
                        connection.close().await;
                        Err(ConnectError::Connect { server: name.to_string(), message: error.to_string() })
                    }
                }
            }
            .await;
            match result {
                Ok((connection, tools)) => {
                    let connection = Arc::new(connection);
                    let codemode = entry.config.codemode();
                    let tools: Vec<Tool> = tools.into_iter().filter_map(|t| Tool::from_raw(name, codemode, t)).collect();
                    let count = tools.len();
                    {
                        let mut state = entry.state.lock();
                        state.connection = Some(connection.clone());
                        state.tools = Some(tools);
                        state.prompts = Some(vec![]);
                        state.status = Some(Status::Connected);
                    }
                    self.watch(name, entry, &connection);
                    tracing::info!(server = %name, directory = %directory, connectionID = %connection_id, tools = count, "mcp connected");
                    self.publish(McpEvent::ToolsChanged { server: name.to_string() });
                    self.publish(McpEvent::ResourcesChanged { server: name.to_string() });
                    self.publish(McpEvent::StatusChanged { server: name.to_string() });
                    let this = self.clone();
                    let name = name.to_string();
                    let entry = entry.clone();
                    tokio::spawn(async move {
                        let _guard = this.inner.locks.lock(&name).await;
                        if entry.is_live(&connection) {
                            this.refresh_prompts(&name, &entry, &connection).await;
                        }
                    });
                }
                Err(error) => {
                    let status = match error {
                        ConnectError::NeedsAuth { message, .. } => Status::NeedsAuth { error: message },
                        ConnectError::Connect { message, .. } => Status::Failed { error: message },
                    };
                    tracing::warn!(server = %name, directory = %directory, connectionID = %connection_id, status = ?status, "mcp connect failed");
                    entry.set_status(status);
                    self.publish(McpEvent::StatusChanged { server: name.to_string() });
                }
            }
            entry.open_startup();
        })
    }

    async fn stop_server(&self, name: &str, entry: &Arc<Entry>) {
        let connection = {
            let mut state = entry.state.lock();
            let connection = state.connection.take();
            if connection.is_some() {
                state.tools = None;
                state.prompts = None;
            }
            connection
        };
        let Some(connection) = connection else { return };
        connection.close().await;
        self.publish(McpEvent::ToolsChanged { server: name.to_string() });
        self.publish(McpEvent::ResourcesChanged { server: name.to_string() });
        self.publish(McpEvent::PromptsChanged { server: name.to_string() });
    }

    async fn refresh_prompts(&self, name: &str, entry: &Arc<Entry>, connection: &Arc<Connection>) {
        let prompts = connection.prompts().await.unwrap_or_default();
        entry.state.lock().prompts = Some(prompts.into_iter().filter_map(|p| Prompt::from_raw(name, p)).collect());
        self.publish(McpEvent::PromptsChanged { server: name.to_string() });
    }

    /// Runs `f` under the server lock only while `connection` is still the entry's live client.
    fn when_live<F, Fut>(&self, name: &str, entry: &Arc<Entry>, connection: &Arc<Connection>, f: F)
    where
        F: FnOnce(McpManager, String, Arc<Entry>, Arc<Connection>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let this = self.clone();
        let name = name.to_string();
        let entry = entry.clone();
        let connection = connection.clone();
        tokio::spawn(async move {
            let _guard = this.inner.locks.lock(&name).await;
            if entry.is_live(&connection) {
                f(this.clone(), name, entry, connection).await;
            }
        });
    }

    fn watch(&self, name: &str, entry: &Arc<Entry>, connection: &Arc<Connection>) {
        let weak = Arc::downgrade(connection);
        let (this, n, e) = (self.clone(), name.to_string(), entry.clone());
        connection.on_close(move |reason| {
            let Some(connection) = weak.upgrade() else { return };
            this.when_live(&n, &e, &connection, move |this, name, entry, _| async move {
                entry.set_status(Status::Failed { error: reason });
                this.stop_server(&name, &entry).await;
                this.publish(McpEvent::StatusChanged { server: name });
            });
        });
        let weak = Arc::downgrade(connection);
        let (this, n, e) = (self.clone(), name.to_string(), entry.clone());
        connection.on_session_expired(move || {
            let Some(connection) = weak.upgrade() else { return };
            let (this, n, e) = (this.clone(), n.clone(), e.clone());
            tokio::spawn(async move { this.recover(&n, &e, &connection).await });
        });
        let weak = Arc::downgrade(connection);
        let (this, n, e) = (self.clone(), name.to_string(), entry.clone());
        connection.on_tools_changed(move || {
            let Some(connection) = weak.upgrade() else { return };
            this.when_live(&n, &e, &connection, |this, name, entry, connection| async move {
                if let Ok(tools) = connection.tools().await {
                    let codemode = entry.config.codemode();
                    entry.state.lock().tools =
                        Some(tools.into_iter().filter_map(|t| Tool::from_raw(&name, codemode, t)).collect());
                    this.publish(McpEvent::ToolsChanged { server: name });
                }
            });
        });
        let weak = Arc::downgrade(connection);
        let (this, n, e) = (self.clone(), name.to_string(), entry.clone());
        connection.on_prompts_changed(move || {
            let Some(connection) = weak.upgrade() else { return };
            this.when_live(&n, &e, &connection, |this, name, entry, connection| async move {
                this.refresh_prompts(&name, &entry, &connection).await;
            });
        });
        let weak = Arc::downgrade(connection);
        let (this, n, e) = (self.clone(), name.to_string(), entry.clone());
        connection.on_resources_changed(move || {
            let Some(connection) = weak.upgrade() else { return };
            this.when_live(&n, &e, &connection, |this, name, _, _| async move {
                this.publish(McpEvent::ResourcesChanged { server: name });
            });
        });
    }

    /// Re-establishes a server whose HTTP session expired.
    async fn recover(&self, name: &str, entry: &Arc<Entry>, connection: &Arc<Connection>) {
        let _guard = self.inner.locks.lock(name).await;
        if !entry.is_live(connection) {
            return;
        }
        tracing::info!(server = %name, "mcp session expired, reconnecting");
        self.stop_server(name, entry).await;
        self.start_server(name, entry).await;
    }

    /// Runs a request and, on session expiry, reconnects and retries once on the new client.
    async fn recovering<T, F, Fut>(&self, name: &str, entry: &Arc<Entry>, connection: Arc<Connection>, run: F) -> Result<T, McpError>
    where
        F: Fn(Arc<Connection>) -> Fut,
        Fut: std::future::Future<Output = Result<T, McpError>>,
    {
        match run(connection.clone()).await {
            Err(error @ McpError::SessionExpired { .. }) => {
                self.recover(name, entry, &connection).await;
                match entry.connection() {
                    Some(next) => run(next).await,
                    None => Err(error),
                }
            }
            other => other,
        }
    }

    // ---- public interface -------------------------------------------------------------------

    /// `servers()`: `{name, status, title?, integrationID?}` sorted by name.
    pub fn servers(&self) -> Vec<ServerInfo> {
        let mut servers: Vec<ServerInfo> = self
            .inner
            .entries
            .lock()
            .iter()
            .map(|(name, entry)| ServerInfo {
                name: name.clone(),
                status: entry.status(),
                title: entry.connection().and_then(|c| server_title(c.client().server_info())),
                integration_id: entry.integration_id.clone(),
            })
            .collect();
        servers.sort_by(|a, b| locale_cmp(&a.name, &b.name));
        servers
    }

    pub fn status(&self, server: &str) -> Option<Status> {
        self.entry(server).map(|e| e.status())
    }

    /// The OAuth integration id of a remote server (`None` for local servers or `oauth: false`).
    pub fn integration_id(&self, server: &str) -> Option<String> {
        self.entry(server).and_then(|e| e.integration_id.clone())
    }

    /// Waits until the server's first connect attempt has settled.
    pub async fn wait_ready(&self, server: &str) -> Result<(), ManagerError> {
        self.require(server)?.await_startup().await;
        Ok(())
    }

    /// Runtime add/replace (not persisted); connects immediately.
    pub async fn add(&self, server: &str, config: ServerConfig) {
        self.inner.overrides.lock().insert(server.to_string(), Some(config));
        self.reconcile().await;
    }

    /// Stop + start; overrides `disabled` until restart.
    pub async fn connect(&self, server: &str) -> Result<(), ManagerError> {
        let _guard = self.inner.locks.lock(server).await;
        let entry = self.require(server)?;
        self.stop_server(server, &entry).await;
        self.start_server(server, &entry).await;
        Ok(())
    }

    /// Stop and mark `disabled` until reconnect.
    pub async fn disconnect(&self, server: &str) -> Result<(), ManagerError> {
        let _guard = self.inner.locks.lock(server).await;
        let entry = self.require(server)?;
        self.stop_server(server, &entry).await;
        entry.set_status(Status::Disabled);
        self.publish(McpEvent::StatusChanged { server: server.to_string() });
        Ok(())
    }

    /// Removes the server until restart (credentials survive).
    pub async fn remove(&self, server: &str) -> Result<(), ManagerError> {
        self.require(server)?;
        self.inner.overrides.lock().insert(server.to_string(), None);
        self.reconcile().await;
        Ok(())
    }

    /// All connected tools sorted by server, then name.
    pub fn tools(&self) -> Vec<Tool> {
        let mut tools: Vec<Tool> =
            self.inner.entries.lock().values().flat_map(|e| e.state.lock().tools.clone().unwrap_or_default()).collect();
        tools.sort_by(|a, b| locale_cmp(&a.server, &b.server).then_with(|| locale_cmp(&a.name, &b.name)));
        tools
    }

    /// Calls a tool; waits for the server's startup to settle first.
    pub async fn call_tool(
        &self,
        server: &str,
        name: &str,
        args: Option<Value>,
        session_id: Option<&str>,
    ) -> Result<ToolResult, ManagerError> {
        let entry = self.require(server)?;
        entry.await_startup().await;
        let Some(connection) = entry.connection() else {
            return Err(ManagerError::ToolCall {
                server: server.to_string(),
                tool: name.to_string(),
                message: unavailable_message(server, &entry.status()),
            });
        };
        let result = self
            .recovering(server, &entry, connection, |c| {
                let args = args.clone();
                async move { c.call_tool(name, args, session_id).await }
            })
            .await
            .map_err(|error| ManagerError::ToolCall {
                server: server.to_string(),
                tool: name.to_string(),
                message: format!("MCP tool \"{name}\" on server \"{server}\" failed: {error}"),
            })?;
        Ok(ToolResult {
            server: server.to_string(),
            tool: name.to_string(),
            is_error: result.is_error,
            structured: result.structured,
            content: result.content,
        })
    }

    /// Each connected server's trimmed, non-empty `instructions`, sorted by server.
    pub fn instructions(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .inner
            .entries
            .lock()
            .iter()
            .filter_map(|(name, e)| e.connection().and_then(|c| c.instructions()).map(|i| (name.clone(), i)))
            .collect();
        out.sort_by(|a, b| locale_cmp(&a.0, &b.0));
        out
    }

    /// All prompts sorted by server, then name.
    pub fn prompts(&self) -> Vec<Prompt> {
        let mut prompts: Vec<Prompt> =
            self.inner.entries.lock().values().flat_map(|e| e.state.lock().prompts.clone().unwrap_or_default()).collect();
        prompts.sort_by(|a, b| locale_cmp(&a.server, &b.server).then_with(|| locale_cmp(&a.name, &b.name)));
        prompts
    }

    /// `prompts/get`; `Ok(None)` without a client or on any error.
    pub async fn prompt(
        &self,
        server: &str,
        name: &str,
        args: Option<IndexMap<String, String>>,
    ) -> Result<Option<PromptResult>, ManagerError> {
        let entry = self.require(server)?;
        entry.await_startup().await;
        let Some(connection) = entry.connection() else { return Ok(None) };
        let result = self
            .recovering(server, &entry, connection, |c| {
                let args = args.clone();
                async move { c.prompt(name, args.as_ref()).await }
            })
            .await;
        let Ok(Value::Object(raw)) = result else { return Ok(None) };
        Ok(Some(PromptResult {
            server: server.to_string(),
            name: name.to_string(),
            description: raw.get("description").and_then(Value::as_str).map(String::from),
            messages: raw.get("messages").and_then(Value::as_array).cloned().unwrap_or_default(),
            raw,
        }))
    }

    async fn load_catalog(&self, name: &str, entry: &Arc<Entry>, connection: Arc<Connection>) -> Result<ResourceCatalog, McpError> {
        let (resources, templates) = self
            .recovering(name, entry, connection, |c| async move {
                // Some servers declare resources without implementing template listing.
                let (resources, templates) = tokio::join!(c.resources(), c.resource_templates());
                Ok((resources?, templates.unwrap_or_default()))
            })
            .await?;
        let s = |v: &serde_json::Map<String, Value>, k: &str| v.get(k).and_then(Value::as_str).map(String::from);
        Ok(ResourceCatalog {
            resources: resources
                .iter()
                .map(|r| Resource {
                    server: name.to_string(),
                    name: s(r, "name").unwrap_or_default(),
                    uri: s(r, "uri").unwrap_or_default(),
                    description: s(r, "description"),
                    mime_type: s(r, "mimeType"),
                })
                .collect(),
            templates: templates
                .iter()
                .map(|t| ResourceTemplate {
                    server: name.to_string(),
                    name: s(t, "name").unwrap_or_default(),
                    uri_template: s(t, "uriTemplate").unwrap_or_default(),
                    description: s(t, "description"),
                    mime_type: s(t, "mimeType"),
                })
                .collect(),
        })
    }

    /// Merged catalog of every connected server; per-server failures yield an empty catalog.
    pub async fn resource_catalog(&self) -> ResourceCatalog {
        let entries: Vec<(String, Arc<Entry>)> =
            self.inner.entries.lock().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let futures = entries.into_iter().map(|(name, entry)| async move {
            match entry.connection() {
                Some(c) => self.load_catalog(&name, &entry, c).await.unwrap_or_default(),
                None => ResourceCatalog::default(),
            }
        });
        merge_catalogs(futures::future::join_all(futures).await)
    }

    /// Catalog of one server (errors propagate).
    pub async fn resources(&self, server: &str) -> Result<ResourceCatalog, McpError> {
        let entry = self.require(server).map_err(|e| McpError::other(e.to_string()))?;
        entry.await_startup().await;
        let Some(connection) = entry.connection() else { return Ok(ResourceCatalog::default()) };
        Ok(merge_catalogs(vec![self.load_catalog(server, &entry, connection).await?]))
    }

    /// `resources/read`; `Ok(None)` when not connected or the server lacks resources.
    pub async fn read_resource(&self, server: &str, uri: &str) -> Result<Option<ResourceContent>, McpError> {
        let entry = self.require(server).map_err(|e| McpError::other(e.to_string()))?;
        entry.await_startup().await;
        let Some(connection) = entry.connection() else { return Ok(None) };
        let result = self.recovering(server, &entry, connection, |c| async move { c.read_resource(uri).await }).await?;
        let Some(result) = result else { return Ok(None) };
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(String::from);
        let contents = result
            .get("contents")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|part| {
                if part.get("text").is_some() {
                    ResourceContentPart::Text {
                        uri: s(part, "uri").unwrap_or_default(),
                        text: s(part, "text").unwrap_or_default(),
                        mime_type: s(part, "mimeType"),
                    }
                } else {
                    ResourceContentPart::Blob {
                        uri: s(part, "uri").unwrap_or_default(),
                        blob: s(part, "blob").unwrap_or_default(),
                        mime_type: s(part, "mimeType"),
                    }
                }
            })
            .collect();
        Ok(Some(ResourceContent { server: server.to_string(), uri: uri.to_string(), contents }))
    }

    // ---- OAuth -------------------------------------------------------------------------------

    /// Starts the interactive OAuth flow for a remote server (`authorize`).
    pub async fn auth_start(&self, server: &str) -> Result<AuthorizeHandle, ManagerError> {
        let entry = self.require(server)?;
        let (ServerConfig::Remote(remote), Some(integration_id)) = (&entry.config, &entry.integration_id) else {
            return Err(ManagerError::Other(format!("MCP server \"{server}\" is not an OAuth-capable remote server")));
        };
        oauth::authorize(AuthorizeOptions {
            name: server.to_string(),
            config: remote.clone(),
            integration_id: integration_id.clone(),
            method_id: integration_id.clone(),
            credentials: self.inner.options.credentials.clone(),
        })
        .await
        .map_err(|e| ManagerError::Other(e.to_string()))
    }

    /// Stores a credential produced by [`AuthorizeHandle::wait`]: the new row keeps the previous
    /// label (default: the server name); older rows are removed after it is stored. Then
    /// reconnects the server. Returns the new credential id.
    pub async fn auth_finish(&self, server: &str, credential: Value) -> Result<String, ManagerError> {
        let entry = self.require(server)?;
        let Some(integration_id) = entry.integration_id.clone() else {
            return Err(ManagerError::Other(format!("MCP server \"{server}\" is not an OAuth-capable remote server")));
        };
        let Some(store) = self.inner.options.credentials.clone() else {
            return Err(ManagerError::Other("No credential store configured".into()));
        };
        let previous = store.list(&integration_id).await.map_err(|e| ManagerError::Other(e.to_string()))?;
        let label = previous.last().map(|p| p.label.clone()).unwrap_or_else(|| server.to_string());
        let id = store.create(&integration_id, &label, credential).await.map_err(|e| ManagerError::Other(e.to_string()))?;
        for old in previous {
            store.remove(&old.id).await.map_err(|e| ManagerError::Other(e.to_string()))?;
        }
        self.credential_switched(&integration_id).await;
        Ok(id)
    }

    /// Removes every stored credential of the server's integration; returns how many were removed.
    pub async fn logout(&self, server: &str) -> Result<usize, ManagerError> {
        let entry = self.require(server)?;
        let (Some(integration_id), Some(store)) = (entry.integration_id.clone(), self.inner.options.credentials.clone())
        else {
            return Ok(0);
        };
        let rows = store.list(&integration_id).await.map_err(|e| ManagerError::Other(e.to_string()))?;
        for row in &rows {
            store.remove(&row.id).await.map_err(|e| ManagerError::Other(e.to_string()))?;
        }
        if !rows.is_empty() {
            self.credential_switched(&integration_id).await;
        }
        Ok(rows.len())
    }

    /// `Credential.Event.Switched` for an integration this registry owns: restart the server
    /// (unless disabled) so a login takes effect without a restart.
    pub async fn credential_switched(&self, integration_id: &str) {
        let name = self
            .inner
            .entries
            .lock()
            .iter()
            .find(|(_, e)| e.integration_id.as_deref() == Some(integration_id))
            .map(|(n, _)| n.clone());
        let Some(name) = name else { return };
        let _guard = self.inner.locks.lock(&name).await;
        let Some(entry) = self.entry(&name) else { return };
        if entry.integration_id.as_deref() != Some(integration_id) || entry.status() == Status::Disabled {
            return;
        }
        self.stop_server(&name, &entry).await;
        self.start_server(&name, &entry).await;
    }

    /// Stops every server (Location disposal).
    /// Stops every server. Servers stop in parallel, so one slow server's stop grace does not delay
    /// the rest past a service restart's kill deadline and leave them orphaned.
    pub async fn shutdown(&self) {
        let entries: Vec<(String, Arc<Entry>)> =
            self.inner.entries.lock().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        futures::future::join_all(entries.into_iter().map(|(name, entry)| async move {
            let _guard = self.inner.locks.lock(&name).await;
            self.stop_server(&name, &entry).await;
        }))
        .await;
    }

    /// The `{server}` payload helper for bus bridges.
    pub fn event_payload(event: &McpEvent) -> Value {
        json!({"server": event.server()})
    }
}

/// `mergeCatalogs`: resources by server, name, uri; templates by server, name, uriTemplate.
pub fn merge_catalogs(catalogs: Vec<ResourceCatalog>) -> ResourceCatalog {
    let mut resources: Vec<Resource> = catalogs.iter().flat_map(|c| c.resources.clone()).collect();
    let mut templates: Vec<ResourceTemplate> = catalogs.iter().flat_map(|c| c.templates.clone()).collect();
    resources.sort_by(|a, b| {
        locale_cmp(&a.server, &b.server).then_with(|| locale_cmp(&a.name, &b.name)).then_with(|| locale_cmp(&a.uri, &b.uri))
    });
    templates.sort_by(|a, b| {
        locale_cmp(&a.server, &b.server)
            .then_with(|| locale_cmp(&a.name, &b.name))
            .then_with(|| locale_cmp(&a.uri_template, &b.uri_template))
    });
    ResourceCatalog { resources, templates }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `reconcile` (core/src/mcp/index.ts): the first reconcile announces disabled servers, then forks each
    /// startup, whose `pending` announcement lands before the State notification returns.
    #[tokio::test]
    async fn startup_statuses_are_published_before_set_servers_returns() {
        let manager = McpManager::new(ManagerOptions {
            directory: std::env::temp_dir(),
            client_info: ClientInfo { name: "opencode".into(), version: "test".into() },
            credentials: None,
            elicitation: None,
        });
        let seen: Arc<Mutex<Vec<McpEvent>>> = Arc::default();
        let sink_seen = seen.clone();
        manager.on_event(Arc::new(move |event: &McpEvent| sink_seen.lock().push(event.clone())));
        let servers: IndexMap<String, ServerConfig> = serde_json::from_value(serde_json::json!({
            "a": {"type": "local", "command": ["/nonexistent/ocd-mcp-test"]},
            "off": {"type": "local", "command": ["/nonexistent/ocd-mcp-test"], "disabled": true},
            "b": {"type": "local", "command": ["/nonexistent/ocd-mcp-test"]},
        }))
        .unwrap();
        manager.set_servers(servers).await;
        let seen = seen.lock().clone();
        let status = |server: &str| McpEvent::StatusChanged { server: server.into() };
        assert_eq!(seen[..3], [status("off"), status("a"), status("b")], "{seen:?}");
        let statuses: Vec<(String, Status)> = manager.servers().into_iter().map(|s| (s.name, s.status)).collect();
        assert!(statuses.contains(&("off".into(), Status::Disabled)), "{statuses:?}");
    }

    #[test]
    fn unavailable_messages() {
        assert_eq!(
            unavailable_message("s", &Status::failed("boom")),
            "MCP server \"s\" is not connected: boom. Reconnect it from /mcps."
        );
        assert_eq!(
            unavailable_message("s", &Status::needs_auth("x")),
            "MCP server \"s\" needs authentication: x. Sign in from /mcps."
        );
        assert_eq!(unavailable_message("s", &Status::Disabled), "MCP server \"s\" is disabled.");
        assert_eq!(unavailable_message("s", &Status::Pending), "MCP server \"s\" is not connected.");
        assert_eq!(ManagerError::NotFound("x".into()).to_string(), "MCP server not found: x");
    }

    #[test]
    fn locale_ordering() {
        let mut names = vec!["b", "A", "a", "C"];
        names.sort_by(|a, b| locale_cmp(a, b));
        assert_eq!(names, vec!["a", "A", "b", "C"]);
    }
}

/// `client.getServerVersion()?.title?.trim() || undefined`: the display title a server reports in `serverInfo`.
pub fn server_title(server_info: Option<Value>) -> Option<String> {
    let title = server_info?.get("title")?.as_str()?.trim().to_string();
    (!title.is_empty()).then_some(title)
}

#[cfg(test)]
mod title_tests {
    use super::server_title;
    use serde_json::json;

    #[test]
    fn titles_are_trimmed_and_empty_ones_dropped() {
        assert_eq!(server_title(Some(json!({"name": "s", "version": "1", "title": "  Docs Server "}))), Some("Docs Server".into()));
        assert_eq!(server_title(Some(json!({"name": "s", "title": "   "}))), None);
        assert_eq!(server_title(Some(json!({"name": "s"}))), None);
        assert_eq!(server_title(None), None);
    }
}
