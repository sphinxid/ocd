//! Streamable HTTP client transport (SDK `StreamableHTTPClientTransport`): JSON-RPC over POST
//! with JSON or SSE responses, `Mcp-Session-Id` / `Mcp-Protocol-Version` headers, the
//! standalone GET SSE stream for server notifications, SSE resumption/reconnection, OAuth on
//! 401 (and 403 `insufficient_scope` step-up) and session termination via DELETE.
//! There is no legacy HTTP+SSE fallback (v2 removed it).

use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use futures::StreamExt;
use futures::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::Mutex;
use reqwest::Method;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{EventSink, SendOptions, Transport, TransportEvent, TransportKind};
use crate::error::{McpError, SdkErrorCode};
use crate::jsonrpc;
use crate::oauth::discovery::parse_www_authenticate;
use crate::oauth::fetch::{Body, Fetcher, HttpRequest};
use crate::oauth::{AuthOptions, AuthResult, Provider, auth};

const PROTOCOL_VERSION_META_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const RESERVED_HEADERS: [&str; 6] =
    ["authorization", "content-type", "mcp-protocol-version", "mcp-method", "mcp-name", "mcp-session-id"];
const INITIAL_RECONNECTION_DELAY_MS: f64 = 1000.0;
const MAX_RECONNECTION_DELAY_MS: f64 = 30000.0;
const RECONNECTION_GROW_FACTOR: f64 = 1.5;
const MAX_RECONNECTION_RETRIES: u32 = 2;
const MAX_STEP_UP_RETRIES: u32 = 1;

pub struct HttpOptions {
    pub url: Url,
    /// Configured request headers (`requestInit.headers`).
    pub headers: Option<IndexMap<String, String>>,
    pub auth_provider: Option<Arc<Provider>>,
    pub fetch: Fetcher,
}

type Headers = Vec<(String, String)>;

fn header_get<'a>(headers: &'a Headers, name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

fn header_set(headers: &mut Headers, name: &str, value: &str) {
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    headers.push((name.to_string(), value.to_string()));
}

fn header_delete(headers: &mut Headers, name: &str) {
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
}

fn merge_accept(headers: &mut Headers, extra: &[&str]) {
    let mut types: Vec<String> = header_get(headers, "accept")
        .map(|a| a.split(',').map(|s| s.trim().to_lowercase()).collect())
        .unwrap_or_default();
    types.extend(extra.iter().map(|s| s.to_string()));
    let mut seen = Vec::new();
    for t in types {
        if !seen.contains(&t) {
            seen.push(t);
        }
    }
    header_set(headers, "accept", &seen.join(", "));
}

/// `computeScopeUnion`.
pub fn scope_union(scopes: &[Option<&str>]) -> Option<String> {
    let mut seen: Vec<String> = Vec::new();
    for scope in scopes.iter().flatten() {
        for token in scope.split_whitespace() {
            if !seen.iter().any(|s| s == token) {
                seen.push(token.to_string());
            }
        }
    }
    (!seen.is_empty()).then(|| seen.join(" "))
}

fn is_strict_scope_superset(union: Option<&str>, current: Option<&str>) -> bool {
    let Some(union) = union else { return false };
    let current: Vec<&str> = current.unwrap_or("").split_whitespace().collect();
    union.split_whitespace().any(|t| !current.contains(&t))
}

/// `encodeMcpParamValue`.
fn encode_param_value(value: &str) -> String {
    let needs = value.is_empty()
        || (value.starts_with("=?base64?") && value.ends_with("?="))
        || value != value.trim()
        || value.chars().any(|c| !(c == '\t' || (' '..='~').contains(&c)));
    if needs {
        format!("=?base64?{}?=", base64::engine::general_purpose::STANDARD.encode(value.as_bytes()))
    } else {
        value.to_string()
    }
}

/// Media type essence of a `Content-Type` header.
fn media_type(header: Option<&str>) -> Option<String> {
    let header = header?;
    let essence = header.split(';').next().unwrap_or("").trim().to_lowercase();
    (!essence.is_empty()).then_some(essence)
}

