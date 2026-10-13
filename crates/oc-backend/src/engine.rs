//! The backend engine: core runtime + per-location service cache + process-local state that
//! upstream keeps in global services oc-core does not have yet.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use indexmap::IndexMap;
use oc_core::app::App;
use oc_core::inbox::Scope;
use oc_schema::types::SessionInfo;
use oc_server::ApiError;
use parking_lot::Mutex;

use crate::locations::{Loc, Locations};

pub struct Engine {
    pub app: Arc<App>,
    /// The core session runner (model catalog, credentials, execution). `None` in tests / when the
    /// CLI runs without providers: execution endpoints then only admit work.
    pub runner: Option<Arc<oc_core::runner::Engine>>,
    own_models: Option<oc_core::models::Models>,
    pub locations: Locations,
    /// `WebSearch` service (built-in providers over the `kv` table and credentials).
    pub websearch: Arc<oc_tools::websearch::WebSearch>,
    /// Persistent PTY service (built on first use, see [`Engine::persistent`]).
    persistent: tokio::sync::OnceCell<oc_persistent_pty::PersistentPty>,
    persistent_options: Mutex<Option<oc_persistent_pty::Options>>,
    /// Well-known integration sources (`WellKnown.Service`) over the `kv` table.
    pub wellknown: oc_ai::credentials::wellknown::WellKnown,
    /// Shell command service (the runner's, else one wired to the bus by `default_services`).
    own_shell: Option<Arc<oc_tools::shell::service::ShellService>>,
    /// Shell id → owning location key.
    pub shell_owner: Mutex<HashMap<String, String>>,
    /// PTY id → owning location key (PTY services are per location upstream).
    pub pty_owner: Mutex<HashMap<String, String>>,
}

/// `KvStore` over the `kv` table.
struct DbKv(oc_storage::Database);

#[async_trait::async_trait]
impl oc_ai::credentials::KvStore for DbKv {
    async fn get(&self, key: &str) -> anyhow::Result<Option<serde_json::Value>> {
        self.0.kv_get(key)
    }
    async fn set(&self, key: &str, value: serde_json::Value) -> anyhow::Result<()> {
        self.0.kv_set(key, &value)
    }
    async fn remove(&self, key: &str) -> anyhow::Result<()> {
        self.0.kv_remove(key)
    }
}

fn websearch(app: &Arc<App>) -> Arc<oc_tools::websearch::WebSearch> {
    let store = Arc::new(oc_core::services::WebSearchKv { app: app.clone() });
    let ws = oc_tools::websearch::WebSearch::new(store, reqwest::Client::new());
    ws.set_user_agent(format!("opencode/{}/{}/opencode", oc_util::version::CHANNEL, app.version));
    let ws = Arc::new(ws);
    app.plugins.add_websearch(ws.clone());
    ws
}

impl Engine {
    /// Engine without a session runner (its own model/credential services).
    pub fn new(app: Arc<App>) -> Arc<Engine> {
        let app_ref = app.clone();
        let models = oc_core::models::Models::new(app.db.clone(), app.bus.clone(), oc_util::version::CHANNEL, &app.version);
        models.set_plugins(app.plugins.clone());
        Arc::new(Engine {
            locations: Locations::new(app.clone(), None),
            app,
            runner: None,
            own_models: Some(models),
            own_shell: Some(oc_core::services::default_services(&app_ref).shell),
            shell_owner: Mutex::new(HashMap::new()),
            websearch: websearch(&app_ref),
            wellknown: oc_ai::credentials::wellknown::WellKnown::new(
                reqwest::Client::new(),
                Arc::new(DbKv(app_ref.db.clone())),
                oc_ai::credentials::process_env(),
            ),
            pty_owner: Mutex::new(HashMap::new()),
            persistent: tokio::sync::OnceCell::new(),
            persistent_options: Mutex::new(None),
        })
    }

