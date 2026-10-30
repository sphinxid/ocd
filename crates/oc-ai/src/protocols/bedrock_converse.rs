//! Bedrock Converse protocol (`protocols/bedrock-converse.ts`, `utils/bedrock-cache.ts`,
//! `utils/bedrock-media.ts`, `utils/bedrock-auth.ts`, `providers/amazon-bedrock.ts`).

pub mod aws;
pub mod sigv4;

use super::lifecycle::Lifecycle;
use super::shared::{
    self, fit_thinking_budget, flatten_tool_request, match_tool_choice, supports_thinking_block_binding, tool_file_media, tool_result_text,
    unsupported_content, wrapped_system_update, MistralIdNormalizer, ToolChoiceMode, THINKING_BINDING_BETA,
};
use super::tool_stream::{PendingTool, ToolStream};
use super::{route_label, Protocol, Step, StreamParser};
use crate::error::{classify_provider_failure, AIError, ProviderFailure};
use crate::json::{encode, num, Object};
use crate::route::framing::{Frame, Framing};
use crate::route::{Endpoint, Route};
use crate::types::*;
use base64::Engine;
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

pub use aws::{bedrock_auth, bedrock_base_url, resolve_region, sigv4_auth, BedrockAuthOptions, CredentialSource};
pub use sigv4::AwsCredentials;

pub const ADAPTER: &str = "bedrock-converse";
const NAME: &str = "Bedrock Converse";
const MIN_THINKING_BUDGET: f64 = 1024.0;
const CAP: u32 = 4;

const LEGACY_CLAUDE: &[&str] = &["anthropic.claude-instant", "anthropic.claude-v1", "anthropic.claude-v2", "anthropic.claude-3-"];
const CLAUDE_5M: &[&str] = &[
    "anthropic.claude-3-5-sonnet-20241022-v2:0",
    "anthropic.claude-3-5-haiku-20241022-v1:0",
    "anthropic.claude-3-7-sonnet-20250219-v1:0",
    "anthropic.claude-sonnet-4-20250514-v1:0",
    "anthropic.claude-opus-4-20250514-v1:0",
    "anthropic.claude-opus-4-1-20250805-v1:0",
];

struct CacheBudget {
    remaining: u32,
    dropped: u32,
    supported: bool,
    ttl1h: bool,
}

impl CacheBudget {
    fn new(model_id: &str, cache_format: Option<&str>) -> Self {
        let short = CLAUDE_5M.iter().any(|id| model_id.contains(id));
        Self {
            remaining: CAP,
            dropped: 0,
            // An explicit Anthropic cache format opts opaque IDs such as application inference profile
            // ARNs in. Assume modern Claude releases retain caching support; older generations need an
            // explicit exception. Other model families use implicit caching where available.
            supported: cache_format == Some("anthropic")
                || (model_id.contains("anthropic.claude-") && (short || !LEGACY_CLAUDE.iter().any(|id| model_id.contains(id)))),
            ttl1h: !short,
        }
    }
    fn block(&mut self, cache: Option<&CacheHint>) -> Option<Value> {
        if !self.supported {
            return None;
        }
        let cache = cache?;
        if self.remaining == 0 {
            self.dropped += 1;
            return None;
        }
        self.remaining -= 1;
        // Older Claude releases cache for five minutes only, so a one-hour request falls back to the default.
        let ttl = cache.ttl.as_deref().filter(|ttl| !(*ttl == "1h" && !self.ttl1h));
        Some(match ttl {
            Some(ttl) => json!({"cachePoint": {"type": "default", "ttl": ttl}}),
            None => json!({"cachePoint": {"type": "default"}}),
        })
    }
    fn text(&mut self, text: &str, cache: Option<&CacheHint>) -> Vec<Value> {
        let mut out = vec![json!({"text": text})];
        if let Some(b) = self.block(cache) {
            out.push(b);
        }
        out
    }
}

fn image_format(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => return None,
    })
}

fn document_format(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "application/pdf" => "pdf",
        "text/csv" => "csv",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "text/html" => "html",
        "text/plain" => "txt",
        "text/markdown" => "md",
        _ => return None,
    })
}

static EXT: Lazy<Regex> = Lazy::new(|| Regex::new(r"\.[^.]*$").unwrap());
static NAME_CHARS: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^a-zA-Z0-9 ()\[\]-]").unwrap());
static SPACES: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());