#[derive(Clone, Default)]
struct SseOptions {
    resumption_token: Option<String>,
    replay_message_id: Option<Value>,
    request_cancel: Option<CancellationToken>,
    on_stream_end: Option<Arc<dyn Fn() + Send + Sync>>,
}

pub struct HttpTransport {
    url: Url,
    headers: Option<IndexMap<String, String>>,
    provider: Option<Arc<Provider>>,
    fetch: Fetcher,
    auth_fetch: Fetcher,
    events: EventSink,
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
    abort: Mutex<Option<CancellationToken>>,
    resource_metadata_url: Mutex<Option<Url>>,
    scope: Mutex<Option<String>>,
    server_retry_ms: Mutex<Option<u64>>,
    this: Weak<HttpTransport>,
}

impl HttpTransport {
    pub fn new(options: HttpOptions, events: EventSink) -> Arc<Self> {
        let auth_fetch = options.fetch.clone().with_base_headers(options.headers.clone());
        Arc::new_cyclic(|this| Self {
            url: options.url,
            headers: options.headers,
            provider: options.auth_provider,
            fetch: options.fetch,
            auth_fetch,
            events,
            session_id: Mutex::new(None),
            protocol_version: Mutex::new(None),
            abort: Mutex::new(None),
            resource_metadata_url: Mutex::new(None),
            scope: Mutex::new(None),
            server_retry_ms: Mutex::new(None),
            this: this.clone(),
        })
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    fn emit(&self, event: TransportEvent) {
        let _ = self.events.send(event);
    }

    fn abort_token(&self) -> CancellationToken {
        self.abort.lock().clone().unwrap_or_default()
    }

    fn intentional_abort(&self, request: Option<&CancellationToken>) -> bool {
        self.abort.lock().as_ref().is_some_and(|t| t.is_cancelled()) || request.is_some_and(|t| t.is_cancelled())
    }

    /// A token cancelled when either the transport or the request is aborted.
    fn signal(&self, request: Option<&CancellationToken>) -> CancellationToken {
        let token = self.abort_token().child_token();
        if let Some(request) = request {
            let request = request.clone();
            let token2 = token.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = request.cancelled() => token2.cancel(),
                    _ = token2.cancelled() => {}
                }
            });
        }
        token
    }

    async fn common_headers(&self) -> Result<Headers, McpError> {
        let mut headers: Headers = Vec::new();
        if let Some(provider) = &self.provider {
            if let Some(token) = provider.tokens().await?.map(|t| t.access_token).filter(|t| !t.is_empty()) {
                headers.push(("Authorization".into(), format!("Bearer {token}")));
            }
        }
        if let Some(session) = self.session_id.lock().clone() {
            headers.push(("mcp-session-id".into(), session));
        }
        if let Some(version) = self.protocol_version.lock().clone() {
            headers.push(("mcp-protocol-version".into(), version));
        }
        for (k, v) in self.headers.iter().flatten() {
            header_set(&mut headers, k, v);
        }
        Ok(headers)
    }

    /// `onUnauthorized` for OAuth providers: run `auth()` with the challenge's parameters.
    async fn on_unauthorized(&self, provider: &Provider, scope: Option<String>, rm: Option<Url>) -> Result<(), McpError> {
        let result = auth(
            provider,
            &AuthOptions { server_url: Some(self.url.clone()), resource_metadata_url: rm, scope, ..Default::default() },
            &self.auth_fetch,
        )
        .await?;
        if result != AuthResult::Authorized {
            return Err(McpError::Unauthorized("Unauthorized".into()));
        }
        Ok(())
    }

    async fn step_up(&self, scope: Option<String>, rm: Option<Url>, status_text: &str, text: &str, retries: u32) -> Result<(), McpError> {
        let Some(provider) = &self.provider else {
            return Err(McpError::other(format!(
                "Insufficient scope{}",
                scope.as_ref().map(|s| format!(": required \"{s}\"")).unwrap_or_default()
            )));
        };
        if retries >= MAX_STEP_UP_RETRIES {
            return Err(McpError::Http {
                code: SdkErrorCode::ClientHttpForbidden,
                message: format!(
                    "Server returned 403 insufficient_scope after step-up re-authorization (retry limit {MAX_STEP_UP_RETRIES} reached)"
                ),
                status: 403,
                status_text: if status_text.is_empty() { "Forbidden".into() } else { status_text.into() },
                text: Some(text.into()),
            });
        }
        if rm.is_some() {
            *self.resource_metadata_url.lock() = rm.clone();
        }
        let tokens = provider.tokens().await?;
        let current = tokens.as_ref().and_then(|t| t.scope.clone());
        let previous = self.scope.lock().clone();
        let union = scope_union(&[previous.as_deref(), current.as_deref(), scope.as_deref()]);
        *self.scope.lock() = union.clone();
        let force = is_strict_scope_superset(union.as_deref(), current.as_deref());
        let resource_metadata_url = self.resource_metadata_url.lock().clone();
        let result = auth(
            provider,
            &AuthOptions {
                server_url: Some(self.url.clone()),
                resource_metadata_url,
                scope: union,
                force_reauthorization: force,
                ..Default::default()
            },
            &self.auth_fetch,
        )
        .await?;
        if result != AuthResult::Authorized {
            return Err(McpError::Unauthorized("Unauthorized".into()));
        }
        Ok(())
    }

    fn note_challenge(&self, headers: &reqwest::header::HeaderMap) -> (Option<String>, Option<Url>) {
        let challenge = parse_www_authenticate(headers);
        if headers.contains_key("www-authenticate") {
            *self.resource_metadata_url.lock() = challenge.resource_metadata_url.clone();
            let previous = self.scope.lock().clone();
            *self.scope.lock() = scope_union(&[previous.as_deref(), challenge.scope.as_deref()]);
        }
        (challenge.scope, challenge.resource_metadata_url)
    }

    fn send_inner(
        self: Arc<Self>,
        message: Value,
        options: SendOptions,
        is_auth_retry: bool,
        step_up: u32,
    ) -> BoxFuture<'static, Result<(), McpError>> {
        Box::pin(async move {
            let result = self.clone().send_attempt(message, options.clone(), is_auth_retry, step_up).await;
            if let Err(error) = &result {
                if !options.request_cancel.as_ref().is_some_and(|t| t.is_cancelled()) && !matches!(error, McpError::Aborted(_)) {
                    self.emit(TransportEvent::Error(error.clone()));
                }
            }
            result
        })
    }

    async fn send_attempt(
        self: Arc<Self>,
        message: Value,
        options: SendOptions,
        is_auth_retry: bool,
        step_up: u32,
    ) -> Result<(), McpError> {
        let mut headers = self.common_headers().await?;
        // Body-derived headers for modern enveloped requests.
        if jsonrpc::is_request(&message) {
            if let Some(version) = message
                .get("params")
                .and_then(|p| p.get("_meta"))
                .and_then(|m| m.get(PROTOCOL_VERSION_META_KEY))
                .and_then(Value::as_str)
            {
                header_set(&mut headers, "mcp-protocol-version", version);
                let method = message["method"].as_str().unwrap_or("");
                header_set(&mut headers, "mcp-method", method);
                let params = message.get("params");
                let name = if method == "resources/read" {
                    params.and_then(|p| p.get("uri")).and_then(Value::as_str)
                } else {
                    params.and_then(|p| p.get("name")).and_then(Value::as_str)
                };
                if let Some(name) = name {
                    header_set(&mut headers, "mcp-name", &encode_param_value(name));
                }
            }
        }
        let is_handshake = jsonrpc::is_initialize_request(&message);
        if is_handshake {
            header_delete(&mut headers, "mcp-session-id");
        }
        for (name, value) in &options.headers {
            if RESERVED_HEADERS.contains(&name.to_lowercase().as_str()) {
                continue;
            }
            header_set(&mut headers, name, value);
        }
        header_set(&mut headers, "content-type", "application/json");
        merge_accept(&mut headers, &["application/json", "text/event-stream"]);
        let signal = self.signal(options.request_cancel.as_ref());
        let request = HttpRequest {
            method: Method::POST,
            url: self.url.clone(),
            headers,
            body: Some(Body::Json(message.to_string())),
        };
        let response = self.fetch.fetch_stream(request, &signal).await?;
        let status = response.status().as_u16();
        let status_text = response.status().canonical_reason().unwrap_or("").to_string();
        let ok = response.status().is_success();
        if is_handshake && ok {
            *self.session_id.lock() = response
                .headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.is_empty())
                .map(String::from);
        }
        if !ok {
            if status == 401 {
                if let Some(provider) = self.provider.clone() {
                    let (scope, rm) = self.note_challenge(response.headers());
                    if !is_auth_retry {
                        self.on_unauthorized(&provider, scope, rm).await?;
                        drop(response);
                        return self.clone().send_inner(message, options, true, step_up).await;
                    }
                    drop(response);
                    return Err(McpError::Http {
                        code: SdkErrorCode::ClientHttpAuthentication,
                        message: "Server returned 401 after re-authentication".into(),
                        status: 401,
                        status_text,
                        text: None,
                    });
                }
            }
            let response_headers = response.headers().clone();
            let text = response.text().await.unwrap_or_default();
            if status == 403 {
                let challenge = parse_www_authenticate(&response_headers);
                if challenge.error.as_deref() == Some("insufficient_scope") {
                    self.step_up(challenge.scope, challenge.resource_metadata_url, &status_text, &text, step_up).await?;
                    return self.clone().send_inner(message, options, is_auth_retry, step_up + 1).await;
                }
            }
            if status == 400 && is_modern_enveloped(&message) {
                if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                    if jsonrpc::validate_message(&parsed).is_ok()
                        && jsonrpc::is_error_response(&parsed)
                        && parsed.get("id") == message.get("id")
                    {
                        self.emit(TransportEvent::Message(parsed));
                        return Ok(());
                    }
                }
            }
            return Err(McpError::Http {
                code: SdkErrorCode::ClientHttpNotImplemented,
                message: format!("Error POSTing to endpoint: {text}"),
                status,
                status_text,
                text: Some(text),
            });
        }
        if status == 202 {
            drop(response);
            if jsonrpc::is_initialized_notification(&message) {
                let this = self.clone();
                tokio::spawn(async move {
                    if let Err(error) = this.clone().start_or_auth_sse(SseOptions::default(), false, 0).await {
                        this.emit(TransportEvent::Error(error));
                    }
                });
            }
            return Ok(());
        }
        let has_requests = jsonrpc::is_request(&message);
        let content_type = response.headers().get("content-type").and_then(|v| v.to_str().ok()).map(String::from);
        if !has_requests {
            let _ = response.bytes().await;
            return Ok(());
        }
        match media_type(content_type.as_deref()).as_deref() {
            Some("text/event-stream") => {
                self.clone().handle_sse_stream(
                    response,
                    SseOptions {
                        request_cancel: options.request_cancel.clone(),
                        on_stream_end: options.on_stream_end.clone(),
                        ..Default::default()
                    },
                    false,
                );
                Ok(())
            }
            Some("application/json") => {
                let body = response.bytes().await.map_err(|e| McpError::from_reqwest(&e))?;
                let data: Value =
                    serde_json::from_slice(&body).map_err(|e| McpError::other(format!("SyntaxError: {e}")))?;
                let messages = match data {
                    Value::Array(items) => items,
                    other => vec![other],
                };
                for msg in &messages {
                    jsonrpc::validate_message(msg)?;
                }
                for msg in messages {
                    self.emit(TransportEvent::Message(msg));
                }
                Ok(())
            }
            _ => {
                let _ = response.bytes().await;
                Err(McpError::sdk(
                    SdkErrorCode::ClientHttpUnexpectedContent,
                    format!("Unexpected content type: {}", content_type.as_deref().unwrap_or("null")),
                ))
            }
        }
    }

    /// `_startOrAuthSse`: opens the GET SSE stream (standalone, or a resumption).
    fn start_or_auth_sse(
        self: Arc<Self>,
        options: SseOptions,
        is_auth_retry: bool,
        step_up: u32,
    ) -> BoxFuture<'static, Result<(), McpError>> {
        Box::pin(async move {
            let result = self.clone().sse_attempt(options.clone(), is_auth_retry, step_up).await;
            if let Err(error) = &result {
                if !self.intentional_abort(options.request_cancel.as_ref()) && !matches!(error, McpError::Aborted(_)) {
                    self.emit(TransportEvent::Error(error.clone()));
                }
            }
            result
        })
    }

    async fn sse_attempt(self: Arc<Self>, options: SseOptions, is_auth_retry: bool, step_up: u32) -> Result<(), McpError> {
        let mut headers = self.common_headers().await?;
        merge_accept(&mut headers, &["text/event-stream"]);
        if let Some(token) = &options.resumption_token {
            header_set(&mut headers, "last-event-id", token);
        }
        let signal = self.signal(options.request_cancel.as_ref());
        let request = HttpRequest { method: Method::GET, url: self.url.clone(), headers, body: None };
        let response = self.fetch.fetch_stream(request, &signal).await?;
        let status = response.status().as_u16();
        let status_text = response.status().canonical_reason().unwrap_or("").to_string();
        if !response.status().is_success() {
            if status == 401 {
                if let Some(provider) = self.provider.clone() {
                    let (scope, rm) = self.note_challenge(response.headers());
                    drop(response);
                    if !is_auth_retry {
                        self.on_unauthorized(&provider, scope, rm).await?;
                        return self.clone().start_or_auth_sse(options, true, step_up).await;
                    }
                    return Err(McpError::Http {
                        code: SdkErrorCode::ClientHttpAuthentication,
                        message: "Server returned 401 after re-authentication".into(),
                        status: 401,
                        status_text,
                        text: None,
                    });
                }
            }
            if status == 403 {
                let challenge = parse_www_authenticate(response.headers());
                if challenge.error.as_deref() == Some("insufficient_scope") {
                    let text = response.text().await.unwrap_or_default();
                    self.step_up(challenge.scope, challenge.resource_metadata_url, &status_text, &text, step_up).await?;
                    return self.clone().start_or_auth_sse(options, is_auth_retry, step_up + 1).await;
                }
            }
            let _ = response.bytes().await;
            if status == 405 {
                if let Some(end) = &options.on_stream_end {
                    end();
                }
                return Ok(());
            }
            return Err(McpError::Http {
                code: SdkErrorCode::ClientHttpFailedToOpenStream,
                message: format!("Failed to open SSE stream: {status_text}"),
                status,
                status_text,
                text: None,
            });
        }
        self.handle_sse_stream(response, options, true);
        Ok(())
    }

    fn reconnection_delay(&self, attempt: u32) -> Duration {
        if let Some(ms) = *self.server_retry_ms.lock() {
            return Duration::from_millis(ms);
        }
        let delay = INITIAL_RECONNECTION_DELAY_MS * RECONNECTION_GROW_FACTOR.powi(attempt as i32);
        Duration::from_millis(delay.min(MAX_RECONNECTION_DELAY_MS) as u64)
    }

    fn schedule_reconnection(self: Arc<Self>, options: SseOptions, attempt: u32) {
        if attempt >= MAX_RECONNECTION_RETRIES {
            self.emit(TransportEvent::Error(McpError::other(format!(
                "Maximum reconnection attempts ({MAX_RECONNECTION_RETRIES}) exceeded."
            ))));
            if let Some(end) = &options.on_stream_end {
                end();
            }
            return;
        }
        let delay = self.reconnection_delay(attempt);
        let abort = self.abort_token();
        tokio::spawn(async move {
            tokio::select! {
                _ = abort.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            if self.intentional_abort(options.request_cancel.as_ref()) {
                return;
            }
            if let Err(error) = self.clone().start_or_auth_sse(options.clone(), false, 0).await {
                if self.intentional_abort(options.request_cancel.as_ref()) {
                    return;
                }
                self.emit(TransportEvent::Error(McpError::other(format!("Failed to reconnect SSE stream: {error}"))));
                self.schedule_reconnection(options, attempt + 1);
            }
        });
    }

    fn handle_sse_stream(self: Arc<Self>, response: reqwest::Response, options: SseOptions, reconnectable: bool) {
        tokio::spawn(async move {
            let abort = self.abort_token();
            let request_cancel = options.request_cancel.clone().unwrap_or_default();
            let mut stream = response.bytes_stream();
            let mut parser = SseParser::default();
            let mut last_event_id: Option<String> = None;
            let mut has_priming = false;
            let mut received_response = false;
            let mut failure: Option<String> = None;
            loop {
                let chunk = tokio::select! {
                    _ = abort.cancelled() => return,
                    _ = request_cancel.cancelled() => return,
                    chunk = stream.next() => chunk,
                };
                let bytes = match chunk {
                    None => break,
                    Some(Err(error)) => {
                        failure = Some(McpError::from_reqwest(&error).to_string());
                        break;
                    }
                    Some(Ok(bytes)) => bytes,
                };
                for event in parser.push(&bytes) {
                    if let Some(retry) = event.retry {
                        *self.server_retry_ms.lock() = Some(retry);
                    }
                    if let Some(id) = event.id.clone().filter(|i| !i.is_empty()) {
                        last_event_id = Some(id);
                        has_priming = true;
                    }
                    if event.data.is_empty() {
                        continue;
                    }
                    if event.event.is_empty() || event.event == "message" {
                        match serde_json::from_str::<Value>(&event.data)
                            .map_err(|e| McpError::other(format!("SyntaxError: {e}")))
                            .and_then(|m| jsonrpc::validate_message(&m).map(|_| m))
                        {
                            Ok(mut message) => {
                                if jsonrpc::is_response(&message) {
                                    received_response = true;
                                    if let Some(id) = &options.replay_message_id {
                                        message["id"] = id.clone();
                                    }
                                }
                                self.emit(TransportEvent::Message(message));
                            }
                            Err(error) => self.emit(TransportEvent::Error(error)),
                        }
                    }
                }
            }
            if self.intentional_abort(options.request_cancel.as_ref()) {
                return;
            }
            if let Some(error) = failure {
                self.emit(TransportEvent::Error(McpError::other(format!("SSE stream disconnected: Error: {error}"))));
            }
            let next = SseOptions { resumption_token: last_event_id, ..options.clone() };
            if (reconnectable || has_priming) && !received_response {
                self.schedule_reconnection(next, 0);
            } else if let Some(end) = &options.on_stream_end {
                end();
            }
        });
    }
}

