//! Helpers shared by protocol implementations (`protocols/shared.ts`, `utils/cache.ts`,
//! `utils/claude-model.ts`).

use crate::error::AIError;
use crate::json::{encode, js_string};
use crate::media::{parse_data_url, Inline, MediaAsset};
use crate::types::{
    CacheHint, ContentPart, LLMRequest, MediaPart, Message, Role, TextPart, ToolChoice, ToolChoiceType, ToolContent,
    ToolDefinition, ToolEntry, ToolResultPart, ToolResultValue,
};
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

pub const OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH: usize = 64;

/// `withUntypedFrames`: an object frame without a `type` key (gateways inject them as keep-alives)
/// decodes as an ignored event, or as an `error` event when it carries a non-null `error`. Any other
/// value is returned unchanged for the protocol to validate.
pub fn with_untyped_frame(mut frame: Value) -> Value {
    if let Some(o) = frame.as_object_mut()
        && !o.contains_key("type")
    {
        let kind = if o.get("error").is_some_and(|e| !e.is_null()) { "error" } else { "ignored" };
        o.insert("type".into(), Value::String(kind.into()));
    }
    frame
}

/// `sendsPromptCacheKey`: body-level `prompt_cache_key` is off when caching is disabled or the model
/// explicitly rejects the field. Header-based affinity is unaffected.
pub fn sends_prompt_cache_key(request: &LLMRequest) -> bool {
    !request.cache.as_ref().map(|c| c.is_none()).unwrap_or(false)
        && request.model.compatibility.as_ref().and_then(|c| c.supports_prompt_cache_key) != Some(false)
}

/// `promptCacheKey(request)`: undefined unless [`sends_prompt_cache_key`]; truncated to 64 code points.
pub fn prompt_cache_key(request: &LLMRequest) -> Option<String> {
    if !sends_prompt_cache_key(request) {
        return None;
    }
    let key = request.prompt_cache_key.as_ref()?;
    // JS `Array.from` iterates code points.
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH {
        Some(key.clone())
    } else {
        Some(chars[..OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH].iter().collect())
    }
}

pub fn total_tokens(input: Option<u64>, output: Option<u64>, total: Option<u64>) -> Option<u64> {
    if total.is_some() {
        return total;
    }
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(input.unwrap_or(0) + output.unwrap_or(0))
}

pub fn subtract_tokens(total: Option<u64>, subtrahend: Option<u64>) -> Option<u64> {
    let total = total?;
    match subtrahend {
        None => Some(total),
        Some(s) => Some(total.saturating_sub(s)),
    }
}

pub fn sum_tokens(values: &[Option<u64>]) -> Option<u64> {
    if values.iter().all(Option::is_none) {
        return None;
    }
    Some(values.iter().map(|v| v.unwrap_or(0)).sum())
}

/// `ProviderShared.reportedBilling(usage)`: the USD `cost` an endpoint reports beside its token usage
/// (OpenRouter, LiteLLM, OpenCode Zen). A malformed or negative amount reads as absent.
pub fn reported_cost(usage: &Value) -> Option<f64> {
    usage.get("cost").and_then(Value::as_f64).filter(|c| c.is_finite() && *c >= 0.0)
}

/// `sumTokens` over billed amounts (`ProviderShared.sumTokens` applied to `cost`).
pub fn sum_costs(values: &[Option<f64>]) -> Option<f64> {
    if values.iter().all(Option::is_none) {
        return None;
    }
    Some(values.iter().map(|v| v.unwrap_or(0.0)).sum())
}

/// `fitThinkingBudget(budget, maxTokens, minimum)`.
pub fn fit_thinking_budget(budget: f64, max_tokens: Option<f64>, minimum: f64) -> f64 {
    match max_tokens {
        None => budget,
        Some(max) if budget <= max / 2.0 => budget,
        Some(max) => minimum.max((max / 2.0).floor()),
    }
}

