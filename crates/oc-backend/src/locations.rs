//! Location service cache (`LocationServiceMap`): one lazily built service graph per requested
//! directory, keyed by its canonical form (`LocationServiceMap.canonical`: normalized, no trailing
//! separator), so `/p/` and `/p` share one location.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use oc_core::app::App;
use oc_core::location::Location;
use oc_git::{Git, SnapshotLocation, Snapshots};
use oc_server::ApiError;
use parking_lot::Mutex;
use serde_json::json;
use tokio::sync::OnceCell;

/// Ephemeral `*.updated` notifications upstream emits when a location boots (see `Locations::build`).
const BOOT_NOTIFICATIONS: [&str; 9] = [
    "integration.updated",
    "provider.updated",
    "model.updated",
    "agent.updated",
    "command.updated",
    "skill.updated",
    "websearch.updated",
    "reference.updated",
    "plugin.updated",
];

/// Services of one loaded location.
pub struct Loc {
    /// The canonical requested directory (cache key, `location.get`'s directory and boot events' location).
    pub key: String,
    pub location: Arc<Location>,
    pub snapshots: Snapshots,
    /// Cached VCS info and the branch watcher.
    pub vcs_state: crate::vcs_watch::VcsState,
}

impl Loc {
    /// Canonical (realpath) location directory.
    pub fn directory(&self) -> &Path {
        &self.location.directory
    }

    /// Project worktree root (`location.project.directory`).
    pub fn project_directory(&self) -> &Path {
        &self.location.project.directory
    }

    pub fn project_id(&self) -> &str {
        &self.location.project.id
    }

    pub fn vcs(&self) -> Option<&'static str> {
        self.location.project.vcs.as_ref().map(|v| v.kind())
    }

    pub fn git_vcs(&self) -> Option<oc_git::GitVcs> {
        (self.vcs() == Some("git")).then(|| oc_git::GitVcs::new(self.directory(), self.project_directory()))
    }
}

type Cell = Arc<OnceCell<Arc<Loc>>>;