fn is_modern_enveloped(message: &Value) -> bool {
    jsonrpc::is_request(message)
        && message
            .get("params")
            .and_then(|p| p.get("_meta"))
            .and_then(|m| m.get(PROTOCOL_VERSION_META_KEY))
            .and_then(Value::as_str)
            .is_some_and(|v| v >= "2026-07-28")
}

#[async_trait]
impl Transport for HttpTransport {
    async fn start(&self) -> Result<(), McpError> {
        let mut abort = self.abort.lock();
        if abort.is_some() {
            return Err(McpError::other(
                "StreamableHTTPClientTransport already started! If using Client class, note that connect() calls start() automatically.",
            ));
        }
        *abort = Some(CancellationToken::new());
        Ok(())
    }

    async fn send(&self, message: Value, options: SendOptions) -> Result<(), McpError> {
        let this = self.this.upgrade().ok_or_else(|| McpError::other("Not connected"))?;
        this.send_inner(message, options, false, 0).await
    }

    async fn close(&self) {
        if let Some(abort) = self.abort.lock().as_ref() {
            abort.cancel();
        }
        self.emit(TransportEvent::Closed);
    }

    fn session_id(&self) -> Option<String> {
        self.session_id.lock().clone()
    }

    fn set_protocol_version(&self, version: &str) {
        *self.protocol_version.lock() = Some(version.to_string());
    }

