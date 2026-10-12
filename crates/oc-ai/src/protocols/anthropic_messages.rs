//! Anthropic Messages protocol and derivatives (`protocols/anthropic-messages.ts`,
//! `alibaba-messages.ts`, `zai-messages.ts`, `meta-messages.ts`, vertex messages body).

use super::lifecycle::Lifecycle;
use super::shared::{
    self, claude_version, fit_thinking_budget, flatten_tool_request, match_tool_choice, supports_thinking_block_binding,
    tool_file_media, tool_result_text, unsupported_content, wrapped_system_update, Breakpoints, ToolChoiceMode,
    THINKING_BINDING_BETA,
};
use super::tool_stream::{PendingTool, ToolStream};
use super::{decode_json_frame, route_label, Protocol, Step, StreamParser};
use crate::compile::{effort_update, resolve_effort_updates};
use crate::error::{classify_provider_failure, AIError, ProviderFailure};
use crate::json::{encode, num, Object};
use crate::route::framing::{Frame, Framing};
use crate::route::{Endpoint, Headers, Route};
use crate::types::*;
use base64::Engine;
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

const ADAPTER: &str = "anthropic-messages";
const NAME: &str = "Anthropic Messages";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";
pub const PATH: &str = "/messages";
pub const DEFAULT_MAX_TOKENS: f64 = 32_000.0;
const MIN_THINKING_BUDGET: f64 = 1_024.0;
const DEFAULT_EFFORT: &str = "high";
const BREAKPOINT_CAP: u32 = 4;

pub const SSE_EVENTS: &[&str] = &[
    "message",
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
    "ping",
    "error",
];

pub fn framing() -> Framing {
    Framing::SseEvents(SSE_EVENTS.iter().map(|s| s.to_string()).collect())
}

/// Ordered body fields; `None` = JS `undefined` (key slot kept, omitted on encode).
pub type Fields = IndexMap<String, Option<Value>>;

/// Drop undefined fields.
pub fn finalize(fields: Fields) -> Value {
    Value::Object(fields.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect())
}

fn invalid(message: impl Into<String>) -> AIError {
    AIError::invalid_request(message)
}

fn options_error(field: &str) -> AIError {
    invalid(format!("Invalid Anthropic Messages provider option `{field}`"))
}

// ---------------------------------------------------------------------------------------------
// Provider options decoding
// ---------------------------------------------------------------------------------------------

fn decode_block_binding(value: Option<&Value>) -> Result<Option<Value>, AIError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(o)) => {
            let mut out = Object::new();
            match o.get("prefix_mismatch_behavior") {
                None => {}
                Some(Value::String(s)) => {
                    out.insert("prefix_mismatch_behavior".into(), Value::String(s.clone()));
                }
                Some(_) => return Err(options_error("thinking")),
            }
            Ok(Some(Value::Object(out)))
        }
        Some(_) => Err(options_error("thinking")),
    }
}

fn opt_string(o: &Object, key: &str, field: &str) -> Result<Option<Value>, AIError> {
    match o.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(Value::String(s.clone()))),
        Some(_) => Err(options_error(field)),
    }
}

/// Decode `thinking` into its wire form (ordered).
pub fn decode_thinking(value: &Value) -> Result<Object, AIError> {
    let o = value.as_object().ok_or_else(|| options_error("thinking"))?;
    let kind = o.get("type").and_then(Value::as_str).ok_or_else(|| options_error("thinking"))?;
    let mut out = Object::new();
    match kind {
        "enabled" => {
            let budget = o
                .get("budgetTokens")
                .filter(|v| v.is_number())
                .or_else(|| o.get("budget_tokens").filter(|v| v.is_number()))
                .ok_or_else(|| options_error("thinking"))?;
            out.insert("type".into(), json!("enabled"));
            out.insert("budget_tokens".into(), budget.clone());
            if let Some(d) = opt_string(o, "display", "thinking")? {
                out.insert("display".into(), d);
            }
            if let Some(b) = decode_block_binding(o.get("block_binding"))? {
                out.insert("block_binding".into(), b);
            }
        }
        "adaptive" => {
            out.insert("type".into(), json!("adaptive"));
            if let Some(d) = opt_string(o, "display", "thinking")? {
                out.insert("display".into(), d);
            }
            if let Some(b) = decode_block_binding(o.get("block_binding"))? {
                out.insert("block_binding".into(), b);
            }
        }
        "disabled" | "between_tools" => {
            out.insert("type".into(), json!(kind));
        }
        _ => return Err(options_error("thinking")),
    }
    Ok(out)
}

fn is_thinking_off(value: Option<&Value>) -> bool {
    value
        .and_then(|v| v.as_object())
        .map(|o| matches!(o.get("type").and_then(Value::as_str), Some("disabled" | "between_tools")) && o.len() >= 1)
        .unwrap_or(false)
}

fn decode_cache_control(value: &Value, field: &str) -> Result<Value, AIError> {
    let o = value.as_object().ok_or_else(|| options_error(field))?;
    if o.get("type").and_then(Value::as_str) != Some("ephemeral") {
        return Err(options_error(field));
    }
    let mut out = Object::new();
    out.insert("type".into(), json!("ephemeral"));
    match o.get("ttl") {
        None => {}
        Some(Value::String(t)) if t == "5m" || t == "1h" => {
            out.insert("ttl".into(), Value::String(t.clone()));
        }
        Some(_) => return Err(options_error(field)),
    }
    Ok(Value::Object(out))
}

fn decode_output_config(value: &Value, field: &str) -> Result<(Option<String>, Option<Value>), AIError> {
    let o = value.as_object().ok_or_else(|| options_error(field))?;
    let effort = match o.get("effort") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(options_error(field)),
    };
    let format = match o.get("format") {
        None | Some(Value::Null) => None,
        Some(Value::Object(f)) => {
            if f.get("type").and_then(Value::as_str) != Some("json_schema") || !f.get("schema").map(Value::is_object).unwrap_or(false) {
                return Err(options_error(field));
            }
            Some(json!({"type": "json_schema", "schema": f["schema"].clone()}))
        }
        Some(_) => return Err(options_error(field)),
    };
    Ok((effort, format))
}

#[derive(Debug, Default, Clone)]
struct Options {
    context_management: Option<Value>,
    thinking: Option<Object>,
    effort: Option<String>,
    service_tier: Option<Value>,
    metadata: Option<Value>,
    container: Option<Value>,
    inference_geo: Option<Value>,
    cache_control: Option<Value>,
    output_effort: Option<String>,
    output_format: Option<Value>,
}

fn decode_context_management(value: &Value) -> Result<Value, AIError> {
    let err = || options_error("contextManagement");
    let edits = value.get("edits").and_then(Value::as_array).ok_or_else(err)?;
    let mut out = vec![];
    for edit in edits {
        let e = edit.as_object().ok_or_else(err)?;
        if e.get("type").and_then(Value::as_str) != Some("compact_20260112") {
            return Err(err());
        }
        let mut item = Object::new();
        item.insert("type".into(), json!("compact_20260112"));
        if let Some(trigger) = e.get("trigger") {
            let t = trigger.as_object().ok_or_else(err)?;
            let value = t.get("value").and_then(Value::as_f64).ok_or_else(err)?;
            if t.get("type").and_then(Value::as_str) != Some("input_tokens") || value.fract() != 0.0 || value < 50000.0 {
                return Err(err());
            }
            item.insert("trigger".into(), json!({"type": "input_tokens", "value": num(value)}));
        }
        if let Some(p) = e.get("pauseAfterCompaction") {
            if !p.is_boolean() {
                return Err(err());
            }
            item.insert("pause_after_compaction".into(), p.clone());
        }
        if let Some(i) = e.get("instructions") {
            if !i.is_string() {
                return Err(err());
            }
            item.insert("instructions".into(), i.clone());
        }
        out.push(Value::Object(item));
    }
    Ok(json!({ "edits": out }))
}

