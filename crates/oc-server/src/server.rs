//! Startup: bind first, serve immediately (readiness-gated), compression + CORS layering.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::HttpBody;
use axum::http::{Response as HttpResponse, StatusCode, header};
use axum::middleware::from_fn_with_state;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tower_http::compression::{CompressionLayer, Predicate};

use crate::app::{App, Status};
use crate::auth::Auth;
use crate::context::ServerMeta;
use crate::generated::Backend;
use crate::webui::WebUi;

/// First port tried when no port is given.
pub const DEFAULT_PORT: u16 = 4096;

/// Server configuration.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Bind host (default `127.0.0.1`).
    pub hostname: String,
    /// `Some(p)` binds exactly (`0` = ephemeral); `None` tries 4096 and counts up while busy.
    pub port: Option<u16>,
    /// Server password; `None`/empty = unauthenticated mode.
    pub password: Option<String>,
    /// Extra allowed CORS origins (`--cors`).
    pub cors: Vec<String>,
    /// App version reported by `/api/info` (`"unknown"` if empty).
    pub version: String,
    /// Global tmp directory (`paths.tmp`).
    pub tmp_dir: String,
    /// Default location when a request names none (default: process cwd).
    pub cwd: Option<String>,
    /// Web UI archive file or directory (SPA served for non-API paths).
    pub web_ui: Option<PathBuf>,
    /// Start in the `ready` state (otherwise call [`RunningServer::set_ready`]).
    pub ready: bool,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            hostname: "127.0.0.1".into(),
            port: None,
            password: None,
            cors: Vec::new(),
            version: "unknown".into(),
            tmp_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            cwd: None,
            web_ui: None,
            ready: false,
        }
    }
}

/// A listening server.
pub struct RunningServer {
    url: String,
    addr: SocketAddr,
    app: Arc<App>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl RunningServer {
    /// `http://host:port` of the bound listener.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Bound socket address.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Connection URLs reported by `/api/info`.
    pub fn urls(&self) -> Vec<String> {
        self.app.meta.urls()
    }

    /// Server identity (e.g. to append remote tunnel URLs).
    pub fn meta(&self) -> Arc<ServerMeta> {
        self.app.meta.clone()
    }

    /// Mark the application ready: routes are served.
    pub fn set_ready(&self) {
        self.app.set_status(Status::Ready);
    }

    /// Mark boot as failed: `/api/info` answers 500, other routes 503 `service_failed`.
    pub fn set_failed(&self) {
        self.app.set_status(Status::Failed);
    }

    /// Current lifecycle status.
    pub fn status(&self) -> Status {
        self.app.status()
    }

    /// Stop accepting connections and wait (≤ 2 s) for in-flight requests.
    pub async fn shutdown(mut self) {
        self.app.set_status(Status::Stopping);
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let abort = task.abort_handle();
            if tokio::time::timeout(Duration::from_secs(2), task).await.is_err() {
                abort.abort();
            }
        }
    }

    /// Wait until the server task ends.
    pub async fn wait(mut self) {
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Compression policy mirroring Effect's `HttpMiddleware.compression()`: skip 1xx/204/206/304,
/// already-encoded bodies, `no-transform`, non-compressible types, SSE, and bodies < 1024 bytes.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompressionPolicy;

fn compressible(ct: &str) -> bool {
    let t = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if t == "text/event-stream" {
        return false;
    }
    if t.starts_with("text/") {
        return true;
    }
    matches!(t.as_str(), "application/json" | "application/javascript" | "application/xml" | "image/svg+xml" | "application/wasm")
        || t.ends_with("+json")
        || t.ends_with("+xml")
}

impl Predicate for CompressionPolicy {
    fn should_compress<B>(&self, response: &HttpResponse<B>) -> bool
    where
        B: HttpBody,
    {
        let s = response.status();
        if s.as_u16() < 200 || s == StatusCode::NO_CONTENT || s == StatusCode::PARTIAL_CONTENT || s == StatusCode::NOT_MODIFIED {
            return false;
        }
        let h = response.headers();
        if h.contains_key(header::CONTENT_ENCODING) {
            return false;
        }
        if let Some(cc) = h.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok())
            && cc.to_ascii_lowercase().split([',', ' ', ';']).any(|p| p == "no-transform")
        {
            return false;
        }
        match h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) {
            Some(ct) if compressible(ct) => {}
            _ => return false,
        }
        let len = h
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .or_else(|| response.body().size_hint().exact());
        !matches!(len, Some(n) if n < 1024)
    }
}

/// Build the full HTTP application (CORS → compression → web UI → gate → API router).
pub(crate) fn build_router(app: Arc<App>) -> Router {
    crate::router::api_router(app.clone())
        .layer(from_fn_with_state(app.clone(), crate::app::gate))
        .layer(from_fn_with_state(app.clone(), crate::webui::web_ui))
        .layer(CompressionLayer::new().compress_when(CompressionPolicy))
        .layer(from_fn_with_state(app, crate::cors::cors))
}