    async fn terminate_session(&self) -> Result<(), McpError> {
        if self.session_id.lock().is_none() {
            return Ok(());
        }
        let result = async {
            let headers = self.common_headers().await?;
            let request = HttpRequest { method: Method::DELETE, url: self.url.clone(), headers, body: None };
            let abort = self.abort_token();
            let response = self.fetch.fetch_stream(request, &abort).await?;
            let status = response.status();
            let _ = response.bytes().await;
            if !status.is_success() && status.as_u16() != 405 {
                return Err(McpError::Http {
                    code: SdkErrorCode::ClientHttpFailedToTerminateSession,
                    message: format!("Failed to terminate session: {}", status.canonical_reason().unwrap_or("")),
                    status: status.as_u16(),
                    status_text: status.canonical_reason().unwrap_or("").to_string(),
                    text: None,
                });
            }
            *self.session_id.lock() = None;
            Ok(())
        }
        .await;
        if let Err(error) = &result {
            self.emit(TransportEvent::Error(error.clone()));
        }
        result
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Http
    }

    fn has_per_request_stream(&self) -> bool {
        true
    }
}

/// One dispatched SSE event.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
    pub id: Option<String>,
    pub retry: Option<u64>,
}

/// Minimal WHATWG event-stream parser.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: String,
    pending: Vec<u8>,
    event: String,
    data: Vec<String>,
    id: Option<String>,
    retry: Option<u64>,
    has_fields: bool,
}

