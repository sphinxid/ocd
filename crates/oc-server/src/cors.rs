//! CORS (spec 06 §5), mirroring Effect's `HttpMiddleware.cors` in predicate mode.

use std::sync::{Arc, LazyLock};

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use regex::Regex;

use crate::app::App;

static OPENCODE_ORIGIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^https://([a-z0-9-]+\.)*opencode\.ai$").expect("regex"));

const ALLOW_METHODS: &str = "GET, HEAD, PUT, PATCH, POST, DELETE";
const MAX_AGE: &str = "86400";

/// `isAllowedCorsOrigin`.
pub fn is_allowed_cors_origin(origin: Option<&str>, extra: &[String]) -> bool {
    let Some(input) = origin.filter(|o| !o.is_empty()) else { return true };
    if input.starts_with("http://localhost:") || input.starts_with("http://127.0.0.1:") || input.starts_with("oc://renderer") {
        return true;
    }
    if matches!(input, "tauri://localhost" | "http://tauri.localhost" | "https://tauri.localhost") {
        return true;
    }
    if OPENCODE_ORIGIN.is_match(input) {
        return true;
    }
    extra.iter().any(|o| o == input)
}

/// `isAllowedRequestOrigin`: allowed CORS origin, or `URL(origin).host == Host`.
pub fn is_allowed_request_origin(headers: &HeaderMap, extra: &[String]) -> bool {
    let origin = headers.get(header::ORIGIN).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
    let Some(origin) = origin.filter(|o| !o.is_empty()) else { return true };
    if let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok())
        && crate::auth::url_host(&origin).as_deref() == Some(host)
    {
        return true;
    }
    is_allowed_cors_origin(Some(&origin), extra)
}

fn vary_with(existing: Option<&HeaderValue>, dimension: &str) -> String {
    match existing.and_then(|v| v.to_str().ok()) {
        None => dimension.to_string(),
        Some(vary) => {
            let lower = dimension.to_ascii_lowercase();
            let members: Vec<String> = vary.split(',').map(|m| m.trim().to_ascii_lowercase()).collect();
            if members.iter().any(|m| m == "*" || *m == lower) {
                vary.to_string()
            } else {
                format!("{vary}, {dimension}")
            }
        }
    }
}

fn set(headers: &mut HeaderMap, name: header::HeaderName, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        headers.insert(name, v);
    }
}

/// Outermost middleware: answers every `OPTIONS` (204) and decorates other responses.
pub(crate) async fn cors(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let origin = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()).map(str::to_string);
    let allowed = is_allowed_cors_origin(origin.as_deref(), &app.cors);
    if req.method() == Method::OPTIONS {
        let mut res = StatusCode::NO_CONTENT.into_response();
        let h = res.headers_mut();
        if allowed && let Some(o) = &origin {
            set(h, header::ACCESS_CONTROL_ALLOW_ORIGIN, o);
        }
        set(h, header::VARY, "Origin");
        set(h, header::ACCESS_CONTROL_ALLOW_METHODS, ALLOW_METHODS);
        set(h, header::ACCESS_CONTROL_MAX_AGE, MAX_AGE);
        match req.headers().get(header::ACCESS_CONTROL_REQUEST_HEADERS).and_then(|v| v.to_str().ok()) {
            Some(requested) if !requested.is_empty() => {
                set(h, header::ACCESS_CONTROL_ALLOW_HEADERS, requested);
                let vary = vary_with(h.get(header::VARY), "Access-Control-Request-Headers");
                set(h, header::VARY, &vary);
            }
            _ => {
                h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static(""));
            }
        }
        return res;
    }
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    if allowed && let Some(o) = &origin {
        set(h, header::ACCESS_CONTROL_ALLOW_ORIGIN, o);
    }
    let vary = vary_with(h.get(header::VARY), "Origin");
    set(h, header::VARY, &vary);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_origins() {
        let extra = vec!["https://example.com".to_string()];
        assert!(is_allowed_cors_origin(None, &[]));
        assert!(is_allowed_cors_origin(Some("http://localhost:3000"), &[]));
        assert!(!is_allowed_cors_origin(Some("http://localhost"), &[]));
        assert!(is_allowed_cors_origin(Some("http://127.0.0.1:1"), &[]));
        assert!(is_allowed_cors_origin(Some("oc://renderer/x"), &[]));
        assert!(is_allowed_cors_origin(Some("tauri://localhost"), &[]));
        assert!(is_allowed_cors_origin(Some("https://tauri.localhost"), &[]));
        assert!(is_allowed_cors_origin(Some("https://opencode.ai"), &[]));
        assert!(is_allowed_cors_origin(Some("https://app.dev.opencode.ai"), &[]));
        assert!(!is_allowed_cors_origin(Some("https://evilopencode.ai"), &[]));
        assert!(!is_allowed_cors_origin(Some("http://opencode.ai"), &[]));
        assert!(is_allowed_cors_origin(Some("https://example.com"), &extra));
        assert!(!is_allowed_cors_origin(Some("https://example.com/"), &extra));
    }

    #[test]
    fn same_host_request_origin() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("192.168.1.2:4096"));
        h.insert(header::ORIGIN, HeaderValue::from_static("http://192.168.1.2:4096"));
        assert!(is_allowed_request_origin(&h, &[]));
        h.insert(header::ORIGIN, HeaderValue::from_static("http://evil.com"));
        assert!(!is_allowed_request_origin(&h, &[]));
    }

    #[test]
    fn vary_merge() {
        assert_eq!(vary_with(None, "Origin"), "Origin");
        assert_eq!(vary_with(Some(&HeaderValue::from_static("accept-encoding")), "Origin"), "accept-encoding, Origin");
        assert_eq!(vary_with(Some(&HeaderValue::from_static("origin")), "Origin"), "origin");
    }
}
