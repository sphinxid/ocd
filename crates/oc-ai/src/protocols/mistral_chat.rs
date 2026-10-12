//! Mistral Chat protocol (`protocols/mistral-chat.ts`, `utils/mistral-tool-id.ts`).

use super::lifecycle::Lifecycle;
use super::shared::{
    self, flatten_tool_request, is_valid_mistral_id, match_tool_choice, mistral_hash, tool_file_media, tool_result_text,
    unsupported_content, wrapped_system_update, MistralIdNormalizer, ToolChoiceMode,
};
use super::tool_stream::{StreamKey, ToolStream};
use super::{decode_json_frame, route_label, Protocol, Step, StreamParser};
use crate::error::{classify_provider_failure, AIError, ProviderFailure};
use crate::json::{encode, num, Object};
use crate::route::framing::{Frame, Framing};
use crate::route::{Endpoint, Route};
use crate::types::*;
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub const ADAPTER: &str = "mistral-chat";
const NAME: &str = "Mistral Chat";
pub const DEFAULT_BASE_URL: &str = "https://api.mistral.ai/v1";
pub const PATH: &str = "/chat/completions";

fn is_thinking_content(value: Option<&Value>) -> bool {
    let Some(o) = value.and_then(Value::as_object) else { return false };
    o.get("type").and_then(Value::as_str) == Some("thinking")
        && o.get("thinking")
            .and_then(Value::as_array)
            .map(|units| {
                units.iter().all(|u| {
                    u.is_object()
                        && u.get("type").map(Value::is_string).unwrap_or(true)
                        && u.get("text").map(Value::is_string).unwrap_or(true)
                })
            })
            .unwrap_or(false)
}

fn native_thinking(part: &ReasoningPart) -> Option<&Value> {
    part.provider_metadata.as_ref().and_then(|pm| pm.get("mistral")).and_then(|m| m.get("thinking")).filter(|t| is_thinking_content(Some(t)))
}

fn lower_media(part: &MediaPart) -> Result<Value, AIError> {
    let media_type = part.media.media_type();
    let mime = media_type.to_lowercase();
    let url = match part.media.url_value() {
        Some(u) => u.to_string(),
        None => shared::require_inline_media(NAME, &part.media)?.data_url,
    };
    if mime.starts_with("image/") {
        return Ok(json!({"type": "image_url", "image_url": url}));
    }
    if mime == "application/pdf" {
        return Ok(json!({"type": "document_url", "document_url": url}));
    }
    Err(AIError::invalid_request(format!("Mistral Chat does not support media type {media_type}")))
}

