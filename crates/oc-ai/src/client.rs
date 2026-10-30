//! `LLMClient`: compile → prepare → HTTP → framing → protocol parser → terminal rule
//! (`route/client.ts`).

use crate::compile::prepare_request;
use crate::error::{AIError, HttpContext, TransportOperation};
use crate::protocols::{route_label, StreamParser};
use crate::response::{LLMResponse, ResponseState};
use crate::route::framing::{Frame, Framer};
use crate::protocols::open_responses_channel::{ChannelOptions, PreparedChannel};
use crate::route::transport::{self, HttpHook, HttpMiddleware, PreparedRequest, Timeouts};
use crate::route::websocket::{ChannelConnect, ChannelExchange, ChannelExecution, ChannelExecutor, FrameStream};
use crate::types::{LLMEvent, LLMRequest};
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Stream of normalized events.
pub type LLMStream = Pin<Box<dyn Stream<Item = Result<LLMEvent, AIError>> + Send>>;

/// A compiled request: the resolved request, protocol body and prepared HTTP request.
#[derive(Debug, Clone)]
pub struct Compiled {
    pub request: LLMRequest,
    pub body: serde_json::Value,
    pub prepared: PreparedRequest,
}

/// Drives framing + parsing + the terminal rule over a byte stream.
pub struct StreamDriver {
    framer: Framer,
    parser: Box<dyn StreamParser>,
    label: String,
    stopped: bool,
    terminal_emitted: bool,
    halted: bool,
    pub http: Option<HttpContext>,
}

impl StreamDriver {
    pub fn new(request: &LLMRequest) -> Self {
        Self {
            framer: Framer::new(&request.model.route.framing),
            parser: request.model.route.protocol.parser(request),
            label: route_label(request),
            stopped: false,
            terminal_emitted: false,
            halted: false,
            http: None,
        }
    }

    /// True once a terminal frame was consumed (no more bytes are needed).
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    fn check(&mut self, events: Vec<LLMEvent>, out: &mut Vec<LLMEvent>) -> Result<(), AIError> {
        for event in events {
            if self.terminal_emitted {
                return Err(AIError::invalid_output(
                    &self.label,
                    format!("Provider emitted {} after the terminal event", event.type_name()),
                )
                .with_http(self.http.clone()));
            }
            if event.is_terminal() {
                self.terminal_emitted = true;
            }
            out.push(event);
        }
        Ok(())
    }

    fn handle_frames(&mut self, frames: Vec<Frame>, out: &mut Vec<LLMEvent>) -> Result<(), AIError> {
        for frame in frames {
            if self.stopped {
                break;
            }
            let step = self.parser.step(&frame).map_err(|mut e| {
                // Keep a body the parser assembled; replace a missing one, or one that is just the event
                // re-serialized, with the frame as it came over the wire (`route/client.ts` `frameError`).
                let reserialized = match (&frame, &e.body) {
                    (Frame::Text(text), Some(body)) => serde_json::from_str::<serde_json::Value>(text)
                        .is_ok_and(|event| crate::json::encode(&event) == *body),
                    _ => false,
                };
                if e.body.is_none() || reserialized {
                    e.body = Some(frame.raw());
                }
                e.with_http(self.http.clone())
            })?;
            self.check(step.events, out)?;
            if step.terminal {
                self.stopped = true;
            }
        }
        Ok(())
    }

    /// The transport starts another request for the same response: the parser keeps its state
    /// (`NextRequest`), the next response body gets a fresh framer.
    pub fn next_request(&mut self) {
        self.parser.next_request();
    }

    /// Feed one already-framed frame (WebSocket text frames).
    pub fn push_frame(&mut self, frame: Frame) -> Result<Vec<LLMEvent>, AIError> {
        let mut out = vec![];
        if self.stopped {
            return Ok(out);
        }
        self.handle_frames(vec![frame], &mut out)?;
        Ok(out)
    }

    /// Feed raw response bytes. On a failure the events decoded before it are lost; streaming callers use
    /// [`StreamDriver::push_events`].
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<LLMEvent>, AIError> {
        match self.push_events(bytes) {
            (events, None) => Ok(events),
            (_, Some(e)) => Err(e),
        }
    }

