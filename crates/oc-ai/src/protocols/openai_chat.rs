//! OpenAI Chat Completions protocol (`protocols/openai-chat.ts`, `openai-compatible-chat.ts`).

use super::lifecycle::Lifecycle;
use super::options;
use super::shared::{
    self, flatten_tool_request, match_tool_choice, prompt_cache_key, tool_file_media, tool_result_text,
    unsupported_content, wrapped_system_update, CacheControl, ToolChoiceMode,
};
use super::tool_stream::ToolStream;
use super::{decode_json_frame, route_label, Protocol, Step, StreamParser};
use crate::error::{classify_provider_failure, AIError, ProviderFailure};
use crate::json::{encode, num, Object};
use crate::route::framing::{Frame, Framing};
use crate::route::{Endpoint, Route};
use crate::types::*;
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const ADAPTER: &str = "openai-chat";
const NAME: &str = "OpenAI Chat";
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const PATH: &str = "/chat/completions";
const RESERVED_REASONING_FIELDS: &[&str] = &["role", "content", "refusal", "tool_calls"];

/// Ordered body fields; `None` = undefined.
pub type Fields = IndexMap<String, Option<Value>>;

pub fn finalize(fields: Fields) -> Value {
    Value::Object(fields.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect())
}

pub type AssistantHook = Arc<dyn Fn(&Message, &mut Object) + Send + Sync>;

/// Lowering options for wrappers.
#[derive(Clone, Default)]
pub struct ChatOptions {
    /// Lower cache hints to `cache_control` (DigitalOcean, OpenRouter, Alibaba, Venice).
    pub cache_control: bool,
    /// Project provider-specific fields onto lowered assistant messages.
    pub assistant: Option<AssistantHook>,
}

// ---------------------------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------------------------

pub fn detect_max_tokens_field(provider: &str, base_url: &str) -> &'static str {
    let p = provider.to_lowercase();
    let url = base_url.to_lowercase();
    let max_tokens = ["deepseek", "moonshotai", "togetherai", "zai", "zai-coding-plan", "zhipuai", "zhipuai-coding-plan", "nvidia", "cerebras", "chutes", "cloudflare-ai-gateway", "cloudflare-workers-ai"]
        .contains(&p.as_str())
        || [
            "deepseek.com",
            "api.moonshot.ai",
            "api.together.",
            "api.z.ai",
            "open.bigmodel.cn",
            "integrate.api.nvidia.com",
            "cerebras.ai",
            "llm.chutes.ai",
            "gateway.ai.cloudflare.com",
            "api.cloudflare.com",
        ]
        .iter()
        .any(|s| url.contains(s));
    if max_tokens { "max_tokens" } else { "max_completion_tokens" }
}

pub fn detect_supports_store(provider: &str, base_url: &str) -> bool {
    let p = provider.to_lowercase();
    let url = base_url.to_lowercase();
    let is = |ids: &[&str], hosts: &[&str]| ids.contains(&p.as_str()) || hosts.iter().any(|h| url.contains(h));
    let non_standard = url.contains("generativelanguage.googleapis.com")
        || is(&["nvidia"], &["integrate.api.nvidia.com"])
        || is(&["cerebras"], &["cerebras.ai"])
        || is(&["xai"], &["api.x.ai"])
        || is(&["togetherai", "together"], &["api.together."])
        || is(&["chutes"], &["chutes.ai"])
        || is(&["deepseek"], &["deepseek.com"])
        || is(&["zai", "zai-coding-plan", "zhipuai", "zhipuai-coding-plan"], &["api.z.ai", "open.bigmodel.cn"])
        || is(&["moonshotai", "moonshotai-cn"], &["api.moonshot."])
        || is(&["opencode"], &["opencode.ai"])
        || is(&["cloudflare-workers-ai"], &["api.cloudflare.com"])
        || is(&["cloudflare-ai-gateway"], &["gateway.ai.cloudflare.com"])
        || is(&["vercel-ai-gateway"], &["ai-gateway.vercel.sh", "vercel.sh"])
        || is(&["ant-ling"], &["api.ant-ling.com"]);
    !non_standard
}

pub fn detect_supports_strict_mode(provider: &str, base_url: &str) -> bool {
    let p = provider.to_lowercase();
    let url = base_url.to_lowercase();
    let moonshot = p == "moonshotai" || p == "moonshotai-cn" || url.contains("api.moonshot.");
    let together = p == "togetherai" || p == "together" || url.contains("api.together.");
    let cf = p == "cloudflare-ai-gateway" || url.contains("gateway.ai.cloudflare.com");
    let nvidia = p == "nvidia" || url.contains("integrate.api.nvidia.com");
    !moonshot && !together && !cf && !nvidia
}

pub fn detect_zai_tool_stream(provider: &str, base_url: &str, model_id: &str) -> bool {
    let p = provider.to_lowercase();
    let url = base_url.to_lowercase();
    let zai = ["zai", "zai-coding-plan", "zhipuai", "zhipuai-coding-plan"].contains(&p.as_str())
        || url.contains("api.z.ai")
        || url.contains("open.bigmodel.cn");
    if !zai {
        return false;
    }
    !matches!(model_id.to_lowercase().as_str(), "glm-4.5" | "glm-4.5-air" | "glm-4.5-flash" | "glm-4.5v")
}

// ---------------------------------------------------------------------------------------------
// Lowering
// ---------------------------------------------------------------------------------------------

struct Lowering<'a> {
    cc: Option<CacheControl>,
    key: String,
    model_lower: String,
    mistral: bool,
    request: &'a LLMRequest,
}

impl Lowering<'_> {
    fn cache(&mut self, hint: Option<&CacheHint>) -> Option<Value> {
        self.cc.as_mut().and_then(|cc| cc.mark(hint))
    }
    fn tool_call_id(&self, id: &str) -> String {
        if self.mistral {
            let mut s: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).take(9).collect();
            while s.chars().count() < 9 {
                s.push('0');
            }
            return s;
        }
        if self.model_lower.contains("claude") {
            return id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
        }
        let provider = self.request.model.provider.as_str();
        if provider == "openai" || provider == "azure" || self.model_lower.starts_with("openai/") {
            // JS `slice` on UTF-16 units; ids are ASCII in practice.
            return id.chars().take(40).collect();
        }
        id.to_string()
    }
}

