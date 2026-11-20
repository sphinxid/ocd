//! MCP Events (SEP-3415), a draft extension (`packages/core/src/mcp/client.ts` `Events`, `mcp/event-errors.ts`).
//! Experimental: it may change without compatibility guarantees.
//!
//! A connection that declared the extension (`events: true`) and whose server offers it
//! (`capabilities.extensions["io.modelcontextprotocol/events"]`) lists its event types with `events/list` and opens
//! push delivery with `events/stream`, a request that stays open while the server sends `notifications/events/*`
//! naming it by its JSON-RPC id.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::connection::Connection;
use crate::error::{McpError, codes};
use crate::protocol::{EventNotification, RequestOptions};

/// Servers send a heartbeat at least every 30 seconds; twice that in silence means the stream is dead.
pub const EVENT_STREAM_IDLE: Duration = Duration::from_secs(60);
const EVENT_LIST_PAGES: usize = 100;

/// JSON-RPC errors that mean an `events/stream` request can never succeed as sent: unknown method, bad arguments, and
/// the events extension's NotFound, Forbidden and Unsupported.
pub const EVENT_STREAM_REJECTED: [i64; 5] = [codes::METHOD_NOT_FOUND, codes::INVALID_PARAMS, -32023, -32024, -32026];

/// One message on an event stream. Persist the latest non-null `cursor` to resume from it.
#[derive(Debug, Clone, PartialEq)]
pub enum EventMessage {
    /// The stream is live. Sent first, and again with `truncated` when the server skipped events it could not replay.
    Active { cursor: Option<String>, truncated: bool },
    Event { event_id: String, name: String, timestamp: String, data: Value, cursor: Option<String> },
    /// Nothing matched; the server has checked up to `cursor`.
    Heartbeat { cursor: Option<String> },
    /// A recoverable failure, such as one failed upstream fetch; the stream stays open. `{code, message, data?}`.
    Error { error: Value },
}

impl EventMessage {
    pub fn cursor(&self) -> Option<&str> {
        match self {
            EventMessage::Active { cursor, .. } | EventMessage::Event { cursor, .. } | EventMessage::Heartbeat { cursor } => {
                cursor.as_deref()
            }
            EventMessage::Error { .. } => None,
        }
    }

    /// The message as the TS client yields it (`{type: "active", cursor, truncated}`, ...).
    pub fn to_json(&self) -> Value {
        match self {
            EventMessage::Active { cursor, truncated } => json!({ "type": "active", "cursor": cursor, "truncated": truncated }),
            EventMessage::Event { event_id, name, timestamp, data, cursor } => {
                json!({ "type": "event", "eventId": event_id, "name": name, "timestamp": timestamp, "data": data, "cursor": cursor })
            }
            EventMessage::Heartbeat { cursor } => json!({ "type": "heartbeat", "cursor": cursor }),
            EventMessage::Error { error } => json!({ "type": "error", "error": error }),
        }
    }
}

/// Why an event stream stopped.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EventStreamError {
    /// `MCP.EventStreamRejectedError`: the server refused the subscription, as for an unknown event type, bad
    /// arguments or missing access.
    #[error("{message}")]
    Rejected { server: String, code: i64, message: String },
    /// `MCP.EventStreamTerminatedError`: the server ended the subscription, as when access was revoked or the event
    /// type was removed or changed.
    #[error("{message}")]
    Terminated { server: String, code: i64, message: String, data: Option<Value> },
    /// The stream broke, fell silent for a minute or failed to open; it can be opened again from the last cursor.
    #[error("{0}")]
    Broken(String),
}

impl EventStreamError {
    /// Rejected or terminated: reopening would not help.
    pub fn ended(&self) -> bool {
        !matches!(self, EventStreamError::Broken(_))
    }
}

/// `events.stream(input)`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamInput {
    pub name: String,
    pub arguments: Option<Value>,
    /// Where to start; `None` starts from now. `Some(None)` sends an explicit `null`.
    pub cursor: Option<Option<String>>,
    /// Replays nothing older than this, even from an older cursor.
    pub max_age_ms: Option<u64>,
}

impl StreamInput {
    fn params(&self) -> Value {
        let mut params = Map::new();
        params.insert("name".into(), json!(self.name));
        if let Some(arguments) = &self.arguments {
            params.insert("arguments".into(), arguments.clone());
        }
        if let Some(cursor) = &self.cursor {
            params.insert("cursor".into(), json!(cursor));
        }
        if let Some(max_age) = self.max_age_ms {
            params.insert("maxAgeMs".into(), json!(max_age));
        }
        Value::Object(params)
    }
}

/// One open `events/stream`. Dropping it cancels the request.
pub struct EventStream {
    server: String,
    messages: mpsc::UnboundedReceiver<Result<EventMessage, EventStreamError>>,
    cancel: CancellationToken,
    done: bool,
}

