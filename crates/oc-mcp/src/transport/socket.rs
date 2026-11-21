//! MCP client transport over a WebSocket that a client app opened to the server (`mcp/socket-client.ts`). The app
//! runs the MCP server and opencode runs the MCP client, so only the connection direction is inverted: the transport
//! is only ever made for an incoming socket and never dials out.
//!
//! Framing: one JSON-RPC 2.0 message per text frame, JSON-encoded, without batching. A frame that is not valid JSON
//! or not a JSON-RPC message is reported through `onerror` and the connection stays open. Frames that arrive before
//! `start()` are buffered and delivered on start.
//!
//! Closing: `close()` closes the socket with 1000 (normal). The transport learns that the socket closed for any other
//! reason through [`SocketTransport::ended`]; which close codes the endpoint uses is its own concern. Either way
//! `onclose` fires exactly once, and nothing is replayed after a disconnect: a reconnecting client starts a fresh MCP
//! session.

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;

use super::{EventSink, SendOptions, Transport, TransportEvent, TransportKind};
use crate::error::McpError;

/// The side of an incoming WebSocket the transport writes to.
pub trait Socket: Send + Sync + 'static {
    fn send(&self, text: String);
    fn close(&self, code: u16);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Ready,
    Open,
    Closed,
}

struct State {
    phase: Phase,
    pending: VecDeque<String>,
}

pub struct SocketTransport {
    socket: Arc<dyn Socket>,
    events: EventSink,
    state: Mutex<State>,
}

impl SocketTransport {
    pub fn new(socket: Arc<dyn Socket>, events: EventSink) -> Arc<Self> {
        Arc::new(Self { socket, events, state: Mutex::new(State { phase: Phase::Ready, pending: VecDeque::new() }) })
    }

    /// Feeds one inbound text frame.
    pub fn deliver(&self, text: &str) {
        let mut state = self.state.lock();
        match state.phase {
            Phase::Closed => {}
            Phase::Ready => state.pending.push_back(text.to_string()),
            // Received under the lock, so frames keep their FIFO order against a concurrent start's flush.
            Phase::Open => self.receive(text),
        }
    }

    /// Reports that the socket closed; fires `onclose` once.
    pub fn ended(&self) {
        self.finish();
    }

    fn receive(&self, text: &str) {
        let event = match crate::jsonrpc::deserialize_message(text) {
            Ok(message) => TransportEvent::Message(message),
            Err(_) => TransportEvent::Error(McpError::other("Invalid MCP socket frame")),
        };
        let _ = self.events.send(event);
    }

    fn finish(&self) {
        let mut state = self.state.lock();
        if state.phase == Phase::Closed {
            return;
        }
        state.phase = Phase::Closed;
        state.pending.clear();
        let _ = self.events.send(TransportEvent::Closed);
    }
}

#[async_trait]
impl Transport for SocketTransport {
    async fn start(&self) -> Result<(), McpError> {
        let mut state = self.state.lock();
        match state.phase {
            Phase::Closed => Err(McpError::other("Not connected")),
            Phase::Open => Err(McpError::other("Socket transport already started")),
            Phase::Ready => {
                state.phase = Phase::Open;
                while let Some(text) = state.pending.pop_front() {
                    self.receive(&text);
                }
                Ok(())
            }
        }
    }

    async fn send(&self, message: Value, _options: SendOptions) -> Result<(), McpError> {
        if self.state.lock().phase != Phase::Open {
            return Err(McpError::other("Not connected"));
        }
        self.socket.send(serde_json::to_string(&message).unwrap_or_default());
        Ok(())
    }

    async fn close(&self) {
        if self.state.lock().phase == Phase::Closed {
            return;
        }
        self.socket.close(1000);
        self.finish();
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Socket
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[derive(Default)]
    struct Recorder {
        sent: Mutex<Vec<String>>,
        closed: Mutex<Vec<u16>>,
    }

    impl Socket for Recorder {
        fn send(&self, text: String) {
            self.sent.lock().push(text);
        }
        fn close(&self, code: u16) {
            self.closed.lock().push(code);
        }
    }

    fn make() -> (Arc<Recorder>, Arc<SocketTransport>, mpsc::UnboundedReceiver<TransportEvent>) {
        let socket = Arc::new(Recorder::default());
        let (tx, rx) = mpsc::unbounded_channel();
        let transport = SocketTransport::new(socket.clone(), tx);
        (socket, transport, rx)
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<TransportEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(match event {
                TransportEvent::Message(message) => format!("message {message}"),
                TransportEvent::Error(error) => format!("error {error}"),
                TransportEvent::Closed => "closed".to_string(),
            });
        }
        out
    }

    #[tokio::test]
    async fn buffers_frames_until_start_and_keeps_their_order() {
        let (_, transport, mut rx) = make();
        transport.deliver(r#"{"jsonrpc":"2.0","method":"a"}"#);
        transport.deliver(r#"{"jsonrpc":"2.0","method":"b"}"#);
        assert!(drain(&mut rx).is_empty());
        transport.start().await.unwrap();
        transport.deliver(r#"{"jsonrpc":"2.0","method":"c"}"#);
        assert_eq!(
            drain(&mut rx),
            [
                r#"message {"jsonrpc":"2.0","method":"a"}"#,
                r#"message {"jsonrpc":"2.0","method":"b"}"#,
                r#"message {"jsonrpc":"2.0","method":"c"}"#,
            ]
        );
        assert_eq!(transport.start().await.unwrap_err().to_string(), "Socket transport already started");
    }

    #[tokio::test]
    async fn reports_invalid_frames_and_stays_open() {
        let (socket, transport, mut rx) = make();
        transport.start().await.unwrap();
        transport.deliver("not json");
        transport.deliver(r#"{"hello":1}"#);
        transport.deliver(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        assert_eq!(
            drain(&mut rx),
            ["error Invalid MCP socket frame", "error Invalid MCP socket frame", r#"message {"jsonrpc":"2.0","id":1,"result":{}}"#]
        );
        transport.send(serde_json::json!({"jsonrpc": "2.0", "method": "x"}), SendOptions::default()).await.unwrap();
        assert_eq!(*socket.sent.lock(), [r#"{"jsonrpc":"2.0","method":"x"}"#]);
    }

    #[tokio::test]
    async fn close_closes_the_socket_normally_and_fires_onclose_once() {
        let (socket, transport, mut rx) = make();
        transport.start().await.unwrap();
        transport.close().await;
        transport.close().await;
        transport.ended();
        assert_eq!(*socket.closed.lock(), [1000]);
        assert_eq!(drain(&mut rx), ["closed"]);
        let error = transport.send(serde_json::json!({}), SendOptions::default()).await.unwrap_err();
        assert_eq!(error.to_string(), "Not connected");
        transport.deliver(r#"{"jsonrpc":"2.0","method":"late"}"#);
        assert!(drain(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn ended_drops_buffered_frames_and_refuses_start() {
        let (socket, transport, mut rx) = make();
        transport.deliver(r#"{"jsonrpc":"2.0","method":"a"}"#);
        transport.ended();
        assert_eq!(drain(&mut rx), ["closed"]);
        assert_eq!(transport.start().await.unwrap_err().to_string(), "Not connected");
        assert!(socket.closed.lock().is_empty());
        let error = transport.send(serde_json::json!({}), SendOptions::default()).await.unwrap_err();
        assert_eq!(error.to_string(), "Not connected");
    }
}
