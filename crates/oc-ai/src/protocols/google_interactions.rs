//! Google Interactions protocol (`protocols/google-interactions.ts`, `providers/google-vertex-interactions.ts`).

use super::lifecycle::Lifecycle;
use super::shared::{self, flatten_tool_request, match_tool_choice, tool_result_text, unsupported_content, wrapped_system_update, ToolChoiceMode};
use super::tool_stream::{PendingTool, StreamKey, ToolStream};
use super::{decode_json_frame, route_label, Protocol, Step, StreamParser};
use crate::error::{classify_provider_failure, provider_error_message, AIError, ProviderFailure};
use crate::json::{encode, num, Object};
use crate::media::{MediaAsset, MediaKind, MediaSource};
use crate::route::framing::{Frame, Framing};
use crate::route::{Endpoint, Headers, Route};
use crate::types::*;
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const ADAPTER: &str = "google-interactions";
/// The status of an interaction that continues in another request.
pub const CONTINUATION: &str = "requires_continuation";
/// Reconnects at one checkpoint before giving up. Matches Google's GenAI SDKs (Python, JS, and Go).
pub const MAX_STREAM_RECONNECTS_WITHOUT_PROGRESS: u32 = 3;
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

// https://aistudio.google.com/docs/document-processing#office-documents
const OFFICE_DOCUMENT_MIME_TYPES: [&str; 10] = [
    "application/msword",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.template",
    "application/vnd.oasis.opendocument.text",
    "application/vnd.ms-powerpoint",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "application/vnd.oasis.opendocument.presentation",
    "application/vnd.ms-excel",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "application/vnd.oasis.opendocument.spreadsheet",
];

/// `GoogleInteractions.mediaContent`: one media input as an Interactions content part.
pub fn media_content(asset: &MediaAsset) -> Result<Value, AIError> {
    let media_type = asset.media_type();
    let kind = asset.kind();
    let kind_name = match kind {
        MediaKind::Image => "image",
        MediaKind::Audio => "audio",
        MediaKind::Video => "video",
        _ if media_type == "application/pdf" || media_type == "text/csv" || OFFICE_DOCUMENT_MIME_TYPES.contains(&media_type.as_str()) => "document",
        _ => return Err(AIError::invalid_request(format!("Google Interactions does not support {media_type} document input"))),
    };
    if let Some(url) = super::gemini::http_video_url(asset) {
        return Ok(json!({"type": kind_name, "uri": url, "mime_type": media_type}));
    }
    if let MediaSource::Ref { provider, id, .. } = &asset.source {
        if provider == "google" {
            return Ok(json!({"type": kind_name, "uri": id, "mime_type": media_type}));
        }
    }
    let inline = shared::require_inline_media("Google Interactions", asset)?;
    Ok(json!({"type": kind_name, "data": inline.base64, "mime_type": inline.mime}))
}

const SKIP_THOUGHT_SIGNATURE_VALIDATOR: &str = "skip_thought_signature_validator";

fn signature(pm: Option<&ProviderMetadata>, key: &str) -> Option<String> {
    pm?.get(key)?.as_object()?.get("interactionSignature")?.as_str().map(String::from)
}