fn text_content(text: &str, cc: Option<Value>) -> Value {
    let mut o = Object::new();
    o.insert("type".into(), json!("text"));
    o.insert("text".into(), json!(text));
    if let Some(cc) = cc {
        o.insert("cache_control".into(), cc);
    }
    Value::Object(o)
}

fn lower_media(part: &MediaPart) -> Result<Value, AIError> {
    let media_type = part.media.media_type();
    if media_type.to_lowercase() == "application/pdf" {
        let inline = shared::require_inline_media(NAME, &part.media)?;
        return Ok(json!({"type": "file", "file": {"filename": part.filename.clone().unwrap_or_else(|| "document.pdf".into()), "file_data": inline.data_url}}));
    }
    if part.media.kind() != crate::media::MediaKind::Image {
        return Err(AIError::invalid_request(format!("OpenAI Chat does not support media type {media_type}")));
    }
    let url = match part.media.url_value() {
        Some(u) => u.to_string(),
        None => shared::require_inline_media(NAME, &part.media)?.data_url,
    };
    Ok(json!({"type": "image_url", "image_url": {"url": url}}))
}

fn is_kimi_detail(detail: &Value) -> bool {
    matches!(detail.get("type").and_then(Value::as_str), Some("summary" | "encrypted"))
}

/// Decode a known reasoning detail (`ReasoningDetail` union); `None` for unknown shapes.
fn decode_detail(detail: &Value) -> Option<Value> {
    let o = detail.as_object()?;
    let kind = o.get("type")?.as_str()?;
    let opt_str = |k: &str| o.get(k).map(|v| v.is_string()).unwrap_or(true);
    let opt_str_null = |k: &str| o.get(k).map(|v| v.is_string() || v.is_null()).unwrap_or(true);
    let opt_num = |k: &str| o.get(k).map(Value::is_number).unwrap_or(true);
    match kind {
        "reasoning.text" | "reasoning.summary" | "reasoning.encrypted" => {
            let field_ok = match kind {
                "reasoning.text" => opt_str("text"),
                "reasoning.summary" => opt_str("summary"),
                _ => o.get("data").map(Value::is_string).unwrap_or(false),
            };
            if field_ok && opt_str_null("id") && opt_str("format") && opt_num("index") && opt_str_null("signature") {
                // Schema decoding re-orders declared fields first: type, <field>, id, format, index, signature, rest.
                let main = match kind {
                    "reasoning.text" => "text",
                    "reasoning.summary" => "summary",
                    _ => "data",
                };
                let mut out = Object::new();
                for k in ["type", main, "id", "format", "index", "signature"] {
                    if let Some(v) = o.get(k) {
                        out.insert(k.into(), v.clone());
                    }
                }
                for (k, v) in o {
                    if !out.contains_key(k) {
                        out.insert(k.clone(), v.clone());
                    }
                }
                Some(Value::Object(out))
            } else {
                None
            }
        }
        "summary" => {
            let summary = o.get("summary")?.as_str()?;
            if !opt_num("index") {
                return None;
            }
            let mut out = Object::new();
            out.insert("type".into(), json!("summary"));
            out.insert("summary".into(), json!(summary));
            if let Some(i) = o.get("index") {
                out.insert("index".into(), i.clone());
            }
            Some(Value::Object(out))
        }
        "encrypted" => {
            let enc = o.get("encrypted")?.as_str()?;
            if !opt_num("index") {
                return None;
            }
            let mut out = Object::new();
            out.insert("type".into(), json!("encrypted"));
            out.insert("encrypted".into(), json!(enc));
            if let Some(i) = o.get("index") {
                out.insert("index".into(), i.clone());
            }
            Some(Value::Object(out))
        }
        _ => None,
    }
}

fn known_details(details: &[Value]) -> Vec<Value> {
    details.iter().filter_map(decode_detail).collect()
}

fn lower_detail(detail: Value) -> Value {
    match detail.get("type").and_then(Value::as_str) {
        Some("summary") => json!({"type": "summary", "summary": detail["summary"].clone()}),
        Some("encrypted") => json!({"type": "encrypted", "encrypted": detail["encrypted"].clone()}),
        _ => detail,
    }
}

fn native_compat(message: &Message) -> Option<&Value> {
    message.native.as_ref().and_then(|n| n.get("openaiCompatible"))
}

fn lower_user(message: &Message, l: &mut Lowering) -> Result<Value, AIError> {
    let mut content = vec![];
    for part in &message.content {
        match part {
            ContentPart::Text(t) => {
                let cc = l.cache(t.cache.as_ref());
                content.push(text_content(&t.text, cc));
            }
            ContentPart::Media(m) => content.push(lower_media(m)?),
            _ => return Err(unsupported_content(NAME, Role::User, &["text", "media"])),
        }
    }
    if content.iter().all(|c| c.get("type").and_then(Value::as_str) == Some("text") && c.get("cache_control").is_none()) {
        let joined: String = content.iter().map(|c| c["text"].as_str().unwrap_or("").to_string()).collect();
        return Ok(json!({"role": "user", "content": joined}));
    }
    Ok(json!({"role": "user", "content": content}))
}

