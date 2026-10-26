//! Open Responses WebSocket channel (`protocols/open-responses-channel.ts`) and its continuation
//! driver (`protocols/open-responses-continuation.ts`).
//!
//! Routes that carry [`ChannelOptions`] (OpenAI, Azure and xAI Responses) send the request as one
//! `response.create` text frame when the caller supplies a [`ChannelExecutor`]; everything else, and
//! every call without an executor, stays on HTTP SSE. The continuation driver turns the next request
//! of a session into an incremental `previous_response_id` frame when the socket's last completed
//! exchange is a prefix of it.

use super::open_responses::provider_failure_with_body;
use crate::error::{AIError, ErrorKind, TransportOperation};
use crate::json::{encode, js_number_string, Object};
use crate::route::websocket::{self, ChannelCheckpoint, ChannelCreate, ChannelDriver, ChannelMode, ChannelObservation};
use crate::route::Headers;
use serde_json::{Map, Value};
use std::fmt;
use std::sync::Arc;

const PROTOCOL: &str = "open-responses.websocket.v1";
const VERSION: i64 = 1;

/// Fields to send next to `previous_response_id` on an incremental step, or `None` to send the step in
/// full (`OpenResponsesContinuation.Shape`).
pub type ShapeFn = Arc<dyn Fn(&Object) -> Option<Object> + Send + Sync>;

/// `OpenResponsesChannel.Options`.
#[derive(Clone)]
pub struct ChannelOptions {
    pub id: String,
    pub name: String,
    pub rotate_after_ms: Option<u64>,
    /// Whether the endpoint (HTTP URL) offers the WebSocket; default yes.
    pub enabled: Option<Arc<dyn Fn(&str) -> bool + Send + Sync>>,
    /// HTTP URL → URL the socket connects to (before `https:` → `wss:`).
    pub url: Option<Arc<dyn Fn(&str) -> String + Send + Sync>>,
    /// Handshake headers from the request headers.
    pub headers: Option<Arc<dyn Fn(Headers) -> Headers + Send + Sync>>,
    pub continuation: Option<ShapeFn>,
}

impl fmt::Debug for ChannelOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelOptions").field("id", &self.id).field("name", &self.name).field("rotate_after_ms", &self.rotate_after_ms).finish()
    }
}

impl ChannelOptions {
    pub fn new(id: &str, name: &str, rotate_after_ms: Option<u64>) -> Self {
        Self { id: id.into(), name: name.into(), rotate_after_ms, enabled: None, url: None, headers: None, continuation: None }
    }

    pub fn enabled(mut self, f: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        self.enabled = Some(Arc::new(f));
        self
    }

    pub fn url(mut self, f: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        self.url = Some(Arc::new(f));
        self
    }

    pub fn headers(mut self, f: impl Fn(Headers) -> Headers + Send + Sync + 'static) -> Self {
        self.headers = Some(Arc::new(f));
        self
    }

    pub fn continuation(mut self, f: impl Fn(&Object) -> Option<Object> + Send + Sync + 'static) -> Self {
        self.continuation = Some(Arc::new(f));
        self
    }

    /// Whether `url` (the HTTP endpoint) offers the WebSocket.
    pub fn offers(&self, url: &str) -> bool {
        self.enabled.as_ref().is_none_or(|f| f(url))
    }

    /// `transport.prepare` for the channel: the `response.create` request, socket URL and handshake
    /// headers. `json_body` is the HTTP body after the `http.body` overlay; `headers` the request
    /// headers after auth (no `content-type`).
    pub fn prepare(&self, url: &str, json_body: &Value, headers: &Headers) -> Result<PreparedChannel, AIError> {
        let request = create_request(json_body)?;
        let message = encode(&Value::Object(request.clone()));
        let mut headers = match &self.headers {
            Some(f) => f(headers.clone()),
            None => headers.clone(),
        };
        headers.shift_remove("content-length");
        let target = self.url.as_ref().map(|f| f(url)).unwrap_or_else(|| url.to_string());
        Ok(PreparedChannel {
            url: websocket::to_websocket_url(&target)?,
            headers,
            rotate_after_ms: self.rotate_after_ms,
            request,
            message,
        })
    }