fn lower_messages(request: &LLMRequest) -> Result<Vec<Value>, AIError> {
    let mut normalizer = MistralIdNormalizer::new(request);
    let mut messages = vec![];
    if !request.system.is_empty() {
        let content = if request.system.len() == 1 {
            json!(request.system[0].text)
        } else {
            Value::Array(request.system.iter().map(|p| json!({"type": "text", "text": p.text})).collect())
        };
        messages.push(json!({"role": "system", "content": content}));
    }
    let last_index = request.messages.len().saturating_sub(1);
    for (index, message) in request.messages.iter().enumerate() {
        match message.role {
            Role::System => {
                let update = wrapped_system_update(NAME, message)?;
                messages.push(json!({"role": "user", "content": update.text}));
            }
            Role::User => {
                let mut content = vec![];
                for part in &message.content {
                    match part {
                        ContentPart::Text(t) => content.push(json!({"type": "text", "text": t.text})),
                        ContentPart::Media(m) => content.push(lower_media(m)?),
                        _ => return Err(unsupported_content(NAME, Role::User, &["text", "media"])),
                    }
                }
                if content.iter().all(|c| c.get("type").and_then(Value::as_str) == Some("text")) {
                    let joined: String = content.iter().map(|c| c["text"].as_str().unwrap_or("").to_string()).collect();
                    messages.push(json!({"role": "user", "content": joined}));
                } else {
                    messages.push(json!({"role": "user", "content": content}));
                }
            }
            Role::Assistant => {
                let has_tool_calls = message.content.iter().any(|p| matches!(p, ContentPart::ToolCall(_)));
                let has_native = message.content.iter().any(|p| matches!(p, ContentPart::Reasoning(r) if native_thinking(r).is_some()));
                let text: String = message
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text(t) => Some(t.text.as_str()),
                        ContentPart::Reasoning(r) => Some(r.text.as_str()),
                        _ => None,
                    })
                    .collect();
                if !has_tool_calls && !has_native && text.trim().is_empty() {
                    continue;
                }
                let prefix = !has_tool_calls && index == last_index;
                let structured = has_native;
                let mut content = vec![];
                let mut texts: Vec<&str> = vec![];
                let mut tool_calls = vec![];
                for part in &message.content {
                    match part {
                        ContentPart::Text(t) => {
                            if structured {
                                content.push(json!({"type": "text", "text": t.text}));
                            } else {
                                texts.push(&t.text);
                            }
                        }
                        ContentPart::Reasoning(r) => match (structured, native_thinking(r)) {
                            (true, Some(native)) => content.push(native.clone()),
                            (true, None) => content.push(json!({"type": "text", "text": r.text})),
                            (false, _) => texts.push(&r.text),
                        },
                        ContentPart::ToolCall(c) => {
                            tool_calls.push(json!({"id": normalizer.normalize(&c.id), "type": "function", "function": {"name": c.name, "arguments": encode(&c.input)}}));
                        }
                        _ => return Err(unsupported_content(NAME, Role::Assistant, &["text", "reasoning", "tool-call"])),
                    }
                }
                let mut o = Object::new();
                o.insert("role".into(), json!("assistant"));
                o.insert("content".into(), if structured { Value::Array(content) } else { json!(texts.concat()) });
                if !tool_calls.is_empty() {
                    o.insert("tool_calls".into(), Value::Array(tool_calls));
                }
                if prefix {
                    o.insert("prefix".into(), json!(true));
                }
                messages.push(Value::Object(o));
            }
            Role::Tool => {
                for part in &message.content {
                    let ContentPart::ToolResult(r) = part else {
                        return Err(unsupported_content(NAME, Role::Tool, &["tool-result"]));
                    };
                    let content = match &r.result {
                        ToolResultValue::Content(items) => {
                            let mut content = vec![];
                            for item in items {
                                match item {
                                    ToolContent::Text { text } => content.push(json!({"type": "text", "text": text})),
                                    ToolContent::File { uri, mime, name } => content.push(lower_media(&tool_file_media(uri, mime, name.as_deref()))?),
                                }
                            }
                            if content.iter().any(|c| c.get("type").and_then(Value::as_str) != Some("text")) {
                                Value::Array(content)
                            } else {
                                json!(content.iter().map(|c| c["text"].as_str().unwrap_or("").to_string()).collect::<String>())
                            }
                        }
                        other => json!(tool_result_text(other)),
                    };
                    messages.push(json!({"role": "tool", "tool_call_id": normalizer.normalize(&r.id), "name": r.name, "content": content}));
                }
            }
        }
    }
    Ok(messages)
}

fn opt(request: &LLMRequest, key: &str, check: fn(&Value) -> bool) -> Result<Option<Value>, AIError> {
    match request.po(key) {
        None => Ok(None),
        Some(v) if check(v) => Ok(Some(v.clone())),
        Some(_) => Err(AIError::invalid_request(format!("Invalid Mistral Chat provider option `{key}`"))),
    }
}

