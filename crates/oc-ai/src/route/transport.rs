//! HTTP JSON transport (`route/transport/http.ts`, `route/executor.ts`).

use super::{auth, normalize_headers, set_query, Headers};
use crate::error::{http_failure, http_status_message, AIError, HttpContext, TransportOperation};
use crate::json::{encode, is_record};
use crate::types::{HttpTimeout, LLMRequest, DEFAULT_HTTP_TIMEOUT_MS};
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use indexmap::IndexMap;
use serde_json::Value;
use std::time::Duration;

/// A fully prepared HTTP request.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedRequest {
    pub method: String,
    pub url: String,
    pub headers: Headers,
    /// Encoded body text (after `http.body` overlay).
    pub body: String,
    /// JSON body after overlay.
    pub json_body: Value,
}

/// Hook wrapping every outgoing HTTP request (core `http.request` hooks, Copilot/Azure header injection).
#[async_trait]
pub trait HttpMiddleware: Send + Sync {
    async fn on_request(&self, request: &LLMRequest, prepared: &mut PreparedRequest) -> Result<(), AIError>;
}

/// A request as plugins see it in `session.http.request` / `session.http.response` (a web `Request`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WireRequest {
    pub url: String,
    pub method: String,
    /// Lowercase names.
    pub headers: IndexMap<String, String>,
    pub body: Option<String>,
}

/// A response as plugins see it in `session.http.response` (a web `Response`, body read as text).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WireResponse {
    pub status: u16,
    pub status_text: String,
    /// Lowercase names.
    pub headers: IndexMap<String, String>,
    pub body: Option<String>,
}

/// Per-call HTTP middleware (upstream `StreamOptions.http`, which core builds from the
/// `session.http.request` / `session.http.response` plugin hooks in `session/model-request.ts`).
/// It wraps the executor's send: the request hook sees the prepared request before Effect's client
/// adds its trace headers, the response hook sees the response before the status check. An `Err`
/// fails the call as an HTTP transport error with that message.
#[async_trait]
pub trait HttpHook: Send + Sync {
    async fn request(&self, request: WireRequest) -> Result<WireRequest, String>;
    async fn response(&self, request: &WireRequest, response: WireResponse) -> Result<WireResponse, String>;
    /// False when nothing observes responses: the body then streams through untouched.
    fn wants_response(&self) -> bool {
        true
    }
}

impl WireRequest {
    /// The request as `HttpClientRequest.toWeb` presents it: the prepared headers plus the
    /// `content-length` that `bodyText` sets.
    pub fn from_prepared(prepared: &PreparedRequest) -> Self {
        let mut headers: IndexMap<String, String> =
            prepared.headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.clone())).collect();
        headers.insert("content-length".into(), prepared.body.len().to_string());
        Self { url: prepared.url.clone(), method: prepared.method.clone(), headers, body: Some(prepared.body.clone()) }
    }
}

/// `bodyWithOverlay`.
pub fn body_with_overlay(body: Value, request: &LLMRequest) -> Result<(Value, String), AIError> {
    let Some(overlay) = request.http.as_ref().and_then(|h| h.body.as_ref()) else {
        let text = encode(&body);
        return Ok((body, text));
    };
    if !is_record(&body) {
        return Err(AIError::invalid_request("http.body can only overlay JSON object request bodies"));
    }
    let merged = crate::json::merge_records(&[body.as_object(), Some(overlay)]).unwrap_or_default();
    let value = Value::Object(merged);
    let text = encode(&value);
    Ok((value, text))
}

