//! Open Responses baseline (`protocols/open-responses.ts`) plus the hosted-tool helper
//! (`utils/responses-hosted-tools.ts`). Provider variants live in `openai_responses` and
//! `responses_variants`.

use super::lifecycle::Lifecycle;
use super::options::{self, ResponsesOptions};
use super::shared::{
    self, flatten_tool_request, match_tool_choice, prompt_cache_key, tool_file_media, tool_result_text,
    unsupported_content, ToolChoiceMode,
};
use super::tool_stream::{PendingTool, StreamKey, ToolStream};
use super::{decode_json_frame, route_label, Protocol, Step, StreamParser};
use crate::compile::effort_update;
use crate::error::{classify_provider_failure, AIError, ProviderFailure};
use crate::json::{encode, num, Object};
use crate::media::MediaKind;
use crate::route::framing::{Frame, Framing};
use crate::route::{Endpoint, Route};
use crate::types::*;
use indexmap::{IndexMap, IndexSet};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

pub const ADAPTER: &str = "open-responses";
pub const NAME: &str = "Open Responses";
pub const PATH: &str = "/responses";
const DEFAULT_EFFORT: &str = "medium";

pub type Fields = IndexMap<String, Option<Value>>;

pub fn finalize(fields: Fields) -> Value {
    Value::Object(fields.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect())
}

/// Remove the keys whose value is `null` (`optionalNullish`: an explicit `null` means absent).
fn drop_nulls(o: &mut Object, keys: &[&str]) {
    for key in keys {
        if o.get(*key).is_some_and(Value::is_null) {
            o.shift_remove(*key);
        }
    }
}

/// `lenient(Schema.Number)`: a malformed value decodes as absent.
fn drop_non_numbers(o: &mut Object, keys: &[&str]) {
    for key in keys {
        if o.get(*key).is_some_and(|v| !v.is_number()) {
            o.shift_remove(*key);
        }
    }
}

fn decode_stream_item(item: &mut Value) {
    if let Some(o) = item.as_object_mut() {
        drop_nulls(o, &["id", "call_id", "name", "namespace", "arguments"]);
    }
}

/// The decode half of the `OpenResponses.Event` schema: untyped frames become `ignored` (or `error`
/// when they carry one), `null` in the nullish fields reads as absent, and malformed usage token
/// counts are dropped. Shared by the SSE parser and the WebSocket channel.
pub fn decode_event(value: Value) -> Value {
    let mut value = shared::with_untyped_frame(value);
    let Some(o) = value.as_object_mut() else { return value };
    drop_nulls(o, &["delta", "arguments", "text", "output_index", "summary_index", "response"]);
    if let Some(item) = o.get_mut("item") {
        decode_stream_item(item);
    }
    if let Some(r) = o.get_mut("response").and_then(Value::as_object_mut) {
        drop_nulls(r, &["id", "output"]);
        if let Some(d) = r.get_mut("incomplete_details").and_then(Value::as_object_mut) {
            drop_nulls(d, &["reason"]);
        }
        if let Some(Value::Array(items)) = r.get_mut("output") {
            items.iter_mut().for_each(decode_stream_item);
        }
        if let Some(u) = r.get_mut("usage").and_then(Value::as_object_mut) {
            drop_non_numbers(u, &["input_tokens", "output_tokens", "total_tokens"]);
            if let Some(d) = u.get_mut("input_tokens_details").and_then(Value::as_object_mut) {
                drop_non_numbers(d, &["cached_tokens", "cache_write_tokens"]);
            }
            if let Some(d) = u.get_mut("output_tokens_details").and_then(Value::as_object_mut) {
                drop_non_numbers(d, &["reasoning_tokens"]);
            }
        }
    }
    value
}

/// A provider-executed tool item handled on `response.output_item.done`.
#[derive(Clone)]
pub struct HostedTool {
    pub item_type: &'static str,
    pub name: &'static str,
    pub input: fn(&Value) -> Value,
    pub result: Option<fn(&str, &Value) -> Result<ToolResultValue, AIError>>,
}

/// Provider-specific lowering/raising hooks (`ProviderAdapter`).
#[derive(Clone)]
pub struct Adapter {
    pub id: &'static str,
    pub name: &'static str,
    /// Lower a `native` tool definition into a hosted tool.
    pub native_tool: Option<fn(&Object) -> Result<Value, AIError>>,
    /// Recognize a replayable hosted tool item.
    pub restore_hosted: Option<fn(&Value) -> bool>,
    pub hosted_tools: Vec<HostedTool>,
    /// OpenAI: handle `response.reasoning_text.delta`.
    pub reasoning_text_delta: bool,
    /// Meta: replay unseen completed output items and attach message annotations.
    pub meta: bool,
}

impl Adapter {
    pub fn base() -> Self {
        Self {
            id: ADAPTER,
            name: NAME,
            native_tool: None,
            restore_hosted: None,
            hosted_tools: vec![],
            reasoning_text_delta: false,
            meta: false,
        }
    }
}

/// `metadataKey(model)`: `route.providerMetadataKey ?? "openresponses"`.
pub fn metadata_key(request: &LLMRequest) -> String {
    request.model.route.provider_metadata_key.clone().unwrap_or_else(|| "openresponses".into())
}

// ---------------------------------------------------------------------------------------------
// Lowering
// ---------------------------------------------------------------------------------------------

pub fn lower_tool(protocol_name: &str, tool: &ToolDefinition) -> Result<Value, AIError> {
    if tool.native.is_some() {
        return Err(AIError::invalid_request(format!("{protocol_name} does not support provider-native tool {}", tool.name)));
    }
    Ok(json!({"type": "function", "name": tool.name, "description": tool.description, "parameters": Value::Object(tool.input_schema.clone()), "strict": false}))
}

pub fn lower_tools(tools: &[ToolDefinition], adapter: &Adapter) -> Result<Vec<Value>, AIError> {
    tools
        .iter()
        .map(|t| match (&t.native, adapter.native_tool) {
            (Some(native), Some(f)) => f(native),
            _ => lower_tool(adapter.name, t),
        })
        .collect()
}

pub fn lower_tool_choice(protocol_name: &str, choice: &ToolChoice) -> Result<Value, AIError> {
    Ok(match match_tool_choice(protocol_name, choice)? {
        ToolChoiceMode::Auto => json!("auto"),
        ToolChoiceMode::None => json!("none"),
        ToolChoiceMode::Required => json!("required"),
        ToolChoiceMode::Tool(name) => json!({"type": "function", "name": name}),
    })
}

fn item_id(pm: Option<&ProviderMetadata>, key: &str) -> Option<String> {
    let id = pm?.get(key)?.as_object()?.get("itemId")?.as_str()?;
    let sep = id.find('_')?;
    if sep > 0 && sep < id.len() - 1 { Some(id.to_string()) } else { None }
}

fn message_phase(value: Option<&Value>) -> Option<Value> {
    match value {
        Some(Value::Null) => Some(Value::Null),
        Some(Value::String(s)) if s == "commentary" || s == "final_answer" => Some(Value::String(s.clone())),
        _ => None,
    }
}