/// Ordered Mistral body fields.
pub fn body_fields(request: &LLMRequest) -> Result<IndexMap<String, Option<Value>>, AIError> {
    let safe_prompt = opt(request, "safePrompt", Value::is_boolean)?;
    let image_limit = opt(request, "documentImageLimit", Value::is_number)?;
    let page_limit = opt(request, "documentPageLimit", Value::is_number)?;
    let parallel = opt(request, "parallelToolCalls", Value::is_boolean)?;
    let effort = opt(request, "reasoningEffort", Value::is_string)?;
    let prompt_mode = opt(request, "promptMode", |v| v.as_str() == Some("reasoning"))?;
    let cache_key = opt(request, "promptCacheKey", Value::is_string)?;
    if let Some(c) = &request.tool_choice {
        if c.kind == ToolChoiceType::Tool && c.name.as_deref().map(|n| n.is_empty()).unwrap_or(true) {
            return Err(AIError::invalid_request("Mistral Chat tool choice requires a tool name"));
        }
    }
    if effort.is_some() && prompt_mode.is_some() {
        return Err(AIError::invalid_request("Mistral Chat reasoningEffort and promptMode provider options are mutually exclusive"));
    }
    let tool_choice = match &request.tool_choice {
        None => None,
        Some(c) => Some(match match_tool_choice(NAME, c)? {
            ToolChoiceMode::Auto => json!("auto"),
            ToolChoiceMode::None => json!("none"),
            ToolChoiceMode::Required => json!("any"),
            ToolChoiceMode::Tool(name) => json!({"type": "function", "function": {"name": name}}),
        }),
    };
    let (tools, flattened) = flatten_tool_request(request);
    let g = request.generation_options();
    let mut f = IndexMap::new();
    f.insert("model".into(), Some(json!(request.model.id)));
    f.insert("messages".into(), Some(Value::Array(lower_messages(&flattened)?)));
    f.insert(
        "tools".into(),
        if tools.is_empty() {
            None
        } else {
            Some(Value::Array(
                tools
                    .iter()
                    .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": Value::Object(t.input_schema.clone()), "strict": false}}))
                    .collect(),
            ))
        },
    );
    f.insert("tool_choice".into(), tool_choice);
    f.insert("stream".into(), Some(json!(true)));
    f.insert("max_tokens".into(), g.max_tokens.map(num));
    f.insert("random_seed".into(), g.seed.map(num));
    f.insert("temperature".into(), g.temperature.map(num));
    f.insert("top_p".into(), g.top_p.map(num));
    f.insert("frequency_penalty".into(), g.frequency_penalty.map(num));
    f.insert("presence_penalty".into(), g.presence_penalty.map(num));
    f.insert("stop".into(), g.stop.clone().map(|s| json!(s)));
    let cache_none = request.cache.as_ref().map(|c| c.is_none()).unwrap_or(false);
    f.insert("prompt_cache_key".into(), if cache_none { None } else { cache_key.or_else(|| request.prompt_cache_key.clone().map(Value::String)) });
    f.insert("safe_prompt".into(), safe_prompt);
    f.insert("document_image_limit".into(), image_limit);
    f.insert("document_page_limit".into(), page_limit);
    let parallel = parallel.or_else(|| {
        if request.tool_choice.as_ref().and_then(|c| c.disable_parallel_tool_use) == Some(true) { Some(json!(false)) } else { None }
    });
    f.insert("parallel_tool_calls".into(), parallel);
    f.insert("reasoning_effort".into(), effort);
    f.insert("prompt_mode".into(), prompt_mode);
    Ok(f)
}

pub struct MistralChat;

impl Protocol for MistralChat {
    fn id(&self) -> &str {
        ADAPTER
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        Ok(Value::Object(body_fields(request)?.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect()))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(MistralParser::new(request))
    }
}

pub fn protocol() -> Arc<dyn Protocol> {
    Arc::new(MistralChat)
}

/// Vertex Mistral: Mistral body without `prompt_cache_key`.
pub struct VertexMistral;

impl Protocol for VertexMistral {
    fn id(&self) -> &str {
        ADAPTER
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        let mut f = body_fields(request)?;
        f.shift_remove("prompt_cache_key");
        Ok(Value::Object(f.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect()))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(MistralParser::new(request))
    }
}

pub fn vertex_protocol() -> Arc<dyn Protocol> {
    Arc::new(VertexMistral)
}

pub fn route() -> Route {
    Route::new(ADAPTER, protocol(), Endpoint::new(Some(DEFAULT_BASE_URL), PATH), Framing::SseWithDone).initial_provider("mistral", Some("mistral"))
}

// ---------------------------------------------------------------------------------------------
// Stream parsing
// ---------------------------------------------------------------------------------------------

fn map_finish(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" | "model_length" => FinishReason::Length,
        "tool_calls" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        "error" | "network_error" => FinishReason::Error,
        _ => FinishReason::Unknown,
    }
}