pub fn join_text<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    parts.into_iter().collect::<Vec<_>>().join("\n")
}

fn escape_system_update_text(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// `wrapSystemUpdate`.
pub fn wrap_system_update(texts: &[&str]) -> String {
    format!("<system-update>\n{}\n</system-update>", escape_system_update_text(&join_text(texts.iter().copied())))
}

fn format_content_types(types: &[&str]) -> String {
    match types.len() {
        0 => String::new(),
        1 => types[0].to_string(),
        2 => format!("{} and {}", types[0], types[1]),
        n => format!("{}, and {}", types[..n - 1].join(", "), types[n - 1]),
    }
}

/// `unsupportedContent(route, role, types)`.
pub fn unsupported_content(route: &str, role: Role, types: &[&str]) -> AIError {
    AIError::invalid_request(format!(
        "{route} {} messages only support {} content for now",
        role.as_str(),
        format_content_types(types)
    ))
}

/// `systemUpdateText`: text parts of a system update; non-text → error.
pub fn system_update_text<'a>(route: &str, message: &'a Message) -> Result<Vec<&'a TextPart>, AIError> {
    message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Text(t) => Ok(t),
            _ => Err(unsupported_content(route, Role::System, &["text"])),
        })
        .collect()
}

/// `wrappedSystemUpdate`: wrapped text part inheriting the last part's cache hint.
pub fn wrapped_system_update(route: &str, message: &Message) -> Result<TextPart, AIError> {
    let content = system_update_text(route, message)?;
    let texts: Vec<&str> = content.iter().map(|t| t.text.as_str()).collect();
    Ok(TextPart {
        text: wrap_system_update(&texts),
        cache: content.last().and_then(|t| t.cache.clone()),
        metadata: None,
        provider_metadata: None,
    })
}

/// `parseToolInput`: empty → `{}`; strict JSON.
pub fn parse_tool_input(route: &str, name: &str, raw: &str) -> Result<Value, AIError> {
    let text = if raw.is_empty() { "{}" } else { raw };
    serde_json::from_str::<Value>(text).map_err(|e| {
        AIError::invalid_output(route, format!("Invalid JSON input for {route} tool call {name}")).with_body(text.to_string()).with_cause(e)
    })
}

pub fn inline_required(route: &str, asset: &MediaAsset) -> AIError {
    AIError::invalid_request(format!(
        "{route} requires inline media (bytes or base64); {} sources must be materialized first",
        asset.source_type()
    ))
}

pub fn require_inline_media(route: &str, asset: &MediaAsset) -> Result<Inline, AIError> {
    asset.inline().ok_or_else(|| inline_required(route, asset))
}

#[derive(Debug, Clone, PartialEq)]
pub enum MediaReference {
    DataUrl(String),
    Url(String),
    Ref(String),
}

impl MediaReference {
    pub fn value(&self) -> &str {
        match self {
            Self::DataUrl(v) | Self::Url(v) | Self::Ref(v) => v,
        }
    }
}

/// `mediaReference(asset, provider, label)`.
pub fn media_reference(asset: &MediaAsset, provider: Option<&str>, label: &str) -> Result<MediaReference, AIError> {
    if let Some(inline) = asset.inline() {
        return Ok(MediaReference::DataUrl(inline.data_url));
    }
    if let Some(url) = asset.url_value() {
        return Ok(MediaReference::Url(url.to_string()));
    }
    if let (Some(p), crate::media::MediaSource::Ref { provider: rp, id, .. }) = (provider, &asset.source) {
        if p == rp {
            return Ok(MediaReference::Ref(id.clone()));
        }
    }
    let accepted = provider.map(|p| format!(", and {p} references")).unwrap_or_default();
    let got = match &asset.source {
        crate::media::MediaSource::Ref { provider, id, .. } => format!("; got {provider}:{id}"),
        _ => String::new(),
    };
    Err(AIError::invalid_request(format!("{label} accepts inline bytes, data URLs, http(s) URLs{accepted}{got}")))
}


