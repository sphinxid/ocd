//! Shared server state, readiness and the dispatch gate (readiness + process-level auth).

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::LazyLock;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use regex::Regex;
use serde_json::json;

use crate::auth::{Auth, Pairing, Tickets, unauthorized_response};
use crate::context::{LocationRef, ServerMeta};
use crate::error::{ApiError, json_response};
use crate::generated::Backend;
use crate::router::parse_query;
use crate::webui::WebUi;

/// Lifecycle state of the server process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Starting = 0,
    Ready = 1,
    Failed = 2,
    Stopping = 3,
}

impl Status {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Status::Ready,
            2 => Status::Failed,
            3 => Status::Stopping,
            _ => Status::Starting,
        }
    }
}

pub(crate) struct App {
    pub backend: Arc<dyn Backend>,
    pub meta: Arc<ServerMeta>,
    pub auth: Auth,
    pub pairing: Pairing,
    pub tickets: Tickets,
    pub cors: Vec<String>,
    pub web_ui: Option<WebUi>,
    status: AtomicU8,
}

impl App {
    pub fn new(
        backend: Arc<dyn Backend>,
        meta: Arc<ServerMeta>,
        auth: Auth,
        cors: Vec<String>,
        web_ui: Option<WebUi>,
    ) -> Self {
        Self {
            backend,
            meta,
            auth,
            pairing: Pairing::new(),
            tickets: Tickets::new(),
            cors,
            web_ui,
            status: AtomicU8::new(Status::Starting as u8),
        }
    }

    pub fn status(&self) -> Status {
        Status::from_u8(self.status.load(Ordering::SeqCst))
    }

    pub fn set_status(&self, s: Status) {
        self.status.store(s as u8, Ordering::SeqCst);
    }

    /// L-scope check: the directory must exist (`LocationNotFoundError` otherwise).
    pub fn check_location(&self, location: &LocationRef) -> Result<(), ApiError> {
        if std::path::Path::new(&location.directory).is_dir() {
            Ok(())
        } else {
            Err(ApiError::location_not_found(&location.directory))
        }
    }
}

static PAIRING_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/auth/connect/[^/]+$").expect("regex"));
static PTY_CONNECT_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/api/pty/[^/]+/connect$").expect("regex"));
static PERSISTENT_CONNECT_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/api/experimental/persistent-pty/[^/]+/connect$").expect("regex"));

pub(crate) fn is_pairing_connect(path: &str) -> bool {
    PAIRING_PATH.is_match(path)
}

pub(crate) fn has_pty_ticket(path: &str, query: &[(String, String)]) -> bool {
    (PTY_CONNECT_PATH.is_match(path) || PERSISTENT_CONNECT_PATH.is_match(path))
        && query.iter().any(|(k, v)| k == "ticket" && !v.is_empty())
}

fn unavailable(status: Status) -> Response {
    if status == Status::Failed {
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({
                "code": "service_failed",
                "message": "The background service could not start.",
                "action": "Run `opencode service restart` after checking the service logs.",
            }),
        );
    }
    let code = if status == Status::Stopping { "service_stopping" } else { "service_starting" };
    let mut res = json_response(StatusCode::SERVICE_UNAVAILABLE, &json!({ "code": code }));
    res.headers_mut().insert("retry-after", HeaderValue::from_static("1"));
    res
}

fn info_response(app: &App, status: Status) -> Response {
    let info = app.meta.info(None);
    let code = match status {
        Status::Ready => StatusCode::OK,
        Status::Failed => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    let mut res = json_response(code, &serde_json::to_value(&info).unwrap_or_default());
    if matches!(status, Status::Starting | Status::Stopping) {
        res.headers_mut().insert("retry-after", HeaderValue::from_static("1"));
    }
    res
}

/// `dispatch` (upstream process.ts): `/api/info` before ready, process-level auth (also for
/// unknown routes and `/openapi.json`), readiness 503s.
pub(crate) async fn gate(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let query = parse_query(req.uri().query());
    let status = app.status();
    let ready = status == Status::Ready;
    if req.method() == Method::GET && path == "/api/info" && !ready {
        if !app.auth.authorized_request(&query, req.headers()) {
            return unauthorized_response(req.headers());
        }
        return info_response(&app, status);
    }
    if !is_pairing_connect(&path)
        && (!ready || !has_pty_ticket(&path, &query))
        && !app.auth.authorized_request(&query, req.headers())
    {
        return unauthorized_response(req.headers());
    }
    if ready {
        return next.run(req).await;
    }
    unavailable(status).into_response()
}