impl SseParser {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.pending.extend_from_slice(bytes);
        // Only decode complete UTF-8 prefixes.
        let valid = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(e) => e.valid_up_to(),
        };
        let text = String::from_utf8_lossy(&self.pending[..valid]).into_owned();
        self.pending.drain(..valid);
        self.buffer.push_str(&text);
        let mut events = Vec::new();
        loop {
            let Some(pos) = self.buffer.find(['\n', '\r']) else { break };
            let mut line: String = self.buffer[..pos].to_string();
            let mut consumed = pos + 1;
            if self.buffer.as_bytes()[pos] == b'\r' {
                if pos + 1 == self.buffer.len() {
                    // Might be \r\n split across chunks; wait.
                    break;
                }
                if self.buffer.as_bytes()[pos + 1] == b'\n' {
                    consumed += 1;
                }
            }
            self.buffer.drain(..consumed);
            if line.is_empty() {
                if self.has_fields {
                    events.push(SseEvent {
                        event: std::mem::take(&mut self.event),
                        data: std::mem::take(&mut self.data).join("\n"),
                        id: self.id.clone(),
                        retry: self.retry.take(),
                    });
                    self.has_fields = false;
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.find(':') {
                Some(i) => {
                    let value = line.split_off(i + 1);
                    line.pop();
                    (line, value.strip_prefix(' ').map(String::from).unwrap_or(value))
                }
                None => (line, String::new()),
            };
            match field.as_str() {
                "event" => {
                    self.event = value;
                    self.has_fields = true;
                }
                "data" => {
                    self.data.push(value);
                    self.has_fields = true;
                }
                "id" => {
                    if !value.contains('\0') {
                        self.id = Some(value);
                        self.has_fields = true;
                    }
                }
                "retry" => {
                    if let Ok(ms) = value.parse::<u64>() {
                        self.retry = Some(ms);
                    }
                }
                _ => {}
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parsing() {
        let mut parser = SseParser::default();
        let mut events = parser.push(b"id: 1\ndata:\n\nevent: message\ndata: {\"a\"");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id.as_deref(), Some("1"));
        assert_eq!(events[0].data, "");
        events = parser.push(b":1}\r\ndata: x\r\n\r\nretry: 500\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "message");
        assert_eq!(events[0].data, "{\"a\":1}\nx");
        assert_eq!(events[0].id.as_deref(), Some("1"));
        assert!(parser.push(b"").is_empty());
    }

    #[test]
    fn header_helpers() {
        let mut headers: Headers = vec![("Accept".into(), "Text/Plain, application/json".into())];
        merge_accept(&mut headers, &["application/json", "text/event-stream"]);
        assert_eq!(header_get(&headers, "accept"), Some("text/plain, application/json, text/event-stream"));
        assert_eq!(scope_union(&[Some("a b"), None, Some("b c")]).as_deref(), Some("a b c"));
        assert!(is_strict_scope_superset(Some("a b"), Some("a")));
        assert!(!is_strict_scope_superset(Some("a"), Some("a b")));
        assert_eq!(encode_param_value("plain"), "plain");
        assert_eq!(encode_param_value(" x"), "=?base64?IHg=?=");
        assert_eq!(media_type(Some("Application/JSON; charset=utf-8")).as_deref(), Some("application/json"));
    }
}