/// `jsonRequestParts` + `jsonPost`: URL, overlaid body, headers, auth, then `content-type`.
pub async fn prepare_json(request: &LLMRequest, body: Value) -> Result<PreparedRequest, AIError> {
    let route = request.model.route.clone();
    let mut url = route.endpoint.render(request, &body).map_err(AIError::invalid_request)?;
    if let Some(query) = request.http.as_ref().and_then(|h| h.query.as_ref()) {
        set_query(&mut url, query);
    }
    let mut headers: Headers = request.http.as_ref().and_then(|h| h.headers.as_ref()).map(normalize_headers).unwrap_or_default();
    if let Some(hook) = &route.prepare_headers {
        hook(request, &body, &mut headers);
    }
    let (json_body, body_text) = body_with_overlay(body, request)?;
    let url = url.to_string();
    let mut headers = auth::apply(
        route.auth.as_ref(),
        auth::AuthInput { method: "POST", url: &url, body: &body_text, headers },
    )
    .await?;
    headers.insert("content-type".into(), "application/json".into());
    Ok(PreparedRequest { method: "POST".into(), url, headers, body: body_text, json_body })
}

/// Executed response: HTTP context and raw byte stream.
pub struct HttpExecution {
    pub http: HttpContext,
    pub body: BoxStream<'static, Result<Bytes, AIError>>,
}

fn duration(value: Option<HttpTimeout>) -> Option<Duration> {
    match value {
        Some(HttpTimeout::Disabled(_)) => None,
        Some(HttpTimeout::Millis(ms)) => Some(Duration::from_millis(ms as u64)),
        None => Some(Duration::from_millis(DEFAULT_HTTP_TIMEOUT_MS as u64)),
    }
}

/// Header and chunk timeout configuration resolved from `HttpOptions`.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub header: Option<Duration>,
    pub chunk: Option<Duration>,
    pub total: Option<Duration>,
}

impl Timeouts {
    pub fn from_request(request: &LLMRequest) -> Self {
        let http = request.http.as_ref();
        let total = http.and_then(|h| h.timeout).and_then(|t| t.millis()).map(|ms| Duration::from_millis(ms as u64));
        let header = duration(http.and_then(|h| h.header_timeout));
        let header = match (header, total) {
            (Some(h), Some(t)) => Some(h.min(t)),
            (h, t) => h.or(t),
        };
        Self { header, chunk: duration(http.and_then(|h| h.chunk_timeout)), total }
    }
}

fn reqwest_message(error: &reqwest::Error) -> String {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(s) = source {
        let text = s.to_string();
        if !message.contains(&text) {
            message = format!("{message}: {text}");
        }
        source = s.source();
    }
    message
}

fn header_map(headers: &reqwest::header::HeaderMap) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    for (name, value) in headers {
        out.insert(name.as_str().to_lowercase(), String::from_utf8_lossy(value.as_bytes()).into_owned());
    }
    out
}

/// Send the request; on status >= 400 read and classify the body. Applies the header timeout.
pub async fn execute(client: &reqwest::Client, prepared: &PreparedRequest, timeouts: Timeouts) -> Result<HttpExecution, AIError> {
    execute_with(client, prepared, timeouts, None).await
}