    /// Feed raw response bytes: the events of every frame decoded before a failure, then the failure. One read
    /// can carry several frames, and upstream's stream emits each frame's events before a later frame fails
    /// (e.g. text followed by an unknown `finish_reason` or an `error` event), so they must not be dropped.
    pub fn push_events(&mut self, bytes: &[u8]) -> (Vec<LLMEvent>, Option<AIError>) {
        let mut out = vec![];
        if self.stopped {
            return (out, None);
        }
        let frames = match self.framer.push(bytes) {
            Ok(frames) => frames,
            Err(e) => return (out, Some(e.with_http(self.http.clone()))),
        };
        let failure = self.handle_frames(frames, &mut out).err();
        (out, failure)
    }

    /// End of byte stream: flush framing, `onHalt`, then enforce the terminal rule.
    pub fn finish(&mut self) -> Result<Vec<LLMEvent>, AIError> {
        match self.finish_events() {
            (events, None) => Ok(events),
            (_, Some(e)) => Err(e),
        }
    }

    /// [`StreamDriver::finish`] keeping the events decoded before a failure (see [`StreamDriver::push_events`]).
    pub fn finish_events(&mut self) -> (Vec<LLMEvent>, Option<AIError>) {
        let mut out = vec![];
        if self.halted {
            return (out, None);
        }
        self.halted = true;
        if !self.stopped {
            let frames = match self.framer.finish() {
                Ok(frames) => frames,
                Err(e) => return (out, Some(e.with_http(self.http.clone()))),
            };
            if let Err(e) = self.handle_frames(frames, &mut out) {
                return (out, Some(e));
            }
        }
        let halt = match self.parser.on_halt() {
            Ok(halt) => halt,
            Err(e) => return (out, Some(e.with_http(self.http.clone()))),
        };
        if let Err(e) = self.check(halt, &mut out) {
            return (out, Some(e));
        }
        if !self.terminal_emitted {
            let e = AIError::incomplete_stream(&self.label, "The provider response ended unexpectedly.").with_http(self.http.clone());
            return (out, Some(e));
        }
        (out, None)
    }

    /// Decode a complete body (tests, recorded fixtures): every event up to the first error, then the error.
    pub fn decode_all(request: &LLMRequest, body: &[u8]) -> Vec<Result<LLMEvent, AIError>> {
        let mut driver = StreamDriver::new(request);
        let (events, failure) = driver.push_events(body);
        let mut out: Vec<Result<LLMEvent, AIError>> = events.into_iter().map(Ok).collect();
        if let Some(e) = failure {
            out.push(Err(e));
            return out;
        }
        let (events, failure) = driver.finish_events();
        out.extend(events.into_iter().map(Ok));
        out.extend(failure.map(Err));
        out
    }
}