fn lower_messages(request: &LLMRequest) -> Result<Vec<Value>, AIError> {
    let mut steps: Vec<Value> = vec![];
    let key = request.metadata_key();
    for message in &request.messages {
        if message.role == Role::System {
            let part = wrapped_system_update("Google Interactions", message)?;
            steps.push(json!({"type": "user_input", "content": [{"type": "text", "text": part.text}]}));
            continue;
        }
        let start = steps.len();
        let step_type = if message.role == Role::Assistant { "model_output" } else { "user_input" };
        let append = |steps: &mut Vec<Value>, content: Value| {
            if steps.len() > start && steps.last().and_then(|l| l.get("type")).and_then(Value::as_str) == Some(step_type) {
                steps.last_mut().unwrap()["content"].as_array_mut().unwrap().push(content);
            } else {
                steps.push(json!({"type": step_type, "content": [content]}));
            }
        };
        let mut has_signed_step = false;
        for part in &message.content {
            if message.role == Role::Tool {
                let ContentPart::ToolResult(r) = part else {
                    return Err(unsupported_content(ADAPTER, Role::Tool, &["tool-result"]));
                };
                let result = match &r.result {
                    ToolResultValue::Json(v) if v.is_object() => v.clone(),
                    ToolResultValue::Content(items) => Value::Array(
                        items
                            .iter()
                            .map(|i| match i {
                                ToolContent::Text { text } => Ok(json!({"type": "text", "text": text})),
                                ToolContent::File { uri, mime, name } => media_content(&super::gemini::tool_media(uri, mime, name.as_deref())),
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                    other => json!(tool_result_text(other)),
                };
                let mut o = Object::new();
                o.insert("type".into(), json!("function_result"));
                o.insert("call_id".into(), json!(r.id));
                o.insert("name".into(), json!(r.name));
                o.insert("result".into(), result);
                if matches!(r.result, ToolResultValue::Error(_)) {
                    o.insert("is_error".into(), json!(true));
                }
                steps.push(Value::Object(o));
                continue;
            }
            match part {
                ContentPart::Text(t) => append(&mut steps, json!({"type": "text", "text": t.text})),
                ContentPart::Media(m) => append(&mut steps, media_content(&m.media)?),
                ContentPart::Reasoning(r) if message.role == Role::Assistant => {
                    let mut o = Object::new();
                    o.insert("type".into(), json!("thought"));
                    if let Some(s) = signature(r.provider_metadata.as_ref(), &key) {
                        o.insert("signature".into(), json!(s));
                        has_signed_step = true;
                    }
                    if !r.text.is_empty() {
                        o.insert("summary".into(), json!([{"type": "text", "text": r.text}]));
                    }
                    steps.push(Value::Object(o));
                }
                ContentPart::ToolCall(c) if message.role == Role::Assistant => {
                    let mut o = Object::new();
                    o.insert("type".into(), json!("function_call"));
                    o.insert("id".into(), json!(c.id));
                    o.insert("name".into(), json!(c.name));
                    o.insert("arguments".into(), c.input.clone());
                    // Google documents this sentinel for replaying function calls after their
                    // original signature was lost.
                    match signature(c.provider_metadata.as_ref(), &key) {
                        Some(s) => {
                            o.insert("signature".into(), json!(s));
                            has_signed_step = true;
                        }
                        None if !has_signed_step => {
                            o.insert("signature".into(), json!(SKIP_THOUGHT_SIGNATURE_VALIDATOR));
                        }
                        None => {}
                    }
                    steps.push(Value::Object(o));
                }
                _ => {
                    let types: &[&str] = if message.role == Role::User { &["text", "media"] } else { &["text", "media", "reasoning", "tool-call"] };
                    return Err(unsupported_content(ADAPTER, message.role, types));
                }
            }
        }
    }
    Ok(steps)
}

fn lenient_str(request: &LLMRequest, key: &str) -> Option<Value> {
    request.po(key).filter(|v| v.is_string()).cloned()
}

pub fn body(request: &LLMRequest) -> Result<Value, AIError> {
    // Upstream `Protocol.make` flattens tool namespaces once before `body.from` for protocols without native ones.
    let flat_request = shared::flatten_request(request);
    let request = &flat_request;
    let (tools, flattened) = flatten_tool_request(request);
    if tools.iter().any(|t| t.native.is_some()) {
        return Err(AIError::invalid_request("Google Interactions hosted tools are not supported"));
    }
    let g = request.generation_options();
    if g.top_k.is_some() || g.frequency_penalty.is_some() || g.presence_penalty.is_some() {
        return Err(AIError::invalid_request("Google Interactions does not support topK, frequencyPenalty, or presencePenalty"));
    }
    let choice = match &request.tool_choice {
        None => None,
        Some(c) => Some(match match_tool_choice(ADAPTER, c)? {
            ToolChoiceMode::Auto => json!("auto"),
            ToolChoiceMode::None => json!("none"),
            ToolChoiceMode::Required => json!("any"),
            ToolChoiceMode::Tool(name) => json!({"allowed_tools": {"mode": "any", "tools": [name]}}),
        }),
    };
    let mut gc = Object::new();
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            gc.insert(k.into(), v);
        }
    };
    put("max_output_tokens", g.max_tokens.map(num));
    put("temperature", g.temperature.map(num));
    put("top_p", g.top_p.map(num));
    put("seed", g.seed.map(num));
    put("stop_sequences", g.stop.clone().map(|s| json!(s)));
    put("thinking_level", lenient_str(request, "thinkingLevel"));
    put("thinking_summaries", lenient_str(request, "thinkingSummaries"));
    put("tool_choice", choice);
    let mut f: IndexMap<String, Option<Value>> = IndexMap::new();
    f.insert("model".into(), Some(json!(request.model.id)));
    f.insert("input".into(), Some(Value::Array(lower_messages(&flattened)?)));
    f.insert("stream".into(), Some(json!(true)));
    f.insert("store".into(), Some(json!(request.po_bool("store").unwrap_or(false))));
    f.insert("previous_interaction_id".into(), lenient_str(request, "previousInteractionId"));
    f.insert(
        "system_instruction".into(),
        if request.system.is_empty() { None } else { Some(json!(shared::join_text(request.system.iter().map(|p| p.text.as_str())))) },
    );
    f.insert("service_tier".into(), lenient_str(request, "serviceTier"));
    // Gemini's `safetySettings` provider option, spelled the Interactions way.
    f.insert(
        "safety_settings".into(),
        super::gemini::safety_settings(request).and_then(|v| v.as_array().cloned()).map(|settings| {
            Value::Array(
                settings
                    .iter()
                    .map(|s| {
                        let category = s["category"].as_str().unwrap_or_default();
                        let category = category.strip_prefix("HARM_CATEGORY_").unwrap_or(category);
                        json!({"type": category.to_lowercase(), "threshold": s["threshold"].as_str().unwrap_or_default().to_lowercase()})
                    })
                    .collect(),
            )
        }),
    );
    f.insert(
        "tools".into(),
        if tools.is_empty() {
            None
        } else {
            Some(Value::Array(
                tools.iter().map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": Value::Object(t.input_schema.clone())})).collect(),
            ))
        },
    );
    f.insert("generation_config".into(), Some(Value::Object(gc)));
    Ok(Value::Object(f.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect()))
}

/// What `GoogleInteractions.transport` tracks over one request's frames to decide whether it may continue the
/// response from a checkpoint.
#[derive(Debug, Default)]
pub struct Checkpoint {
    /// The latest `continuation_token`, starting from the one the request resumed from.
    pub token: Option<String>,
    /// The interaction that owns the request (`interaction_id` / `interaction.id`).
    pub owner: Option<String>,
    pub status: Option<String>,
    /// Output arrived after the latest token, so resuming from it would replay that output.
    pub content_since_token: bool,
    /// `interaction.completed` arrived.
    pub terminal: bool,
    /// Function calls started and not yet stopped.
    pub pending_tools: std::collections::BTreeSet<i64>,
}

impl Checkpoint {
    pub fn new(token: Option<String>) -> Self {
        Self { token, ..Default::default() }
    }

    /// Observe one frame. A frame that does not decode changes nothing (the parser fails on it).
    pub fn observe(&mut self, frame: &Frame) {
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
        let text = |v: Option<&Value>| v.and_then(Value::as_str).map(String::from);
        let non_empty = |v: Option<&Value>| match v {
            Some(Value::Array(items)) => !items.is_empty(),
            Some(Value::String(s)) => !s.is_empty(),
            _ => false,
        };
        let index = event.get("index").and_then(Value::as_i64);
        match event.get("event_type").and_then(Value::as_str) {
            Some("interaction.created") => self.owner = text(event.get("interaction").and_then(|i| i.get("id"))),
            Some("interaction.status_update") => {
                self.status = text(event.get("status"));
                if let Some(owner) = text(event.get("interaction_id")) {
                    self.owner = Some(owner);
                }
                if let Some(token) = text(event.get("continuation_token")).filter(|t| !t.is_empty())
                    && self.token.as_ref() != Some(&token)
                {
                    self.token = Some(token);
                    self.content_since_token = false;
                }
            }
            Some("interaction.completed") => {
                self.terminal = true;
                let interaction = event.get("interaction");
                self.status = text(interaction.and_then(|i| i.get("status")));
                if let Some(owner) = text(interaction.and_then(|i| i.get("id"))) {
                    self.owner = Some(owner);
                }
                if let Some(token) = text(interaction.and_then(|i| i.get("continuation_token"))).filter(|t| !t.is_empty()) {
                    self.token = Some(token);
                    self.content_since_token = false;
                }
            }
            Some("step.start") => {
                let Some(index) = index else { return };
                let step = event.get("step");
                if step.and_then(|s| s.get("type")).and_then(Value::as_str) == Some("function_call") {
                    self.pending_tools.insert(index);
                }
                if self.pending_tools.contains(&index)
                    || non_empty(step.and_then(|s| s.get("content")))
                    || non_empty(step.and_then(|s| s.get("summary")))
                    || non_empty(step.and_then(|s| s.get("signature")))
                {
                    self.content_since_token = true;
                }
            }
            Some("step.delta") => self.content_since_token = true,
            Some("step.stop") => {
                if index.is_some_and(|i| self.pending_tools.remove(&i)) {
                    self.content_since_token = true;
                }
            }
            _ => {}
        }
    }
}

/// The next request of a continued response: `{...body, continuation_token, ...(store && owner ? {input: undefined,
/// previous_interaction_id: owner} : {})}`. The body object holds every key `fromRequest` writes, unset ones too, so
/// `previous_interaction_id` keeps its slot after `store` and `continuation_token` goes last.
pub fn continuation_body(body: &Value, token: &str, owner: Option<&str>) -> Value {
    const ORDER: [&str; 10] = [
        "model",
        "input",
        "stream",
        "store",
        "previous_interaction_id",
        "system_instruction",
        "service_tier",
        "safety_settings",
        "tools",
        "generation_config",
    ];
    let mut next = body.as_object().cloned().unwrap_or_default();
    next.shift_remove("continuation_token");
    if let (Some(true), Some(owner)) = (next.get("store").and_then(Value::as_bool), owner) {
        next.shift_remove("input");
        next.insert("previous_interaction_id".into(), json!(owner));
    }
    let mut out = Object::new();
    for key in ORDER {
        if let Some(value) = next.shift_remove(key) {
            out.insert(key.into(), value);
        }
    }
    out.extend(next);
    out.insert("continuation_token".into(), json!(token));
    Value::Object(out)
}

pub struct GoogleInteractions;

impl Protocol for GoogleInteractions {
    fn id(&self) -> &str {
        ADAPTER
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        body(request)
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(Parser {
            label: route_label(request),
            key: request.metadata_key(),
            lifecycle: Lifecycle::new(),
            hop: 0,
            has_tool_calls: false,
            usage: None,
            continued_usage: vec![],
            steps: BTreeMap::new(),
            tools: ToolStream::new(ADAPTER),
        })
    }
    fn sanitizer(&self) -> Option<&'static str> {
        Some("gemini")
    }
}