fn map_usage(usage: &Value) -> Option<Usage> {
    if !usage.is_object() {
        return None;
    }
    let u = |p: &[&str]| crate::json::path(usage, p).and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)));
    let input = u(&["prompt_tokens"]);
    let reported = u(&["num_cached_tokens"]).or_else(|| u(&["prompt_tokens_details", "cached_tokens"])).or_else(|| u(&["prompt_token_details", "cached_tokens"]));
    let cached = match (input, reported) {
        (Some(i), Some(r)) => Some(r.min(i)),
        _ => None,
    };
    let output = u(&["completion_tokens"]);
    let mut pm = Object::new();
    pm.insert("mistral".into(), usage.clone());
    Some(Usage {
        context_tokens: None,
        input_tokens: input,
        output_tokens: output,
        non_cached_input_tokens: shared::subtract_tokens(input, cached),
        cache_read_input_tokens: cached,
        cache_write_input_tokens: None,
        reasoning_tokens: None,
        total_tokens: shared::total_tokens(input, output, u(&["total_tokens"])),
        provider_metadata: Some(pm),
    })
}

fn thinking_units(value: Option<&Value>) -> Vec<Value> {
    match value {
        Some(Value::String(s)) => vec![json!({"type": "text", "text": s})],
        Some(Value::Array(items)) => items
            .iter()
            .filter(|u| u.is_object() && u.get("type").map(Value::is_string).unwrap_or(true) && u.get("text").map(Value::is_string).unwrap_or(true))
            .cloned()
            .collect(),
        _ => vec![],
    }
}

fn thinking_text(units: &[Value]) -> String {
    units.iter().filter_map(|u| u.get("text").and_then(Value::as_str)).collect()
}

struct Active {
    reasoning: bool,
    id: String,
    thinking: Option<Object>,
    units: Vec<Value>,
}

#[derive(Clone)]
struct Pending {
    id: String,
    input: String,
}

pub struct MistralParser {
    label: String,
    tools: ToolStream,
    pending: IndexMap<StreamKey, Pending>,
    tool_ids: HashMap<String, String>,
    used_ids: HashSet<String>,
    completed: Vec<LLMEvent>,
    latest_key: Option<StreamKey>,
    generated: u64,
    lifecycle: Lifecycle,
    active: Option<Active>,
    next_content: u64,
    usage: Option<Usage>,
    finish: Option<FinishReasonDetails>,
}

impl MistralParser {
    pub fn new(request: &LLMRequest) -> Self {
        Self {
            label: route_label(request),
            tools: ToolStream::new(ADAPTER),
            pending: IndexMap::new(),
            tool_ids: HashMap::new(),
            used_ids: HashSet::new(),
            completed: vec![],
            latest_key: None,
            generated: 0,
            lifecycle: Lifecycle::new(),
            active: None,
            next_content: 0,
            usage: None,
            finish: None,
        }
    }

    fn close_active(&mut self, events: &mut Vec<LLMEvent>) {
        let Some(active) = self.active.take() else { return };
        if !active.reasoning {
            self.lifecycle.text_end(events, &active.id, None, None);
            return;
        }
        let mut thinking = active.thinking.clone().unwrap_or_default();
        thinking.insert("type".into(), json!("thinking"));
        thinking.insert("thinking".into(), Value::Array(active.units.clone()));
        let mut meta = Object::new();
        meta.insert("mistral".into(), json!({"thinking": Value::Object(thinking)}));
        self.lifecycle.reasoning_end(events, &active.id, Some(meta), Some(thinking_text(&active.units)));
    }

    fn append_text(&mut self, events: &mut Vec<LLMEvent>, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.active.as_ref().map(|a| a.reasoning).unwrap_or(false) {
            self.close_active(events);
        }
        if self.active.is_none() {
            self.active = Some(Active { reasoning: false, id: format!("text-{}", self.next_content), thinking: None, units: vec![] });
            self.next_content += 1;
        }
        let id = self.active.as_ref().unwrap().id.clone();
        self.lifecycle.text_delta(events, &id, text, None);
    }