/// The LLM client.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    middleware: Vec<Arc<dyn HttpMiddleware>>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        let http = oc_util::http::client();
        Self { http, middleware: vec![] }
    }

    pub fn with_http_client(http: reqwest::Client) -> Self {
        Self { http, middleware: vec![] }
    }

    /// Add an HTTP middleware applied to every prepared request (in insertion order).
    pub fn with_middleware(mut self, middleware: Arc<dyn HttpMiddleware>) -> Self {
        self.middleware.push(middleware);
        self
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Compile a request: options/defaults, tool history, schema projection, cache policy, body lowering,
    /// headers/auth (`compile`).
    pub async fn compile(&self, request: &LLMRequest) -> Result<Compiled, AIError> {
        crate::compile::reject_local_file_references(request)?;
        let resolved = prepare_request(request);
        let body = resolved.model.route.protocol.body(&resolved)?;
        let mut prepared = transport::prepare_json(&resolved, body.clone()).await?;
        for m in &self.middleware {
            m.on_request(&resolved, &mut prepared).await?;
        }
        Ok(Compiled { request: resolved, body, prepared })
    }

    /// Start a streaming call. Resolves once response headers arrive; HTTP failures are returned here.
    /// Cancellation ends the stream with a Transport error (code `"Aborted"`).
    pub async fn stream(&self, request: LLMRequest, cancel: CancellationToken) -> Result<LLMStream, AIError> {
        self.stream_opts(request, cancel, StreamOptions::default()).await
    }

    /// [`Client::stream`] with a per-call [`HttpHook`] (upstream `StreamOptions.http`).
    pub async fn stream_with(&self, request: LLMRequest, cancel: CancellationToken, hook: Option<Arc<dyn HttpHook>>) -> Result<LLMStream, AIError> {
        self.stream_opts(request, cancel, StreamOptions { http: hook, web_socket: None }).await
    }

    /// [`Client::stream`] with [`StreamOptions`]: the HTTP hook and, for routes with a WebSocket
    /// channel, the channel executor that carries the request (upstream `StreamOptions.webSocket`).
    /// On the channel path every failure (including the HTTP fallback's) arrives in the stream.
    pub async fn stream_opts(&self, request: LLMRequest, cancel: CancellationToken, options: StreamOptions) -> Result<LLMStream, AIError> {
        let hook = options.http;
        if let Some(prepared) = request.model.route.stream.clone() {
            // `compile` then `route.streamPrepared`: no HTTP transport, no terminal-event rule.
            crate::compile::reject_local_file_references(&request)?;
            let resolved = prepare_request(&request);
            let body = resolved.model.route.protocol.body(&resolved)?;
            return Ok(prepared.stream(resolved, body, hook, cancel));
        }
        if let (Some(executor), Some(channel)) = (options.web_socket, request.model.route.channel.clone()) {
            // `OpenResponsesChannel.transport`: prepare both the channel and the HTTP request.
            crate::compile::reject_local_file_references(&request)?;
            let resolved = prepare_request(&request);
            let body = resolved.model.route.protocol.body(&resolved)?;
            let parts = transport::json_request_parts(&resolved, body.clone()).await?;
            let mut prepared = parts.clone();
            prepared.headers.insert("content-type".into(), "application/json".into());
            prepared.encoding = resolved.http.as_ref().and_then(|h| h.encoding);
            for m in &self.middleware {
                m.on_request(&resolved, &mut prepared).await?;
            }
            let compiled = Compiled { request: resolved, body, prepared };
            if channel.offers(&parts.url) {
                let ws = channel.prepare(&parts.url, &parts.json_body, &parts.headers)?;
                return Ok(self.channel_stream(compiled, &channel, ws, executor, cancel, hook));
            }
            tracing::warn!(url = %parts.url, "{} does not offer WebSocket for this endpoint; using HTTP", channel.name);
            return self.http_stream(compiled, cancel, hook).await;
        }
        let compiled = self.compile(&request).await?;
        self.http_stream(compiled, cancel, hook).await
    }

    async fn http_stream(&self, compiled: Compiled, cancel: CancellationToken, hook: Option<Arc<dyn HttpHook>>) -> Result<LLMStream, AIError> {
        if compiled.request.model.route.continuation {
            return self.continued_stream(compiled, cancel, hook).await;
        }
        let timeouts = Timeouts::from_request(&compiled.request);
        let url = compiled.prepared.url.clone();
        let started = tokio::time::Instant::now();
        let execution = tokio::select! {
            _ = cancel.cancelled() => {
                return Err(AIError::transport(TransportOperation::Request, "Request aborted", Some("Aborted"), Some(&url)));
            }
            result = transport::execute_with(&self.http, &compiled.prepared, timeouts, hook.as_deref()) => result?,
        };
        let mut driver = StreamDriver::new(&compiled.request);
        driver.http = Some(execution.http.clone());
        let mut body = body_stream(execution, timeouts, started, cancel);
        let stream = async_stream::stream! {
            loop {
                match body.next().await {
                    None => break,
                    Some(Err(e)) => { yield Err(e); return; }
                    Some(Ok(bytes)) => {
                        let (events, failure) = driver.push_events(&bytes);
                        for e in events { yield Ok(e); }
                        if let Some(e) = failure { yield Err(e); return; }
                        if driver.is_stopped() { break; }
                    }
                }
            }
            let (events, failure) = driver.finish_events();
            for e in events { yield Ok(e); }
            if let Some(e) = failure { yield Err(e); }
        };
        Ok(Box::pin(stream))
    }

    /// `Gemini.transport`: SSE that continues responses Gemini splits across requests. Chunks carry
    /// `continuationToken` checkpoints, and a request that reaches its output cap finishes with
    /// `CONTINUATION`. The transport resends the original body with the latest token, which also
    /// resumes a request that ended or dropped (not timed out) before finishing.
    #[allow(clippy::result_large_err)]
    async fn continued_stream(&self, compiled: Compiled, cancel: CancellationToken, hook: Option<Arc<dyn HttpHook>>) -> Result<LLMStream, AIError> {
        const CONTINUATION: &str = "CONTINUATION";
        let timeouts = Timeouts::from_request(&compiled.request);
        let url = compiled.prepared.url.clone();
        let started = tokio::time::Instant::now();
        let first = tokio::select! {
            _ = cancel.cancelled() => {
                return Err(AIError::transport(TransportOperation::Request, "Request aborted", Some("Aborted"), Some(&url)));
            }
            result = transport::execute_with(&self.http, &compiled.prepared, timeouts, hook.as_deref()) => result?,
        };
        let mut driver = StreamDriver::new(&compiled.request);
        driver.http = Some(first.http.clone());
        let label = route_label(&compiled.request);
        let client = self.clone();
        let stream = async_stream::stream! {
            let mut execution = first;
            let mut started = started;
            loop {
                let mut framer = Framer::new(&compiled.request.model.route.framing);
                let mut body = body_stream(execution, timeouts, started, cancel.clone());
                let mut token: Option<String> = None;
                let mut finish_reason: Option<String> = None;
                let mut dropped: Option<AIError> = None;
                loop {
                    let (frames, end) = match body.next().await {
                        None => match framer.finish() {
                            Ok(frames) => (frames, true),
                            Err(e) => { yield Err(e.with_http(driver.http.clone())); return; }
                        },
                        // A connection failure may resume; a timeout keeps its meaning and ends the response.
                        Some(Err(e)) if matches!(&e.kind, crate::error::ErrorKind::Transport { code, .. } if !matches!(code.as_deref(), Some("Timeout" | "Aborted"))) => {
                            dropped = Some(e);
                            break;
                        }
                        Some(Err(e)) => { yield Err(e.with_http(driver.http.clone())); return; }
                        Some(Ok(bytes)) => match framer.push(&bytes) {
                            Ok(frames) => (frames, false),
                            Err(e) => { yield Err(e.with_http(driver.http.clone())); return; }
                        },
                    };
                    for frame in frames {
                        observe_checkpoint(&frame, &mut token, &mut finish_reason);
                        match driver.push_frame(frame) {
                            Ok(events) => for e in events { yield Ok(e); },
                            Err(e) => { yield Err(e); return; }
                        }
                    }
                    if end { break; }
                }
                if token.is_some() && (finish_reason.is_none() || finish_reason.as_deref() == Some(CONTINUATION)) {
                    driver.next_request();
                    let mut body = compiled.body.clone();
                    if let Some(o) = body.as_object_mut() {
                        o.insert("continuationToken".into(), serde_json::Value::String(token.unwrap_or_default()));
                    }
                    let prepared = match client.prepare(&compiled.request, body).await {
                        Ok(prepared) => prepared,
                        Err(e) => { yield Err(e.with_http(driver.http.clone())); return; }
                    };
                    started = tokio::time::Instant::now();
                    let next = tokio::select! {
                        _ = cancel.cancelled() => Err(AIError::transport(TransportOperation::Request, "Request aborted", Some("Aborted"), Some(&prepared.url))),
                        result = transport::execute_with(&client.http, &prepared, timeouts, hook.as_deref()) => result,
                    };
                    match next {
                        Ok(next) => { execution = next; continue; }
                        Err(e) => { yield Err(e.with_http(driver.http.clone())); return; }
                    }
                }
                if let Some(e) = dropped { yield Err(e.with_http(driver.http.clone())); return; }
                if finish_reason.as_deref() == Some(CONTINUATION) {
                    yield Err(AIError::invalid_output(&label, "Gemini continued the response without a token").with_http(driver.http.clone()));
                    return;
                }
                break;
            }
            let (events, failure) = driver.finish_events();
            for e in events { yield Ok(e); }
            if let Some(e) = failure { yield Err(e); }
        };
        Ok(Box::pin(stream))
    }

    /// `jsonRequestParts` + `jsonPost` and the client middleware for a body the client builds itself.
    #[allow(clippy::result_large_err)]
    async fn prepare(&self, request: &LLMRequest, body: serde_json::Value) -> Result<PreparedRequest, AIError> {
        let mut prepared = transport::prepare_json(request, body).await?;
        for m in &self.middleware {
            m.on_request(request, &mut prepared).await?;
        }
        Ok(prepared)
    }

    /// The channel half of `OpenResponsesChannel.transport.execute`: the executor carries the exchange,
    /// its frames are parsed like SSE data, and a successful end acknowledges the exchange.
    fn channel_stream(
        &self,
        compiled: Compiled,
        channel: &ChannelOptions,
        prepared: PreparedChannel,
        executor: Arc<dyn ChannelExecutor>,
        cancel: CancellationToken,
        hook: Option<Arc<dyn HttpHook>>,
    ) -> LLMStream {
        let fallback_http: Arc<parking_lot::Mutex<Option<HttpContext>>> = Arc::default();
        let fallback = {
            let (client, compiled, cancel, slot) = (self.http.clone(), compiled.clone(), cancel.clone(), fallback_http.clone());
            Box::new(move || http_frames(client, compiled, hook, cancel, slot)) as Box<dyn FnOnce() -> FrameStream + Send>
        };
        let url = prepared.url.clone();
        let exchange = ChannelExchange {
            id: compiled.request.id.clone().unwrap_or_else(|| "request".into()),
            connect: ChannelConnect { url: prepared.url.clone(), headers: prepared.headers.clone(), rotate_after_ms: prepared.rotate_after_ms },
            fallback,
            driver: Box::new(channel.driver(&prepared)),
        };
        let ChannelExecution { mut frames, complete } = executor.execute(exchange);
        let mut driver = StreamDriver::new(&compiled.request);
        let stream = async_stream::stream! {
            loop {
                let item = tokio::select! {
                    _ = cancel.cancelled() => {
                        yield Err(AIError::transport(TransportOperation::Read, "Request aborted", Some("Aborted"), Some(&url)));
                        return;
                    }
                    item = frames.next() => item,
                };
                driver.http = fallback_http.lock().clone();
                match item {
                    None => break,
                    Some(Err(e)) => { yield Err(e.with_http(driver.http.clone())); return; }
                    Some(Ok(frame)) => {
                        match driver.push_frame(Frame::Text(frame)) {
                            Ok(events) => for e in events { yield Ok(e); },
                            Err(e) => { yield Err(e); return; }
                        }
                        if driver.is_stopped() { break; }
                    }
                }
            }
            // The exchange ends before its acknowledgement (`Stream.onEnd(execution.complete)`).
            drop(frames);
            let (events, failure) = driver.finish_events();
            for e in events { yield Ok(e); }
            match failure {
                None => complete(),
                Some(e) => { yield Err(e); }
            }
        };
        Box::pin(stream)
    }

    /// Stream and fold into an [`LLMResponse`].
    pub async fn generate(&self, request: LLMRequest, cancel: CancellationToken) -> Result<LLMResponse, AIError> {
        self.generate_opts(request, cancel, StreamOptions::default()).await
    }

    /// [`Client::generate`] with a per-call [`HttpHook`].
    pub async fn generate_with(&self, request: LLMRequest, cancel: CancellationToken, hook: Option<Arc<dyn HttpHook>>) -> Result<LLMResponse, AIError> {
        self.generate_opts(request, cancel, StreamOptions { http: hook, web_socket: None }).await
    }

    /// [`Client::generate`] with [`StreamOptions`].
    pub async fn generate_opts(&self, request: LLMRequest, cancel: CancellationToken, options: StreamOptions) -> Result<LLMResponse, AIError> {
        let label = route_label(&request);
        let mut stream = self.stream_opts(request, cancel, options).await?;
        let mut state = ResponseState::new();
        while let Some(item) = stream.next().await {
            state.reduce(item?);
        }
        state.complete().ok_or_else(|| AIError::incomplete_stream(&label, "The provider response ended unexpectedly."))
    }
}