fn decode_options(po: Option<&Object>) -> Result<Options, AIError> {
    let mut out = Options::default();
    let Some(po) = po else { return Ok(out) };
    let get = |k: &str| po.get(k).filter(|v| !v.is_null());
    if let Some(v) = get("contextManagement") {
        out.context_management = Some(decode_context_management(v)?);
    }
    if let Some(v) = get("thinking") {
        out.thinking = Some(decode_thinking(v)?);
    }
    if let Some(v) = get("effort") {
        out.effort = Some(v.as_str().ok_or_else(|| options_error("effort"))?.to_string());
    }
    for key in ["service_tier", "serviceTier"] {
        if let Some(v) = get(key) {
            if !v.is_string() {
                return Err(options_error(key));
            }
        }
    }
    out.service_tier = get("service_tier").or_else(|| get("serviceTier")).cloned();
    if let Some(v) = get("metadata") {
        let o = v.as_object().ok_or_else(|| options_error("metadata"))?;
        let mut m = Object::new();
        if let Some(u) = o.get("user_id") {
            if !(u.is_string() || u.is_null()) {
                return Err(options_error("metadata"));
            }
            m.insert("user_id".into(), u.clone());
        }
        out.metadata = Some(Value::Object(m));
    }
    if let Some(v) = get("container") {
        match v {
            Value::String(_) => out.container = Some(v.clone()),
            Value::Object(o) => {
                let mut c = Object::new();
                if let Some(id) = o.get("id") {
                    c.insert("id".into(), id.clone());
                }
                if let Some(skills) = o.get("skills") {
                    c.insert("skills".into(), skills.clone());
                }
                out.container = Some(Value::Object(c));
            }
            _ => return Err(options_error("container")),
        }
    }
    out.inference_geo = get("inference_geo").or_else(|| get("inferenceGeo")).cloned();
    let cc = get("cache_control").map(|v| decode_cache_control(v, "cache_control")).transpose()?;
    let cc2 = get("cacheControl").map(|v| decode_cache_control(v, "cacheControl")).transpose()?;
    out.cache_control = cc.or(cc2);
    let oc = get("output_config").map(|v| decode_output_config(v, "output_config")).transpose()?;
    let oc2 = get("outputConfig").map(|v| decode_output_config(v, "outputConfig")).transpose()?;
    if let Some((effort, format)) = oc.or(oc2) {
        out.output_effort = effort;
        out.output_format = format;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Lowering
// ---------------------------------------------------------------------------------------------

fn cache_control(bp: &mut Breakpoints, cache: Option<&CacheHint>) -> Option<Value> {
    let cache = cache?;
    if bp.remaining == 0 {
        bp.dropped += 1;
        return None;
    }
    bp.remaining -= 1;
    Some(if shared::ttl_bucket(cache.ttl_seconds) == Some("1h") {
        json!({"type": "ephemeral", "ttl": "1h"})
    } else {
        json!({"type": "ephemeral"})
    })
}

fn with_cc(mut block: Object, cc: Option<Value>) -> Value {
    if let Some(cc) = cc {
        block.insert("cache_control".into(), cc);
    }
    Value::Object(block)
}

fn text_block(text: &str, cc: Option<Value>) -> Value {
    let mut o = Object::new();
    o.insert("type".into(), json!("text"));
    o.insert("text".into(), json!(text));
    with_cc(o, cc)
}

pub fn scrub_tool_call_id(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

fn metadata_signature(pm: Option<&ProviderMetadata>, key: &str, field: &str) -> Option<String> {
    pm?.get(key)?.as_object()?.get(field)?.as_str().map(String::from)
}

fn meta_obj<'a>(metadata: Option<&'a Object>) -> Option<&'a Object> {
    metadata
}

fn file_id(metadata: Option<&Object>) -> Option<String> {
    let m = meta_obj(metadata)?;
    if let Some(a) = m.get("anthropic").and_then(Value::as_object) {
        if let Some(s) = a.get("file_id").and_then(Value::as_str) {
            return Some(s.into());
        }
        if let Some(s) = a.get("fileId").and_then(Value::as_str) {
            return Some(s.into());
        }
    }
    m.get("file_id").and_then(Value::as_str).or_else(|| m.get("fileId").and_then(Value::as_str)).map(String::from)
}

fn transformations(metadata: Option<&Object>) -> Option<Value> {
    let m = meta_obj(metadata)?;
    let anthropic = m.get("anthropic").and_then(Value::as_object);
    let raw = anthropic.and_then(|a| a.get("transformations")).filter(|v| !v.is_null()).or_else(|| m.get("transformations"));
    if let Some(r) = raw.and_then(Value::as_object) {
        if let Some(v) = r.get("oversized_image").and_then(Value::as_str) {
            if v == "downsize" || v == "error" {
                return Some(json!({"oversized_image": v}));
            }
        }
    }
    if let Some(v) = anthropic.and_then(|a| a.get("oversized_image")).and_then(Value::as_str) {
        if v == "downsize" || v == "error" {
            return Some(json!({"oversized_image": v}));
        }
    }
    None
}

fn document_title(part: &MediaPart) -> Option<String> {
    if let Some(m) = part.metadata.as_ref() {
        if let Some(t) = m.get("anthropic").and_then(Value::as_object).and_then(|a| a.get("title")).and_then(Value::as_str) {
            return Some(t.into());
        }
        if let Some(t) = m.get("title").and_then(Value::as_str) {
            return Some(t.into());
        }
    }
    part.filename.clone().filter(|f| !f.is_empty())
}

fn document_context(metadata: Option<&Object>) -> Option<String> {
    let m = metadata?;
    if let Some(c) = m.get("anthropic").and_then(Value::as_object).and_then(|a| a.get("context")).and_then(Value::as_str) {
        return Some(c.into());
    }
    m.get("context").and_then(Value::as_str).map(String::from)
}

fn citations(metadata: Option<&Object>) -> Option<Value> {
    let m = metadata?;
    let raw = match m.get("anthropic").and_then(Value::as_object) {
        Some(a) => a.get("citations").filter(|v| !v.is_null()).or_else(|| m.get("citations")),
        None => m.get("citations"),
    };
    let enabled = raw?.as_object()?.get("enabled")?.as_bool()?;
    Some(json!({ "enabled": enabled }))
}

fn image_block(source: Value, cc: Option<Value>, part: &MediaPart) -> Value {
    let mut o = Object::new();
    o.insert("type".into(), json!("image"));
    o.insert("source".into(), source);
    if let Some(cc) = cc {
        o.insert("cache_control".into(), cc);
    }
    if let Some(t) = transformations(part.metadata.as_ref()) {
        o.insert("transformations".into(), t);
    }
    Value::Object(o)
}

fn document_block(source: Value, cc: Option<Value>, part: &MediaPart) -> Value {
    let mut o = Object::new();
    o.insert("type".into(), json!("document"));
    o.insert("source".into(), source);
    if let Some(cc) = cc {
        o.insert("cache_control".into(), cc);
    }
    if let Some(t) = document_title(part) {
        o.insert("title".into(), json!(t));
    }
    if let Some(c) = document_context(part.metadata.as_ref()) {
        o.insert("context".into(), json!(c));
    }
    if let Some(c) = citations(part.metadata.as_ref()) {
        o.insert("citations".into(), c);
    }
    Value::Object(o)
}

fn lower_media(part: &MediaPart, bp: Option<&mut Breakpoints>) -> Result<Value, AIError> {
    let media_type = part.media.media_type();
    let mime = media_type.to_lowercase();
    let cc = match bp {
        Some(bp) => cache_control(bp, part.cache.as_ref()),
        None => None,
    };
    if let Some(id) = file_id(part.metadata.as_ref()) {
        let source = json!({"type": "file", "file_id": id});
        return Ok(if mime.starts_with("image/") { image_block(source, cc, part) } else { document_block(source, cc, part) });
    }
    if let Some(url) = part.media.url_value().map(str::trim) {
        let lower = url.to_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            if mime.starts_with("image/") {
                return Ok(image_block(json!({"type": "url", "url": url}), cc, part));
            }
            if mime == "application/pdf" {
                return Ok(document_block(json!({"type": "url", "url": url}), cc, part));
            }
        }
    }
    let inline = shared::require_inline_media(NAME, &part.media)?;
    if mime == "text/plain" {
        let bytes = base64::engine::general_purpose::STANDARD.decode(inline.base64.trim()).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        return Ok(document_block(json!({"type": "text", "media_type": "text/plain", "data": text}), cc, part));
    }
    if inline.mime == "application/pdf" {
        return Ok(document_block(json!({"type": "base64", "media_type": "application/pdf", "data": inline.base64}), cc, part));
    }
    if !inline.mime.starts_with("image/") {
        return Err(invalid(format!("Anthropic Messages does not support media type {media_type}")));
    }
    Ok(image_block(json!({"type": "base64", "media_type": inline.mime, "data": inline.base64}), cc, part))
}