    fn append_thinking(&mut self, events: &mut Vec<LLMEvent>, part: &Value) {
        if self.active.as_ref().map(|a| !a.reasoning).unwrap_or(false) {
            self.close_active(events);
        }
        if self.active.is_none() {
            self.active = Some(Active { reasoning: true, id: format!("reasoning-{}", self.next_content), thinking: None, units: vec![] });
            self.next_content += 1;
        }
        let units = thinking_units(part.get("thinking"));
        let text = thinking_text(&units);
        let active = self.active.as_mut().unwrap();
        active.units.extend(units);
        let mut thinking = active.thinking.clone().unwrap_or_default();
        // decoded part order: type, text?, thinking?, rest
        let mut decoded = Object::new();
        if let Some(o) = part.as_object() {
            for k in ["type", "text", "thinking"] {
                if let Some(v) = o.get(k) {
                    decoded.insert(k.into(), v.clone());
                }
            }
            for (k, v) in o {
                if !decoded.contains_key(k) {
                    decoded.insert(k.clone(), v.clone());
                }
            }
        }
        for (k, v) in decoded {
            thinking.insert(k, v);
        }
        thinking.insert("type".into(), json!("thinking"));
        thinking.insert("thinking".into(), json!([]));
        active.thinking = Some(thinking);
        let id = active.id.clone();
        if text.is_empty() {
            self.lifecycle.reasoning_start(events, &id, None);
        } else {
            self.lifecycle.reasoning_delta(events, &id, &text, None);
        }
    }

    fn normalize_id(&mut self, source: &str) -> String {
        if is_valid_mistral_id(source) {
            self.used_ids.insert(source.into());
            return source.into();
        }
        if let Some(prev) = self.tool_ids.get(source) {
            return prev.clone();
        }
        let mut attempt = 0;
        let mut id = mistral_hash(source);
        while self.used_ids.contains(&id) {
            attempt += 1;
            id = mistral_hash(&format!("{source}:{attempt}"));
        }
        self.tool_ids.insert(source.into(), id.clone());
        self.used_ids.insert(id.clone());
        id
    }

    fn append_tools(&mut self, events: &mut Vec<LLMEvent>, deltas: &[Value]) -> Result<(), AIError> {
        if deltas.is_empty() {
            return Ok(());
        }
        self.close_active(events);
        let count = deltas.len();
        for (position, delta) in deltas.iter().enumerate() {
            let wire = delta.get("id").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
            let provided = wire.filter(|w| *w != "null").map(String::from);
            let key: StreamKey = match delta.get("index").and_then(Value::as_i64) {
                Some(i) => StreamKey::Index(i),
                None => match &provided {
                    Some(p) => StreamKey::Id(format!("id:{p}")),
                    None if count > 1 => StreamKey::Id(format!("position:{position}")),
                    None => self.latest_key.clone().unwrap_or_else(|| StreamKey::Id(format!("missing:{}", self.generated))),
                },
            };
            let existing = self.tools.get(&key).cloned();
            let pending = self.pending.get(&key).cloned();
            let key_string = match &key {
                StreamKey::Index(i) => i.to_string(),
                StreamKey::Id(s) => s.clone(),
            };
            let source = provided.clone().unwrap_or_else(|| format!("generated:{key_string}"));
            let id = match (&existing, &pending) {
                (Some(e), _) => e.id.clone(),
                (None, Some(p)) => p.id.clone(),
                _ => self.normalize_id(&source),
            };
            let fname = delta.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
            let name = existing.as_ref().map(|e| e.name.clone()).or_else(|| fname.map(String::from));
            let args = match delta.get("function").and_then(|f| f.get("arguments")) {
                Some(Value::String(s)) => s.clone(),
                None | Some(Value::Null) => String::new(),
                Some(other) => encode(other),
            };
            let text = format!("{}{}", pending.as_ref().map(|p| p.input.as_str()).unwrap_or(""), args);
            let Some(name) = name else {
                let increment = provided.is_none() && pending.is_none();
                self.pending.insert(key.clone(), Pending { id, input: text });
                self.latest_key = Some(key);
                if increment {
                    self.generated += 1;
                }
                continue;
            };
            let mut out = vec![];
            self.tools.append_or_start(key.clone(), Some(&id), Some(&name), &text, None, "Mistral Chat tool call delta is missing a name", &mut out)?;
            if !out.is_empty() {
                self.lifecycle.step_start(events);
            }
            events.extend(out);
            self.pending.shift_remove(&key);
            self.latest_key = Some(key);
            if provided.is_none() && existing.is_none() && pending.is_none() {
                self.generated += 1;
            }
        }
        Ok(())
    }