    /// The exchange driver: the base protocol checks wrapped by the continuation.
    pub fn driver(&self, prepared: &PreparedChannel) -> ContinuationDriver {
        ContinuationDriver::new(
            &self.id,
            &self.name,
            &prepared.request,
            BaseDriver::new(&self.id, &self.name, &prepared.message),
            self.continuation.clone(),
        )
    }
}

/// The channel half of a prepared request.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedChannel {
    pub url: String,
    pub headers: Headers,
    pub rotate_after_ms: Option<u64>,
    /// The decoded `response.create` request.
    pub request: Object,
    /// Its encoding.
    pub message: String,
}

/// `message(body)`: drop the HTTP-only fields and tag the request `response.create` (the tag first).
pub fn create_request(body: &Value) -> Result<Object, AIError> {
    let Value::Object(body) = body else {
        return Err(AIError::invalid_request("Open Responses WebSocket body must be a JSON object"));
    };
    let mut out = Map::new();
    out.insert("type".into(), Value::String("response.create".into()));
    for (key, value) in body {
        if matches!(key.as_str(), "stream" | "stream_options" | "background" | "type") {
            continue;
        }
        out.insert(key.clone(), value.clone());
    }
    Ok(out)
}

fn event_error(route: &str, message: impl Into<String>, frame: &str) -> AIError {
    AIError::invalid_output(route, message).with_body(frame)
}

/// `OpenResponses.decodeChannelEvent`: one JSON frame through the event schema; an untyped frame
/// reads as an `error` event when it carries one and is ignored otherwise. Returns `None` when the
/// frame is not an Open Responses event.
pub fn decode_channel_event(frame: &str) -> Option<Value> {
    let value = super::open_responses::decode_event(serde_json::from_str(frame).ok()?);
    valid_event(&value).then_some(value)
}

fn optional(o: &Object, key: &str, check: fn(&Value) -> bool) -> bool {
    o.get(key).is_none_or(check)
}

fn optional_null(o: &Object, key: &str, check: fn(&Value) -> bool) -> bool {
    o.get(key).is_none_or(|v| v.is_null() || check(v))
}

fn valid_item(value: &Value) -> bool {
    let Value::Object(o) = value else { return false };
    o.get("type").is_some_and(Value::is_string)
        && ["id", "call_id", "name", "namespace", "arguments"].iter().all(|k| optional(o, k, Value::is_string))
        && optional_null(o, "encrypted_content", Value::is_string)
}

fn valid_details(value: &Value, keys: &[&str]) -> bool {
    let Value::Object(o) = value else { return false };
    keys.iter().all(|k| optional(o, k, Value::is_number))
}

fn valid_usage(value: &Value) -> bool {
    let Value::Object(o) = value else { return false };
    ["input_tokens", "output_tokens", "total_tokens"].iter().all(|k| optional(o, k, Value::is_number))
        && o.get("input_tokens_details").is_none_or(|v| v.is_null() || valid_details(v, &["cached_tokens", "cache_write_tokens"]))
        && o.get("output_tokens_details").is_none_or(|v| v.is_null() || valid_details(v, &["reasoning_tokens"]))
}

/// The `OpenResponses.Event` schema checks (types of the fields the parser reads).
fn valid_event(value: &Value) -> bool {
    let Value::Object(o) = value else { return false };
    if !o.get("type").is_some_and(Value::is_string) {
        return false;
    }
    let strings = ["delta", "arguments", "text", "item_id"].iter().all(|k| optional(o, k, Value::is_string));
    let numbers = ["output_index", "summary_index"].iter().all(|k| optional(o, k, Value::is_number));
    let item = o.get("item").is_none_or(|v| v.is_null() || valid_item(v));
    let response = o.get("response").is_none_or(|r| {
        let Value::Object(r) = r else { return false };
        optional(r, "id", Value::is_string)
            && optional_null(r, "service_tier", Value::is_string)
            && r.get("incomplete_details").is_none_or(|v| {
                v.is_null() || v.as_object().is_some_and(|d| optional(d, "reason", Value::is_string))
            })
            && r.get("output").is_none_or(|v| v.as_array().is_some_and(|items| items.iter().all(valid_item)))
            && r.get("usage").is_none_or(|v| v.is_null() || valid_usage(v))
    });
    strings && numbers && item && response
}

