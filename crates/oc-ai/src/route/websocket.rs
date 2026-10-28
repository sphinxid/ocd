//! WebSocket channel transport (`route/transport/websocket-channel.ts`, `route/transport/websocket.ts`).
//!
//! A route with a WebSocket channel (the Open Responses channel transport) hands a
//! [`ChannelExchange`] to the caller's [`ChannelExecutor`] (core's `SessionModelTransport`), which owns
//! the socket: it sends the driver's `response.create` message, feeds every inbound text frame through
//! the driver and returns the frames the route parses like SSE data. The exchange carries an HTTP
//! fallback for executors that decide not to use the socket.
//!
//! [`open`] is the socket upstream's runtime opens (`core/src/effect/websocket-constructor.ts` on Bun
//! 1.4.4): through the proxy the environment selects ([`proxy`]), with Bun's upgrade request and
//! `permessage-deflate` ([`client`]).

use super::Headers;
use crate::error::{AIError, ErrorKind, TransportOperation, TransportType};
use futures::stream::BoxStream;
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

pub mod client;
pub mod proxy;

/// Largest inbound message accepted (`MAX_FRAME_BYTES`).
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// `ChannelCheckpoint`: protocol-owned continuation state staged by a completed exchange.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelCheckpoint {
    pub protocol: String,
    pub value: Value,
}

/// `ChannelCreate.mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelMode {
    Full,
    Incremental,
}

impl ChannelMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Incremental => "incremental",
        }
    }
}

/// `ChannelCreate`: the outbound message of one exchange.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelCreate {
    pub message: String,
    pub mode: ChannelMode,
}

/// `ChannelObservation`: what one inbound frame means for the exchange.
#[derive(Debug, Clone)]
pub enum ChannelObservation {
    Frame(String),
    Completed { frame: String, checkpoint: Option<ChannelCheckpoint> },
    Incomplete(String),
    ProviderFailure(AIError),
    /// `recovery`: `"retry-full"` or `"rotate-and-retry-full"`.
    Rejected { error: AIError, recovery: String },
}

impl ChannelObservation {
    /// Every observation except a plain frame ends the exchange.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Frame(_))
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Frame(_) => "frame",
            Self::Completed { .. } => "completed",
            Self::Incomplete(_) => "incomplete",
            Self::ProviderFailure(_) => "provider-failure",
            Self::Rejected { .. } => "rejected",
        }
    }

    /// `observationFrame`: the frame to parse, or the failure.
    pub fn into_frame(self) -> Result<String, AIError> {
        match self {
            Self::Frame(frame) | Self::Completed { frame, .. } | Self::Incomplete(frame) => Ok(frame),
            Self::ProviderFailure(error) | Self::Rejected { error, .. } => Err(error),
        }
    }
}

/// `WebSocketChannelDriver`: builds the outbound message and classifies inbound frames.
pub trait ChannelDriver: Send {
    fn create(&mut self, checkpoint: Option<&ChannelCheckpoint>) -> Result<ChannelCreate, AIError>;
    fn observe(&mut self, create: &ChannelCreate, frame: &str) -> Result<ChannelObservation, AIError>;
}

/// `WebSocketChannelExchange.connect`.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelConnect {
    pub url: String,
    /// Lower-case names.
    pub headers: Headers,
    /// Provider-safe connection age after which the executor reconnects before sending.
    pub rotate_after_ms: Option<u64>,
}

/// Frames of an exchange (text frames, or SSE data of the HTTP fallback).
pub type FrameStream = BoxStream<'static, Result<String, AIError>>;

/// `WebSocketChannelExchange`.
pub struct ChannelExchange {
    pub id: String,
    pub connect: ChannelConnect,
    /// The same request over HTTP (lazy: nothing is sent until the stream is polled).
    pub fallback: Box<dyn FnOnce() -> FrameStream + Send>,
    pub driver: Box<dyn ChannelDriver>,
}

/// `WebSocketChannelExecution`: the frames and the success acknowledgement that commits staged state
/// (called after the decoded route stream ended successfully).
pub struct ChannelExecution {
    pub frames: FrameStream,
    pub complete: Box<dyn FnOnce() + Send>,
}

/// `WebSocketChannelExecutor`.
pub trait ChannelExecutor: Send + Sync {
    fn execute(&self, exchange: ChannelExchange) -> ChannelExecution;
}