pub fn protocol() -> Arc<dyn Protocol> {
    Arc::new(GoogleInteractions)
}

pub fn route() -> Route {
    Route::new(ADAPTER, protocol(), Endpoint::new(Some(DEFAULT_BASE_URL), "/interactions"), Framing::Sse)
        .initial_provider("google", Some("google"))
        .interactions_continuation()
}

/// `google-vertex-interactions` route (base URL configured by the factory).
pub fn vertex_route() -> Route {
    let mut endpoint = Endpoint::new(None, "/interactions");
    endpoint.query.insert("alt".into(), "sse".into());
    Route::new("google-vertex-interactions", protocol(), endpoint, Framing::Sse)
        .initial_provider("google-vertex", Some("vertex"))
        .interactions_continuation()
        .headers_fn(|request| {
            let mut h = Headers::new();
            if let Some(tier) = request.po_str("serviceTier") {
                h.insert("x-vertex-ai-llm-shared-request-type".into(), tier.into());
            }
            h
        })
}

/// Vertex interactions base URL.
pub fn vertex_base_url(location: &str, project: Option<String>, api_key: bool) -> Result<String, crate::error::ProviderConfigurationError> {
    let host = super::gemini::vertex_host(location);
    if api_key {
        return Ok(format!("https://{host}/v1beta1/locations/{location}"));
    }
    Ok(format!("https://{host}/v1beta1/projects/{}/locations/{location}", super::gemini::require_project(project)?))
}

