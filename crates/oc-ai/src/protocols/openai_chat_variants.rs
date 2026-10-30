//! OpenAI Chat wrappers: `zai-chat`, `alibaba-chat`, `venice-chat` (`protocols/{zai,alibaba,venice}-chat.ts`).

use super::openai_chat::{self, body_fields, finalize, ChatOptions, ChatParser};
use super::shared::fit_thinking_budget;
use super::{decode_json_frame, route_label, Protocol, Step, StreamParser};
use crate::error::{classify_provider_failure, AIError, ProviderFailure};
use crate::json::{encode, num, Object};
use crate::route::framing::Frame;
use crate::types::*;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

fn opt_err(route: &str, key: &str) -> AIError {
    AIError::invalid_request(format!("Invalid {route} provider option `{key}`"))
}

/// Ordered subset of an object (schema struct decode), validating basic types.
fn pick(value: &Value, keys: &[(&str, fn(&Value) -> bool)], route: &str, field: &str) -> Result<Value, AIError> {
    let o = value.as_object().ok_or_else(|| opt_err(route, field))?;
    let mut out = Object::new();
    for (k, check) in keys {
        if let Some(v) = o.get(*k) {
            if !check(v) {
                return Err(opt_err(route, field));
            }
            out.insert((*k).into(), v.clone());
        }
    }
    Ok(Value::Object(out))
}

fn is_str(v: &Value) -> bool {
    v.is_string()
}
fn is_bool(v: &Value) -> bool {
    v.is_boolean()
}
fn is_num(v: &Value) -> bool {
    v.is_number()
}
fn is_obj(v: &Value) -> bool {
    v.is_object()
}

fn typed(request: &LLMRequest, key: &str, check: fn(&Value) -> bool, route: &str) -> Result<Option<Value>, AIError> {
    match request.po(key) {
        None => Ok(None),
        Some(v) if check(v) => Ok(Some(v.clone())),
        Some(_) => Err(opt_err(route, key)),
    }
}

// ---------------------------------------------------------------------------------------------
// zai-chat
// ---------------------------------------------------------------------------------------------

static GLM: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)(?:^|/)glm-(\d+)(?:\.(\d+))?(?:-|$)").unwrap());

fn supports_tool_streaming(model_id: &str) -> bool {
    GLM.captures(model_id)
        .map(|c| {
            let major: u32 = c[1].parse().unwrap_or(0);
            let minor: u32 = c.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            major > 4 || (major == 4 && minor >= 6)
        })
        .unwrap_or(false)
}

pub struct ZaiChat;

impl Protocol for ZaiChat {
    fn id(&self) -> &str {
        "zai-chat"
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        const R: &str = "Z.ai Chat";
        typed(request, "reasoningEffort", is_str, R)?;
        let thinking = request.po("thinking").map(|v| pick(v, &[("type", is_str), ("clear_thinking", is_bool)], R, "thinking")).transpose()?;
        let tool_stream = typed(request, "toolStream", is_bool, R)?;
        let do_sample = typed(request, "doSample", is_bool, R)?;
        let response_format = request.po("responseFormat").map(|v| pick(v, &[("type", is_str)], R, "responseFormat")).transpose()?;
        let request_id = typed(request, "requestID", is_str, R)?;
        let user_id = typed(request, "userID", is_str, R)?;
        let mut f = body_fields(request, &ChatOptions::default())?;
        let has_tools = f.get("tools").and_then(|t| t.as_ref()).and_then(Value::as_array).map(|a| !a.is_empty()).unwrap_or(false);
        f.insert("thinking".into(), thinking);
        let ts = tool_stream.or_else(|| if has_tools && supports_tool_streaming(&request.model.id) { Some(json!(true)) } else { None });
        f.insert("tool_stream".into(), ts);
        f.insert("do_sample".into(), do_sample);
        f.insert("response_format".into(), response_format);
        f.insert("request_id".into(), request_id);
        f.insert("user_id".into(), user_id);
        Ok(finalize(f))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(ChatParser::new(request))
    }
}