/// A WebSocket transport error (`transportError` in websocket.ts / model-transport.ts).
#[derive(Debug, Clone, Default)]
pub struct WsError<'a> {
    pub url: Option<&'a str>,
    pub code: Option<&'a str>,
    pub phase: Option<&'a str>,
    pub delivery: Option<&'a str>,
    pub body: Option<String>,
}

pub fn transport_error(message: impl Into<String>, operation: TransportOperation, input: WsError<'_>) -> AIError {
    let mut error = AIError::new(
        ErrorKind::Transport {
            transport: TransportType::Websocket,
            operation,
            code: input.code.map(Into::into),
            url: input.url.map(Into::into),
            phase: input.phase.map(Into::into),
            delivery: input.delivery.map(Into::into),
            recovery: None,
        },
        message,
    );
    error.body = input.body;
    error
}

/// `annotate`: phase and delivery of a transport error (other errors unchanged).
pub fn annotate(mut error: AIError, phase: &str, delivery: &str) -> AIError {
    if let ErrorKind::Transport { phase: p, delivery: d, .. } = &mut error.kind {
        *p = Some(phase.into());
        *d = Some(delivery.into());
    }
    error
}

/// `toWebSocketUrl`: `https:` → `wss:`, `http:` → `ws:`.
pub fn to_websocket_url(value: &str) -> Result<String, AIError> {
    let fail = |message: String| {
        transport_error(
            message,
            TransportOperation::Request,
            WsError { url: Some(value), code: Some("invalid-url"), phase: Some("prepare"), delivery: Some("not-sent"), ..Default::default() },
        )
    };
    let mut url = url::Url::parse(value).map_err(|_| fail("Invalid URL".into()))?;
    let scheme = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => return Err(fail(format!("Unsupported WebSocket URL protocol {other}:"))),
    };
    // `url` refuses special→special scheme changes only between special and non-special schemes.
    url.set_scheme(scheme).map_err(|_| fail("Invalid URL".into()))?;
    Ok(url.to_string())
}

/// An inbound WebSocket message.
#[derive(Debug, Clone, PartialEq)]
pub enum WsMessage {
    Text(String),
    Binary(Vec<u8>),
}

type Sink = client::Writer;

/// An open WebSocket (`WebSocketConnection`): text sends, the inbound message queue and close.
pub struct WebSocketConnection {
    url: String,
    sink: Arc<tokio::sync::Mutex<Option<Sink>>>,
    messages: parking_lot::Mutex<Option<mpsc::UnboundedReceiver<Result<WsMessage, AIError>>>>,
    reader: tokio::task::AbortHandle,
    closed: Arc<std::sync::atomic::AtomicBool>,
    deflate: Option<client::DeflateParams>,
}

impl WebSocketConnection {
    /// `sendText`.
    pub async fn send_text(&self, message: String) -> Result<(), AIError> {
        let not_open = |state: u8| {
            transport_error(
                format!("WebSocket is not open (state {state})"),
                TransportOperation::Write,
                WsError { url: Some(&self.url), phase: Some("send"), delivery: Some("not-sent"), ..Default::default() },
            )
        };
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(not_open(3));
        }
        let mut sink = self.sink.lock().await;
        let Some(sink) = sink.as_mut() else { return Err(not_open(3)) };
        sink.text(&message).await.map_err(|e| match e.kind() {
            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::NotConnected => not_open(3),
            _ => transport_error(
                e.to_string(),
                TransportOperation::Write,
                WsError { url: Some(&self.url), phase: Some("send"), delivery: Some("not-sent"), ..Default::default() },
            ),
        })
    }

    /// The inbound messages (taken once). Ends with an error when the socket closes or fails.
    pub fn take_messages(&self) -> Option<mpsc::UnboundedReceiver<Result<WsMessage, AIError>>> {
        self.messages.lock().take()
    }

    /// `close`: stop reading and send a normal close (1000) unless the socket is already closed. The
    /// close frame goes out in the background.
    pub fn close(&self) {
        if let Some(close) = self.begin_close()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            handle.spawn(close);
        }
    }

    /// [`close`](Self::close), waiting until the close frame was sent (shutdown).
    pub async fn close_and_wait(&self) {
        if let Some(close) = self.begin_close() {
            close.await;
        }
    }

    fn begin_close(&self) -> Option<impl std::future::Future<Output = ()> + Send + 'static> {
        if self.closed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        self.reader.abort();
        let sink = self.sink.clone();
        Some(async move {
            let mut guard = sink.lock().await;
            if let Some(mut sink) = guard.take() {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    let _ = sink.control(client::OP_CLOSE, &1000u16.to_be_bytes()).await;
                    let _ = sink.io.shutdown().await;
                })
                .await;
            }
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// The negotiated `permessage-deflate` parameters (`None`: the server accepted no extension).
    pub fn deflate(&self) -> Option<client::DeflateParams> {
        self.deflate
    }
}