/// Per-call options (`StreamOptions`).
#[derive(Clone, Default)]
pub struct StreamOptions {
    /// `session.http.*` middleware around every HTTP request of the call (incl. a WebSocket fallback).
    pub http: Option<Arc<dyn HttpHook>>,
    /// The executor that carries the call over a route's WebSocket channel.
    pub web_socket: Option<Arc<dyn ChannelExecutor>>,
}

/// `GeminiCheckpoint`: the first candidate's `continuationToken` and `finishReason` of one frame. A
/// finish without a token clears the token; a frame that does not decode leaves both unchanged.
fn observe_checkpoint(frame: &Frame, token: &mut Option<String>, finish_reason: &mut Option<String>) {
    use serde_json::Value;
    let parsed;
    let event = match frame {
        Frame::Json(value) => value,
        Frame::Text(text) => match serde_json::from_str::<Value>(text) {
            Ok(value) => {
                parsed = value;
                &parsed
            }
            Err(_) => return,
        },
    };
    let nullable_string = |v: Option<&Value>| match v {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(s)) => Some(Some(s.clone())),
        Some(_) => None,
    };
    let candidates = match event.get("candidates") {
        Some(Value::Array(candidates)) => candidates,
        _ => return,
    };
    // Every candidate must decode for the checkpoint to decode.
    let mut decoded = vec![];
    for candidate in candidates {
        let Some(o) = candidate.as_object() else { return };
        let (Some(reason), Some(continuation)) = (nullable_string(o.get("finishReason")), nullable_string(o.get("continuationToken"))) else {
            return;
        };
        decoded.push((reason, continuation));
    }
    let Some((reason, continuation)) = decoded.into_iter().next() else { return };
    *token = match continuation {
        Some(t) => Some(t),
        None if reason.as_deref().is_some_and(|r| !r.is_empty()) => None,
        None => token.take(),
    };
    if let Some(reason) = reason {
        *finish_reason = Some(reason);
    }
}