fn lower_media(part: &MediaPart, request: &LLMRequest, adapter: &Adapter, tool_result: bool) -> Result<Value, AIError> {
    let key = metadata_key(request);
    let detail = match part.provider_metadata.as_ref().and_then(|pm| pm.get(&key)).and_then(|m| m.get("detail")) {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(AIError::invalid_request("Invalid image detail")),
    };
    let mime = part.media.media_type().to_lowercase();
    let url = part.media.url_value().map(String::from);
    let location = match &url {
        Some(u) => u.clone(),
        None => shared::require_inline_media(adapter.name, &part.media)?.data_url,
    };
    if part.media.kind() != MediaKind::Image {
        if tool_result && part.media.kind() == MediaKind::Video {
            return Ok(json!({"type": "input_video", "video_url": location}));
        }
        let mut o = Object::new();
        o.insert("type".into(), json!("input_file"));
        o.insert(
            "filename".into(),
            json!(part.filename.clone().unwrap_or_else(|| if mime == "application/pdf" { "document.pdf".into() } else { "file".into() })),
        );
        if let Some(d) = detail {
            o.insert("detail".into(), json!(d));
        }
        match url {
            Some(u) => o.insert("file_url".into(), json!(u)),
            None => o.insert("file_data".into(), json!(location)),
        };
        return Ok(Value::Object(o));
    }
    let mut o = Object::new();
    o.insert("type".into(), json!("input_image"));
    o.insert("image_url".into(), json!(location));
    if let Some(d) = detail {
        o.insert("detail".into(), json!(d));
    }
    Ok(Value::Object(o))
}

fn lower_message_media(part: &MediaPart, request: &LLMRequest, adapter: &Adapter) -> Result<Value, AIError> {
    let lowered = lower_media(part, request, adapter, false)?;
    if lowered.get("type").and_then(Value::as_str) == Some("input_video") {
        return Err(AIError::invalid_request(format!("{} user messages do not support input_video", adapter.name)));
    }
    Ok(lowered)
}

fn lower_content_item(item: &ToolContent, request: &LLMRequest, adapter: &Adapter, hosted: bool) -> Result<Value, AIError> {
    match item {
        ToolContent::Text { text } => Ok(json!({"type": "input_text", "text": text})),
        ToolContent::File { uri, mime, name } => {
            let media = tool_file_media(uri, mime, name.as_deref());
            if hosted { lower_message_media(&media, request, adapter) } else { lower_media(&media, request, adapter, true) }
        }
    }
}

/// Built-in hosted item validation (`HostedToolItem` union).
fn is_builtin_hosted(value: &Value) -> bool {
    let Some(o) = value.as_object() else { return false };
    let id_ok = o.get("id").map(Value::is_string).unwrap_or(false);
    let opt_str = |k: &str| o.get(k).map(Value::is_string).unwrap_or(true);
    let opt_null_obj = |k: &str| o.get(k).map(|v| v.is_object() || v.is_null()).unwrap_or(true);
    if !id_ok || !opt_str("status") {
        return false;
    }
    match o.get("type").and_then(Value::as_str) {
        Some("web_search_call") => opt_null_obj("action"),
        Some("file_search_call") => {
            o.get("queries").map(|q| q.as_array().map(|a| a.iter().all(Value::is_string)).unwrap_or(false)).unwrap_or(true)
                && o.get("results").map(|r| r.is_null() || r.as_array().map(|a| a.iter().all(Value::is_object)).unwrap_or(false)).unwrap_or(true)
        }
        Some("code_interpreter_call") => {
            o.get("code").map(|v| v.is_string() || v.is_null()).unwrap_or(true)
                && o.get("container_id").map(|v| v.is_string() || v.is_null()).unwrap_or(true)
                && o.get("outputs").map(|r| r.is_null() || r.as_array().map(|a| a.iter().all(Value::is_object)).unwrap_or(false)).unwrap_or(true)
        }
        Some("mcp_call") => {
            opt_str("server_label") && opt_str("name") && opt_str("arguments") && o.get("output").map(|v| v.is_string() || v.is_null()).unwrap_or(true)
        }
        _ => false,
    }
}

