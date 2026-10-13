//! The Rust end of the AI SDK bridge (plugin phase 2): AI SDK provider packages (`aisdk:` models)
//! run in the sidecar, which installs/loads the package, runs the internal and plugin
//! `aisdk.sdk` / `aisdk.language` hooks, constructs the `LanguageModelV3` and streams `doStream`
//! parts back. See README "AI SDK bridge".
//!
//! This crate stays independent of oc-ai: values are JSON (the `RuntimeInfo` model, the
//! `LanguageModelV3CallOptions`, the stream parts and error shapes); oc-core adapts them to
//! `oc_ai::aisdk::AiSdkBridge`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use futures::Stream;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::host::PluginHost;
use crate::process::CallError;
use crate::protocol::LocationRef;

/// `session.http.request` / `session.http.response` hooks around the SDK's `fetch` for one
/// stream. Requests and responses are the wire shapes `{url, method, headers, body}` /
/// `{status, statusText, headers, body}` (bodies as text).
#[async_trait]
pub trait AisdkHttp: Send + Sync {
    async fn request(&self, request: Value) -> Result<Value, String>;
    async fn response(&self, request: Value, response: Value) -> Result<Value, String>;
    /// False when nothing observes responses: the body then streams through untouched.
    fn wants_response(&self) -> bool;
}

/// A bridge failure.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AisdkError {
    /// Model construction failed (`AISDK.InitError`): `detail` is upstream's `causeMessage`.
    #[error("{}", .detail.clone().unwrap_or_default())]
    Init { detail: Option<String> },
    /// The provider threw or streamed an error; `operation` is `request` (from `doStream`) or
    /// `read` (while reading), `shape` the error shape (see `oc_ai::aisdk::error_from_shape`).
    #[error("{}", .shape.get("message").and_then(Value::as_str).unwrap_or("Provider request failed"))]
    Provider { operation: String, shape: Value },
    /// The sidecar is unavailable or failed.
    #[error("{0}")]
    Host(String),
}

/// One `doStream` call.
pub struct AisdkStreamRequest {
    pub location: LocationRef,
    /// `RuntimeInfo` (the effective `Model.Info`).
    pub model: Value,
    /// `LanguageModelV3CallOptions` in wire form (`$bytes` / `$url` markers for file data).
    pub options: Value,
    pub http: Option<Arc<dyn AisdkHttp>>,
    /// Enabled internal plugin ids (built-in gating); `None` = all.
    pub enabled: Option<Vec<String>>,
}

pub(crate) struct StreamEntry {
    tx: mpsc::UnboundedSender<Result<Value, AisdkError>>,
    http: Option<Arc<dyn AisdkHttp>>,
}

/// In-flight streams of a host.
#[derive(Default)]
pub(crate) struct Streams {
    next: AtomicU64,
    entries: Mutex<HashMap<u64, StreamEntry>>,
}

impl Streams {
    /// `aisdk.part` notification.
    pub(crate) fn part(&self, params: &Value) {
        let Some(id) = params.get("stream").and_then(Value::as_u64) else { return };
        if let Some(entry) = self.entries.lock().get(&id) {
            let _ = entry.tx.send(Ok(params.get("part").cloned().unwrap_or(Value::Null)));
        }
    }

    pub(crate) fn http(&self, params: &Value) -> Option<Arc<dyn AisdkHttp>> {
        let id = params.get("stream").and_then(Value::as_u64)?;
        self.entries.lock().get(&id).and_then(|e| e.http.clone())
    }
}

fn call_error(error: CallError) -> AisdkError {
    match error {
        CallError::Remote(error) if error.kind.as_deref() == Some("aisdk.error") => {
            let data = error.data.unwrap_or(Value::Null);
            AisdkError::Provider {
                operation: data.get("operation").and_then(Value::as_str).unwrap_or("read").to_string(),
                shape: data.get("shape").cloned().unwrap_or_else(|| json!({ "kind": "unknown", "message": error.message })),
            }
        }
        CallError::Remote(error) if error.kind.as_deref() == Some("aisdk.init") => AisdkError::Init {
            detail: error.data.as_ref().and_then(|d| d.get("detail")).and_then(Value::as_str).map(String::from),
        },
        CallError::Remote(error) => AisdkError::Host(error.message),
        CallError::Closed(message) => AisdkError::Host(format!("Plugin host is not running: {message}")),
    }
}

/// The parts of one stream; dropping it cancels the call in the sidecar.
pub type AisdkParts = std::pin::Pin<Box<dyn Stream<Item = Result<Value, AisdkError>> + Send>>;

impl PluginHost {
    /// `AISDK.language(model)`: constructs (and caches, per location) the language model,
    /// starting the sidecar when needed. `enabled` gates the internal provider plugins.
    pub async fn aisdk_load(&self, location: &LocationRef, model: Value, enabled: Option<Vec<String>>) -> Result<(), AisdkError> {
        let sidecar = self.sidecar().await.map_err(AisdkError::Host)?;
        let mut params = json!({ "location": location.key(), "model": model });
        if let Some(enabled) = enabled {
            params["enabled"] = json!(enabled);
        }
        sidecar.request("aisdk.load", params).await.map(|_| ()).map_err(call_error)
    }

    /// `language.doStream(options)`: the stream parts the sidecar forwards (upstream drops
    /// `stream-start`, `response-metadata`, `raw`, `file`, `source` and `tool-approval-request`,
    /// so they are not sent). A streamed or thrown error ends the stream with
    /// [`AisdkError::Provider`]. Dropping the stream cancels the call.
    pub async fn aisdk_stream(&self, request: AisdkStreamRequest) -> Result<AisdkParts, AisdkError> {
        let sidecar = self.sidecar().await.map_err(AisdkError::Host)?;
        let streams = self.streams();
        let id = streams.next.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = mpsc::unbounded_channel();
        let mut params = json!({
            "location": request.location.key(),
            "model": request.model,
            "options": request.options,
            "stream": id,
        });
        if let Some(http) = &request.http {
            params["http"] = json!({ "response": http.wants_response() });
        }
        if let Some(enabled) = request.enabled {
            params["enabled"] = json!(enabled);
        }
        streams.entries.lock().insert(id, StreamEntry { tx: tx.clone(), http: request.http });
        let owner = streams.clone();
        tokio::spawn(async move {
            let call = sidecar.request("aisdk.stream", params);
            tokio::select! {
                result = call => {
                    if let Err(error) = result {
                        let _ = tx.send(Err(call_error(error)));
                    }
                }
                // The consumer went away: dropping the request future cancels it in the sidecar.
                _ = tx.closed() => {}
            }
            owner.entries.lock().remove(&id);
        });
        Ok(Box::pin(futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) })))
    }

    /// Drops the location's AI SDK caches (its language models and SDKs).
    pub async fn aisdk_close(&self, location: &LocationRef) {
        if let Some(sidecar) = self.running() {
            let _ = sidecar.request("aisdk.close", json!({ "location": location.key() })).await;
        }
    }
}

/// Answers the sidecar's `aisdk.http.request` / `aisdk.http.response` calls.
pub(crate) async fn http_call(streams: &Streams, method: &str, params: &Value) -> Result<Value, String> {
    let http = streams.http(params).ok_or_else(|| "Unknown AI SDK stream".to_string())?;
    let request = params.get("request").cloned().unwrap_or(Value::Null);
    match method {
        "aisdk.http.request" => http.request(request).await,
        _ => http.response(request, params.get("response").cloned().unwrap_or(Value::Null)).await,
    }
}