fn lower_assistant(message: &Message, configured: Option<&str>, require_reasoning: bool, l: &mut Lowering) -> Result<Object, AIError> {
    let mut content: Vec<Value> = vec![];
    let mut reasoning: Vec<&ReasoningPart> = vec![];
    let mut tool_calls: Vec<Value> = vec![];
    for part in &message.content {
        match part {
            ContentPart::Text(t) => {
                let cc = l.cache(t.cache.as_ref());
                content.push(text_content(&t.text, cc));
            }
            ContentPart::Reasoning(r) => reasoning.push(r),
            ContentPart::ToolCall(c) => {
                let mut o = Object::new();
                o.insert("id".into(), json!(l.tool_call_id(&c.id)));
                o.insert("type".into(), json!("function"));
                let input = if c.input.is_null() { json!({}) } else { c.input.clone() };
                o.insert("function".into(), json!({"name": c.name, "arguments": encode(&input)}));
                let extra = c.provider_metadata.as_ref().and_then(|pm| pm.get(&l.key)).and_then(|m| m.get("extraContent"));
                if let Some(sig) = extra.and_then(|e| e.get("google")).and_then(|g| g.get("thought_signature")).and_then(Value::as_str) {
                    o.insert("extra_content".into(), json!({"google": {"thought_signature": sig}}));
                }
                tool_calls.push(Value::Object(o));
            }
            _ => return Err(unsupported_content(NAME, Role::Assistant, &["text", "reasoning", "tool-call"])),
        }
    }
    let text: String = reasoning.iter().map(|r| r.text.as_str()).collect();
    let key = l.key.clone();
    let part_details = |r: &ReasoningPart| -> Option<Vec<Value>> {
        r.provider_metadata.as_ref().and_then(|pm| pm.get(&key)).and_then(|m| m.get("reasoningDetails")).and_then(Value::as_array).cloned()
    };
    let native = native_compat(message);
    let details: Option<Vec<Value>> = if reasoning.iter().any(|r| part_details(r).is_some()) {
        let observed: Vec<Value> = reasoning.iter().flat_map(|r| part_details(r).unwrap_or_default()).collect();
        Some(known_details(&observed).into_iter().map(lower_detail).collect())
    } else {
        native.and_then(|n| n.get("reasoning_details")).and_then(Value::as_array).map(|d| known_details(d).into_iter().map(lower_detail).collect())
    };
    let observed_field = reasoning.iter().find_map(|r| {
        r.provider_metadata.as_ref().and_then(|pm| pm.get(&key)).and_then(|m| m.get("reasoningField")).and_then(Value::as_str).map(String::from)
    });
    let native_reasoning = native.and_then(|n| n.get("reasoning_content")).and_then(Value::as_str).map(String::from);
    let fully_structured = reasoning.iter().all(|r| part_details(r).is_some());
    let field: Option<String> = if configured.is_some() && (require_reasoning || !reasoning.is_empty() || native_reasoning.is_some()) {
        configured.map(String::from)
    } else if reasoning.is_empty() {
        if require_reasoning { Some("reasoning_content".into()) } else { None }
    } else if observed_field.is_some() {
        observed_field
    } else if native_reasoning.is_some() || !fully_structured || require_reasoning {
        Some("reasoning_content".into())
    } else if details.as_ref().map(|d| d.iter().any(is_kimi_detail)).unwrap_or(false) {
        Some("reasoning_content".into())
    } else {
        None
    };
    let reasoning_text: Option<String> = if reasoning.is_empty() {
        native_reasoning.clone().or(if require_reasoning { Some(String::new()) } else { None })
    } else {
        Some(text)
    };
    let mut result = Object::new();
    result.insert("role".into(), json!("assistant"));
    let content_value = if content.iter().any(|c| c.get("cache_control").is_some()) {
        Value::Array(content)
    } else if content.is_empty() && !tool_calls.is_empty() {
        Value::Null
    } else {
        Value::String(content.iter().map(|c| c["text"].as_str().unwrap_or("").to_string()).collect())
    };
    result.insert("content".into(), content_value);
    if !tool_calls.is_empty() {
        result.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    if let Some(d) = details {
        result.insert("reasoning_details".into(), Value::Array(d));
    }
    if let (Some(field), Some(text)) = (field, reasoning_text) {
        result.insert(field, json!(text));
    }
    Ok(result)
}

fn tool_message(id: String, text: String, cc: Option<Value>) -> Value {
    match cc {
        None => json!({"role": "tool", "tool_call_id": id, "content": text}),
        Some(cc) => json!({"role": "tool", "tool_call_id": id, "content": [text_content(&text, Some(cc))]}),
    }
}

fn lower_tool_messages(message: &Message, l: &mut Lowering) -> Result<(Vec<Value>, Vec<Value>), AIError> {
    let mut messages = vec![];
    let mut attachments = vec![];
    for part in &message.content {
        let ContentPart::ToolResult(r) = part else {
            return Err(unsupported_content(NAME, Role::Tool, &["tool-result"]));
        };
        let id = l.tool_call_id(&r.id);
        match &r.result {
            ToolResultValue::Content(items) => {
                let (texts, files) = shared::content_text_and_files(items);
                let cc = l.cache(r.cache.as_ref());
                messages.push(tool_message(id, texts.join("\n"), cc));
                for (uri, mime, name) in files {
                    attachments.push(lower_media(&tool_file_media(uri, mime, name))?);
                }
            }
            other => {
                let cc = l.cache(r.cache.as_ref());
                messages.push(tool_message(id, tool_result_text(other), cc));
            }
        }
    }
    Ok((messages, attachments))
}

const MISTRAL_FAMILIES: &[&str] = &["mistral", "devstral", "codestral", "pixtral", "mixtral"];

fn lower_messages(request: &LLMRequest, l: &mut Lowering, hook: Option<&AssistantHook>) -> Result<Vec<Value>, AIError> {
    let mut messages: Vec<Value> = vec![];
    if !request.system.is_empty() {
        if request.system.iter().any(|p| p.cache.is_some()) && l.cc.is_some() {
            let parts: Vec<Value> = request.system.iter().map(|p| { let cc = l.cache(p.cache.as_ref()); text_content(&p.text, cc) }).collect();
            messages.push(json!({"role": "system", "content": parts}));
        } else {
            messages.push(json!({"role": "system", "content": shared::join_text(request.system.iter().map(|p| p.text.as_str()))}));
        }
    }
    let compat = request.compat();
    let model_lower = request.model.id.to_lowercase();
    let require_reasoning = compat.require_reasoning.unwrap_or_else(|| {
        compat.reasoning_field.is_some()
            || request.model.provider == "deepseek"
            || request.base_url().to_lowercase().contains("deepseek.com")
            || model_lower.contains("deepseek")
    });
    let configured = compat.reasoning_field.clone();
    let require_assistant_after_tool = compat.require_assistant_after_tool.unwrap_or(l.mistral);
    let mut pending: Vec<Value> = vec![];

    fn bridge(messages: &mut Vec<Value>, enabled: bool) {
        if enabled && messages.last().and_then(|m| m.get("role")).and_then(Value::as_str) == Some("tool") {
            messages.push(json!({"role": "assistant", "content": "Done."}));
        }
    }
    fn flush(messages: &mut Vec<Value>, pending: &mut Vec<Value>, enabled: bool) {
        if pending.is_empty() {
            return;
        }
        bridge(messages, enabled);
        messages.push(json!({"role": "user", "content": std::mem::take(pending)}));
    }

    for message in &request.messages {
        if message.role == Role::User {
            bridge(&mut messages, require_assistant_after_tool);
        }
        match message.role {
            Role::System => {
                let part = wrapped_system_update(NAME, message)?;
                let cc = l.cache(part.cache.as_ref());
                if !pending.is_empty() {
                    let mut content = std::mem::take(&mut pending);
                    content.push(text_content(&part.text, cc));
                    messages.push(json!({"role": "user", "content": content}));
                    continue;
                }
                let previous = messages.last().filter(|m| m.get("role").and_then(Value::as_str) == Some("user")).cloned();
                match previous.as_ref().and_then(|p| p.get("content")) {
                    Some(Value::String(prev)) => {
                        let replacement = match cc {
                            Some(cc) => json!({"role": "user", "content": [text_content(prev, None), text_content(&part.text, Some(cc))]}),
                            None => json!({"role": "user", "content": format!("{prev}\n{}", part.text)}),
                        };
                        *messages.last_mut().unwrap() = replacement;
                    }
                    Some(Value::Array(prev)) => {
                        let mut content = prev.clone();
                        content.push(text_content(&part.text, cc));
                        *messages.last_mut().unwrap() = json!({"role": "user", "content": content});
                    }
                    _ => messages.push(match cc {
                        Some(cc) => json!({"role": "user", "content": [text_content(&part.text, Some(cc))]}),
                        None => json!({"role": "user", "content": part.text}),
                    }),
                }
            }
            Role::Assistant if message.content.iter().all(|p| matches!(p, ContentPart::Text(t) if t.text.trim().is_empty())) => {}
            Role::Tool => {
                let (lowered, attachments) = lower_tool_messages(message, l)?;
                messages.extend(lowered);
                pending.extend(attachments);
            }
            Role::User => {
                flush(&mut messages, &mut pending, require_assistant_after_tool);
                messages.push(lower_user(message, l)?);
            }
            Role::Assistant => {
                flush(&mut messages, &mut pending, require_assistant_after_tool);
                let mut lowered = lower_assistant(message, configured.as_deref(), require_reasoning, l)?;
                if let Some(hook) = hook {
                    hook(message, &mut lowered);
                }
                messages.push(Value::Object(lowered));
            }
        }
    }
    flush(&mut messages, &mut pending, require_assistant_after_tool);
    Ok(messages)
}

fn has_tool_history(messages: &[Message]) -> bool {
    messages.iter().any(|m| m.role == Role::Tool || (m.role == Role::Assistant && m.content.iter().any(|p| matches!(p, ContentPart::ToolCall(_)))))
}

/// Build the ordered OpenAI Chat body fields.
pub fn body_fields(request: &LLMRequest, opts: &ChatOptions) -> Result<Fields, AIError> {
    let compat = request.compat();
    if let Some(field) = &compat.reasoning_field {
        if RESERVED_REASONING_FIELDS.contains(&field.as_str()) {
            return Err(AIError::invalid_request(format!("OpenAI Chat reasoning field conflicts with reserved field {field}")));
        }
    }
    let generation = request.generation_options();
    let (tools_flat, flattened) = flatten_tool_request(request);
    let provider = request.model.provider.clone();
    let base_url = request.base_url();
    let max_tokens_field = compat.max_tokens_field.clone().unwrap_or_else(|| detect_max_tokens_field(&provider, &base_url).into());
    let supports_store = compat.supports_store.unwrap_or_else(|| detect_supports_store(&provider, &base_url));
    let supports_usage = compat.supports_usage_in_streaming.unwrap_or(true);
    let supports_strict = compat.supports_strict_mode.unwrap_or_else(|| detect_supports_strict_mode(&provider, &base_url));
    let zai_tool_stream = compat.zai_tool_stream.unwrap_or_else(|| detect_zai_tool_stream(&provider, &base_url, &request.model.id));
    let has_history = has_tool_history(&request.messages);
    let active = !tools_flat.is_empty();
    let model_lower = request.model.id.to_lowercase();
    let mut l = Lowering {
        cc: if opts.cache_control { Some(CacheControl::default()) } else { None },
        key: request.metadata_key(),
        mistral: MISTRAL_FAMILIES.iter().any(|f| model_lower.contains(f)),
        model_lower,
        request,
    };
    let messages = lower_messages(&flattened, &mut l, opts.assistant.as_ref())?;
    let tools = if tools_flat.is_empty() {
        if has_history { Some(json!([])) } else { None }
    } else {
        Some(Value::Array(
            tools_flat
                .iter()
                .map(|t| {
                    let mut f = Object::new();
                    f.insert("name".into(), json!(t.name));
                    f.insert("description".into(), json!(t.description));
                    f.insert("parameters".into(), Value::Object(t.input_schema.clone()));
                    if supports_strict {
                        f.insert("strict".into(), json!(false));
                    }
                    let mut o = Object::new();
                    o.insert("type".into(), json!("function"));
                    o.insert("function".into(), Value::Object(f));
                    if let Some(cc) = l.cache(t.cache.as_ref()) {
                        o.insert("cache_control".into(), cc);
                    }
                    Value::Object(o)
                })
                .collect(),
        ))
    };
    let tool_choice = match (&request.tool_choice, active) {
        (Some(choice), true) => Some(match match_tool_choice(NAME, choice)? {
            ToolChoiceMode::Auto => json!("auto"),
            ToolChoiceMode::None => json!("none"),
            ToolChoiceMode::Required => json!("required"),
            ToolChoiceMode::Tool(name) => json!({"type": "function", "function": {"name": name}}),
        }),
        _ => None,
    };
    let mut f = Fields::new();
    f.insert("model".into(), Some(json!(request.model.id)));
    f.insert("messages".into(), Some(Value::Array(messages)));
    f.insert("tools".into(), tools);
    f.insert("tool_choice".into(), tool_choice);
    f.insert("stream".into(), Some(json!(true)));
    if supports_usage {
        f.insert("stream_options".into(), Some(json!({"include_usage": true})));
    }
    if zai_tool_stream && active {
        f.insert("tool_stream".into(), Some(json!(true)));
    }
    let field = if max_tokens_field == "max_completion_tokens" { "max_completion_tokens" } else { "max_tokens" };
    f.insert(field.into(), generation.max_tokens.map(num));
    f.insert("temperature".into(), generation.temperature.map(num));
    f.insert("top_p".into(), generation.top_p.map(num));
    f.insert("frequency_penalty".into(), generation.frequency_penalty.map(num));
    f.insert("presence_penalty".into(), generation.presence_penalty.map(num));
    f.insert("seed".into(), generation.seed.map(num));
    f.insert("stop".into(), generation.stop.clone().map(|s| json!(s)));
    let po = options::resolve(request);
    if supports_store {
        f.insert("store".into(), Some(json!(po.store.unwrap_or(false))));
    }
    if compat.supports_prompt_cache_key.unwrap_or(false) {
        if let Some(key) = prompt_cache_key(request).filter(|k| !k.is_empty()) {
            f.insert("prompt_cache_key".into(), Some(json!(key)));
        }
    }
    if let Some(effort) = po.reasoning_effort.filter(|e| !e.is_empty()) {
        f.insert("reasoning_effort".into(), Some(json!(effort)));
    }
    Ok(f)
}

// ---------------------------------------------------------------------------------------------
// Protocol + routes
// ---------------------------------------------------------------------------------------------

pub struct OpenAIChat {
    pub options: ChatOptions,
}

impl Protocol for OpenAIChat {
    fn id(&self) -> &str {
        ADAPTER
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        Ok(finalize(body_fields(request, &self.options)?))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(ChatParser::new(request))
    }
}

pub fn protocol() -> Arc<dyn Protocol> {
    Arc::new(OpenAIChat { options: ChatOptions::default() })
}

pub fn protocol_with(options: ChatOptions) -> Arc<dyn Protocol> {
    Arc::new(OpenAIChat { options })
}

/// `openai-chat` route.
pub fn route() -> Route {
    Route::new(ADAPTER, protocol(), Endpoint::new(Some(DEFAULT_BASE_URL), PATH), Framing::SseWithDone)
        .initial_provider("openai", Some("openai"))
}

/// `openai-compatible-chat` route (no provider, no default baseURL; metadata key `openai`, re-keyed when a
/// provider is set via [`Route::provider`]).
pub fn compatible_route() -> Route {
    Route::new("openai-compatible-chat", protocol(), Endpoint::new(None, PATH), Framing::SseWithDone).metadata_key("openai")
}

// ---------------------------------------------------------------------------------------------
// Stream parsing
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct PendingDelta {
    id: Option<String>,
    name: Option<String>,
    input: String,
    extra: Option<Value>,
}

/// Hooks for wrapper parsers (Venice).
pub trait ChatStreamHooks: Send {
    /// Rewrite the scalar reasoning delta text before emission (`None` hides it).
    fn reasoning_text(&mut self, _field: &str, text: &str) -> Option<String> {
        Some(text.to_string())
    }
    /// Extra provider metadata merged into reasoning-end.
    fn reasoning_end_metadata(&mut self) -> Option<Object> {
        None
    }
}

struct NoHooks;
impl ChatStreamHooks for NoHooks {}

pub struct ChatParser {
    key: String,
    label: String,
    tools: ToolStream,
    pending: BTreeMap<i64, PendingDelta>,
    tool_call_events: Vec<LLMEvent>,
    usage: Option<Usage>,
    finish: Option<FinishReasonDetails>,
    lifecycle: Lifecycle,
    reasoning_field: Option<String>,
    reasoning_text_observed: bool,
    details: Vec<Value>,
    details_observed: bool,
    reasoning_emitted: bool,
    latest_tool_index: Option<i64>,
    next_tool_index: i64,
    require_finish_reason: bool,
    pub hooks: Box<dyn ChatStreamHooks>,
    pub error_issue_suffix: bool,
}

pub fn map_usage(usage: &Value, key: &str) -> Option<Usage> {
    if !usage.is_object() {
        return None;
    }
    let u = |path: &[&str]| crate::json::path(usage, path).and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)));
    let input = u(&["prompt_tokens"]);
    let output = u(&["completion_tokens"]);
    let cached = u(&["prompt_tokens_details", "cached_tokens"])
        .or_else(|| u(&["prompt_cache_hit_tokens"]))
        .or_else(|| u(&["cached_tokens"]))
        .or_else(|| u(&["cache_read_input_tokens"]));
    let write = u(&["prompt_tokens_details", "cache_write_tokens"]).or_else(|| u(&["cache_created_input_tokens"]));
    let reasoning = u(&["completion_tokens_details", "reasoning_tokens"]);
    let non_cached = shared::subtract_tokens(input, shared::sum_tokens(&[cached, write]));
    let mut pm = Object::new();
    pm.insert(key.into(), usage.clone());
    Some(Usage {
        context_tokens: None,
        input_tokens: input,
        output_tokens: output,
        non_cached_input_tokens: non_cached,
        cache_read_input_tokens: cached,
        cache_write_input_tokens: write,
        reasoning_tokens: reasoning,
        total_tokens: shared::total_tokens(input, output, u(&["total_tokens"])),
        provider_metadata: Some(pm),
    })
}