fn lower_messages(request: &LLMRequest, adapter: &Adapter) -> Result<Vec<Value>, AIError> {
    let mut input: Vec<Value> = vec![];
    let key = metadata_key(request);
    for message in &request.messages {
        let metadata = match message.provider_metadata.as_ref().and_then(|pm| pm.get(&key)) {
            None => None,
            Some(Value::Object(o)) => {
                let valid = o.get("itemId").map(Value::is_string).unwrap_or(true)
                    && o.get("status").map(Value::is_string).unwrap_or(true)
                    && o.get("phase").map(|p| message_phase(Some(p)).is_some()).unwrap_or(true);
                if !valid {
                    return Err(AIError::invalid_request("Invalid Open Responses message metadata"));
                }
                Some(o.clone())
            }
            Some(_) => return Err(AIError::invalid_request("Invalid Open Responses message metadata")),
        };
        let meta_item = metadata.as_ref().and_then(|m| m.get("itemId")).and_then(Value::as_str).map(String::from);
        match message.role {
            Role::System => {
                if let Some(update) = effort_update(message) {
                    if input.last().and_then(|l| l.get("type")).and_then(Value::as_str) == Some("configuration_update") {
                        input.pop();
                    }
                    input.push(json!({"type": "configuration_update", "reasoning": {"effort": update.effort.clone().unwrap_or_else(|| DEFAULT_EFFORT.into())}}));
                    continue;
                }
                let texts = shared::system_update_text(adapter.name, message)?;
                input.push(json!({"type": "message", "role": "developer", "content": shared::join_text(texts.iter().map(|t| t.text.as_str()))}));
            }
            Role::User => {
                let mut content = vec![];
                for part in &message.content {
                    match part {
                        ContentPart::Text(t) => content.push(json!({"type": "input_text", "text": t.text})),
                        ContentPart::Media(m) => content.push(lower_message_media(m, request, adapter)?),
                        _ => return Err(unsupported_content(adapter.name, Role::User, &["text", "media"])),
                    }
                }
                if !content.is_empty() {
                    let mut o = Object::new();
                    o.insert("type".into(), json!("message"));
                    o.insert("role".into(), json!("user"));
                    o.insert("content".into(), Value::Array(content));
                    if let Some(id) = &meta_item {
                        o.insert("id".into(), json!(id));
                    }
                    if let Some(status) = metadata.as_ref().and_then(|m| m.get("status")) {
                        o.insert("status".into(), status.clone());
                    }
                    input.push(Value::Object(o));
                }
            }
            Role::Assistant => {
                let message_phase_value = metadata.as_ref().and_then(|m| m.get("phase")).cloned();
                let mut text_parts: Vec<&TextPart> = vec![];
                let mut reasoning_index: BTreeMap<String, usize> = BTreeMap::new();
                let mut hosted_ids: HashSet<String> = HashSet::new();
                let flush = |text_parts: &mut Vec<&TextPart>, input: &mut Vec<Value>| {
                    if text_parts.is_empty() {
                        return;
                    }
                    struct Group {
                        id: Option<String>,
                        phase: Option<Value>,
                        parts: Vec<String>,
                    }
                    let mut groups: Vec<Group> = vec![];
                    for part in text_parts.iter() {
                        let pm = part.provider_metadata.as_ref().and_then(|pm| pm.get(&key));
                        let id = item_id(part.provider_metadata.as_ref(), &key).or_else(|| meta_item.clone());
                        let part_phase = message_phase(pm.and_then(|m| m.get("phase")));
                        let phase = if part_phase.is_none() { message_phase_value.clone() } else { part_phase };
                        match groups.last_mut() {
                            Some(g) if g.id == id && g.phase == phase => g.parts.push(part.text.clone()),
                            _ => groups.push(Group { id, phase, parts: vec![part.text.clone()] }),
                        }
                    }
                    for g in groups {
                        let mut o = Object::new();
                        o.insert("type".into(), json!("message"));
                        if let Some(id) = g.id {
                            o.insert("id".into(), json!(id));
                        }
                        o.insert("role".into(), json!("assistant"));
                        o.insert("status".into(), json!("completed"));
                        o.insert("content".into(), Value::Array(g.parts.iter().map(|t| json!({"type": "output_text", "text": t})).collect()));
                        if let Some(p) = g.phase {
                            o.insert("phase".into(), p);
                        }
                        input.push(Value::Object(o));
                    }
                    text_parts.clear();
                };
                for part in &message.content {
                    match part {
                        ContentPart::Compaction(c) => {
                            flush(&mut text_parts, &mut input);
                            if c.provider != request.model.provider || c.encrypted.is_none() {
                                return Err(AIError::invalid_request("Compaction state must be replayed to its originating provider and API"));
                            }
                            let mut o = Object::new();
                            o.insert("type".into(), json!("compaction"));
                            if let Some(id) = &c.id {
                                o.insert("id".into(), json!(id));
                            }
                            o.insert("encrypted_content".into(), json!(c.encrypted.clone().unwrap()));
                            input.push(Value::Object(o));
                        }
                        ContentPart::Text(t) => text_parts.push(t),
                        ContentPart::Reasoning(r) => {
                            flush(&mut text_parts, &mut input);
                            let Some(meta) = r.provider_metadata.as_ref().and_then(|pm| pm.get(&key)).and_then(Value::as_object) else {
                                continue;
                            };
                            let id = item_id(r.provider_metadata.as_ref(), &key);
                            let encrypted = match meta.get("reasoningEncryptedContent") {
                                Some(Value::String(s)) => Some(Value::String(s.clone())),
                                Some(Value::Null) => Some(Value::Null),
                                _ => None,
                            };
                            let has_encrypted = encrypted.as_ref().and_then(Value::as_str).map(|s| !s.is_empty()).unwrap_or(false);
                            if r.text.is_empty() && !has_encrypted {
                                continue;
                            }
                            let summary: Vec<Value> =
                                if r.text.is_empty() { vec![] } else { vec![json!({"type": "summary_text", "text": r.text})] };
                            if let Some(existing) = id.as_ref().and_then(|i| reasoning_index.get(i)).copied() {
                                let item = input[existing].as_object_mut().unwrap();
                                item.get_mut("summary").unwrap().as_array_mut().unwrap().extend(summary);
                                if let Some(Value::String(s)) = &encrypted {
                                    item.insert("encrypted_content".into(), json!(s));
                                }
                                continue;
                            }
                            let mut o = Object::new();
                            o.insert("type".into(), json!("reasoning"));
                            if let Some(id) = &id {
                                o.insert("id".into(), json!(id));
                            }
                            o.insert("summary".into(), Value::Array(summary));
                            if let Some(e) = encrypted {
                                o.insert("encrypted_content".into(), e);
                            }
                            if let Some(id) = id {
                                reasoning_index.insert(id, input.len());
                            }
                            input.push(Value::Object(o));
                        }
                        ContentPart::ToolCall(c) => {
                            flush(&mut text_parts, &mut input);
                            if c.provider_executed == Some(true) {
                                continue;
                            }
                            let mut o = Object::new();
                            o.insert("type".into(), json!("function_call"));
                            if let Some(id) = item_id(c.provider_metadata.as_ref(), &key) {
                                o.insert("id".into(), json!(id));
                            }
                            o.insert("call_id".into(), json!(c.id));
                            o.insert("name".into(), json!(c.name));
                            if let Some(ns) = &c.namespace {
                                o.insert("namespace".into(), json!(ns));
                            }
                            let input_value = if c.input.is_null() { json!({}) } else { c.input.clone() };
                            o.insert("arguments".into(), json!(encode(&input_value)));
                            input.push(Value::Object(o));
                        }
                        ContentPart::ToolResult(r) if r.provider_executed == Some(true) => {
                            flush(&mut text_parts, &mut input);
                            let id = item_id(r.provider_metadata.as_ref(), &key);
                            let hosted = match &r.result {
                                ToolResultValue::Json(v)
                                    if is_builtin_hosted(v) || adapter.restore_hosted.map(|f| f(v)).unwrap_or(false) =>
                                {
                                    Some(v)
                                }
                                _ => None,
                            };
                            if let (Some(id), Some(h)) = (&id, hosted) {
                                if h.get("id").and_then(Value::as_str) == Some(id.as_str()) {
                                    if hosted_ids.insert(id.clone()) {
                                        input.push(h.clone());
                                    }
                                    continue;
                                }
                            }
                            let content: Vec<ToolContent> = match &r.result {
                                ToolResultValue::Content(items) => items.clone(),
                                other => vec![ToolContent::Text { text: tool_result_text(other) }],
                            };
                            let lowered: Result<Vec<Value>, AIError> =
                                content.iter().map(|i| lower_content_item(i, request, adapter, true)).collect();
                            input.push(json!({"type": "message", "role": "user", "content": lowered?}));
                        }
                        ContentPart::Media(m) => {
                            flush(&mut text_parts, &mut input);
                            input.push(json!({"type": "message", "role": "user", "content": [lower_message_media(m, request, adapter)?]}));
                        }
                        _ => {
                            return Err(unsupported_content(
                                adapter.name,
                                Role::Assistant,
                                &["text", "reasoning", "tool-call", "tool-result", "media"],
                            ));
                        }
                    }
                }
                flush(&mut text_parts, &mut input);
            }
            Role::Tool => {
                for part in &message.content {
                    let ContentPart::ToolResult(r) = part else {
                        return Err(unsupported_content(adapter.name, Role::Tool, &["tool-result"]));
                    };
                    let output = match &r.result {
                        ToolResultValue::Content(items) => Value::Array(
                            items.iter().map(|i| lower_content_item(i, request, adapter, false)).collect::<Result<Vec<_>, _>>()?,
                        ),
                        other => json!(tool_result_text(other)),
                    };
                    input.push(json!({"type": "function_call_output", "call_id": r.id, "output": output}));
                }
            }
        }
    }
    Ok(input)
}

/// `lowerConversation`: `model`, `input`, `instructions?`.
pub fn lower_conversation(request: &LLMRequest, adapter: &Adapter) -> Result<Fields, AIError> {
    let instructions = shared::join_text(request.system.iter().map(|p| p.text.as_str()));
    let mut f = Fields::new();
    f.insert("model".into(), Some(json!(request.model.id)));
    f.insert("input".into(), Some(Value::Array(lower_messages(request, adapter)?)));
    if !instructions.is_empty() {
        f.insert("instructions".into(), Some(json!(instructions)));
    }
    Ok(f)
}

pub fn resolve_parallel_tool_calls(request: &LLMRequest, options: &ResponsesOptions) -> Option<bool> {
    options.parallel_tool_calls.or_else(|| request.tool_choice.as_ref().and_then(|c| c.disable_parallel_tool_use).map(|d| !d))
}