/// Computes a gated domain's projection for a location (`crate::notify`).
pub type LocProjector = Arc<dyn Fn(Arc<Loc>, &'static str) -> futures::future::BoxFuture<'static, Option<String>> + Send + Sync>;

/// Locations whose configuration changed: their services are rebuilt, and the rebuild notifies like
/// upstream's in-place State reload instead of a fresh boot. Holds the replaced services, if any.
type Reloading = Arc<Mutex<HashMap<String, Option<Arc<Loc>>>>>;

const ACTIVATE_INTERVAL: Duration = Duration::from_secs(60);

pub struct Locations {
    app: Arc<App>,
    runner: Option<Arc<oc_core::runner::Engine>>,
    git: Git,
    map: Arc<Mutex<IndexMap<String, Cell>>>,
    activated: Mutex<HashMap<String, Instant>>,
    /// Runs before a location is built (the models.dev refresh starts when the first location boots).
    on_build: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    reloading: Reloading,
    projector: Mutex<Option<LocProjector>>,
}

fn is_dir(p: &str) -> bool {
    std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

impl Locations {
    pub fn new(app: Arc<App>, runner: Option<Arc<oc_core::runner::Engine>>) -> Self {
        let map: Arc<Mutex<IndexMap<String, Cell>>> = Arc::new(Mutex::new(IndexMap::new()));
        let reloading: Reloading = Arc::new(Mutex::new(HashMap::new()));
        // Core reloads configuration and announces `config.updated` for a location: drop the cached
        // services so agents and snapshots are rebuilt on next use.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let mut rx = app.bus.subscribe();
            let weak = Arc::downgrade(&map);
            let reloading = reloading.clone();
            handle.spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(e) if e.kind == "config.updated" => {
                            let Some(map) = weak.upgrade() else { break };
                            if let Some(dir) = e.location.as_ref().map(|l| oc_core::location::canonical_directory(&l.directory)) {
                                let previous = map.lock().shift_remove(&dir);
                                if let Some(previous) = previous {
                                    reloading.lock().insert(dir, previous.get().cloned());
                                }
                            }
                        }
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if weak.strong_count() == 0 {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
        // Plugins changed a location's VCS providers: the Vcs State notify refreshes the info
        // (once the boot refresh ran).
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let mut rx = app.plugins.subscribe_changes();
            let (weak, app) = (Arc::downgrade(&map), app.clone());
            handle.spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(change) if change.domain == "vcs" => {
                            let Some(map) = weak.upgrade() else { break };
                            let loc = map.lock().get(&change.key).and_then(|c| c.get().cloned());
                            if let Some(loc) = loc.filter(|l| l.vcs_state.cached().is_some()) {
                                crate::vcs_watch::refresh(&app, &loc).await;
                            }
                        }
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if weak.strong_count() == 0 {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
        Locations {
            app,
            runner,
            git: Git::new(),
            map,
            activated: Mutex::new(HashMap::new()),
            on_build: Mutex::new(None),
            reloading,
            projector: Mutex::new(None),
        }
    }

    /// Installs the domain projections the boot and reload notifications compare.
    pub fn set_projector(&self, projector: LocProjector) {
        *self.projector.lock() = Some(projector);
    }

    /// The services of a loaded location, without building them.
    pub fn loaded(&self, directory: &str) -> Option<Arc<Loc>> {
        self.map.lock().get(&oc_core::location::canonical_directory(directory)).and_then(|c| c.get().cloned())
    }

    pub fn git(&self) -> &Git {
        &self.git
    }

    /// Loaded location keys (`debug.location.list`), in load order.
    pub fn keys(&self) -> Vec<String> {
        self.map.lock().iter().filter(|(_, c)| c.initialized()).map(|(k, _)| k.clone()).collect()
    }

    /// Get or build the services for `directory`.
    pub async fn get(&self, directory: &str) -> Result<Arc<Loc>, ApiError> {
        let canonical = oc_core::location::canonical_directory(directory);
        let directory = canonical.as_str();
        let cell = self.map.lock().entry(directory.to_string()).or_default().clone();
        let result = cell.get_or_try_init(|| self.build(directory)).await.cloned();
        if result.is_err() {
            let mut map = self.map.lock();
            if map.get(directory).is_some_and(|c| Arc::ptr_eq(c, &cell)) {
                map.shift_remove(directory);
            }
        }
        result
    }

    /// Run `f` before every location build.
    pub fn on_build(&self, f: Arc<dyn Fn() + Send + Sync>) {
        *self.on_build.lock() = Some(f);
    }

    async fn build(&self, directory: &str) -> Result<Arc<Loc>, ApiError> {
        if !is_dir(directory) {
            return Err(ApiError::location_not_found(directory));
        }
        let hook = self.on_build.lock().clone();
        if let Some(hook) = hook {
            hook();
        }
        let loaded = match &self.runner {
            Some(r) => r.location(directory).await,
            None => Location::load(&self.app.global, &self.app.bus, Path::new(directory)).await.map(Arc::new),
        };
        let location = loaded.map_err(|e| {
            if is_dir(directory) { ApiError::defect(format!("location load failed: {e:#}")) } else { ApiError::location_not_found(directory) }
        })?;
        // Plugins activate asynchronously (`PluginSupervisor`); the runner already started them
        // for its own location load.
        self.app.plugins.ensure(&location);
        let snapshots = Snapshots::new(
            self.git.clone(),
            SnapshotLocation {
                data: self.app.global.data.clone(),
                project_id: location.project.id.clone(),
                project_directory: location.project.directory.clone(),
                directory: location.directory.clone(),
                vcs: location.project.vcs.as_ref().map(|v| v.kind().to_string()),
            },
        );
        let loc = Arc::new(Loc { key: directory.to_string(), location, snapshots, vcs_state: Default::default() });
        let at = oc_schema::types::LocationRef { directory: directory.to_string(), workspace_id: None };
        // A configuration reload: the rebuilt services notify like an in-place reload, and keep the VCS info.
        let reload = self.reloading.lock().remove(directory);
        if let Some(Some(previous)) = &reload {
            loc.vcs_state.seed(previous.vcs_state.cached());
        }
        let notify = Notify {
            reload: reload.is_some(),
            previous: reload.flatten(),
            projector: self.projector.lock().clone(),
            runner: self.runner.clone(),
        };
        // `LocationLifecycle` activates the project inside the location's service scope, so the
        // event carries the location (core/src/location-lifecycle.ts:32, bus.ts publish).
        self.activate(loc.project_id(), Some(at.clone()));
        if oc_core::plugins::Plugins::has_operations(&loc.location) && !self.app.plugins.activated(&loc.key) {
            // Configured plugins: the boot batch ends when their activation settles.
            let (app, git, loc2, at2) = (self.app.clone(), self.git.clone(), loc.clone(), at.clone());
            tokio::spawn(async move {
                app.plugins.await_activation(&loc2.key).await;
                boot_notifications(&app, &git, &loc2, &at2, notify).await;
                app.plugins.rebuild(&loc2.location).await;
            });
            return Ok(loc);
        }
        boot_notifications(&self.app, &self.git, &loc, &at, notify).await;
        Ok(loc)
    }
}

/// How a built location announces itself.
struct Notify {
    /// Rebuilt after a configuration change rather than booted.
    reload: bool,
    /// The services the reload replaced: their projections are the baseline when none was recorded yet.
    previous: Option<Arc<Loc>>,
    projector: Option<LocProjector>,
    /// Owns the location's MCP servers, which a boot starts.
    runner: Option<Arc<oc_core::runner::Engine>>,
}

/// A configuration reload's notifications (upstream reloads each domain State in place): provider and model
/// always notify (twice: once for the configuration, once for the provider plugins' rebuild), every other
/// domain only when its projection changed (`State.changes`).
const RELOAD_NOTIFICATIONS: [&str; 11] = [
    "integration",
    "provider",
    "model",
    "provider",
    "model",
    "command",
    "agent",
    "skill",
    "websearch",
    "reference",
    "plugin",
];

/// The `*.updated` notifications of a location's boot batch (or configuration reload), then the branch watcher.
async fn boot_notifications(app: &Arc<App>, git: &Git, loc: &Arc<Loc>, at: &oc_schema::types::LocationRef, notify: Notify) {
    if notify.reload {
        crate::vcs_watch::refresh(app, loc).await;
        for domain in RELOAD_NOTIFICATIONS {
            let gated = oc_core::plugins::GATED_DOMAINS.contains(&domain);
            if gated && let Some(projector) = &notify.projector {
                // A location's first notify always fires (its boot); the replaced services' projection is what
                // it announced, unless a later notify recorded one.
                if !app.plugins.has_projection(&loc.key, domain)
                    && let Some(previous) = &notify.previous
                    && let Some(baseline) = projector(previous.clone(), domain).await
                {
                    app.plugins.note_projection(&loc.key, domain, baseline);
                }
                let changed = match projector(loc.clone(), domain).await {
                    Some(projection) => app.plugins.note_projection(&loc.key, domain, projection),
                    None => true,
                };
                if !changed {
                    continue;
                }
            }
            app.bus.publish(&format!("{domain}.updated"), json!({}), Some(at.clone()));
        }
        let (app, git, loc) = (app.clone(), git.clone(), loc.clone());
        tokio::spawn(async move { crate::vcs_watch::watch(&app, &git, &loc).await });
        return;
    }
    // Booting a location activates the built-in plugins inside one `State.batch`; when the batch
    // ends every State they touched notifies once, in first-change order, and the plugin supervisor
    // then announces its inventory (core/src/plugin.ts `activate`, core/src/state.ts `batch`). With
    // the default plugin set those are exactly these nine events. The MCP State notifies first
    // (`opencode.config.mcp` is the first plugin to touch a State that notifies): it registers the
    // configured servers and starts connecting them, announcing each `pending` (or `disabled`) before
    // the burst (core/src/mcp/index.ts `reconcile`).
    if let Some(runner) = &notify.runner {
        runner.mcp(&loc.location).await;
    }
    for kind in BOOT_NOTIFICATIONS {
        // The git VCS provider registers in the same batch; the Vcs State notify (after models,
        // before agents) refreshes the info and announces the branch when it changed from unknown
        // (core/src/vcs.ts `refresh`).
        if kind == "agent.updated" {
            crate::vcs_watch::refresh(app, loc).await;
        }
        app.bus.publish(kind, json!({}), Some(at.clone()));
    }
    // A booted location's first notify always fires: whatever was recorded for an earlier instance is stale.
    app.plugins.forget_projections(&loc.key);
    // `LocationWatcher` starts once plugins have activated, in the background.
    let (app, git, loc) = (app.clone(), git.clone(), loc.clone());
    tokio::spawn(async move { crate::vcs_watch::watch(&app, &git, &loc).await });
}

impl Locations {

    /// `Project.activate`: records activity at most once per minute per project.
    pub fn activate(&self, project_id: &str, location: Option<oc_schema::types::LocationRef>) {
        {
            let mut activated = self.activated.lock();
            let now = Instant::now();
            if activated.get(project_id).is_some_and(|t| now.duration_since(*t) < ACTIVATE_INTERVAL) {
                return;
            }
            activated.insert(project_id.to_string(), now);
        }
        let now = crate::util::now_ms();
        let updated = self
            .app
            .db
            .with(|c| c.execute("UPDATE project SET time_active = ? WHERE id = ?", rusqlite::params![now, project_id]))
            .unwrap_or(0);
        if updated > 0
            && let Ok(Some(info)) = oc_core::project::get(&self.app.bus, project_id) {
                self.app.bus.publish("project.updated", info, location);
            }
    }

    /// Shut a location down (`LocationLifecycle.shutdown`): announce `location.shutdown`.
    fn shutdown(&self, directory: &str) {
        self.app.bus.publish(
            "location.shutdown",
            json!({}),
            Some(oc_schema::types::LocationRef { directory: directory.to_string(), workspace_id: None }),
        );
    }

    /// Drop the cached services of a location without announcing a shutdown (configuration reload).
    pub fn forget(&self, directory: &str) {
        self.map.lock().shift_remove(&oc_core::location::canonical_directory(directory));
    }

    /// `Instance.live`: whether the location is loaded or being built, without booting it.
    pub fn is_live(&self, directory: &str) -> bool {
        self.map.lock().contains_key(&oc_core::location::canonical_directory(directory))
    }

    /// `LocationServiceMap.invalidate(ref)` (`debug.location.evict`, `vcs.init`, `location.reload`):
    /// shut the location down (`LocationLifecycle.shutdown`: pending permissions rejected, forms
    /// cancelled, MCP servers stopped via `runner.close_location`), then announce
    /// `location.shutdown`.
    pub async fn evict(&self, directory: &str) {
        let canonical = oc_core::location::canonical_directory(directory);
        let directory = canonical.as_str();
        let removed = self.map.lock().shift_remove(directory);
        match &self.runner {
            Some(r) => r.close_location(directory).await,
            None => self.app.plugins.close(directory).await,
        }
        if removed.is_some_and(|c| c.initialized()) {
            self.shutdown(directory);
        }
    }

    /// `LocationServiceMap.reload()`: invalidate every loaded location and rebuild them all.
    pub async fn reload(&self) -> Result<(), ApiError> {
        let keys = self.keys();
        for key in &keys {
            self.evict(key).await;
        }
        let results = futures::future::join_all(keys.iter().map(|k| self.get(k))).await;
        if let Some(Err(e)) = results.into_iter().find(Result::is_err) {
            let message = match &e {
                ApiError::Typed { body, .. } => body.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string(),
                other => other.to_string(),
            };
            return Err(ApiError::service_unavailable(message, Some("location".into())));
        }
        tracing::info!(count = keys.len(), "location services reloaded");
        Ok(())
    }

    /// Paths of every loaded location, used by `permission.request.list` filtering.
    pub fn canonical_of(&self, directory: &str) -> PathBuf {
        std::fs::canonicalize(directory).unwrap_or_else(|_| PathBuf::from(directory))
    }
}