/// The response body with the chunk timeout, the whole-request deadline and cancellation applied.
fn body_stream(
    execution: transport::HttpExecution,
    timeouts: Timeouts,
    started: tokio::time::Instant,
    cancel: CancellationToken,
) -> futures::stream::BoxStream<'static, Result<bytes::Bytes, AIError>> {
    let http = execution.http;
    let mut body = execution.body;
    let deadline = timeouts.total.map(|t| started + t);
    Box::pin(async_stream::stream! {
        loop {
            let chunk_timeout = timeouts.chunk;
            let next = async {
                match chunk_timeout {
                    Some(limit) => match tokio::time::timeout(limit, body.next()).await {
                        Ok(v) => Ok(v),
                        Err(_) => Err(AIError::transport(TransportOperation::Read, "Timed out waiting for response data", Some("Timeout"), Some(&http.url)).with_http(Some(http.clone()))),
                    },
                    None => Ok(body.next().await),
                }
            };
            let total = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => futures::future::pending::<()>().await,
                }
            };
            let item = tokio::select! {
                _ = cancel.cancelled() => {
                    yield Err(AIError::transport(TransportOperation::Read, "Request aborted", Some("Aborted"), Some(&http.url)).with_http(Some(http.clone())));
                    return;
                }
                _ = total => {
                    yield Err(AIError::transport(TransportOperation::Read, "Timed out waiting for the response to complete", Some("Timeout"), Some(&http.url)).with_http(Some(http.clone())));
                    return;
                }
                item = next => item,
            };
            match item {
                Err(e) => { yield Err(e); return; }
                Ok(None) => return,
                Ok(Some(Err(e))) => { yield Err(e); return; }
                Ok(Some(Ok(bytes))) => yield Ok(bytes),
            }
        }
    })
}

