//! MCP Events (SEP-3415), a beta extension on the 2026-07-28 revision (`packages/core/src/mcp/client.ts` `Events`,
//! `mcp/event-stream.ts`). Experimental: it may change without compatibility guarantees.
//!
//! Every connection declares the extension. One that negotiated 2026-07-28 with a server that offers it
//! (`capabilities.extensions["io.modelcontextprotocol/events"]`) lists the server's event types with `events/list`
//! and opens push delivery with `events/stream`, a request that stays open while the server sends
//! `notifications/events/*` naming it by its JSON-RPC id. Each listed type is checked on its own: a malformed one,
//! one with an invalid `x-mcp-header` annotation or with a schema that can't be checked is left out. A stream's
//! arguments are checked against its type's `inputSchema` before it opens, annotated arguments travel as
//! `Mcp-Param-*` headers too, and events whose name or data don't match the type are dropped. Heartbeats, not a
//! request timeout, tell a quiet stream from a dead one. A server whose types change at runtime says so on a
//! `subscriptions/listen` stream, and the types are listed again.

use std::sync::{Arc, Weak};
use std::time::Duration;

use base64::Engine;
use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::connection::Connection;
use crate::error::{McpError, codes};
use crate::protocol::{Client, EventNotification, RequestOptions};

/// A stream is dead once three heartbeat intervals pass without a message: the server's grant, or the spec's
/// recommended 30 seconds before the first grant arrives.
pub const MISSED_HEARTBEATS: u32 = 3;
pub const UNGRANTED_SILENCE: Duration = Duration::from_secs(30 * MISSED_HEARTBEATS as u64);
const EVENT_LIST_PAGES: usize = 100;
/// A watch that was down for long resumes with at most this much backlog, not everything since its cursor.
pub const REPLAY_LIMIT_MS: u64 = 60 * 60 * 1_000;
/// SEP-2243 HeaderMismatch; the SDK does not export the constant.
const HEADER_MISMATCH: i64 = -32020;

/// JSON-RPC errors that mean an `events/stream` request can never succeed as sent: unknown method, bad arguments, and
/// the events extension's NotFound, Forbidden and Unsupported.
pub const EVENT_STREAM_REJECTED: [i64; 5] = [codes::METHOD_NOT_FOUND, codes::INVALID_PARAMS, -32023, -32024, -32026];

/// `reopen` (`event-stream.ts`): a dropped event stream or list-change listener reopens after a delay growing from a
/// second to about 30 seconds, jittered (0.8 to 1.2 times) so connections that broke together don't all reopen at the
/// same moment. `attempt` counts from 0.
pub fn reopen_delay(attempt: u32) -> Duration {
    let base = Duration::from_secs(1u64 << attempt.min(16)).min(Duration::from_secs(30));
    base.mul_f64(rand::Rng::random_range(&mut rand::rng(), 0.8..=1.2))
}

/// One message on an event stream. Persist the latest non-null `cursor` to resume from it.
#[derive(Debug, Clone, PartialEq)]
pub enum EventMessage {
    /// The stream is live. Sent first, and again with `truncated` when the server skipped events it could not replay.
    Active { cursor: Option<String>, truncated: bool },
    /// `data` is always an object.
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

/// Why an event stream stopped, or why it would not open.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EventStreamError {
    /// `MCP.EventStreamRejectedError`: the server refused the subscription, as for an unknown event type, bad
    /// arguments or missing access, or the client refused it for the same reasons before asking.
    #[error("{message}")]
    Rejected { server: String, code: i64, message: String },
    /// `MCP.EventStreamTerminatedError`: the server ended the subscription, as when access was revoked or the event
    /// type was removed or changed.
    #[error("{message}")]
    Terminated { server: String, code: i64, message: String, data: Option<Value> },
    /// The stream broke, missed three heartbeats or failed to open; it can be opened again from the last cursor.
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
    /// Replays nothing older than this, even from an older cursor. Defaults to an hour when resuming from a cursor.
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
        let resuming = matches!(&self.cursor, Some(Some(_)));
        match self.max_age_ms {
            Some(max_age) => {
                params.insert("maxAgeMs".into(), json!(max_age));
            }
            None if resuming => {
                params.insert("maxAgeMs".into(), json!(REPLAY_LIMIT_MS));
            }
            None => {}
        }
        Value::Object(params)
    }
}

