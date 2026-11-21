//! `McpManager`: the per-Location server registry and lifecycle
//! (`packages/core/src/mcp/index.ts`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use futures::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};

use crate::config::{ServerConfig, apply_codemode_defaults};
use crate::connection::{ConnectError, Connection};
use crate::error::McpError;
use crate::link::{AuthProvider, Kind, Link, LinkEvents, LinkInput, Listed, Runner};
use crate::oauth::fetch::Fetcher;
use crate::oauth::{self, AuthorizeHandle, AuthorizeOptions, McpCredentialStore};
use crate::pool::{self, McpPool, PoolInput, Pooled, Selection};
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

/// Why `subscribe` cannot follow a server's events (`MCP.EventsUnavailableError`, `MCP.NotFoundError`).
/// Experimental: MCP Events is a draft extension.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubscribeError {
    #[error("MCP server not found: {0}")]
    NotFound(String),
    #[error("{message}")]
    EventsUnavailable { server: String, message: String },
}

/// `Mcp.SubscriptionStatus`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionStatus {
    Pending,
    Active,
    /// `reason`: `unavailable`, `rejected` or `terminated`; `error` `{code, message}` for the last two.
    Ended { reason: String, error: Option<Value> },
}

impl SubscriptionStatus {
    fn kind(&self) -> &'static str {
        match self {
            SubscriptionStatus::Pending => "pending",
            SubscriptionStatus::Active => "active",
            SubscriptionStatus::Ended { .. } => "ended",
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            SubscriptionStatus::Ended { reason, error } => {
                let mut out = json!({ "status": "ended", "reason": reason });
                if let Some(error) = error {
                    out["error"] = error.clone();
                }
                out
            }
            other => json!({ "status": other.kind() }),
        }
    }
}

/// An event type of a server, followed onto the bus as `experimental.mcp.event` (`Mcp.Subscription`).
/// Experimental: MCP Events is a draft extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    /// The same for every subscribe to this server, event name and arguments, within this Location.
    pub id: String,
    pub server: String,
    pub name: String,
    pub arguments: Option<Value>,
    pub status: SubscriptionStatus,
    /// Where it resumes from: the last cursor the server sent.
    pub cursor: Option<String>,
}

/// A subscription and whether the server skipped events since it was last delivered one.
type Followed = Arc<Mutex<(Subscription, bool)>>;

/// `canonicalJson`: object keys sorted by UTF-16 code units, everything else as `JSON.stringify`.
fn canonical_json(value: &Value) -> String {
    match value {
        Value::Array(items) => format!("[{}]", items.iter().map(canonical_json).collect::<Vec<_>>().join(",")),
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            let fields: Vec<String> =
                keys.into_iter().map(|k| format!("{}:{}", Value::String(k.clone()), canonical_json(&map[k]))).collect();
            format!("{{{}}}", fields.join(","))
        }
        other => other.to_string(),
    }
}

/// A broken, silent or closed event stream reopens after a delay growing from a second to this cap (`EVENT_REOPEN`).
const EVENT_REOPEN_CAP: std::time::Duration = std::time::Duration::from_secs(30);

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

#[derive(Default)]
pub struct ManagerOptions {
    /// The Location directory (stdio cwd base, `roots/list`).
    pub directory: PathBuf,
    /// `{name:"opencode", version: app version}`.
    pub client_info: ClientInfo,
    pub credentials: Option<Arc<dyn McpCredentialStore>>,
    pub elicitation: Option<Arc<dyn ElicitationHandler>>,
    /// The process's connection pool (`McpPool.Service`): servers with `pool: true` share one connection with every
    /// other Location that configures them identically. Without one, every server connects on its own.
    pub pool: Option<McpPool>,
    /// Experimental `mcp_events` flag of the Location: connections declare MCP Events, and [`McpManager::events`]
    /// and subscriptions read the events of servers that offer them.
    pub mcp_events: bool,
}

#[derive(Default)]
struct KeyedMutex {
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl KeyedMutex {
    fn get(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.lock().entry(key.to_string()).or_default().clone()
    }

    async fn lock(&self, key: &str) -> tokio::sync::OwnedMutexGuard<()> {
        self.get(key).lock_owned().await
    }
}

/// How long a model call may wait for a server that is still connecting, counted from when it began
/// (`STARTUP_GRACE_MS`).
pub const STARTUP_GRACE: std::time::Duration = std::time::Duration::from_millis(2_000);

/// The connection attempt in flight: when it began and a latch that opens once it settles.
struct Attempt {
    started: tokio::time::Instant,
    settled: watch::Sender<bool>,
}

impl Attempt {
    fn new(started: tokio::time::Instant) -> Self {
        Self { started, settled: watch::channel(false).0 }
    }
}

#[derive(Default)]
struct EntryState {
    status: Option<Status>,
    attempt: Option<Attempt>,
}

/// A server's connection: its own, or a pooled one (`McpConnection.Connection`).
enum Conn {
    Direct(Arc<Link>),
    Pooled(Arc<Pooled>),
}

impl Conn {
    fn client(&self) -> Option<Arc<Connection>> {
        match self {
            Conn::Direct(link) => link.client(),
            Conn::Pooled(pooled) => pooled.client(),
        }
    }

    fn tools(&self) -> Option<Listed> {
        match self {
            Conn::Direct(link) => link.tools(),
            Conn::Pooled(pooled) => pooled.tools(),
        }
    }

    fn prompts(&self) -> Option<Listed> {
        match self {
            Conn::Direct(link) => link.prompts(),
            Conn::Pooled(pooled) => pooled.prompts(),
        }
    }

    fn start(&self) -> BoxFuture<'_, ()> {
        match self {
            Conn::Direct(link) => link.start(),
            Conn::Pooled(pooled) => pooled.start(),
        }
    }