/// `mapUsage(usage, key, continued)`: a response continued across requests bills its input once per request, so the
/// logical call keeps the first request's input and sums the output; the per-request totals stay in provider metadata.
fn map_usage(usage: Option<&Value>, key: &str, continued: &[Option<Value>]) -> Option<Usage> {
    if usage.is_none() && continued.iter().all(Option::is_none) {
        return None;
    }
    let first = if continued.is_empty() { usage } else { continued[0].as_ref() };
    let hops: Vec<Option<&Value>> = continued.iter().map(Option::as_ref).chain(std::iter::once(usage)).collect();
    let field = |hop: Option<&Value>, k: &str| hop.and_then(|h| crate::json::get_u64(h, k));
    let sum = |k: &str| shared::sum_tokens(&hops.iter().map(|hop| field(*hop, k)).collect::<Vec<_>>());
    let input = field(first, "total_input_tokens");
    let cached = field(first, "total_cached_tokens");
    let reasoning = sum("total_thought_tokens");
    let text = sum("total_output_tokens");
    let output = if continued.is_empty() { text.map(|t| t + reasoning.unwrap_or(0)) } else { shared::sum_tokens(&[text, reasoning]) };
    let metadata = if continued.is_empty() {
        usage.cloned().unwrap_or_else(|| json!({}))
    } else {
        // Keep re-billed input in provider metadata, separate from the logical call's input.
        let mut o = Object::new();
        let mut put = |k: &str, v: Option<u64>| {
            if let Some(v) = v {
                o.insert(k.into(), json!(v));
            }
        };
        put("total_input_tokens", sum("total_input_tokens"));
        put("total_cached_tokens", sum("total_cached_tokens"));
        put("total_output_tokens", text);
        put("total_thought_tokens", reasoning);
        put("total_tokens", sum("total_tokens"));
        o.insert("requests".into(), json!(hops.len()));
        Value::Object(o)
    };
    let mut pm = Object::new();
    pm.insert(key.into(), metadata);
    Some(Usage {
        context_tokens: field(first, "raw_prompt_token").or(input),
        input_tokens: input,
        output_tokens: output,
        non_cached_input_tokens: shared::subtract_tokens(input, cached),
        cache_read_input_tokens: cached,
        cache_write_input_tokens: None,
        reasoning_tokens: reasoning,
        total_tokens: if continued.is_empty() { field(usage, "total_tokens") } else { shared::total_tokens(input, output, None) },
        cost: None,
        provider_metadata: Some(pm),
    })
}