/// `lowerGeneration`.
pub fn lower_generation(request: &LLMRequest, options: &ResponsesOptions, f: &mut Fields) {
    let generation = request.generation_options();
    let cache_key = prompt_cache_key(request);
    let parallel = resolve_parallel_tool_calls(request, &options::resolve(request));
    f.insert("stream".into(), Some(json!(true)));
    f.insert("max_output_tokens".into(), generation.max_tokens.map(num));
    f.insert("temperature".into(), generation.temperature.map(num));
    f.insert("top_p".into(), generation.top_p.map(num));
    f.insert("presence_penalty".into(), generation.presence_penalty.map(num));
    f.insert("frequency_penalty".into(), generation.frequency_penalty.map(num));
    if let Some(store) = options.store {
        f.insert("store".into(), Some(json!(store)));
    }
    if let Some(m) = &options.metadata {
        f.insert("metadata".into(), Some(Value::Object(m.clone())));
    }
    if let Some(s) = options.safety_identifier.as_ref().filter(|s| !s.is_empty()) {
        f.insert("safety_identifier".into(), Some(json!(s)));
    }
    if let Some(Some(o)) = options.include_obfuscation {
        f.insert("stream_options".into(), Some(json!({"include_obfuscation": o})));
    }
    if let Some(t) = options.top_logprobs {
        f.insert("top_logprobs".into(), Some(json!(t)));
    }
    if let Some(k) = cache_key.filter(|k| !k.is_empty()) {
        f.insert("prompt_cache_key".into(), Some(json!(k)));
    }
    if let Some(i) = &options.include {
        f.insert("include".into(), Some(Value::Array(i.clone())));
    }
    let effort = options.reasoning_effort.clone().filter(|e| !e.is_empty());
    let summary = options.reasoning_summary.clone().filter(|e| !e.is_empty());
    if effort.is_some() || summary.is_some() {
        let mut r = Object::new();
        if let Some(e) = effort {
            r.insert("effort".into(), json!(e));
        }
        if let Some(s) = summary {
            r.insert("summary".into(), json!(s));
        }
        f.insert("reasoning".into(), Some(Value::Object(r)));
    }
    if let Some(v) = options.text_verbosity.as_ref().filter(|s| !s.is_empty()) {
        f.insert("text".into(), Some(json!({"verbosity": v})));
    }
    if let Some(s) = options.service_tier.as_ref().filter(|s| !s.is_empty()) {
        f.insert("service_tier".into(), Some(json!(s)));
    }
    if let Some(m) = options.max_tool_calls {
        f.insert("max_tool_calls".into(), Some(json!(m)));
    }
    if let Some(p) = parallel {
        f.insert("parallel_tool_calls".into(), Some(json!(p)));
    }
    if let Some(t) = options.truncation.as_ref().filter(|s| !s.is_empty()) {
        f.insert("truncation".into(), Some(json!(t)));
    }
}

pub fn allowed_tool_choice(options: &ResponsesOptions) -> Option<Value> {
    let allowed = options.allowed_tools.as_ref()?;
    Some(json!({
        "type": "allowed_tools",
        "mode": allowed.mode,
        "tools": allowed.tool_names.iter().map(|n| json!({"type": "function", "name": n})).collect::<Vec<_>>(),
    }))
}

/// `fromRequestWithAdapter`.
pub fn body_fields(request: &LLMRequest, adapter: &Adapter) -> Result<Fields, AIError> {
    let (tools, projected) = flatten_tool_request(request);
    let mut f = lower_conversation(&projected, adapter)?;
    let options = options::resolve(request);
    lower_generation(request, &options, &mut f);
    f.insert("tools".into(), if tools.is_empty() { None } else { Some(Value::Array(lower_tools(&tools, adapter)?)) });
    let choice = match allowed_tool_choice(&options) {
        Some(c) => Some(c),
        None => request.tool_choice.as_ref().map(|c| lower_tool_choice(adapter.name, c)).transpose()?,
    };
    f.insert("tool_choice".into(), choice);
    Ok(f)
}

// ---------------------------------------------------------------------------------------------
// Protocol
// ---------------------------------------------------------------------------------------------

pub struct OpenResponses {
    pub adapter: Adapter,
}

impl Protocol for OpenResponses {
    fn id(&self) -> &str {
        self.adapter.id
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        Ok(finalize(body_fields(request, &self.adapter)?))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(ResponsesParser::new(request, &self.adapter))
    }
}

pub fn protocol() -> Arc<dyn Protocol> {
    Arc::new(OpenResponses { adapter: Adapter::base() })
}

pub fn protocol_with(adapter: Adapter) -> Arc<dyn Protocol> {
    Arc::new(OpenResponses { adapter })
}

fn default_responses_options() -> Object {
    let mut po = Object::new();
    po.insert("store".into(), json!(false));
    po.insert("include".into(), json!(["reasoning.encrypted_content"]));
    po
}

/// `openai-compatible-responses` route (no baseURL; metadata key `openresponses`, re-keyed on provider change).
pub fn compatible_route() -> Route {
    Route::new("openai-compatible-responses", protocol(), Endpoint::new(None, PATH), Framing::Sse)
        .metadata_key("openresponses")
        .with_provider_options(default_responses_options())
}

/// Default responses provider options `{store:false, include:["reasoning.encrypted_content"]}`.
pub fn default_options() -> Object {
    default_responses_options()
}

// ---------------------------------------------------------------------------------------------
// Stream parsing
// ---------------------------------------------------------------------------------------------

pub fn map_usage(usage: Option<&Value>, key: &str) -> Option<Usage> {
    let usage = usage.filter(|u| u.is_object())?;
    let u = |p: &[&str]| crate::json::path(usage, p).and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)));
    let input = u(&["input_tokens"]);
    let output = u(&["output_tokens"]);
    let cached = u(&["input_tokens_details", "cached_tokens"]);
    let write = u(&["input_tokens_details", "cache_write_tokens"]);
    let reasoning = u(&["output_tokens_details", "reasoning_tokens"]);
    let mut pm = Object::new();
    pm.insert(key.into(), usage.clone());
    Some(Usage {
        context_tokens: None,
        input_tokens: input,
        output_tokens: output,
        non_cached_input_tokens: shared::subtract_tokens(input, shared::sum_tokens(&[cached, write])),
        cache_read_input_tokens: cached,
        cache_write_input_tokens: write,
        reasoning_tokens: reasoning,
        total_tokens: shared::total_tokens(input, output, u(&["total_tokens"])),
        provider_metadata: Some(pm),
    })
}

const TERMINAL_TYPES: &[&str] = &["error", "response.completed", "response.incomplete", "response.failed"];

const STREAM_ITEM_KEYS: &[&str] = &["type", "id", "call_id", "name", "namespace", "arguments", "encrypted_content"];