    fn stop(&self) -> BoxFuture<'_, ()> {
        match self {
            Conn::Direct(link) => link.stop(),
            Conn::Pooled(pooled) => pooled.stop(),
        }
    }

    /// Runs a request and, on session expiry, reconnects and retries once on the new client. A request through a
    /// shared connection brings this Location's elicitation handler along.
    async fn recovering<T, F, Fut>(&self, client: Arc<Connection>, run: F) -> Result<T, McpError>
    where
        F: Fn(Arc<Connection>, Option<Arc<dyn ElicitationHandler>>) -> Fut,
        Fut: std::future::Future<Output = Result<T, McpError>>,
    {
        match self {
            Conn::Direct(link) => link.recovering(client, |c| run(c, None)).await,
            Conn::Pooled(pooled) => pooled.recovering(client, run).await,
        }
    }
}

struct Entry {
    config: ServerConfig,
    integration_id: Option<String>,
    state: Mutex<EntryState>,
    startup: watch::Sender<bool>,
    conn: OnceLock<Conn>,
    /// The initial reconcile already announced this start's `pending` status, before `set_servers` returned.
    announced: AtomicBool,
}

impl Entry {
    fn new(name: &str, config: ServerConfig, started: tokio::time::Instant) -> Arc<Self> {
        let integration_id = match &config {
            ServerConfig::Remote(remote) if !remote.oauth_disabled() => Some(oauth::integration_id(name, &remote.url)),
            _ => None,
        };
        Arc::new(Self {
            config,
            integration_id,
            state: Mutex::new(EntryState { status: Some(Status::Pending), attempt: Some(Attempt::new(started)) }),
            startup: watch::channel(false).0,
            conn: OnceLock::new(),
            announced: AtomicBool::new(false),
        })
    }

    fn conn(&self) -> &Conn {
        self.conn.get().expect("entry connection")
    }

    /// `connecting`: a server still pending from creation keeps the window that started with its entry.
    fn connecting(&self) {
        let mut state = self.state.lock();
        if state.attempt.is_none() || state.status != Some(Status::Pending) {
            state.attempt = Some(Attempt::new(tokio::time::Instant::now()));
        }
        state.status = Some(Status::Pending);
    }

    /// `settled`: the attempt ended, however it ended.
    fn settled(&self) {
        if let Some(attempt) = self.state.lock().attempt.take() {
            attempt.settled.send_replace(true);
        }
        self.open_startup();
    }

    fn status(&self) -> Status {
        self.state.lock().status.clone().unwrap_or(Status::Pending)
    }

    fn set_status(&self, status: Status) {
        self.state.lock().status = Some(status);
    }

    fn connection(&self) -> Option<Arc<Connection>> {
        self.conn.get().and_then(Conn::client)
    }

    fn open_startup(&self) {
        self.startup.send_replace(true);
    }

    async fn await_startup(&self) {
        let mut rx = self.startup.subscribe();
        let _ = rx.wait_for(|open| *open).await;
    }

    /// The connection's tool list tagged for this server.
    fn tools(&self, name: &str) -> Vec<Tool> {
        let codemode = self.config.codemode();
        let Some(listed) = self.conn.get().and_then(Conn::tools) else { return Vec::new() };
        listed.iter().filter_map(|t| Tool::from_raw(name, codemode, t.clone())).collect()
    }

    fn prompts(&self, name: &str) -> Vec<Prompt> {
        let Some(listed) = self.conn.get().and_then(Conn::prompts) else { return Vec::new() };
        listed.iter().filter_map(|p| Prompt::from_raw(name, p.clone())).collect()
    }
}

/// What a server's connection reports to the registry (the `events` of `makeEntry`).
struct EntryEvents {
    manager: Weak<Inner>,
    name: String,
    entry: Weak<Entry>,
}

impl EntryEvents {
    fn parts(&self) -> Option<(McpManager, Arc<Entry>)> {
        Some((McpManager { inner: self.manager.upgrade()? }, self.entry.upgrade()?))
    }
}

#[async_trait]
impl LinkEvents for EntryEvents {
    async fn connecting(&self) {
        let Some((manager, entry)) = self.parts() else { return };
        entry.connecting();
        // Announce the handshake so connect() and credential reconnects don't show a stale disabled/failed status for
        // the duration of the connection attempt. The initial reconcile announced the first one already.
        if entry.announced.swap(false, Ordering::SeqCst) {
            return;
        }
        manager.publish(McpEvent::StatusChanged { server: self.name.clone() });
    }

    async fn connected(&self) {
        let Some((manager, entry)) = self.parts() else { return };
        entry.set_status(Status::Connected);
        let tools = entry.conn.get().and_then(Conn::tools).map(|t| t.len()).unwrap_or_default();
        tracing::info!(server = %self.name, tools, "mcp connected");
        // Announce every list a consumer may read, since each reads them only on these events. A connection another
        // Location already holds has loaded its prompts, and no later change would tell this one; prompts still
        // loading read empty now and announce themselves later.
        let server = || self.name.clone();
        manager.publish(McpEvent::ToolsChanged { server: server() });
        manager.publish(McpEvent::ResourcesChanged { server: server() });
        manager.publish(McpEvent::PromptsChanged { server: server() });
        manager.publish(McpEvent::StatusChanged { server: server() });
    }

    async fn failed(&self, failure: Status, _answered: bool) {
        let Some((manager, entry)) = self.parts() else { return };
        tracing::warn!(server = %self.name, status = ?failure, "mcp connect failed");
        entry.set_status(failure);
        manager.publish(McpEvent::StatusChanged { server: self.name.clone() });
    }

    async fn settled(&self) {
        if let Some(entry) = self.entry.upgrade() {
            entry.settled();
        }
    }

    async fn closed(&self, reason: String) {
        let Some((manager, entry)) = self.parts() else { return };
        entry.set_status(Status::Failed { error: reason });
        entry.conn().stop().await;
        manager.publish(McpEvent::StatusChanged { server: self.name.clone() });
    }