fn map_finish_reason(event: &Value, reason: &str) -> Result<FinishReason, AIError> {
    Ok(match reason {
        "error" => {
            return Err(AIError::unknown_provider("Provider reported an error (finish_reason: error)").with_body(encode(event)));
        }
        "network_error" => {
            return Err(AIError::provider_internal("Provider reported a network error (finish_reason: network_error)").with_body(encode(event)));
        }
        "stop" | "end" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        "function_call" | "tool_calls" => FinishReason::ToolCalls,
        other => return Err(AIError::unknown_provider(format!("Provider finish_reason: {other}")).with_body(encode(event))),
    })
}

fn detail_text(details: &[Value], hide_kimi: bool) -> Option<String> {
    let parts: Vec<String> = details
        .iter()
        .filter_map(|d| {
            let s = |k: &str| d.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
            match d.get("type").and_then(Value::as_str) {
                Some("reasoning.text") => s("text"),
                Some("reasoning.summary") => s("summary"),
                Some("summary") if !hide_kimi => s("summary"),
                _ => None,
            }
        })
        .collect();
    if parts.is_empty() { None } else { Some(parts.concat()) }
}

fn conflicting(prev: Option<&Value>, cur: Option<&Value>) -> bool {
    match (prev, cur) {
        (Some(p), Some(c)) if !p.is_null() && !c.is_null() => p != c,
        _ => false,
    }
}