fn document_name(filename: Option<&str>, names: &mut HashSet<String>) -> String {
    let base = EXT.replace(filename.unwrap_or(""), "").to_string();
    let base = NAME_CHARS.replace_all(&base, " ").to_string();
    let base = SPACES.replace_all(&base, " ").trim().to_string();
    let base: String = base.chars().take(200).collect::<String>().trim().to_string();
    let base = if base.is_empty() { "document".to_string() } else { base };
    let mut name = base.clone();
    let mut index = 2;
    while names.contains(&name) {
        let suffix = format!(" {index}");
        let prefix: String = base.chars().take(200 - suffix.chars().count()).collect::<String>().trim_end().to_string();
        name = format!("{prefix}{suffix}");
        index += 1;
    }
    names.insert(name.clone());
    name
}

fn media_base64(part: &MediaPart) -> Result<String, AIError> {
    let inline = shared::require_inline_media(NAME, &part.media)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(inline.base64.trim())
        .map_err(|_| AIError::invalid_request("Bedrock Converse media data must be valid base64"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn lower_media(part: &MediaPart, names: &mut HashSet<String>) -> Result<Vec<Value>, AIError> {
    let media_type = part.media.media_type();
    let mime = media_type.to_lowercase();
    if let Some(format) = image_format(&mime) {
        return Ok(vec![json!({"image": {"format": format, "source": {"bytes": media_base64(part)?}}})]);
    }
    if mime.starts_with("image/") {
        return Err(AIError::invalid_request(format!("Bedrock Converse does not support image media type {media_type}")));
    }
    if let Some(format) = document_format(&mime) {
        let name = document_name(part.filename.as_deref(), names);
        let block = json!({"document": {"format": format, "name": name, "source": {"bytes": media_base64(part)?}}});
        if let Some(filename) = &part.filename {
            if filename != &name {
                return Ok(vec![
                    json!({"text": format!("Attached file {} has document label {}.", encode(&json!(filename)), encode(&json!(name)))}),
                    block,
                ]);
            }
        }
        return Ok(vec![block]);
    }
    Err(AIError::invalid_request(format!("Bedrock Converse does not support media type {media_type}")))
}

fn remove_empty_keys(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(remove_empty_keys).collect()),
        Value::Object(o) => Value::Object(o.iter().filter(|(k, _)| !k.is_empty()).map(|(k, v)| (k.clone(), remove_empty_keys(v))).collect()),
        other => other.clone(),
    }
}

fn tool_name(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).take(64).collect();
    if s.is_empty() { "_".into() } else { s }
}

fn meta_str(pm: Option<&ProviderMetadata>, key: &str, field: &str) -> Option<String> {
    pm?.get(key)?.as_object()?.get(field)?.as_str().map(String::from)
}

fn append_user(messages: &mut Vec<Value>, content: Vec<Value>) {
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some("user") {
            last["content"].as_array_mut().unwrap().extend(content);
            return;
        }
    }
    messages.push(json!({"role": "user", "content": content}));
}

