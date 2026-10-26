//! Google Interactions protocol (`protocols/google-interactions.ts`, `providers/google-vertex-interactions.ts`).

use super::lifecycle::Lifecycle;
use super::shared::{self, flatten_tool_request, match_tool_choice, tool_file_media, tool_result_text, unsupported_content, wrapped_system_update, ToolChoiceMode};
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
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

fn media_content(asset: &MediaAsset) -> Result<Value, AIError> {
    let media_type = asset.media_type();
    let kind = asset.kind();
    let kind_name = match kind {
        MediaKind::Image => "image",
        MediaKind::Audio => "audio",
        MediaKind::Video => "video",
        _ if media_type == "application/pdf" || media_type == "text/csv" => "document",
        _ => return Err(AIError::invalid_request(format!("Google Interactions does not support {media_type} document input"))),
    };
    if let MediaSource::Ref { provider, id, .. } = &asset.source {
        if provider == "google" {
            return Ok(json!({"type": kind_name, "uri": id, "mime_type": media_type}));
        }
    }
    let inline = shared::require_inline_media("Google Interactions", asset)?;
    Ok(json!({"type": kind_name, "data": inline.base64, "mime_type": inline.mime}))
}

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
                                ToolContent::File { uri, mime, name } => media_content(&tool_file_media(uri, mime, name.as_deref()).media),
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
                    if let Some(s) = signature(c.provider_metadata.as_ref(), &key) {
                        o.insert("signature".into(), json!(s));
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
            steps: BTreeMap::new(),
            tools: ToolStream::new(ADAPTER),
            completed: false,
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
    Route::new(ADAPTER, protocol(), Endpoint::new(Some(DEFAULT_BASE_URL), "/interactions"), Framing::Sse).initial_provider("google", Some("google"))
}

/// `google-vertex-interactions` route (base URL configured by the factory).
pub fn vertex_route() -> Route {
    let mut endpoint = Endpoint::new(None, "/interactions");
    endpoint.query.insert("alt".into(), "sse".into());
    Route::new("google-vertex-interactions", protocol(), endpoint, Framing::Sse)
        .initial_provider("google-vertex", Some("vertex"))
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

fn map_usage(usage: &Value, key: &str) -> Usage {
    let u = |k: &str| crate::json::get_u64(usage, k);
    let input = u("total_input_tokens");
    let cached = u("total_cached_tokens");
    let reasoning = u("total_thought_tokens");
    let output = u("total_output_tokens").map(|o| o + reasoning.unwrap_or(0));
    let mut pm = Object::new();
    pm.insert(key.into(), usage.clone());
    Usage {
        context_tokens: u("raw_prompt_token").or(input),
        input_tokens: input,
        output_tokens: output,
        non_cached_input_tokens: shared::subtract_tokens(input, cached),
        cache_read_input_tokens: cached,
        cache_write_input_tokens: None,
        reasoning_tokens: reasoning,
        total_tokens: u("total_tokens"),
        provider_metadata: Some(pm),
    }
}

struct Parser {
    label: String,
    key: String,
    lifecycle: Lifecycle,
    steps: BTreeMap<i64, Value>,
    tools: ToolStream,
    completed: bool,
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
                let id = index.to_string();
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
                self.steps.insert(index, step);
            }
            "step.delta" => {
                let index = index.ok_or_else(invalid)?;
                let delta = event.get("delta").filter(|d| d.get("type").map(Value::is_string).unwrap_or(false)).ok_or_else(invalid)?.clone();
                let Some(step) = self.steps.get(&index).cloned() else {
                    return Err(Self::err("Interactions delta without step.start", &delta));
                };
                let step_type = step.get("type").and_then(Value::as_str).unwrap_or("");
                let id = index.to_string();
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
                let id = index.to_string();
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
            "interaction.created" | "interaction.status_update" => {}
            "error" => {
                let raw = encode(event);
                let message = provider_error_message(&raw).unwrap_or_else(|| "Google Interactions stream error".into());
                return Err(classify_provider_failure(ProviderFailure { message, data: event.get("error").cloned(), raw_body: Some(raw), ..Default::default() }));
            }
            "interaction.completed" => {
                let interaction = event.get("interaction").filter(|i| i.get("status").map(Value::is_string).unwrap_or(false)).ok_or_else(invalid)?;
                let status = interaction["status"].as_str().unwrap();
                if status == "failed" || status == "cancelled" {
                    return Err(classify_provider_failure(ProviderFailure {
                        message: format!("Google Interactions {status}"),
                        data: Some(interaction.clone()),
                        raw_body: Some(encode(event)),
                        ..Default::default()
                    }));
                }
                let normalized = match status {
                    "completed" => FinishReason::Stop,
                    "requires_action" => FinishReason::ToolCalls,
                    "incomplete" => FinishReason::Length,
                    other => return Err(Self::err(format!("Unexpected terminal Interactions status: {other}"), event)),
                };
                self.tools.finish_all(&mut events)?;
                let usage = interaction.get("usage").filter(|u| u.is_object()).map(|u| map_usage(u, &self.key));
                let mut inner = Object::new();
                if let Some(id) = interaction.get("id") {
                    inner.insert("interactionId".into(), id.clone());
                }
                let mut pm = Object::new();
                pm.insert(self.key.clone(), Value::Object(inner));
                self.lifecycle.finish(&mut events, FinishReasonDetails::raw(normalized, Some(status.into())), usage, Some(pm));
                self.completed = true;
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
    fn on_halt(&mut self) -> Result<Vec<LLMEvent>, AIError> {
        if self.completed {
            Ok(vec![])
        } else {
            Err(AIError::invalid_output(ADAPTER, "Google Interactions stream ended before interaction.completed"))
        }
    }
}