fn truthy_str(v: Option<&Value>) -> bool {
    v.map(crate::json::truthy).unwrap_or(false)
}

fn merge_value(prev: Option<&Value>, cur: Option<&Value>) -> Option<Value> {
    if truthy_str(prev) {
        return prev.cloned();
    }
    if truthy_str(cur) {
        return cur.cloned();
    }
    if prev.is_some() { prev.cloned() } else { cur.cloned() }
}

fn merge_details(prev: &Value, detail: &Value) -> Option<Value> {
    let g = |v: &Value, k: &str| v.get(k).cloned();
    if conflicting(prev.get("id"), detail.get("id"))
        || conflicting(prev.get("index"), detail.get("index"))
        || conflicting(prev.get("format"), detail.get("format"))
        || (truthy_str(prev.get("signature")) && truthy_str(detail.get("signature")) && prev.get("signature") != detail.get("signature"))
    {
        return None;
    }
    let pt = prev.get("type").and_then(Value::as_str);
    let dt = detail.get("type").and_then(Value::as_str);
    let spread = |field: &str| -> Value {
        let mut out = prev.as_object().cloned().unwrap_or_default();
        for (k, v) in detail.as_object().cloned().unwrap_or_default() {
            out.insert(k, v);
        }
        let joined = format!(
            "{}{}",
            prev.get(field).and_then(Value::as_str).unwrap_or(""),
            detail.get(field).and_then(Value::as_str).unwrap_or("")
        );
        out.insert(field.into(), json!(joined));
        Value::Object(out)
    };
    match (pt, dt) {
        (Some("reasoning.text"), Some("reasoning.text")) | (Some("reasoning.summary"), Some("reasoning.summary")) => {
            let field = if pt == Some("reasoning.text") { "text" } else { "summary" };
            let mut out = spread(field);
            let o = out.as_object_mut().unwrap();
            if let Some(sig) = merge_value(g(prev, "signature").as_ref(), g(detail, "signature").as_ref()) {
                o.insert("signature".into(), sig);
            }
            if let Some(fmt) = merge_value(g(prev, "format").as_ref(), g(detail, "format").as_ref()) {
                o.insert("format".into(), fmt);
            }
            Some(out)
        }
        (Some("summary"), Some("summary")) => Some(spread("summary")),
        _ => None,
    }
}