fn format_url(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') { format!("http://[{host}]:{port}") } else { format!("http://{host}:{port}") }
}

/// `connectionURLs`: wildcard binds report the loopback URL.
pub(crate) fn connection_urls(hostname: &str, port: u16) -> Vec<String> {
    match hostname {
        "0.0.0.0" => vec![format!("http://127.0.0.1:{port}")],
        "::" | "[::]" => vec![format!("http://[::1]:{port}")],
        h => vec![format_url(h, port)],
    }
}

async fn bind(hostname: &str, port: Option<u16>) -> anyhow::Result<TcpListener> {
    let host = hostname.trim_start_matches('[').trim_end_matches(']');
    if let Some(p) = port {
        return Ok(TcpListener::bind((host, p)).await?);
    }
    let mut p = DEFAULT_PORT;
    loop {
        match TcpListener::bind((host, p)).await {
            Ok(l) => return Ok(l),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && p < u16::MAX => p += 1,
            Err(e) => return Err(e.into()),
        }
    }
}

/// Build the application state + router without binding (tests, embedding).
pub fn router(backend: Arc<dyn Backend>, opts: &ServerOptions) -> anyhow::Result<(Router, RouterHandle)> {
    let app = Arc::new(make_app(backend, opts)?);
    if opts.ready {
        app.set_status(Status::Ready);
    }
    Ok((build_router(app.clone()), RouterHandle { app }))
}

/// Readiness control for a [`router`] built without [`serve`].
#[derive(Clone)]
pub struct RouterHandle {
    app: Arc<App>,
}

impl RouterHandle {
    pub fn set_ready(&self) {
        self.app.set_status(Status::Ready);
    }
    pub fn set_failed(&self) {
        self.app.set_status(Status::Failed);
    }
    pub fn set_stopping(&self) {
        self.app.set_status(Status::Stopping);
    }
    pub fn meta(&self) -> Arc<ServerMeta> {
        self.app.meta.clone()
    }
    /// Issue a pairing session token (as `/auth/connect` would).
    pub fn issue_session(&self) -> Option<String> {
        self.app.auth.issue_session()
    }
}

fn make_app(backend: Arc<dyn Backend>, opts: &ServerOptions) -> anyhow::Result<App> {
    let version = if opts.version.is_empty() { "unknown".to_string() } else { opts.version.clone() };
    let mut meta = ServerMeta::new(version, opts.tmp_dir.clone());
    if let Some(cwd) = &opts.cwd {
        meta = meta.with_cwd(cwd.clone());
    }
    let web_ui = match &opts.web_ui {
        Some(p) => Some(WebUi::load(p)?),
        None => None,
    };
    let password = opts.password.clone().filter(|p| !p.is_empty());
    Ok(App::new(backend, Arc::new(meta), Auth::new(password), opts.cors.clone(), web_ui))
}

/// Bind the listener and start serving. The server answers immediately (readiness-gated:
/// `/api/info` 503 + `retry-after`, other routes 503 `{code}`) until [`RunningServer::set_ready`].
pub async fn serve(backend: Arc<dyn Backend>, opts: ServerOptions) -> anyhow::Result<RunningServer> {
    let listener = bind(&opts.hostname, opts.port).await?;
    let addr = listener.local_addr()?;
    let app = Arc::new(make_app(backend, &opts)?);
    let urls = connection_urls(&opts.hostname, addr.port());
    let url = format_url(opts.hostname.trim_start_matches('[').trim_end_matches(']'), addr.port());
    app.meta.set_urls(urls);
    if opts.ready {
        app.set_status(Status::Ready);
    }
    let router = build_router(app.clone());
    let (tx, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let result = axum::serve(listener, router.into_make_service())
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
        if let Err(e) = result {
            tracing::error!(error = %e, "http server failed");
        }
    });
    tracing::info!(%url, "server listening");
    Ok(RunningServer { url, addr, app, shutdown: Some(tx), task: Some(task) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(connection_urls("127.0.0.1", 4096), vec!["http://127.0.0.1:4096"]);
        assert_eq!(connection_urls("0.0.0.0", 1), vec!["http://127.0.0.1:1"]);
        assert_eq!(connection_urls("::1", 2), vec!["http://[::1]:2"]);
        assert_eq!(connection_urls("::", 3), vec!["http://[::1]:3"]);
    }

    #[test]
    fn compressible_types() {
        assert!(compressible("application/json"));
        assert!(compressible("text/html; charset=utf-8"));
        assert!(compressible("application/problem+json"));
        assert!(!compressible("text/event-stream"));
        assert!(!compressible("image/png"));
        assert!(!compressible("application/octet-stream"));
    }
}
