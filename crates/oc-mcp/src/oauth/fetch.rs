//! `loggedFetch` (`packages/core/src/mcp/oauth.ts:30-103`): every MCP HTTP request goes through
//! here so OAuth token requests and authentication rejections are logged, and concurrent refreshes
//! of the same (rotating) refresh token share one request.

use std::collections::HashMap;
use std::time::Instant;

use bytes::Bytes;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::McpError;

/// Request body.
#[derive(Debug, Clone)]
pub enum Body {
    Json(String),
    /// `application/x-www-form-urlencoded` parameters in order (a `URLSearchParams`).
    Form(Vec<(String, String)>),
}

impl Body {
    fn form_value(&self, key: &str) -> Option<&str> {
        match self {
            Body::Form(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: url::Url,
    /// Header names are case-insensitive; later entries replace earlier ones.
    pub headers: Vec<(String, String)>,
    pub body: Option<Body>,
}

impl HttpRequest {
    pub fn get(url: url::Url) -> Self {
        Self { method: Method::GET, url, headers: vec![], body: None }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

/// A fully buffered response (`Response` after `.text()`).
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl HttpResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Result<Value, McpError> {
        serde_json::from_slice(&self.body).map_err(|e| McpError::other(format!("SyntaxError: {e}")))
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.get(name).and_then(|v| v.to_str().ok()).map(String::from)
    }
}

type SharedResponse = Shared<BoxFuture<'static, Result<HttpResponse, McpError>>>;

// Refresh tokens rotate, so concurrent refreshes of the same token share one request or the
// second gets invalid_grant.
static REFRESHES: Lazy<Mutex<HashMap<String, SharedResponse>>> = Lazy::new(|| Mutex::new(HashMap::new()));

static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::limited(20)).build().expect("reqwest client")
});

/// The process-wide HTTP client.
pub fn http_client() -> reqwest::Client {
    CLIENT.clone()
}

fn refresh_key(request: &HttpRequest) -> Option<String> {
    let body = request.body.as_ref()?;
    if body.form_value("grant_type") != Some("refresh_token") {
        return None;
    }
    Some(
        [
            request.url.as_str(),
            body.form_value("client_id").unwrap_or(""),
            body.form_value("refresh_token").unwrap_or(""),
        ]
        .join("\u{0}"),
    )
}

/// `application/x-www-form-urlencoded` serialization (`URLSearchParams.toString()`).
pub fn encode_form(pairs: &[(String, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        serializer.append_pair(k, v);
    }
    serializer.finish()
}

fn build(client: &reqwest::Client, request: &HttpRequest) -> reqwest::RequestBuilder {
    let mut headers = HeaderMap::new();
    for (name, value) in &request.headers {
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
            headers.insert(name, value);
        }
    }
    let mut builder = client.request(request.method.clone(), request.url.clone());
    match &request.body {
        Some(Body::Json(text)) => {
            builder = builder.body(text.clone());
        }
        Some(Body::Form(pairs)) => {
            if !headers.contains_key(reqwest::header::CONTENT_TYPE) {
                headers.insert(
                    reqwest::header::CONTENT_TYPE,
                    HeaderValue::from_static("application/x-www-form-urlencoded;charset=UTF-8"),
                );
            }
            builder = builder.body(encode_form(pairs));
        }
        None => {}
    }
    builder.headers(headers)
}

async fn send_buffered(request: HttpRequest) -> Result<HttpResponse, McpError> {
    let response = build(&CLIENT, &request).send().await.map_err(|e| McpError::from_reqwest(&e))?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.bytes().await.map_err(|e| McpError::from_reqwest(&e))?;
    Ok(HttpResponse {
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or("").to_string(),
        headers,
        body,
    })
}

/// `send`: refresh requests are de-duplicated process-wide by `url \0 client_id \0 refresh_token`.
async fn send(request: HttpRequest) -> Result<HttpResponse, McpError> {
    let Some(key) = refresh_key(&request) else { return send_buffered(request).await };
    let shared = {
        let mut map = REFRESHES.lock();
        if let Some(current) = map.get(&key) {
            current.clone()
        } else {
            let fut: BoxFuture<'static, Result<HttpResponse, McpError>> = send_buffered(request).boxed();
            let shared = fut.shared();
            map.insert(key.clone(), shared.clone());
            shared
        }
    };
    let result = shared.clone().await;
    let mut map = REFRESHES.lock();
    if map.get(&key).is_some_and(|current| current.ptr_eq(&shared)) {
        map.remove(&key);
    }
    result
}

/// Fetch used for MCP traffic of one server: logging, refresh de-duplication, and optional base
/// headers merged into every request (`createFetchWithInit` with the configured headers).
#[derive(Clone)]
pub struct Fetcher {
    pub server: String,
    pub directory: Option<String>,
    pub base_headers: Option<IndexMap<String, String>>,
    /// Extra structured fields for logs (e.g. the OAuth attempt id).
    pub fields: Vec<(String, String)>,
}

impl Fetcher {
    pub fn new(server: impl Into<String>) -> Self {
        Self { server: server.into(), directory: None, base_headers: None, fields: vec![] }
    }

    pub fn with_base_headers(mut self, headers: Option<IndexMap<String, String>>) -> Self {
        self.base_headers = headers;
        self
    }