fn append_details(result: &mut Vec<Value>, details: Vec<Value>) {
    for detail in details {
        let merged = result.last().and_then(|p| merge_details(p, &detail));
        match merged {
            Some(m) => *result.last_mut().unwrap() = m,
            None => result.push(detail),
        }
    }
}

impl ChatParser {
    pub fn new(request: &LLMRequest) -> Self {
        let compat = request.compat();
        Self {
            key: request.metadata_key(),
            label: route_label(request),
            tools: ToolStream::new(ADAPTER),
            pending: BTreeMap::new(),
            tool_call_events: vec![],
            usage: None,
            finish: None,
            lifecycle: Lifecycle::new(),
            reasoning_field: compat.reasoning_field.clone(),
            reasoning_text_observed: false,
            details: vec![],
            details_observed: false,
            reasoning_emitted: false,
            latest_tool_index: None,
            next_tool_index: 0,
            require_finish_reason: compat.require_finish_reason.unwrap_or(true),
            hooks: Box::new(NoHooks),
            error_issue_suffix: false,
        }
    }

    pub fn with_hooks(mut self, hooks: Box<dyn ChatStreamHooks>) -> Self {
        self.hooks = hooks;
        self
    }

    fn reasoning_metadata(&self, field: Option<&str>, details: Option<Vec<Value>>) -> ProviderMetadata {
        let mut inner = Object::new();
        if let Some(f) = field {
            inner.insert("reasoningField".into(), json!(f));
        }
        if let Some(d) = details {
            inner.insert("reasoningDetails".into(), Value::Array(d));
        }
        let mut pm = Object::new();
        pm.insert(self.key.clone(), Value::Object(inner));
        pm
    }