fn event_type(event: &Value) -> &str {
    event.get("type").and_then(Value::as_str).unwrap_or_default()
}

fn response_id(event: &Value) -> Option<&str> {
    event.get("response").and_then(|r| r.get("id")).and_then(Value::as_str)
}

fn as_text(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(js_number_string(n)),
        _ => None,
    }
}

/// `OpenResponses.errorDetail(event).code`.
pub fn error_code(event: &Value) -> Option<String> {
    let raw = event.get("error").filter(|v| !v.is_null()).or_else(|| event.get("response").and_then(|r| r.get("error")));
    let nested = match raw {
        Some(Value::Object(o)) => Some(o),
        _ => None,
    };
    as_text(event.get("code")).or_else(|| as_text(nested.and_then(|n| n.get("code"))))
}

/// The protocol checks of one exchange (`driver` in open-responses-channel.ts).
pub struct BaseDriver {
    id: String,
    name: String,
    message: String,
    response_id: Option<String>,
    terminal: bool,
}

impl BaseDriver {
    pub fn new(id: &str, name: &str, message: &str) -> Self {
        Self { id: id.into(), name: name.into(), message: message.into(), response_id: None, terminal: false }
    }

    fn observe_event(&mut self, event: &Value, frame: &str) -> Result<ChannelObservation, AIError> {
        let (id, name) = (self.id.as_str(), self.name.as_str());
        let kind = event_type(event);
        if self.terminal {
            return Err(event_error(id, format!("{name} emitted {kind} after a terminal event"), frame));
        }
        if kind == "error" {
            self.terminal = true;
            return Ok(ChannelObservation::ProviderFailure(provider_failure_with_body(event, &format!("{name} stream error"), frame)));
        }
        if kind == "response.failed" {
            self.terminal = true;
            if let (Some(current), Some(id2)) = (&self.response_id, response_id(event).filter(|s| !s.is_empty()))
                && current != id2
            {
                return Err(event_error(id, format!("{name} response ID changed during execution"), frame));
            }
            return Ok(ChannelObservation::ProviderFailure(provider_failure_with_body(event, &format!("{name} response failed"), frame)));
        }
        if kind == "response.created" {
            if self.response_id.is_some() {
                return Err(event_error(id, format!("{name} emitted duplicate response.created"), frame));
            }
            let Some(created) = response_id(event).filter(|s| !s.is_empty()) else {
                return Err(event_error(id, format!("{name} response.created is missing response.id"), frame));
            };
            self.response_id = Some(created.to_string());
            return Ok(ChannelObservation::Frame(frame.into()));
        }
        // Keepalives and provider notifications carry no response state and may precede response.created.
        if !kind.starts_with("response.") {
            return Ok(ChannelObservation::Frame(frame.into()));
        }
        let Some(current) = &self.response_id else {
            return Err(event_error(id, format!("{name} emitted {kind} before response.created"), frame));
        };
        if let Some(other) = response_id(event).filter(|s| !s.is_empty())
            && other != current
        {
            return Err(event_error(id, format!("{name} response ID changed during execution"), frame));
        }
        if kind == "response.completed" {
            self.terminal = true;
            return Ok(ChannelObservation::Completed { frame: frame.into(), checkpoint: None });
        }
        if kind == "response.incomplete" {
            self.terminal = true;
            return Ok(ChannelObservation::Incomplete(frame.into()));
        }
        Ok(ChannelObservation::Frame(frame.into()))
    }
}

impl ChannelDriver for BaseDriver {
    fn create(&mut self, _checkpoint: Option<&ChannelCheckpoint>) -> Result<ChannelCreate, AIError> {
        self.response_id = None;
        self.terminal = false;
        Ok(ChannelCreate { message: self.message.clone(), mode: ChannelMode::Full })
    }

    fn observe(&mut self, _create: &ChannelCreate, frame: &str) -> Result<ChannelObservation, AIError> {
        let Some(event) = decode_channel_event(frame) else {
            return Err(event_error(&self.id, format!("Invalid {} WebSocket event", self.name), frame));
        };
        self.observe_event(&event, frame)
    }
}

/// Continuation state of a checkpoint (`CheckpointValue`).
struct CheckpointValue {
    response_id: String,
    request: Object,
    output: Vec<Value>,
}

