//! Server plugins (`core/src/plugin.ts`, `core/src/plugin/{supervisor,host,hooks}.ts`) over the
//! oc-plugin Bun sidecar.
//!
//! One [`Plugins`] per server process (owned by [`crate::app::App`]). The [`PluginHost`] is created
//! the first time a location configures plugin operations; the Bun sidecar itself only starts when
//! an operation needs a package or local plugin. With no plugins configured nothing here does any
//! work: hooks and transforms return their input unchanged.
//!
//! Locations are named by the directory string they were requested with (the location cache key,
//! [`Location::key`]), like upstream's `Location.Ref`.
//!
//! Domain transforms run between the native transforms that upstream registers before the plugins
//! (`pre` internal plugins) and the ones after them (`post`); see the `*_with_plugins` helpers. A
//! transform result is cached per (location, domain) until the plugin registrations change
//! (`domain_revision`) or the native input changes.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use futures::future::BoxFuture;
use oc_plugin::{
    ApiError, ApiRequest, HookError, HostEvent, HostOptions, InternalPlugins, LoadRequest, LoadResult, LocationRef, Operation,
    PluginApi, PluginHost, StorageEntry, StorageScan,
};
use oc_schema::{Metadata, ToolContent};
use oc_tools::tool::{DefinitionContext, ToolOptions};
use oc_tools::{Registry, Tool, ToolContext, ToolDefinition, ToolError, ToolResult};
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};

use crate::bus::Bus;
use crate::location::Location;

/// Built-in (internal) plugin ids in activation order (`core/src/plugin/internal.ts` `pre`, the
/// provider and web-search plugins, `post`), as `plugin.list` reports them.
pub const BUILTIN_PLUGINS: [&str; 90] = [
    "opencode.tool.input.repair",
    "opencode.config.worktree",
    "opencode.config.mcp",
    "opencode.mcp.codemode.defaults",
    "opencode.wellknown",
    "opencode.vcs.git",
    "opencode.agent",
    "opencode.plan",
    "opencode.command",
    "opencode.goal",
    "opencode.skill",
    "opencode.vcs.hg",
    "opencode.models.dev",
    "opencode.provider.amazon.bedrock",
    "opencode.provider.azure",
    "opencode.provider.cerebras",
    "opencode.provider.chatgpt",
    "opencode.provider.cloudflare.ai.gateway",
    "opencode.provider.cloudflare.workers.ai",
    "opencode.provider.databricks",
    "opencode.provider.digitalocean",
    "opencode.provider.github.copilot",
    "opencode.provider.gitlab",
    "opencode.provider.google.vertex",
    "opencode.provider.kilo",
    "opencode.provider.llmgateway",
    "opencode.provider.lmstudio",
    "opencode.provider.modal",
    "opencode.provider.neon",
    "opencode.provider.nvidia",
    "opencode.provider.ollama",
    "opencode.provider.opencode",
    "opencode.provider.snowflake.cortex",
    "opencode.provider.openai",
    "opencode.provider.openai-context-caps",
    "opencode.provider.openrouter",
    "opencode.provider.poe",
    "opencode.provider.prompt-cache-key",
    "opencode.provider.sap.ai.core",
    "opencode.provider.vllm",
    "opencode.provider.xai",
    "opencode.provider.zenmux",
    "opencode.provider.dynamic",
    "opencode.websearch.exa",
    "opencode.websearch.firecrawl",
    "opencode.websearch.parallel",
    "opencode.websearch.tavily",
    "opencode.websearch.tinyfish",
    "opencode.tool.patch",
    "opencode.prompt.openai",
    "opencode.prompt.anthropic",
    "opencode.prompt.kimi",
    "opencode.prompt.arcee",
    "opencode.prompt.meta",
    "opencode.prompt.verbosity",
    "opencode.prompt.identity",
    "opencode.tool.edit",
    "opencode.tool.glob",
    "opencode.tool.grep",
    "opencode.tool.image-generate",
    "opencode.tools",
    "opencode.tools.mcp-resources",
    "opencode.tool.question",
    "opencode.tool.read",
    "opencode.tool.schedule",
    "opencode.tool.shell",
    "opencode.tool.skill",
    "opencode.tool.subagent",
    "opencode.tool.todo",
    "opencode.tool.webfetch",
    "opencode.tool.websearch",
    "opencode.tool.write",
    "opencode.warming",
    "opencode.config.instruction",
    "opencode.config.reference",
    "opencode.config.agent",
    "opencode.browser",
    "opencode.config.command",
    "opencode.config.compaction",
    "opencode.config.formatter",
    "opencode.config.image",
    "opencode.config.location-watcher",
    "opencode.config.shell",
    "opencode.config.snapshot",
    "opencode.config.tool-output",
    "opencode.config.compatibility",
    "opencode.config.skill",
    "opencode.config.provider",
    "opencode.config.websearch",
    "opencode.config.policy",
];

/// Index of the first `post` internal plugin in [`BUILTIN_PLUGINS`].
const POST_START: usize = 73;

/// The ocd-native internal plugins in upstream order.
pub fn internal_plugins() -> InternalPlugins {
    InternalPlugins {
        pre: BUILTIN_PLUGINS[..POST_START].iter().map(|s| s.to_string()).collect(),
        post: BUILTIN_PLUGINS[POST_START..].iter().map(|s| s.to_string()).collect(),
    }
}

/// `Plugin.Info` of a built-in plugin.
pub fn builtin_info(id: &str) -> Value {
    json!({ "id": id, "source": { "type": "builtin" }, "features": { "server": true }, "state": { "status": "active" } })
}

/// Domains whose `*.updated` notify runs through `State.changes` (#56267): it fires only when the
/// domain's public projection changed since the last notification. Provider and model notify always.
pub const GATED_DOMAINS: [&str; 7] = ["integration", "agent", "command", "skill", "websearch", "reference", "plugin"];

/// Computes a domain's public projection at a location (serialized JSON), for [`Plugins::domain_changed`].
pub type Projector = Arc<dyn Fn(String, &'static str) -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// `<domain>.updated` event published when a domain changes (the domain State's `notify`).
pub fn domain_event(domain: &str) -> Option<&'static str> {
    Some(match domain {
        "agent" => "agent.updated",
        "command" => "command.updated",
        "skill" => "skill.updated",
        "model" => "model.updated",
        "provider" => "provider.updated",
        "integration" => "integration.updated",
        "websearch" => "websearch.updated",
        "reference" => "reference.updated",
        _ => return None,
    })
}

/// Turns a configured operation into the sidecar's [`Operation`].
pub fn operations(location: &Location) -> Vec<Operation> {
    match oc_config::sources::plugin_operations(&location.entries, &location.directory) {
        Ok(ops) => ops
            .into_iter()
            .map(|op| match op {
                oc_config::sources::PluginOperation::Add { target, options } => Operation::Add { target, options, mtime: None },
                oc_config::sources::PluginOperation::Remove { target } => Operation::Remove { target },
            })
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "invalid plugin configuration");
            vec![]
        }
    }
}

/// The built-in plugin that registers a native tool (by registered, un-namespaced name).
pub fn tool_plugin(tool: &dyn Tool) -> Option<&'static str> {
    Some(match tool.name() {
        "patch" => "opencode.tool.patch",
        "edit" => "opencode.tool.edit",
        "glob" => "opencode.tool.glob",
        "grep" => "opencode.tool.grep",
        "list_mcp_resources" | "read_mcp_resource" => "opencode.tools.mcp-resources",
        "question" => "opencode.tool.question",
        "read" => "opencode.tool.read",
        "shell" => "opencode.tool.shell",
        "skill" => "opencode.tool.skill",
        "subagent" => "opencode.tool.subagent",
        "todowrite" => "opencode.tool.todo",
        "webfetch" => "opencode.tool.webfetch",
        "websearch" => "opencode.tool.websearch",
        "write" => "opencode.tool.write",
        _ if tool.options().namespace.as_deref() == Some("opencode") => "opencode.tools",
        _ => return None,
    })
}

/// Native function handle of a tool executor.
pub fn tool_handle(id: &str) -> String {
    format!("rs:tool.{id}")
}

/// Native function handle of a command.
pub fn command_handle(name: &str) -> String {
    format!("rs:command.{name}")
}

fn hash_value(value: &Value) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.to_string().hash(&mut hasher);
    hasher.finish()
}

#[derive(Default)]
struct LocState {
    /// Last loaded location (watch → reload mapping, replays).
    location: Option<Arc<Location>>,
    fingerprint: Option<String>,
    /// Local plugin targets (directories) of the last load.
    locals: Vec<PathBuf>,
    result: Option<LoadResult>,
    /// Activation generation requested/finished.
    progress: Option<watch::Sender<(u64, u64)>>,
    /// Inventory last announced with `plugin.updated` (`None` until the first activation settled).
    announced: Option<Vec<Value>>,
}

impl LocState {
    fn progress(&mut self) -> &watch::Sender<(u64, u64)> {
        self.progress.get_or_insert_with(|| watch::channel((0, 0)).0)
    }
}

/// A native function the core exposes to plugins through `{"$fn": "rs:..."}` handles.
pub type NativeFn = Arc<dyn Fn(Vec<Value>) -> BoxFuture<'static, Result<Value, ApiError>> + Send + Sync>;

/// Supplies the current tool domain value of a location (`tool.list`).
pub trait ToolSource: Send + Sync {
    fn tool_value(&self, key: String) -> BoxFuture<'static, Option<Value>>;
}

/// Domain change notifications for consumers of plugin-shaped state (e.g. MCP reconciliation).
#[derive(Debug, Clone)]
pub struct DomainChange {
    pub key: String,
    pub domain: String,
}

pub struct Plugins {
    global: Arc<oc_util::Global>,
    db: oc_storage::Database,
    bus: Bus,
    app_info: Value,
    /// The built-in GitHub Copilot plugin's runtime state and hooks.
    pub copilot: Arc<crate::copilot::Copilot>,
    /// The built-in OpenAI provider plugins' loaded sign-in state.
    pub openai: Arc<crate::openai::OpenAi>,
    /// The built-in Azure provider plugin's Entra ID token hooks.
    pub azure: Arc<crate::azure::Azure>,
    /// Each location's latest model catalog, for built-in hooks that look up the request's model
    /// (`Model.Service.get` in upstream's plugins).
    catalogs: Mutex<HashMap<String, oc_ai::catalog::Catalog>>,
    api: RwLock<Option<Arc<dyn PluginApi>>>,
    host: OnceLock<PluginHost>,
    locations: Mutex<HashMap<String, LocState>>,
    cache: Mutex<HashMap<(String, String), (u64, u64, Value)>>,
    /// Native functions by location key and handle.
    natives: Mutex<HashMap<String, HashMap<String, NativeFn>>>,
    /// In-flight tool calls (call id → context) for native executors invoked by plugins.
    inflight: Mutex<HashMap<String, ToolContext>>,
    tool_source: RwLock<Option<Arc<dyn ToolSource>>>,
    changes: broadcast::Sender<DomainChange>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    /// The location-less web search service with the built-in providers (the template every
    /// location's registry starts from).
    websearch: RwLock<Option<Arc<oc_tools::websearch::WebSearch>>>,
    /// Each location's web search registry (built-ins → plugin `websearch` transforms).
    websearch_by_key: Mutex<HashMap<String, Arc<oc_tools::websearch::WebSearch>>>,
    /// Decoded catalog transform results per (location, domain): (revision, input hash, value).
    catalog_cache: Mutex<HashMap<(String, String), (u64, u64, CatalogValue)>>,
    watched: Mutex<HashSet<PathBuf>>,
    /// Bumped whenever cached plugin `provider`/`model`/`integration` results are dropped (a
    /// reload, new registrations, a (re)activation): part of [`Plugins::catalog_state`].
    catalog_epoch: std::sync::atomic::AtomicU64,
    /// Projections of the gated domains (`State.changes`) per (location key, domain), as last notified.
    /// They outlive a location rebuild, as upstream reloads its States in place.
    published: Mutex<HashMap<(String, &'static str), String>>,
    projector: RwLock<Option<Projector>>,
    me: Weak<Plugins>,
}

impl Plugins {
    pub fn new(global: Arc<oc_util::Global>, db: oc_storage::Database, bus: Bus, version: &str) -> Arc<Plugins> {
        let (changes, _) = broadcast::channel(256);
        Arc::new_cyclic(|me| Plugins {
            global,
            db,
            copilot: crate::copilot::Copilot::new(bus.clone(), version),
            openai: Arc::new(crate::openai::OpenAi::new()),
            azure: Arc::default(),
            catalogs: Mutex::new(HashMap::new()),
            bus,
            // `App.Info` of the server (`App.configured(options.app)`: the CLI names itself "cli").
            app_info: json!({ "name": oc_util::version::ARTIFACT, "version": version, "channel": oc_util::version::CHANNEL }),
            api: RwLock::new(None),
            host: OnceLock::new(),
            locations: Mutex::new(HashMap::new()),
            cache: Mutex::new(HashMap::new()),
            natives: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            tool_source: RwLock::new(None),
            changes,
            watcher: Mutex::new(None),
            websearch: RwLock::new(None),
            websearch_by_key: Mutex::new(HashMap::new()),
            catalog_cache: Mutex::new(HashMap::new()),
            watched: Mutex::new(HashSet::new()),
            catalog_epoch: std::sync::atomic::AtomicU64::new(0),
            published: Mutex::new(HashMap::new()),
            projector: RwLock::new(None),
            me: me.clone(),
        })
    }