fn lower_messages(request: &LLMRequest, cache: &mut CacheBudget) -> Result<Vec<Value>, AIError> {
    let mut messages: Vec<Value> = vec![];
    let mut names: HashSet<String> = HashSet::new();
    let mistral = request.model.id.contains("mistral.");
    let mut normalizer = MistralIdNormalizer::new(request);
    let mut normalize = |id: &str| if mistral { normalizer.normalize(id) } else { id.to_string() };
    let key = request.metadata_key();
    let hoist = !(request.model.id.contains("anthropic.claude-") || request.model.id.contains("amazon.nova-"));
    let mut pending_images: Vec<Value> = vec![];

    for message in &request.messages {
        if message.role != Role::Tool && !pending_images.is_empty() {
            if let Some(last) = messages.last_mut() {
                if last.get("role").and_then(Value::as_str) == Some("user") {
                    last["content"].as_array_mut().unwrap().extend(pending_images.drain(..));
                }
            }
            pending_images.clear();
        }
        match message.role {
            Role::System => {
                let part = wrapped_system_update(NAME, message)?;
                let content = cache.text(&part.text, part.cache.as_ref());
                append_user(&mut messages, content);
            }
            Role::User => {
                let mut content = vec![];
                for part in &message.content {
                    match part {
                        ContentPart::Text(t) => content.extend(cache.text(&t.text, t.cache.as_ref())),
                        ContentPart::Media(m) => content.extend(lower_media(m, &mut names)?),
                        _ => return Err(unsupported_content(NAME, Role::User, &["text", "media"])),
                    }
                }
                append_user(&mut messages, content);
            }
            Role::Assistant => {
                let mut content = vec![];
                for part in &message.content {
                    match part {
                        ContentPart::Text(t) => content.extend(cache.text(&t.text, t.cache.as_ref())),
                        ContentPart::Reasoning(r) => {
                            let signature = r.encrypted.clone().or_else(|| meta_str(r.provider_metadata.as_ref(), &key, "signature"));
                            let redacted = meta_str(r.provider_metadata.as_ref(), &key, "redactedData");
                            if signature.is_none() {
                                if let Some(data) = redacted {
                                    content.push(json!({"reasoningContent": {"redactedContent": data}}));
                                    continue;
                                }
                            }
                            match signature.filter(|s| !s.trim().is_empty()) {
                                None => {
                                    if !r.text.trim().is_empty() {
                                        content.extend(cache.text(&r.text, r.cache.as_ref()));
                                    }
                                }
                                Some(sig) => content.push(json!({"reasoningContent": {"reasoningText": {"text": r.text, "signature": sig}}})),
                            }
                        }
                        ContentPart::ToolCall(c) => {
                            content.push(json!({"toolUse": {"toolUseId": normalize(&c.id), "name": tool_name(&c.name), "input": remove_empty_keys(&c.input)}}));
                        }
                        _ => return Err(unsupported_content(NAME, Role::Assistant, &["text", "reasoning", "tool-call"])),
                    }
                }
                if !content.is_empty() {
                    messages.push(json!({"role": "assistant", "content": content}));
                }
            }
            Role::Tool => {
                let mut content = vec![];
                for part in &message.content {
                    let ContentPart::ToolResult(r) = part else {
                        return Err(unsupported_content(NAME, Role::Tool, &["tool-result"]));
                    };
                    let items: Vec<Value> = match &r.result {
                        ToolResultValue::Text(_) | ToolResultValue::Error(_) => vec![json!({"text": tool_result_text(&r.result)})],
                        ToolResultValue::Json(v) => vec![json!({"json": v})],
                        ToolResultValue::Content(items) => {
                            let mut out = vec![];
                            for item in items {
                                match item {
                                    ToolContent::Text { text } => out.push(json!({"text": text})),
                                    ToolContent::File { uri, mime, name } => out.extend(lower_media(&tool_file_media(uri, mime, name.as_deref()), &mut names)?),
                                }
                            }
                            out
                        }
                    };
                    let status = if matches!(r.result, ToolResultValue::Error(_)) { "error" } else { "success" };
                    let images: Vec<Value> = if hoist { items.iter().filter(|i| i.get("image").is_some()).cloned().collect() } else { vec![] };
                    let result_content = if images.is_empty() {
                        items
                    } else {
                        let non_image: Vec<Value> = items.into_iter().filter(|i| i.get("image").is_none()).collect();
                        if non_image.is_empty() { vec![json!({"text": "See attached image."})] } else { non_image }
                    };
                    content.push(json!({"toolResult": {"toolUseId": normalize(&r.id), "content": result_content, "status": status}}));
                    pending_images.extend(images);
                    if let Some(b) = cache.block(r.cache.as_ref()) {
                        content.push(b);
                    }
                }
                append_user(&mut messages, content);
            }
        }
    }
    if !pending_images.is_empty() {
        if let Some(last) = messages.last_mut() {
            if last.get("role").and_then(Value::as_str) == Some("user") {
                last["content"].as_array_mut().unwrap().extend(pending_images.drain(..));
            }
        }
    }
    Ok(messages)
}

static NOVA2: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bamazon\.nova-2-").unwrap());

fn http_body_path<'a>(request: &'a LLMRequest, path: &[&str]) -> Option<&'a Value> {
    let body = request.http.as_ref()?.body.as_ref()?;
    let mut current = body.get(path[0])?;
    for k in &path[1..] {
        current = current.get(*k)?;
    }
    Some(current)
}

/// Decoded Bedrock Converse provider options (`Options`); unknown keys are ignored.
#[derive(Debug, Default)]
struct Options {
    thinking: Option<Thinking>,
    effort: Option<String>,
    service_tier: Option<String>,
    output_format: Option<Object>,
    guardrail: Option<Object>,
    request_metadata: Option<Object>,
}