fn lower_tool_result_content(part: &ToolResultPart) -> Result<Value, AIError> {
    match &part.result {
        ToolResultValue::Content(items) => {
            let mut out = vec![];
            for item in items {
                match item {
                    ToolContent::Text { text } => out.push(json!({"type": "text", "text": text})),
                    ToolContent::File { uri, mime, name } => {
                        out.push(lower_media(&tool_file_media(uri, mime, name.as_deref()), None)?);
                    }
                }
            }
            Ok(Value::Array(out))
        }
        other => Ok(Value::String(tool_result_text(other))),
    }
}

fn require_thinking_signature(request: &LLMRequest) -> bool {
    if let Some(v) = request.model.compatibility.as_ref().and_then(|c| c.require_signature) {
        return v;
    }
    let provider = request.model.provider.to_lowercase();
    let model = request.model.id.to_lowercase();
    let base = request.base_url().to_lowercase();
    if provider == "kimi-for-coding"
        || provider == "moonshotai"
        || provider == "moonshotai-cn"
        || model.starts_with("kimi-")
        || base.contains("api.kimi.com/coding")
        || base.contains("api.moonshot.ai/anthropic")
        || base.contains("api.moonshot.cn/anthropic")
    {
        return false;
    }
    if provider.contains("xiaomi") || model.contains("mimo") || base.contains("xiaomimimo.com") {
        return false;
    }
    true
}

fn supports_native_system_updates(request: &LLMRequest) -> bool {
    match claude_version(&request.model.id) {
        None => false,
        Some(v) if v.family == "opus" && v.major == 4 => v.minor >= 8,
        Some(v) => v.major >= 5,
    }
}

fn accepts_native_system_after(message: Option<&Value>) -> bool {
    let Some(m) = message else { return false };
    match m.get("role").and_then(Value::as_str) {
        Some("user") => true,
        Some("assistant") => {
            m.get("content").and_then(Value::as_array).and_then(|c| c.last()).and_then(|b| b.get("type")).and_then(Value::as_str)
                == Some("server_tool_use")
        }
        _ => false,
    }
}

fn splits_local_tool_results(messages: &[Message], index: usize) -> bool {
    let mut pending: Vec<&str> = vec![];
    for message in &messages[..index] {
        for part in &message.content {
            if message.role == Role::Assistant {
                if let ContentPart::ToolCall(c) = part {
                    if c.provider_executed != Some(true) {
                        pending.push(&c.id);
                    }
                }
            }
            if message.role == Role::Tool {
                if let ContentPart::ToolResult(r) = part {
                    pending.retain(|id| *id != r.id);
                }
            }
        }
    }
    !pending.is_empty()
}

fn append_to_user_turn(messages: &mut Vec<Value>, block: Value) {
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some("user") {
            if let Some(content) = last.get_mut("content").and_then(Value::as_array_mut) {
                content.push(block);
                return;
            }
        }
    }
    messages.push(json!({"role": "user", "content": [block]}));
}

