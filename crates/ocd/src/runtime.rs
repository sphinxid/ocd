//! Builds the in-process server runtime: database → `oc_core::app::App` → session runner →
//! `oc_backend` HTTP server (which also resumes suspended sessions at boot).

use std::sync::Arc;

use oc_core::app::App;
use oc_util::Global;

pub struct Runtime {
    pub app: Arc<App>,
    pub runner: Arc<oc_core::runner::Engine>,
}

/// Open the channel database and construct the core runtime + session runner.
pub fn build(global: &Global) -> anyhow::Result<Runtime> {
    // The certificate store is read while the database opens; the HTTP clients below need it.
    oc_util::http::preload();
    let db = oc_storage::Database::open(&global.database(oc_util::version::CHANNEL), &global.data)?;
    let app = App::new(global.clone(), db, oc_util::version::VERSION);
    let services = oc_core::services::default_services(&app);
    let runner = oc_core::runner::Engine::new(app.clone(), &services);
    Ok(Runtime { app, runner })
}

/// Bind the HTTP API for the runtime (marks it ready). `oc_backend::start` adopts an inherited
/// `OPENCODE_PTY_HANDOFF` at boot; the returned engine serves
/// `POST /api/experimental/persistent-pty/handoff` (`Engine::prepare_pty_handoff`), which the
/// client side of `service stop/restart`/version replacement calls before terminating this process
/// and then passes to the replacement contender through `OPENCODE_PTY_HANDOFF` (`service.rs`).
pub async fn serve(rt: &Runtime, opts: oc_server::ServerOptions) -> anyhow::Result<(oc_server::RunningServer, std::sync::Arc<oc_backend::Engine>)> {
    oc_backend::start(rt.app.clone(), Some(rt.runner.clone()), opts).await
}