    fn merge_base(&self, mut request: HttpRequest) -> HttpRequest {
        if let Some(base) = &self.base_headers {
            // `createFetchWithInit`: `{...baseInit.headers, ...init.headers}`.
            let mut headers: Vec<(String, String)> = base.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            for (k, v) in request.headers.drain(..) {
                headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(&k));
                headers.push((k, v));
            }
            request.headers = headers;
        }
        request
    }

    fn log_context(&self, request: &HttpRequest) -> (String, Option<&'static str>) {
        let grant = request.body.as_ref().and_then(|b| b.form_value("grant_type"));
        let operation = match grant {
            Some("refresh_token") => Some("refresh"),
            Some("authorization_code") => Some("exchange"),
            _ => None,
        };
        (uuid::Uuid::new_v4().to_string(), operation)
    }

    /// Buffered fetch (OAuth discovery, registration and token requests, session termination).
    pub async fn fetch(&self, request: HttpRequest) -> Result<HttpResponse, McpError> {
        let request = self.merge_base(request);
        let (request_id, operation) = self.log_context(&request);
        let origin = request.url.origin().ascii_serialization();
        let method = request.method.to_string();
        let span = tracing::info_span!(
            "mcp_http",
            server = %self.server,
            requestID = %request_id,
            origin = %origin,
            method = %method,
            operation = operation.unwrap_or(""),
        );
        let _guard = span.enter();
        let started = Instant::now();
        if operation.is_some() {
            tracing::info!("mcp oauth request started");
        }
        drop(_guard);
        let result = send(request).await;
        let _guard = span.enter();
        let duration_ms = started.elapsed().as_millis() as u64;
        match &result {
            Ok(response) => {
                if operation.is_some() && !response.ok() {
                    let error = parse_error_response(response.status, &response.text());
                    let (code, message) = match &error {
                        McpError::OAuth { code, message } => (code.clone(), message.clone()),
                        other => (String::new(), other.to_string()),
                    };
                    tracing::warn!(status = response.status, durationMs = duration_ms, error = %code, message = %message, "mcp oauth request rejected");
                }
                if operation.is_some() && response.ok() {
                    tracing::info!(status = response.status, durationMs = duration_ms, "mcp oauth request succeeded");
                }
                if operation.is_none() && (response.status == 401 || response.status == 403) {
                    tracing::warn!(status = response.status, durationMs = duration_ms, "mcp http authentication rejected");
                }
            }
            Err(error) => {
                tracing::warn!(errors = %error, durationMs = duration_ms, "mcp http request failed");
            }
        }
        result
    }

    /// Streaming fetch (MCP POST/GET); the caller reads the body. Cancelling `cancel` aborts.
    pub async fn fetch_stream(
        &self,
        request: HttpRequest,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response, McpError> {
        let request = self.merge_base(request);
        let (request_id, _) = self.log_context(&request);
        let origin = request.url.origin().ascii_serialization();
        let method = request.method.to_string();
        let started = Instant::now();
        let builder = build(&CLIENT, &request);
        let result = tokio::select! {
            _ = cancel.cancelled() => {
                tracing::debug!(server = %self.server, requestID = %request_id, origin = %origin, method = %method, "mcp http request aborted");
                return Err(McpError::Aborted("This operation was aborted".into()));
            }
            result = builder.send() => result,
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(response) => {
                let status = response.status().as_u16();
                if status == 401 || status == 403 {
                    tracing::warn!(server = %self.server, requestID = %request_id, origin = %origin, method = %method, status, durationMs = duration_ms, "mcp http authentication rejected");
                }
                Ok(response)
            }
            Err(error) => {
                let error = McpError::from_reqwest(&error);
                tracing::warn!(server = %self.server, requestID = %request_id, origin = %origin, method = %method, errors = %error, durationMs = duration_ms, "mcp http request failed");
                Err(error)
            }
        }
    }
}

/// `parseErrorResponse`: a standard OAuth error body becomes `OAuthError(error, error_description ?? error)`;
/// anything else is a `server_error` describing the raw body.
pub fn parse_error_response(status: u16, body: &str) -> McpError {
    match serde_json::from_str::<Value>(body) {
        Ok(Value::Object(obj)) if obj.get("error").is_some_and(Value::is_string) => {
            let code = obj["error"].as_str().unwrap().to_string();
            let message = obj
                .get("error_description")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| code.clone());
            McpError::OAuth { code, message }
        }
        Ok(_) => McpError::OAuth {
            code: "server_error".into(),
            message: format!("HTTP {status}: Invalid OAuth error response: ZodError: error is required. Raw body: {body}"),
        },
        Err(error) => McpError::OAuth {
            code: "server_error".into(),
            message: format!("HTTP {status}: Invalid OAuth error response: SyntaxError: {error}. Raw body: {body}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_encoding_matches_url_search_params() {
        assert_eq!(
            encode_form(&[("a".into(), "x y".into()), ("b".into(), "1/2*~".into())]),
            "a=x+y&b=1%2F2*%7E"
        );
    }

    #[test]
    fn error_response_parsing() {
        match parse_error_response(400, r#"{"error":"invalid_grant"}"#) {
            McpError::OAuth { code, message } => {
                assert_eq!(code, "invalid_grant");
                assert_eq!(message, "invalid_grant");
            }
            other => panic!("{other:?}"),
        }
        match parse_error_response(500, "boom") {
            McpError::OAuth { code, message } => {
                assert_eq!(code, "server_error");
                assert!(message.starts_with("HTTP 500: Invalid OAuth error response:"));
                assert!(message.ends_with("Raw body: boom"));
            }
            other => panic!("{other:?}"),
        }
    }
}
