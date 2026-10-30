//! `oc_server::Backend` implemented over `oc_core::App` (spec 06): every HTTP operation maps to
//! a core service, a direct storage/git/config call, or a documented `TODO(core)` stub.

pub mod backend;
pub mod catalog;
pub mod engine;
pub mod file_watch;
pub mod fs;
pub mod fuzzy;
pub mod locations;
pub mod notify;
pub mod persistent;
pub mod plugins;
pub mod sessions;
pub mod shells;
pub mod stats;
pub mod store;
pub mod util;
pub mod vcs_watch;

use std::sync::Arc;

pub use engine::Engine;
use oc_core::app::App;

fn engine(app: Arc<App>, runner: Option<Arc<oc_core::runner::Engine>>) -> Arc<Engine> {
    match runner {
        Some(r) => {
            if r.app.coordinator.get().is_none() {
                r.install();
            }
            Engine::with_runner(r)
        }
        None => Engine::new(app),
    }
}

/// Bind the HTTP server and mark it ready (the app is already built). Pass the core session
/// runner to enable execution, the model catalog and integrations over its services (it is
/// installed as the app's coordinator if no coordinator is set yet).
pub async fn run_server(
    app: Arc<App>,
    runner: Option<Arc<oc_core::runner::Engine>>,
    opts: oc_server::ServerOptions,
) -> anyhow::Result<oc_server::RunningServer> {
    Ok(start(app, runner, opts).await?.0)
}

/// [`run_server`] that also returns the engine, so the CLI can call
/// [`Engine::prepare_pty_handoff`] on service stop/restart.
pub async fn start(
    app: Arc<App>,
    runner: Option<Arc<oc_core::runner::Engine>>,
    opts: oc_server::ServerOptions,
) -> anyhow::Result<(oc_server::RunningServer, Arc<Engine>)> {
    let engine = engine(app, runner);
    plugins::install(&engine, &opts);
    // Adopt an inherited persistent-PTY handoff at boot.
    engine.persistent().await;
    let server = oc_server::serve(engine.clone() as Arc<dyn oc_server::Backend>, opts).await?;
    server.set_ready();
    start_v1_import(&engine);
    start_models_refresh(&engine);
    start_project_sweep(&engine);
    // Restart recovery: resume sessions whose execution claim survived the last process.
    if let Some(r) = engine.runner.clone() {
        tokio::spawn(async move {
            match r.resume_suspended().await {
                Ok(n) if n > 0 => tracing::info!(count = n, "resumed suspended sessions"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "failed to resume suspended sessions"),
            }
        });
    }
    Ok((server, engine))
}

/// Start the models.dev background refresh (`ModelsDev.layer`: when the catalog is first needed, then every
/// 5 minutes, unless `OPENCODE_DISABLE_MODELS_FETCH` is set). A refresh that changed the catalog publishes
/// `models-dev.refreshed` server-wide, then the models.dev plugin of every loaded location reloads its
/// integrations and providers (`integration.updated`, `provider.updated`, and `model.updated` from the
/// model State that follows providers). Catalogs rebuild on their next read, since they are keyed by
/// the models.dev data they were built from.
pub fn start_models_refresh(engine: &Arc<Engine>) {
    let models_dev = engine.models().models_dev.clone();
    if !models_dev.options().fetch {
        return;
    }
    let mut refreshed = models_dev.subscribe();
    models_dev.refresh_on_first_use();
    // Upstream builds the models.dev service, and forks its refresh, when the first location boots.
    let pending = Arc::downgrade(&models_dev);
    engine.locations.on_build(Arc::new(move || {
        if let Some(models_dev) = pending.upgrade() {
            models_dev.start_pending_refresh();
        }
    }));
    let engine = Arc::downgrade(engine);
    tokio::spawn(async move {
        loop {
            match refreshed.recv().await {
                Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
            let Some(engine) = engine.upgrade() else { return };
            engine.app.bus.publish("models-dev.refreshed", serde_json::json!({}), None);
            for directory in engine.locations.keys() {
                let at = oc_schema::types::LocationRef { directory, workspace_id: None };
                for kind in ["integration.updated", "provider.updated", "model.updated"] {
                    engine.app.bus.publish(kind, serde_json::json!({}), Some(at.clone()));
                }
            }
        }
    });
}

/// Start the hourly project archive sweep (`Project.sweepNode`): once at startup, then an hour after each run.
fn start_project_sweep(engine: &Arc<Engine>) {
    let engine = Arc::downgrade(engine);
    tokio::spawn(async move {
        loop {
            let Some(strong) = engine.upgrade() else { return };
            let bus = strong.app.bus.clone();
            drop(strong);
            match tokio::task::spawn_blocking(move || oc_core::project::sweep(&bus)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::error!(error = %e, "Project archive sweep failed"),
                Err(e) => tracing::error!(error = %e, "Project archive sweep failed"),
            }
            tokio::time::sleep(std::time::Duration::from_millis(oc_core::project::SWEEP_INTERVAL_MS as u64)).await;
        }
    });
}

/// Start the background V1 → V2 history import (`V1Migration.layer`, `server/src/routes.ts:188`). It runs on its
/// own thread, one transaction per session; nothing waits for it. Status: `GET /api/experimental/migration/v1`.
fn start_v1_import(engine: &Engine) {
    let importer = oc_core::v1_import::Importer::global();
    importer.start(engine.app.db.clone(), engine.app.global.data.clone());
}

/// Build the router without binding (tests / embedding).
pub fn router(
    app: Arc<App>,
    runner: Option<Arc<oc_core::runner::Engine>>,
    opts: &oc_server::ServerOptions,
) -> anyhow::Result<(axum::Router, oc_server::RouterHandle)> {
    let engine = engine(app, runner);
    plugins::install(&engine, opts);
    oc_server::router(engine as Arc<dyn oc_server::Backend>, opts)
}