    /// Installs the function computing domain projections (the backend's list endpoints).
    pub fn set_projector(&self, projector: Projector) {
        *self.projector.write() = Some(projector);
    }

    fn gated(domain: &str) -> Option<&'static str> {
        GATED_DOMAINS.iter().copied().find(|d| *d == domain)
    }

    /// `State.changes`: whether `domain`'s projection at `key` differs from the last notified one (recording
    /// it). Ungated domains, and every domain without a projector, always notify.
    pub async fn domain_changed(&self, key: &str, domain: &str) -> bool {
        let Some(domain) = Self::gated(domain) else { return true };
        let projector = self.projector.read().clone();
        let Some(projector) = projector else { return true };
        let Some(projection) = projector(key.to_string(), domain).await else { return true };
        self.note_projection(key, domain, projection)
    }

    /// Whether a projection was recorded for `domain` at `key`.
    pub fn has_projection(&self, key: &str, domain: &str) -> bool {
        Self::gated(domain).is_some_and(|d| self.published.lock().contains_key(&(key.to_string(), d)))
    }

    /// Drops the projections recorded at `key` (a fresh boot notifies every domain again).
    pub fn forget_projections(&self, key: &str) {
        self.published.lock().retain(|(k, _), _| k != key);
    }

    /// Records a gated domain's projection at `key`; whether it differs from the last one recorded.
    pub fn note_projection(&self, key: &str, domain: &str, projection: String) -> bool {
        let Some(domain) = Self::gated(domain) else { return true };
        let mut published = self.published.lock();
        let entry = (key.to_string(), domain);
        if published.get(&entry) == Some(&projection) {
            return false;
        }
        published.insert(entry, projection);
        true
    }

    /// Publishes `<domain>.updated` at `at` unless the domain's projection is unchanged. Runs in its own task
    /// so a projection that needs the plugin host never waits on the host's event loop.
    fn notify_domain(&self, key: &str, domain: &str, at: Option<oc_schema::types::LocationRef>) {
        let Some(kind) = domain_event(domain) else { return };
        let Some(me) = self.me.upgrade() else { return };
        let (key, domain) = (key.to_string(), domain.to_string());
        tokio::spawn(async move {
            if me.domain_changed(&key, &domain).await {
                me.bus.publish(kind, json!({}), at);
            }
        });
    }

    /// Installs the [`PluginApi`] plugins call back into (the backend's, which serves the
    /// HTTP-equivalent methods). Must happen before the first plugin loads; without it the core's
    /// own [`CoreApi`] answers (storage, events, reloads).
    pub fn set_api(&self, api: Arc<dyn PluginApi>) {
        *self.api.write() = Some(api);
    }

    pub fn set_tool_source(&self, source: Arc<dyn ToolSource>) {
        *self.tool_source.write() = Some(source);
    }

    /// The built-in web search service locations fork their registries from (the first one wins).
    pub fn add_websearch(&self, websearch: Arc<oc_tools::websearch::WebSearch>) {
        let mut base = self.websearch.write();
        if base.is_none() {
            *base = Some(websearch);
        }
    }

    /// Location `key`'s web search service (upstream `WebSearch` is a location node): the
    /// built-in providers plus the providers the location's plugins registered. `None` before a
    /// base service was installed.
    pub fn websearch_for(&self, key: &str) -> Option<Arc<oc_tools::websearch::WebSearch>> {
        let base = self.websearch.read().clone()?;
        Some(self.websearch_by_key.lock().entry(key.to_string()).or_insert_with(|| Arc::new(base.fork())).clone())
    }

    pub fn subscribe_changes(&self) -> broadcast::Receiver<DomainChange> {
        self.changes.subscribe()
    }

    /// The plugin host, when one was created.
    pub fn host(&self) -> Option<&PluginHost> {
        self.host.get()
    }

    fn me(&self) -> Arc<Plugins> {
        self.me.upgrade().expect("plugins alive")
    }

    pub(crate) fn host_or_create(&self) -> &PluginHost {
        self.host.get_or_init(|| {
            let api = self.api.read().clone().unwrap_or_else(|| Arc::new(CoreApi::new(self.me.clone())) as Arc<dyn PluginApi>);
            let host = PluginHost::new(HostOptions::from_global(&self.global, self.app_info.clone()), api);
            self.spawn_listeners(&host);
            host
        })
    }