struct Parser {
    label: String,
    key: String,
    lifecycle: Lifecycle,
    /// Requests started before the current one for this response.
    hop: usize,
    has_tool_calls: bool,
    /// The current request's latest usage (`interaction.status_update` or a continued `interaction.completed`).
    usage: Option<Value>,
    /// The usage of each earlier request of the response.
    continued_usage: Vec<Option<Value>>,
    steps: BTreeMap<i64, Value>,
    tools: ToolStream,
}

impl Parser {
    fn meta(&self, step: &Value) -> ProviderMetadata {
        let mut o = Object::new();
        let mut inner = Object::new();
        if let Some(s) = step.get("signature") {
            inner.insert("interactionSignature".into(), s.clone());
        }
        o.insert(self.key.clone(), Value::Object(inner));
        o
    }

    /// `blockID`: block ids of a continued request carry its hop, so they never collide with an earlier request's.
    fn block_id(&self, index: i64) -> String {
        if self.hop == 0 { index.to_string() } else { format!("{}:{index}", self.hop) }
    }

    fn err(message: impl Into<String>, body: &Value) -> AIError {
        AIError::invalid_output(ADAPTER, message).with_body(encode(body))
    }

    fn handle(&mut self, event: &Value) -> Result<Step, AIError> {
        let mut events = vec![];
        let invalid = || AIError::invalid_output(ADAPTER, format!("Invalid {} stream event", self.label)).with_body(encode(event));
        let kind = event.get("event_type").and_then(Value::as_str).ok_or_else(invalid)?;
        let index = event.get("index").and_then(Value::as_i64);
        match kind {
            "step.start" => {
                let index = index.ok_or_else(invalid)?;
                let step = event.get("step").filter(|s| s.get("type").map(Value::is_string).unwrap_or(false)).ok_or_else(invalid)?.clone();
                self.lifecycle.step_start(&mut events);
                let id = self.block_id(index);
                match step["type"].as_str().unwrap() {
                    "thought" => {
                        let meta = self.meta(&step);
                        self.lifecycle.reasoning_start(&mut events, &id, Some(meta));
                        for part in step.get("summary").and_then(Value::as_array).cloned().unwrap_or_default() {
                            let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                            self.lifecycle.reasoning_delta(&mut events, &id, text, None);
                        }
                    }
                    "model_output" => {
                        self.lifecycle.text_start(&mut events, &id, None);
                        for part in step.get("content").and_then(Value::as_array).cloned().unwrap_or_default() {
                            let ptype = part.get("type").and_then(Value::as_str).unwrap_or("");
                            if ptype != "text" {
                                return Err(Self::err(format!("Unsupported Interactions output: {ptype}"), &step));
                            }
                            if let Some(t) = part.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                                self.lifecycle.text_delta(&mut events, &id, t, None);
                            }
                        }
                    }
                    "function_call" => {
                        let (Some(call_id), Some(name)) = (
                            step.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()),
                            step.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()),
                        ) else {
                            return Err(Self::err("Interactions function call lacks id or name", &step));
                        };
                        let meta = self.meta(&step);
                        let input = match step.get("arguments").and_then(Value::as_object) {
                            Some(a) if !a.is_empty() => encode(&step["arguments"]),
                            _ => String::new(),
                        };
                        self.tools.start(
                            index,
                            PendingTool { id: call_id.into(), name: name.into(), namespace: None, input, provider_executed: false, provider_metadata: Some(meta.clone()) },
                        );
                        events.push(LLMEvent::ToolInputStart { id: call_id.into(), name: name.into(), namespace: None, provider_executed: None, provider_metadata: Some(meta) });
                    }
                    other => return Err(Self::err(format!("Unsupported Interactions step: {other}"), &step)),
                }
                self.has_tool_calls |= step["type"] == "function_call";
                self.steps.insert(index, step);
            }
            "step.delta" => {
                let index = index.ok_or_else(invalid)?;
                let delta = event.get("delta").filter(|d| d.get("type").map(Value::is_string).unwrap_or(false)).ok_or_else(invalid)?.clone();
                let Some(step) = self.steps.get(&index).cloned() else {
                    return Err(Self::err("Interactions delta without step.start", &delta));
                };
                let step_type = step.get("type").and_then(Value::as_str).unwrap_or("");
                let id = self.block_id(index);
                let dtype = delta["type"].as_str().unwrap();
                // A known delta must have its fields; an unknown one fails explicitly below, so new
                // output modalities never disappear from a successful response.
                let is_str = |v: Option<&Value>| v.map(Value::is_string).unwrap_or(false);
                let well_formed = match dtype {
                    "text" => is_str(delta.get("text")),
                    "arguments_delta" => is_str(delta.get("arguments")),
                    "thought_signature" => is_str(delta.get("signature")),
                    "thought_summary" => delta
                        .get("content")
                        .is_some_and(|c| c.get("type").and_then(Value::as_str) == Some("text") && is_str(c.get("text"))),
                    other => {
                        return Err(Self::err(format!("Unsupported Interactions delta: {other}"), &json!({"type": "unknown", "rawType": other})));
                    }
                };
                if !well_formed {
                    return Err(invalid());
                }
                match dtype {
                    "text" if step_type == "model_output" && delta.get("text").map(Value::is_string).unwrap_or(false) => {
                        self.lifecycle.text_delta(&mut events, &id, delta["text"].as_str().unwrap(), None);
                    }
                    "thought_summary" if step_type == "thought" && delta.get("content").and_then(|c| c.get("text")).map(Value::is_string).unwrap_or(false) => {
                        self.lifecycle.reasoning_delta(&mut events, &id, delta["content"]["text"].as_str().unwrap(), None);
                    }
                    "thought_signature" if delta.get("signature").map(Value::is_string).unwrap_or(false) => {
                        let mut next = step.clone();
                        next.as_object_mut().unwrap().insert("signature".into(), delta["signature"].clone());
                        let meta = self.meta(&next);
                        if let Some(tool) = self.tools.tools.get_mut(&StreamKey::Index(index)) {
                            tool.provider_metadata = Some(meta);
                        }
                        self.steps.insert(index, next);
                    }
                    "arguments_delta" if step_type == "function_call" && delta.get("arguments").map(Value::is_string).unwrap_or(false) => {
                        self.tools.append_existing(index, delta["arguments"].as_str().unwrap(), "Interactions arguments without function call", &mut events)?;
                    }
                    other => return Err(Self::err(format!("Unsupported Interactions delta: {other}"), &delta)),
                }
            }
            "step.stop" => {
                let index = index.ok_or_else(invalid)?;
                let Some(step) = self.steps.get(&index).cloned() else {
                    return Err(AIError::invalid_output(ADAPTER, "Interactions step.stop without step.start"));
                };
                let id = self.block_id(index);
                match step.get("type").and_then(Value::as_str) {
                    Some("thought") => {
                        let meta = self.meta(&step);
                        self.lifecycle.reasoning_end(&mut events, &id, Some(meta), None);
                    }
                    Some("model_output") => self.lifecycle.text_end(&mut events, &id, None, None),
                    _ => {
                        self.tools.finish(index, &mut events)?;
                    }
                }
            }
            // Google may add stream event types: unknown ones are ignored, while a malformed known
            // one still fails.
            "interaction.status_update" => {
                if let Some(usage) = event.get("usage").filter(|u| u.is_object()) {
                    self.usage = Some(usage.clone());
                }
            }
            "interaction.created" => {}
            "error" => {
                let raw = encode(event);
                // `{code?, error_type?, message?}`: a malformed field reads as absent.
                let error = event.get("error").filter(|e| e.is_object());
                let field = |k: &str| error.and_then(|e| e.get(k)).and_then(Value::as_str);
                let code = field("code").or_else(|| field("error_type"));
                let message = field("message")
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .map(String::from)
                    .or_else(|| provider_error_message(&raw).filter(|m| !m.is_empty()))
                    .unwrap_or_else(|| "Google Interactions stream error".into());
                return Err(failure_reason(&self.label, code, message, event.get("error").cloned(), raw));
            }
            "interaction.completed" => {
                let interaction = event.get("interaction").filter(|i| i.get("status").map(Value::is_string).unwrap_or(false)).ok_or_else(invalid)?;
                let status = interaction["status"].as_str().unwrap();
                // `errors: [{code?, message?}]`
                let errors = match interaction.get("errors") {
                    None => vec![],
                    Some(Value::Array(errors))
                        if errors.iter().all(|e| {
                            e.as_object().is_some_and(|o| {
                                o.get("code").map(Value::is_string).unwrap_or(true) && o.get("message").map(Value::is_string).unwrap_or(true)
                            })
                        }) =>
                    {
                        errors.clone()
                    }
                    Some(_) => return Err(invalid()),
                };
                if status == CONTINUATION {
                    if let Some(usage) = interaction.get("usage").filter(|u| u.is_object()) {
                        self.usage = Some(usage.clone());
                    }
                    return Ok(Step::events(events));
                }
                if status == "failed" || status == "cancelled" {
                    let messages: Vec<&str> = errors
                        .iter()
                        .filter_map(|e| e.get("message").and_then(Value::as_str))
                        .filter(|m| !m.trim().is_empty())
                        .collect();
                    let coded = errors.iter().find(|e| e.get("code").and_then(Value::as_str).is_some_and(|c| !c.is_empty()));
                    return Err(failure_reason(
                        &self.label,
                        coded.and_then(|e| e.get("code")).and_then(Value::as_str),
                        if messages.is_empty() { format!("Google Interactions {status}") } else { messages.join("\n") },
                        // The first coded error decides the classification; every message stays visible.
                        Some(coded.unwrap_or(interaction).clone()),
                        encode(event),
                    ));
                }
                let has_tool_calls = self.has_tool_calls;
                let normalized = match status {
                    "completed" if has_tool_calls => FinishReason::ToolCalls,
                    "completed" => FinishReason::Stop,
                    "requires_action" => FinishReason::ToolCalls,
                    "incomplete" => FinishReason::Length,
                    other => return Err(Self::err(format!("Unexpected terminal Interactions status: {other}"), event)),
                };
                self.tools.finish_all(&mut events)?;
                let usage = interaction.get("usage").filter(|u| u.is_object()).or(self.usage.as_ref());
                let usage = map_usage(usage, &self.key, &self.continued_usage);
                let mut inner = Object::new();
                if let Some(id) = interaction.get("id") {
                    inner.insert("interactionId".into(), id.clone());
                }
                let mut pm = Object::new();
                pm.insert(self.key.clone(), Value::Object(inner));
                self.lifecycle.finish(&mut events, FinishReasonDetails::raw(normalized, Some(status.into())), usage, Some(pm));
                return Ok(Step::terminal(events));
            }
            _ => {}
        }
        Ok(Step::events(events))
    }
}

