//! `RequestSite` (`server/src/request-site.ts`, 2.2.7 #62422): API routes are called by scripts and native clients,
//! not opened as pages from other sites. Browsers label requests with `Sec-Fetch-Site` and `Sec-Fetch-Mode`; another
//! site may navigate to the API or call it only from an allowed CORS origin. Clients that send no `Sec-Fetch`
//! headers (CLI, TUI, SDK, mobile) and same-origin or typed-in requests pass.

use std::sync::{Arc, LazyLock};

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use regex::Regex;

use crate::app::App;
use crate::error::ApiError;

/// Artifact pages render in sandboxed frames, so their own requests come from an opaque origin.
static ARTIFACT_VIEW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/api/experimental/artifacts/[^/]+/view(/|$)").expect("regex"));

/// `rejected(request, options)`.
pub fn rejected(path_and_query: &str, headers: &HeaderMap, extra: &[String]) -> bool {
    let site = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok());
    if !matches!(site, Some("cross-site" | "same-site")) {
        return false;
    }
    let pathname = crate::webui::pathname(path_and_query);
    if pathname != "/api" && !pathname.starts_with("/api/") {
        return false;
    }
    if ARTIFACT_VIEW.is_match(&pathname) {
        return false;
    }
    if headers.get("sec-fetch-mode").is_some_and(|v| v.as_bytes() == b"navigate") {
        return true;
    }
    headers.get(axum::http::header::ORIGIN).is_some() && !crate::cors::is_allowed_request_origin(headers, extra)
}

/// Middleware: `403 {"_tag":"ForbiddenError","message":"Cross-site requests to the API are not allowed"}` before
/// the web UI, auth and readiness (inside compression and CORS).
pub(crate) async fn middleware(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let path = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if !rejected(path, req.headers(), &app.cors) {
        return next.run(req).await;
    }
    ApiError::forbidden("Cross-site requests to the API are not allowed").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    /// `request-site.test.ts`.
    #[test]
    fn rejects_cross_site_navigations_and_disallowed_origins_on_the_api() {
        let extra: Vec<String> = vec![];
        let host = ("host", "127.0.0.1:4096");
        // No Sec-Fetch headers (CLI, SDK) or same-origin requests pass.
        assert!(!rejected("/api/session", &headers(&[host]), &extra));
        assert!(!rejected("/api/session", &headers(&[host, ("sec-fetch-site", "same-origin"), ("sec-fetch-mode", "navigate")]), &extra));
        assert!(!rejected("/api/session", &headers(&[host, ("sec-fetch-site", "none"), ("sec-fetch-mode", "navigate")]), &extra));
        // Another site navigating to the API.
        assert!(rejected("/api/fs/read/a.html", &headers(&[host, ("sec-fetch-site", "cross-site"), ("sec-fetch-mode", "navigate")]), &extra));
        assert!(rejected("/api", &headers(&[host, ("sec-fetch-site", "same-site"), ("sec-fetch-mode", "navigate")]), &extra));
        assert!(rejected("/x/../api/session", &headers(&[host, ("sec-fetch-site", "cross-site"), ("sec-fetch-mode", "navigate")]), &extra));
        // Requests from a disallowed origin; an allowed or same-host origin, or none, passes.
        let cors = |origin: &'static str| headers(&[host, ("sec-fetch-site", "cross-site"), ("sec-fetch-mode", "cors"), ("origin", origin)]);
        assert!(rejected("/api/session", &cors("https://evil.example"), &extra));
        assert!(!rejected("/api/session", &cors("http://localhost:3000"), &extra));
        assert!(!rejected("/api/session", &cors("https://app.opencode.ai"), &extra));
        assert!(!rejected("/api/session", &cors("http://127.0.0.1:4096"), &extra));
        assert!(!rejected("/api/session", &cors("https://evil.example"), &["https://evil.example".to_string()]));
        assert!(!rejected("/api/session", &headers(&[host, ("sec-fetch-site", "cross-site"), ("sec-fetch-mode", "no-cors")]), &extra));
        // Not the API, or an artifact page's own requests.
        assert!(!rejected("/", &headers(&[host, ("sec-fetch-site", "cross-site"), ("sec-fetch-mode", "navigate")]), &extra));
        assert!(!rejected("/apix", &headers(&[host, ("sec-fetch-site", "cross-site"), ("sec-fetch-mode", "navigate")]), &extra));
        assert!(!rejected(
            "/api/experimental/artifacts/art_1/view/index.html",
            &headers(&[host, ("sec-fetch-site", "cross-site"), ("sec-fetch-mode", "navigate")]),
            &extra
        ));
        assert!(!rejected("/api/experimental/artifacts/art_1/view", &cors("null"), &extra));
    }
}