    fn spawn_listeners(&self, host: &PluginHost) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        // Host notifications.
        let mut rx = host.subscribe();
        let weak = self.me.clone();
        handle.spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let Some(plugins) = weak.upgrade() else { break };
                        plugins.host_event(event).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
        // `ctx.event.subscribe()`: forward every bus event while some plugin listens.
        let mut events = self.bus.subscribe();
        let weak = self.me.clone();
        handle.spawn(async move {
            loop {
                match events.recv().await {
                    Ok(env) => {
                        let Some(plugins) = weak.upgrade() else { break };
                        let Some(host) = plugins.host.get() else { continue };
                        if !host.wants_events() {
                            continue;
                        }
                        let location = env
                            .location
                            .as_ref()
                            .map(|l| LocationRef { directory: l.directory.clone(), workspace_id: l.workspace_id.clone() });
                        let wire = serde_json::to_value(&*env).unwrap_or(Value::Null);
                        host.publish_event(location.as_ref(), &wire);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
    }

    async fn host_event(&self, event: HostEvent) {
        match event {
            HostEvent::Watch { file } => self.watch_file(file),
            HostEvent::PluginsChanged { location } => self.announce(&location.key()),
            HostEvent::DomainsChanged { location, domains } => {
                let key = location.key();
                let settled = self.locations.lock().get(&key).is_some_and(|s| s.announced.is_some());
                for domain in domains {
                    self.cache.lock().remove(&(key.clone(), domain.clone()));
                    self.catalog_changed(&domain);
                    if settled {
                        self.notify_domain(&key, &domain, Some(oc_schema::types::LocationRef { directory: key.clone(), workspace_id: None }));
                    }
                    if domain == "websearch" && settled {
                        self.search_providers(&key).await;
                    }
                    let _ = self.changes.send(DomainChange { key: key.clone(), domain });
                }
            }
            HostEvent::Restarted => {
                self.cache.lock().clear();
                self.catalog_changed("*");
                let keys: Vec<String> = self.locations.lock().keys().cloned().collect();
                for key in keys {
                    for domain in ["agent", "command", "skill", "tool", "mcp", "reference", "model", "provider", "integration", "websearch", "vcs", "worktree"] {
                        let _ = self.changes.send(DomainChange { key: key.clone(), domain: domain.into() });
                    }
                }
            }
            HostEvent::Crashed { status } => tracing::warn!(%status, "plugin host crashed"),
            HostEvent::HooksChanged { .. } | HostEvent::RpcChanged { .. } => {}
        }
    }

    /// Publishes `plugin.updated` when the inventory differs from the last announced one (only once
    /// the location's first activation settled; the boot notifications announce that one).
    fn announce(&self, key: &str) {
        let inventory = self.inventory(key);
        let previous = {
            let mut locations = self.locations.lock();
            let Some(state) = locations.get_mut(key) else { return };
            match &state.announced {
                Some(previous) if *previous != inventory => state.announced.replace(inventory.clone()),
                _ => None,
            }
        };
        let Some(previous) = previous else { return };
        let at = Some(oc_schema::types::LocationRef { directory: key.to_string(), workspace_id: None });
        // A plugin disabled after a failing transform: its registrations are refreshed, including
        // the domain whose rebuild failed (`State.group` refresh), before the inventory update.
        let failed = |list: &[Value], id: &str| list.iter().any(|p| p["id"] == id && p["state"]["status"] == "failed");
        for plugin in &inventory {
            let Some(id) = plugin.get("id").and_then(Value::as_str) else { continue };
            if failed(&previous, id) {
                continue;
            }
            let Some(error) = plugin.pointer("/state/error").and_then(Value::as_str) else { continue };
            let Some(domain) = error.strip_prefix("Plugin disabled after ").and_then(|r| r.split_once(".transform failed")).map(|(d, _)| d) else { continue };
            self.cache.lock().remove(&(key.to_string(), domain.to_string()));
            self.catalog_changed(domain);
            if domain_event(domain).is_some() {
                self.notify_domain(key, domain, at.clone());
            }
            let _ = self.changes.send(DomainChange { key: key.to_string(), domain: domain.to_string() });
        }
        self.bus.publish("plugin.updated", json!({}), at);
    }

    /// Rebuilds every domain plugins transform at the location, like the domain States' notify at
    /// the end of an activation batch (a failing transform disables its plugin right away).
    pub async fn rebuild(&self, location: &Arc<Location>) {
        let Some(host) = self.host.get() else { return };
        let domains = host.transform_domains(&LocationRef::new(location.key.clone()));
        let version = self.app_info["version"].as_str().unwrap_or_default().to_string();
        for domain in domains {
            match domain.as_str() {
                "agent" => {
                    self.agents(location, &self.global.home).await;
                }
                "skill" => {
                    self.skills(location, &self.global, &version).await;
                }
                "reference" => {
                    self.references(location, &self.global).await;
                }
                "mcp" => {
                    let sections = crate::mcp::sections(location);
                    self.mcp_servers(location, oc_mcp::config::merge_documents(sections.iter())).await;
                }
                "command" => {
                    self.commands(&location.key, vec![]).await;
                }
                "tool" => {
                    self.tool_list(&location.key).await;
                }
                "websearch" => self.search_providers(&location.key).await,
                _ => {}
            }
        }
    }

    fn watch_file(&self, file: PathBuf) {
        if oc_config::discovery::filewatcher_disabled() || !self.watched.lock().insert(file.clone()) {
            return;
        }
        let mut watcher = self.watcher.lock();
        if watcher.is_none() {
            let weak = self.me.clone();
            let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();
            let created = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else { return };
                if matches!(event.kind, notify::EventKind::Access(_)) {
                    return;
                }
                for path in event.paths {
                    let _ = tx.send(path);
                }
            });
            match created {
                Ok(w) => *watcher = Some(w),
                Err(error) => {
                    tracing::warn!(%error, "plugin file watcher unavailable");
                    return;
                }
            }
            handle.spawn(async move {
                while let Some(first) = rx.recv().await {
                    let mut changed = vec![first];
                    // Debounce like upstream's supervisor (100 ms).
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(100);
                    while let Ok(Some(path)) = tokio::time::timeout_at(deadline, rx.recv()).await {
                        changed.push(path);
                    }
                    let Some(plugins) = weak.upgrade() else { break };
                    plugins.files_changed(&changed).await;
                }
            });
        }
        if let Some(w) = watcher.as_mut() {
            use notify::Watcher;
            if let Err(error) = w.watch(&file, notify::RecursiveMode::NonRecursive) {
                tracing::debug!(%error, file = %file.display(), "cannot watch plugin file");
            }
        }
    }

    async fn files_changed(&self, files: &[PathBuf]) {
        let targets: Vec<Arc<Location>> = {
            let watched = self.watched.lock();
            let locations = self.locations.lock();
            locations
                .values()
                .filter(|s| {
                    files.iter().any(|f| watched.contains(f) && s.locals.iter().any(|l| f.starts_with(l) || f == l))
                        || files.iter().any(|f| s.locals.iter().any(|l| f.starts_with(l)))
                })
                .filter_map(|s| s.location.clone())
                .collect()
        };
        for location in targets {
            tracing::info!(location = %location.key, "plugin source changed; reloading plugins");
            self.load(location, true).await;
        }
    }

    /// Activates the location's plugins when its configuration changed since the last load
    /// (`PluginSupervisor.activate`). Returns immediately; activation runs in the background and
    /// [`Plugins::await_activation`] waits for it.
    pub fn ensure(&self, location: &Arc<Location>) {
        let ops = operations(location);
        let fingerprint = serde_json::to_string(&ops).unwrap_or_default();
        {
            let mut locations = self.locations.lock();
            let state = locations.entry(location.key.clone()).or_default();
            if state.fingerprint.as_deref() == Some(fingerprint.as_str()) {
                state.location = Some(location.clone());
                return;
            }
            if ops.is_empty() && self.host.get().is_none_or(|h| h.packages(&LocationRef::new(location.key.clone())).is_empty()) && state.result.is_none() {
                // Nothing configured and nothing running: built-ins only, no host needed.
                state.fingerprint = Some(fingerprint);
                state.location = Some(location.clone());
                state.announced.get_or_insert_with(Vec::new);
                return;
            }
            // Claim this configuration now: a second `ensure` for the same location (the runner and
            // the backend each load locations) must not start another load before this one runs.
            state.fingerprint = Some(fingerprint);
            let (requested, done) = *state.progress().borrow();
            state.progress().send_replace((requested.max(done) + 1, done));
        }
        let me = self.me();
        let location = location.clone();
        tokio::spawn(async move {
            me.load(location, false).await;
        });
    }

    /// Loads (or reloads) a location's plugins and waits for activation.
    pub async fn load(&self, location: Arc<Location>, force: bool) {
        let ops = operations(&location);
        let fingerprint = serde_json::to_string(&ops).unwrap_or_default();
        let key = location.key.clone();
        let generation = {
            let mut locations = self.locations.lock();
            let state = locations.entry(key.clone()).or_default();
            if !force && state.fingerprint.as_deref() == Some(fingerprint.as_str()) && state.result.is_some() {
                return;
            }
            state.fingerprint = Some(fingerprint);
            state.location = Some(location.clone());
            state.locals = ops
                .iter()
                .filter_map(|op| match op {
                    Operation::Add { target, .. } if Path::new(target).is_absolute() => Some(PathBuf::from(target)),
                    _ => None,
                })
                .collect();
            let (requested, done) = *state.progress().borrow();
            let generation = requested.max(done + 1);
            state.progress().send_replace((generation, done));
            generation
        };
        let host = self.host_or_create();
        let info = json!({
            "directory": key,
            "project": {
                "id": location.project.id,
                "directory": location.project.directory.display().to_string(),
                "canonical": location.project.canonical.display().to_string(),
            },
        });
        let result = host
            .load(LoadRequest { location: LocationRef::new(key.clone()), info, operations: ops, internal: internal_plugins(), blocked: None })
            .await;
        let first = {
            let mut locations = self.locations.lock();
            let state = locations.entry(key.clone()).or_default();
            state.result = Some(result);
            state.announced.is_none()
        };
        // Every cached domain of this location may have changed.
        self.cache.lock().retain(|(k, _), _| *k != key);
        self.catalog_cache.lock().retain(|(k, _), _| *k != key);
        self.catalog_changed("*");
        let inventory = first.then(|| self.inventory(&key));
        {
            let mut locations = self.locations.lock();
            let state = locations.entry(key.clone()).or_default();
            if let Some(inventory) = inventory {
                state.announced = Some(inventory);
            }
            let (requested, done) = *state.progress().borrow();
            state.progress().send_replace((requested, done.max(generation)));
        }
        if !first {
            self.announce(&key);
            self.rebuild(&location).await;
        }
    }

    /// `Plugin.awaitActivation`: waits until every requested activation of the location settled.
    pub async fn await_activation(&self, key: &str) {
        let rx = {
            let mut locations = self.locations.lock();
            let Some(state) = locations.get_mut(key) else { return };
            state.progress().subscribe()
        };
        let mut rx = rx;
        let _ = rx.wait_for(|(requested, done)| done >= requested).await;
    }

    /// Whether the location's first activation finished (plugins configured or not).
    pub fn activated(&self, key: &str) -> bool {
        self.locations.lock().get(key).is_some_and(|s| s.announced.is_some())
    }

    /// Whether the location has plugin operations (so its boot is asynchronous).
    pub fn has_operations(location: &Location) -> bool {
        !operations(location).is_empty()
    }

    /// Closes a location's plugins (cleanup functions run) and forgets its state.
    pub async fn close(&self, key: &str) {
        self.copilot.close(key);
        let state = self.locations.lock().remove(key);
        self.websearch_by_key.lock().remove(key);
        self.cache.lock().retain(|(k, _), _| k != key);
        self.catalog_cache.lock().retain(|(k, _), _| k != key);
        self.catalog_changed("*");
        self.natives.lock().remove(key);
        if state.is_some_and(|s| s.result.is_some())
            && let Some(host) = self.host.get()
        {
            host.close_location(&LocationRef::new(key.to_string())).await;
        }
        // The location's AI SDK models and SDKs (`AISDK` is a location node upstream).
        if let Some(host) = self.host.get() {
            host.aisdk_close(&LocationRef::new(key.to_string())).await;
        }
    }

    /// Stops the sidecar (server exit).
    pub async fn shutdown(&self) {
        if let Some(host) = self.host.get() {
            host.shutdown().await;
        }
    }

    /// `plugin.list`: the location's inventory in upstream order.
    pub fn inventory(&self, key: &str) -> Vec<Value> {
        let locations = self.locations.lock();
        match locations.get(key).and_then(|s| s.result.as_ref()) {
            Some(result) => {
                // The sidecar's latest inventory (a plugin disabled after a failing transform).
                let mut result = result.clone();
                if let Some(host) = self.host.get() {
                    let packages = host.packages(&LocationRef::new(key.to_string()));
                    if !packages.is_empty() {
                        result.packages = packages;
                    }
                }
                result.inventory(builtin_info)
            }
            None => BUILTIN_PLUGINS.iter().map(|id| builtin_info(id)).collect(),
        }
    }

    /// Enabled plugin ids (built-in gating), `None` when the location loaded no operations.
    pub fn enabled(&self, key: &str) -> Option<Vec<String>> {
        self.locations.lock().get(key).and_then(|s| s.result.as_ref()).map(|r| r.resolution.enabled_ids())
    }

    /// Whether a built-in plugin is enabled at the location (`-opencode.browser`, `-*`, ...).
    pub fn builtin_enabled(&self, key: &str, id: &str) -> bool {
        self.enabled(key).is_none_or(|ids| ids.iter().any(|i| i == id))
    }

    // ---------------------------------------------------------------------------------- hooks

    /// The server version (`App.Info.version`).
    pub fn version(&self) -> &str {
        self.app_info["version"].as_str().unwrap_or_default()
    }

    /// Records the catalog location `key` serves (`None`: the location closed).
    pub fn set_catalog(&self, key: &str, catalog: Option<oc_ai::catalog::Catalog>) {
        match catalog {
            Some(catalog) => self.catalogs.lock().insert(key.to_string(), catalog),
            None => self.catalogs.lock().remove(key),
        };
    }

    /// The latest catalog of location `key`.
    pub fn catalog(&self, key: &str) -> Option<oc_ai::catalog::Catalog> {
        self.catalogs.lock().get(key).cloned()
    }

    /// Whether the built-in GitHub Copilot plugin runs at the location.
    pub fn copilot_enabled(&self, key: &str) -> bool {
        self.builtin_enabled(key, crate::copilot::PLUGIN_ID)
    }

    /// Whether the built-in Azure plugin's Entra hooks apply to `provider_id`'s requests at the location.
    pub fn azure_hooks(&self, key: &str, provider_id: &str) -> bool {
        provider_id == crate::azure::PROVIDER_ID && self.builtin_enabled(key, crate::azure::PLUGIN_ID)
    }

    pub fn has_hook(&self, key: &str, hook: &str, provider_id: Option<&str>) -> bool {
        self.host.get().is_some_and(|h| h.has_hook(&LocationRef::new(key.to_string()), hook, provider_id))
    }

    /// `PluginHooks.trigger`: runs the plugin callbacks of `hook` and returns the mutated event
    /// (the event itself, without IPC, when nothing is registered).
    pub async fn trigger(&self, key: &str, hook: &str, event: Value) -> Result<Value, HookError> {
        match self.host.get() {
            Some(host) => host.trigger_hook(&LocationRef::new(key.to_string()), hook, event).await,
            None => Ok(event),
        }
    }

    /// `shell.create.before`: plugins may rewrite the command, cwd, timeout, shell and env.
    pub async fn shell_before(&self, key: &str, invocation: &mut oc_tools::host::ShellInvocation) -> Result<(), String> {
        if !self.has_hook(key, "shell.create.before", None) {
            return Ok(());
        }
        let event = json!({
            "command": invocation.command,
            "cwd": invocation.cwd,
            "timeout": invocation.timeout,
            "shell": invocation.shell,
            "env": invocation.env,
        });
        let out = self.trigger(key, "shell.create.before", event).await.map_err(|e| e.to_string())?;
        if let Some(v) = out.get("command").and_then(Value::as_str) {
            invocation.command = v.to_string();
        }
        if let Some(v) = out.get("cwd").and_then(Value::as_str) {
            invocation.cwd = v.to_string();
        }
        if let Some(v) = out.get("timeout").and_then(Value::as_f64) {
            invocation.timeout = v.max(0.0) as u64;
        }
        if let Some(v) = out.get("shell").and_then(Value::as_str) {
            invocation.shell = v.to_string();
        }
        if let Some(env) = out.get("env").and_then(Value::as_object) {
            invocation.env = env.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect();
        }
        Ok(())
    }

    /// The session's location key.
    pub fn session_key(&self, session_id: &str) -> Option<String> {
        self.db.session_get(session_id).ok().flatten().map(|s| s.location.directory)
    }

    // ---------------------------------------------------------------------------- transforms

    /// Applies the plugin transforms of `domain` (registration order) to the native `pre` value.
    /// Cached until the registrations or the input change.
    pub async fn transform(&self, key: &str, domain: &str, value: Value) -> Value {
        let Some(host) = self.host.get() else { return value };
        let location = LocationRef::new(key.to_string());
        if !host.transform_domains(&location).iter().any(|d| d == domain) {
            return value;
        }
        let revision = host.domain_revision(&location, domain);
        let input = hash_value(&value);
        let cache_key = (key.to_string(), domain.to_string());
        if let Some((r, h, out)) = self.cache.lock().get(&cache_key)
            && *r == revision
            && *h == input
        {
            return out.clone();
        }
        match host.transform_ref(&location, domain, &value).await {
            Ok(Some(out)) => {
                // The catalog domains keep their decoded result instead (`catalog_cache`): a raw
                // copy of every provider record and model would only double the memory held.
                if !matches!(domain, "provider" | "model") {
                    self.cache.lock().insert(cache_key, (revision, input, out.clone()));
                }
                out
            }
            Ok(None) => value,
            Err(error) => {
                tracing::error!(%error, domain, "plugin transform failed");
                value
            }
        }
    }

    /// [`Plugins::transform`] of a catalog domain (`provider`, `model`), whose input holds every
    /// provider record and model: the sidecar keeps the input under its fingerprint `input`, so a
    /// rebuild with unchanged native records (e.g. after a plugin's `provider.reload()`) neither
    /// builds nor sends it again. `None` when no plugin transforms the domain or the transform failed.
    async fn transform_catalog(&self, key: &str, domain: &str, input: u64, value: impl FnOnce() -> Value) -> Option<Value> {
        let host = self.host.get()?;
        match host.transform_keyed(&LocationRef::new(key.to_string()), domain, input, value).await {
            Ok(out) => out,
            Err(error) => {
                tracing::error!(%error, domain, "plugin transform failed");
                None
            }
        }
    }

    /// Drops a cached domain (`<domain>.reload`).
    pub fn invalidate(&self, key: &str, domain: &str) {
        self.catalog_cache.lock().remove(&(key.to_string(), domain.to_string()));
        self.cache.lock().remove(&(key.to_string(), domain.to_string()));
        self.catalog_changed(domain);
    }

    /// Marks cached catalog inputs stale when `domain` (`*`: every domain) feeds the catalog.
    fn catalog_changed(&self, domain: &str) {
        if matches!(domain, "*" | "provider" | "model" | "integration") {
            self.catalog_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Fingerprint of everything plugins contribute to location `key`'s model catalog (which of
    /// `provider`/`model`/`integration` they transform, their registration revisions and reloads).
    /// [`crate::models::Models`] rebuilds a cached catalog when it changes.
    pub fn catalog_state(&self, key: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.catalog_epoch.load(std::sync::atomic::Ordering::SeqCst).hash(&mut hasher);
        if let Some(host) = self.host.get() {
            let location = LocationRef::new(key.to_string());
            let domains = host.transform_domains(&location);
            for domain in ["provider", "model", "integration"] {
                domains.iter().any(|d| d == domain).hash(&mut hasher);
                host.domain_revision(&location, domain).hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    fn register_native(&self, key: &str, handle: String, f: NativeFn) {
        self.natives.lock().entry(key.to_string()).or_default().insert(handle, f);
    }

    /// Calls a native (`rs:`) function handle on behalf of a plugin.
    pub async fn invoke_native(&self, location: &LocationRef, handle: &str, args: Vec<Value>) -> Result<Value, ApiError> {
        let f = self.natives.lock().get(&location.key()).and_then(|m| m.get(handle).cloned());
        match f {
            Some(f) => f(args).await,
            None => Err(ApiError::new(format!("Unknown function handle: {handle}"))),
        }
    }

    /// Agents: built-in (`opencode.agent`, `opencode.plan`) → plugins → configuration and browser.
    pub async fn agents(&self, location: &Location, home: &Path) -> crate::agent::Agents {
        let mut builtin = crate::agent::Agents::builtin(home);
        // Built-in plugins removed by configuration (`-opencode.agent`, `-opencode.plan`).
        if !self.builtin_enabled(&location.key, "opencode.agent") {
            builtin.items.retain(|id, _| id == "plan");
        }
        if !self.builtin_enabled(&location.key, "opencode.plan") {
            builtin.items.shift_remove("plan");
        }
        let builtin = if self.host.get().is_some() {
            let value = json!({
                "agents": builtin.items.values().collect::<Vec<_>>(),
                "default": builtin.default,
            });
            let out = self.transform(&location.key, "agent", value).await;
            let mut agents = crate::agent::Agents::default();
            for item in out.get("agents").and_then(Value::as_array).cloned().unwrap_or_default() {
                match serde_json::from_value::<crate::agent::Agent>(item.clone()) {
                    Ok(agent) => {
                        agents.items.insert(agent.id.clone(), agent);
                    }
                    Err(error) => tracing::warn!(%error, agent = %item, "invalid plugin agent"),
                }
            }
            agents.default = out.get("default").and_then(Value::as_str).map(String::from);
            agents
        } else {
            builtin
        };
        let browser = self.builtin_enabled(&location.key, "opencode.browser");
        if !self.builtin_enabled(&location.key, "opencode.config.agent") {
            let mut out = builtin;
            if browser {
                for agent in out.items.values_mut() {
                    agent.permissions.push(oc_schema::types::PermissionRule {
                        action: "browser".into(),
                        resource: "*".into(),
                        effect: oc_schema::types::Effect::Deny,
                    });
                }
            }
            return out;
        }
        location.agents_from(builtin, home, browser)
    }

    /// Skills: built-ins (`opencode.skill`) → plugins → compatibility roots and configuration.
    pub async fn skills(&self, location: &Location, global: &oc_util::Global, version: &str) -> Vec<oc_config::sources::Skill> {
        let mut builtin = crate::skills::builtin(location, version);
        if !self.builtin_enabled(&location.key, "opencode.skill") {
            builtin.clear();
        }
        let builtin = if self.host.get().is_some() {
            let value = json!({ "skills": builtin.iter().map(skill_json).collect::<Vec<_>>() });
            let out = self.transform(&location.key, "skill", value).await;
            out.get("skills").and_then(Value::as_array).cloned().unwrap_or_default().iter().filter_map(skill_from_json).collect()
        } else {
            builtin
        };
        let compat = self.builtin_enabled(&location.key, "opencode.config.compatibility");
        let config = self.builtin_enabled(&location.key, "opencode.config.skill");
        let mut skills = crate::skills::with_sources(builtin, location, global, compat, config).await;
        let policies = self.policies(location);
        skills.retain(|skill| crate::policy::skill_allowed(&policies, &skill.id));
        skills
    }

    /// References: plugins → configuration (`opencode.config.reference` is a `post` plugin).
    pub async fn references(&self, location: &Location, global: &oc_util::Global) -> Vec<crate::reference::Info> {
        let configured =
            if self.builtin_enabled(&location.key, "opencode.config.reference") { crate::reference::list(location, global) } else { vec![] };
        if self.host.get().is_none() {
            return configured;
        }
        let out = self.transform(&location.key, "reference", json!({ "sources": [] })).await;
        let mut sources: indexmap::IndexMap<String, oc_config::fold::ReferenceSource> = indexmap::IndexMap::new();
        for item in out.get("sources").and_then(Value::as_array).cloned().unwrap_or_default() {
            let (Some(name), Some(source)) = (item.get(0).and_then(Value::as_str), item.get(1)) else { continue };
            match serde_json::from_value::<oc_config::fold::ReferenceSource>(source.clone()) {
                Ok(source) => {
                    sources.insert(name.to_string(), source);
                }
                Err(error) => tracing::warn!(%error, "invalid plugin reference source"),
            }
        }
        if sources.is_empty() {
            return configured;
        }
        let mut out = crate::reference::infos(sources.into_iter().collect(), global);
        for info in configured {
            match out.iter_mut().find(|i| i.name == info.name) {
                Some(existing) => *existing = info,
                None => out.push(info),
            }
        }
        out
    }

    /// MCP servers: configuration (`opencode.config.mcp`, a `pre` plugin) → plugins.
    pub async fn mcp_servers(
        &self,
        location: &Location,
        servers: indexmap::IndexMap<String, oc_mcp::config::ServerConfig>,
    ) -> indexmap::IndexMap<String, oc_mcp::config::ServerConfig> {
        let servers = if self.builtin_enabled(&location.key, "opencode.config.mcp") { servers } else { indexmap::IndexMap::new() };
        let policies = self.policies(location);
        let allowed = |servers: indexmap::IndexMap<String, oc_mcp::config::ServerConfig>| {
            servers.into_iter().filter(|(name, _)| crate::policy::mcp_allowed(&policies, name)).collect()
        };
        if self.host.get().is_none() {
            return allowed(servers);
        }
        let value = json!({
            "servers": servers.iter().map(|(name, config)| json!([name, config])).collect::<Vec<_>>(),
            "removed": [],
        });
        let out = self.transform(&location.key, "mcp", value).await;
        let mut result = indexmap::IndexMap::new();
        for item in out.get("servers").and_then(Value::as_array).cloned().unwrap_or_default() {
            let (Some(name), Some(config)) = (item.get(0).and_then(Value::as_str), item.get(1)) else { continue };
            match serde_json::from_value::<oc_mcp::config::ServerConfig>(config.clone()) {
                Ok(config) => {
                    result.insert(name.to_string(), config);
                }
                Err(error) => tracing::warn!(%error, server = name, "invalid plugin MCP server"),
            }
        }
        allowed(result)
    }

    /// `opencode.config.policy` statements of a location (none when the plugin is disabled).
    pub fn policies(&self, location: &Location) -> Vec<oc_config::policy::Statement> {
        if !self.builtin_enabled(&location.key, crate::policy::PLUGIN_ID) {
            return vec![];
        }
        crate::policy::statements(&location.entries)
    }

    /// [`Plugins::policies`] for the location last loaded under `key`.
    pub fn policies_at(&self, key: &str) -> Vec<oc_config::policy::Statement> {
        let location = self.locations.lock().get(key).and_then(|s| s.location.clone());
        location.map(|l| self.policies(&l)).unwrap_or_default()
    }

    /// Commands: the native `pre` value (`{commands: [{name, description?, execute}]}`) → plugins.
    /// Returns the transformed list of (name, description, handle).
    pub async fn commands(&self, key: &str, native: Vec<(String, Option<String>)>) -> Vec<(String, Option<String>, String)> {
        let value = json!({
            "commands": native
                .iter()
                .map(|(name, description)| {
                    let mut v = json!({ "name": name });
                    if let Some(d) = description {
                        v["description"] = json!(d);
                    }
                    v["execute"] = json!({ "$fn": command_handle(name) });
                    v
                })
                .collect::<Vec<_>>(),
        });
        let out = self.transform(key, "command", value).await;
        out.get("commands")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|item| {
                let name = item.get("name")?.as_str()?.to_string();
                let description = item.get("description").and_then(Value::as_str).map(String::from);
                let handle = oc_plugin::protocol::function_handle(item.get("execute")?).map(String::from).unwrap_or_default();
                Some((name, description, handle))
            })
            .collect()
    }

    /// Whether a plugin transforms `domain` at the location.
    pub fn transforms(&self, key: &str, domain: &str) -> bool {
        self.host.get().is_some_and(|h| h.transform_domains(&LocationRef::new(key.to_string())).iter().any(|d| d == domain))
    }

    /// Provider records → plugin `provider` transforms → records.
    pub async fn provider_records(
        &self,
        key: &str,
        records: indexmap::IndexMap<String, oc_ai::catalog::ProviderRecord>,
    ) -> indexmap::IndexMap<String, oc_ai::catalog::ProviderRecord> {
        if !self.transforms(key, "provider") {
            return records;
        }
        // Catalog values are large: reuse the decoded result while the registrations and the
        // native records are unchanged.
        let revision = self.revision(key, "provider");
        let input = {
            let mut hasher = HashWriter::default();
            for record in records.values() {
                let _ = serde_json::to_writer(&mut hasher, &record.provider);
                let _ = serde_json::to_writer(&mut hasher, &record.models);
                let _ = serde_json::to_writer(&mut hasher, &record.source_connection);
            }
            hasher.0.finish()
        };
        if let Some((r, h, CatalogValue::Providers(cached))) = self.catalog_cache.lock().get(&(key.to_string(), "provider".to_string()))
            && *r == revision
            && *h == input
        {
            return cached.clone();
        }
        let value = || json!({ "records": records.values().map(record_json).collect::<Vec<_>>() });
        let Some(mut out) = self.transform_catalog(key, "provider", input, value).await else { return records };
        let mut next = indexmap::IndexMap::new();
        let items = match out.get_mut("records").map(Value::take) {
            Some(Value::Array(items)) => items,
            _ => Vec::new(),
        };
        for item in items {
            // A record no transform touched comes back as `{"$same": id}` (the sidecar decodes
            // catalog records lazily): keep the native one.
            if let Some(id) = item.get("$same").and_then(Value::as_str) {
                if let Some(record) = records.get(id) {
                    next.insert(id.to_string(), record.clone());
                }
                continue;
            }
            match record_from_json(&item) {
                Some(record) => {
                    next.insert(record.provider.id.clone(), record);
                }
                None => tracing::warn!(record = %item, "invalid plugin provider record"),
            }
        }
        self.catalog_cache.lock().insert((key.to_string(), "provider".to_string()), (revision, input, CatalogValue::Providers(next.clone())));
        next
    }

    fn revision(&self, key: &str, domain: &str) -> u64 {
        self.host.get().map(|h| h.domain_revision(&LocationRef::new(key.to_string()), domain)).unwrap_or(0)
    }

    /// Available models (and the default) → plugin `model` transforms.
    pub async fn model_records(
        &self,
        key: &str,
        models: indexmap::IndexMap<String, indexmap::IndexMap<String, oc_ai::catalog::ModelInfo>>,
        default: Option<(String, String)>,
        records: indexmap::IndexMap<String, oc_ai::catalog::ProviderRecord>,
    ) -> (indexmap::IndexMap<String, indexmap::IndexMap<String, oc_ai::catalog::ModelInfo>>, Option<(String, String)>) {
        if !self.transforms(key, "model") {
            return (models, default);
        }
        let revision = self.revision(key, "model");
        let input = {
            let mut hasher = HashWriter::default();
            let _ = serde_json::to_writer(&mut hasher, &models);
            let _ = serde_json::to_writer(&mut hasher, &default);
            // The whole value sent to the sidecar, which keeps it under this fingerprint.
            for record in records.values() {
                let _ = serde_json::to_writer(&mut hasher, &record.provider);
                let _ = serde_json::to_writer(&mut hasher, &record.models);
                let _ = serde_json::to_writer(&mut hasher, &record.source_connection);
            }
            hasher.0.finish()
        };
        if let Some((r, h, CatalogValue::Models(cached, cached_default))) = self.catalog_cache.lock().get(&(key.to_string(), "model".to_string()))
            && *r == revision
            && *h == input
        {
            return (cached.clone(), cached_default.clone());
        }
        let value = || {
            let mut value = json!({
                "models": models.iter().map(|(pid, ms)| json!([pid, ms.values().collect::<Vec<_>>()])).collect::<Vec<_>>(),
                "providers": records.values().map(record_json).collect::<Vec<_>>(),
            });
            if let Some((p, m)) = &default {
                value["default"] = json!({ "providerID": p, "modelID": m });
            }
            value
        };
        let Some(mut out) = self.transform_catalog(key, "model", input, value).await else { return (models, default) };
        let mut next: indexmap::IndexMap<String, indexmap::IndexMap<String, oc_ai::catalog::ModelInfo>> = indexmap::IndexMap::new();
        let items = match out.get_mut("models").map(Value::take) {
            Some(Value::Array(items)) => items,
            _ => Vec::new(),
        };
        for mut item in items {
            let Some(pid) = item.get(0).and_then(Value::as_str).map(String::from) else { continue };
            // Models no transform touched come back as `{"$same": true}`: keep the native ones.
            if item.get(1).is_some_and(|m| m.get("$same").is_some()) {
                let same = models.get(&pid).cloned().unwrap_or_default();
                next.insert(pid, same);
                continue;
            }
            let entry = next.entry(pid).or_default();
            let models = match item.get_mut(1).map(Value::take) {
                Some(Value::Array(models)) => models,
                _ => Vec::new(),
            };
            for model in models {
                match serde_json::from_value::<oc_ai::catalog::ModelInfo>(model) {
                    Ok(model) => {
                        entry.insert(model.id.clone(), model);
                    }
                    Err(error) => tracing::warn!(%error, "invalid plugin model"),
                }
            }
        }
        let default = out
            .get("default")
            .and_then(|d| Some((d.get("providerID")?.as_str()?.to_string(), d.get("modelID")?.as_str()?.to_string())));
        self.catalog_cache.lock().insert((key.to_string(), "model".to_string()), (revision, input, CatalogValue::Models(next.clone(), default.clone())));
        (next, default)
    }

    /// Integrations: the native registrations (models.dev, provider and web-search plugins) →
    /// plugin `integration` transforms. Plugin OAuth methods authorize through their handles.
    /// The configured-provider registrations (`post`) follow in the caller.
    pub async fn integrations(&self, key: &str, integrations: &oc_ai::credentials::Integrations) {
        if !self.transforms(key, "integration") {
            return;
        }
        let refs = integrations.refs();
        let value = json!({
            "integrations": refs
                .iter()
                .map(|r| {
                    let methods = integrations.methods(&r.id);
                    json!({
                        "ref": { "id": r.id, "name": r.name },
                        "methods": methods.iter().map(|m| m.to_json()).collect::<Vec<_>>(),
                        "implementations": methods
                            .iter()
                            .filter(|m| m.kind() == "oauth")
                            .map(|m| json!({
                                "integrationID": r.id,
                                "method": m.to_json(),
                                "authorize": { "$fn": format!("rs:integration.{}.{}", r.id, m.id().unwrap_or_default()) },
                            }))
                            .collect::<Vec<_>>(),
                    })
                })
                .collect::<Vec<_>>(),
        });
        let out = self.transform(key, "integration", value).await;
        let items = out.get("integrations").and_then(Value::as_array).cloned().unwrap_or_default();
        let kept: HashSet<String> = items.iter().filter_map(|i| i.pointer("/ref/id").and_then(Value::as_str).map(String::from)).collect();
        for r in &refs {
            if !kept.contains(&r.id) {
                integrations.remove(&r.id);
            }
        }
        for item in items {
            let Some(id) = item.pointer("/ref/id").and_then(Value::as_str).map(String::from) else { continue };
            let name = item.pointer("/ref/name").and_then(Value::as_str).unwrap_or(&id).to_string();
            if refs.iter().find(|r| r.id == id).is_none_or(|r| r.name != name) {
                integrations.update(&id, |r| r.name = name.clone());
            }
            let methods: Vec<oc_ai::credentials::Method> =
                item.get("methods").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter_map(|m| serde_json::from_value(m).ok()).collect();
            for existing in integrations.methods(&id) {
                if !methods.iter().any(|m| m.same_slot(&existing)) {
                    integrations.remove_method(&id, &existing);
                }
            }
            let implementations = item.get("implementations").and_then(Value::as_array).cloned().unwrap_or_default();
            for method in methods {
                if method.kind() != "oauth" {
                    integrations.register_method(&id, method);
                    continue;
                }
                let implementation = implementations.iter().find(|i| i.pointer("/method/id").and_then(Value::as_str) == method.id());
                let handle = |k: &str| implementation.and_then(|i| i.get(k)).and_then(oc_plugin::protocol::function_handle).map(String::from);
                let Some(authorize) = handle("authorize").filter(|h| oc_plugin::protocol::is_plugin_handle(h)) else { continue };
                let oc_ai::credentials::Method::Oauth { id: method_id, label, form } = method else { continue };
                integrations.register_oauth(Arc::new(PluginOAuth {
                    plugins: self.me.clone(),
                    key: key.to_string(),
                    integration_id: id.clone(),
                    info: oc_ai::credentials::integration::OAuthMethodInfo { id: method_id, label, form },
                    authorize,
                    refresh: handle("refresh"),
                }));
            }
        }
    }

    /// Web search providers of location `key`: the built-in providers → plugin `websearch`
    /// transforms (a provider with an existing id replaces it in place; `default.set` sets the
    /// selection, which config still overrides). Providers a plugin adds execute through their
    /// handle; built-ins it kept (or wrapped) through `rs:websearch.<id>`.
    pub async fn search_providers(&self, key: &str) {
        let Some(base) = self.websearch.read().clone() else { return };
        let Some(ws) = self.websearch_for(key) else { return };
        let builtins = base.provider_list();
        if !self.transforms(key, "websearch") {
            ws.set_providers(builtins);
            return;
        }
        for provider in &builtins {
            let handle = format!("rs:websearch.{}", provider.info().id);
            let provider = provider.clone();
            let f: NativeFn = Arc::new(move |args: Vec<Value>| {
                let provider = provider.clone();
                Box::pin(async move {
                    let query = args.first().and_then(|a| a.get("query")).and_then(Value::as_str).unwrap_or_default().to_string();
                    match provider.execute(&query).await {
                        Ok(results) => Ok(serde_json::to_value(results).unwrap_or(Value::Null)),
                        Err(error) => Err(ApiError::new(error.message)),
                    }
                })
            });
            self.register_native(key, handle, f);
        }
        let value = json!({
            "providers": builtins
                .iter()
                .map(|p| {
                    let info = p.info();
                    json!({ "id": info.id, "name": info.name, "execute": { "$fn": format!("rs:websearch.{}", info.id) } })
                })
                .collect::<Vec<_>>(),
        });
        let out = self.transform(key, "websearch", value).await;
        let mut providers: Vec<Arc<dyn oc_tools::websearch::SearchProvider>> = Vec::new();
        for p in out.get("providers").and_then(Value::as_array).cloned().unwrap_or_default() {
            let (Some(id), Some(handle)) = (p.get("id").and_then(Value::as_str), p.get("execute").and_then(oc_plugin::protocol::function_handle)) else {
                continue;
            };
            let name = p.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
            let info = oc_tools::websearch::ProviderInfo { id: id.to_string(), name };
            let native = handle.strip_prefix("rs:websearch.").and_then(|n| builtins.iter().find(|b| b.info().id == n).cloned());
            let provider: Arc<dyn oc_tools::websearch::SearchProvider> = match native {
                // An untouched built-in keeps its own executor; a re-registered one runs it under its new id/name.
                Some(builtin) if builtin.info() == info => builtin,
                Some(builtin) => Arc::new(RenamedSearch { inner: builtin, info }),
                None => Arc::new(PluginSearch { plugins: self.me.clone(), key: key.to_string(), info, handle: handle.to_string() }),
            };
            providers.push(provider);
        }
        ws.set_providers(providers);
        if let Some(selection) = out.get("selection").and_then(oc_tools::websearch::Selection::from_json) {
            ws.set_config_selection(Some(selection));
        }
    }

    /// Calls a function handle: native (`rs:`) ones in process, plugin ones in the sidecar.
    async fn call_handle(&self, key: &str, handle: &str, args: Vec<Value>) -> Result<Value, oc_plugin::protocol::WireError> {
        let location = LocationRef::new(key.to_string());
        if !oc_plugin::protocol::is_plugin_handle(handle) {
            return self.invoke_native(&location, handle, args).await.map_err(|e| {
                let mut w = oc_plugin::protocol::WireError::new(e.message);
                w.name = e.name;
                w.data = e.data;
                w
            });
        }
        let host = self.host.get().ok_or_else(|| oc_plugin::protocol::WireError::new("Plugin host is not running"))?;
        host.invoke(&location, handle, args, None).await.map_err(|e| match e {
            oc_plugin::CallError::Remote(w) => *w,
            other => oc_plugin::protocol::WireError::new(other.to_string()),
        })
    }

    /// Location `key`'s VCS registry (`core/src/vcs.ts` State): the built-in providers (`pre`),
    /// then plugin `vcs` transforms. `vcs` is the location's detected VCS type.
    pub async fn vcs(&self, key: &str, vcs: Option<&str>) -> crate::vcs::Registry {
        let mut native = crate::vcs::Registry::native(vcs);
        if !self.builtin_enabled(key, "opencode.vcs.git") {
            native.providers.clear();
        }
        if !self.transforms(key, "vcs") {
            return native;
        }
        let mut providers = Vec::new();
        for provider in &native.providers {
            let id = provider.id().to_string();
            let mut entry = json!({ "id": id, "name": provider.name() });
            let ops = ["init", "info", "base", "branches", "status", "diff"];
            for op in ops {
                if (op == "init" && !provider.has_init()) || (op == "base" && !provider.has_base()) {
                    continue;
                }
                let handle = format!("rs:vcs.{id}.{op}");
                let provider = provider.clone();
                let f: NativeFn = Arc::new(move |args: Vec<Value>| {
                    let provider = provider.clone();
                    Box::pin(async move { native_vcs(provider.as_ref(), op, args.into_iter().next().unwrap_or(Value::Null)).await })
                });
                self.register_native(key, handle.clone(), f);
                entry[op] = json!({ "$fn": handle });
            }
            providers.push(entry);
        }
        let out = self.transform(key, "vcs", json!({ "providers": providers })).await;
        let mut registry = crate::vcs::Registry { providers: Vec::new(), selection: out.get("selection").and_then(Value::as_str).map(String::from) };
        for entry in out.get("providers").and_then(Value::as_array).cloned().unwrap_or_default() {
            let Some(id) = entry.get("id").and_then(Value::as_str) else { continue };
            let handle = |op: &str| entry.get(op).and_then(oc_plugin::protocol::function_handle).map(String::from);
            let (Some(info), Some(branches), Some(status), Some(diff)) = (handle("info"), handle("branches"), handle("status"), handle("diff")) else {
                continue;
            };
            // An untouched built-in keeps its native implementation.
            let untouched = native.get(id).filter(|n| {
                entry.get("name").and_then(Value::as_str) == Some(n.name())
                    && ["init", "info", "base", "branches", "status", "diff"].iter().all(|op| {
                        handle(op) == ((*op != "init" || n.has_init()) && (*op != "base" || n.has_base())).then(|| format!("rs:vcs.{id}.{op}"))
                    })
            });
            let provider: Arc<dyn crate::vcs::Provider> = match untouched {
                Some(native) => native,
                None => Arc::new(PluginVcs {
                    plugins: self.me.clone(),
                    key: key.to_string(),
                    id: id.to_string(),
                    name: entry.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                    init: handle("init"),
                    info,
                    base: handle("base"),
                    branches,
                    status,
                    diff,
                }),
            };
            registry.providers.push(provider);
        }
        registry
    }

    /// Location `key`'s worktree strategies (`core/src/worktree/strategies.ts` State): the git
    /// strategy, then plugin `worktree` transforms (an added strategy becomes the selected one).
    pub async fn worktree_strategies(&self, key: &str) -> crate::worktree::Strategies {
        let native = crate::worktree::Strategies::default();
        if !self.transforms(key, "worktree") {
            return native;
        }
        let mut strategies = Vec::new();
        for strategy in &native.strategies {
            let id = strategy.id().to_string();
            let mut entry = json!({ "id": id });
            for op in ["create", "remove", "list"] {
                let handle = format!("rs:worktree.{id}.{op}");
                let strategy = strategy.clone();
                let f: NativeFn = Arc::new(move |args: Vec<Value>| {
                    let strategy = strategy.clone();
                    Box::pin(async move { native_worktree(strategy.as_ref(), op, args.into_iter().next().unwrap_or(Value::Null)).await })
                });
                self.register_native(key, handle.clone(), f);
                entry[op] = json!({ "$fn": handle });
            }
            strategies.push(entry);
        }
        let out = self.transform(key, "worktree", json!({ "strategies": strategies, "selected": native.selected })).await;
        let mut result = crate::worktree::Strategies {
            strategies: Vec::new(),
            selected: out.get("selected").and_then(Value::as_str).unwrap_or(&native.selected).to_string(),
        };
        for entry in out.get("strategies").and_then(Value::as_array).cloned().unwrap_or_default() {
            let Some(id) = entry.get("id").and_then(Value::as_str) else { continue };
            let handle = |op: &str| entry.get(op).and_then(oc_plugin::protocol::function_handle).map(String::from);
            let (Some(create), Some(remove), Some(list)) = (handle("create"), handle("remove"), handle("list")) else { continue };
            let untouched = native.strategies.iter().find(|n| {
                n.id() == id && [("create", &create), ("remove", &remove), ("list", &list)].iter().all(|(op, h)| **h == format!("rs:worktree.{id}.{op}"))
            });
            let strategy: Arc<dyn crate::worktree::Strategy> = match untouched {
                Some(native) => native.clone(),
                None => Arc::new(PluginStrategy { plugins: self.me.clone(), key: key.to_string(), id: id.to_string(), create, remove, list }),
            };
            result.strategies.push(strategy);
        }
        result
    }

    /// Executes a plugin command handle (`Command.execute`).
    pub async fn execute_command(&self, key: &str, handle: &str, invocation: Value) -> Result<(), String> {
        let host = self.host.get().ok_or_else(|| "Plugin host is not running".to_string())?;
        host.invoke(&LocationRef::new(key.to_string()), handle, vec![invocation], None).await.map(|_| ()).map_err(|e| match e {
            oc_plugin::CallError::Remote(e) => e.message,
            other => other.to_string(),
        })
    }

    /// Tools: the native registry (built-ins and MCP tools, upstream's `pre` registrations) →
    /// plugins. Plugin tools execute in the sidecar; native tools a plugin edited keep their
    /// executor.
    pub async fn tools(&self, key: &str, registry: Registry) -> Registry {
        let Some(host) = self.host.get() else { return registry };
        let location = LocationRef::new(key.to_string());
        if !host.transform_domains(&location).iter().any(|d| d == "tool") {
            return registry;
        }
        let value = tool_value(&registry);
        // Native executors a plugin may call back (`rs:tool.<id>`).
        let me = self.me.clone();
        for (id, tool) in registry.list() {
            let me = me.clone();
            let name = id.clone();
            let f: NativeFn = Arc::new(move |args: Vec<Value>| {
                let me = me.clone();
                let tool = tool.clone();
                let name = name.clone();
                Box::pin(async move {
                    let plugins = me.upgrade().ok_or_else(|| ApiError::new("plugin host is shutting down"))?;
                    let input = args.first().cloned().unwrap_or(Value::Null);
                    let call = args.get(1).and_then(|c| c.get("id")).and_then(Value::as_str).unwrap_or_default().to_string();
                    let ctx = plugins.inflight.lock().get(&call).cloned().ok_or_else(|| ApiError::new(format!("Tool call context is unavailable: {name}")))?;
                    match oc_tools::registry::run(tool.as_ref(), &name, input, ctx).await {
                        Ok(result) => Ok(result_json(&result)),
                        Err(error) => Err(native_tool_error(error)),
                    }
                })
            });
            self.register_native(key, tool_handle(&id), f);
        }
        let out = self.transform(key, "tool", value.clone()).await;
        let native: HashMap<String, Arc<dyn Tool>> = registry.list().into_iter().collect();
        let sent: HashMap<String, Value> = value["tools"]
            .as_array()
            .map(|a| a.iter().filter_map(|t| Some((t.get("id")?.as_str()?.to_string(), t.clone()))).collect())
            .unwrap_or_default();
        let mut next = Registry::new();
        for namespace in out.get("namespaces").and_then(Value::as_array).cloned().unwrap_or_default() {
            let (Some(name), Some(description)) = (namespace.get("name").and_then(Value::as_str), namespace.get("description").and_then(Value::as_str)) else { continue };
            let _ = next.namespace(oc_tools::tool::Namespace { name: name.into(), description: description.into() });
        }
        for item in out.get("tools").and_then(Value::as_array).cloned().unwrap_or_default() {
            if item.get("id").and_then(Value::as_str).is_none() {
                continue;
            }
            let handle = item.get("execute").and_then(oc_plugin::protocol::function_handle).unwrap_or_default().to_string();
            let tool: Arc<dyn Tool> = if !oc_plugin::protocol::is_plugin_handle(&handle)
                && let Some(original) = handle.strip_prefix("rs:tool.").and_then(|n| native.get(n))
            {
                let before = handle.strip_prefix("rs:tool.").and_then(|n| sent.get(n));
                if before.is_some_and(|b| same_definition(b, &item)) {
                    original.clone()
                } else {
                    Arc::new(EditedTool::new(original.clone(), &item))
                }
            } else {
                Arc::new(PluginTool::new(self.me.clone(), key.to_string(), handle, &item))
            };
            let _ = next.add(tool);
        }
        next
    }

    /// `tool.list`: the current tool domain value of a location.
    pub async fn tool_list(&self, key: &str) -> Value {
        let source = self.tool_source.read().clone();
        match source {
            Some(source) => source.tool_value(key.to_string()).await.unwrap_or_else(|| json!({ "tools": [], "namespaces": [] })),
            None => json!({ "tools": [], "namespaces": [] }),
        }
    }

    /// Answers the host-service API methods that have no HTTP endpoint (`<domain>.reload`,
    /// `tool.list`, `event.publish`, `rpc.call`); `None` for everything else.
    pub async fn core_call(&self, request: &ApiRequest) -> Option<Result<Value, ApiError>> {
        let key = request.location.key();
        let method = request.method.as_str();
        if let Some(domain) = method.strip_suffix(".reload") {
            self.invalidate(&key, domain);
            if domain_event(domain).is_some() {
                self.notify_domain(&key, domain, Some(oc_schema::types::LocationRef { directory: request.location.directory.clone(), workspace_id: request.location.workspace_id.clone() }));
            }
            let _ = self.changes.send(DomainChange { key, domain: domain.to_string() });
            return Some(Ok(Value::Null));
        }
        match method {
            "tool.list" => Some(Ok(self.tool_list(&key).await)),
            "event.publish" => {
                let kind = request.input.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
                let data = request.input.get("data").cloned().unwrap_or(json!({}));
                self.bus.publish(&kind, data, Some(oc_schema::types::LocationRef { directory: request.location.directory.clone(), workspace_id: request.location.workspace_id.clone() }));
                Some(Ok(Value::Null))
            }
            "rpc.call" => {
                let id = request.input.get("rpcID").and_then(Value::as_str).unwrap_or_default();
                // The built-in goal plugin's RPC.
                if id == crate::goal::RPC_ID && self.builtin_enabled(&key, crate::goal::PLUGIN_ID) {
                    let method = request.input.get("method").and_then(Value::as_str).unwrap_or_default();
                    let input = request.input.get("input").cloned().unwrap_or(Value::Null);
                    return Some(crate::goal::rpc(&self.db, method, &input).map_err(|(kind, message)| ApiError::named(kind, message)));
                }
                Some(Err(ApiError::named("rpc.unavailable", format!("RPC is unavailable: {id}"))))
            }
            _ => None,
        }
    }

    fn kv(&self) -> &oc_storage::Database {
        &self.db
    }

    /// The server database.
    pub fn db(&self) -> &oc_storage::Database {
        &self.db
    }
}

/// Decoded catalog transform results.
enum CatalogValue {
    Providers(indexmap::IndexMap<String, oc_ai::catalog::ProviderRecord>),
    Models(indexmap::IndexMap<String, indexmap::IndexMap<String, oc_ai::catalog::ModelInfo>>, Option<(String, String)>),
}

/// An `io::Write` that hashes what is written (fingerprints without building the JSON text).
#[derive(Default)]
struct HashWriter(std::collections::hash_map::DefaultHasher);

impl std::io::Write for HashWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A provider record as the `provider` domain value carries it.
fn record_json(record: &oc_ai::catalog::ProviderRecord) -> Value {
    let mut v = json!({
        "provider": record.provider,
        "models": record.models.values().collect::<Vec<_>>(),
    });
    if let Some(key) = &record.source_connection {
        v["sourceConnection"] = match key.split_once(':') {
            Some(("credential", id)) => json!({ "type": "credential", "id": id }),
            Some(("env", name)) => json!({ "type": "env", "name": name }),
            _ => json!(key),
        };
    }
    v
}

fn record_from_json(v: &Value) -> Option<oc_ai::catalog::ProviderRecord> {
    let provider: oc_ai::catalog::ProviderInfo = serde_json::from_value(v.get("provider")?.clone()).ok()?;
    let mut models = indexmap::IndexMap::new();
    for model in v.get("models").and_then(Value::as_array).cloned().unwrap_or_default() {
        if let Ok(mut model) = serde_json::from_value::<oc_ai::catalog::ModelInfo>(model) {
            model.provider_id = provider.id.clone();
            models.insert(model.id.clone(), model);
        }
    }
    let source_connection = v.get("sourceConnection").and_then(|c| match c {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => match o.get("type").and_then(Value::as_str) {
            Some("credential") => Some(format!("credential:{}", o.get("id")?.as_str()?)),
            Some("env") => Some(format!("env:{}", o.get("name")?.as_str()?)),
            _ => None,
        },
        _ => None,
    });
    Some(oc_ai::catalog::ProviderRecord { provider, models, source_connection })
}

/// `Skill.Info` JSON.
pub fn skill_json(skill: &oc_config::sources::Skill) -> Value {
    let mut v = json!({ "id": skill.id, "name": skill.name });
    if let Some(d) = &skill.description {
        v["description"] = json!(d);
    }
    if let Some(a) = skill.autoinvoke {
        v["autoinvoke"] = json!(a);
    }
    v["path"] = json!(skill.path.display().to_string());
    v["content"] = json!(skill.content);
    v
}

fn skill_from_json(v: &Value) -> Option<oc_config::sources::Skill> {
    Some(oc_config::sources::Skill {
        id: v.get("id")?.as_str()?.to_string(),
        name: v.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
        description: v.get("description").and_then(Value::as_str).map(String::from),
        autoinvoke: v.get("autoinvoke").and_then(Value::as_bool),
        path: PathBuf::from(v.get("path").and_then(Value::as_str).unwrap_or_default()),
        content: v.get("content").and_then(Value::as_str).unwrap_or_default().to_string(),
    })
}

fn options_json(options: &ToolOptions) -> Value {
    let mut v = serde_json::Map::new();
    if let Some(ns) = &options.namespace {
        v.insert("namespace".into(), json!(ns));
    }
    if let Some(p) = &options.permission {
        v.insert("permission".into(), json!(p));
    }
    v.insert("codemode".into(), json!(options.codemode));
    if options.pinned {
        v.insert("pinned".into(), json!(true));
    }
    Value::Object(v)
}

fn options_from_json(v: Option<&Value>) -> ToolOptions {
    let get = |k: &str| v.and_then(|o| o.get(k));
    ToolOptions {
        namespace: get("namespace").and_then(Value::as_str).map(String::from),
        permission: get("permission").and_then(Value::as_str).map(String::from),
        codemode: get("codemode").and_then(Value::as_bool) != Some(false),
        pinned: get("pinned").and_then(Value::as_bool) == Some(true),
    }
}

/// The tool domain value of a registry (`{tools, namespaces}`).
pub fn tool_value(registry: &Registry) -> Value {
    let ctx = DefinitionContext::default();
    let tools: Vec<Value> = registry
        .list()
        .into_iter()
        .map(|(id, tool)| {
            let definition = tool.definition(&ctx);
            let mut v = json!({
                "id": id,
                "name": tool.name(),
                "description": definition.description,
                "input": definition.input_schema,
            });
            if let Some(output) = definition.output_schema {
                v["output"] = output;
            }
            v["options"] = options_json(&tool.options());
            v["execute"] = json!({ "$fn": tool_handle(&id) });
            v
        })
        .collect();
    let namespaces: Vec<Value> =
        registry.namespaces().iter().map(|n| json!({ "name": n.name, "description": n.description })).collect();
    json!({ "tools": tools, "namespaces": namespaces })
}

fn same_definition(before: &Value, after: &Value) -> bool {
    ["name", "description", "input", "output", "options"].iter().all(|k| before.get(*k) == after.get(*k))
}

fn result_json(result: &ToolResult) -> Value {
    let mut v = serde_json::Map::new();
    if let Some(output) = &result.output {
        v.insert("output".into(), output.clone());
    }
    v.insert("content".into(), serde_json::to_value(&result.content).unwrap_or(json!([])));
    if let Some(metadata) = &result.metadata {
        v.insert("metadata".into(), Value::Object(metadata.clone().into_iter().collect()));
    }
    Value::Object(v)
}

fn metadata_of(v: Option<&Value>) -> Option<Metadata> {
    v.and_then(Value::as_object).map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// A native tool whose registration a plugin edited (description, schema or options) without
/// replacing its executor.
pub struct EditedTool {
    inner: Arc<dyn Tool>,
    description: String,
    input: Value,
    output: Option<Value>,
    options: ToolOptions,
}

impl EditedTool {
    fn new(inner: Arc<dyn Tool>, item: &Value) -> Self {
        EditedTool {
            description: item.get("description").and_then(Value::as_str).unwrap_or_default().to_string(),
            input: item.get("input").cloned().unwrap_or(json!({ "type": "object", "properties": {} })),
            output: item.get("output").cloned(),
            options: options_from_json(item.get("options")),
            inner,
        }
    }
}

#[async_trait]
impl Tool for EditedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn options(&self) -> ToolOptions {
        self.options.clone()
    }
    fn definition(&self, ctx: &DefinitionContext) -> ToolDefinition {
        let mut definition = self.inner.definition(ctx);
        definition.description = self.description.clone();
        definition.input_schema = self.input.clone();
        definition.output_schema = self.output.clone();
        definition
    }
    fn advertised(&self, ctx: &DefinitionContext) -> bool {
        self.inner.advertised(ctx)
    }
    fn validate(&self, input: &Value) -> Vec<oc_tools::validate::Issue> {
        self.inner.validate(input)
    }
    fn prepare_input(&self, input: Value) -> Value {
        self.inner.prepare_input(input)
    }
    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        self.inner.execute(input, ctx).await
    }
}

/// A tool whose executor lives in the plugin sidecar (a plugin tool, or a native tool a plugin
/// wrapped). Input decoding, output encoding and content normalisation happen in the sidecar with
/// upstream's tool runtime.
pub struct PluginTool {
    plugins: Weak<Plugins>,
    key: String,
    handle: String,
    name: String,
    description: String,
    input: Value,
    output: Option<Value>,
    options: ToolOptions,
}

impl PluginTool {
    fn new(plugins: Weak<Plugins>, key: String, handle: String, item: &Value) -> Self {
        PluginTool {
            plugins,
            key,
            handle,
            name: item.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
            description: item.get("description").and_then(Value::as_str).unwrap_or_default().to_string(),
            input: item.get("input").cloned().unwrap_or(json!({ "type": "object", "properties": {} })),
            output: item.get("output").cloned(),
            options: options_from_json(item.get("options")),
        }
    }
}

#[async_trait]
impl Tool for PluginTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn options(&self) -> ToolOptions {
        self.options.clone()
    }
    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        let id = oc_tools::tool::effective_name(&self.name, self.options.namespace.as_deref());
        ToolDefinition::new(id, self.description.clone(), self.input.clone()).with_output(self.output.clone())
    }
    /// The sidecar validates with the plugin's own schema (upstream's runtime decode).
    fn validate(&self, _input: &Value) -> Vec<oc_tools::validate::Issue> {
        vec![]
    }
    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let plugins = self.plugins.upgrade().ok_or_else(|| ToolError::new("Plugin host is not running"))?;
        let host = plugins.host.get().ok_or_else(|| ToolError::new("Plugin host is not running"))?.clone();
        let context = json!({ "sessionID": ctx.session_id, "agent": ctx.agent, "messageID": ctx.message_id, "id": ctx.call_id });
        let call = ctx.call_id.clone();
        plugins.inflight.lock().insert(call.clone(), ctx.clone());
        let progress_ctx = ctx.clone();
        let progress: oc_plugin::ProgressFn = Arc::new(move |metadata: Value| {
            if let Some(m) = metadata_of(Some(&metadata)) {
                crate::bus::without_location(|| progress_ctx.progress(m));
            }
        });
        let location = LocationRef::new(self.key.clone());
        let result = tokio::select! {
            r = host.execute_tool(&location, &self.handle, input, context, Some(progress)) => r,
            _ = ctx.cancel.cancelled() => {
                plugins.inflight.lock().remove(&call);
                return Err(ToolError::interrupted());
            }
        };
        plugins.inflight.lock().remove(&call);
        match result {
            Ok(value) => {
                let content: Vec<ToolContent> = value
                    .get("content")
                    .cloned()
                    .and_then(|c| serde_json::from_value(c).ok())
                    .unwrap_or_default();
                Ok(ToolResult { output: value.get("output").cloned(), content, metadata: metadata_of(value.get("metadata")) })
            }
            Err(error) => {
                if let Some(decline) = error.declined {
                    return Err(declined_tool_error(decline));
                }
                let mut e = with_cause(ToolError::new(error.message), error.cause);
                if let Some(m) = metadata_of(error.metadata.as_ref()) {
                    e = e.with_metadata(m);
                }
                Err(e)
            }
        }
    }
}

/// Attaches the cause of a plugin `Tool.Error` (see [`oc_plugin::ToolError::cause`]) so the session error has
/// upstream's type.
pub fn with_cause(error: ToolError, cause: Option<(String, String)>) -> ToolError {
    let Some((kind, message)) = cause else { return error };
    let cause = match kind.as_str() {
        "permission.rejected" => oc_tools::tool::ErrorCause::PermissionRejected(message),
        "tool.execution" => oc_tools::tool::ErrorCause::Tool(Box::new(ToolError::new(message))),
        _ => oc_tools::tool::ErrorCause::Error(message),
    };
    ToolError { cause: Some(cause), ..error }
}

/// The session-side failure of a decline that crossed the sidecar.
pub fn declined_tool_error(decline: oc_plugin::Decline) -> ToolError {
    match decline {
        oc_plugin::Decline::Permission => ToolError::declined(),
        oc_plugin::Decline::Question => ToolError::question_dismissed(),
    }
}

/// How a native tool failure crosses to a plugin that called the tool: a decline keeps its name so
/// the plugin host turns it back into the defect upstream's tools die with.
fn native_tool_error(error: ToolError) -> ApiError {
    match error.kind {
        oc_tools::tool::ToolErrorKind::Declined | oc_tools::tool::ToolErrorKind::LocationClosed => {
            ApiError::named(oc_plugin::Decline::Permission.name(), oc_plugin::Decline::Permission.message())
        }
        oc_tools::tool::ToolErrorKind::QuestionDismissed => {
            ApiError::named(oc_plugin::Decline::Question.name(), oc_plugin::Decline::Question.message())
        }
        _ => {
            let mut e = ApiError::named("Tool.Error", error.model_message());
            e.data = error.metadata.map(|m| Value::Object(m.into_iter().collect()));
            e
        }
    }
}

/// Runs one operation of a native VCS provider for a plugin (`rs:vcs.<id>.<op>`).
async fn native_vcs(provider: &dyn crate::vcs::Provider, op: &str, input: Value) -> Result<Value, ApiError> {
    let scope = crate::vcs::Scope::from_json(&input).ok_or_else(|| ApiError::new("Invalid VCS scope"))?;
    let diff_error = |e: crate::vcs::ProviderError| match e {
        crate::vcs::ProviderError::Diff(e) => ApiError::named(oc_git::vcs::DiffError::TAG, e.message),
        crate::vcs::ProviderError::Other(m) => ApiError::new(m),
    };
    let json = |v: Result<Value, serde_json::Error>| v.map_err(|e| ApiError::new(e.to_string()));
    match op {
        "init" => provider.init(&scope).await.map(|_| Value::Null).map_err(ApiError::new),
        "info" => json(serde_json::to_value(provider.info(&scope).await.map_err(ApiError::new)?)),
        "base" => json(serde_json::to_value(provider.base(&scope).await.map_err(diff_error)?)),
        "branches" => json(serde_json::to_value(provider.branches(&scope, &crate::vcs::branch_options(&input)).await.map_err(ApiError::new)?)),
        "status" => json(serde_json::to_value(provider.status(&scope).await.map_err(ApiError::new)?)),
        _ => {
            let (mode, base, context) = crate::vcs::diff_args(&input);
            json(serde_json::to_value(provider.diff(&scope, mode, base.as_deref(), context).await.map_err(diff_error)?))
        }
    }
}

/// A VCS provider registered (or wrapped) by a plugin.
struct PluginVcs {
    plugins: Weak<Plugins>,
    key: String,
    id: String,
    name: String,
    init: Option<String>,
    info: String,
    base: Option<String>,
    branches: String,
    status: String,
    diff: String,
}

impl PluginVcs {
    async fn call(&self, handle: &str, input: Value) -> Result<Value, oc_plugin::protocol::WireError> {
        let plugins = self.plugins.upgrade().ok_or_else(|| oc_plugin::protocol::WireError::new("Plugin host is not running"))?;
        plugins.call_handle(&self.key, handle, vec![input]).await
    }

    async fn decode<T: serde::de::DeserializeOwned>(&self, handle: &str, input: Value) -> Result<T, String> {
        let value = self.call(handle, input).await.map_err(|e| e.message)?;
        serde_json::from_value(value).map_err(|e| e.to_string())
    }

    /// `review`: a provider's `Vcs.DiffError` passes through, anything else is generic.
    async fn review<T: serde::de::DeserializeOwned>(&self, handle: &str, input: Value) -> Result<T, crate::vcs::ProviderError> {
        let value = self.call(handle, input).await.map_err(|e| {
            // Only the core's own `Vcs.DiffError` (a wrapped native provider) passes through.
            if e.name.as_deref() == Some(oc_git::vcs::DiffError::TAG) && !oc_plugin::protocol::is_plugin_handle(handle) {
                crate::vcs::ProviderError::Diff(oc_git::vcs::DiffError { message: e.message })
            } else {
                crate::vcs::ProviderError::Other(e.message)
            }
        })?;
        serde_json::from_value(value).map_err(|e| crate::vcs::ProviderError::Other(e.to_string()))
    }
}

#[async_trait]
impl crate::vcs::Provider for PluginVcs {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn has_init(&self) -> bool {
        self.init.is_some()
    }
    fn has_base(&self) -> bool {
        self.base.is_some()
    }
    async fn init(&self, scope: &crate::vcs::Scope) -> Result<(), String> {
        let handle = self.init.as_deref().ok_or("unsupported")?;
        self.call(handle, scope.to_json()).await.map(|_| ()).map_err(|e| e.message)
    }
    async fn info(&self, scope: &crate::vcs::Scope) -> Result<oc_git::vcs::Info, String> {
        self.decode(&self.info, scope.to_json()).await
    }
    async fn base(&self, scope: &crate::vcs::Scope) -> Result<Option<oc_git::vcs::Base>, crate::vcs::ProviderError> {
        match &self.base {
            Some(handle) => self.review(handle, scope.to_json()).await,
            None => Ok(None),
        }
    }
    async fn branches(&self, scope: &crate::vcs::Scope, options: &oc_git::vcs::BranchOptions) -> Result<Vec<String>, String> {
        self.decode(&self.branches, crate::vcs::branches_input(scope, options)).await
    }
    async fn status(&self, scope: &crate::vcs::Scope) -> Result<Vec<oc_git::vcs::VcsFileStatus>, String> {
        self.decode(&self.status, scope.to_json()).await
    }
    async fn diff(
        &self,
        scope: &crate::vcs::Scope,
        mode: oc_git::vcs::Mode,
        base: Option<&str>,
        context: u64,
    ) -> Result<Vec<oc_schema::types::FileDiff>, crate::vcs::ProviderError> {
        self.review(&self.diff, crate::vcs::diff_input(scope, mode, base, context)).await
    }
}

/// Runs one operation of a native worktree strategy for a plugin (`rs:worktree.<id>.<op>`).
async fn native_worktree(strategy: &dyn crate::worktree::Strategy, op: &str, input: Value) -> Result<Value, ApiError> {
    let error = |e: crate::worktree::StrategyError| match e {
        crate::worktree::StrategyError::Worktree { message, force_required } => {
            let mut e = ApiError::named("Worktree.OperationError", message.clone());
            e.data = Some(json!({ "message": message, "forceRequired": force_required }));
            e
        }
        crate::worktree::StrategyError::Other(message) => ApiError::new(message),
    };
    let path = |k: &str| input.get(k).and_then(Value::as_str).map(std::path::PathBuf::from).ok_or_else(|| ApiError::new(format!("Missing {k}")));
    match op {
        "create" => {
            let created = strategy
                .create(&path("directory")?, &path("sourceDirectory")?, input.get("branch").and_then(Value::as_str))
                .await
                .map_err(error)?;
            Ok(json!({ "directory": created.display().to_string() }))
        }
        "remove" => {
            let force = input.get("force").and_then(Value::as_bool).unwrap_or(false);
            strategy.remove(&path("directory")?, force).await.map(|_| Value::Null).map_err(error)
        }
        _ => {
            let root = input.as_str().ok_or_else(|| ApiError::new("Expected a directory"))?;
            let entries = strategy.list(std::path::Path::new(root)).await.map_err(error)?;
            serde_json::to_value(entries).map_err(|e| ApiError::new(e.to_string()))
        }
    }
}

/// A worktree strategy registered (or wrapped) by a plugin.
struct PluginStrategy {
    plugins: Weak<Plugins>,
    key: String,
    id: String,
    create: String,
    remove: String,
    list: String,
}

impl PluginStrategy {
    async fn call(&self, handle: &str, input: Value) -> Result<Value, crate::worktree::StrategyError> {
        let plugins = self.plugins.upgrade().ok_or_else(|| crate::worktree::StrategyError::Other("Plugin host is not running".into()))?;
        plugins.call_handle(&self.key, handle, vec![input]).await.map_err(|e| {
            // `operationError` keeps git and operation errors as they are.
            if matches!(e.name.as_deref(), Some("Worktree.OperationError" | "Git.WorktreeError")) {
                let force_required = e.data.as_ref().and_then(|d| d.get("forceRequired")).and_then(Value::as_bool);
                crate::worktree::StrategyError::Worktree { message: e.message, force_required }
            } else {
                crate::worktree::StrategyError::Other(e.message)
            }
        })
    }
}

#[async_trait]
impl crate::worktree::Strategy for PluginStrategy {
    fn id(&self) -> &str {
        &self.id
    }
    async fn create(&self, directory: &std::path::Path, source: &std::path::Path, branch: Option<&str>) -> Result<std::path::PathBuf, crate::worktree::StrategyError> {
        let mut input = json!({ "sourceDirectory": source.display().to_string(), "directory": directory.display().to_string() });
        if let Some(branch) = branch {
            input["branch"] = json!(branch);
        }
        let out = self.call(&self.create, input).await?;
        // `Worktree.Info`: `{directory: AbsolutePath}`.
        out.get("directory")
            .and_then(Value::as_str)
            .filter(|d| std::path::Path::new(d).is_absolute())
            .map(std::path::PathBuf::from)
            .ok_or_else(|| crate::worktree::StrategyError::Other(format!("Expected an absolute path at [\"directory\"], got {out}")))
    }
    async fn remove(&self, directory: &std::path::Path, force: bool) -> Result<(), crate::worktree::StrategyError> {
        self.call(&self.remove, json!({ "directory": directory.display().to_string(), "force": force })).await.map(|_| ())
    }
    async fn list(&self, root: &std::path::Path) -> Result<Vec<oc_git::ListEntry>, crate::worktree::StrategyError> {
        let out = self.call(&self.list, json!(root.display().to_string())).await?;
        serde_json::from_value(out).map_err(|e| crate::worktree::StrategyError::Other(e.to_string()))
    }
}

/// The location config's `websearch` selection (`opencode.config.websearch`): `false` disables,
/// `"random"`, or a provider id.
pub fn config_websearch_selection(entries: &[oc_config::schema::Entry]) -> Option<oc_tools::websearch::Selection> {
    use oc_tools::websearch::Selection;
    oc_config::fold::websearch(entries).map(|w| match w {
        None => Selection::Disabled,
        Some("random") => Selection::Random,
        Some(p) => Selection::Provider(p.to_string()),
    })
}

/// A built-in web search provider a plugin registered under another id or name.
struct RenamedSearch {
    inner: Arc<dyn oc_tools::websearch::SearchProvider>,
    info: oc_tools::websearch::ProviderInfo,
}

#[async_trait]
impl oc_tools::websearch::SearchProvider for RenamedSearch {
    fn info(&self) -> oc_tools::websearch::ProviderInfo {
        self.info.clone()
    }

    async fn execute(&self, query: &str) -> Result<Vec<oc_tools::websearch::SearchResult>, oc_tools::websearch::ProviderError> {
        self.inner.execute(query).await
    }
}

/// A web search provider registered by a plugin.
struct PluginSearch {
    plugins: Weak<Plugins>,
    key: String,
    info: oc_tools::websearch::ProviderInfo,
    handle: String,
}

#[async_trait]
impl oc_tools::websearch::SearchProvider for PluginSearch {
    fn info(&self) -> oc_tools::websearch::ProviderInfo {
        self.info.clone()
    }

    async fn execute(&self, query: &str) -> Result<Vec<oc_tools::websearch::SearchResult>, oc_tools::websearch::ProviderError> {
        use oc_tools::websearch::ProviderError;
        let plugins = self.plugins.upgrade().ok_or_else(|| ProviderError::new("Plugin host is not running"))?;
        let host = plugins.host.get().ok_or_else(|| ProviderError::new("Plugin host is not running"))?.clone();
        let out = host
            .invoke(&LocationRef::new(self.key.clone()), &self.handle, vec![json!({ "query": query })], None)
            .await
            .map_err(|e| ProviderError::new(flow_error(e).message))?;
        serde_json::from_value(out).map_err(|e| ProviderError::new(e.to_string()))
    }
}

/// A plugin OAuth method: `authorize(answer)` runs in the sidecar inside a per-attempt scope; the
/// returned callback handle finishes the attempt.
struct PluginOAuth {
    plugins: Weak<Plugins>,
    key: String,
    integration_id: String,
    info: oc_ai::credentials::integration::OAuthMethodInfo,
    authorize: String,
    refresh: Option<String>,
}

fn flow_error(e: oc_plugin::CallError) -> oc_ai::credentials::integration::FlowError {
    oc_ai::credentials::integration::FlowError::new(match e {
        oc_plugin::CallError::Remote(e) => e.message,
        other => other.to_string(),
    })
}

fn oauth_value(v: Value) -> oc_ai::credentials::integration::FlowResult<oc_ai::credentials::OAuthValue> {
    match serde_json::from_value::<oc_ai::credentials::CredentialValue>(v) {
        Ok(oc_ai::credentials::CredentialValue::Oauth { method_id, refresh, access, expires, metadata }) => {
            Ok(oc_ai::credentials::OAuthValue { method_id, refresh, access, expires, metadata })
        }
        _ => Err(oc_ai::credentials::integration::FlowError::new("Invalid OAuth credential from plugin")),
    }
}

#[async_trait]
impl oc_ai::credentials::OAuthMethod for PluginOAuth {
    fn integration_id(&self) -> String {
        self.integration_id.clone()
    }

    fn info(&self) -> oc_ai::credentials::integration::OAuthMethodInfo {
        self.info.clone()
    }

    async fn authorize(
        &self,
        answer: oc_ai::credentials::form::FormAnswer,
        scope: tokio_util::sync::CancellationToken,
    ) -> oc_ai::credentials::integration::FlowResult<oc_ai::credentials::AuthorizeResult> {
        use oc_ai::credentials::integration::FlowError;
        let plugins = self.plugins.upgrade().ok_or_else(|| FlowError::new("Plugin host is not running"))?;
        let host = plugins.host.get().ok_or_else(|| FlowError::new("Plugin host is not running"))?.clone();
        let location = LocationRef::new(self.key.clone());
        let attempt = format!("oauth-{}", oc_schema::id::ascending("att"));
        let out = host.invoke(&location, &self.authorize, vec![Value::Object(answer)], Some(&attempt)).await.map_err(flow_error)?;
        // The attempt's resources (loopback servers) close with its scope.
        {
            let (host, location, attempt, scope) = (host.clone(), location.clone(), attempt.clone(), scope.clone());
            tokio::spawn(async move {
                scope.cancelled().await;
                host.close_scope(&location, &attempt).await;
            });
        }
        let callback = out.get("callback").and_then(oc_plugin::protocol::function_handle).map(String::from).ok_or_else(|| FlowError::new("OAuth authorization has no callback"))?;
        let url = out.get("url").and_then(Value::as_str).unwrap_or_default().to_string();
        let instructions = out.get("instructions").and_then(Value::as_str).unwrap_or_default().to_string();
        let expires_at = out.get("expiresAt").and_then(Value::as_u64);
        let callback = if out.get("mode").and_then(Value::as_str) == Some("code") {
            oc_ai::credentials::OAuthCallback::Code(Box::new(move |code: String| {
                Box::pin(async move { oauth_value(host.invoke(&location, &callback, vec![Value::String(code)], Some(&attempt)).await.map_err(flow_error)?) })
            }))
        } else {
            oc_ai::credentials::OAuthCallback::Auto(Box::pin(async move {
                oauth_value(host.invoke(&location, &callback, vec![], Some(&attempt)).await.map_err(flow_error)?)
            }))
        };
        Ok(oc_ai::credentials::AuthorizeResult { url, instructions, expires_at, callback })
    }

    fn can_refresh(&self) -> bool {
        self.refresh.is_some()
    }

    async fn refresh(&self, value: oc_ai::credentials::OAuthValue) -> oc_ai::credentials::integration::FlowResult<oc_ai::credentials::OAuthValue> {
        use oc_ai::credentials::integration::FlowError;
        let Some(handle) = &self.refresh else { return Ok(value) };
        let plugins = self.plugins.upgrade().ok_or_else(|| FlowError::new("Plugin host is not running"))?;
        let host = plugins.host.get().ok_or_else(|| FlowError::new("Plugin host is not running"))?.clone();
        let input = serde_json::to_value(value.into_value()).unwrap_or_default();
        oauth_value(host.invoke(&LocationRef::new(self.key.clone()), handle, vec![input], None).await.map_err(flow_error)?)
    }
}

/// The core's own [`PluginApi`]: storage, host-service methods and native handles. The backend
/// installs one that adds the HTTP-equivalent methods.
pub struct CoreApi {
    plugins: Weak<Plugins>,
}

impl CoreApi {
    pub fn new(plugins: Weak<Plugins>) -> Self {
        CoreApi { plugins }
    }

    fn plugins(&self) -> Result<Arc<Plugins>, ApiError> {
        self.plugins.upgrade().ok_or_else(|| ApiError::new("plugin host is shutting down"))
    }
}

fn kv_error(e: anyhow::Error) -> ApiError {
    ApiError::new(e.to_string())
}

#[async_trait]
impl PluginApi for CoreApi {
    async fn call(&self, request: ApiRequest) -> Result<Value, ApiError> {
        let plugins = self.plugins()?;
        match plugins.core_call(&request).await {
            Some(result) => result,
            None => Err(ApiError::unsupported(&request.method)),
        }
    }

    async fn invoke(&self, location: &LocationRef, handle: &str, args: Vec<Value>) -> Result<Value, ApiError> {
        self.plugins()?.invoke_native(location, handle, args).await
    }

    async fn storage_get(&self, key: &str) -> Result<Option<Value>, ApiError> {
        self.plugins()?.kv().kv_get(key).map_err(kv_error)
    }

    async fn storage_set(&self, key: &str, value: Value) -> Result<(), ApiError> {
        self.plugins()?.kv().kv_set(key, &value).map_err(kv_error)
    }

    async fn storage_remove(&self, key: &str) -> Result<(), ApiError> {
        self.plugins()?.kv().kv_remove(key).map_err(kv_error)
    }

    async fn storage_scan(&self, prefix: &str, after: Option<&str>, limit: Option<u64>) -> Result<StorageScan, ApiError> {
        let (entries, next) = self.plugins()?.kv().kv_scan(prefix, after, limit.map(|l| l as usize)).map_err(kv_error)?;
        Ok(StorageScan { entries: entries.into_iter().map(|(key, value)| StorageEntry { key, value }).collect(), next })
    }
}
