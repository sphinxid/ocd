//! Request decoding (path / query / body, with upstream's 400 `InvalidRequestError` shapes),
//! response envelopes and the axum route table.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use axum::extract::{FromRequestParts, RawPathParams, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodFilter, MethodRouter, on};
use axum::Router;
use bytes::Bytes;
use parking_lot::Mutex;
use regex::Regex;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::app::App;
use crate::context::{LocationRef, RequestContext};
use crate::error::ApiError;
use crate::generated::{OPS, Op, OpMeta, Scope};
use crate::location::request_ref;

/// Maximum accepted request body.
const BODY_LIMIT: usize = 512 * 1024 * 1024;

/// A decoded-enough request: raw path params, query pairs, headers and body.
pub(crate) struct Call {
    pub ctx: RequestContext,
    pub path: HashMap<String, String>,
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub method: Method,
    pub body: Bytes,
}

static PATTERNS: LazyLock<Mutex<HashMap<&'static str, Regex>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn matches(pattern: &'static str, value: &str) -> bool {
    let mut cache = PATTERNS.lock();
    let re = cache.entry(pattern).or_insert_with(|| Regex::new(pattern).expect("valid pattern"));
    re.is_match(value)
}

/// Turn `Option<T>` from an optional decoder into a required value (`Query` schema error).
pub(crate) fn required<T>(value: Option<T>, name: &str) -> Result<T, ApiError> {
    value.ok_or_else(|| ApiError::schema("Query", format!("Missing key\n  at [\"{name}\"]")))
}

fn query_issue(name: &str, detail: impl std::fmt::Display) -> ApiError {
    ApiError::schema("Query", format!("{detail}\n  at [\"{name}\"]"))
}