    fn handle(&mut self, event: &Value) -> Result<Vec<LLMEvent>, AIError> {
        if let Some(error) = event.get("error").filter(|e| !e.is_null()) {
            let message = error.get("message").and_then(Value::as_str).unwrap_or("").to_string();
            let status = error.get("code").and_then(Value::as_u64).map(|c| c as u16);
            return Err(classify_provider_failure(ProviderFailure { message, status, raw_body: Some(encode(event)), ..Default::default() }));
        }
        let mut events = vec![];
        let usage = event.get("usage").filter(|u| !u.is_null()).and_then(map_usage).or_else(|| self.usage.clone());
        let choice = event.get("choices").and_then(Value::as_array).and_then(|c| c.first());
        let delta = choice.and_then(|c| c.get("delta")).filter(|d| d.is_object());
        if self.finish.is_some() {
            let late = match delta.and_then(|d| d.get("content")) {
                Some(Value::String(s)) => !s.is_empty(),
                Some(Value::Array(a)) => !a.is_empty(),
                _ => false,
            } || delta.and_then(|d| d.get("tool_calls")).and_then(Value::as_array).map(|a| {
                a.iter().any(|t| {
                    t.get("id").map(crate::json::truthy).unwrap_or(false)
                        || t.get("function").and_then(|f| f.get("name")).map(crate::json::truthy).unwrap_or(false)
                        || t.get("function").map(|f| f.get("arguments").is_some()).unwrap_or(false)
                })
            }).unwrap_or(false);
            if late {
                return Err(AIError::invalid_output(ADAPTER, "Mistral Chat received content after the finish reason").with_body(encode(event)));
            }
            self.usage = usage;
            return Ok(events);
        }
        match delta.and_then(|d| d.get("content")) {
            Some(Value::String(s)) => self.append_text(&mut events, s),
            Some(Value::Array(parts)) => {
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let t = part.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                            self.append_text(&mut events, &t);
                        }
                        Some("thinking") => self.append_thinking(&mut events, part),
                        _ => self.close_active(&mut events),
                    }
                }
            }
            _ => {}
        }
        let deltas = delta.and_then(|d| d.get("tool_calls")).and_then(Value::as_array).cloned().unwrap_or_default();
        self.append_tools(&mut events, &deltas)?;
        let Some(raw) = choice.and_then(|c| c.get("finish_reason")).and_then(Value::as_str).filter(|s| !s.is_empty()) else {
            self.usage = usage;
            return Ok(events);
        };
        let normalized = map_finish(raw);
        if normalized == FinishReason::Error {
            let message = format!("Mistral Chat stopped with {raw}");
            let err = if raw == "network_error" { AIError::provider_internal(message) } else { AIError::unknown_provider(message) };
            return Err(err.with_body(encode(event)));
        }
        let incomplete = matches!(normalized, FinishReason::Length | FinishReason::ContentFilter);
        if !incomplete && !self.pending.is_empty() {
            return Err(AIError::invalid_output(ADAPTER, "Mistral Chat tool call delta is missing a name").with_body(encode(event)));
        }
        if !incomplete && !self.tools.is_empty() {
            let mut out = vec![];
            self.tools.finish_all(&mut out)?;
            self.completed = out;
        }
        self.usage = usage;
        self.finish = Some(FinishReasonDetails::raw(normalized, Some(raw.into())));
        Ok(events)
    }
}

impl StreamParser for MistralParser {
    fn step(&mut self, frame: &Frame) -> Result<Step, AIError> {
        if let Frame::Text(t) = frame {
            if t == "[DONE]" {
                return Ok(Step::terminal(vec![]));
            }
        }
        let event = decode_json_frame(ADAPTER, &self.label, frame)?;
        Ok(Step::events(self.handle(&event)?))
    }

    fn on_halt(&mut self) -> Result<Vec<LLMEvent>, AIError> {
        let Some(finish) = self.finish.clone() else {
            return Err(AIError::incomplete_stream(ADAPTER, "Mistral Chat stream ended without finish_reason"));
        };
        let mut events = vec![];
        self.close_active(&mut events);
        let completed = std::mem::take(&mut self.completed);
        if !completed.is_empty() {
            self.lifecycle.step_start(&mut events);
        }
        let has_call = completed.iter().any(|e| matches!(e, LLMEvent::ToolCall { .. }));
        events.extend(completed);
        let mut reason = finish;
        if reason.normalized == FinishReason::Stop && has_call {
            reason.normalized = FinishReason::ToolCalls;
        }
        self.lifecycle.finish(&mut events, reason, self.usage.clone(), None);
        Ok(events)
    }
}