impl Drop for WebSocketConnection {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// `WebSocketConnector`.
#[async_trait::async_trait]
pub trait WebSocketConnector: Send + Sync {
    async fn open(&self, url: &str, headers: &Headers) -> Result<WebSocketConnection, AIError>;
}

/// The default connector (`WebSocketTransport.open` with the platform constructor).
#[derive(Debug, Clone, Copy, Default)]
pub struct DirectConnector;

#[async_trait::async_trait]
impl WebSocketConnector for DirectConnector {
    async fn open(&self, url: &str, headers: &Headers) -> Result<WebSocketConnection, AIError> {
        open(url, headers).await
    }
}

fn connect_error(url: &str, message: String) -> AIError {
    transport_error(
        message,
        TransportOperation::Request,
        WsError { url: Some(url), phase: Some("connect"), delivery: Some("not-sent"), ..Default::default() },
    )
}

/// RFC 7230 `token` characters (a header name `Headers` accepts).
fn header_name_ok(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// `WebSocketTransport.open`: connect with the handshake headers, through the proxy the process environment
/// selects ([`proxy`]).
pub async fn open(url: &str, headers: &Headers) -> Result<WebSocketConnection, AIError> {
    open_with_env(url, headers, &proxy::process_env).await
}

/// [`open`] with an explicit environment lookup for the proxy variables.
pub async fn open_with_env(url: &str, headers: &Headers, env: proxy::Env<'_>) -> Result<WebSocketConnection, AIError> {
    // The constructor's synchronous checks (`new WebSocket` throws; upstream reports the exception's message).
    let parsed = url::Url::parse(url).map_err(|_| connect_error(url, format!("Invalid url for WebSocket {url}")))?;
    if parsed.scheme() != "ws" && parsed.scheme() != "wss" {
        return Err(connect_error(url, format!("Wrong url scheme for WebSocket {url}")));
    }
    if parsed.fragment().is_some() {
        return Err(connect_error(url, format!("URL has fragment component {url}")));
    }
    for (name, value) in headers {
        if !header_name_ok(name) {
            return Err(connect_error(url, format!("Invalid header name: '{name}'")));
        }
        if value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
            return Err(connect_error(url, format!("Header '{name}' has invalid value: '{value}'")));
        }
    }
    let upgraded = client::connect(&parsed, headers, env).await.map_err(|failure| match failure {
        client::ConnectFailure::Thrown(message) => connect_error(url, message),
        client::ConnectFailure::Failed(text) => connect_error(url, format!("Failed to open WebSocket: WebSocket connection to '{parsed}' failed: {text}")),
    })?;
    let deflate = upgraded.deflate;
    let (mut source, sink) = tokio::io::split(upgraded.io);
    let sink = Arc::new(tokio::sync::Mutex::new(Some(client::Writer { io: sink, deflate: deflate.map(client::Compressor::new) })));
    let (tx, rx) = mpsc::unbounded_channel();
    let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let read_url = url.to_string();
    let message_url = parsed.to_string();
    let read_closed = closed.clone();
    let read_sink = sink.clone();
    let mut frames = client::Reader::new(upgraded.leftover, deflate);
    let reader = tokio::spawn(async move {
        let close_error = |code: u16, reason: String| {
            transport_error(
                format!("WebSocket closed with code {code}"),
                TransportOperation::Read,
                WsError { url: Some(&read_url), code: Some(&code.to_string()), phase: Some("close"), body: Some(reason), ..Default::default() },
            )
        };
        // Ends the socket from this side: a close frame (when there is one to send), then the write half.
        let finish = |close: Option<(u16, String)>| {
            read_closed.store(true, std::sync::atomic::Ordering::SeqCst);
            let sink = read_sink.clone();
            async move {
                if let Some(mut sink) = sink.lock().await.take() {
                    if let Some((code, reason)) = close {
                        let mut payload = code.to_be_bytes().to_vec();
                        payload.extend_from_slice(reason.as_bytes());
                        let _ = sink.control(client::OP_CLOSE, &payload).await;
                    }
                    let _ = sink.io.shutdown().await;
                }
            }
        };
        let too_large = |body: String| {
            transport_error(
                "WebSocket message exceeds the 16 MiB limit",
                TransportOperation::Read,
                WsError { url: Some(&read_url), code: Some("message-too-large"), phase: Some("receive"), body: Some(body), ..Default::default() },
            )
        };
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let message = match frames.next_message() {
                Ok(Some(message)) => message,
                Ok(None) => {
                    match source.read(&mut chunk).await {
                        Ok(n) if n > 0 => frames.buf.extend_from_slice(&chunk[..n]),
                        // The peer went away without a close frame: the runtime reports an abnormal closure
                        // (Bun 1.4.4, which 2.1 runs on, gives it an empty reason; 1.4.2 said "Connection ended").
                        _ => {
                            finish(None).await;
                            let _ = tx.send(Err(close_error(1006, String::new())));
                            return;
                        }
                    }
                    continue;
                }
                // A protocol violation fails the socket: the runtime's error event comes first.
                Err(error) => {
                    finish(error.close_code.map(|code| (code, String::new()))).await;
                    let _ = tx.send(Err(transport_error(
                        format!("WebSocket error: WebSocket connection to '{message_url}' failed: {}", error.message),
                        TransportOperation::Read,
                        WsError { url: Some(&read_url), code: Some("message"), phase: Some("receive"), ..Default::default() },
                    )));
                    return;
                }
            };
            match message {
                client::Inbound::Text(text) => {
                    // `rejectOversized`: the frame is refused and the socket closed with 1009.
                    if text.len() > MAX_FRAME_BYTES {
                        finish(Some((1009, "Message too large".into()))).await;
                        let _ = tx.send(Err(too_large(text)));
                        return;
                    }
                    if tx.send(Ok(WsMessage::Text(text))).is_err() {
                        return;
                    }
                }
                client::Inbound::Binary(bytes) => {
                    if bytes.len() > MAX_FRAME_BYTES {
                        finish(Some((1009, "Message too large".into()))).await;
                        let _ = tx.send(Err(too_large(String::from_utf8_lossy(&bytes).into_owned())));
                        return;
                    }
                    if tx.send(Ok(WsMessage::Binary(bytes))).is_err() {
                        return;
                    }
                }
                client::Inbound::Ping(payload) => {
                    if let Some(sink) = read_sink.lock().await.as_mut() {
                        let _ = sink.control(client::OP_PONG, &payload).await;
                    }
                }
                client::Inbound::Close(code, reason) => {
                    finish(Some((if code == 1005 { 1000 } else { code }, String::new()))).await;
                    let _ = tx.send(Err(close_error(code, reason)));
                    return;
                }
            }
        }
    });
    Ok(WebSocketConnection {
        url: url.to_string(),
        sink,
        messages: parking_lot::Mutex::new(Some(rx)),
        reader: reader.abort_handle(),
        closed,
        deflate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_urls() {
        assert_eq!(to_websocket_url("https://api.openai.com/v1/responses").unwrap(), "wss://api.openai.com/v1/responses");
        assert_eq!(to_websocket_url("http://127.0.0.1:8080/v1/responses?x=1").unwrap(), "ws://127.0.0.1:8080/v1/responses?x=1");
        let err = to_websocket_url("ftp://example.com/x").unwrap_err();
        assert_eq!(err.message, "Unsupported WebSocket URL protocol ftp:");
        assert!(matches!(&err.kind, ErrorKind::Transport { code: Some(c), phase: Some(p), .. } if c == "invalid-url" && p == "prepare"));
    }

    #[test]
    fn annotate_only_touches_transport_errors() {
        let e = annotate(transport_error("x", TransportOperation::Read, WsError::default()), "receive", "accepted");
        assert!(matches!(&e.kind, ErrorKind::Transport { phase: Some(p), delivery: Some(d), .. } if p == "receive" && d == "accepted"));
        let other = annotate(AIError::provider_internal("y"), "receive", "accepted");
        assert_eq!(other.tag(), "ProviderInternal");
    }
}
