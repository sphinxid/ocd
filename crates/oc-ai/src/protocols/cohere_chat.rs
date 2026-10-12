//! Cohere Chat v2 protocol (`protocols/cohere-chat.ts`).

use super::lifecycle::Lifecycle;
use super::shared::{self, fit_thinking_budget, flatten_tool_request, tool_result_text, unsupported_content, wrapped_system_update};
use super::tool_stream::ToolStream;
use super::{decode_json_frame, route_label, Protocol, Step, StreamParser};
use crate::error::AIError;
use crate::json::{encode, num, Object};
use crate::route::framing::{Frame, Framing};
use crate::route::{Endpoint, Route};
use crate::types::*;
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::sync::Arc;

pub const ADAPTER: &str = "cohere-chat";
const NAME: &str = "Cohere Chat";
pub const DEFAULT_BASE_URL: &str = "https://api.cohere.com/v2";

fn invalid_option() -> AIError {
    AIError::invalid_request("Invalid Cohere Chat provider option `thinking`")
}

pub fn body(request: &LLMRequest) -> Result<Value, AIError> {
    let thinking_opt = match request.po("thinking") {
        None => None,
        Some(v) => {
            let o = v.as_object().ok_or_else(invalid_option)?;
            let kind = match o.get("type") {
                None => None,
                Some(Value::String(s)) if s == "enabled" || s == "disabled" => Some(s.clone()),
                _ => return Err(invalid_option()),
            };
            let budget = match o.get("tokenBudget") {
                None => None,
                Some(b) => Some(b.as_f64().filter(|f| f.fract() == 0.0 && *f > 0.0).ok_or_else(invalid_option)?),
            };
            Some((kind, budget))
        }
    };
    let (tools_flat, flattened) = flatten_tool_request(request);
    let mut messages = vec![];
    if !request.system.is_empty() {
        let content = if request.system.len() == 1 {
            json!(request.system[0].text)
        } else {
            Value::Array(request.system.iter().map(|p| json!({"type": "text", "text": p.text})).collect())
        };
        messages.push(json!({"role": "system", "content": content}));
    }
    for message in &flattened.messages {
        match message.role {
            Role::System => {
                let text = wrapped_system_update(NAME, message)?.text;
                messages.push(json!({"role": "user", "content": text}));
            }
            Role::Tool => {
                for part in &message.content {
                    let ContentPart::ToolResult(r) = part else {
                        return Err(unsupported_content(NAME, Role::Tool, &["tool-result"]));
                    };
                    if let ToolResultValue::Content(items) = &r.result {
                        if items.iter().any(|i| matches!(i, ToolContent::File { .. })) {
                            return Err(AIError::invalid_request("Cohere Chat does not support file content in tool results"));
                        }
                    }
                    messages.push(json!({"role": "tool", "tool_call_id": r.id, "content": tool_result_text(&r.result)}));
                }
            }
            Role::User | Role::Assistant => {
                let mut content = vec![];
                let mut calls = vec![];
                let mut plans: Vec<String> = vec![];
                for part in &message.content {
                    match part {
                        ContentPart::Text(t) => content.push(json!({"type": "text", "text": t.text})),
                        ContentPart::Reasoning(r) if message.role == Role::Assistant => {
                            let tool_plan = r.provider_metadata.as_ref().and_then(|pm| pm.get("cohere")).and_then(|c| c.get("toolPlan")) == Some(&json!(true));
                            if tool_plan {
                                plans.push(r.text.clone());
                            } else {
                                content.push(json!({"type": "thinking", "thinking": r.text}));
                            }
                        }
                        ContentPart::ToolCall(c) if message.role == Role::Assistant => {
                            calls.push(json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": encode(&c.input)}}));
                        }
                        ContentPart::Media(m) if message.role == Role::User && m.media.media_type().starts_with("image/") => {
                            let url = match m.media.url_value() {
                                Some(u) => u.to_string(),
                                None => shared::require_inline_media(NAME, &m.media)?.data_url,
                            };
                            content.push(json!({"type": "image_url", "image_url": {"url": url}}));
                        }
                        _ => {
                            let types: &[&str] = if message.role == Role::User { &["text", "media"] } else { &["text", "reasoning", "tool-call"] };
                            return Err(unsupported_content(NAME, message.role, types));
                        }
                    }
                }
                let mut o = Object::new();
                o.insert("role".into(), json!(message.role.as_str()));
                if !content.is_empty() {
                    o.insert("content".into(), Value::Array(content));
                }
                if !calls.is_empty() {
                    o.insert("tool_calls".into(), Value::Array(calls));
                }
                if !plans.is_empty() {
                    o.insert("tool_plan".into(), json!(plans.concat()));
                }
                messages.push(Value::Object(o));
            }
        }
    }
    let selected = request.tool_choice.as_ref().filter(|c| c.kind == ToolChoiceType::Tool).and_then(|c| c.name.clone());
    let tools: Vec<&ToolDefinition> = match &selected {
        None => tools_flat.iter().collect(),
        Some(name) => tools_flat.iter().filter(|t| &t.name == name).collect(),
    };
    if selected.is_some() && tools.is_empty() {
        return Err(AIError::invalid_request("Cohere Chat tool choice must name an available tool"));
    }
    if tools.iter().any(|t| t.native.is_some()) {
        return Err(AIError::invalid_request("Cohere Chat does not support provider-defined tools"));
    }
    let tool_choice = match request.tool_choice.as_ref().map(|c| c.kind) {
        Some(ToolChoiceType::None) => Some(json!("NONE")),
        Some(ToolChoiceType::Required | ToolChoiceType::Tool) => Some(json!("REQUIRED")),
        _ => None,
    };
    let g = request.generation_options();
    let thinking = thinking_opt.map(|(kind, budget)| {
        let mut o = Object::new();
        o.insert("type".into(), json!(kind.unwrap_or_else(|| "enabled".into())));
        if let Some(b) = budget {
            o.insert("token_budget".into(), num(fit_thinking_budget(b, g.max_tokens, 1.0)));
        }
        Value::Object(o)
    });
    let mut f: IndexMap<String, Option<Value>> = IndexMap::new();
    f.insert("model".into(), Some(json!(request.model.id)));
    f.insert("messages".into(), Some(Value::Array(messages)));
    f.insert("stream".into(), Some(json!(true)));
    f.insert(
        "tools".into(),
        if tools.is_empty() {
            None
        } else {
            Some(Value::Array(
                tools
                    .iter()
                    .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": Value::Object(t.input_schema.clone())}}))
                    .collect(),
            ))
        },
    );
    f.insert("tool_choice".into(), tool_choice);
    f.insert("thinking".into(), thinking);
    f.insert("max_tokens".into(), g.max_tokens.map(num));
    f.insert("temperature".into(), g.temperature.map(num));
    f.insert("p".into(), g.top_p.map(num));
    f.insert("k".into(), g.top_k.map(num));
    f.insert("seed".into(), g.seed.map(num));
    f.insert("stop_sequences".into(), g.stop.clone().map(|s| json!(s)));
    f.insert("frequency_penalty".into(), g.frequency_penalty.map(num));
    f.insert("presence_penalty".into(), g.presence_penalty.map(num));
    Ok(Value::Object(f.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect()))
}

pub struct CohereChat;

impl Protocol for CohereChat {
    fn id(&self) -> &str {
        ADAPTER
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        body(request)
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(CohereParser { label: route_label(request), lifecycle: Lifecycle::new(), tools: ToolStream::new(ADAPTER), finished: false })
    }
}

pub fn protocol() -> Arc<dyn Protocol> {
    Arc::new(CohereChat)
}

pub fn route() -> Route {
    Route::new(ADAPTER, protocol(), Endpoint::new(Some(DEFAULT_BASE_URL), "/chat"), Framing::Sse).initial_provider("cohere", Some("cohere"))
}

fn finish_reason(raw: &str) -> FinishReasonDetails {
    let normalized = match raw {
        "COMPLETE" | "STOP_SEQUENCE" => FinishReason::Stop,
        "MAX_TOKENS" => FinishReason::Length,
        "TOOL_CALL" => FinishReason::ToolCalls,
        "ERROR" | "TIMEOUT" => FinishReason::Error,
        _ => FinishReason::Unknown,
    };
    FinishReasonDetails::raw(normalized, Some(raw.into()))
}

fn token_counts(v: &Value) -> Value {
    let mut o = Object::new();
    for k in ["input_tokens", "output_tokens", "reasoning_tokens"] {
        if let Some(x) = v.get(k) {
            o.insert(k.into(), x.clone());
        }
    }
    Value::Object(o)
}

fn map_usage(usage: &Value) -> Usage {
    let mut decoded = Object::new();
    if let Some(t) = usage.get("tokens") {
        decoded.insert("tokens".into(), token_counts(t));
    }
    if let Some(t) = usage.get("billed_units") {
        decoded.insert("billed_units".into(), token_counts(t));
    }
    if let Some(c) = usage.get("cached_tokens") {
        decoded.insert("cached_tokens".into(), c.clone());
    }
    let u = |k: &str| usage.get("tokens").and_then(|t| crate::json::get_u64(t, k));
    let cached = crate::json::get_u64(usage, "cached_tokens");
    let mut pm = Object::new();
    pm.insert("cohere".into(), Value::Object(decoded));
    Usage {
        context_tokens: None,
        input_tokens: u("input_tokens"),
        output_tokens: u("output_tokens"),
        non_cached_input_tokens: shared::subtract_tokens(u("input_tokens"), cached),
        cache_read_input_tokens: cached,
        cache_write_input_tokens: None,
        reasoning_tokens: u("reasoning_tokens"),
        total_tokens: shared::total_tokens(u("input_tokens"), u("output_tokens"), None),
        provider_metadata: Some(pm),
    }
}

struct CohereParser {
    label: String,
    lifecycle: Lifecycle,
    tools: ToolStream,
    finished: bool,
}

const EVENT_TYPES: &[&str] = &[
    "message-start",
    "content-start",
    "content-delta",
    "content-end",
    "tool-plan-delta",
    "tool-call-start",
    "tool-call-delta",
    "tool-call-end",
    "message-end",
    "citation-start",
    "citation-end",
];

impl StreamParser for CohereParser {
    fn step(&mut self, frame: &Frame) -> Result<Step, AIError> {
        let event = decode_json_frame(ADAPTER, &self.label, frame)?;
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        let invalid = || AIError::invalid_output(ADAPTER, format!("Invalid {} stream event", self.label)).with_body(encode(&event));
        if !EVENT_TYPES.contains(&kind) {
            return Err(invalid());
        }
        let index = event.get("index").and_then(Value::as_i64);
        let mut events = vec![];
        match kind {
            "message-start" => self.lifecycle.step_start(&mut events),
            "content-delta" => {
                let index = index.ok_or_else(invalid)?;
                let content = crate::json::path(&event, &["delta", "message", "content"]).ok_or_else(invalid)?;
                let id = index.to_string();
                if let Some(thinking) = content.get("thinking").and_then(Value::as_str) {
                    self.lifecycle.reasoning_delta(&mut events, &id, thinking, None);
                } else {
                    let text = content.get("text").and_then(Value::as_str).unwrap_or("");
                    self.lifecycle.text_delta(&mut events, &id, text, None);
                }
            }
            "content-end" => {
                let id = index.ok_or_else(invalid)?.to_string();
                self.lifecycle.reasoning_end(&mut events, &id, None, None);
                self.lifecycle.text_end(&mut events, &id, None, None);
            }
            "tool-plan-delta" => {
                let plan = crate::json::path(&event, &["delta", "message", "tool_plan"]).and_then(Value::as_str).ok_or_else(invalid)?;
                let mut meta = Object::new();
                meta.insert("cohere".into(), json!({"toolPlan": true}));
                self.lifecycle.reasoning_delta(&mut events, "tool-plan", plan, Some(meta));
            }
            "tool-call-start" | "tool-call-delta" => {
                let index = index.ok_or_else(invalid)?;
                let call = crate::json::path(&event, &["delta", "message", "tool_calls"]).ok_or_else(invalid)?;
                let id = call.get("id").and_then(Value::as_str);
                let name = call.get("function").and_then(|f| f.get("name")).and_then(Value::as_str);
                let args = call.get("function").and_then(|f| f.get("arguments")).and_then(Value::as_str).unwrap_or("");
                self.tools.append_or_start(index, id, name, args, None, "Cohere tool call is missing id or name", &mut events)?;
            }
            "tool-call-end" => {
                let index = index.ok_or_else(invalid)?;
                self.tools.finish(index, &mut events)?;
            }
            "message-end" => {
                let reason = crate::json::path(&event, &["delta", "finish_reason"]).and_then(Value::as_str).ok_or_else(invalid)?;
                self.tools.finish_all(&mut events)?;
                let usage = crate::json::path(&event, &["delta", "usage"]).map(map_usage);
                self.lifecycle.finish(&mut events, finish_reason(reason), usage, None);
                self.finished = true;
                return Ok(Step::terminal(events));
            }
            _ => {}
        }
        Ok(Step::events(events))
    }

    fn on_halt(&mut self) -> Result<Vec<LLMEvent>, AIError> {
        if self.finished {
            Ok(vec![])
        } else {
            Err(AIError::invalid_output(ADAPTER, "Cohere stream ended without message-end"))
        }
    }
}