fn lower_messages(request: &LLMRequest, bp: &mut Breakpoints) -> Result<Vec<Value>, AIError> {
    let mut messages: Vec<Value> = vec![];
    let key = request.metadata_key();
    let hold = supports_native_system_updates(request);
    let mut held: Vec<&Message> = vec![];

    fn release(held: &mut Vec<&Message>, messages: &mut Vec<Value>, bp: &mut Breakpoints) -> Result<(), AIError> {
        let native = accepts_native_system_after(messages.iter().rev().find(|m| m.get("role").and_then(Value::as_str) != Some("system")));
        for update in held.drain(..) {
            if native {
                let content = shared::system_update_text(NAME, update)?;
                let blocks: Vec<Value> = content.iter().map(|p| text_block(&p.text, cache_control(bp, p.cache.as_ref()))).collect();
                messages.push(json!({"role": "system", "content": blocks}));
            } else {
                let part = wrapped_system_update(NAME, update)?;
                let block = text_block(&part.text, cache_control(bp, part.cache.as_ref()));
                append_to_user_turn(messages, block);
            }
        }
        Ok(())
    }

    for (index, message) in request.messages.iter().enumerate() {
        match message.role {
            Role::System => {
                if let Some(update) = effort_update(message) {
                    messages.push(json!({"role": "system", "content": [], "output_config": {"effort": update.effort.clone().unwrap_or_else(|| DEFAULT_EFFORT.into())}}));
                    continue;
                }
                if splits_local_tool_results(&request.messages, index) {
                    return Err(invalid("Anthropic Messages system updates cannot split a local tool call from its tool result"));
                }
                if hold {
                    held.push(message);
                } else {
                    let part = wrapped_system_update(NAME, message)?;
                    let block = text_block(&part.text, cache_control(bp, part.cache.as_ref()));
                    append_to_user_turn(&mut messages, block);
                }
            }
            Role::User => {
                let mut content = vec![];
                for part in &message.content {
                    match part {
                        ContentPart::Text(t) => {
                            if t.text.trim().is_empty() {
                                continue;
                            }
                            content.push(text_block(&t.text, cache_control(bp, t.cache.as_ref())));
                        }
                        ContentPart::Media(m) => content.push(lower_media(m, Some(bp))?),
                        _ => return Err(unsupported_content(NAME, Role::User, &["text", "media"])),
                    }
                }
                if !content.is_empty() {
                    messages.push(json!({"role": "user", "content": content}));
                }
            }
            Role::Assistant => {
                let mut content = vec![];
                for part in &message.content {
                    match part {
                        ContentPart::Compaction(c) => {
                            if c.provider != request.model.provider || c.text.is_none() {
                                return Err(invalid("Compaction state must be replayed to its originating provider and API"));
                            }
                            content.push(json!({"type": "compaction", "content": c.text.clone().unwrap()}));
                        }
                        ContentPart::Text(t) => {
                            if t.text.trim().is_empty() {
                                continue;
                            }
                            content.push(text_block(&t.text, cache_control(bp, t.cache.as_ref())));
                        }
                        ContentPart::Reasoning(r) => {
                            let signature = r.encrypted.clone().or_else(|| metadata_signature(r.provider_metadata.as_ref(), &key, "signature"));
                            let redacted = metadata_signature(r.provider_metadata.as_ref(), &key, "redactedData");
                            if signature.is_none() {
                                if let Some(data) = redacted {
                                    content.push(json!({"type": "redacted_thinking", "data": data}));
                                    continue;
                                }
                            }
                            match signature.filter(|s| !s.trim().is_empty()) {
                                None => {
                                    if r.text.trim().is_empty() {
                                        continue;
                                    }
                                    if !require_thinking_signature(request) {
                                        content.push(json!({"type": "thinking", "thinking": r.text, "signature": ""}));
                                        continue;
                                    }
                                    content.push(text_block(&r.text, cache_control(bp, r.cache.as_ref())));
                                }
                                Some(sig) => content.push(json!({"type": "thinking", "thinking": r.text, "signature": sig})),
                            }
                        }
                        ContentPart::ToolCall(c) => {
                            let kind = if c.provider_executed == Some(true) { "server_tool_use" } else { "tool_use" };
                            let mut o = Object::new();
                            o.insert("type".into(), json!(kind));
                            o.insert("id".into(), json!(scrub_tool_call_id(&c.id)));
                            o.insert("name".into(), json!(c.name));
                            o.insert("input".into(), c.input.clone());
                            content.push(Value::Object(o));
                        }
                        ContentPart::ToolResult(r) if r.provider_executed == Some(true) => {
                            let wire = match r.name.as_str() {
                                "web_search" => "web_search_tool_result",
                                "code_execution" => "code_execution_tool_result",
                                "web_fetch" => "web_fetch_tool_result",
                                _ => {
                                    return Err(invalid(format!(
                                        "Anthropic Messages does not know how to round-trip server tool result for {}",
                                        r.name
                                    )));
                                }
                            };
                            let payload = r
                                .provider_metadata
                                .as_ref()
                                .and_then(|pm| pm.get(&key))
                                .and_then(|m| m.get("result"))
                                .filter(|v| !v.is_null())
                                .cloned()
                                .unwrap_or_else(|| result_value(&r.result));
                            content.push(json!({"type": wire, "tool_use_id": scrub_tool_call_id(&r.id), "content": payload}));
                        }
                        _ => {
                            return Err(invalid(
                                "Anthropic Messages assistant messages only support text, reasoning, and tool-call content for now",
                            ));
                        }
                    }
                }
                if content.is_empty() {
                    continue;
                }
                release(&mut held, &mut messages, bp)?;
                messages.push(json!({"role": "assistant", "content": content}));
            }
            Role::Tool => {
                let mut content = vec![];
                for part in &message.content {
                    let ContentPart::ToolResult(r) = part else {
                        return Err(unsupported_content(NAME, Role::Tool, &["tool-result"]));
                    };
                    let mut o = Object::new();
                    o.insert("type".into(), json!("tool_result"));
                    o.insert("tool_use_id".into(), json!(scrub_tool_call_id(&r.id)));
                    o.insert("content".into(), lower_tool_result_content(r)?);
                    if matches!(r.result, ToolResultValue::Error(_)) {
                        o.insert("is_error".into(), json!(true));
                    }
                    if let Some(cc) = cache_control(bp, r.cache.as_ref()) {
                        o.insert("cache_control".into(), cc);
                    }
                    content.push(Value::Object(o));
                }
                let merge = messages
                    .last()
                    .map(|m| {
                        m.get("role").and_then(Value::as_str) == Some("user")
                            && m.get("content")
                                .and_then(Value::as_array)
                                .map(|c| c.iter().all(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")))
                                .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if merge {
                    messages.last_mut().unwrap()["content"].as_array_mut().unwrap().extend(content);
                } else {
                    messages.push(json!({"role": "user", "content": content}));
                }
            }
        }
    }
    release(&mut held, &mut messages, bp)?;
    Ok(messages)
}

fn result_value(result: &ToolResultValue) -> Value {
    match result {
        ToolResultValue::Json(v) | ToolResultValue::Text(v) | ToolResultValue::Error(v) => v.clone(),
        ToolResultValue::Content(c) => serde_json::to_value(c).unwrap_or(Value::Null),
    }
}

/// `supportsEffortUpdates`.
pub fn supports_effort_updates(request: &LLMRequest) -> bool {
    if is_thinking_off(request.po("thinking")) {
        return false;
    }
    if let Some(v) = request.model.compatibility.as_ref().and_then(|c| c.supports_effort_updates) {
        return v;
    }
    match claude_version(&request.model.id) {
        None => false,
        Some(v) if v.family == "opus" && v.major >= 5 => true,
        Some(v) => v.major > 5 || (v.major == 5 && v.minor >= 1),
    }
}

fn thinking_off_obj(t: &Object) -> bool {
    matches!(t.get("type").and_then(Value::as_str), Some("disabled" | "between_tools"))
}

fn apply_thinking_binding_default(request: &LLMRequest, thinking: Option<Object>) -> Option<Object> {
    if let Some(t) = &thinking {
        if thinking_off_obj(t) {
            return thinking;
        }
    }
    if !supports_thinking_block_binding(request) {
        return thinking;
    }
    let mut out = thinking.clone().unwrap_or_else(|| {
        let mut o = Object::new();
        o.insert("type".into(), json!("adaptive"));
        o
    });
    let mut binding = Object::new();
    binding.insert("prefix_mismatch_behavior".into(), json!("drop_block"));
    if let Some(existing) = thinking.as_ref().and_then(|t| t.get("block_binding")).and_then(Value::as_object) {
        for (k, v) in existing {
            binding.insert(k.clone(), v.clone());
        }
    }
    out.insert("block_binding".into(), Value::Object(binding));
    Some(out)
}

fn fit_thinking(thinking: Option<Object>, max_tokens: f64) -> Option<Object> {
    let mut t = thinking?;
    if t.get("type").and_then(Value::as_str) == Some("enabled") {
        if let Some(b) = t.get("budget_tokens").and_then(Value::as_f64) {
            t.insert("budget_tokens".into(), num(fit_thinking_budget(b, Some(max_tokens), MIN_THINKING_BUDGET)));
        }
    }
    Some(t)
}

fn lower_tool_choice(choice: &ToolChoice) -> Result<Value, AIError> {
    let mode = match_tool_choice(NAME, choice)?;
    let mut o = Object::new();
    match mode {
        ToolChoiceMode::Auto => {
            o.insert("type".into(), json!("auto"));
        }
        ToolChoiceMode::None => {
            o.insert("type".into(), json!("none"));
            return Ok(Value::Object(o));
        }
        ToolChoiceMode::Required => {
            o.insert("type".into(), json!("any"));
        }
        ToolChoiceMode::Tool(name) => {
            o.insert("type".into(), json!("tool"));
            o.insert("name".into(), json!(name));
        }
    }
    if let Some(d) = choice.disable_parallel_tool_use {
        o.insert("disable_parallel_tool_use".into(), json!(d));
    }
    Ok(Value::Object(o))
}

/// Lower a request into ordered body fields.
pub fn body_fields(request: &LLMRequest) -> Result<Fields, AIError> {
    let options = decode_options(request.provider_options.as_ref())?;
    let (messages_after_updates, effort) =
        resolve_effort_updates(request, options.effort.clone().or_else(|| options.output_effort.clone()));
    let mut updated = request.clone();
    updated.messages = messages_after_updates;
    let generation = request.generation_options();
    let mut bp = Breakpoints::new(BREAKPOINT_CAP);
    let (tools_flat, flattened) = flatten_tool_request(&updated);
    let tools: Option<Vec<Value>> = if tools_flat.is_empty() {
        None
    } else {
        Some(
            tools_flat
                .iter()
                .map(|t| {
                    let mut o = Object::new();
                    o.insert("name".into(), json!(t.name));
                    o.insert("description".into(), json!(t.description));
                    o.insert("input_schema".into(), Value::Object(t.input_schema.clone()));
                    with_cc(o, cache_control(&mut bp, t.cache.as_ref()))
                })
                .collect(),
        )
    };
    let tool_choice = match (&tools, &request.tool_choice) {
        (Some(_), Some(choice)) => Some(lower_tool_choice(choice)?),
        _ => None,
    };
    let system_parts: Vec<&SystemPart> = request.system.iter().filter(|p| !p.text.is_empty()).collect();
    let system = if system_parts.is_empty() {
        None
    } else {
        Some(Value::Array(system_parts.iter().map(|p| text_block(&p.text, cache_control(&mut bp, p.cache.as_ref()))).collect()))
    };
    let messages = lower_messages(&flattened, &mut bp)?;
    if bp.dropped > 0 {
        tracing::warn!(
            "Anthropic Messages: dropped {} cache breakpoint(s); the API allows at most {} per request.",
            bp.dropped,
            BREAKPOINT_CAP
        );
    }
    let output_config = if effort.is_none() && options.output_format.is_none() {
        None
    } else {
        let mut o = Object::new();
        if let Some(e) = &effort {
            o.insert("effort".into(), json!(e));
        }
        if let Some(f) = &options.output_format {
            o.insert("format".into(), f.clone());
        }
        Some(Value::Object(o))
    };
    let max_tokens = generation.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let thinking = apply_thinking_binding_default(request, fit_thinking(options.thinking.clone(), max_tokens));
    let mut f = Fields::new();
    f.insert("model".into(), Some(json!(request.model.id)));
    f.insert("system".into(), system);
    f.insert("messages".into(), Some(Value::Array(messages)));
    f.insert("tools".into(), tools.map(Value::Array));
    f.insert("tool_choice".into(), tool_choice);
    f.insert("stream".into(), Some(json!(true)));
    f.insert("max_tokens".into(), Some(num(max_tokens)));
    f.insert("temperature".into(), generation.temperature.map(num));
    f.insert("top_p".into(), generation.top_p.map(num));
    f.insert("top_k".into(), generation.top_k.map(num));
    f.insert("stop_sequences".into(), generation.stop.clone().map(|s| json!(s)));
    f.insert("thinking".into(), thinking.map(Value::Object));
    f.insert("output_config".into(), output_config);
    f.insert("cache_control".into(), options.cache_control.clone());
    f.insert("container".into(), options.container.clone());
    f.insert("inference_geo".into(), options.inference_geo.clone());
    f.insert("metadata".into(), options.metadata.clone());
    f.insert("service_tier".into(), options.service_tier.clone());
    if let Some(cm) = options.context_management {
        f.insert("context_management".into(), Some(cm));
    }
    Ok(f)
}

/// Betas required by a body (`requiredBetaHeaders`).
pub fn required_betas(body: &Value) -> Vec<&'static str> {
    let mut betas = vec!["interleaved-thinking-2025-05-14"];
    let requests_compaction =
        body.get("context_management").and_then(|c| c.get("edits")).and_then(Value::as_array).map(|e| !e.is_empty()).unwrap_or(false);
    let messages = body.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let replays = messages.iter().any(|m| {
        m.get("content").and_then(Value::as_array).map(|c| c.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("compaction"))).unwrap_or(false)
    });
    if requests_compaction || replays {
        betas.push("compact-2026-01-12");
    }
    if messages.iter().any(|m| m.get("role").and_then(Value::as_str) == Some("system") && m.get("output_config").is_some()) {
        betas.push("mid-conversation-output-config-2026-07-01");
    }
    if let Some(t) = body.get("thinking").and_then(Value::as_object) {
        if !thinking_off_obj(t) && t.get("block_binding").is_some() {
            betas.push(THINKING_BINDING_BETA);
        }
    }
    betas
}

/// Merge required betas into an existing `anthropic-beta` header.
pub fn merge_beta_header(headers: &mut Headers, betas: &[&str]) {
    if betas.is_empty() {
        return;
    }
    let mut all: Vec<String> = headers
        .get("anthropic-beta")
        .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    for b in betas {
        if !all.iter().any(|x| x == b) {
            all.push(b.to_string());
        }
    }
    let mut dedup: Vec<String> = vec![];
    for b in all {
        if !dedup.contains(&b) {
            dedup.push(b);
        }
    }
    headers.insert("anthropic-beta".into(), dedup.join(","));
}

/// Add the Anthropic beta transport hook to a route.
pub fn beta_prepare_headers(route: Route) -> Route {
    route.prepare_headers(|_, body, headers| merge_beta_header(headers, &required_betas(body)))
}

// ---------------------------------------------------------------------------------------------
// Protocols
// ---------------------------------------------------------------------------------------------

pub struct AnthropicMessages;

impl Protocol for AnthropicMessages {
    fn id(&self) -> &str {
        ADAPTER
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        Ok(finalize(body_fields(request)?))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(Parser::new(request))
    }
    fn supports_effort_updates(&self, request: &LLMRequest) -> bool {
        supports_effort_updates(request)
    }
}

pub fn protocol() -> Arc<dyn Protocol> {
    Arc::new(AnthropicMessages)
}

/// Canonical `anthropic-messages` route (auth configured by provider factories).
pub fn route() -> Route {
    let endpoint = Endpoint::dynamic(Some(DEFAULT_BASE_URL), |request, _| {
        if request.model.provider == "anthropic" { format!("{PATH}?beta=true") } else { PATH.to_string() }
    });
    beta_prepare_headers(
        Route::new(ADAPTER, protocol(), endpoint, framing())
            .initial_provider("anthropic", Some("anthropic"))
            .headers_fn(|_| {
                let mut h = Headers::new();
                h.insert("anthropic-version".into(), "2023-06-01".into());
                h
            }),
    )
}

fn without_po_key(request: &LLMRequest, key: &str) -> LLMRequest {
    let mut r = request.clone();
    if let Some(po) = r.provider_options.as_mut() {
        po.shift_remove(key);
    }
    r
}

/// `alibaba-messages`.
pub struct AlibabaMessages;

impl Protocol for AlibabaMessages {
    fn id(&self) -> &str {
        "alibaba-messages"
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        let thinking = match request.po("thinking") {
            None => None,
            Some(v) => {
                let o = v.as_object().ok_or_else(|| options_error("thinking"))?;
                let kind = o.get("type").and_then(Value::as_str).ok_or_else(|| options_error("thinking"))?.to_string();
                let budget = o.get("budgetTokens").filter(|v| !v.is_null()).or_else(|| o.get("budget_tokens").filter(|v| !v.is_null()));
                let budget = match budget {
                    None => None,
                    Some(b) => {
                        let f = b.as_f64().filter(|f| f.fract() == 0.0).ok_or_else(|| options_error("thinking"))?;
                        Some(f)
                    }
                };
                Some((kind, budget))
            }
        };
        if let Some(e) = request.po("effort") {
            if !e.is_string() {
                return Err(options_error("effort"));
            }
        }
        let mut fields = body_fields(&without_po_key(request, "thinking"))?;
        let max_tokens = fields.get("max_tokens").and_then(|v| v.as_ref()).and_then(Value::as_f64);
        let thinking = thinking.map(|(kind, budget)| {
            let mut o = Object::new();
            o.insert("type".into(), json!(kind));
            if let Some(b) = budget {
                o.insert("budget_tokens".into(), num(fit_thinking_budget(b, max_tokens, 1.0)));
            }
            Value::Object(o)
        });
        fields.insert("thinking".into(), thinking);
        Ok(finalize(fields))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(Parser::new(request))
    }
}

pub fn alibaba_protocol() -> Arc<dyn Protocol> {
    Arc::new(AlibabaMessages)
}

/// `zai-messages`: thinking replaced by PO.thinking `{type}`.
pub struct ZaiMessages;

impl Protocol for ZaiMessages {
    fn id(&self) -> &str {
        "zai-messages"
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        let thinking = match request.po("thinking") {
            None => None,
            Some(v) => {
                let kind = v.get("type").and_then(Value::as_str).ok_or_else(|| options_error("thinking"))?;
                Some(json!({ "type": kind }))
            }
        };
        let mut fields = body_fields(&without_po_key(request, "thinking"))?;
        fields.insert("thinking".into(), thinking);
        Ok(finalize(fields))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(Parser::new(request))
    }
}

pub fn zai_protocol() -> Arc<dyn Protocol> {
    Arc::new(ZaiMessages)
}

/// `meta-messages`: native `meta` web search tools.
pub struct MetaMessages;

impl Protocol for MetaMessages {
    fn id(&self) -> &str {
        "meta-messages"
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        let (flat, projected) = flatten_tool_request(request);
        let mut fields = body_fields(&projected)?;
        if let Some(Some(Value::Array(tools))) = fields.get_mut("tools") {
            for (index, tool) in tools.iter_mut().enumerate() {
                let Some(native) = flat.get(index).and_then(|t| t.native.as_ref()) else { continue };
                let search = native.get("meta").and_then(Value::as_object).ok_or_else(|| invalid("Invalid Meta web search tool"))?;
                if search.get("type").and_then(Value::as_str) != Some("web_search") {
                    return Err(invalid("Invalid Meta web search tool"));
                }
                if search.get("search_context_size").map(|v| !v.is_null()).unwrap_or(false) {
                    return Err(invalid("Meta Messages does not support searchContextSize"));
                }
                let mut o = Object::new();
                o.insert("type".into(), json!("web_search"));
                o.insert("name".into(), json!("web_search"));
                if let Some(loc) = search.get("user_location") {
                    o.insert("user_location".into(), loc.clone());
                }
                *tool = Value::Object(o);
            }
        }
        Ok(finalize(fields))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(Parser::new(request))
    }
    fn supports_effort_updates(&self, _request: &LLMRequest) -> bool {
        false
    }
}

pub fn meta_protocol() -> Arc<dyn Protocol> {
    Arc::new(MetaMessages)
}

/// Google Vertex Messages body: drop `model`, append `anthropic_version`.
pub struct VertexMessages;

pub const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";

impl Protocol for VertexMessages {
    fn id(&self) -> &str {
        ADAPTER
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        let mut fields = body_fields(request)?;
        fields.shift_remove("model");
        fields.insert("anthropic_version".into(), Some(json!(VERTEX_ANTHROPIC_VERSION)));
        Ok(finalize(fields))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(Parser::new(request))
    }
    fn supports_effort_updates(&self, request: &LLMRequest) -> bool {
        supports_effort_updates(request)
    }
}

pub fn vertex_protocol() -> Arc<dyn Protocol> {
    Arc::new(VertexMessages)
}

// ---------------------------------------------------------------------------------------------
// Stream parsing
// ---------------------------------------------------------------------------------------------

fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" | "pause_turn" => FinishReason::Stop,
        "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        "refusal" => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

fn u(v: &Value, key: &str) -> Option<u64> {
    crate::json::get_u64(v, key)
}

fn map_usage(usage: Option<&Value>, key: &str) -> Option<Usage> {
    let usage = usage.filter(|u| u.is_object())?;
    let iterations: Vec<&Value> = match usage.get("iterations").and_then(Value::as_array) {
        Some(items) if !items.is_empty() => items.iter().collect(),
        _ => vec![usage],
    };
    let last = usage.get("iterations").and_then(Value::as_array).and_then(|a| a.last());
    let sum = |field: &str| shared::sum_tokens(&iterations.iter().map(|i| u(i, field)).collect::<Vec<_>>());
    let non_cached = sum("input_tokens");
    let cache_read = sum("cache_read_input_tokens");
    let cache_write = sum("cache_creation_input_tokens");
    let input = shared::sum_tokens(&[non_cached, cache_read, cache_write]);
    let output = sum("output_tokens");
    let reasoning = shared::sum_tokens(
        &iterations.iter().map(|i| i.get("output_tokens_details").and_then(|d| u(d, "thinking_tokens"))).collect::<Vec<_>>(),
    );
    let context = last.filter(|l| l.get("type").and_then(Value::as_str) == Some("message")).and_then(|l| {
        shared::sum_tokens(&[u(l, "input_tokens"), u(l, "cache_read_input_tokens"), u(l, "cache_creation_input_tokens")])
    });
    let mut pm = Object::new();
    pm.insert(key.into(), usage.clone());
    Some(Usage {
        context_tokens: context,
        input_tokens: input,
        output_tokens: output,
        non_cached_input_tokens: non_cached,
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: cache_write,
        reasoning_tokens: reasoning,
        total_tokens: shared::total_tokens(input, output, None),
        provider_metadata: Some(pm),
    })
}

fn merge_usage(left: Option<Usage>, right: Option<Usage>, key: &str) -> Option<Usage> {
    let Some(left) = left else { return right };
    let Some(right) = right else { return Some(left) };
    let non_cached = right.non_cached_input_tokens.or(left.non_cached_input_tokens);
    let cache_read = right.cache_read_input_tokens.or(left.cache_read_input_tokens);
    let cache_write = right.cache_write_input_tokens.or(left.cache_write_input_tokens);
    let input = shared::sum_tokens(&[non_cached, cache_read, cache_write]);
    let output = right.output_tokens.or(left.output_tokens);
    let l = left.provider_metadata.as_ref().and_then(|p| p.get(key)).and_then(Value::as_object);
    let r = right.provider_metadata.as_ref().and_then(|p| p.get(key)).and_then(Value::as_object);
    let merged = crate::json::merge_records(&[l, r]).unwrap_or_default();
    let mut pm = Object::new();
    pm.insert(key.into(), Value::Object(merged));
    Some(Usage {
        context_tokens: right.context_tokens.or(left.context_tokens),
        input_tokens: input,
        output_tokens: output,
        non_cached_input_tokens: non_cached,
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: cache_write,
        reasoning_tokens: right.reasoning_tokens.or(left.reasoning_tokens),
        total_tokens: shared::total_tokens(input, output, None),
        provider_metadata: Some(pm),
    })
}

fn pm(key: &str, value: Value) -> ProviderMetadata {
    let mut o = Object::new();
    o.insert(key.into(), value);
    o
}

fn server_tool_name(kind: &str) -> Option<&'static str> {
    match kind {
        "web_search_tool_result" => Some("web_search"),
        "code_execution_tool_result" => Some("code_execution"),
        "web_fetch_tool_result" => Some("web_fetch"),
        _ => None,
    }
}

const STREAM_BLOCK_TYPES: &[&str] = &["compaction", "text", "thinking", "redacted_thinking", "tool_use", "server_tool_use"];
const STREAM_DELTA_TYPES: &[&str] = &["compaction_delta", "text_delta", "thinking_delta", "signature_delta", "input_json_delta"];

/// Loose validation of a stream block (`AnthropicStreamBlock`).
fn valid_block(block: &Value) -> bool {
    let Some(o) = block.as_object() else { return false };
    if !o.get("type").map(Value::is_string).unwrap_or(false) {
        return false;
    }
    for key in ["id", "name", "text", "thinking", "signature", "data", "tool_use_id"] {
        if let Some(v) = o.get(key) {
            if !v.is_string() {
                return false;
            }
        }
    }
    true
}

fn valid_delta(delta: &Value) -> bool {
    let Some(o) = delta.as_object() else { return false };
    for key in ["type", "text", "thinking", "partial_json", "signature"] {
        if let Some(v) = o.get(key) {
            if !v.is_string() {
                return false;
            }
        }
    }
    for key in ["content", "stop_reason", "stop_sequence"] {
        if let Some(v) = o.get(key) {
            if !(v.is_string() || v.is_null()) {
                return false;
            }
        }
    }
    if let Some(sd) = o.get("stop_details") {
        if !(sd.is_null() || sd.is_object()) {
            return false;
        }
    }
    true
}

pub struct Parser {
    provider: String,
    key: String,
    label: String,
    compactions: BTreeMap<i64, Option<String>>,
    tools: ToolStream,
    signatures: BTreeMap<i64, String>,
    usage: Option<Usage>,
    pending_finish: Option<(FinishReasonDetails, Option<ProviderMetadata>)>,
    lifecycle: Lifecycle,
}

impl Parser {
    pub fn new(request: &LLMRequest) -> Self {
        Self {
            provider: request.model.provider.clone(),
            key: request.metadata_key(),
            label: route_label(request),
            compactions: BTreeMap::new(),
            tools: ToolStream::new(ADAPTER),
            signatures: BTreeMap::new(),
            usage: None,
            pending_finish: None,
            lifecycle: Lifecycle::new(),
        }
    }

    fn invalid_event(&self, event: &Value) -> AIError {
        AIError::invalid_output(ADAPTER, "Invalid anthropic/anthropic-messages stream event").with_body(encode(event))
    }

    fn handle(&mut self, event: &Value) -> Result<Vec<LLMEvent>, AIError> {
        let mut events = vec![];
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if !SSE_EVENTS.contains(&kind) {
            return Ok(events);
        }
        if kind != "content_block_start" {
            if let Some(b) = event.get("content_block") {
                if !valid_block(b) {
                    return Err(self.invalid_event(event));
                }
            }
        }
        if kind != "content_block_delta" {
            if let Some(d) = event.get("delta") {
                if !valid_delta(d) {
                    return Err(self.invalid_event(event));
                }
            }
        }
        let index = event.get("index").and_then(Value::as_i64);
        match kind {
            "message_start" => {
                if let Some(usage) = map_usage(event.get("message").and_then(|m| m.get("usage")), &self.key) {
                    self.usage = merge_usage(self.usage.take(), Some(usage), &self.key);
                }
            }
            "content_block_start" => {
                let block = event.get("content_block").filter(|b| b.is_object()).ok_or_else(|| self.invalid_event(event))?;
                let btype = block.get("type").and_then(Value::as_str).ok_or_else(|| self.invalid_event(event))?.to_string();
                if btype == "compaction" {
                    let content = block.get("content");
                    let valid = matches!(content, Some(Value::String(_)) | Some(Value::Null));
                    let Some(index) = index.filter(|_| valid) else { return Err(self.invalid_event(event)) };
                    self.compactions.insert(index, content.and_then(Value::as_str).map(String::from));
                    return Ok(events);
                }
                if !STREAM_BLOCK_TYPES.contains(&btype.as_str()) && server_tool_name(&btype).is_none() {
                    return Ok(events);
                }
                if !valid_block(block) {
                    return Err(self.invalid_event(event));
                }
                let s = |k: &str| block.get(k).and_then(Value::as_str);
                match btype.as_str() {
                    "tool_use" | "server_tool_use" => {
                        let Some(index) = index else {
                            return Err(AIError::invalid_output(ADAPTER, format!("Anthropic {btype} missing index")));
                        };
                        let Some(id) = s("id").filter(|i| !i.is_empty()) else {
                            return Err(AIError::invalid_output(ADAPTER, format!("Anthropic tool_use missing id at index {index}")));
                        };
                        let input = match block.get("input") {
                            Some(Value::Object(o)) if o.is_empty() => String::new(),
                            Some(v) => encode(v),
                            None => String::new(),
                        };
                        let provider_executed = btype == "server_tool_use";
                        let tool = PendingTool {
                            id: id.into(),
                            name: s("name").unwrap_or("").into(),
                            namespace: None,
                            input,
                            provider_executed,
                            provider_metadata: None,
                        };
                        self.lifecycle.step_start(&mut events);
                        events.push(LLMEvent::ToolInputStart {
                            id: tool.id.clone(),
                            name: tool.name.clone(),
                            namespace: None,
                            provider_executed: if provider_executed { Some(true) } else { None },
                            provider_metadata: None,
                        });
                        self.tools.start(index, tool);
                    }
                    "text" => {
                        if let Some(text) = s("text") {
                            let id = format!("text-{}", index.unwrap_or(0));
                            self.lifecycle.text_start(&mut events, &id, None);
                            if !text.is_empty() {
                                self.lifecycle.text_delta(&mut events, &id, text, None);
                            }
                        }
                    }
                    "thinking" => {
                        if let Some(thinking) = s("thinking") {
                            let id = format!("reasoning-{}", index.unwrap_or(0));
                            let metadata = s("signature").map(|sig| pm(&self.key, json!({"signature": sig})));
                            self.lifecycle.reasoning_start(&mut events, &id, metadata.clone());
                            if !thinking.is_empty() {
                                self.lifecycle.reasoning_delta(&mut events, &id, thinking, metadata);
                            }
                            if let (Some(i), Some(sig)) = (index, s("signature")) {
                                self.signatures.insert(i, sig.into());
                            }
                        }
                    }
                    "redacted_thinking" => {
                        if let Some(data) = s("data") {
                            let id = format!("reasoning-{}", index.unwrap_or(0));
                            self.lifecycle.reasoning_start(&mut events, &id, Some(pm(&self.key, json!({"redactedData": data}))));
                        }
                    }
                    other => {
                        let name = server_tool_name(other).unwrap();
                        let content = block.get("content").cloned().unwrap_or(Value::Null);
                        let is_error = content
                            .as_object()
                            .and_then(|c| c.get("type"))
                            .map(|t| crate::json::js_string(t).ends_with("_tool_result_error"))
                            .unwrap_or(false);
                        self.lifecycle.step_start(&mut events);
                        let mut meta = Object::new();
                        meta.insert("blockType".into(), json!(other));
                        meta.insert("result".into(), content.clone());
                        events.push(LLMEvent::ToolResult {
                            id: s("tool_use_id").unwrap_or("").into(),
                            name: name.into(),
                            namespace: None,
                            result: if is_error { ToolResultValue::Error(content) } else { ToolResultValue::Json(content) },
                            output: None,
                            provider_executed: Some(true),
                            provider_metadata: Some(pm(&self.key, Value::Object(meta))),
                        });
                    }
                }
            }
            "content_block_delta" => {
                let delta = event.get("delta").filter(|d| d.is_object()).ok_or_else(|| self.invalid_event(event))?;
                if let Some(t) = delta.get("type").and_then(Value::as_str) {
                    if !STREAM_DELTA_TYPES.contains(&t) {
                        return Ok(events);
                    }
                }
                if !valid_delta(delta) {
                    return Err(self.invalid_event(event));
                }
                let dtype = delta.get("type").and_then(Value::as_str).unwrap_or("");
                let s = |k: &str| delta.get(k).and_then(Value::as_str);
                match dtype {
                    "compaction_delta" => {
                        let valid = index.map(|i| self.compactions.contains_key(&i)).unwrap_or(false);
                        let content = delta.get("content");
                        if !valid || content.is_none() {
                            return Err(AIError::invalid_output(ADAPTER, "Compaction delta is missing its block or content"));
                        }
                        self.compactions.insert(index.unwrap(), content.and_then(Value::as_str).map(String::from));
                    }
                    "text_delta" => {
                        if let Some(text) = s("text").filter(|t| !t.is_empty()) {
                            let id = format!("text-{}", index.unwrap_or(0));
                            if self.lifecycle.is_text_open(&id) {
                                self.lifecycle.text_delta(&mut events, &id, text, None);
                            }
                        }
                    }
                    "thinking_delta" => {
                        if let Some(text) = s("thinking").filter(|t| !t.is_empty()) {
                            let id = format!("reasoning-{}", index.unwrap_or(0));
                            if self.lifecycle.is_reasoning_open(&id) {
                                self.lifecycle.reasoning_delta(&mut events, &id, text, None);
                            }
                        }
                    }
                    "signature_delta" => {
                        if let Some(sig) = s("signature").filter(|t| !t.is_empty()) {
                            let i = index.unwrap_or(0);
                            if self.lifecycle.is_reasoning_open(&format!("reasoning-{i}")) {
                                self.signatures.insert(i, sig.into());
                            }
                        }
                    }
                    "input_json_delta" => {
                        if let (Some(i), Some(partial)) = (index, s("partial_json").filter(|p| !p.is_empty())) {
                            if self.tools.contains(&i.into()) {
                                let mut out = vec![];
                                self.tools.append_existing(i, partial, "Anthropic Messages tool argument delta is missing its tool call", &mut out)?;
                                if !out.is_empty() {
                                    self.lifecycle.step_start(&mut events);
                                }
                                events.extend(out);
                            }
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let Some(index) = index else { return Ok(events) };
                if let Some(content) = self.compactions.remove(&index) {
                    self.lifecycle.step_start(&mut events);
                    events.push(LLMEvent::Compaction(CompactionPart {
                        provider: self.provider.clone(),
                        id: None,
                        encrypted: None,
                        text: Some(content),
                    }));
                    return Ok(events);
                }
                let mut out = vec![];
                self.tools.finish(index, &mut out)?;
                let signature = self.signatures.remove(&index);
                if !out.is_empty() {
                    self.lifecycle.step_start(&mut events);
                } else {
                    self.lifecycle.text_end(&mut events, &format!("text-{index}"), None, None);
                    self.lifecycle.reasoning_end(
                        &mut events,
                        &format!("reasoning-{index}"),
                        signature.map(|s| pm(&self.key, json!({"signature": s}))),
                        None,
                    );
                }
                events.extend(out);
            }
            "message_delta" => {
                let delta = event.get("delta");
                if let Some(d) = delta {
                    if !valid_delta(d) {
                        return Err(self.invalid_event(event));
                    }
                } else {
                    return Err(self.invalid_event(event));
                }
                let usage = map_usage(event.get("usage"), &self.key);
                self.usage = merge_usage(self.usage.take(), usage, &self.key);
                let d = delta.unwrap();
                if let Some(stop) = d.get("stop_reason").and_then(Value::as_str) {
                    let metadata = match d.get("stop_sequence").and_then(Value::as_str) {
                        Some(seq) => Some(pm(&self.key, json!({"stopSequence": seq}))),
                        None => self.pending_finish.as_ref().and_then(|p| p.1.clone()),
                    };
                    let details = d.get("stop_details");
                    let category = details.and_then(|x| x.get("category")).and_then(Value::as_str).filter(|s| !s.is_empty());
                    let explanation = details.and_then(|x| x.get("explanation")).and_then(Value::as_str).filter(|s| !s.is_empty());
                    self.pending_finish = Some((
                        FinishReasonDetails {
                            normalized: map_finish_reason(stop),
                            raw: Some(stop.into()),
                            category: category.map(String::from),
                            explanation: explanation.map(String::from),
                        },
                        metadata,
                    ));
                }
            }
            "message_stop" => {
                if !self.compactions.is_empty() {
                    return Err(AIError::invalid_output(ADAPTER, "Response ended with an incomplete compaction block"));
                }
                let mut out = vec![];
                self.tools.finish_all(&mut out)?;
                if !out.is_empty() {
                    self.lifecycle.step_start(&mut events);
                }
                events.extend(out);
                let signatures = std::mem::take(&mut self.signatures);
                for (index, sig) in signatures {
                    self.lifecycle.reasoning_end(&mut events, &format!("reasoning-{index}"), Some(pm(&self.key, json!({"signature": sig}))), None);
                }
                let (reason, metadata) = self
                    .pending_finish
                    .clone()
                    .unwrap_or((FinishReasonDetails::new(FinishReason::Unknown), None));
                self.lifecycle.finish(&mut events, reason, self.usage.clone(), metadata);
            }
            "error" => {
                let error = event.get("error");
                let t = error.and_then(|e| e.get("type")).and_then(Value::as_str).filter(|s| !s.is_empty());
                let m = error.and_then(|e| e.get("message")).and_then(Value::as_str).filter(|s| !s.is_empty());
                let message = match (t, m) {
                    (Some(t), Some(m)) => format!("{t}: {m}"),
                    (None, Some(m)) => m.into(),
                    (Some(t), None) => t.into(),
                    _ => "Anthropic Messages stream error".into(),
                };
                return Err(classify_provider_failure(ProviderFailure::new(message).raw_body(encode(event))));
            }
            _ => {}
        }
        Ok(events)
    }
}

impl StreamParser for Parser {
    fn step(&mut self, frame: &Frame) -> Result<Step, AIError> {
        let event = decode_json_frame(ADAPTER, &self.label, frame)?;
        if !event.is_object() || !event.get("type").map(Value::is_string).unwrap_or(false) {
            return Err(AIError::invalid_output(ADAPTER, format!("Invalid {} stream event", self.label)));
        }
        Ok(Step::events(self.handle(&event)?))
    }
}