pub fn zai_protocol() -> Arc<dyn Protocol> {
    Arc::new(ZaiChat)
}

pub fn zai_compatibility() -> Compatibility {
    Compatibility {
        max_tokens_field: Some("max_tokens".into()),
        supports_strict_mode: Some(false),
        reasoning_field: Some("reasoning_content".into()),
        zai_tool_stream: Some(false),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------------------------
// alibaba-chat
// ---------------------------------------------------------------------------------------------

pub struct AlibabaChat;

impl Protocol for AlibabaChat {
    fn id(&self) -> &str {
        "alibaba-chat"
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        const R: &str = "Alibaba Chat";
        typed(request, "reasoningEffort", is_str, R)?;
        let enable_thinking = typed(request, "enableThinking", is_bool, R)?;
        let budget = typed(request, "thinkingBudget", |v| v.as_f64().map(|f| f.fract() == 0.0).unwrap_or(false), R)?;
        let preserve = typed(request, "preserveThinking", is_bool, R)?;
        let clear = typed(request, "clearThinking", is_bool, R)?;
        let thinking = request.po("thinking").map(|v| pick(v, &[("type", is_str)], R, "thinking")).transpose()?;
        let tool_stream = typed(request, "toolStream", is_bool, R)?;
        let parallel = typed(request, "parallelToolCalls", is_bool, R)?;
        let repetition = typed(request, "repetitionPenalty", is_num, R)?;
        let response_format =
            request.po("responseFormat").map(|v| pick(v, &[("type", is_str), ("json_schema", is_obj)], R, "responseFormat")).transpose()?;
        let enable_search = typed(request, "enableSearch", is_bool, R)?;
        let search_options = request
            .po("searchOptions")
            .map(|v| pick(v, &[("forced_search", is_bool), ("search_strategy", is_str), ("enable_search_extension", is_bool)], R, "searchOptions"))
            .transpose()?;
        let mut f = body_fields(request, &ChatOptions { cache_control: true, assistant: None })?;
        let max_tokens = request.generation.as_ref().and_then(|g| g.max_tokens);
        f.insert("enable_thinking".into(), enable_thinking);
        f.insert(
            "thinking_budget".into(),
            budget.and_then(|b| b.as_f64()).map(|b| num(fit_thinking_budget(b, max_tokens, 1.0))),
        );
        f.insert("preserve_thinking".into(), preserve);
        f.insert("clear_thinking".into(), clear);
        f.insert("thinking".into(), thinking);
        f.insert("tool_stream".into(), tool_stream);
        let parallel = parallel.or_else(|| {
            request.tool_choice.as_ref().and_then(|c| c.disable_parallel_tool_use).map(|d| json!(!d))
        });
        f.insert("parallel_tool_calls".into(), parallel);
        f.insert("repetition_penalty".into(), repetition);
        f.insert("top_k".into(), request.generation.as_ref().and_then(|g| g.top_k).map(num));
        f.insert("response_format".into(), response_format);
        f.insert("enable_search".into(), enable_search);
        f.insert("search_options".into(), search_options);
        Ok(finalize(f))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(ChatParser::new(request))
    }
}

pub fn alibaba_protocol() -> Arc<dyn Protocol> {
    Arc::new(AlibabaChat)
}

pub fn alibaba_compatibility() -> Compatibility {
    Compatibility {
        max_tokens_field: Some("max_completion_tokens".into()),
        supports_strict_mode: Some(false),
        reasoning_field: Some("reasoning_content".into()),
        zai_tool_stream: Some(false),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------------------------
// venice-chat
// ---------------------------------------------------------------------------------------------

const MARKER: &str = "__ENCRYPTED_REASONING__";

fn venice_meta<'a>(part: &'a ContentPart, key: &str) -> Option<&'a str> {
    part.provider_metadata()?.get("venice")?.get(key)?.as_str()
}

fn snake(key: &str) -> String {
    let mut out = String::new();
    for c in key.chars() {
        if c.is_ascii_uppercase() {
            out.push('_');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

pub struct VeniceChat;

impl Protocol for VeniceChat {
    fn id(&self) -> &str {
        "venice-chat"
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        const R: &str = "Venice Chat";
        let reasoning_effort = typed(request, "reasoningEffort", is_str, R)?;
        let reasoning = request
            .po("reasoning")
            .map(|v| pick(v, &[("effort", is_str), ("enabled", is_bool), ("summary", is_str)], R, "reasoning"))
            .transpose()?;
        const PARAMS: &[(&str, fn(&Value) -> bool)] = &[
            ("enableWebSearch", is_str),
            ("enableWebScraping", is_bool),
            ("enableWebCitations", is_bool),
            ("enableXSearch", is_bool),
            ("stripThinkingResponse", is_bool),
            ("disableThinking", is_bool),
            ("includeVeniceSystemPrompt", is_bool),
            ("characterSlug", is_str),
            ("includeSearchResultsInStream", is_bool),
            ("returnSearchResultsAsDocuments", is_bool),
        ];
        let params = request.po("veniceParameters").map(|v| pick(v, PARAMS, R, "veniceParameters")).transpose()?;
        let get = |k: &str, check: fn(&Value) -> bool| typed(request, k, check, R);
        typed(request, "promptCacheKey", is_str, R)?;
        let retention = get("promptCacheRetention", is_str)?;
        let parallel = get("parallelToolCalls", is_bool)?;
        let max_completion = get("maxCompletionTokens", is_num)?;
        let max_tokens = get("maxTokens", is_num)?;
        let min_p = get("minP", is_num)?;
        let repetition = get("repetitionPenalty", is_num)?;
        let stop_ids = get("stopTokenIds", |v| v.as_array().map(|a| a.iter().all(Value::is_number)).unwrap_or(false))?;
        let logprobs = get("logprobs", is_bool)?;
        let top_logprobs = get("topLogprobs", is_num)?;
        let max_temp = get("maxTemp", is_num)?;
        let min_temp = get("minTemp", is_num)?;
        let response_format = get("responseFormat", is_obj)?;
        let user = get("user", is_str)?;

        let hook: openai_chat::AssistantHook = Arc::new(|source: &Message, message: &mut Object| {
            let parts: Vec<&ContentPart> = source
                .content
                .iter()
                .filter(|p| matches!(p, ContentPart::Text(_) | ContentPart::Reasoning(_) | ContentPart::ToolCall(_)))
                .collect();
            let calls: Vec<&ContentPart> = source.content.iter().filter(|p| matches!(p, ContentPart::ToolCall(_))).collect();
            if let Some(sig) = parts.iter().find_map(|p| venice_meta(p, "messageThoughtSignature")) {
                message.insert("thought_signature".into(), json!(sig));
            }
            if let Some(raw) = parts.iter().find_map(|p| venice_meta(p, "encryptedReasoningContent")) {
                message.insert("reasoning_content".into(), json!(raw));
            }
            if let Some(Value::Array(tool_calls)) = message.get_mut("tool_calls") {
                for (i, call) in tool_calls.iter_mut().enumerate() {
                    if let Some(sig) = calls.get(i).and_then(|c| venice_meta(c, "thoughtSignature")) {
                        call.as_object_mut().unwrap().insert("thought_signature".into(), json!(sig));
                    }
                }
            }
        });
        let mut f = body_fields(request, &ChatOptions { cache_control: true, assistant: Some(hook) })?;
        let mct = max_completion.or(max_tokens).or_else(|| request.generation.as_ref().and_then(|g| g.max_tokens).map(num));
        f.insert("max_completion_tokens".into(), mct);
        f.insert("reasoning_effort".into(), None);
        let reasoning = match reasoning_effort {
            None => reasoning,
            Some(effort) => {
                let mut o = reasoning.and_then(|r| r.as_object().cloned()).unwrap_or_default();
                o.insert("effort".into(), effort);
                Some(Value::Object(o))
            }
        };
        f.insert("reasoning".into(), reasoning);
        let mut vp = params.and_then(|p| p.as_object().cloned()).unwrap_or_default();
        if !vp.contains_key("includeVeniceSystemPrompt") {
            vp.insert("includeVeniceSystemPrompt".into(), json!(false));
        }
        let vp: Object = vp.into_iter().map(|(k, v)| (snake(&k), v)).collect();
        f.insert("venice_parameters".into(), Some(Value::Object(vp)));
        let retention = retention.or_else(|| f.get("prompt_cache_retention").cloned().flatten());
        f.insert("prompt_cache_retention".into(), retention);
        f.insert("parallel_tool_calls".into(), parallel);
        f.insert("min_p".into(), min_p);
        f.insert("top_k".into(), request.generation.as_ref().and_then(|g| g.top_k).map(num));
        f.insert("repetition_penalty".into(), repetition);
        f.insert("stop_token_ids".into(), stop_ids);
        f.insert("logprobs".into(), logprobs);
        f.insert("top_logprobs".into(), top_logprobs);
        f.insert("max_temp".into(), max_temp);
        f.insert("min_temp".into(), min_temp);
        f.insert("response_format".into(), response_format);
        f.insert("user".into(), user);
        Ok(finalize(f))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(VeniceParser {
            shared: ChatParser::new(request),
            label: route_label(request),
            pending: String::new(),
            reasoning: String::new(),
            encrypted: false,
            signature: None,
            tools: HashMap::new(),
        })
    }
}

pub fn venice_protocol() -> Arc<dyn Protocol> {
    Arc::new(VeniceChat)
}

pub fn venice_compatibility() -> Compatibility {
    Compatibility {
        max_tokens_field: Some("max_completion_tokens".into()),
        supports_prompt_cache_key: Some(true),
        ..Default::default()
    }
}

struct VeniceParser {
    shared: ChatParser,
    label: String,
    pending: String,
    reasoning: String,
    encrypted: bool,
    signature: Option<String>,
    tools: HashMap<String, String>,
}

impl StreamParser for VeniceParser {
    fn step(&mut self, frame: &Frame) -> Result<Step, AIError> {
        if let Frame::Text(t) = frame {
            if t == "[DONE]" {
                return Ok(Step::terminal(vec![]));
            }
        }
        let mut event = decode_json_frame("venice-chat", &self.label, frame)?;
        if let Some(Value::String(error)) = event.get("error") {
            let mut parts = vec![error.clone()];
            for issue in event.get("issues").and_then(Value::as_array).cloned().unwrap_or_default() {
                let message = issue.get("message").and_then(Value::as_str).unwrap_or("");
                let path: Vec<String> =
                    issue.get("path").and_then(Value::as_array).map(|p| p.iter().map(crate::json::js_string).collect()).unwrap_or_default();
                parts.push(if path.is_empty() { message.to_string() } else { format!("{}: {message}", path.join(".")) });
            }
            return Err(classify_provider_failure(ProviderFailure::new(parts.join("; ")).raw_body(encode(&event))));
        }
        let delta = event.get("choices").and_then(Value::as_array).and_then(|c| c.first()).and_then(|c| c.get("delta")).cloned();
        let scalar = delta.as_ref().and_then(|d| d.get("reasoning_content")).and_then(Value::as_str).unwrap_or("").to_string();
        let text = format!("{}{}", self.pending, scalar);
        let marker = text.find(MARKER);
        let pending_len = if marker.is_some() || self.encrypted {
            0
        } else {
            (1..=text.len().min(MARKER.len() - 1)).filter(|len| text.ends_with(&MARKER[..*len])).last().unwrap_or(0)
        };
        let visible = if self.encrypted {
            String::new()
        } else if let Some(m) = marker {
            text[..m].to_string()
        } else {
            text[..text.len() - pending_len].to_string()
        };
        if let Some(usage) = event.get_mut("usage").and_then(Value::as_object_mut) {
            if let Some(details) = usage.get_mut("prompt_tokens_details").and_then(Value::as_object_mut) {
                if details.get("cache_write_tokens").map(Value::is_null).unwrap_or(true) {
                    if let Some(v) = details.get("cache_creation_input_tokens").cloned() {
                        details.insert("cache_write_tokens".into(), v);
                    }
                }
            }
        }
        if let Some(choice) = event.get_mut("choices").and_then(Value::as_array_mut).and_then(|c| c.first_mut()) {
            if let Some(d) = choice.get_mut("delta").and_then(Value::as_object_mut) {
                if visible.is_empty() {
                    d.shift_remove("reasoning_content");
                } else {
                    d.insert("reasoning_content".into(), json!(visible));
                }
                if d.get("reasoning_details").map(Value::is_null).unwrap_or(true) && marker.is_some() {
                    d.insert("reasoning_details".into(), json!([]));
                }
            }
        }
        let events = self.shared.handle(&event)?;
        for call in delta.as_ref().and_then(|d| d.get("tool_calls")).and_then(Value::as_array).cloned().unwrap_or_default() {
            let Some(sig) = call.get("thought_signature").and_then(Value::as_str).filter(|s| !s.is_empty()) else { continue };
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .map(String::from)
                .or_else(|| self.shared.tool_id(call.get("index").and_then(Value::as_i64)));
            if let Some(id) = id {
                self.tools.insert(id, sig.into());
            }
        }
        self.pending = if pending_len > 0 { text[text.len() - pending_len..].to_string() } else { String::new() };
        self.reasoning.push_str(&scalar);
        self.encrypted = self.encrypted || marker.is_some();
        if let Some(sig) = delta.as_ref().and_then(|d| d.get("thought_signature")).and_then(Value::as_str) {
            self.signature = Some(sig.into());
        }
        Ok(Step::events(events))
    }

    fn on_halt(&mut self) -> Result<Vec<LLMEvent>, AIError> {
        let events = self.shared.finish_events()?;
        let mut out = vec![];
        for event in events {
            let (is_target, id) = match &event {
                LLMEvent::ReasoningEnd { id, .. } | LLMEvent::TextEnd { id, .. } => (true, id.clone()),
                LLMEvent::ToolCall { id, .. } | LLMEvent::ToolInputEnd { id, .. } => (true, id.clone()),
                _ => (false, String::new()),
            };
            if !is_target {
                out.push(event);
                continue;
            }
            let tool_sig = match &event {
                LLMEvent::ToolCall { .. } | LLMEvent::ToolInputEnd { .. } => self.tools.get(&id).cloned(),
                _ => None,
            };
            let is_reasoning_end = matches!(event, LLMEvent::ReasoningEnd { .. });
            let patch = |pm: &mut Option<ProviderMetadata>| {
                let mut map = pm.clone().unwrap_or_default();
                let mut venice = map.get("venice").and_then(Value::as_object).cloned().unwrap_or_default();
                if let Some(s) = &self.signature {
                    venice.insert("messageThoughtSignature".into(), json!(s));
                }
                if let Some(s) = &tool_sig {
                    venice.insert("thoughtSignature".into(), json!(s));
                }
                if is_reasoning_end && self.encrypted {
                    venice.insert("encryptedReasoningContent".into(), json!(self.reasoning));
                }
                map.insert("venice".into(), Value::Object(venice));
                *pm = Some(map);
            };
            let mut event = event;
            match &mut event {
                LLMEvent::ReasoningEnd { provider_metadata, .. }
                | LLMEvent::TextEnd { provider_metadata, .. }
                | LLMEvent::ToolCall { provider_metadata, .. }
                | LLMEvent::ToolInputEnd { provider_metadata, .. } => patch(provider_metadata),
                _ => {}
            }
            if is_reasoning_end && !self.pending.is_empty() {
                out.push(LLMEvent::ReasoningDelta { id: id.clone(), text: self.pending.clone(), provider_metadata: None });
            }
            out.push(event);
        }
        Ok(out)
    }
}