/// `toolFileMedia`: lift a tool-result file into a media part.
pub fn tool_file_media(uri: &str, mime: &str, name: Option<&str>) -> MediaPart {
    let asset = match parse_data_url(uri) {
        Some(parsed) => match parsed.source {
            crate::media::MediaSource::Base64 { data, .. } => MediaAsset::base64(data, mime),
            _ => parsed,
        },
        // Any URI (`https://`, `gs://`, `s3://`, ...) stays a URL asset.
        None if uri.contains("://") => MediaAsset::url(uri, Some(mime.to_string())),
        None => MediaAsset::base64(uri, mime),
    };
    MediaPart { media: asset, filename: name.map(String::from), cache: None, metadata: None, provider_metadata: None }
}

/// `toolResultText(part)`.
pub fn tool_result_text(result: &ToolResultValue) -> String {
    match result {
        ToolResultValue::Text(v) => js_string(v),
        ToolResultValue::Error(v) => match v {
            Value::Object(_) | Value::Array(_) => encode(v),
            other => js_string(other),
        },
        ToolResultValue::Json(v) => encode(v),
        ToolResultValue::Content(items) => encode(&serde_json::to_value(items).unwrap_or(Value::Null)),
    }
}

pub fn tool_result_part_text(part: &ToolResultPart) -> String {
    tool_result_text(&part.result)
}

/// Text items of a content tool result joined with `sep`, and file items.
pub fn content_text_and_files(items: &[ToolContent]) -> (Vec<&str>, Vec<(&str, &str, Option<&str>)>) {
    let mut texts = vec![];
    let mut files = vec![];
    for item in items {
        match item {
            ToolContent::Text { text } => texts.push(text.as_str()),
            ToolContent::File { uri, mime, name } => files.push((uri.as_str(), mime.as_str(), name.as_deref())),
        }
    }
    (texts, files)
}

/// `flattenTools`: namespaces → `<path>_<name>`, dedup by final name (last wins, first position).
pub fn flatten_tools(tools: &[ToolEntry]) -> Vec<ToolDefinition> {
    fn walk(tools: &[ToolEntry], path: &[String], out: &mut Vec<ToolDefinition>) {
        for tool in tools {
            match tool {
                ToolEntry::Namespace(ns) => {
                    let mut next = path.to_vec();
                    next.push(ns.name.clone());
                    walk(&ns.tools, &next, out);
                }
                ToolEntry::Tool(def) => {
                    if path.is_empty() {
                        out.push(def.clone());
                    } else {
                        let mut renamed = def.clone();
                        renamed.name = path.iter().cloned().chain(std::iter::once(def.name.clone())).collect::<Vec<_>>().join("_");
                        out.push(renamed);
                    }
                }
            }
        }
    }
    let mut flat = vec![];
    walk(tools, &[], &mut flat);
    let mut map: IndexMap<String, ToolDefinition> = IndexMap::new();
    for tool in flat {
        map.insert(tool.name.clone(), tool);
    }
    map.into_values().collect()
}

/// `flattenToolRequest`: rewrite namespaced history parts and flatten tools.
pub fn flatten_tool_request(request: &LLMRequest) -> (Vec<ToolDefinition>, LLMRequest) {
    let mut req = request.clone();
    for message in &mut req.messages {
        for part in &mut message.content {
            match part {
                ContentPart::ToolCall(call) => {
                    if let Some(ns) = call.namespace.take() {
                        call.name = format!("{ns}_{}", call.name);
                    }
                }
                ContentPart::ToolResult(result) => {
                    if let Some(ns) = result.namespace.take() {
                        result.name = format!("{ns}_{}", result.name);
                    }
                }
                _ => {}
            }
        }
    }
    (flatten_tools(&request.tools), req)
}

/// Resolved tool choice.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolChoiceMode {
    Auto,
    None,
    Required,
    Tool(String),
}