/// The HTTP fallback of a channel exchange: the prepared request over HTTP, framed into the text frames
/// the route parses (`http.execute(prepared.http, ...)` → `execution.frames`).
fn http_frames(
    client: reqwest::Client,
    compiled: Compiled,
    hook: Option<Arc<dyn HttpHook>>,
    cancel: CancellationToken,
    slot: Arc<parking_lot::Mutex<Option<HttpContext>>>,
) -> FrameStream {
    Box::pin(async_stream::stream! {
        let timeouts = Timeouts::from_request(&compiled.request);
        let started = tokio::time::Instant::now();
        let url = compiled.prepared.url.clone();
        let execution = tokio::select! {
            _ = cancel.cancelled() => {
                yield Err(AIError::transport(TransportOperation::Request, "Request aborted", Some("Aborted"), Some(&url)));
                return;
            }
            result = transport::execute_with(&client, &compiled.prepared, timeouts, hook.as_deref()) => result,
        };
        let execution = match execution {
            Ok(e) => e,
            Err(e) => { yield Err(e); return; }
        };
        *slot.lock() = Some(execution.http.clone());
        let http = execution.http.clone();
        let mut framer = Framer::new(&compiled.request.model.route.framing);
        let mut body = body_stream(execution, timeouts, started, cancel);
        while let Some(item) = body.next().await {
            match item.and_then(|bytes| framer.push(&bytes).map_err(|e| e.with_http(Some(http.clone())))) {
                Ok(frames) => for f in frames { yield Ok(f.raw()); },
                Err(e) => { yield Err(e); return; }
            }
        }
        match framer.finish() {
            Ok(frames) => for f in frames { yield Ok(f.raw()); },
            Err(e) => yield Err(e.with_http(Some(http))),
        }
    })
}