fn checkpoint_value(checkpoint: Option<&ChannelCheckpoint>) -> Option<CheckpointValue> {
    let checkpoint = checkpoint?;
    if checkpoint.protocol != PROTOCOL {
        return None;
    }
    let value = checkpoint.value.as_object()?;
    if value.get("version").and_then(Value::as_i64) != Some(VERSION) {
        return None;
    }
    let response_id = value.get("responseID").and_then(Value::as_str).filter(|s| !s.trim().is_empty())?;
    let request = value.get("request").and_then(Value::as_object)?;
    let output = value.get("output").and_then(Value::as_array)?;
    Some(CheckpointValue { response_id: response_id.into(), request: request.clone(), output: output.clone() })
}

/// `canonical`: JSON with sorted keys (undefined members are absent here).
pub fn canonical(value: &Value) -> String {
    match value {
        Value::Array(items) => format!("[{}]", items.iter().map(canonical).collect::<Vec<_>>().join(",")),
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            // `Array.prototype.sort` compares UTF-16 code units.
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            let parts: Vec<String> = keys.iter().map(|k| format!("{}:{}", encode(&Value::String((*k).clone())), canonical(&o[k.as_str()]))).collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Number(n) => js_number_string(n),
        other => encode(other),
    }
}

/// `json(value)`: a string holding JSON is compared parsed.
fn parsed(value: &Value) -> Value {
    match value {
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| value.clone()),
        other => other.clone(),
    }
}

fn copy(out: &mut Object, from: &Object, key: &str) {
    if let Some(v) = from.get(key) {
        out.insert(key.into(), v.clone());
    }
}

/// `comparable`: the parts of an item that replay in model input.
fn comparable(value: &Value) -> Value {
    let Value::Object(o) = value else { return value.clone() };
    let kind = o.get("type").and_then(Value::as_str);
    if kind == Some("message") && o.get("role").and_then(Value::as_str) == Some("assistant") {
        let mut out = Object::new();
        out.insert("role".into(), Value::String("assistant".into()));
        if let Some(content) = o.get("content") {
            // Annotations and logprobs describe the response, not the text replayed in model input.
            let content = match content {
                Value::Array(parts) => Value::Array(
                    parts
                        .iter()
                        .map(|part| match part {
                            Value::Object(p) if p.get("type").and_then(Value::as_str) == Some("output_text") => {
                                let mut q = Object::new();
                                copy(&mut q, p, "type");
                                copy(&mut q, p, "text");
                                Value::Object(q)
                            }
                            other => other.clone(),
                        })
                        .collect(),
                ),
                other => other.clone(),
            };
            out.insert("content".into(), content);
        }
        copy(&mut out, o, "phase");
        return Value::Object(out);
    }
    if kind == Some("function_call") {
        let mut out = Object::new();
        copy(&mut out, o, "type");
        copy(&mut out, o, "call_id");
        copy(&mut out, o, "name");
        if let Some(arguments) = o.get("arguments") {
            out.insert("arguments".into(), parsed(arguments));
        }
        return Value::Object(out);
    }
    if kind == Some("reasoning") {
        let mut out = Object::new();
        copy(&mut out, o, "type");
        copy(&mut out, o, "summary");
        copy(&mut out, o, "encrypted_content");
        return Value::Object(out);
    }
    value.clone()
}