/// Schema decode of a `StreamItem`: declared keys first, then the rest.
pub fn reorder_item(item: &Value) -> Value {
    let Some(o) = item.as_object() else { return item.clone() };
    let mut out = Object::new();
    for k in STREAM_ITEM_KEYS {
        if let Some(v) = o.get(*k) {
            out.insert((*k).into(), v.clone());
        }
    }
    for (k, v) in o {
        if !out.contains_key(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum PartStatus {
    Active,
    CanConclude,
    Concluded,
}

#[derive(Debug, Clone)]
struct ReasoningItem {
    encrypted: Option<Value>,
    parts: BTreeMap<i64, PartStatus>,
    delta_indexes: HashSet<i64>,
}

pub struct ResponsesParser {
    provider: String,
    label: String,
    id: String,
    name: String,
    key: String,
    adapter: Adapter,
    completed_compactions: HashSet<String>,
    tools: ToolStream,
    has_function_call: bool,
    lifecycle: Lifecycle,
    output_items: BTreeMap<i64, String>,
    message: Option<(String, Option<Value>)>,
    reasoning_items: IndexMap<String, ReasoningItem>,
    completed_items: IndexSet<String>,
}

fn mint(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

fn as_text(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(crate::json::js_number_string(n)),
        _ => None,
    }
}

/// `providerFailure(event, fallback)`.
pub fn provider_failure(event: &Value, fallback: &str) -> AIError {
    provider_failure_with_body(event, fallback, &encode(event))
}

/// `providerFailure(event, fallback, body)` with an explicit body (the WebSocket channel passes the frame).
pub fn provider_failure_with_body(event: &Value, fallback: &str, body: &str) -> AIError {
    let body = body.to_string();
    let raw = event.get("error").filter(|v| !v.is_null()).or_else(|| event.get("response").and_then(|r| r.get("error")));
    let nested: Option<Object> = match raw {
        Some(Value::String(s)) => {
            let mut o = Object::new();
            o.insert("message".into(), json!(s));
            Some(o)
        }
        Some(Value::Object(o)) => Some(o.clone()),
        _ => None,
    };
    let message = as_text(event.get("message")).or_else(|| as_text(nested.as_ref().and_then(|n| n.get("message"))));
    let code = as_text(event.get("code")).or_else(|| as_text(nested.as_ref().and_then(|n| n.get("code"))));
    let summary = match (&message, &code) {
        (Some(m), Some(c)) => Some(format!("{c}: {m}")),
        (Some(m), None) => Some(m.clone()),
        (None, Some(c)) => Some(c.clone()),
        _ => None,
    };
    let text = summary.clone().unwrap_or_else(|| if body == "{}" { fallback.to_string() } else { body.clone() });
    let status = event
        .get("status")
        .and_then(Value::as_f64)
        .or_else(|| event.get("status_code").and_then(Value::as_f64))
        .map(|s| s as u16);
    if event.get("type").and_then(Value::as_str) == Some("error")
        && event.get("error").is_none()
        && event.get("response").is_none()
        && summary.is_none()
        && status.is_none()
    {
        return AIError::provider_internal(text).with_body(body);
    }
    classify_provider_failure(ProviderFailure { message: text, status, raw_body: Some(body), ..Default::default() })
}

impl ResponsesParser {
    pub fn new(request: &LLMRequest, adapter: &Adapter) -> Self {
        Self {
            provider: request.model.provider.clone(),
            label: route_label(request),
            id: adapter.id.into(),
            name: adapter.name.into(),
            key: metadata_key(request),
            adapter: adapter.clone(),
            completed_compactions: HashSet::new(),
            tools: ToolStream::new(adapter.id),
            has_function_call: false,
            lifecycle: Lifecycle::new(),
            output_items: BTreeMap::new(),
            message: None,
            reasoning_items: IndexMap::new(),
            completed_items: IndexSet::new(),
        }
    }

    fn pm(&self, value: Value) -> ProviderMetadata {
        let mut o = Object::new();
        o.insert(self.key.clone(), value);
        o
    }

    fn err(&self, message: impl Into<String>) -> AIError {
        AIError::invalid_output(&self.id, message)
    }

    fn resolve_item(&self, item: &Value, index: Option<i64>) -> Value {
        let mut item = reorder_item(item);
        let has_id = item.get("id").and_then(Value::as_str).is_some();
        if !has_id {
            let id = index.and_then(|i| self.output_items.get(&i).cloned()).unwrap_or_else(|| {
                let prefix = match item.get("type").and_then(Value::as_str) {
                    Some("message") => "msg",
                    Some("reasoning") => "rs",
                    Some("function_call") => "fc",
                    Some("compaction") => "cmp",
                    _ => "item",
                };
                mint(prefix)
            });
            item.as_object_mut().unwrap().insert("id".into(), json!(id));
        }
        item
    }

    /// `normalize`: remap `item_id` via `output_index`, resolve item ids.
    fn normalize(&self, event: &Value) -> Value {
        let mut out = event.clone();
        let output_index = event.get("output_index").and_then(Value::as_i64);
        if let Some(obj) = out.as_object_mut() {
            if event.get("item_id").is_some() {
                let mapped = output_index.and_then(|i| self.output_items.get(&i).cloned());
                if let Some(m) = mapped {
                    obj.insert("item_id".into(), json!(m));
                }
            }
            if let Some(item) = event.get("item").filter(|i| i.is_object()) {
                obj.insert("item".into(), self.resolve_item(item, output_index));
            }
        }
        out
    }

    fn text_meta(&self, id: &str, phase: &Option<Value>) -> ProviderMetadata {
        let mut o = Object::new();
        o.insert("itemId".into(), json!(id));
        if let Some(p) = phase {
            o.insert("phase".into(), p.clone());
        }
        self.pm(Value::Object(o))
    }

    fn on_output_text_delta(&mut self, delta: Option<&str>, id: &str, events: &mut Vec<LLMEvent>) {
        let Some(delta) = delta.filter(|d| !d.is_empty()) else { return };
        let Some((mid, phase)) = self.message.clone() else { return };
        if mid != id {
            return;
        }
        let meta = self.text_meta(id, &phase);
        self.lifecycle.text_start(events, id, Some(meta));
        self.lifecycle.text_delta(events, id, delta, None);
    }

    fn on_output_text_done(&mut self, text: Option<&str>, id: &str, events: &mut Vec<LLMEvent>) {
        if self.message.as_ref().map(|m| m.0 == id).unwrap_or(false) {
            if self.lifecycle.is_text_open(id) || text.is_none() {
                return;
            }
            self.on_output_text_delta(text, id, events);
            return;
        }
        self.lifecycle.text_end(events, id, None, None);
    }

    fn reasoning_meta(&self, id: &str, encrypted: Option<&Value>) -> ProviderMetadata {
        self.pm(json!({"itemId": id, "reasoningEncryptedContent": encrypted.cloned().unwrap_or(Value::Null)}))
    }

    fn start_summary_part(&mut self, item_id: &str, index: i64, events: &mut Vec<LLMEvent>) {
        let Some(item) = self.reasoning_items.get(item_id).cloned() else { return };
        if index == 0 || item.parts.contains_key(&index) {
            return;
        }
        for (i, status) in &item.parts {
            if *status != PartStatus::Concluded {
                let meta = self.pm(json!({"itemId": item_id}));
                self.lifecycle.reasoning_end(events, &format!("{item_id}:{i}"), Some(meta), None);
            }
        }
        let meta = self.reasoning_meta(item_id, item.encrypted.as_ref());
        self.lifecycle.reasoning_start(events, &format!("{item_id}:{index}"), Some(meta));
        let entry = self.reasoning_items.get_mut(item_id).unwrap();
        for status in entry.parts.values_mut() {
            *status = PartStatus::Concluded;
        }
        entry.parts.insert(index, PartStatus::Active);
    }

    fn on_reasoning_delta(&mut self, event: &Value, item_id: &str, delta: Option<&str>, events: &mut Vec<LLMEvent>) {
        let Some(delta) = delta.filter(|d| !d.is_empty()) else { return };
        let Some(item) = self.reasoning_items.get(item_id) else { return };
        let index = event.get("summary_index").and_then(Value::as_i64).unwrap_or(0);
        if item.parts.get(&index) == Some(&PartStatus::Concluded) {
            return;
        }
        self.start_summary_part(item_id, index, events);
        if !self.reasoning_items.contains_key(item_id) {
            return;
        }
        self.lifecycle.reasoning_delta(events, &format!("{item_id}:{index}"), delta, None);
        self.reasoning_items.get_mut(item_id).unwrap().delta_indexes.insert(index);
    }

    fn on_reasoning_done(&mut self, event: &Value, item_id: &str, events: &mut Vec<LLMEvent>) {
        let Some(item) = self.reasoning_items.get(item_id) else { return };
        let Some(text) = event.get("text").and_then(Value::as_str) else { return };
        let index = event.get("summary_index").and_then(Value::as_i64).unwrap_or(0);
        if item.delta_indexes.contains(&index) {
            return;
        }
        self.on_reasoning_delta(event, item_id, Some(text), events);
    }

    fn on_output_item_added(&mut self, item: &Value, events: &mut Vec<LLMEvent>) {
        let id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let phase = message_phase(item.get("phase"));
                let open: Vec<String> = self.lifecycle.text.iter().filter(|t| **t != id).cloned().collect();
                for t in open {
                    let open_phase = self.message.as_ref().filter(|m| m.0 == t).and_then(|m| m.1.clone());
                    let meta = self.text_meta(&t, &open_phase);
                    self.lifecycle.text_end(events, &t, Some(meta), None);
                }
                let phase = if phase.is_none() && self.message.as_ref().map(|m| m.0 == id).unwrap_or(false) {
                    self.message.as_ref().unwrap().1.clone()
                } else {
                    phase
                };
                self.message = Some((id, phase));
            }
            Some("reasoning") => {
                if self.reasoning_items.contains_key(&id) {
                    return;
                }
                let encrypted = item.get("encrypted_content").cloned();
                let meta = self.reasoning_meta(&id, encrypted.as_ref());
                self.lifecycle.reasoning_start(events, &format!("{id}:0"), Some(meta));
                let mut parts = BTreeMap::new();
                parts.insert(0, PartStatus::Active);
                self.reasoning_items.insert(id, ReasoningItem { encrypted, parts, delta_indexes: HashSet::new() });
            }
            Some("function_call") => {
                let Some(call_id) = item.get("call_id").and_then(Value::as_str).filter(|s| !s.is_empty()) else { return };
                let key = StreamKey::Id(id.clone());
                if self.tools.contains(&key) {
                    return;
                }
                let meta = self.pm(json!({"itemId": id}));
                self.lifecycle.step_start(events);
                let tool = PendingTool {
                    id: call_id.into(),
                    name: item.get("name").and_then(Value::as_str).unwrap_or("").into(),
                    namespace: item.get("namespace").and_then(Value::as_str).map(String::from),
                    input: item.get("arguments").and_then(Value::as_str).unwrap_or("").into(),
                    provider_executed: false,
                    provider_metadata: Some(meta.clone()),
                };
                events.push(LLMEvent::ToolInputStart {
                    id: tool.id.clone(),
                    name: tool.name.clone(),
                    namespace: tool.namespace.clone(),
                    provider_executed: None,
                    provider_metadata: Some(meta),
                });
                self.tools.start(key, tool);
            }
            _ => {}
        }
    }

    fn on_summary_part_done(&mut self, item_id: &str, index: i64) {
        if let Some(item) = self.reasoning_items.get_mut(item_id) {
            if item.parts.get(&index) == Some(&PartStatus::Active) {
                item.parts.insert(index, PartStatus::CanConclude);
            }
        }
    }

    fn on_arguments(&mut self, event: &Value, item_id: &str, events: &mut Vec<LLMEvent>) -> Result<(), AIError> {
        let key = StreamKey::Id(item_id.into());
        let Some(tool) = self.tools.get(&key).cloned() else { return Ok(()) };
        let is_done = event.get("type").and_then(Value::as_str) == Some("response.function_call_arguments.done");
        let final_args = if is_done { event.get("arguments").and_then(Value::as_str) } else { None };
        if is_done && final_args.is_none() {
            return Ok(());
        }
        if let Some(f) = final_args {
            if !f.starts_with(&tool.input) {
                let mut t = tool.clone();
                t.input = f.into();
                self.tools.start(key, t);
                return Ok(());
            }
        }
        let delta = match final_args {
            None => event.get("delta").and_then(Value::as_str).unwrap_or("").to_string(),
            Some(f) => f[tool.input.len()..].to_string(),
        };
        if delta.is_empty() {
            return Ok(());
        }
        let mut out = vec![];
        let name = self.name.clone();
        self.tools.append_existing(key, &delta, &format!("{name} tool argument delta is missing its tool call"), &mut out)?;
        if !out.is_empty() {
            self.lifecycle.step_start(events);
        }
        events.extend(out);
        Ok(())
    }

    fn hosted_done(&mut self, item: &Value, events: &mut Vec<LLMEvent>) -> Result<bool, AIError> {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        let Some(tool) = self.adapter.hosted_tools.iter().find(|t| t.item_type == item_type).cloned() else { return Ok(false) };
        let id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let meta = self.pm(json!({"itemId": id}));
        self.lifecycle.step_start(events);
        let result = match tool.result {
            Some(f) => f(&self.id, item)?,
            None => match item.get("error") {
                Some(e) if !e.is_null() => ToolResultValue::Error(e.clone()),
                _ => ToolResultValue::Json(item.clone()),
            },
        };
        events.push(LLMEvent::ToolCall {
            id: id.clone(),
            name: tool.name.into(),
            namespace: None,
            input: (tool.input)(item),
            provider_executed: Some(true),
            provider_metadata: Some(meta.clone()),
        });
        events.push(LLMEvent::ToolResult {
            id,
            name: tool.name.into(),
            namespace: None,
            result,
            output: None,
            provider_executed: Some(true),
            provider_metadata: Some(meta),
        });
        Ok(true)
    }

    fn on_output_item_done(&mut self, item: &Value, events: &mut Vec<LLMEvent>) -> Result<(), AIError> {
        let id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        match item.get("type").and_then(Value::as_str) {
            Some("compaction") => {
                let Some(enc) = item.get("encrypted_content").and_then(Value::as_str) else {
                    return Err(self.err("Compaction output is missing its encrypted content"));
                };
                if self.completed_compactions.contains(&id) {
                    return Ok(());
                }
                self.lifecycle.step_start(events);
                events.push(LLMEvent::Compaction(CompactionPart {
                    provider: self.provider.clone(),
                    id: Some(id.clone()),
                    encrypted: Some(enc.into()),
                    text: None,
                }));
                self.completed_compactions.insert(id);
            }
            Some("message") => {
                let active = self.message.as_ref().map(|m| m.0 == id).unwrap_or(false);
                let item_phase = message_phase(item.get("phase"));
                let phase = if item_phase.is_none() && active { self.message.as_ref().unwrap().1.clone() } else { item_phase };
                let mut content: Vec<String> = vec![];
                for part in item.get("content").and_then(Value::as_array).cloned().unwrap_or_default() {
                    match part.get("type").and_then(Value::as_str) {
                        Some("output_text") => {
                            if let Some(t) = part.get("text").and_then(Value::as_str) {
                                content.push(t.into());
                            }
                        }
                        Some("refusal") => {
                            if let Some(t) = part.get("refusal").and_then(Value::as_str) {
                                content.push(t.into());
                            }
                        }
                        _ => {}
                    }
                }
                let text = if content.is_empty() { None } else { Some(content.concat()) };
                let meta = self.text_meta(&id, &phase);
                if text.as_ref().map(|t| !t.is_empty()).unwrap_or(false) {
                    self.lifecycle.text_start(events, &id, Some(meta.clone()));
                }
                self.lifecycle.text_end(events, &id, Some(meta), text);
                if active {
                    self.message = None;
                }
            }
            Some("function_call") => {
                let (Some(call_id), Some(name)) = (
                    item.get("call_id").and_then(Value::as_str).filter(|s| !s.is_empty()),
                    item.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()),
                ) else {
                    return Ok(());
                };
                let meta = self.pm(json!({"itemId": id}));
                let key = StreamKey::Id(id.clone());
                let registered = self.tools.contains(&key);
                if !registered {
                    self.tools.start(
                        key.clone(),
                        PendingTool {
                            id: call_id.into(),
                            name: name.into(),
                            namespace: item.get("namespace").and_then(Value::as_str).map(String::from),
                            input: String::new(),
                            provider_executed: false,
                            provider_metadata: Some(meta.clone()),
                        },
                    );
                }
                let mut finished = vec![];
                match item.get("arguments").and_then(Value::as_str) {
                    None => self.tools.finish(key, &mut finished)?,
                    Some(args) => self.tools.finish_with_input(key, args, &mut finished)?,
                };
                let mut result_events = vec![];
                if !registered && !finished.is_empty() {
                    result_events.push(LLMEvent::ToolInputStart {
                        id: call_id.into(),
                        name: name.into(),
                        namespace: item.get("namespace").and_then(Value::as_str).map(String::from),
                        provider_executed: None,
                        provider_metadata: Some(meta),
                    });
                }
                result_events.extend(finished);
                if !result_events.is_empty() {
                    self.lifecycle.step_start(events);
                }
                if result_events.iter().any(|e| matches!(e, LLMEvent::ToolCall { .. } | LLMEvent::ToolInputError { .. })) {
                    self.has_function_call = true;
                }
                events.extend(result_events);
            }
            Some("reasoning") => {
                let encrypted = item.get("encrypted_content").cloned();
                let meta = self.reasoning_meta(&id, encrypted.as_ref());
                let summary: Vec<Option<String>> = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|p| {
                                if p.get("type").and_then(Value::as_str) == Some("summary_text") {
                                    p.get("text").and_then(Value::as_str).map(String::from)
                                } else {
                                    None
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let content: Vec<Option<String>> = item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter(|p| p.get("type").and_then(Value::as_str) == Some("reasoning_text"))
                            .filter_map(|p| p.get("text").and_then(Value::as_str).map(|s| Some(s.to_string())))
                            .collect()
                    })
                    .unwrap_or_default();
                let join = |parts: &[Option<String>]| -> Option<String> {
                    if !parts.iter().any(|p| p.as_ref().map(|s| !s.is_empty()).unwrap_or(false)) {
                        return None;
                    }
                    Some(parts.iter().flatten().cloned().collect::<Vec<_>>().join("\n\n"))
                };
                let item_text = join(&summary).or_else(|| join(&content));
                if let Some(reasoning_item) = self.reasoning_items.shift_remove(&id) {
                    let count = reasoning_item.parts.len();
                    for (index, status) in &reasoning_item.parts {
                        if *status == PartStatus::Concluded {
                            continue;
                        }
                        let final_text = if count == 1 { item_text.clone() } else { summary.get(*index as usize).cloned().flatten() };
                        self.lifecycle.reasoning_end(
                            events,
                            &format!("{id}:{index}"),
                            Some(meta.clone()),
                            final_text.filter(|t| !t.is_empty()),
                        );
                    }
                    return Ok(());
                }
                self.lifecycle.step_start(events);
                events.push(LLMEvent::ReasoningStart { id: id.clone(), provider_metadata: Some(meta.clone()) });
                events.push(LLMEvent::ReasoningEnd { id, text: item_text, provider_metadata: Some(meta) });
            }
            _ => {}
        }
        Ok(())
    }

    fn map_finish(&self, event: &Value) -> FinishReason {
        let reason = event.get("response").and_then(|r| r.get("incomplete_details")).and_then(|d| d.get("reason")).and_then(Value::as_str);
        match reason {
            None => {
                if self.has_function_call {
                    FinishReason::ToolCalls
                } else if event.get("type").and_then(Value::as_str) == Some("response.incomplete") {
                    FinishReason::Unknown
                } else {
                    FinishReason::Stop
                }
            }
            Some("max_output_tokens") => FinishReason::Length,
            Some("content_filter") => FinishReason::ContentFilter,
            Some(_) => {
                if self.has_function_call {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Unknown
                }
            }
        }
    }

    fn on_response_finish(&mut self, event: &Value, events: &mut Vec<LLMEvent>) -> Result<(), AIError> {
        if event.get("type").and_then(Value::as_str) == Some("response.completed") {
            let output = event.get("response").and_then(|r| r.get("output")).and_then(Value::as_array).cloned().unwrap_or_default();
            for (index, raw) in output.iter().enumerate() {
                let item = self.resolve_item(raw, Some(index as i64));
                let id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
                if kind == "compaction" && self.lifecycle.step_started && !self.completed_compactions.contains(&id) {
                    return Err(self.err("Cannot recover a compaction checkpoint after output has been emitted"));
                }
                let recoverable = kind == "compaction" || (kind == "function_call" && self.tools.contains(&StreamKey::Id(id)));
                if recoverable {
                    self.on_output_item_done(&item, events)?;
                }
            }
            let mut pending = vec![];
            self.tools.finish_all(&mut pending)?;
            if pending.iter().any(|e| matches!(e, LLMEvent::ToolCall { .. } | LLMEvent::ToolInputError { .. })) {
                self.has_function_call = true;
            }
            events.extend(pending);
        }
        let response = event.get("response");
        let raw = response.and_then(|r| r.get("incomplete_details")).and_then(|d| d.get("reason")).and_then(Value::as_str).map(String::from);
        let usage = map_usage(response.and_then(|r| r.get("usage")), &self.key);
        let rid = response.and_then(|r| r.get("id")).and_then(Value::as_str).filter(|s| !s.is_empty());
        let tier = response.and_then(|r| r.get("service_tier")).and_then(Value::as_str).filter(|s| !s.is_empty());
        let metadata = if rid.is_some() || tier.is_some() {
            let mut o = Object::new();
            if let Some(r) = response.and_then(|r| r.get("id")) {
                o.insert("responseId".into(), r.clone());
            }
            if let Some(t) = response.and_then(|r| r.get("service_tier")) {
                o.insert("serviceTier".into(), t.clone());
            }
            Some(self.pm(Value::Object(o)))
        } else {
            None
        };
        let reason = FinishReasonDetails::raw(self.map_finish(event), raw);
        self.lifecycle.finish(events, reason, usage, metadata);
        Ok(())
    }

    /// Base step over a normalized event.
    fn base_step(&mut self, event: &Value, events: &mut Vec<LLMEvent>) -> Result<(), AIError> {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        let item_id = event.get("item_id").and_then(Value::as_str).map(String::from);
        let missing = |k: &str| AIError::invalid_output(&self.id, format!("{k} is missing item_id"));
        match kind.as_str() {
            "response.output_text.delta" | "response.output_text.done" => {
                let id = item_id.ok_or_else(|| missing(&kind))?;
                if kind.ends_with("delta") {
                    self.on_output_text_delta(event.get("delta").and_then(Value::as_str), &id, events);
                } else {
                    self.on_output_text_done(event.get("text").and_then(Value::as_str), &id, events);
                }
            }
            "response.refusal.delta" | "response.refusal.done" => {
                let value = if kind.ends_with("delta") { event.get("delta") } else { event.get("refusal") };
                let (Some(id), Some(value)) = (item_id, value.and_then(Value::as_str)) else {
                    return Err(self.err(format!("{kind} is malformed")));
                };
                if kind.ends_with("delta") {
                    self.on_output_text_delta(Some(value), &id, events);
                } else {
                    self.on_output_text_done(Some(value), &id, events);
                }
            }
            "response.reasoning.delta" | "response.reasoning_summary_text.delta" => {
                let id = item_id.ok_or_else(|| missing(&kind))?;
                self.on_reasoning_delta(event, &id, event.get("delta").and_then(Value::as_str), events);
            }
            "response.reasoning_text.delta" if self.adapter.reasoning_text_delta => {
                let id = item_id.ok_or_else(|| missing(&kind))?;
                self.on_reasoning_delta(event, &id, event.get("delta").and_then(Value::as_str), events);
            }
            "response.reasoning.done" | "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                let id = item_id.ok_or_else(|| missing(&kind))?;
                self.on_reasoning_done(event, &id, events);
            }
            "response.reasoning_summary_part.added" => {
                let id = item_id.ok_or_else(|| missing(&kind))?;
                if let Some(index) = event.get("summary_index").and_then(Value::as_i64) {
                    self.start_summary_part(&id, index, events);
                }
            }
            "response.reasoning_summary_part.done" => {
                let id = item_id.ok_or_else(|| missing(&kind))?;
                if let Some(index) = event.get("summary_index").and_then(Value::as_i64) {
                    self.on_summary_part_done(&id, index);
                }
            }
            "response.output_item.added" => {
                let item = event.get("item").filter(|i| i.is_object());
                if let Some(item) = item {
                    let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                    if item.get("type").and_then(Value::as_str) == Some("reasoning")
                        && !self.reasoning_items.contains_key(id)
                        && !self.lifecycle.reasoning.is_empty()
                    {
                        return Err(self.err(format!("{kind} started reasoning before the previous item ended")));
                    }
                    if let Some(index) = event.get("output_index").and_then(Value::as_i64) {
                        self.output_items.insert(index, id.to_string());
                    }
                    let item = item.clone();
                    self.on_output_item_added(&item, events);
                }
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                let id = item_id.ok_or_else(|| missing(&kind))?;
                self.on_arguments(event, &id, events)?;
            }
            "response.output_item.done" => {
                if let Some(item) = event.get("item").filter(|i| i.is_object()).cloned() {
                    self.on_output_item_done(&item, events)?;
                }
            }
            "response.completed" | "response.incomplete" => self.on_response_finish(event, events)?,
            "response.failed" => return Err(provider_failure(event, &format!("{} response failed", self.name))),
            "error" => return Err(provider_failure(event, &format!("{} stream error", self.name))),
            _ => {}
        }
        Ok(())
    }

    /// Adapter step: hosted tools, then base handling; Meta extras.
    fn on_event(&mut self, event: &Value, events: &mut Vec<LLMEvent>) -> Result<(), AIError> {
        let is_done = event.get("type").and_then(Value::as_str) == Some("response.output_item.done");
        if is_done {
            if let Some(item) = event.get("item").filter(|i| i.is_object()) {
                let item = item.clone();
                if self.hosted_done(&item, events)? {
                    return Ok(());
                }
            }
        }
        let start = events.len();
        self.base_step(event, events)?;
        if self.adapter.meta && is_done {
            if let Some(item) = event.get("item").filter(|i| i.get("type").and_then(Value::as_str) == Some("message")) {
                let content = item.get("content").and_then(Value::as_array);
                let Some(content) = content else {
                    return Err(self.err("Meta returned invalid message annotations").with_body(encode(event)));
                };
                let annotations: Vec<Value> =
                    content.iter().flat_map(|p| p.get("annotations").and_then(Value::as_array).cloned().unwrap_or_default()).collect();
                if !annotations.is_empty() {
                    for e in &mut events[start..] {
                        if let LLMEvent::TextEnd { provider_metadata, .. } = e {
                            let mut pm = provider_metadata.clone().unwrap_or_default();
                            let mut inner = pm.get(&self.key).and_then(Value::as_object).cloned().unwrap_or_default();
                            inner.insert("annotations".into(), Value::Array(annotations.clone()));
                            pm.insert(self.key.clone(), Value::Object(inner));
                            *provider_metadata = Some(pm);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn handle(&mut self, raw: &Value) -> Result<Vec<LLMEvent>, AIError> {
        let mut events = vec![];
        let event = self.normalize(raw);
        if self.adapter.meta {
            let is_done = event.get("type").and_then(Value::as_str) == Some("response.output_item.done");
            if is_done {
                if let Some(id) = event.get("item").and_then(|i| i.get("id")).and_then(Value::as_str) {
                    if self.completed_items.contains(id) {
                        return Ok(events);
                    }
                }
            }
            if event.get("type").and_then(Value::as_str) == Some("response.completed") {
                let output = event.get("response").and_then(|r| r.get("output")).and_then(Value::as_array).cloned().unwrap_or_default();
                let original_items = self.output_items.clone();
                for (index, item) in output.iter().enumerate() {
                    let done_raw = json!({"type": "response.output_item.done", "item": item, "output_index": index});
                    let done = self.normalize(&done_raw);
                    let Some(id) = done.get("item").and_then(|i| i.get("id")).and_then(Value::as_str).map(String::from) else {
                        continue;
                    };
                    if self.completed_items.contains(&id)
                        || original_items.get(&(index as i64)).map(|o| self.completed_items.contains(o)).unwrap_or(false)
                    {
                        continue;
                    }
                    self.on_event(&done, &mut events)?;
                    self.completed_items.insert(id);
                }
            }
            self.on_event(&event, &mut events)?;
            if is_done {
                if let Some(id) = event.get("item").and_then(|i| i.get("id")).and_then(Value::as_str) {
                    self.completed_items.insert(id.into());
                }
            }
            return Ok(events);
        }
        self.on_event(&event, &mut events)?;
        Ok(events)
    }
}

impl StreamParser for ResponsesParser {
    fn step(&mut self, frame: &Frame) -> Result<Step, AIError> {
        let event = decode_event(decode_json_frame(&self.id, &self.label, frame)?);
        if !event.get("type").map(Value::is_string).unwrap_or(false) {
            return Err(AIError::invalid_output(&self.id, format!("Invalid {} stream event", self.label)).with_body(encode(&event)));
        }
        let terminal = TERMINAL_TYPES.contains(&event.get("type").and_then(Value::as_str).unwrap_or(""));
        let events = self.handle(&event)?;
        Ok(Step { events, terminal })
    }
}