/// `matchToolChoice`.
pub fn match_tool_choice(route: &str, choice: &ToolChoice) -> Result<ToolChoiceMode, AIError> {
    Ok(match choice.kind {
        ToolChoiceType::Auto => ToolChoiceMode::Auto,
        ToolChoiceType::None => ToolChoiceMode::None,
        ToolChoiceType::Required => ToolChoiceMode::Required,
        ToolChoiceType::Tool => match choice.name.as_deref().filter(|n| !n.is_empty()) {
            Some(name) => ToolChoiceMode::Tool(name.into()),
            None => return Err(AIError::invalid_request(format!("{route} tool choice requires a tool name"))),
        },
    })
}

// ---------------------------------------------------------------------------------------------
// Cache markers (`utils/cache.ts`)
// ---------------------------------------------------------------------------------------------

/// Breakpoint budget for Anthropic-style cache markers.
#[derive(Debug, Clone)]
pub struct Breakpoints {
    pub remaining: u32,
    pub dropped: u32,
}

impl Breakpoints {
    pub fn new(cap: u32) -> Self {
        Self { remaining: cap, dropped: 0 }
    }
}

/// `cacheControl()`: at most 4 markers; `{type:"ephemeral", ttl?}` with the hint's lifetime as given.
#[derive(Debug, Clone)]
pub struct CacheControl {
    pub breakpoints: Breakpoints,
}

impl Default for CacheControl {
    fn default() -> Self {
        Self { breakpoints: Breakpoints::new(4) }
    }
}

impl CacheControl {
    pub fn mark(&mut self, cache: Option<&CacheHint>) -> Option<Value> {
        let cache = cache?;
        if self.breakpoints.remaining == 0 {
            return None;
        }
        self.breakpoints.remaining -= 1;
        let mut obj = serde_json::Map::new();
        obj.insert("type".into(), Value::String("ephemeral".into()));
        if let Some(ttl) = &cache.ttl {
            obj.insert("ttl".into(), Value::String(ttl.clone()));
        }
        Some(Value::Object(obj))
    }
}

// ---------------------------------------------------------------------------------------------
// Claude model detection (`utils/claude-model.ts`)
// ---------------------------------------------------------------------------------------------

pub const THINKING_BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";

#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeVersion {
    pub family: String,
    pub major: u32,
    pub minor: u32,
}

static CLAUDE_VERSION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:^|[./])claude-(?P<family>[a-z]+)-(?P<major>\d+)(?:[.-](?P<minor>\d{1,2}))?(?:$|[-:@])").unwrap());

/// `claudeVersion(id)`. Note JS regex backtracking: the optional minor group may be skipped so the
/// terminator matches `-`; emulate by trying both alternatives.
pub fn claude_version(id: &str) -> Option<ClaudeVersion> {
    let lower = id.to_lowercase();
    let caps = CLAUDE_VERSION.captures(&lower)?;
    Some(ClaudeVersion {
        family: caps["family"].to_string(),
        major: caps["major"].parse().ok()?,
        minor: caps.name("minor").and_then(|m| m.as_str().parse().ok()).unwrap_or(0),
    })
}

pub fn supports_thinking_block_binding(request: &LLMRequest) -> bool {
    if let Some(v) = request.model.compatibility.as_ref().and_then(|c| c.supports_thinking_block_binding) {
        return v;
    }
    claude_version(&request.model.id).map(|v| v.major > 5 || (v.major == 5 && v.minor >= 1)).unwrap_or(false)
}

