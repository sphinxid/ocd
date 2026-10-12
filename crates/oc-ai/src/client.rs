//! `LLMClient`: compile → prepare → HTTP → framing → protocol parser → terminal rule
//! (`route/client.ts`).

use crate::compile::prepare_request;
use crate::error::{AIError, HttpContext, TransportOperation};
use crate::protocols::{route_label, StreamParser};
use crate::response::{LLMResponse, ResponseState};
use crate::route::framing::{Frame, Framer};
use crate::route::transport::{self, HttpMiddleware, PreparedRequest, Timeouts};
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
                if e.body.is_none() {
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

    /// Feed raw response bytes.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<LLMEvent>, AIError> {
        let mut out = vec![];
        if self.stopped {
            return Ok(out);
        }
        let frames = self.framer.push(bytes).map_err(|e| e.with_http(self.http.clone()))?;
        self.handle_frames(frames, &mut out)?;
        Ok(out)
    }

    /// End of byte stream: flush framing, `onHalt`, then enforce the terminal rule.
    pub fn finish(&mut self) -> Result<Vec<LLMEvent>, AIError> {
        let mut out = vec![];
        if self.halted {
            return Ok(out);
        }
        self.halted = true;
        if !self.stopped {
            let frames = self.framer.finish().map_err(|e| e.with_http(self.http.clone()))?;
            self.handle_frames(frames, &mut out)?;
        }
        let halt = self.parser.on_halt().map_err(|e| e.with_http(self.http.clone()))?;
        self.check(halt, &mut out)?;
        if !self.terminal_emitted {
            return Err(AIError::incomplete_stream(&self.label, "The provider response ended unexpectedly.").with_http(self.http.clone()));
        }
        Ok(out)
    }

    /// Decode a complete body (tests, recorded fixtures). Stops at the first error.
    pub fn decode_all(request: &LLMRequest, body: &[u8]) -> Vec<Result<LLMEvent, AIError>> {
        let mut driver = StreamDriver::new(request);
        let mut out: Vec<Result<LLMEvent, AIError>> = vec![];
        match driver.push(body) {
            Ok(events) => out.extend(events.into_iter().map(Ok)),
            Err(e) => {
                out.push(Err(e));
                return out;
            }
        }
        match driver.finish() {
            Ok(events) => out.extend(events.into_iter().map(Ok)),
            Err(e) => out.push(Err(e)),
        }
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
        let http = reqwest::Client::builder().build().unwrap_or_else(|_| reqwest::Client::new());
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
        let compiled = self.compile(&request).await?;
        let timeouts = Timeouts::from_request(&compiled.request);
        let url = compiled.prepared.url.clone();
        let started = tokio::time::Instant::now();
        let execution = tokio::select! {
            _ = cancel.cancelled() => {
                return Err(AIError::transport(TransportOperation::Request, "Request aborted", Some("Aborted"), Some(&url)));
            }
            result = transport::execute(&self.http, &compiled.prepared, timeouts) => result?,
        };
        let mut driver = StreamDriver::new(&compiled.request);
        driver.http = Some(execution.http.clone());
        let http = execution.http.clone();
        let mut body = execution.body;
        let deadline = timeouts.total.map(|t| started + t);
        let stream = async_stream::stream! {
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
                    Ok(None) => break,
                    Ok(Some(Err(e))) => { yield Err(e); return; }
                    Ok(Some(Ok(bytes))) => {
                        match driver.push(&bytes) {
                            Ok(events) => for e in events { yield Ok(e); },
                            Err(e) => { yield Err(e); return; }
                        }
                        if driver.is_stopped() { break; }
                    }
                }
            }
            match driver.finish() {
                Ok(events) => for e in events { yield Ok(e); },
                Err(e) => { yield Err(e); }
            }
        };
        Ok(Box::pin(stream))
    }

    /// Stream and fold into an [`LLMResponse`].
    pub async fn generate(&self, request: LLMRequest, cancel: CancellationToken) -> Result<LLMResponse, AIError> {
        let label = route_label(&request);
        let mut stream = self.stream(request, cancel).await?;
        let mut state = ResponseState::new();
        while let Some(item) = stream.next().await {
            state.reduce(item?);
        }
        state.complete().ok_or_else(|| AIError::incomplete_stream(&label, "The provider response ended unexpectedly."))
    }
}