#[derive(Debug)]
enum Thinking {
    Enabled(f64),
    /// `{type: "adaptive", display?}` or `{type: "disabled"}`, sent as decoded.
    Verbatim(Object),
}

fn option_error(key: &str) -> AIError {
    AIError::invalid_request(format!("Invalid Bedrock Converse provider option `{key}`"))
}

/// Picks the declared keys of a struct option: `required` must be strings, `optional` strings when present.
fn string_struct(value: &Value, key: &str, required: &[&str], optional: &[&str]) -> Result<Object, AIError> {
    let o = value.as_object().ok_or_else(|| option_error(key))?;
    let mut out = Object::new();
    for k in required {
        let v = o.get(*k).filter(|v| v.is_string()).ok_or_else(|| option_error(key))?;
        out.insert((*k).into(), v.clone());
    }
    for k in optional {
        match o.get(*k) {
            None => {}
            Some(v) if v.is_string() => {
                out.insert((*k).into(), v.clone());
            }
            Some(_) => return Err(option_error(key)),
        }
    }
    Ok(out)
}

fn decode_options(request: &LLMRequest) -> Result<Options, AIError> {
    let mut out = Options::default();
    let string = |key: &str| -> Result<Option<String>, AIError> {
        match request.po(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(option_error(key)),
        }
    };
    if let Some(v) = request.po("thinking") {
        out.thinking = Some(match v.get("type").and_then(Value::as_str) {
            Some("enabled") => Thinking::Enabled(v.get("budgetTokens").and_then(Value::as_f64).ok_or_else(|| option_error("thinking"))?),
            Some("adaptive") => Thinking::Verbatim(string_struct(v, "thinking", &["type"], &["display"])?),
            Some("disabled") => Thinking::Verbatim(string_struct(v, "thinking", &["type"], &[])?),
            _ => return Err(option_error("thinking")),
        });
    }
    out.effort = string("effort")?;
    out.service_tier = string("serviceTier")?;
    if let Some(v) = request.po("outputFormat") {
        let o = v.as_object().ok_or_else(|| option_error("outputFormat"))?;
        let schema = o.get("schema").filter(|s| s.is_object()).ok_or_else(|| option_error("outputFormat"))?;
        let mut format = string_struct(v, "outputFormat", &["type"], &[])?;
        format.insert("schema".into(), schema.clone());
        format.extend(string_struct(v, "outputFormat", &[], &["name", "description"])?);
        out.output_format = Some(format);
    }
    if let Some(v) = request.po("guardrail") {
        out.guardrail = Some(string_struct(v, "guardrail", &["identifier", "version"], &["trace", "streamProcessingMode"])?);
    }
    if let Some(v) = request.po("requestMetadata") {
        let o = v.as_object().filter(|o| o.values().all(Value::is_string)).ok_or_else(|| option_error("requestMetadata"))?;
        out.request_metadata = Some(o.clone());
    }
    Ok(out)
}

/// Claude 4.6+ pairs effort with adaptive thinking; earlier Claude models take effort beside a manual budget.
fn supports_adaptive_thinking(model_id: &str) -> bool {
    super::shared::claude_version(model_id).is_some_and(|v| v.major > 4 || (v.major == 4 && v.minor >= 6))
}

/// Reasoning effort, in the field each model family reads.
fn effort_fields(model_id: &str, effort: &str) -> (&'static str, Value) {
    if model_id.contains("anthropic") {
        return ("output_config", json!({"effort": effort}));
    }
    // gpt-oss takes the flat chat-completions spelling; newer OpenAI and xAI models take the Responses one.
    if model_id.contains("openai.gpt-oss") {
        return ("reasoning_effort", json!(effort));
    }
    if model_id.contains("openai.") || model_id.contains("xai.") {
        return ("reasoning", json!({"effort": effort}));
    }
    ("reasoningConfig", json!({"type": "enabled", "maxReasoningEffort": effort}))
}

