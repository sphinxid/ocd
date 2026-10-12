//! Transport abstraction (the SDK `Transport` interface): a transport delivers inbound
//! messages, errors (`onerror`) and close (`onclose`) as [`TransportEvent`]s on a channel given
//! at construction.

pub mod http;
pub mod stdio;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::McpError;

#[derive(Debug)]
pub enum TransportEvent {
    /// An inbound JSON-RPC message (`onmessage`).
    Message(Value),
    /// `onerror`.
    Error(McpError),
    /// `onclose`.
    Closed,
}

pub type EventSink = mpsc::UnboundedSender<TransportEvent>;

/// Per-send options (`TransportSendOptions`).
#[derive(Clone, Default)]
pub struct SendOptions {
    /// Extra request headers (e.g. `Mcp-Param-*`); reserved names are ignored.
    pub headers: Vec<(String, String)>,
    /// Aborts the per-request stream (modern era) when cancelled.
    pub request_cancel: Option<CancellationToken>,
    /// Called when a per-request stream ends without a response.
    pub on_stream_end: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Stdio,
    Http,
}

#[async_trait]
pub trait Transport: Send + Sync + 'static {
    async fn start(&self) -> Result<(), McpError>;
    async fn send(&self, message: Value, options: SendOptions) -> Result<(), McpError>;
    async fn close(&self);
    fn session_id(&self) -> Option<String> {
        None
    }
    fn set_protocol_version(&self, _version: &str) {}
    async fn terminate_session(&self) -> Result<(), McpError> {
        Ok(())
    }
    fn kind(&self) -> TransportKind;
    /// Modern-era HTTP transports carry one stream per request.
    fn has_per_request_stream(&self) -> bool {
        false
    }
}