/// Effect's message for a failed string pattern check: `Schema.isStartingWith` exports `^prefix`,
/// everything else is `Schema.isPattern`.
pub(crate) fn pattern_message(pattern: &str) -> String {
    let literal_prefix = pattern
        .strip_prefix('^')
        .filter(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    match literal_prefix {
        Some(prefix) => format!("Expected a string starting with {prefix:?}"),
        None => format!("Expected a string matching the RegExp {pattern}"),
    }
}

impl Call {
    /// First value of a query parameter.
    pub fn query_raw(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn path_str(&self, name: &str, pattern: Option<&'static str>) -> Result<String, ApiError> {
        let Some(v) = self.path.get(name) else {
            return Err(ApiError::schema("Params", format!("Missing key\n  at [\"{name}\"]")));
        };
        if let Some(p) = pattern
            && !matches(p, v)
        {
            return Err(ApiError::schema("Params", format!("{}\n  at [\"{name}\"]", pattern_message(p))));
        }
        Ok(v.clone())
    }

    pub fn query_str(&self, name: &str, pattern: Option<&'static str>) -> Result<Option<String>, ApiError> {
        let Some(v) = self.query_raw(name) else { return Ok(None) };
        if let Some(p) = pattern
            && !matches(p, v)
        {
            return Err(query_issue(name, pattern_message(p)));
        }
        Ok(Some(v.to_string()))
    }

    /// A literal union (`Schema.Literals`): unknown values fail with `Expected <expected>`, where
    /// `expected` is the schema identifier (`Vcs.Mode`) or the literals (`"asc" | "desc"`).
    pub fn query_enum<T: DeserializeOwned>(&self, name: &str, expected: &str) -> Result<Option<T>, ApiError> {
        let Some(v) = self.query_raw(name) else { return Ok(None) };
        serde_json::from_value(serde_json::Value::String(v.to_string()))
            .map(Some)
            .map_err(|_| query_issue(name, format!("Expected {expected}")))
    }

    /// `Schema.ArrayEnsure(X)`: every value of a repeated key (`?k=a&k=b`), a single value being
    /// promoted to a one-element array. Items that do not decode fail with `Expected <expected>`.
    pub fn query_array<T: DeserializeOwned>(&self, name: &str, expected: &str) -> Result<Option<Vec<T>>, ApiError> {
        let values: Vec<&str> = self.query.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str()).collect();
        if values.is_empty() {
            return Ok(None);
        }
        let mut out = Vec::with_capacity(values.len());
        for (index, v) in values.iter().enumerate() {
            let item = serde_json::from_value(serde_json::Value::String(v.to_string())).map_err(|_| {
                let at = if values.len() == 1 { format!("[\"{name}\"]") } else { format!("[\"{name}\"][{index}]") };
                ApiError::schema("Query", format!("Expected {expected}\n  at {at}"))
            })?;
            out.push(item);
        }
        Ok(Some(out))
    }

    /// `BooleanFromString`: only `"true"` / `"false"`.
    pub fn query_bool(&self, name: &str) -> Result<Option<bool>, ApiError> {
        match self.query_raw(name) {
            None => Ok(None),
            Some("true") => Ok(Some(true)),
            Some("false") => Ok(Some(false)),
            Some(_) => Err(query_issue(name, "Expected \"true\" | \"false\"")),
        }
    }

    /// `NumberFromString` decoded to `Int` checked with `isGreaterThanOrEqualTo(min)` /
    /// `isLessThanOrEqualTo(max)`. Non-numeric strings decode to `NaN` and fail the integer check.
    pub fn query_int(&self, name: &str, min: Option<i64>, max: Option<i64>) -> Result<Option<i64>, ApiError> {
        let Some(n) = self.query_integer(name)? else { return Ok(None) };
        if let Some(min) = min
            && n < min
        {
            return Err(query_issue(name, format!("Expected a value greater than or equal to {min}")));
        }
        if let Some(max) = max
            && n > max
        {
            return Err(query_issue(name, format!("Expected a value less than or equal to {max}")));
        }
        Ok(Some(n))
    }

    /// `NumberFromString` decoded to `PositiveInt` (`isGreaterThan(0)`), optionally `isLessThanOrEqualTo(max)`.
    pub fn query_pos_int(&self, name: &str, max: Option<i64>) -> Result<Option<i64>, ApiError> {
        let Some(n) = self.query_integer(name)? else { return Ok(None) };
        if n <= 0 {
            return Err(query_issue(name, "Expected a value greater than 0"));
        }
        if let Some(max) = max
            && n > max
        {
            return Err(query_issue(name, format!("Expected a value less than or equal to {max}")));
        }
        Ok(Some(n))
    }

    fn query_integer(&self, name: &str) -> Result<Option<i64>, ApiError> {
        let Some(v) = self.query_raw(name) else { return Ok(None) };
        let n = parse_js_number(v).unwrap_or(f64::NAN);
        if !n.is_finite() || n.fract() != 0.0 || n.abs() > 9_007_199_254_740_991.0 {
            return Err(query_issue(name, "Expected an integer"));
        }
        Ok(Some(n as i64))
    }

    /// `NumberFromString` (any number): like JS `Number(string)`, unparsable input decodes to `NaN`.
    pub fn query_num(&self, name: &str) -> Result<Option<f64>, ApiError> {
        Ok(self.query_raw(name).map(|v| parse_js_number(v).unwrap_or(f64::NAN)))
    }

    fn content_type(&self) -> Option<String> {
        if !method_has_body(&self.method) {
            return None;
        }
        let raw = self.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("application/json");
        Some(raw.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
    }

    /// Decode the JSON payload of `operation`: malformed JSON, then the Effect-style schema check
    /// against the operation's request schema (`crate::validate`), then serde.
    pub fn json_body_for<T: DeserializeOwned>(&self, operation: &str) -> Result<T, ApiError> {
        let ct = self.content_type().unwrap_or_else(|| "application/x-www-form-urlencoded".into());
        if ct != "application/json" {
            return Err(ApiError::Text {
                status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
                body: format!("Unsupported content-type: {ct}"),
            });
        }
        if self.body.is_empty() {
            return Err(ApiError::schema("Payload", "Expected object"));
        }
        let value: serde_json::Value =
            serde_json::from_slice(&self.body).map_err(|_| ApiError::schema("Payload", "Expected a valid JSON body"))?;
        if let Some(message) = crate::validate::payload_issue(operation, &value) {
            return Err(ApiError::schema("Payload", message));
        }
        serde_json::from_value(value).map_err(|e| ApiError::schema("Payload", e.to_string()))
    }

    /// Raw `application/octet-stream` payload.
    pub fn bytes_body(&self) -> Result<Bytes, ApiError> {
        let ct = self.content_type().unwrap_or_default();
        if ct != "application/octet-stream" {
            return Err(ApiError::Text {
                status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
                body: format!("Unsupported content-type: {ct}"),
            });
        }
        Ok(self.body.clone())
    }
}

fn method_has_body(m: &Method) -> bool {
    !matches!(*m, Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE)
}

/// JS `Number(string)` for the usual decimal forms (`""`/whitespace → `0`).
pub(crate) fn parse_js_number(v: &str) -> Option<f64> {
    let t = v.trim();
    if t.is_empty() {
        return Some(0.0);
    }
    match t {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    if t.chars().any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E') {
        return None;
    }
    t.parse::<f64>().ok()
}

/// Parse a raw query string into ordered `(key, value)` pairs (form-urlencoded).
pub(crate) fn parse_query(raw: Option<&str>) -> Vec<(String, String)> {
    match raw {
        Some(q) => form_urlencoded::parse(q.as_bytes()).into_owned().collect(),
        None => Vec::new(),
    }
}

/// Response envelopes (spec 06 §2.2).
pub(crate) mod respond {
    use super::*;

    #[derive(Serialize)]
    struct PublicRef<'a> {
        directory: &'a str,
    }

    #[derive(Serialize)]
    struct LocationEnvelope<'a, T> {
        location: PublicRef<'a>,
        data: &'a T,
    }

    #[derive(Serialize)]
    struct DataEnvelope<'a, T> {
        data: &'a T,
    }

    pub(crate) fn json<T: Serialize + ?Sized>(status: StatusCode, value: &T) -> Response {
        match crate::support::to_js_vec(value) {
            Ok(bytes) => {
                let mut res = (status, bytes).into_response();
                res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
                res
            }
            Err(e) => ApiError::defect(format!("failed to encode response: {e}")).into_response(),
        }
    }

    /// `{location:{directory}, data}`
    pub(crate) fn location<T: Serialize>(ctx: &RequestContext, data: &T) -> Response {
        json(StatusCode::OK, &LocationEnvelope { location: PublicRef { directory: &ctx.location.directory }, data })
    }

    /// `{data}`
    pub(crate) fn data<T: Serialize>(data: &T) -> Response {
        json(StatusCode::OK, &DataEnvelope { data })
    }

    /// The value as-is.
    pub(crate) fn bare<T: Serialize>(data: &T) -> Response {
        json(StatusCode::OK, data)
    }

    /// `204`, empty body, no content-type.
    pub(crate) fn no_content() -> Response {
        StatusCode::NO_CONTENT.into_response()
    }
}

