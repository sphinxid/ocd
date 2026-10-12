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
    let mut builder = client.request(
        reqwest::Method::from_bytes(prepared.method.as_bytes()).unwrap_or(reqwest::Method::POST),
        &prepared.url,
    );
    for (k, v) in &prepared.headers {
        builder = builder.header(k, v);
    }
    let builder = builder.body(prepared.body.clone());
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