    fn tool_index_by_id(&self, id: Option<&str>) -> Option<i64> {
        let id = id?;
        let mut keys: BTreeMap<i64, Option<String>> = BTreeMap::new();
        for (k, p) in &self.pending {
            keys.insert(*k, p.id.clone());
        }
        for (k, t) in &self.tools.tools {
            if let super::tool_stream::StreamKey::Index(i) = k {
                keys.insert(*i, Some(t.id.clone()));
            }
        }
        keys.into_iter().find(|(_, v)| v.as_deref() == Some(id)).map(|(k, _)| k)
    }

    /// Tool id at `index` (or the latest tool index), from started or pending tools.
    pub fn tool_id(&self, index: Option<i64>) -> Option<String> {
        let index = index.or(self.latest_tool_index)?;
        self.tool_at(index).map(|t| t.id.clone()).or_else(|| self.pending.get(&index).and_then(|p| p.id.clone()))
    }

    fn tool_at(&self, index: i64) -> Option<&super::tool_stream::PendingTool> {
        self.tools.get(&index.into())
    }

    pub fn handle(&mut self, event: &Value) -> Result<Vec<LLMEvent>, AIError> {
        if let Some(error) = event.get("error").filter(|e| !e.is_null()) {
            let message = match error {
                Value::String(s) => s.clone(),
                other => other.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
            };
            let status = error.get("code").and_then(Value::as_u64).map(|c| c as u16);
            return Err(classify_provider_failure(ProviderFailure { message, status, raw_body: Some(encode(event)), ..Default::default() }));
        }
        let mut events = vec![];
        let choice = event.get("choices").and_then(Value::as_array).and_then(|c| c.first());
        let usage = event
            .get("usage")
            .filter(|u| u.is_object())
            .and_then(|u| map_usage(u, &self.key))
            .or_else(|| choice.and_then(|c| c.get("usage")).filter(|u| u.is_object()).and_then(|u| map_usage(u, &self.key)))
            .or_else(|| self.usage.clone());
        let raw_finish = choice.and_then(|c| c.get("finish_reason")).and_then(Value::as_str).filter(|s| !s.is_empty());
        let finish = match raw_finish {
            Some(raw) => Some(FinishReasonDetails::raw(
                map_finish_reason(event, raw)?,
                Some(choice.and_then(|c| c.get("native_finish_reason")).and_then(Value::as_str).unwrap_or(raw).to_string()),
            )),
            None => self.finish.clone(),
        };
        let delta = choice.and_then(|c| c.get("delta")).filter(|d| d.is_object());
        let dstr = |k: &str| delta.and_then(|d| d.get(k)).and_then(Value::as_str).filter(|s| !s.is_empty());
        let tool_deltas: Vec<Value> = delta.and_then(|d| d.get("tool_calls")).and_then(Value::as_array).cloned().unwrap_or_default();
        let reasoning = {
            let mut fields: Vec<String> = vec![];
            for f in [self.reasoning_field.clone(), Some("reasoning_content".into()), Some("reasoning".into()), Some("reasoning_text".into())]
                .into_iter()
                .flatten()
            {
                if !fields.contains(&f) {
                    fields.push(f);
                }
            }
            fields.into_iter().find_map(|f| dstr(&f).map(|t| (f.clone(), t.to_string())))
        };
        let detail_array = delta.and_then(|d| d.get("reasoning_details")).and_then(Value::as_array);
        let has_late = dstr("content").is_some()
            || dstr("refusal").is_some()
            || reasoning.is_some()
            || detail_array.map(|a| !a.is_empty()).unwrap_or(false)
            || tool_deltas.iter().any(|t| {
                crate::json::truthy(t.get("id").unwrap_or(&Value::Null))
                    || t.get("function").map(|f| truthy_str(f.get("name")) || truthy_str(f.get("arguments"))).unwrap_or(false)
            });
        if self.finish.is_some() {
            if has_late {
                return Err(AIError::invalid_output(ADAPTER, "OpenAI Chat received content after the finish reason").with_body(encode(event)));
            }
            self.usage = usage;
            return Ok(events);
        }
        if self.reasoning_field.is_none() {
            self.reasoning_field = reasoning.as_ref().map(|(f, _)| f.clone());
        }
        if reasoning.is_some() {
            self.reasoning_text_observed = true;
        }
        let detail_delta = detail_array.map(|a| known_details(a));
        if let Some(d) = &detail_delta {
            append_details(&mut self.details, d.clone());
            self.details_observed = true;
        }
        let delta_metadata = self.reasoning_metadata(self.reasoning_field.clone().as_deref(), None);
        let text = match &detail_delta {
            Some(d) if !d.is_empty() => detail_text(d, self.reasoning_text_observed).or_else(|| reasoning.as_ref().map(|r| r.1.clone())),
            _ => reasoning.as_ref().map(|r| r.1.clone()),
        };
        let text = match (text, &reasoning) {
            (Some(t), Some((field, raw))) if &t == raw => self.hooks.reasoning_text(field, &t),
            (t, _) => t,
        };
        if let Some(t) = text.filter(|t| !t.is_empty()) {
            self.lifecycle.reasoning_delta(&mut events, "reasoning-0", &t, Some(delta_metadata.clone()));
        } else if self.details_observed
            && !self.lifecycle.is_reasoning_open("reasoning-0")
            && (dstr("content").is_some() || dstr("refusal").is_some() || !tool_deltas.is_empty())
        {
            self.lifecycle.reasoning_start(&mut events, "reasoning-0", Some(delta_metadata.clone()));
        }
        if self.lifecycle.is_reasoning_open("reasoning-0") {
            self.reasoning_emitted = true;
        }
        if let Some(c) = dstr("content") {
            self.lifecycle.text_delta(&mut events, "text-0", c, None);
        }
        if let Some(r) = dstr("refusal") {
            self.lifecycle.text_delta(&mut events, "text-0", r, None);
        }
        let count = tool_deltas.len();
        for (position, tool) in tool_deltas.iter().enumerate() {
            let tid = tool.get("id").and_then(Value::as_str).filter(|s| !s.is_empty());
            let matched = self.tool_index_by_id(tid);
            let fallback = if count > 1 { position as i64 } else { self.latest_tool_index.unwrap_or(position as i64) };
            let fallback_id = self.tool_at(fallback).map(|t| t.id.clone()).or_else(|| self.pending.get(&fallback).and_then(|p| p.id.clone()));
            let index = tool.get("index").and_then(Value::as_i64).or(matched).unwrap_or_else(|| match (tid, &fallback_id) {
                (Some(id), Some(fid)) if !fid.is_empty() && fid != id => self.next_tool_index,
                _ => fallback,
            });
            let current = self.tool_at(index).cloned();
            let pending = self.pending.get(&index).cloned();
            let fname = tool.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).filter(|s| !s.is_empty());
            let fargs = tool.get("function").and_then(|f| f.get("arguments")).and_then(Value::as_str).unwrap_or("");
            let id = current.as_ref().map(|c| c.id.clone()).or_else(|| pending.as_ref().and_then(|p| p.id.clone())).or(tid.map(String::from));
            let name = current.as_ref().map(|c| c.name.clone()).or_else(|| pending.as_ref().and_then(|p| p.name.clone())).or(fname.map(String::from));
            let text = format!("{}{}", pending.as_ref().map(|p| p.input.as_str()).unwrap_or(""), fargs);
            let extra = pending.as_ref().and_then(|p| p.extra.clone()).or_else(|| {
                let sig = tool.get("extra_content").and_then(|e| e.get("google")).and_then(|g| g.get("thought_signature")).and_then(Value::as_str)?;
                Some(json!({"google": {"thought_signature": sig}}))
            });
            self.latest_tool_index = Some(index);
            self.next_tool_index = self.next_tool_index.max(index + 1);
            let id = id.filter(|s| !s.is_empty());
            let name = name.filter(|s| !s.is_empty());
            if current.is_none() && (id.is_none() || name.is_none()) {
                self.pending.insert(index, PendingDelta { id, name, input: text, extra });
                continue;
            }
            if pending.is_some() {
                self.pending.remove(&index);
            }
            let pm = extra.map(|e| {
                let mut inner = Object::new();
                inner.insert("extraContent".into(), e);
                let mut pm = Object::new();
                pm.insert(self.key.clone(), Value::Object(inner));
                pm
            });
            let mut out = vec![];
            self.tools
                .append_or_start(index, id.as_deref(), name.as_deref(), &text, pm, "OpenAI Chat tool call delta is missing id or name", &mut out)
                .map_err(|e| e.with_body(encode(event)))?;
            if !out.is_empty() {
                self.lifecycle.step_start(&mut events);
            }
            events.extend(out);
        }
        let incomplete = matches!(finish.as_ref().map(|f| f.normalized), Some(FinishReason::ContentFilter | FinishReason::Length));
        if finish.is_some() && !incomplete && !self.pending.is_empty() {
            return Err(AIError::invalid_output(ADAPTER, "OpenAI Chat tool call delta is missing id or name").with_body(encode(event)));
        }
        if finish.is_some() && !incomplete && !self.tools.is_empty() {
            let mut out = vec![];
            self.tools.finish_all(&mut out)?;
            self.tool_call_events = out;
        }
        self.usage = usage;
        self.finish = finish;
        Ok(events)
    }

    pub fn finish_events(&mut self) -> Result<Vec<LLMEvent>, AIError> {
        if self.finish.is_none() && self.require_finish_reason {
            return Err(AIError::incomplete_stream(ADAPTER, "OpenAI Chat stream ended without finish_reason"));
        }
        let mut events = vec![];
        let tool_call_events = if self.finish.is_none() && !self.tools.is_empty() {
            let mut out = vec![];
            self.tools.finish_all(&mut out)?;
            out
        } else {
            std::mem::take(&mut self.tool_call_events)
        };
        let has_tool_calls = !tool_call_events.is_empty();
        let reason = match &self.finish {
            Some(f) => {
                let mut f = f.clone();
                if f.normalized == FinishReason::Stop && has_tool_calls {
                    f.normalized = FinishReason::ToolCalls;
                }
                f
            }
            None => FinishReasonDetails::new(if has_tool_calls { FinishReason::ToolCalls } else { FinishReason::Stop }),
        };
        let mut metadata = self.reasoning_metadata(
            self.reasoning_field.clone().as_deref(),
            if self.details_observed { Some(self.details.clone()) } else { None },
        );
        if let Some(extra) = self.hooks.reasoning_end_metadata() {
            if let Some(inner) = metadata.get_mut(&self.key).and_then(Value::as_object_mut) {
                for (k, v) in extra {
                    inner.insert(k, v);
                }
            }
        }
        if self.details_observed && !self.reasoning_emitted {
            let m = self.reasoning_metadata(self.reasoning_field.clone().as_deref(), None);
            self.lifecycle.reasoning_start(&mut events, "reasoning-0", Some(m));
        }
        self.lifecycle.reasoning_end(&mut events, "reasoning-0", Some(metadata), None);
        if has_tool_calls {
            self.lifecycle.step_start(&mut events);
        }
        events.extend(tool_call_events);
        self.lifecycle.finish(&mut events, reason, self.usage.clone(), None);
        Ok(events)
    }
}

impl StreamParser for ChatParser {
    fn step(&mut self, frame: &Frame) -> Result<Step, AIError> {
        if let Frame::Text(t) = frame {
            if t == "[DONE]" {
                return Ok(Step::terminal(vec![]));
            }
        }
        let event = decode_json_frame(ADAPTER, &self.label, frame)?;
        if !event.is_object() {
            return Err(AIError::invalid_output(ADAPTER, format!("Invalid {} stream event", self.label)).with_body(encode(&event)));
        }
        Ok(Step::events(self.handle(&event)?))
    }
    fn on_halt(&mut self) -> Result<Vec<LLMEvent>, AIError> {
        self.finish_events()
    }
}