/// [`execute`] with an optional [`HttpHook`] around the send (`RequestExecutor.execute(request, middleware)`).
/// Every request carries the `b3`/`traceparent` headers of a new client span
/// ([`oc_util::trace`]), set after the request hook like Effect's traced client does.
pub async fn execute_with(
    client: &reqwest::Client,
    prepared: &PreparedRequest,
    timeouts: Timeouts,
    hook: Option<&dyn HttpHook>,
) -> Result<HttpExecution, AIError> {
    let hooked = match hook {
        None => None,
        Some(hook) => Some(
            hook.request(WireRequest::from_prepared(prepared))
                .await
                .map_err(|message| AIError::transport(TransportOperation::Request, message, None, Some(&prepared.url)))?,
        ),
    };
    let (method, url) = match &hooked {
        Some(w) => (w.method.as_str(), w.url.as_str()),
        None => (prepared.method.as_str(), prepared.url.as_str()),
    };
    let mut builder = client.request(reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::POST), url);
    match &hooked {
        Some(w) => {
            for (k, v) in &w.headers {
                // The client computes the length of the body it actually sends.
                if k != "content-length" && k != "b3" && k != "traceparent" {
                    builder = builder.header(k, v);
                }
            }
            if let Some(body) = &w.body
                && !matches!(w.method.as_str(), "GET" | "HEAD")
            {
                builder = builder.body(body.clone());
            }
        }
        None => {
            for (k, v) in &prepared.headers {
                if !k.eq_ignore_ascii_case("b3") && !k.eq_ignore_ascii_case("traceparent") {
                    builder = builder.header(k, v);
                }
            }
            builder = builder.body(prepared.body.clone());
        }
    }
    for (k, v) in oc_util::trace::headers() {
        builder = builder.header(k, v);
    }
    let send = builder.send();
    let response = match timeouts.header {
        Some(limit) => match tokio::time::timeout(limit, send).await {
            Ok(r) => r,
            Err(_) => {
                return Err(AIError::transport(
                    TransportOperation::Request,
                    "Timed out waiting for response headers",
                    Some("Timeout"),
                    Some(&prepared.url),
                ));
            }
        },
        None => send.await,
    };
    let response = response.map_err(|e| {
        let code = if e.is_timeout() {
            Some("Timeout")
        } else if e.is_connect() {
            Some("ConnectionError")
        } else {
            Some("TransportError")
        };
        AIError::transport(TransportOperation::Request, reqwest_message(&e), code, Some(&prepared.url)).with_cause(&e)
    })?;
    if let (Some(hook), Some(sent)) = (hook, &hooked)
        && hook.wants_response()
    {
        return hooked_response(hook, sent, response).await;
    }
    let status = response.status().as_u16();
    let url = response.url().to_string();
    let headers = header_map(response.headers());
    let http = HttpContext { url: url.clone(), status, headers: headers.clone() };
    if status >= 400 {
        let text = response.text().await.ok();
        let message = http_status_message(status, text.as_deref());
        return Err(http_failure(message, Some(&url), Some(status), &headers, text));
    }
    let read_http = http.clone();
    let body = response
        .bytes_stream()
        .map(move |chunk| {
            chunk.map_err(|e| {
                let detail = reqwest_message(&e);
                AIError::transport(
                    TransportOperation::Read,
                    format!("Connection lost while reading the response: {detail}"),
                    Some("TransportError"),
                    Some(&read_http.url),
                )
                .with_http(Some(read_http.clone()))
            })
        })
        .boxed();
    Ok(HttpExecution { http, body })
}

/// The `session.http.response` half: the response (body read in full, as the plugin reads it) goes
/// through the hook, then the hook's response is status-checked and streamed.
async fn hooked_response(hook: &dyn HttpHook, sent: &WireRequest, response: reqwest::Response) -> Result<HttpExecution, AIError> {
    let status = response.status();
    let wire = WireResponse {
        status: status.as_u16(),
        // `new Response(stream, {status, headers})` in model-request.ts: no status text.
        status_text: String::new(),
        headers: header_map(response.headers()),
        body: None,
    };
    let url = sent.url.clone();
    let bytes = response.bytes().await.map_err(|e| {
        AIError::transport(
            TransportOperation::Read,
            format!("Connection lost while reading the response: {}", reqwest_message(&e)),
            Some("TransportError"),
            Some(&url),
        )
    })?;
    let wire = WireResponse {
        body: if [204, 205, 304].contains(&wire.status) { None } else { Some(String::from_utf8_lossy(&bytes).into_owned()) },
        ..wire
    };
    let out = hook
        .response(sent, wire)
        .await
        .map_err(|message| AIError::transport(TransportOperation::Request, message, None, Some(&url)))?;
    let headers: IndexMap<String, String> = out.headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.clone())).collect();
    let http = HttpContext { url: url.clone(), status: out.status, headers: headers.clone() };
    if out.status >= 400 {
        let message = http_status_message(out.status, out.body.as_deref());
        return Err(http_failure(message, Some(&url), Some(out.status), &headers, out.body));
    }
    let body = futures::stream::iter(out.body.map(|b| Ok(Bytes::from(b.into_bytes())))).boxed();
    Ok(HttpExecution { http, body })
}