/// `invariant`: everything except the type tag, the input and the continuation pointer.
fn invariant(request: &Object) -> Value {
    Value::Object(request.iter().filter(|(k, _)| !matches!(k.as_str(), "type" | "input" | "previous_response_id")).map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// `incremental`: the input items after the checkpoint's request and output, when those are a prefix.
fn incremental(request: &Object, checkpoint: &CheckpointValue) -> Option<Vec<Value>> {
    let input = request.get("input")?.as_array()?;
    let previous = checkpoint.request.get("input")?.as_array()?;
    if canonical(&invariant(request)) != canonical(&invariant(&checkpoint.request)) {
        return None;
    }
    let baseline: Vec<&Value> = previous.iter().chain(checkpoint.output.iter()).collect();
    if input.len() <= baseline.len() {
        return None;
    }
    if !baseline.iter().enumerate().all(|(i, item)| canonical(&comparable(item)) == canonical(&comparable(&input[i]))) {
        return None;
    }
    Some(input[baseline.len()..].to_vec())
}

fn rejected(error: AIError, recovery: &str) -> ChannelObservation {
    let mut out = websocket::transport_error(
        error.message.clone(),
        TransportOperation::Read,
        websocket::WsError { phase: Some("receive"), delivery: Some("rejected"), ..Default::default() },
    );
    if let ErrorKind::Transport { recovery: r, .. } = &mut out.kind {
        *r = Some(recovery.into());
    }
    out.body = error.body;
    out.http = error.http;
    out.cause = error.cause;
    ChannelObservation::Rejected { error: out, recovery: recovery.into() }
}

/// `OpenResponsesContinuation.driver`.
pub struct ContinuationDriver {
    id: String,
    name: String,
    request: Object,
    base: BaseDriver,
    shape: Option<ShapeFn>,
    output: Vec<Value>,
}

impl ContinuationDriver {
    pub fn new(id: &str, name: &str, request: &Object, base: BaseDriver, shape: Option<ShapeFn>) -> Self {
        let mut request = request.clone();
        request.shift_remove("previous_response_id");
        Self { id: id.into(), name: name.into(), request, base, shape, output: vec![] }
    }

    fn has_trigger(&self) -> bool {
        self.request.get("input").and_then(Value::as_array).is_some_and(|items| {
            items.iter().any(|item| item.get("type").and_then(Value::as_str) == Some("compaction_trigger"))
        })
    }
}

impl ChannelDriver for ContinuationDriver {
    fn create(&mut self, checkpoint: Option<&ChannelCheckpoint>) -> Result<ChannelCreate, AIError> {
        self.output.clear();
        let full = || ChannelCreate { message: encode(&Value::Object(self.request.clone())), mode: ChannelMode::Full };
        let Some(previous) = checkpoint_value(checkpoint) else { return Ok(full()) };
        // Ask the route first: diffing the whole history is wasted when it declines the continuation.
        let fields = match &self.shape {
            Some(shape) => shape(&self.request),
            None => Some(self.request.clone()),
        };
        let Some(mut fields) = fields else { return Ok(full()) };
        let Some(delta) = incremental(&self.request, &previous) else { return Ok(full()) };
        fields.insert("input".into(), Value::Array(delta));
        fields.insert("previous_response_id".into(), Value::String(previous.response_id));
        Ok(ChannelCreate { message: encode(&Value::Object(fields)), mode: ChannelMode::Incremental })
    }

    fn observe(&mut self, create: &ChannelCreate, frame: &str) -> Result<ChannelObservation, AIError> {
        let Some(event) = decode_channel_event(frame) else {
            return Err(event_error(&self.id, format!("Invalid {} WebSocket event", self.name), frame));
        };
        let observation = self.base.observe(create, frame)?;
        if event_type(&event) == "response.output_item.done"
            && let Some(item) = event.get("item").filter(|i| !i.is_null())
        {
            self.output.push(item.clone());
        }
        if let ChannelObservation::ProviderFailure(error) = &observation {
            match error_code(&event).as_deref() {
                Some("previous_response_not_found") => return Ok(rejected(error.clone(), "retry-full")),
                Some("websocket_connection_limit_reached") => return Ok(rejected(error.clone(), "rotate-and-retry-full")),
                _ => {}
            }
            // Only the continuation distinguishes an incremental send from a full one, so an unclassified
            // invalid request there is retried full; Codex reports a stale previous_response_id that way, with
            // no code. Classified failures such as context overflow keep their runner-owned recovery.
            if create.mode == ChannelMode::Incremental && matches!(error.kind, ErrorKind::InvalidRequest { classification: None, .. }) {
                return Ok(rejected(error.clone(), "retry-full"));
            }
        }
        let ChannelObservation::Completed { frame, .. } = &observation else { return Ok(observation) };
        // A trigger installs a different context window. Clear the append baseline, retaining the socket.
        if self.has_trigger() {
            return Ok(observation);
        }
        let Some(id) = response_id(&event).filter(|s| !s.trim().is_empty()) else { return Ok(observation) };
        let completed = event.get("response").and_then(|r| r.get("output")).and_then(Value::as_array).filter(|o| !o.is_empty());
        // Completion can re-encrypt reasoning. Callers replay the item already emitted by output_item.done.
        let output: Vec<Value> = match completed {
            Some(items) => items
                .iter()
                .map(|item| {
                    let reasoning = item.get("type").and_then(Value::as_str) == Some("reasoning");
                    match item.get("id") {
                        Some(item_id) if reasoning => self
                            .output
                            .iter()
                            .find(|done| done.get("type") == item.get("type") && done.get("id") == Some(item_id))
                            .cloned()
                            .unwrap_or_else(|| item.clone()),
                        _ => item.clone(),
                    }
                })
                .collect(),
            None => self.output.clone(),
        };
        let mut value = Object::new();
        value.insert("version".into(), Value::from(VERSION));
        value.insert("responseID".into(), Value::String(id.into()));
        value.insert("request".into(), Value::Object(self.request.clone()));
        value.insert("output".into(), Value::Array(output));
        Ok(ChannelObservation::Completed {
            frame: frame.clone(),
            checkpoint: Some(ChannelCheckpoint { protocol: PROTOCOL.into(), value: Value::Object(value) }),
        })
    }
}

/// xAI continues a chain only from stored responses: with `store: false` (the route default)
/// `previous_response_id` fails, so those steps are sent in full over the reused connection. It also
/// rejects `instructions` next to `previous_response_id` and keeps the instructions of the response it
/// continues.
pub fn xai_continuation(request: &Object) -> Option<Object> {
    let mut rest = request.clone();
    rest.shift_remove("instructions");
    if rest.get("store") == Some(&Value::Bool(false)) {
        return None;
    }
    Some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Object {
        v.as_object().unwrap().clone()
    }

    fn driver(request: Value, shape: Option<ShapeFn>) -> ContinuationDriver {
        let request = obj(request);
        let message = encode(&Value::Object(request.clone()));
        ContinuationDriver::new("openai-responses", "OpenAI Responses", &request, BaseDriver::new("openai-responses", "OpenAI Responses", &message), shape)
    }

    fn complete(d: &mut ContinuationDriver, create: &ChannelCreate, id: &str, output: Value) -> Option<ChannelCheckpoint> {
        d.observe(create, &json!({"type": "response.created", "response": {"id": id}}).to_string()).unwrap();
        for item in output.as_array().unwrap() {
            d.observe(create, &json!({"type": "response.output_item.done", "item": item}).to_string()).unwrap();
        }
        match d.observe(create, &json!({"type": "response.completed", "response": {"id": id, "output": output}}).to_string()).unwrap() {
            ChannelObservation::Completed { checkpoint, .. } => checkpoint,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn create_request_tags_first_and_drops_http_fields() {
        let r = create_request(&json!({"model": "m", "stream": true, "input": [], "stream_options": {}, "background": false, "store": false})).unwrap();
        assert_eq!(encode(&Value::Object(r)), r#"{"type":"response.create","model":"m","input":[],"store":false}"#);
        assert_eq!(create_request(&json!([1])).unwrap_err().message, "Open Responses WebSocket body must be a JSON object");
    }

    #[test]
    fn continuation_sends_the_appended_input_incrementally() {
        let user = json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]});
        let call = json!({"type": "function_call", "id": "fc_1", "call_id": "c1", "name": "read", "arguments": "{\"path\":\"a\"}", "status": "completed"});
        let mut first = driver(json!({"type": "response.create", "model": "m", "input": [user], "store": false}), None);
        let create = first.create(None).unwrap();
        assert_eq!(create.mode, ChannelMode::Full);
        let checkpoint = complete(&mut first, &create, "resp_1", json!([call])).unwrap();
        assert_eq!(checkpoint.protocol, "open-responses.websocket.v1");

        // The replayed call has re-serialized arguments and no status: still the same item.
        let replay = json!({"type": "function_call", "id": "fc_1", "call_id": "c1", "name": "read", "arguments": "{\"path\": \"a\"}"});
        let output = json!({"type": "function_call_output", "call_id": "c1", "output": "x"});
        let mut second = driver(json!({"type": "response.create", "model": "m", "input": [user, replay, output], "store": false, "previous_response_id": "ignored"}), None);
        let create = second.create(Some(&checkpoint)).unwrap();
        assert_eq!(create.mode, ChannelMode::Incremental);
        assert_eq!(
            create.message,
            encode(&json!({"type": "response.create", "model": "m", "input": [output], "store": false, "previous_response_id": "resp_1"}))
        );

        // A changed invariant (model) or a rewritten history goes full.
        let mut other = driver(json!({"type": "response.create", "model": "n", "input": [user, replay, output], "store": false}), None);
        assert_eq!(other.create(Some(&checkpoint)).unwrap().mode, ChannelMode::Full);
        let mut rewritten = driver(json!({"type": "response.create", "model": "m", "input": [output, output, output], "store": false}), None);
        assert_eq!(rewritten.create(Some(&checkpoint)).unwrap().mode, ChannelMode::Full);
        // No new items: full.
        let mut same = driver(json!({"type": "response.create", "model": "m", "input": [user, replay], "store": false}), None);
        assert_eq!(same.create(Some(&checkpoint)).unwrap().mode, ChannelMode::Full);
    }

    #[test]
    fn xai_shape_declines_unstored_and_drops_instructions() {
        let user = json!({"type": "message", "role": "user", "content": "a"});
        let shape: ShapeFn = Arc::new(xai_continuation);
        let mut first = driver(json!({"type": "response.create", "model": "m", "instructions": "x", "input": [user], "store": true}), Some(shape.clone()));
        let create = first.create(None).unwrap();
        let checkpoint = complete(&mut first, &create, "resp_1", json!([{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "b", "annotations": []}]}])).unwrap();
        let next = json!({"type": "message", "role": "user", "content": "c"});
        let mut second = driver(
            json!({"type": "response.create", "model": "m", "instructions": "x", "input": [user, {"role": "assistant", "content": [{"type": "output_text", "text": "b"}]}, next], "store": true}),
            Some(shape.clone()),
        );
        let create = second.create(Some(&checkpoint)).unwrap();
        assert_eq!(create.mode, ChannelMode::Incremental);
        assert_eq!(create.message, encode(&json!({"type": "response.create", "model": "m", "input": [next], "store": true, "previous_response_id": "resp_1"})));
        let mut unstored = driver(json!({"type": "response.create", "model": "m", "input": [user, next], "store": false}), Some(shape));
        assert_eq!(unstored.create(Some(&checkpoint)).unwrap().mode, ChannelMode::Full);
    }

    #[test]
    fn rejections_and_protocol_errors() {
        let mut d = driver(json!({"type": "response.create", "model": "m", "input": []}), None);
        let create = d.create(None).unwrap();
        let frame = json!({"type": "error", "status": 400, "error": {"code": "previous_response_not_found", "message": "gone"}}).to_string();
        match d.observe(&create, &frame).unwrap() {
            ChannelObservation::Rejected { error, recovery } => {
                assert_eq!(recovery, "retry-full");
                assert_eq!(error.message, "previous_response_not_found: gone");
                assert_eq!(error.body.as_deref(), Some(frame.as_str()));
                assert!(matches!(&error.kind, ErrorKind::Transport { delivery: Some(d), recovery: Some(r), .. } if d == "rejected" && r == "retry-full"));
            }
            other => panic!("{other:?}"),
        }
        let mut d = driver(json!({"type": "response.create", "model": "m", "input": []}), None);
        let create = d.create(None).unwrap();
        let err = d.observe(&create, r#"{"type":"response.output_text.delta","delta":"x"}"#).unwrap_err();
        assert_eq!(err.message, "OpenAI Responses emitted response.output_text.delta before response.created");
        let err = d.observe(&create, "not json").unwrap_err();
        assert_eq!(err.message, "Invalid OpenAI Responses WebSocket event");
        // A bare error envelope classifies.
        let mut d = driver(json!({"type": "response.create", "model": "m", "input": []}), None);
        let create = d.create(None).unwrap();
        match d.observe(&create, r#"{"error":{"message":"busy","type":"server_error"}}"#).unwrap() {
            ChannelObservation::ProviderFailure(e) => assert_eq!(e.message, "busy"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn canonical_sorts_keys_and_numbers() {
        assert_eq!(canonical(&json!({"b": 1.0, "a": [true, null, "x"]})), r#"{"a":[true,null,"x"],"b":1}"#);
    }
}