    /// Engine over the core session runner (shares its models, credentials and location cache).
    pub fn with_runner(runner: Arc<oc_core::runner::Engine>) -> Arc<Engine> {
        let app = runner.app.clone();
        let app_ref = app.clone();
        Arc::new(Engine {
            locations: Locations::new(app.clone(), Some(runner.clone())),
            app,
            runner: Some(runner),
            own_models: None,
            own_shell: None,
            shell_owner: Mutex::new(HashMap::new()),
            websearch: websearch(&app_ref),
            wellknown: oc_ai::credentials::wellknown::WellKnown::new(
                reqwest::Client::new(),
                Arc::new(DbKv(app_ref.db.clone())),
                oc_ai::credentials::process_env(),
            ),
            pty_owner: Mutex::new(HashMap::new()),
            persistent: tokio::sync::OnceCell::new(),
            persistent_options: Mutex::new(None),
        })
    }

    /// Options for the persistent PTY service (e.g. a restart handoff taken by the CLI at startup).
    /// Must be called before the service is first used.
    pub fn set_persistent_pty_options(&self, options: oc_persistent_pty::Options) {
        *self.persistent_options.lock() = Some(options);
    }

    /// The global persistent PTY service. Built once: with the configured options or, by default,
    /// the restart handoff from `OPENCODE_PTY_HANDOFF`; its `persistent-pty.added/removed` events
    /// are forwarded to the bus.
    pub async fn persistent(&self) -> &oc_persistent_pty::PersistentPty {
        self.persistent
            .get_or_init(|| async {
                let options = self.persistent_options.lock().take().unwrap_or_else(|| oc_persistent_pty::Options {
                    handoff: oc_persistent_pty::handoff::take_from_env(),
                    ..Default::default()
                });
                let pty = oc_persistent_pty::PersistentPty::new(&self.app.global, options).await;
                let mut rx = pty.subscribe_events();
                let bus = self.app.bus.clone();
                tokio::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(e) => {
                                bus.publish(e.event_type(), e.properties(), None);
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(_) => break,
                        }
                    }
                });
                pty
            })
            .await
    }

    /// Restart handoff (`prepare_handoff` on the daemon owner connection) for the CLI's service
    /// stop/restart (spec 01 §7.5); also served by `POST /api/experimental/persistent-pty/handoff`.
    pub async fn prepare_pty_handoff(&self) -> Result<Option<oc_persistent_pty::handoff::Handoff>, oc_persistent_pty::Error> {
        self.persistent().await.handoff().await
    }

    /// The shell command service.
    pub fn shell(&self) -> &Arc<oc_tools::shell::service::ShellService> {
        match &self.runner {
            Some(r) => &r.shell,
            None => self.own_shell.as_ref().expect("shell service"),
        }
    }

    /// Model catalog + integrations/credentials service.
    pub fn models(&self) -> &oc_core::models::Models {
        match &self.runner {
            Some(r) => &r.models,
            None => self.own_models.as_ref().expect("models"),
        }
    }

    /// The location's model catalog (`Provider`/`Model` services).
    pub async fn catalog(&self, loc: &Loc) -> Result<oc_ai::catalog::Catalog, ApiError> {
        self.models()
            .catalog_at(&loc.location.entries, Some(&loc.key))
            .await
            .map_err(|e| ApiError::service_unavailable(e.to_string(), Some("provider".into())))
    }

    /// MCP manager of a location (runner only).
    pub async fn mcp(&self, loc: &Loc) -> Option<Arc<oc_mcp::McpManager>> {
        match &self.runner {
            Some(r) => Some(r.mcp(&loc.location).await),
            None => None,
        }
    }

    /// Commands of a location: built-ins and MCP prompt commands, plugin transforms, then
    /// configured commands.
    pub async fn commands(&self, loc: &Loc) -> IndexMap<String, (Option<String>, crate::catalog::CommandDef)> {
        let mut native = self.native_commands(loc).await;
        let plugins = &self.app.plugins;
        // Built-in command plugins removed by configuration (`-opencode.command`).
        if !plugins.builtin_enabled(&loc.key, "opencode.command") {
            native.retain(|_, (_, def)| !matches!(def, crate::catalog::CommandDef::Init | crate::catalog::CommandDef::Review));
        }
        let mut out = if plugins.host().is_some() {
            let list: Vec<(String, Option<String>)> = native.iter().map(|(n, (d, _))| (n.clone(), d.clone())).collect();
            let mut out = IndexMap::new();
            for (name, description, handle) in plugins.commands(&loc.key, list).await {
                let def = match handle.strip_prefix("rs:command.").and_then(|n| native.shift_remove(n)) {
                    Some((_, def)) => def,
                    None if oc_plugin::protocol::is_plugin_handle(&handle) => crate::catalog::CommandDef::Plugin(handle),
                    None => continue,
                };
                out.insert(name, (description, def));
            }
            out
        } else {
            native
        };
        if plugins.builtin_enabled(&loc.key, "opencode.config.command") {
            crate::catalog::configured(loc, &mut out);
        }
        out
    }

    /// The native commands registered before plugins (built-ins, MCP prompts).
    pub async fn native_commands(&self, loc: &Loc) -> IndexMap<String, (Option<String>, crate::catalog::CommandDef)> {
        let prompts = match self.mcp(loc).await {
            Some(m) => m.prompts(),
            None => vec![],
        };
        crate::catalog::native(&prompts)
    }

    /// Agents of a location after plugin transforms and configuration.
    pub async fn agents(&self, loc: &Loc) -> oc_core::agent::Agents {
        self.app.plugins.agents(&loc.location, &self.app.global.home).await
    }

    /// Skills of a location after plugin transforms and configuration.
    pub async fn skills(&self, loc: &Loc) -> Vec<oc_config::sources::Skill> {
        crate::catalog::skills(loc, &self.app.version, &self.app.global, &self.app.plugins).await
    }

    /// Environment variables set for a session through `PUT /api/session/{id}/environment`.
    pub fn environment(&self, session_id: &str) -> Option<std::collections::HashMap<String, String>> {
        self.app.environments.lock().get(session_id).cloned()
    }

    /// `Session.get` → 404 `SessionNotFoundError`.
    pub fn session(&self, id: &str) -> Result<SessionInfo, ApiError> {
        match self.app.db.session_get(id) {
            Ok(Some(s)) => Ok(s),
            Ok(None) => Err(ApiError::session_not_found(id)),
            Err(e) => Err(ApiError::defect(e.to_string())),
        }
    }

    /// Session-scoped (`S`) middleware: the session and its location's services.
    pub async fn session_scope(&self, id: &str) -> Result<(SessionInfo, Arc<Loc>), ApiError> {
        let session = self.session(id)?;
        let loc = self.locations.get(&session.location.directory).await?;
        Ok((session, loc))
    }

    /// `execution.wake(sessionID)` when the runner is installed.
    pub fn wake(&self, session_id: &str, scope: Scope) {
        match self.app.coordinator.get() {
            Some(c) => c.wake(session_id, scope),
            None => tracing::debug!(session_id, "no session runner installed; wake skipped"),
        }
    }

    pub fn is_active(&self, session_id: &str) -> bool {
        self.app.coordinator.get().is_some_and(|c| c.is_active(session_id))
    }

    pub fn home(&self) -> &Path {
        &self.app.global.home
    }
}

impl Engine {
    /// `PluginUpdate.check(target, {refresh: true})`: whether a newer package is available.
    pub async fn plugin_outdated(&self, target: &str) -> bool {
        let Some(host) = self.app.plugins.host() else { return false };
        let Ok(bun) = host.bun().await else { return false };
        match host.npm().check(&bun, target).await {
            Ok(outdated) => outdated,
            Err(error) => {
                tracing::warn!(%error, target, "plugin update check failed");
                false
            }
        }
    }

    /// `PluginUpdate.update(target)`: installs the latest revision of a package plugin.
    pub async fn plugin_install_update(&self, target: &str) -> Result<(), String> {
        let host = self.app.plugins.host().ok_or_else(|| "Plugin host is not running".to_string())?;
        let bun = host.bun().await?;
        host.npm().update(&bun, target).await.map(|_| ()).map_err(|e| e.to_string())
    }
}