/// Empty `404` (unknown routes and method mismatches).
pub(crate) async fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

fn method_filter(m: &str) -> MethodFilter {
    match m {
        "GET" => MethodFilter::GET,
        "POST" => MethodFilter::POST,
        "PUT" => MethodFilter::PUT,
        "PATCH" => MethodFilter::PATCH,
        "DELETE" => MethodFilter::DELETE,
        other => panic!("unsupported method {other}"),
    }
}

/// Build the API router: every OpenAPI operation, `/openapi.json`, and an empty-404 fallback.
pub(crate) fn api_router(app: Arc<App>) -> Router {
    let mut by_route: Vec<(&'static str, MethodRouter<Arc<App>>)> = Vec::new();
    for meta in OPS {
        let handler = move |State(app): State<Arc<App>>, req: Request| async move { handle_op(app, meta, req).await };
        let mr = on(method_filter(meta.method), handler);
        if let Some((_, existing)) = by_route.iter_mut().find(|(r, _)| *r == meta.route) {
            let prev = std::mem::take(existing);
            *existing = prev.merge(mr);
        } else {
            by_route.push((meta.route, mr));
        }
    }
    let mut router = Router::new();
    for (route, mr) in by_route {
        router = router.route(route, mr.fallback(not_found));
    }
    router = router.route(
        "/openapi.json",
        axum::routing::get(|| async {
            let mut res = (StatusCode::OK, crate::OPENAPI_JSON).into_response();
            res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            res
        })
        .fallback(not_found),
    );
    router.fallback(not_found).with_state(app)
}

/// Resolve the location for the operation's scope (spec 06 §1.1); runs before param decoding
/// like upstream's group middleware.
fn check_scope(app: &App, meta: &OpMeta, call: &Call) -> Result<(), ApiError> {
    match meta.scope {
        Scope::L => app.check_location(&call.ctx.location),
        Scope::S | Scope::F => {
            let raw = call.path.get("sessionID").map(String::as_str).unwrap_or("");
            if meta.scope == Scope::F && raw == "global" {
                return app.check_location(&call.ctx.location);
            }
            if !raw.starts_with("ses") {
                return Err(ApiError::invalid_request("Invalid session ID").with_field("sessionID"));
            }
            Ok(())
        }
        Scope::G | Scope::H => Ok(()),
    }
}

async fn handle_op(app: Arc<App>, meta: &'static OpMeta, req: Request) -> Response {
    let (mut parts, body) = req.into_parts();
    let path: HashMap<String, String> = match RawPathParams::from_request_parts(&mut parts, &()).await {
        Ok(params) => params.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        Err(_) => return ApiError::schema("Params", "Invalid path parameters").into_response(),
    };
    let ws = if matches!(meta.op, Op::PtyConnect | Op::ExperimentalPersistentPtyConnect) {
        axum::extract::ws::WebSocketUpgrade::from_request_parts(&mut parts, &()).await.ok()
    } else {
        None
    };
    let body = match axum::body::to_bytes(body, BODY_LIMIT).await {
        Ok(b) => b,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let query = parse_query(parts.uri.query());
    let location: LocationRef = request_ref(&query, &parts.headers, &app.meta.cwd);
    let call = Call {
        ctx: RequestContext {
            location,
            scope: meta.scope,
            operation: meta.id,
            headers: parts.headers.clone(),
            server: app.meta.clone(),
        },
        path,
        query,
        headers: parts.headers,
        method: parts.method,
        body,
    };
    if let Err(e) = check_scope(&app, meta, &call) {
        return e.into_response();
    }
    let result = if meta.special {
        crate::handlers::special(&app, meta, call, parts.uri, ws).await
    } else {
        crate::generated::dispatch(app.backend.as_ref(), meta.op, &call).await
    };
    match result {
        Ok(res) => res,
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_numbers() {
        assert_eq!(parse_js_number("20"), Some(20.0));
        assert_eq!(parse_js_number(" 1e3 "), Some(1000.0));
        assert_eq!(parse_js_number(""), Some(0.0));
        assert_eq!(parse_js_number("abc"), None);
        assert_eq!(parse_js_number("-Infinity"), Some(f64::NEG_INFINITY));
    }

    #[test]
    fn query_pairs_decode_deep_object_keys() {
        let q = parse_query(Some("location%5Bdirectory%5D=%2Ftmp%2Fa+b&limit=2"));
        assert_eq!(q[0], ("location[directory]".to_string(), "/tmp/a b".to_string()));
        assert_eq!(q[1].1, "2");
    }
}