/// One open `events/stream`. Dropping it cancels the request.
pub struct EventStream {
    messages: mpsc::UnboundedReceiver<Result<EventMessage, EventStreamError>>,
    cancel: CancellationToken,
    done: bool,
}

impl EventStream {
    /// The next message: `Ok(None)` once the server closed the stream, `Err` when it failed.
    pub async fn next(&mut self) -> Result<Option<EventMessage>, EventStreamError> {
        if self.done {
            return Ok(None);
        }
        match self.messages.recv().await {
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

type Matcher = Box<dyn Fn(&Value) -> bool + Send + Sync>;

/// A listed event type with its `x-mcp-header` annotations and the checks for its arguments and data.
struct Known {
    /// `{name, description?, delivery, inputSchema, payloadSchema}`.
    value: Value,
    headers: Vec<HeaderParam>,
    arguments: Matcher,
    data: Matcher,
}

impl Known {
    fn push(&self) -> bool {
        self.value.get("delivery").and_then(Value::as_array).is_some_and(|d| d.iter().any(|v| v == "push"))
    }
}

/// Maps a request failure like the connection's other requests do (`failure(error)`).
pub(crate) type Failure = Arc<dyn Fn(McpError) -> McpError + Send + Sync>;

/// The MCP Events state of one connection: its listed event types, swapped in whole by one refresh at a time, and the
/// background work that keeps them current.
pub(crate) struct EventsState {
    server: String,
    client: Arc<Client>,
    catalog: Duration,
    failure: Failure,
    types: RwLock<Arc<IndexMap<String, Known>>>,
    refresh: tokio::sync::Mutex<()>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Drop for EventsState {
    fn drop(&mut self) {
        self.stop();
    }
}

impl EventsState {
    /// The state of a connection whose client offers events. Starts following list changes when the server
    /// announces them (`listChanged: true`).
    pub(crate) fn start(server: &str, client: Arc<Client>, catalog: Duration, failure: Failure) -> Arc<EventsState> {
        let state = Arc::new(EventsState {
            server: server.to_string(),
            client: client.clone(),
            catalog,
            failure,
            types: RwLock::new(Arc::default()),
            refresh: tokio::sync::Mutex::new(()),
            tasks: Mutex::new(Vec::new()),
        });
        let settings = client
            .server_capabilities()
            .and_then(|c| c.get("extensions").and_then(|e| e.get(crate::protocol::EVENTS_EXTENSION)).cloned());
        if settings.as_ref().and_then(|s| s.get("listChanged")) == Some(&Value::Bool(true)) {
            state.follow_list_changes();
        }
        state
    }

    /// Stops the background work, as closing the connection does.
    pub(crate) fn stop(&self) {
        for task in self.tasks.lock().drain(..) {
            task.abort();
        }
    }

    /// Relists on `notifications/events/list_changed` (changes that arrive during a relist collapse into one more),
    /// over a `subscriptions/listen` stream held for the connection's life: one the server ends or drops is reopened
    /// after a capped, jittered delay, then relisted to catch changes missed while it was closed. A refusal is not
    /// retried. Open streams keep running.
    fn follow_list_changes(self: &Arc<Self>) {
        let changed = Arc::new(Notify::new());
        {
            let changed = changed.clone();
            self.client.set_on_events_list_changed(move || changed.notify_one());
        }
        let weak: Weak<EventsState> = Arc::downgrade(self);
        let relist = {
            let (weak, changed) = (weak.clone(), changed.clone());
            tokio::spawn(async move {
                loop {
                    changed.notified().await;
                    let Some(state) = weak.upgrade() else { return };
                    let _ = state.list().await;
                }
            })
        };
        let listener = tokio::spawn(async move {
            let mut attempt = 0;
            loop {
                let Some(state) = weak.upgrade() else { return };
                let (client, timeout, server) = (state.client.clone(), state.catalog, state.server.clone());
                drop(state);
                let error = match client.listen(json!({ "eventsListChanged": true }), timeout, None).await {
                    Ok(mut listen) => {
                        changed.notify_one();
                        let _ = (&mut listen.closed).await;
                        McpError::other("the server closed the listen stream")
                    }
                    Err(error) => error,
                };
                tracing::warn!(server = %server, error = %error.describe(), "MCP event list change listener stopped");
                if matches!(error, McpError::Protocol { .. } | McpError::UnsupportedProtocolVersion { .. }) {
                    return;
                }
                tokio::time::sleep(reopen_delay(attempt)).await;
                attempt += 1;
            }
        });
        self.tasks.lock().extend([relist, listener]);
    }

    fn types(&self) -> Arc<IndexMap<String, Known>> {
        self.types.read().clone()
    }

    /// `listEvents`: every page of event types (at most 100), each checked on its own; swaps in the new map.
    async fn list(&self) -> Result<Vec<Value>, McpError> {
        let _refresh = self.refresh.lock().await;
        let options = RequestOptions { timeout: Some(self.catalog), ..Default::default() };
        let mut events: Vec<Value> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..EVENT_LIST_PAGES {
            let params = match &cursor {
                None => json!({}),
                Some(cursor) => json!({ "cursor": cursor }),
            };
            let page = crate::connection::retrying(&self.server, "list MCP events", None, || async {
                let result = self.client.request("events/list", Some(params.clone()), &options).await?;
                decode_event_list(&result)
            })
            .await
            .map_err(|error| (self.failure)(error));
            let (page, next) = match page {
                Ok(page) => page,
                Err(error) => {
                    tracing::warn!(server = %self.server, error = %error, "failed to list MCP events");
                    return Err(error);
                }
            };
            events.extend(page);
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
        if cursor.is_some() {
            tracing::warn!(server = %self.server, pages = EVENT_LIST_PAGES, "MCP event list has more pages than read");
        }
        let mut known: IndexMap<String, Known> = IndexMap::new();
        for raw in events {
            let Some(event) = decode_event_type(&raw) else {
                tracing::warn!(server = %self.server, event = ?raw.get("name"), "skipping malformed MCP event type");
                continue;
            };
            let name = event["name"].as_str().unwrap_or_default().to_string();
            let headers = match header_params(&event["inputSchema"]) {
                Ok(headers) => headers,
                Err(reason) => {
                    tracing::warn!(server = %self.server, event = %name, reason = %reason, "skipping MCP event type with an invalid x-mcp-header");
                    continue;
                }
            };
            let (Some(arguments), Some(data)) = (matcher(&event["inputSchema"]), matcher(&event["payloadSchema"])) else {
                tracing::warn!(server = %self.server, event = %name, "skipping MCP event type with a schema that can't be checked");
                continue;
            };
            known.insert(name, Known { value: event, headers, arguments, data });
        }
        let listed = known.values().map(|k| k.value.clone()).collect();
        *self.types.write() = Arc::new(known);
        Ok(listed)
    }

    /// `check`: fails with [`EventStreamError::Rejected`] when an event type is unknown, offers no push delivery, or
    /// doesn't accept these arguments; with [`EventStreamError::Broken`] when the types could not be listed.
    async fn check(&self, name: &str, arguments: Option<&Value>) -> Result<(), EventStreamError> {
        if !self.types().contains_key(name) {
            self.list().await.map_err(|error| EventStreamError::Broken(error.to_string()))?;
        }
        let types = self.types();
        let reject = |code: i64, message: String| EventStreamError::Rejected { server: self.server.clone(), code, message };
        let Some(known) = types.get(name).filter(|k| k.push()) else {
            return Err(reject(-32023, format!("No push event type {name} on MCP server {}", self.server)));
        };
        let empty = json!({});
        if !(known.arguments)(arguments.unwrap_or(&empty)) {
            return Err(reject(codes::INVALID_PARAMS, format!("Arguments for {name} do not match its inputSchema")));
        }
        Ok(())
    }
}

/// MCP Events of one connection, when the connection declared them and negotiated 2026-07-28 with a server that
/// offers them.
#[derive(Clone)]
pub struct Events {
    connection: Arc<Connection>,
    state: Arc<EventsState>,
}

impl Events {
    pub fn of(connection: &Arc<Connection>) -> Option<Events> {
        connection.events_state().map(|state| Events { connection: connection.clone(), state })
    }

    pub fn connection(&self) -> &Arc<Connection> {
        &self.connection
    }

    /// Every event type the server offers (`events/list`), each `{name, description?, delivery, inputSchema,
    /// payloadSchema}`, reading at most 100 pages and leaving out those whose schemas can't be used.
    pub async fn list(&self) -> Result<Vec<Value>, McpError> {
        self.state.list().await
    }

    /// Fails with [`EventStreamError::Rejected`] when an event type is unknown, offers no push delivery, or doesn't
    /// accept these arguments. [`Events::stream`] runs the same check before opening.
    pub async fn check(&self, name: &str, arguments: Option<&Value>) -> Result<(), EventStreamError> {
        self.state.check(name, arguments).await
    }

    /// Push delivery (`events/stream`) from `cursor`, or from now. Ends when the server closes the stream; fails with
    /// [`EventStreamError::Rejected`] when the type is refused, [`EventStreamError::Terminated`] when the server ends
    /// it, and [`EventStreamError::Broken`] when the stream breaks or misses three heartbeats. Events whose name or
    /// data don't match the event type are dropped.
    pub fn stream(&self, input: StreamInput) -> EventStream {
        let (tx, rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let connection = self.connection.clone();
        let state = self.state.clone();
        let token = cancel.clone();
        tokio::spawn(async move { run_stream(connection, state, input, tx, token).await });
        EventStream { messages: rx, cancel, done: false }
    }
}

/// What one notification on a stream comes to.
enum Delivered {
    Message(EventMessage),
    /// An event whose name or data don't match its type: heard, but dropped.
    Dropped,
    Terminated(EventStreamError),
}

async fn run_stream(
    connection: Arc<Connection>,
    state: Arc<EventsState>,
    input: StreamInput,
    tx: mpsc::UnboundedSender<Result<EventMessage, EventStreamError>>,
    token: CancellationToken,
) {
    let server = connection.server().to_string();
    let client = connection.client().clone();
    let (notes_tx, mut notes) = mpsc::unbounded_channel::<EventNotification>();
    let opened: Arc<Mutex<Vec<i64>>> = Arc::default();
    let request = {
        let (state, client, connection, opened, token, input) =
            (state.clone(), client.clone(), connection.clone(), opened.clone(), token.clone(), input.clone());
        async move {
            state.check(&input.name, input.arguments.as_ref()).await?;
            let send = || {
                let options = RequestOptions {
                    timeout: Some(Duration::MAX),
                    cancel: Some(token.clone()),
                    on_request_id: Some({
                        let (client, opened, notes_tx) = (client.clone(), opened.clone(), notes_tx.clone());
                        Arc::new(move |id| {
                            // Routes the stream's notifications, which can arrive before the request settles.
                            opened.lock().push(id);
                            client.open_event_stream(id, notes_tx.clone());
                        })
                    }),
                    headers: state
                        .types()
                        .get(&input.name)
                        .map(|known| param_headers(&known.headers, input.arguments.as_ref()))
                        .unwrap_or_default(),
                    ..Default::default()
                };
                let client = client.clone();
                let params = input.params();
                async move { client.request("events/stream", Some(params), &options).await }
            };
            // A modern server rejects headers mirrored from a stale or unlisted event type before opening the
            // stream, so relist and retry once with the fresh schema.
            let result = match send().await {
                Err(McpError::Protocol { code: HEADER_MISMATCH, .. }) => match state.list().await {
                    Ok(_) => send().await,
                    Err(error) => Err(error),
                },
                other => other,
            };
            result.map(drop).map_err(|error| match error {
                McpError::Protocol { code, message, .. } if EVENT_STREAM_REJECTED.contains(&code) => {
                    EventStreamError::Rejected { server: connection.server().to_string(), code, message }
                }
                error => EventStreamError::Broken(connection.failure_message(error)),
            })
        }
    };
    tokio::pin!(request);
    // Every message restarts the watchdog's deadline, using the grant it may have just changed.
    let mut silence = UNGRANTED_SILENCE;
    let mut deadline = tokio::time::Instant::now() + silence;
    let deliver = |note: EventNotification, silence: &mut Duration| -> Delivered {
        match note {
            EventNotification::Active { cursor, truncated, heartbeat_ms } => {
                // The server's grant is authoritative, and it comes with every `active`.
                *silence = Duration::from_millis(heartbeat_ms.saturating_mul(MISSED_HEARTBEATS as u64));
                Delivered::Message(EventMessage::Active { cursor, truncated })
            }
            EventNotification::Event { event_id, name, timestamp, data, cursor } => {
                let matches = name == input.name && state.types().get(&input.name).is_some_and(|known| (known.data)(&data));
                if !matches {
                    tracing::warn!(server = %server, event = %input.name, event_id = %event_id, "dropping an MCP event that doesn't match its event type");
                    return Delivered::Dropped;
                }
                Delivered::Message(EventMessage::Event { event_id, name, timestamp, data, cursor })
            }
            EventNotification::Heartbeat { cursor } => Delivered::Message(EventMessage::Heartbeat { cursor }),
            EventNotification::Error { error } => Delivered::Message(EventMessage::Error { error }),
            EventNotification::Terminated { error } => Delivered::Terminated(EventStreamError::Terminated {
                server: server.clone(),
                code: error.get("code").and_then(crate::protocol::int).unwrap_or_default(),
                message: error.get("message").and_then(Value::as_str).unwrap_or_default().to_string(),
                data: error.get("data").cloned(),
            }),
        }
    };
    let mut settled = false;
    let outcome: Option<Result<(), EventStreamError>> = loop {
        tokio::select! {
            biased;
            Some(note) = notes.recv() => {
                match deliver(note, &mut silence) {
                    Delivered::Terminated(error) => break Some(Err(error)),
                    Delivered::Message(message) => {
                        if tx.send(Ok(message)).is_err() {
                            break None;
                        }
                    }
                    Delivered::Dropped => {}
                }
                deadline = tokio::time::Instant::now() + silence;
            }
            result = &mut request, if !settled => {
                settled = true;
                // Deliver what arrived before the server closed the stream.
                while let Ok(note) = notes.try_recv() {
                    if let Delivered::Message(message) = deliver(note, &mut silence) {
                        let _ = tx.send(Ok(message));
                    }
                }
                break Some(result);
            }
            _ = tokio::time::sleep_until(deadline) => {
                break Some(Err(EventStreamError::Broken(format!("MCP server {server} stopped sending event stream heartbeats"))));
            }
            _ = token.cancelled() => break None,
            _ = tx.closed() => break None,
        }
    };
    for id in opened.lock().drain(..) {
        client.close_event_stream(id);
    }
    if let Some(Err(error)) = outcome {
        if !matches!(error, EventStreamError::Terminated { .. }) {
            tracing::warn!(server = %server, error = %error, "failed to stream MCP events");
        }
        let _ = tx.send(Err(error));
    }
    // A stream that ends any other way than the server closing it (terminated, dropped, silent) cancels its request,
    // which sends the cancellation.
    if !settled {
        token.cancel();
        let _ = (&mut request).await;
    }
}

/// `EventList`: `{events: unknown[], nextCursor?: string}`; each type is decoded on its own.
fn decode_event_list(result: &Value) -> Result<(Vec<Value>, Option<String>), McpError> {
    let invalid = || McpError::other("Invalid result for events/list");
    let events = result.get("events").and_then(Value::as_array).ok_or_else(invalid)?;
    let next = match result.get("nextCursor") {
        None => None,
        Some(Value::String(cursor)) => Some(cursor.clone()),
        Some(_) => return Err(invalid()),
    };
    Ok((events.clone(), next))
}

/// `EventType`: `{name, description?, delivery (non-empty, strings; modes added later pass through), inputSchema,
/// payloadSchema}`, keeping only those fields; `None` when malformed.
fn decode_event_type(raw: &Value) -> Option<Value> {
    let event = raw.as_object()?;
    let mut kept = Map::new();
    kept.insert("name".into(), json!(event.get("name")?.as_str()?));
    if let Some(description) = event.get("description") {
        kept.insert("description".into(), json!(description.as_str()?));
    }
    let delivery = event.get("delivery")?.as_array().filter(|d| !d.is_empty() && d.iter().all(Value::is_string))?;
    kept.insert("delivery".into(), Value::Array(delivery.clone()));
    for key in ["inputSchema", "payloadSchema"] {
        kept.insert(key.into(), Value::Object(event.get(key)?.as_object()?.clone()));
    }
    Some(Value::Object(kept))
}

fn has_key(schema: &Value, key: &str) -> bool {
    match schema {
        Value::Object(map) => map.contains_key(key) || map.values().any(|v| has_key(v, key)),
        Value::Array(items) => items.iter().any(|v| has_key(v, key)),
        _ => false,
    }
}

/// Properties a closed object (`additionalProperties: false`) does not declare: decoding with
/// `onExcessProperty: "error"` refuses them.
fn excess(schema: &Value, value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            let properties = schema.get("properties").and_then(Value::as_object);
            if schema.get("additionalProperties") == Some(&Value::Bool(false))
                && map.keys().any(|key| !properties.is_some_and(|p| p.contains_key(key)))
            {
                return true;
            }
            map.iter().any(|(key, item)| properties.and_then(|p| p.get(key)).is_some_and(|s| excess(s, item)))
        }
        Value::Array(items) => schema.get("items").is_some_and(|s| items.iter().any(|item| excess(s, item))),
        _ => false,
    }
}

/// `matcher(schema)`: fails closed, giving `None`, for a schema that can't be compiled (upstream's JSON Schema codec
/// can't be built for a `pattern`). Never coerces or strips fields.
fn matcher(schema: &Value) -> Option<Matcher> {
    if has_key(schema, "pattern") {
        return None;
    }
    let schema = schema.clone();
    Some(Box::new(move |value| {
        oc_tools::validate::validate(&schema, value, oc_tools::validate::Flavor::Json).is_empty() && !excess(&schema, value)
    }))
}

/// SEP-2243 `x-mcp-header`: an annotated argument of an input schema, mirrored as `Mcp-Param-<header>`.
#[derive(Debug, Clone, PartialEq)]
struct HeaderParam {
    path: Vec<String>,
    header: String,
}

static HEADER_TOKEN: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$").expect("header token"));
static SENTINEL: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^=\?base64\?.*\?=$").expect("sentinel"));

/// Subschemas a properties-only chain never passes through; an annotation under one invalidates the schema.
const UNREACHABLE: [&str; 18] = [
    "items",
    "prefixItems",
    "contains",
    "additionalProperties",
    "unevaluatedProperties",
    "unevaluatedItems",
    "propertyNames",
    "patternProperties",
    "dependentSchemas",
    "oneOf",
    "anyOf",
    "allOf",
    "not",
    "if",
    "then",
    "else",
    "$defs",
    "definitions",
];

/// `headerParams(schema)`: the annotated arguments of an input schema, or why its annotations are invalid.
fn header_params(schema: &Value) -> Result<Vec<HeaderParam>, String> {
    fn visit(node: &Value, path: &[String], reachable: bool, seen: &mut Vec<String>, params: &mut Vec<HeaderParam>) -> Option<String> {
        if let Value::Array(items) = node {
            return items.iter().find_map(|child| visit(child, path, false, seen, params));
        }
        let Value::Object(map) = node else { return None };
        if let Some(header) = map.get("x-mcp-header") {
            let at = if path.is_empty() { "<root>".to_string() } else { path.join(".") };
            if !reachable || path.is_empty() {
                return Some(format!("{at}: x-mcp-header is only allowed on a property reached through properties"));
            }
            let Some(header) = header.as_str().filter(|h| HEADER_TOKEN.is_match(h)) else {
                return Some(format!("{at}: x-mcp-header is not a valid header name"));
            };
            if !map.get("type").and_then(Value::as_str).is_some_and(|t| matches!(t, "string" | "integer" | "boolean")) {
                return Some(format!("{at}: x-mcp-header needs a string, integer or boolean type"));
            }
            if seen.contains(&header.to_lowercase()) {
                return Some(format!("{at}: x-mcp-header {header} is declared twice"));
            }
            seen.push(header.to_lowercase());
            params.push(HeaderParam { path: path.to_vec(), header: header.to_string() });
        }
        if let Some(Value::Object(properties)) = map.get("properties") {
            for (key, child) in properties {
                let mut next = path.to_vec();
                next.push(key.clone());
                if let Some(fault) = visit(child, &next, reachable, seen, params) {
                    return Some(fault);
                }
            }
        }
        for (key, child) in map {
            if !UNREACHABLE.contains(&key.as_str()) {
                continue;
            }
            let values;
            let child = match child {
                Value::Object(inner) if matches!(key.as_str(), "patternProperties" | "dependentSchemas" | "$defs" | "definitions") => {
                    values = Value::Array(inner.values().cloned().collect());
                    &values
                }
                other => other,
            };
            if let Some(fault) = visit(child, path, false, seen, params) {
                return Some(fault);
            }
        }
        None
    }
    let mut params = Vec::new();
    match visit(schema, &[], true, &mut Vec::new(), &mut params) {
        Some(fault) => Err(fault),
        None => Ok(params),
    }
}

/// `paramHeaders`: `Mcp-Param-*` headers for the annotated arguments that hold a primitive value. Values that are not
/// plain visible ASCII, or would be read as the sentinel, travel as base64.
fn param_headers(params: &[HeaderParam], args: Option<&Value>) -> Vec<(String, String)> {
    const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
    params
        .iter()
        .filter_map(|param| {
            let mut node = args?;
            for key in &param.path {
                node = node.as_object()?.get(key)?;
            }
            let text = match node {
                Value::String(text) => text.clone(),
                Value::Bool(value) => value.to_string(),
                Value::Number(number) => {
                    let value = number.as_f64()?;
                    if !value.is_finite() || (value.fract() == 0.0 && value.abs() > MAX_SAFE) {
                        return None;
                    }
                    match number.as_i64() {
                        Some(n) => n.to_string(),
                        None => ryu_js::Buffer::new().format(value).to_string(),
                    }
                }
                _ => return None,
            };
            let plain = !text.is_empty()
                && text == text.trim()
                && text.chars().all(|c| c == '\t' || (' '..='~').contains(&c))
                && !SENTINEL.is_match(&text);
            let value = if plain {
                text
            } else {
                format!("=?base64?{}?=", base64::engine::general_purpose::STANDARD.encode(text.as_bytes()))
            };
            Some((format!("Mcp-Param-{}", param.header), value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_lists_decode_each_type_on_its_own() {
        let (events, next) = decode_event_list(&json!({
            "events": [{"name": "job.finished"}, 1],
            "nextCursor": "page-2"
        }))
        .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(next.as_deref(), Some("page-2"));
        assert!(decode_event_list(&json!({})).is_err());
        assert_eq!(
            decode_event_type(&json!({
                "name": "job.finished", "description": "A job ended", "delivery": ["push", "future"],
                "inputSchema": {}, "payloadSchema": {"type": "object"}, "extra": 1
            })),
            Some(json!({"name": "job.finished", "description": "A job ended", "delivery": ["push", "future"], "inputSchema": {}, "payloadSchema": {"type": "object"}}))
        );
        for malformed in [
            json!({"name": "a", "delivery": ["push"], "inputSchema": {}}),
            json!({"name": "a", "delivery": [], "inputSchema": {}, "payloadSchema": {}}),
            json!({"name": "a", "inputSchema": {}, "payloadSchema": {}}),
            json!({"name": "a", "delivery": ["push"], "inputSchema": [], "payloadSchema": {}}),
            json!({"name": "a", "description": null, "delivery": ["push"], "inputSchema": {}, "payloadSchema": {}}),
            json!({"delivery": ["push"], "inputSchema": {}, "payloadSchema": {}}),
        ] {
            assert_eq!(decode_event_type(&malformed), None, "{malformed}");
        }
    }

    #[test]
    fn stream_params_replay_at_most_an_hour_when_resuming() {
        let input = StreamInput { name: "job".into(), arguments: Some(json!({"a": 1})), cursor: Some(Some("c0".into())), max_age_ms: None };
        assert_eq!(input.params(), json!({"name": "job", "arguments": {"a": 1}, "cursor": "c0", "maxAgeMs": 3_600_000}));
        let input = StreamInput { name: "job".into(), cursor: Some(Some("c0".into())), max_age_ms: Some(5), ..Default::default() };
        assert_eq!(input.params(), json!({"name": "job", "cursor": "c0", "maxAgeMs": 5}));
        let input = StreamInput { name: "job".into(), cursor: Some(None), ..Default::default() };
        assert_eq!(input.params(), json!({"name": "job", "cursor": null}));
        let input = StreamInput { name: "job".into(), ..Default::default() };
        assert_eq!(input.params(), json!({"name": "job"}));
    }

    #[test]
    fn matchers_refuse_excess_properties_of_closed_objects_and_fail_closed() {
        let schema = json!({"type": "object", "properties": {"build": {"type": "integer"}}, "required": ["build"], "additionalProperties": false});
        let valid = matcher(&schema).unwrap();
        assert!(valid(&json!({"build": 7})));
        assert!(!valid(&json!({"build": "wrong"})));
        assert!(!valid(&json!({"build": 7, "unexpected": true})));
        let open = json!({"type": "object", "properties": {"status": {"type": "string"}}, "required": ["status"]});
        let valid = matcher(&open).unwrap();
        assert!(valid(&json!({"status": "pending", "more": 1})));
        assert!(!valid(&json!({"status": 2})));
        assert!(matcher(&json!({})).unwrap()(&json!(1)));
        assert!(matcher(&json!({"type": "string", "pattern": "^a"})).is_none());
    }

    fn header(path: &[&str], header: &str) -> HeaderParam {
        HeaderParam { path: path.iter().map(|s| s.to_string()).collect(), header: header.into() }
    }

    #[test]
    fn header_annotations_are_read_through_properties_only() {
        let schema = json!({"type": "object", "properties": {
            "job": {"type": "string", "x-mcp-header": "Job"},
            "nested": {"type": "object", "properties": {"ok": {"type": "boolean", "x-mcp-header": "Ok"}}},
            "plain": {"type": "integer"}
        }});
        assert_eq!(header_params(&schema), Ok(vec![header(&["job"], "Job"), header(&["nested", "ok"], "Ok")]));
        assert_eq!(header_params(&json!({})), Ok(vec![]));
        let fault = |schema: Value| header_params(&schema).unwrap_err();
        assert_eq!(
            fault(json!({"type": "string", "x-mcp-header": "Root"})),
            "<root>: x-mcp-header is only allowed on a property reached through properties"
        );
        assert_eq!(
            fault(json!({"properties": {"list": {"type": "array", "items": {"type": "string", "x-mcp-header": "Item"}}}})),
            "list: x-mcp-header is only allowed on a property reached through properties"
        );
        assert_eq!(
            fault(json!({"$defs": {"a": {"properties": {"b": {"type": "string", "x-mcp-header": "B"}}}}})),
            "b: x-mcp-header is only allowed on a property reached through properties"
        );
        assert_eq!(fault(json!({"properties": {"a": {"type": "string", "x-mcp-header": "a b"}}})), "a: x-mcp-header is not a valid header name");
        assert_eq!(fault(json!({"properties": {"a": {"type": "string", "x-mcp-header": null}}})), "a: x-mcp-header is not a valid header name");
        assert_eq!(
            fault(json!({"properties": {"a": {"type": "number", "x-mcp-header": "A"}}})),
            "a: x-mcp-header needs a string, integer or boolean type"
        );
        assert_eq!(
            fault(json!({"properties": {"a": {"type": "string", "x-mcp-header": "Job"}, "b": {"type": "string", "x-mcp-header": "job"}}})),
            "b: x-mcp-header job is declared twice"
        );
    }

    #[test]
    fn param_headers_mirror_primitive_arguments_and_encode_the_rest() {
        let params = vec![header(&["job"], "Job"), header(&["n"], "N"), header(&["flag"], "Flag"), header(&["deep", "x"], "X")];
        let headers = |args: Value| param_headers(&params, Some(&args));
        let pair = |name: &str, value: &str| (name.to_string(), value.to_string());
        assert_eq!(
            headers(json!({"job": "j1", "n": 1.5, "flag": true, "deep": {"x": 7}})),
            vec![pair("Mcp-Param-Job", "j1"), pair("Mcp-Param-N", "1.5"), pair("Mcp-Param-Flag", "true"), pair("Mcp-Param-X", "7")]
        );
        let b64 = |s: &str| format!("=?base64?{}?=", base64::engine::general_purpose::STANDARD.encode(s));
        assert_eq!(headers(json!({"job": " padded"})), vec![pair("Mcp-Param-Job", &b64(" padded"))]);
        assert_eq!(headers(json!({"job": "héllo"})), vec![pair("Mcp-Param-Job", &b64("héllo"))]);
        assert_eq!(headers(json!({"job": ""})), vec![pair("Mcp-Param-Job", &b64(""))]);
        assert_eq!(headers(json!({"job": "=?base64?x?="})), vec![pair("Mcp-Param-Job", &b64("=?base64?x?="))]);
        assert_eq!(headers(json!({"job": ["a"], "n": 9_007_199_254_740_993u64, "flag": null})), vec![]);
        assert_eq!(param_headers(&params, None), vec![]);
        assert_eq!(headers(json!({"n": 1e21})), vec![]);
        assert_eq!(headers(json!({"n": 0.1})), vec![pair("Mcp-Param-N", "0.1")]);
    }

    #[test]
    fn reopen_delays_grow_to_about_thirty_seconds_with_jitter() {
        for (attempt, base) in [(0, 1.0), (1, 2.0), (2, 4.0), (4, 16.0), (5, 30.0), (40, 30.0)] {
            let delay = reopen_delay(attempt).as_secs_f64();
            assert!(delay >= base * 0.8 - 1e-9 && delay <= base * 1.2 + 1e-9, "{attempt}: {delay}");
        }
    }
}