impl StreamParser for Parser {
    fn step(&mut self, frame: &Frame) -> Result<Step, AIError> {
        let event = decode_json_frame(ADAPTER, &self.label, frame)?;
        self.handle(&event)
    }
    /// `NextRequest`: a request boundary, without another logical step or finish. Pending tool calls finish, open
    /// blocks end, and the next request numbers its blocks under a new hop.
    fn next_request(&mut self) -> Result<Vec<LLMEvent>, AIError> {
        let mut events = vec![];
        self.tools.finish_all(&mut events)?;
        for (index, step) in std::mem::take(&mut self.steps) {
            let id = self.block_id(index);
            match step.get("type").and_then(Value::as_str) {
                Some("thought") => {
                    let meta = self.meta(&step);
                    self.lifecycle.reasoning_end(&mut events, &id, Some(meta), None);
                }
                Some("model_output") => self.lifecycle.text_end(&mut events, &id, None, None),
                _ => {}
            }
        }
        self.hop += 1;
        self.tools = ToolStream::new(ADAPTER);
        self.continued_usage.push(self.usage.take());
        Ok(events)
    }
}

const RECOVERABLE_CODES: &[&str] = &["MALFORMED_FUNCTION_CALL", "MALFORMED_TOOL_CALL", "UNEXPECTED_TOOL_CALL", "MALFORMED_RESPONSE"];
const INVALID_OUTPUT_CODES: &[&str] = &["MISSING_THOUGHT_SIGNATURE", "TOO_MANY_TOOL_CALLS", "NO_IMAGE"];

/// `failureReason(route, code, message, data, rawBody)`: Gemini's output codes classify before the generic rules.
fn failure_reason(route: &str, code: Option<&str>, message: String, data: Option<Value>, raw_body: String) -> AIError {
    let normalized = code.map(|c| c.trim().to_uppercase().replace('-', "_")).filter(|c| !c.is_empty());
    match normalized.as_deref() {
        Some(c) if RECOVERABLE_CODES.contains(&c) => AIError::incomplete_stream(route, message).with_body(raw_body),
        Some(c) if INVALID_OUTPUT_CODES.contains(&c) => AIError::invalid_output(route, message).with_body(raw_body),
        Some(c) if c == "CONTENT_BLOCKED" || super::gemini::content_filtered(c) => AIError::new(crate::error::ErrorKind::ContentPolicy, message).with_body(raw_body),
        _ => classify_provider_failure(ProviderFailure { message, data, raw_body: Some(raw_body), ..Default::default() }),
    }
}