    async fn changed(&self, kind: Kind) {
        let Some(manager) = self.manager.upgrade().map(|inner| McpManager { inner }) else { return };
        let server = self.name.clone();
        manager.publish(match kind {
            Kind::Tools => McpEvent::ToolsChanged { server },
            Kind::Resources => McpEvent::ResourcesChanged { server },
            Kind::Prompts => McpEvent::PromptsChanged { server },
        });
    }
}

/// Connection callbacks run here (`callbacks`): once the Location is closing, in-flight callbacks are dropped and
/// later ones never start, so connections closing with the Location publish nothing. Clients re-fetch on MCP events,
/// which would boot the Location straight back up.
struct Callbacks(Weak<Inner>);

impl Runner for Callbacks {
    fn run(&self, work: BoxFuture<'static, ()>) {
        let closing = |inner: &Weak<Inner>| inner.upgrade().is_none_or(|inner| inner.closing.load(Ordering::SeqCst));
        if closing(&self.0) {
            return;
        }
        let inner = self.0.clone();
        tokio::spawn(async move {
            if !closing(&inner) {
                work.await;
            }
        });
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
    /// Set first by [`McpManager::shutdown`]: connections closing with the Location publish nothing and
    /// their callbacks never run, because clients re-fetch on MCP events, which would boot the Location
    /// straight back up (`mcp/index.ts` `callbacks`).
    closing: std::sync::atomic::AtomicBool,
    /// The tools the session tool registry lists ([`McpManager::registered_tools`]), once read.
    registered: Mutex<Option<Vec<Tool>>>,
    /// Bumped by every `ToolsChanged`; the registry reads the tools again once these settle.
    tools_changed: tokio::sync::watch::Sender<u64>,
    /// Experimental MCP Events subscriptions, by server, then id. Each streams while its server is connected.
    followed: Mutex<IndexMap<String, IndexMap<String, Followed>>>,
    /// The running stream of each subscription, by id.
    streams: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// Serializes `follows`, which runs on every status change.
    follows_lock: tokio::sync::Mutex<()>,
    /// Work tied to this Location's MCP service (see [`McpManager::spawn_scoped`]).
    scoped: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Client-hosted MCP servers attached over a connection the client opened, by name (see [`McpManager::attach`]).
    attached: Mutex<IndexMap<String, Vec<Arc<AttachedEntry>>>>,
    /// Attached connections lock separately so a configured server of the same name never contends.
    attached_locks: KeyedMutex,
    next_attached: std::sync::atomic::AtomicU64,
}

/// A client-hosted MCP server attached over a connection the client opened. Internal: never listed, never reconciled
/// against config, and removed entirely once its [`Attachment`] or connection closes. Connections attached under one
/// name stack; the most recent live one serves the name's tools.
struct AttachedEntry {
    id: u64,
    connection: Arc<Connection>,
    tools: Mutex<Vec<Tool>>,
}

/// Bounds re-listing the tools of an attached connection that takes over the top of its stack
/// (`FALLBACK_LIST_TIMEOUT`).
pub const FALLBACK_LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A client-hosted MCP server attached with [`McpManager::attach`]. It serves its name's tools until it is closed or
/// dropped, or its connection closes; then the most recent remaining connection under the name takes over.
pub struct Attachment {
    manager: Weak<Inner>,
    name: String,
    id: u64,
    done: bool,
}

impl Attachment {
    /// Detaches the connection and closes it, waiting for the fallback's tool list to settle.
    pub async fn close(mut self) {
        self.done = true;
        if let Some(inner) = self.manager.upgrade() {
            McpManager { inner }.detach_locked(&self.name, self.id).await;
        }
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let (Some(inner), Ok(runtime)) = (self.manager.upgrade(), tokio::runtime::Handle::try_current()) else { return };
        let (name, id) = (std::mem::take(&mut self.name), self.id);
        runtime.spawn(async move { McpManager { inner }.detach_locked(&name, id).await });
    }
}

/// How long `ToolsChanged` bursts settle before the session tool registry reads the tools again (`tool/mcp.ts`
/// `Stream.debounce("100 millis")`).
pub const TOOLS_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(100);

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
        let (tools_changed, mut changes) = tokio::sync::watch::channel(0u64);
        let inner = Arc::new(Inner {
            options,
            entries: Mutex::new(IndexMap::new()),
            locks: KeyedMutex::default(),
            events,
            base: Mutex::new(IndexMap::new()),
            overrides: Mutex::new(IndexMap::new()),
            applied: Mutex::new(None),
            reconcile_lock: tokio::sync::Mutex::new(()),
            sink: Mutex::new(None),
            closing: Default::default(),
            registered: Mutex::new(None),
            tools_changed,
            followed: Mutex::new(IndexMap::new()),
            streams: Mutex::new(HashMap::new()),
            follows_lock: tokio::sync::Mutex::new(()),
            scoped: Mutex::new(Vec::new()),
            attached: Mutex::new(IndexMap::new()),
            attached_locks: KeyedMutex::default(),
            next_attached: Default::default(),
        });
        // `tool/mcp.ts`: after the initial registration, `ToolsChanged` bursts settle for 100 ms before the
        // registry reads the tools again.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let weak = Arc::downgrade(&inner);
            runtime.spawn(async move {
                while changes.changed().await.is_ok() {
                    loop {
                        match tokio::time::timeout(TOOLS_DEBOUNCE, changes.changed()).await {
                            Ok(Ok(())) => continue,
                            Ok(Err(_)) => return,
                            Err(_) => break,
                        }
                    }
                    let Some(inner) = weak.upgrade() else { return };
                    let manager = McpManager { inner };
                    let tools = manager.tools();
                    *manager.inner.registered.lock() = Some(tools);
                }
            });
        }
        Self { inner }
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

    fn closing(&self) -> bool {
        self.inner.closing.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// True once [`McpManager::shutdown`] began: the Location is going away.
    pub fn is_closing(&self) -> bool {
        self.closing()
    }

    fn publish(&self, event: McpEvent) {
        if self.closing() {
            return;
        }
        if matches!(event, McpEvent::ToolsChanged { .. }) {
            self.inner.tools_changed.send_modify(|n| *n += 1);
        }
        // Subscriptions start or stop with their server's status (a bus subscriber upstream).
        let follow = match &event {
            McpEvent::StatusChanged { server } if self.inner.options.mcp_events => Some(server.clone()),
            _ => None,
        };
        let sink = self.inner.sink.lock().clone();
        if let Some(sink) = sink {
            sink(&event);
        }
        let _ = self.inner.events.send(event);
        if let Some(server) = follow {
            let this = self.clone();
            tokio::spawn(async move { this.follows(&server).await });
        }
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
            let started = tokio::time::Instant::now();
            let entries: Vec<(String, Arc<Entry>)> =
                servers.iter().map(|(name, config)| (name.clone(), self.make_entry(name, config.clone(), started))).collect();
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
                entry.announced.store(true, Ordering::SeqCst);
                self.publish(McpEvent::StatusChanged { server: name.clone() });
            }
            for (name, entry) in starting {
                let this = self.clone();
                tokio::spawn(async move {
                    let _guard = this.inner.locks.lock(&name).await;
                    // connect() or disconnect() may have settled the server while this startup waited to run.
                    if entry.status() != Status::Pending {
                        entry.announced.store(false, Ordering::SeqCst);
                        entry.open_startup();
                        return;
                    }
                    entry.conn().start().await;
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
            previous.conn().stop().await;
        }
        let entry = self.make_entry(name, config, tokio::time::Instant::now());
        self.inner.entries.lock().insert(name.to_string(), entry.clone());
        if entry.config.disabled() {
            entry.set_status(Status::Disabled);
            self.publish(McpEvent::StatusChanged { server: name.to_string() });
        } else {
            entry.conn().start().await;
        }
        entry.open_startup();
    }

    async fn remove_server(&self, name: &str) {
        let Some(entry) = self.entry(name) else { return };
        entry.conn().stop().await;
        // Credentials are keyed by name + URL and intentionally survive removal.
        self.inner.entries.lock().shift_remove(name);
        self.publish(McpEvent::StatusChanged { server: name.to_string() });
    }

    /// `makeEntry`: the server's entry with its connection, pooled when its config asks for that.
    fn make_entry(&self, name: &str, config: ServerConfig, started: tokio::time::Instant) -> Arc<Entry> {
        let entry = Entry::new(name, config, started);
        let options = &self.inner.options;
        let weak_entry = Arc::downgrade(&entry);
        let credentials = options.credentials.clone();
        // The credential one start connects with: the integration's latest, or none. Read it once per start and pass
        // the result along, so anything keyed on it and the provider it builds agree on the account.
        let select_credential = {
            let (weak, credentials) = (weak_entry.clone(), credentials.clone());
            move || {
                let (weak, credentials) = (weak.clone(), credentials.clone());
                async move {
                    let Some(entry) = weak.upgrade() else { return Ok(None) };
                    let (ServerConfig::Remote(_), Some(integration_id), Some(store)) = (&entry.config, &entry.integration_id, &credentials)
                    else {
                        return Ok(None);
                    };
                    store.list(integration_id).await.map(|mut rows| rows.pop()).map_err(|e| e.to_string())
                }
            }
        };
        let provider = {
            let (weak, credentials, server) = (weak_entry.clone(), credentials.clone(), name.to_string());
            move |credential: Option<crate::oauth::StoredCredential>| -> Option<Arc<crate::oauth::ConnectAuth>> {
                let entry = weak.upgrade()?;
                let (ServerConfig::Remote(remote), Some(_)) = (&entry.config, &entry.integration_id) else { return None };
                Some(Arc::new(oauth::connect_provider(
                    remote,
                    credential,
                    credentials.as_ref(),
                    Fetcher::new(server.clone()).with_base_headers(remote.headers.clone()),
                )))
            }
        };
        let auth_provider: AuthProvider = {
            let (select_credential, provider) = (select_credential.clone(), provider.clone());
            Arc::new(move || {
                let (select_credential, provider) = (select_credential.clone(), provider.clone());
                Box::pin(async move { Ok(provider(select_credential().await?)) })
            })
        };
        let link = LinkInput {
            name: name.to_string(),
            // Pooling offers 2026-07-28, the only revision a connection can be shared on.
            config: pool::effective_config(&entry.config),
            directory: options.directory.clone(),
            shared: false,
            client_info: options.client_info.clone(),
            mcp_events: options.mcp_events,
            auth_provider,
            elicitation: options.elicitation.clone(),
            lock: self.inner.locks.get(name),
            callbacks: Arc::new(Callbacks(Arc::downgrade(&self.inner))),
            fork: Arc::new(Callbacks(Arc::downgrade(&self.inner))),
            events: Arc::new(EntryEvents { manager: Arc::downgrade(&self.inner), name: name.to_string(), entry: weak_entry }),
        };
        let conn = match options.pool.clone().filter(|_| pool::pooled(&entry.config)) {
            None => Conn::Direct(Link::new(link)),
            Some(pool) => {
                let config = link.config.clone();
                let (directory, client_info, mcp_events) = (options.directory.clone(), options.client_info.clone(), options.mcp_events);
                let select: pool::Select = Arc::new(move || {
                    let (config, directory, client_info) = (config.clone(), directory.clone(), client_info.clone());
                    let (select_credential, provider) = (select_credential.clone(), provider.clone());
                    Box::pin(async move {
                        if let ServerConfig::Local(local) = &config {
                            // Resolved exactly as the spawn resolves it, `~` included, so the key matches where the
                            // server runs.
                            let cwd = crate::connection::resolve_cwd(&directory, local.cwd.as_deref());
                            let key = pool::key(&pool::KeyInput {
                                config: &config,
                                cwd: Some(&cwd),
                                credential: None,
                                client_info: Some(&client_info),
                                mcp_events,
                            });
                            return Selection { key, auth_provider: crate::link::no_auth() };
                        }
                        let credential = select_credential().await;
                        let id = credential.as_ref().ok().and_then(|c| c.as_ref().map(|c| c.id.clone()));
                        let key = pool::key(&pool::KeyInput {
                            config: &config,
                            cwd: None,
                            credential: id.as_deref(),
                            client_info: Some(&client_info),
                            mcp_events,
                        });
                        let auth_provider: AuthProvider = Arc::new(move || {
                            let result = credential.clone().map(|c| provider(c));
                            Box::pin(async move { result })
                        });
                        Selection { key, auth_provider }
                    })
                });
                Conn::Pooled(pool.connection(PoolInput { link, select }))
            }
        };
        let _ = entry.conn.set(conn);
        entry
    }

    // ---- public interface -------------------------------------------------------------------    // ---- public interface -------------------------------------------------------------------

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

    /// `awaitStartup`: waits for servers that are still connecting, each until it settles or until its startup grace
    /// window ([`STARTUP_GRACE`] from when its attempt began) ends. Servers past their window never wait.
    pub async fn await_startup(&self) {
        let now = tokio::time::Instant::now();
        let waits: Vec<_> = self
            .inner
            .entries
            .lock()
            .values()
            .filter_map(|entry| {
                let state = entry.state.lock();
                if state.status != Some(Status::Pending) {
                    return None;
                }
                let attempt = state.attempt.as_ref()?;
                let deadline = attempt.started + STARTUP_GRACE;
                (deadline > now).then(|| (attempt.settled.subscribe(), deadline))
            })
            .collect();
        futures::future::join_all(waits.into_iter().map(|(mut settled, deadline)| async move {
            let _ = tokio::time::timeout_at(deadline, settled.wait_for(|done| *done)).await;
        }))
        .await;
    }

    /// `mcpTools.flush` (`tool/mcp.ts`): gives servers still connecting a bounded chance to finish
    /// ([`McpManager::await_startup`]), then brings the registered tools up to date without waiting for the debounced
    /// reload. Slow servers miss this step's tools and join once they publish a change.
    pub async fn flush(&self) {
        self.await_startup().await;
        let current = self.tools();
        let mut registered = self.inner.registered.lock();
        if registered.as_ref() != Some(&current) {
            *registered = Some(current);
        }
    }

    /// Runtime add/replace (not persisted); connects immediately.
    pub async fn add(&self, server: &str, config: ServerConfig) {
        self.inner.overrides.lock().insert(server.to_string(), Some(config));
        self.reconcile().await;
    }

    /// Runs a requested lifecycle operation to the end even if its caller goes away, such as a client dropping the
    /// HTTP request (`transition`). Stopped halfway, a server stays `pending` after a cancelled connect, or
    /// `connected` with no connection after a cancelled disconnect.
    async fn transition<F, Fut>(&self, run: F) -> Result<(), ManagerError>
    where
        F: FnOnce(McpManager) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), ManagerError>> + Send + 'static,
    {
        tokio::spawn(run(self.clone())).await.unwrap_or_else(|error| Err(ManagerError::Other(error.to_string())))
    }

    /// Stop + start; overrides `disabled` until restart.
    pub async fn connect(&self, server: &str) -> Result<(), ManagerError> {
        let server = server.to_string();
        self.transition(move |this| async move {
            let _guard = this.inner.locks.lock(&server).await;
            let entry = this.require(&server)?;
            entry.conn().stop().await;
            entry.conn().start().await;
            Ok(())
        })
        .await
    }

    /// Stop and mark `disabled` until reconnect.
    pub async fn disconnect(&self, server: &str) -> Result<(), ManagerError> {
        let server = server.to_string();
        self.transition(move |this| async move {
            let _guard = this.inner.locks.lock(&server).await;
            let entry = this.require(&server)?;
            entry.conn().stop().await;
            entry.set_status(Status::Disabled);
            this.publish(McpEvent::StatusChanged { server });
            Ok(())
        })
        .await
    }

    /// Removes the server until restart (credentials survive).
    pub async fn remove(&self, server: &str) -> Result<(), ManagerError> {
        self.require(server)?;
        self.inner.overrides.lock().insert(server.to_string(), None);
        self.reconcile().await;
        Ok(())
    }

    /// The tools the session tool registry lists (`tool/mcp.ts`): read once when first needed (after the servers'
    /// first connection attempts settled), then again only once `ToolsChanged` events have settled for
    /// [`TOOLS_DEBOUNCE`]. A step that starts right after a server went away still lists its tools.
    pub fn registered_tools(&self) -> Vec<Tool> {
        self.inner.registered.lock().get_or_insert_with(|| self.tools()).clone()
    }

    /// All connected tools sorted by server, then name. Attached clients contribute the tools of the top connection of
    /// each name; like [`McpManager::call_tool`], an attached client's tool shadows a configured server's tool of the
    /// same name.
    pub fn tools(&self) -> Vec<Tool> {
        let clients: Vec<Tool> = self
            .inner
            .attached
            .lock()
            .values()
            .filter_map(|stack| stack.last().map(|top| top.tools.lock().clone()))
            .flatten()
            .collect();
        let shadowed: std::collections::HashSet<(&str, &str)> =
            clients.iter().map(|t| (t.server.as_str(), t.name.as_str())).collect();
        let mut tools: Vec<Tool> = self
            .inner
            .entries
            .lock()
            .iter()
            .flat_map(|(name, e)| e.tools(name))
            .filter(|t| !shadowed.contains(&(t.server.as_str(), t.name.as_str())))
            .collect();
        tools.extend(clients.iter().cloned());
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
        // An attached client owns the tools it lists, even when a configured server shares its name, and owns its
        // name outright when no configured server has it.
        let client = self.inner.attached.lock().get(server).and_then(|stack| stack.last().cloned());
        if let Some(client) = client
            && (client.tools.lock().iter().any(|t| t.name == name) || self.entry(server).is_none())
        {
            let result = client.connection.call_tool(name, args, session_id).await.map_err(|error| ManagerError::ToolCall {
                server: server.to_string(),
                tool: name.to_string(),
                message: format!("MCP tool \"{name}\" on client \"{server}\" failed: {error}"),
            })?;
            return Ok(ToolResult {
                server: server.to_string(),
                tool: name.to_string(),
                is_error: result.is_error,
                structured: result.structured,
                content: result.content,
            });
        }
        let entry = self.require(server)?;
        entry.await_startup().await;
        let Some(connection) = entry.connection() else {
            return Err(ManagerError::ToolCall {
                server: server.to_string(),
                tool: name.to_string(),
                message: unavailable_message(server, &entry.status()),
            });
        };
        let result = entry
            .conn()
            .recovering(connection, |c, elicitation| {
                let args = args.clone();
                async move { c.call_tool_for(name, args, session_id, elicitation).await }
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
            self.inner.entries.lock().iter().flat_map(|(name, e)| e.prompts(name)).collect();
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
        let result = entry
            .conn()
            .recovering(connection, |c, elicitation| {
                let args = args.clone();
                async move { c.prompt_for(name, args.as_ref(), elicitation).await }
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
        let (resources, templates) = entry
            .conn()
            .recovering(connection, |c, _| async move {
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
        let result = entry
            .conn()
            .recovering(connection, |c, elicitation| async move { c.read_resource_for(uri, elicitation).await })
            .await?;
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

    // ---- MCP Events (experimental) ------------------------------------------------------------

    /// Runs `work` for as long as this Location's MCP service lives: [`McpManager::shutdown`] stops it, as closing
    /// the Location's plugin scope interrupts what a tool forked into it upstream (the MCP event watches).
    pub fn spawn_scoped(&self, work: impl std::future::Future<Output = ()> + Send + 'static) {
        if self.closing() {
            return;
        }
        let mut scoped = self.inner.scoped.lock();
        scoped.retain(|task| !task.is_finished());
        scoped.push(tokio::spawn(work));
    }

    /// Experimental: MCP Events of a connected server that offers them, while the `mcp_events` flag is on.
    pub async fn events(&self, server: &str) -> Result<Option<crate::events::Events>, ManagerError> {
        let entry = self.require(server)?;
        entry.await_startup().await;
        if !self.inner.options.mcp_events {
            return Ok(None);
        }
        Ok(entry.connection().as_ref().and_then(crate::events::Events::of))
    }

    /// Experimental: follows an event type of a server, publishing each event as `experimental.mcp.event` while the
    /// server is connected, and resuming from the last cursor after a reconnect or a broken stream. Its status changes
    /// publish `experimental.mcp.subscription.changed`. Subscribing again to the same server, name and arguments
    /// returns the existing subscription. An ended one stays until unsubscribed. `cursor`: where to start; `None`
    /// starts from now.
    pub async fn subscribe_events(
        &self,
        server: &str,
        name: &str,
        arguments: Option<Value>,
        cursor: Option<String>,
    ) -> Result<Subscription, SubscribeError> {
        if !self.inner.options.mcp_events {
            return Err(SubscribeError::EventsUnavailable {
                server: server.to_string(),
                message: "MCP events need the mcp_events flag.".into(),
            });
        }
        if self.entry(server).is_none() {
            return Err(SubscribeError::NotFound(server.to_string()));
        }
        let key = json!([server, name, arguments.clone().unwrap_or_else(|| json!({}))]);
        let digest = <sha2::Sha256 as sha2::Digest>::digest(canonical_json(&key).as_bytes());
        let id = format!("mcpsub_{}", &hex::encode(digest)[..24]);
        let view = {
            let mut followed = self.inner.followed.lock();
            let subs = followed.entry(server.to_string()).or_default();
            let existing = subs.get(&id).map(|sub| sub.lock().0.clone());
            if let Some(existing) = &existing
                && !matches!(existing.status, SubscriptionStatus::Ended { .. })
            {
                return Ok(existing.clone());
            }
            let view = Subscription {
                id: id.clone(),
                server: server.to_string(),
                name: name.to_string(),
                arguments,
                status: SubscriptionStatus::Pending,
                cursor: cursor.or_else(|| existing.and_then(|e| e.cursor)),
            };
            subs.insert(id.clone(), Arc::new(Mutex::new((view.clone(), false))));
            view
        };
        self.publish(McpEvent::ExperimentalSubscriptionChanged { id });
        self.follows(server).await;
        Ok(self.event_subscriptions().into_iter().find(|s| s.id == view.id).unwrap_or(view))
    }

    /// Experimental: stops following a subscription.
    pub fn unsubscribe_events(&self, id: &str) {
        let removed = self.inner.followed.lock().values_mut().any(|subs| subs.shift_remove(id).is_some());
        if !removed {
            return;
        }
        if let Some(stream) = self.inner.streams.lock().remove(id) {
            stream.abort();
        }
        self.publish(McpEvent::ExperimentalSubscriptionChanged { id: id.to_string() });
    }

    /// Experimental: every subscription, with its status.
    pub fn event_subscriptions(&self) -> Vec<Subscription> {
        self.inner.followed.lock().values().flat_map(|subs| subs.values().map(|sub| sub.lock().0.clone()).collect::<Vec<_>>()).collect()
    }

    fn set_status(&self, sub: &Followed, status: SubscriptionStatus) {
        let id = {
            let mut sub = sub.lock();
            if sub.0.status.kind() == status.kind() {
                return;
            }
            sub.0.status = status;
            sub.0.id.clone()
        };
        self.publish(McpEvent::ExperimentalSubscriptionChanged { id });
    }

    fn receive(&self, sub: &Followed, message: crate::events::EventMessage) {
        use crate::events::EventMessage;
        match message {
            EventMessage::Error { error } => {
                let (server, name) = {
                    let sub = sub.lock();
                    (sub.0.server.clone(), sub.0.name.clone())
                };
                tracing::warn!(server = %server, name = %name, code = ?error.get("code"), "mcp event stream error");
            }
            EventMessage::Heartbeat { cursor } => sub.lock().0.cursor = cursor,
            EventMessage::Active { cursor, truncated } => {
                {
                    let mut sub = sub.lock();
                    sub.1 |= cursor.is_some() && truncated;
                    sub.0.cursor = cursor;
                }
                self.set_status(sub, SubscriptionStatus::Active);
            }
            EventMessage::Event { event_id, timestamp, data, cursor, .. } => {
                let mut payload = {
                    let sub = sub.lock();
                    json!({
                        "id": sub.0.id, "server": sub.0.server, "name": sub.0.name, "eventId": event_id,
                        "timestamp": timestamp, "data": data, "cursor": cursor,
                    })
                };
                if sub.lock().1 {
                    payload["truncated"] = json!(true);
                }
                // The cursor moves past the event only once it is published.
                self.publish(McpEvent::ExperimentalEvent { data: payload });
                let mut sub = sub.lock();
                sub.0.cursor = cursor;
                sub.1 = false;
            }
        }
    }

    /// Streams until the server terminates or rejects the subscription. A clean close reopens like a broken stream;
    /// only a confirmed open resets the delay.
    async fn follow(self, sub: Followed, events: crate::events::Events) {
        use crate::events::{EventMessage, EventStreamError, StreamInput};
        let mut delay = std::time::Duration::from_secs(1);
        loop {
            let (server, input) = {
                let sub = sub.lock();
                let view = &sub.0;
                let input = StreamInput {
                    name: view.name.clone(),
                    arguments: view.arguments.clone(),
                    cursor: Some(view.cursor.clone()),
                    max_age_ms: None,
                };
                (view.server.clone(), input)
            };
            let mut stream = events.stream(input);
            let error = loop {
                match stream.next().await {
                    Ok(Some(message)) => {
                        if matches!(message, EventMessage::Active { .. }) {
                            delay = std::time::Duration::from_secs(1);
                        }
                        self.receive(&sub, message);
                    }
                    Ok(None) => break EventStreamError::Broken(format!("MCP server {server} closed the event stream")),
                    Err(error) => break error,
                }
            };
            drop(stream);
            match error {
                EventStreamError::Rejected { code, message, .. } => {
                    let error = json!({ "code": code, "message": message });
                    return self.set_status(&sub, SubscriptionStatus::Ended { reason: "rejected".into(), error: Some(error) });
                }
                EventStreamError::Terminated { code, message, .. } => {
                    let error = json!({ "code": code, "message": message });
                    return self.set_status(&sub, SubscriptionStatus::Ended { reason: "terminated".into(), error: Some(error) });
                }
                EventStreamError::Broken(_) => self.set_status(&sub, SubscriptionStatus::Pending),
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(EVENT_REOPEN_CAP);
        }
    }

    /// Starts or stops a server's subscriptions to match its status; runs on every status change.
    async fn follows(&self, server: &str) {
        let _guard = self.inner.follows_lock.lock().await;
        if self.closing() {
            return;
        }
        let entry = self.entry(server);
        let client = entry.as_ref().filter(|e| e.status() == Status::Connected).and_then(|e| e.connection());
        let events = client.as_ref().and_then(crate::events::Events::of);
        let subs: Vec<Followed> = self.inner.followed.lock().get(server).map(|subs| subs.values().cloned().collect()).unwrap_or_default();
        for sub in subs {
            let (id, ended) = {
                let sub = sub.lock();
                (sub.0.id.clone(), matches!(sub.0.status, SubscriptionStatus::Ended { .. }))
            };
            if ended {
                continue;
            }
            if let Some(events) = &events {
                let mut streams = self.inner.streams.lock();
                if streams.get(&id).is_none_or(|s| s.is_finished()) {
                    streams.insert(id, tokio::spawn(self.clone().follow(sub.clone(), events.clone())));
                }
                continue;
            }
            if let Some(stream) = self.inner.streams.lock().remove(&id) {
                stream.abort();
            }
            let unavailable = entry.as_ref().is_none_or(|e| e.status() == Status::Disabled) || client.is_some();
            self.set_status(
                &sub,
                if unavailable { SubscriptionStatus::Ended { reason: "unavailable".into(), error: None } } else { SubscriptionStatus::Pending },
            );
        }
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
        entry.conn().stop().await;
        entry.conn().start().await;
    }

    /// Stops every server (Location disposal), publishing nothing. Servers stop in parallel, so one slow server's stop grace does not delay
    /// the rest past a service restart's kill deadline and leave them orphaned.
    pub async fn shutdown(&self) {
        self.inner.closing.store(true, std::sync::atomic::Ordering::SeqCst);
        let attached: Vec<Arc<AttachedEntry>> = self.inner.attached.lock().drain(..).flat_map(|(_, stack)| stack).collect();
        futures::future::join_all(attached.iter().map(|entry| entry.connection.close())).await;
        for (_, stream) in self.inner.streams.lock().drain() {
            stream.abort();
        }
        for task in self.inner.scoped.lock().drain(..) {
            task.abort();
        }
        let entries: Vec<(String, Arc<Entry>)> =
            self.inner.entries.lock().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        futures::future::join_all(entries.into_iter().map(|(name, entry)| async move {
            let _guard = self.inner.locks.lock(&name).await;
            entry.conn().stop().await;
        }))
        .await;
    }

    /// Attaches a client-hosted MCP server (`MCP.attach`): connects over `transport`, which the client already opened,
    /// with the legacy handshake, then lists its tools. Its tools are named after `name` like any server's. Attaching a
    /// name again stacks the new connection on top: it serves the name until it closes, then the most recent remaining
    /// connection takes over. A failed connect or tool list closes the transport.
    ///
    /// The handshake runs outside the lock, so a stalled client never blocks detach or fallback for its name; the lock
    /// only guards adding the finished connection to the stack.
    pub async fn attach(
        &self,
        name: &str,
        transport: Arc<dyn crate::transport::Transport>,
        events: tokio::sync::mpsc::UnboundedReceiver<crate::transport::TransportEvent>,
    ) -> Result<Attachment, ConnectError> {
        if self.closing() {
            transport.close().await;
            return Err(ConnectError::connect(name, "Connection closed"));
        }
        let options = &self.inner.options;
        let connection = Arc::new(
            Connection::attach(name, transport, events, &options.directory, options.elicitation.clone(), &options.client_info)
                .await?,
        );
        // Set before the first tool list, so a connection closing meanwhile is never added to the stack; once added,
        // its close detaches it.
        let state = Arc::new(AttachState::default());
        {
            let (state, weak, name) = (state.clone(), Arc::downgrade(&self.inner), name.to_string());
            connection.on_close(move |_| {
                state.closed.store(true, Ordering::SeqCst);
                let Some(id) = *state.id.lock() else { return };
                let (name, inner) = (name.clone(), weak.clone());
                Callbacks(weak.clone()).run(Box::pin(async move {
                    if let Some(inner) = inner.upgrade() {
                        McpManager { inner }.detach_locked(&name, id).await;
                    }
                }));
            });
        }
        let listed = match connection.tools().await {
            Ok(listed) => listed,
            Err(error) => {
                connection.close().await;
                return Err(ConnectError::connect(name, error.to_string()));
            }
        };
        let _guard = self.inner.attached_locks.lock(name).await;
        if state.closed.load(Ordering::SeqCst) || self.closing() {
            connection.close().await;
            return Err(ConnectError::connect(name, "Connection closed"));
        }
        let id = self.inner.next_attached.fetch_add(1, Ordering::SeqCst);
        let entry = Arc::new(AttachedEntry { id, connection: connection.clone(), tools: Mutex::new(attached_tools(name, listed)) });
        self.inner.attached.lock().entry(name.to_string()).or_default().push(entry.clone());
        *state.id.lock() = Some(id);
        {
            let (weak, name, entry) = (Arc::downgrade(&self.inner), name.to_string(), Arc::downgrade(&entry));
            connection.on_tools_changed(move || {
                let (weak_inner, name, entry) = (weak.clone(), name.clone(), entry.clone());
                Callbacks(weak.clone()).run(Box::pin(async move {
                    let (Some(inner), Some(entry)) = (weak_inner.upgrade(), entry.upgrade()) else { return };
                    McpManager { inner }.refresh_attached(&name, &entry).await;
                }));
            });
        }
        let count = entry.tools.lock().len();
        tracing::info!(server = %name, tools = count, "mcp client attached");
        self.publish(McpEvent::ToolsChanged { server: name.to_string() });
        Ok(Attachment { manager: Arc::downgrade(&self.inner), name: name.to_string(), id, done: false })
    }

    /// Lists the tools of the top connection again after it reported a change. Bounded like fallback, so a connection
    /// that stalls after announcing a change cannot hold the lock; a failed list publishes nothing.
    async fn refresh_attached(&self, name: &str, entry: &Arc<AttachedEntry>) {
        let _guard = self.inner.attached_locks.lock(name).await;
        let top = self.inner.attached.lock().get(name).and_then(|stack| stack.last().cloned());
        if !top.is_some_and(|top| Arc::ptr_eq(&top, entry)) {
            return;
        }
        let Ok(Ok(listed)) = tokio::time::timeout(FALLBACK_LIST_TIMEOUT, entry.connection.tools()).await else { return };
        *entry.tools.lock() = attached_tools(name, listed);
        self.publish(McpEvent::ToolsChanged { server: name.to_string() });
    }

    async fn detach_locked(&self, name: &str, id: u64) {
        let _guard = self.inner.attached_locks.lock(name).await;
        self.detach(name, id).await;
    }

    async fn detach(&self, name: &str, id: u64) {
        let (entry, top, next) = {
            let mut attached = self.inner.attached.lock();
            let Some(stack) = attached.get_mut(name) else { return };
            let Some(position) = stack.iter().position(|e| e.id == id) else { return };
            let top = position + 1 == stack.len();
            // Unlocked reads must never pick a closing connection, so update the stack before closing it.
            let entry = stack.remove(position);
            let next = stack.last().cloned();
            if next.is_none() {
                attached.shift_remove(name);
            }
            (entry, top, next)
        };
        entry.connection.close().await;
        let Some(next) = next else {
            self.publish(McpEvent::ToolsChanged { server: name.to_string() });
            return;
        };
        // Closing an idle connection changes nothing visible.
        if !top {
            return;
        }
        // Idle connections ignore tool list changes, so the one taking over lists its tools again. A short timeout keeps
        // a half-open connection from holding the lock; it then keeps its stored tools.
        let previous = next.tools.lock().clone();
        if let Ok(Ok(listed)) = tokio::time::timeout(FALLBACK_LIST_TIMEOUT, next.connection.tools()).await {
            *next.tools.lock() = attached_tools(name, listed);
        }
        // Readers may have seen the stored tools in the meantime, so compare against both.
        let current = next.tools.lock().clone();
        if *entry.tools.lock() != current || previous != current {
            self.publish(McpEvent::ToolsChanged { server: name.to_string() });
        }
    }

    /// The `{server}` payload helper for bus bridges.
    pub fn event_payload(event: &McpEvent) -> Value {
        json!({"server": event.server()})
    }
}

#[derive(Default)]
struct AttachState {
    closed: AtomicBool,
    id: Mutex<Option<u64>>,
}

/// `toAttachedTools`: an attached client's tools, tagged with its name and without a Code Mode setting of their own.
fn attached_tools(name: &str, listed: Vec<serde_json::Map<String, Value>>) -> Vec<Tool> {
    listed.into_iter().filter_map(|raw| Tool::from_raw(name, None, raw)).collect()
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
            pool: None,
            mcp_events: false,
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