impl EventStream {
    /// The next message: `Ok(None)` once the server closed the stream, `Err` when it failed. A stream that sends
    /// nothing for a minute fails.
    pub async fn next(&mut self) -> Result<Option<EventMessage>, EventStreamError> {
        if self.done {
            return Ok(None);
        }
        let item = match tokio::time::timeout(EVENT_STREAM_IDLE, self.messages.recv()).await {
            Ok(item) => item,
            Err(_) => {
                self.finish();
                return Err(EventStreamError::Broken(format!(
                    "MCP server {} sent nothing on the event stream for 60 seconds",
                    self.server
                )));
            }
        };
        match item {
            Some(Ok(message)) => Ok(Some(message)),
            Some(Err(error)) => {
                self.finish();
                Err(error)
            }
            None => {
                self.finish();
                Ok(None)
            }
        }
    }

    fn finish(&mut self) {
        self.done = true;
        self.cancel.cancel();
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// MCP Events of one connection, when the connection declared them and the server offers them.
#[derive(Clone)]
pub struct Events {
    connection: Arc<Connection>,
}

impl Events {
    pub fn of(connection: &Arc<Connection>) -> Option<Events> {
        connection.client().offers_events().then(|| Events { connection: connection.clone() })
    }

    pub fn connection(&self) -> &Arc<Connection> {
        &self.connection
    }

    /// Every event type the server offers (`events/list`), each `{name, description?, delivery?, inputSchema?,
    /// payloadSchema?}`, reading at most 100 pages.
    pub async fn list(&self) -> Result<Vec<Value>, McpError> {
        let connection = &self.connection;
        let mut events = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..EVENT_LIST_PAGES {
            let params = match &cursor {
                None => json!({}),
                Some(cursor) => json!({ "cursor": cursor }),
            };
            let page = connection
                .list_value("list MCP events", || async {
                    let result = connection.client().request("events/list", Some(params.clone()), &connection.catalog_options()).await?;
                    decode_event_list(&result)
                })
                .await?;
            events.extend(page.0);
            cursor = page.1;
            if cursor.is_none() {
                return Ok(events);
            }
        }
        tracing::warn!(server = %connection.server(), pages = EVENT_LIST_PAGES, "MCP event list has more pages than read");
        Ok(events)
    }

    /// Push delivery (`events/stream`) from `cursor`, or from now. Ends when the server closes the stream; fails with
    /// [`EventStreamError::Rejected`] when the server refuses the subscription, [`EventStreamError::Terminated`] when
    /// it ends it, and [`EventStreamError::Broken`] when the stream breaks or falls silent for a minute.
    pub fn stream(&self, input: StreamInput) -> EventStream {
        let (tx, rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let connection = self.connection.clone();
        let server = connection.server().to_string();
        let token = cancel.clone();
        tokio::spawn(async move {
            let client = connection.client().clone();
            let (notes_tx, mut notes) = mpsc::unbounded_channel::<EventNotification>();
            let opened: Arc<parking_lot::Mutex<Option<i64>>> = Arc::default();
            let options = RequestOptions {
                // Heartbeats, not a request timeout, tell a quiet stream from a dead one.
                timeout: Some(Duration::MAX),
                cancel: Some(token.clone()),
                on_request_id: Some({
                    let client = client.clone();
                    let opened = opened.clone();
                    Arc::new(move |id| {
                        // Routes the stream's notifications, which can arrive before the request settles.
                        *opened.lock() = Some(id);
                        client.open_event_stream(id, notes_tx.clone());
                    })
                }),
                ..Default::default()
            };
            let params = input.params();
            let request = client.request("events/stream", Some(params), &options);
            tokio::pin!(request);
            let mut settled = false;
            let outcome = loop {
                tokio::select! {
                    biased;
                    Some(note) = notes.recv() => {
                        let message = match note {
                            EventNotification::Active { cursor, truncated } => EventMessage::Active { cursor, truncated },
                            EventNotification::Event { event_id, name, timestamp, data, cursor } => {
                                EventMessage::Event { event_id, name, timestamp, data, cursor }
                            }
                            EventNotification::Heartbeat { cursor } => EventMessage::Heartbeat { cursor },
                            EventNotification::Error { error } => EventMessage::Error { error },
                            EventNotification::Terminated { error } => {
                                break Some(Err(EventStreamError::Terminated {
                                    server: server.clone(),
                                    code: error.get("code").and_then(Value::as_i64).unwrap_or_default(),
                                    message: error.get("message").and_then(Value::as_str).unwrap_or_default().to_string(),
                                    data: error.get("data").cloned(),
                                }));
                            }
                        };
                        if tx.send(Ok(message)).is_err() {
                            break None;
                        }
                    }
                    result = &mut request => {
                        settled = true;
                        // Deliver what arrived before the server closed the stream.
                        while let Ok(note) = notes.try_recv() {
                            let message = match note {
                                EventNotification::Active { cursor, truncated } => EventMessage::Active { cursor, truncated },
                                EventNotification::Event { event_id, name, timestamp, data, cursor } => {
                                    EventMessage::Event { event_id, name, timestamp, data, cursor }
                                }
                                EventNotification::Heartbeat { cursor } => EventMessage::Heartbeat { cursor },
                                EventNotification::Error { error } => EventMessage::Error { error },
                                EventNotification::Terminated { .. } => continue,
                            };
                            let _ = tx.send(Ok(message));
                        }
                        break Some(match result {
                            Ok(_) => Ok(()),
                            Err(McpError::Protocol { code, message, .. }) if EVENT_STREAM_REJECTED.contains(&code) => {
                                Err(EventStreamError::Rejected { server: server.clone(), code, message })
                            }
                            Err(error) => Err(EventStreamError::Broken(connection.failure_message(error))),
                        });
                    }
                    _ = token.cancelled() => break None,
                }
            };
            if let Some(id) = *opened.lock() {
                client.close_event_stream(id);
            }
            if let Some(Err(error)) = outcome {
                if !matches!(error, EventStreamError::Terminated { .. }) {
                    tracing::warn!(server = %server, error = %error, "failed to stream MCP events");
                }
                let _ = tx.send(Err(error));
            }
            // A stream that ends any other way than the server closing it (terminated, dropped, timed out) cancels its
            // request, which sends the cancellation.
            if !settled {
                token.cancel();
                let _ = (&mut request).await;
            }
        });
        EventStream { server: self.connection.server().to_string(), messages: rx, cancel, done: false }
    }
}

/// `EventList`: `{events: EventType[], nextCursor?: string}`; each type keeps only its schema's fields.
fn decode_event_list(result: &Value) -> Result<(Vec<Value>, Option<String>), McpError> {
    let invalid = || McpError::other("Invalid result for events/list");
    let events = result.get("events").and_then(Value::as_array).ok_or_else(invalid)?;
    let mut out = Vec::with_capacity(events.len());
    for event in events {
        let event = event.as_object().ok_or_else(invalid)?;
        let mut kept = Map::new();
        kept.insert("name".into(), json!(event.get("name").and_then(Value::as_str).ok_or_else(invalid)?));
        if let Some(description) = event.get("description") {
            kept.insert("description".into(), json!(description.as_str().ok_or_else(invalid)?));
        }
        if let Some(delivery) = event.get("delivery") {
            let delivery = delivery.as_array().filter(|d| d.iter().all(Value::is_string)).ok_or_else(invalid)?;
            kept.insert("delivery".into(), Value::Array(delivery.clone()));
        }
        for key in ["inputSchema", "payloadSchema"] {
            if let Some(schema) = event.get(key) {
                kept.insert(key.into(), Value::Object(schema.as_object().ok_or_else(invalid)?.clone()));
            }
        }
        out.push(Value::Object(kept));
    }
    let next = match result.get("nextCursor") {
        None => None,
        Some(Value::String(cursor)) => Some(cursor.clone()),
        Some(_) => return Err(invalid()),
    };
    Ok((out, next))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_lists_keep_only_schema_fields() {
        let (events, next) = decode_event_list(&json!({
            "events": [{"name": "job.finished", "description": "A job ended", "inputSchema": {}, "extra": 1}],
            "nextCursor": "page-2"
        }))
        .unwrap();
        assert_eq!(events, vec![json!({"name": "job.finished", "description": "A job ended", "inputSchema": {}})]);
        assert_eq!(next.as_deref(), Some("page-2"));
        assert!(decode_event_list(&json!({"events": [{"description": "no name"}]})).is_err());
        assert!(decode_event_list(&json!({})).is_err());
    }

    /// `fails a stream that sends nothing for a minute` (mcp-events.test.ts): a heartbeat restarts the idle timer.
    #[tokio::test(start_paused = true)]
    async fn fails_a_stream_that_sends_nothing_for_a_minute() {
        let (tx, rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let mut stream = EventStream { server: "jobs".into(), messages: rx, cancel: cancel.clone(), done: false };
        let sender = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(59)).await;
            tx.send(Ok(EventMessage::Heartbeat { cursor: Some("c1".into()) })).unwrap();
            tokio::time::sleep(Duration::from_secs(300)).await;
            drop(tx);
        });
        let start = tokio::time::Instant::now();
        assert_eq!(stream.next().await, Ok(Some(EventMessage::Heartbeat { cursor: Some("c1".into()) })));
        assert_eq!(
            stream.next().await,
            Err(EventStreamError::Broken("MCP server jobs sent nothing on the event stream for 60 seconds".into()))
        );
        assert_eq!(start.elapsed(), Duration::from_secs(119));
        // Timing out cancels the request.
        assert!(cancel.is_cancelled());
        sender.abort();
    }

    #[test]
    fn stream_params() {
        let input = StreamInput { name: "job".into(), arguments: Some(json!({"a": 1})), cursor: Some(Some("c0".into())), max_age_ms: None };
        assert_eq!(input.params(), json!({"name": "job", "arguments": {"a": 1}, "cursor": "c0"}));
        let input = StreamInput { name: "job".into(), cursor: Some(None), ..Default::default() };
        assert_eq!(input.params(), json!({"name": "job", "cursor": null}));
    }
}
