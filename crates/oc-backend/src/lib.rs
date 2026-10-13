//! `oc_server::Backend` implemented over `oc_core::App` (spec 06): every HTTP operation maps to
//! a core service, a direct storage/git/config call, or a documented `TODO(core)` stub.

pub mod backend;
pub mod catalog;
pub mod engine;
pub mod fs;
pub mod fuzzy;
pub mod locations;
pub mod persistent;
pub mod plugins;
pub mod sessions;
pub mod shells;
pub mod stats;
pub mod store;
pub mod util;

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