/// Content type names for error messages.
pub fn content_type(part: &ContentPart) -> &'static str {
    part.type_name()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_versions() {
        let v = claude_version("claude-opus-4-5-20251101").unwrap();
        assert_eq!((v.family.as_str(), v.major, v.minor), ("opus", 4, 5));
        let v = claude_version("anthropic.claude-sonnet-5-1").unwrap();
        assert_eq!((v.major, v.minor), (5, 1));
        let v = claude_version("claude-sonnet-4-20250514").unwrap();
        assert_eq!((v.major, v.minor), (4, 0));
        assert!(claude_version("gpt-5").is_none());
    }

    #[test]
    fn budgets_and_tokens() {
        assert_eq!(fit_thinking_budget(16000.0, Some(20000.0), 1024.0), 10000.0);
        assert_eq!(fit_thinking_budget(1000.0, Some(20000.0), 1024.0), 1000.0);
        assert_eq!(fit_thinking_budget(16000.0, Some(1000.0), 1024.0), 1024.0);
        assert_eq!(total_tokens(None, None, None), None);
        assert_eq!(subtract_tokens(Some(5), Some(10)), Some(0));
    }

    #[test]
    fn system_update_wrapping() {
        assert_eq!(wrap_system_update(&["a <b> & c"]), "<system-update>\na &lt;b&gt; &amp; c\n</system-update>");
    }
}

// ---------------------------------------------------------------------------------------------
// Mistral tool ids (`utils/mistral-tool-id.ts`)
// ---------------------------------------------------------------------------------------------

static MISTRAL_VALID_ID: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z0-9]{9}$").unwrap());

pub fn is_valid_mistral_id(id: &str) -> bool {
    MISTRAL_VALID_ID.is_match(id)
}

fn to_base36(mut n: u32) -> String {
    if n == 0 {
        return "0".into();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = vec![];
    while n > 0 {
        out.push(digits[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

/// `MistralToolID.hash`: two FNV-style hashes over the first UTF-16 unit of each code point.
pub fn mistral_hash(value: &str) -> String {
    let hash = |seed: u32| {
        let mut result = seed;
        for c in value.chars() {
            let cp = c as u32;
            let unit = if cp > 0xFFFF { ((cp - 0x10000) >> 10) + 0xD800 } else { cp };
            result = (result ^ unit).wrapping_mul(16777619);
        }
        to_base36(result)
    };
    let joined = format!("{:0>7}{:0>7}", hash(2166136261), hash(2246822519));
    joined[joined.len() - 9..].to_string()
}

/// `MistralToolID.normalizer(request)`.
pub struct MistralIdNormalizer {
    used: std::collections::HashSet<String>,
    normalized: std::collections::HashMap<String, String>,
}

impl MistralIdNormalizer {
    pub fn new(request: &LLMRequest) -> Self {
        let mut used = std::collections::HashSet::new();
        for message in &request.messages {
            for part in &message.content {
                let id = match part {
                    ContentPart::ToolCall(c) => &c.id,
                    ContentPart::ToolResult(r) => &r.id,
                    _ => continue,
                };
                if is_valid_mistral_id(id) {
                    used.insert(id.clone());
                }
            }
        }
        Self { used, normalized: std::collections::HashMap::new() }
    }

    pub fn normalize(&mut self, id: &str) -> String {
        if is_valid_mistral_id(id) {
            return id.to_string();
        }
        if let Some(prev) = self.normalized.get(id) {
            return prev.clone();
        }
        let mut attempt = 0;
        let mut candidate = mistral_hash(id);
        while self.used.contains(&candidate) {
            attempt += 1;
            candidate = mistral_hash(&format!("{id}:{attempt}"));
        }
        self.used.insert(candidate.clone());
        self.normalized.insert(id.to_string(), candidate.clone());
        candidate
    }
}

#[cfg(test)]
mod mistral_tests {
    use super::*;

    #[test]
    fn mistral_hash_matches_js() {
        assert_eq!(mistral_hash("call_abc"), "121vn79bs");
        assert_eq!(mistral_hash("tooluse_XYZ-1"), "6o06ba4iq");
        assert_eq!(mistral_hash("😀x"), "vk17haxyq");
        assert_eq!(mistral_hash(""), "fp115p7cn");
    }
}