/// Ordered Bedrock Converse body.
pub fn body(request: &LLMRequest) -> Result<Value, AIError> {
    let tool_choice = match &request.tool_choice {
        None => None,
        Some(c) => match match_tool_choice(NAME, c)? {
            ToolChoiceMode::Auto => Some(json!({"auto": {}})),
            ToolChoiceMode::None => None,
            ToolChoiceMode::Required => Some(json!({"any": {}})),
            ToolChoiceMode::Tool(name) => Some(json!({"tool": {"name": name}})),
        },
    };
    let (tools, flattened) = flatten_tool_request(request);
    let g = request.generation_options();
    let options = decode_options(request)?;
    let high_effort = http_body_path(request, &["additionalModelRequestFields", "reasoningConfig", "maxReasoningEffort"]).and_then(Value::as_str) == Some("high");
    let max_tokens =
        if NOVA2.is_match(&request.model.id) && (options.effort.as_deref() == Some("high") || high_effort) { None } else { g.max_tokens };
    let mut thinking: Option<Object> = match &options.thinking {
        Some(Thinking::Enabled(budget)) => {
            let mut o = Object::new();
            o.insert("type".into(), json!("enabled"));
            o.insert("budget_tokens".into(), num(fit_thinking_budget(*budget, max_tokens, MIN_THINKING_BUDGET)));
            Some(o)
        }
        Some(Thinking::Verbatim(o)) => Some(o.clone()),
        None if options.effort.is_some() && supports_adaptive_thinking(&request.model.id) => {
            let mut o = Object::new();
            o.insert("type".into(), json!("adaptive"));
            Some(o)
        }
        None => None,
    };
    let disabled = thinking.as_ref().and_then(|t| t.get("type")).and_then(Value::as_str) == Some("disabled")
        || matches!(
            http_body_path(request, &["additionalModelRequestFields", "thinking", "type"]).and_then(Value::as_str),
            Some("disabled" | "between_tools")
        );
    if !disabled && supports_thinking_block_binding(request) {
        let mut t = thinking.unwrap_or_else(|| {
            let mut o = Object::new();
            o.insert("type".into(), json!("adaptive"));
            o
        });
        t.insert("block_binding".into(), json!({"prefix_mismatch_behavior": "drop_block"}));
        thinking = Some(t);
    }
    let mut cache = CacheBudget::new(&request.model.id, request.compat().cache_format.as_deref());
    let tool_config = if tools.is_empty() {
        None
    } else {
        let mut lowered = vec![];
        for t in &tools {
            let mut spec = Object::new();
            spec.insert("name".into(), json!(t.name));
            if !t.description.trim().is_empty() {
                spec.insert("description".into(), json!(t.description));
            }
            spec.insert("inputSchema".into(), json!({"json": Value::Object(t.input_schema.clone())}));
            lowered.push(json!({"toolSpec": spec}));
            if let Some(b) = cache.block(t.cache.as_ref()) {
                lowered.push(b);
            }
        }
        let mut o = Object::new();
        o.insert("tools".into(), Value::Array(lowered));
        if let Some(c) = tool_choice {
            o.insert("toolChoice".into(), c);
        }
        Some(Value::Object(o))
    };
    let system: Vec<Value> = request.system.iter().filter(|p| !p.text.is_empty()).flat_map(|p| cache.text(&p.text, p.cache.as_ref())).collect();
    let messages = lower_messages(&flattened, &mut cache)?;
    if cache.dropped > 0 {
        tracing::warn!("Bedrock Converse: dropped {} cache breakpoint(s); the API allows at most {CAP} per request.", cache.dropped);
    }
    let stop_empty = g.stop.as_ref().map(|s| s.is_empty()).unwrap_or(true);
    let inference = if max_tokens.is_none() && g.temperature.is_none() && g.top_p.is_none() && stop_empty {
        None
    } else {
        let mut o = Object::new();
        if let Some(m) = max_tokens {
            o.insert("maxTokens".into(), num(m));
        }
        if let Some(t) = g.temperature {
            o.insert("temperature".into(), num(t));
        }
        if let Some(t) = g.top_p {
            o.insert("topP".into(), num(t));
        }
        if let Some(s) = &g.stop {
            o.insert("stopSequences".into(), json!(s));
        }
        Some(Value::Object(o))
    };
    // Converse's base inferenceConfig has no topK, thinking or effort; models accept them as model-specific
    // fields, so they go through additionalModelRequestFields.
    let additional = {
        let mut o = Object::new();
        if let Some(k) = g.top_k {
            o.insert("top_k".into(), num(k));
        }
        let binding = thinking.as_ref().map(|t| t.contains_key("block_binding")).unwrap_or(false);
        if let Some(t) = thinking {
            o.insert("thinking".into(), Value::Object(t));
        }
        if let Some(effort) = &options.effort {
            let (key, value) = effort_fields(&request.model.id, effort);
            o.insert(key.into(), value);
        }
        // Converse takes Anthropic betas in the body, and Bedrock rejects `block_binding` without this one.
        if binding {
            o.insert("anthropic_beta".into(), json!([THINKING_BINDING_BETA]));
        }
        if o.is_empty() { None } else { Some(Value::Object(o)) }
    };
    let guardrail = options.guardrail.map(|g| {
        let mut o = Object::new();
        o.insert("guardrailIdentifier".into(), g["identifier"].clone());
        o.insert("guardrailVersion".into(), g["version"].clone());
        for key in ["trace", "streamProcessingMode"] {
            if let Some(v) = g.get(key) {
                o.insert(key.into(), v.clone());
            }
        }
        Value::Object(o)
    });
    let output_config = options.output_format.map(|f| {
        let mut schema = Object::new();
        schema.insert("schema".into(), json!(crate::json::encode(&f["schema"])));
        for key in ["name", "description"] {
            if let Some(v) = f.get(key) {
                schema.insert(key.into(), v.clone());
            }
        }
        json!({"textFormat": {"type": f["type"], "structure": {"jsonSchema": schema}}})
    });
    let mut f: IndexMap<String, Option<Value>> = IndexMap::new();
    f.insert("modelId".into(), Some(json!(request.model.id)));
    f.insert("messages".into(), Some(Value::Array(messages)));
    f.insert("system".into(), if system.is_empty() { None } else { Some(Value::Array(system)) });
    f.insert("inferenceConfig".into(), inference);
    f.insert("toolConfig".into(), tool_config);
    f.insert("additionalModelRequestFields".into(), additional);
    f.insert("serviceTier".into(), options.service_tier.map(|t| json!({"type": t})));
    f.insert("guardrailConfig".into(), guardrail);
    f.insert("outputConfig".into(), output_config);
    f.insert("requestMetadata".into(), options.request_metadata.map(Value::Object));
    Ok(Value::Object(f.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect()))
}

pub struct BedrockConverse;

impl Protocol for BedrockConverse {
    fn id(&self) -> &str {
        ADAPTER
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        body(request)
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(Parser::new(request))
    }
}

pub fn protocol() -> Arc<dyn Protocol> {
    Arc::new(BedrockConverse)
}

/// `bedrock-converse` route (baseURL/auth configured by the provider factory).
pub fn route() -> Route {
    Route::new(
        ADAPTER,
        protocol(),
        Endpoint::dynamic(None, |_, body| {
            let model = body.get("modelId").and_then(Value::as_str).unwrap_or("");
            format!("/model/{}/converse-stream", sigv4::encode_uri_component(model))
        }),
        Framing::AwsEventStream(ADAPTER.into()),
    )
    .initial_provider("bedrock", Some("bedrock"))
    .auth(sigv4_auth(None, "bedrock", NAME))
}

// ---------------------------------------------------------------------------------------------
// Stream parsing
// ---------------------------------------------------------------------------------------------

fn map_finish(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        "content_filtered" | "guardrail_intervened" => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

fn map_usage(usage: &Value, key: &str) -> Option<Usage> {
    if !usage.is_object() {
        return None;
    }
    let u = |k: &str| crate::json::get_u64(usage, k);
    let input = shared::sum_tokens(&[u("inputTokens"), u("cacheReadInputTokens"), u("cacheWriteInputTokens")]);
    // Provider metadata is the decoded `BedrockUsageSchema`: its five counts only (`serverToolUsage` and other
    // members are dropped).
    let mut decoded = Object::new();
    for k in ["inputTokens", "outputTokens", "totalTokens", "cacheReadInputTokens", "cacheWriteInputTokens"] {
        if let Some(v) = usage.get(k).filter(|v| v.is_number()) {
            decoded.insert(k.into(), v.clone());
        }
    }
    let mut pm = Object::new();
    pm.insert(key.into(), Value::Object(decoded));
    Some(Usage {
        context_tokens: None,
        input_tokens: input,
        output_tokens: u("outputTokens"),
        non_cached_input_tokens: u("inputTokens"),
        cache_read_input_tokens: u("cacheReadInputTokens"),
        cache_write_input_tokens: u("cacheWriteInputTokens"),
        reasoning_tokens: None,
        total_tokens: shared::total_tokens(input, u("outputTokens"), u("totalTokens")),
        cost: None,
        provider_metadata: Some(pm),
    })
}

pub struct Parser {
    key: String,
    label: String,
    tools: ToolStream,
    finish: Option<FinishReasonDetails>,
    usage: Option<Usage>,
    has_tool_calls: bool,
    lifecycle: Lifecycle,
    signatures: BTreeMap<i64, String>,
    redacted: BTreeMap<i64, Vec<u8>>,
}

impl Parser {
    pub fn new(request: &LLMRequest) -> Self {
        Self {
            key: request.metadata_key(),
            label: route_label(request),
            tools: ToolStream::new(ADAPTER),
            finish: None,
            usage: None,
            has_tool_calls: false,
            lifecycle: Lifecycle::new(),
            signatures: BTreeMap::new(),
            redacted: BTreeMap::new(),
        }
    }

    fn pm(&self, value: Value) -> ProviderMetadata {
        let mut o = Object::new();
        o.insert(self.key.clone(), value);
        o
    }

    fn handle(&mut self, event: &Value) -> Result<Vec<LLMEvent>, AIError> {
        let mut events = vec![];
        let raw = |e: &Value| e.get("rawBody").and_then(Value::as_str).map(String::from).unwrap_or_else(|| encode(e));
        if let Some(start) = event.get("contentBlockStart") {
            if let Some(tool) = start.get("start").and_then(|s| s.get("toolUse")) {
                let index = start.get("contentBlockIndex").and_then(Value::as_i64).unwrap_or(0);
                let id = tool.get("toolUseId").and_then(Value::as_str).unwrap_or("").to_string();
                let name = tool.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                self.lifecycle.step_start(&mut events);
                events.push(LLMEvent::ToolInputStart { id: id.clone(), name: name.clone(), namespace: None, provider_executed: None, provider_metadata: None });
                self.tools.start(index, PendingTool::new(id, name));
                return Ok(events);
            }
        }
        if let Some(delta_event) = event.get("contentBlockDelta") {
            let index = delta_event.get("contentBlockIndex").and_then(Value::as_i64).unwrap_or(0);
            let delta = delta_event.get("delta");
            if let Some(text) = delta.and_then(|d| d.get("text")).and_then(Value::as_str).filter(|t| !t.is_empty()) {
                self.lifecycle.text_delta(&mut events, &format!("text-{index}"), text, None);
                return Ok(events);
            }
            if let Some(reasoning) = delta.and_then(|d| d.get("reasoningContent")).filter(|r| r.is_object()) {
                let chunk = match reasoning.get("redactedContent").and_then(Value::as_str) {
                    None => None,
                    Some(b) => Some(base64::engine::general_purpose::STANDARD.decode(b).map_err(|_| {
                        AIError::invalid_output(ADAPTER, "Bedrock Converse reasoningContent.redactedContent contains invalid base64 data")
                    })?),
                };
                let signature = reasoning.get("signature").and_then(Value::as_str).filter(|s| !s.is_empty());
                let data = reasoning.get("data").and_then(Value::as_str);
                let metadata = if let Some(sig) = signature {
                    Some(self.pm(json!({"signature": sig})))
                } else if chunk.is_none() && data.is_some() {
                    Some(self.pm(json!({"redactedData": data.unwrap()})))
                } else {
                    None
                };
                let id = format!("reasoning-{index}");
                let text = reasoning.get("text").and_then(Value::as_str);
                if text.is_some() || metadata.is_some() {
                    self.lifecycle.reasoning_delta(&mut events, &id, text.unwrap_or(""), metadata);
                } else if chunk.is_some() {
                    self.lifecycle.reasoning_start(&mut events, &id, None);
                }
                match chunk {
                    Some(c) => self.redacted.entry(index).or_default().extend(c),
                    None => {
                        if data.is_some() {
                            self.redacted.remove(&index);
                        }
                    }
                }
                if let Some(sig) = signature {
                    self.signatures.insert(index, sig.into());
                }
                return Ok(events);
            }
            if let Some(input) = delta.and_then(|d| d.get("toolUse")).and_then(|t| t.get("input")).and_then(Value::as_str) {
                let mut out = vec![];
                if self.tools.append(index, input, &mut out).is_some() && !out.is_empty() {
                    self.lifecycle.step_start(&mut events);
                }
                events.extend(out);
                return Ok(events);
            }
            return Ok(events);
        }
        if let Some(stop) = event.get("contentBlockStop") {
            let index = stop.get("contentBlockIndex").and_then(Value::as_i64).unwrap_or(0);
            let mut out = vec![];
            self.tools.finish(index, &mut out)?;
            if !out.is_empty() {
                self.lifecycle.step_start(&mut events);
            } else {
                let metadata = if let Some(sig) = self.signatures.get(&index) {
                    Some(self.pm(json!({"signature": sig})))
                } else {
                    self.redacted.get(&index).map(|chunks| self.pm(json!({"redactedData": base64::engine::general_purpose::STANDARD.encode(chunks)})))
                };
                self.lifecycle.text_end(&mut events, &format!("text-{index}"), None, None);
                self.lifecycle.reasoning_end(&mut events, &format!("reasoning-{index}"), metadata, None);
            }
            if out.iter().any(|e| matches!(e, LLMEvent::ToolCall { .. } | LLMEvent::ToolInputError { .. })) {
                self.has_tool_calls = true;
            }
            events.extend(out);
            self.signatures.remove(&index);
            self.redacted.remove(&index);
            return Ok(events);
        }
        if let Some(stop) = event.get("messageStop") {
            let reason = stop.get("stopReason").and_then(Value::as_str).unwrap_or("");
            if reason == "malformed_model_output" || reason == "malformed_tool_use" {
                return Err(AIError::invalid_output(ADAPTER, format!("Bedrock Converse stopped with {reason}")).with_body(raw(event)));
            }
            self.finish = Some(FinishReasonDetails::raw(map_finish(reason), Some(reason.into())));
            return Ok(events);
        }
        if let Some(metadata) = event.get("metadata") {
            if let Some(u) = metadata.get("usage").and_then(|u| map_usage(u, &self.key)) {
                self.usage = Some(u);
            }
            return Ok(events);
        }
        if let Some(exception) = event.get("exception") {
            let details = exception.get("details");
            let message = details
                .and_then(|d| d.get("message"))
                .and_then(Value::as_str)
                .or_else(|| details.and_then(|d| d.get("originalMessage")).and_then(Value::as_str))
                .unwrap_or("Bedrock Converse stream error");
            let mut clean = event.clone();
            if let Some(o) = clean.as_object_mut() {
                o.shift_remove("rawBody");
            }
            // The original HTTP status lets throttling and server errors classify like HTTP failures.
            let status = details
                .and_then(|d| d.get("originalStatusCode"))
                .and_then(Value::as_f64)
                .filter(|s| s.fract() == 0.0 && (0.0..=f64::from(u16::MAX)).contains(s))
                .map(|s| s as u16);
            return Err(classify_provider_failure(ProviderFailure::new(message).status(status).raw_body(encode(&clean))).with_body(raw(event)));
        }
        Ok(events)
    }
}

impl StreamParser for Parser {
    fn step(&mut self, frame: &Frame) -> Result<Step, AIError> {
        let event = match frame {
            Frame::Json(v) => v.clone(),
            Frame::Text(t) => serde_json::from_str(t)
                .map_err(|_| AIError::invalid_output(ADAPTER, format!("Invalid {} stream event", self.label)).with_body(t.clone()))?,
        };
        Ok(Step::events(self.handle(&event)?))
    }

    fn on_halt(&mut self) -> Result<Vec<LLMEvent>, AIError> {
        let Some(finish) = self.finish.clone() else { return Ok(vec![]) };
        let mut events = vec![];
        let mut reason = finish;
        if reason.normalized == FinishReason::Stop && self.has_tool_calls {
            reason.normalized = FinishReason::ToolCalls;
        }
        let redacted = std::mem::take(&mut self.redacted);
        for (index, chunks) in redacted {
            let meta = match self.signatures.get(&index) {
                Some(sig) => self.pm(json!({"signature": sig})),
                None => self.pm(json!({"redactedData": base64::engine::general_purpose::STANDARD.encode(chunks)})),
            };
            self.lifecycle.reasoning_end(&mut events, &format!("reasoning-{index}"), Some(meta), None);
        }
        self.lifecycle.finish(&mut events, reason, self.usage.clone(), None);
        Ok(events)
    }
}
