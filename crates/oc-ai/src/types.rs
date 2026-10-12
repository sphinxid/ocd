//! Normalized request / message / content model (`packages/ai/src/schema/{messages,options}.ts`).

use crate::json::Object;
use crate::media::MediaAsset;
use crate::route::LanguageModel;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `ProviderMetadata = Record<string, Record<string, unknown>>`, keyed by metadata namespace.
pub type ProviderMetadata = Object;
/// Flat provider options record interpreted by the selected protocol.
pub type ProviderOptions = Object;

/// Reasoning effort values (open string).
pub const REASONING_EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

// ---------------------------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheHintType {
    Ephemeral,
    Persistent,
}

/// `CacheHint`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheHint {
    #[serde(rename = "type")]
    pub kind: CacheHintType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<f64>,
}

impl CacheHint {
    pub fn ephemeral() -> Self {
        Self { kind: CacheHintType::Ephemeral, ttl_seconds: None }
    }
    pub fn with_ttl(ttl_seconds: Option<f64>) -> Self {
        Self { kind: CacheHintType::Ephemeral, ttl_seconds }
    }
}

/// `CachePolicyObject.messages`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CacheMessages {
    Named(String),
    Tail { tail: u32 },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachePolicyObject {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<CacheMessages>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<f64>,
}

/// `CachePolicy = "auto" | "none" | object`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CachePolicy {
    Mode(String),
    Object(CachePolicyObject),
}

impl CachePolicy {
    pub fn auto() -> Self {
        Self::Mode("auto".into())
    }
    pub fn none() -> Self {
        Self::Mode("none".into())
    }
    pub fn is_none(&self) -> bool {
        matches!(self, Self::Mode(m) if m == "none")
    }
    pub fn is_auto(&self) -> bool {
        matches!(self, Self::Mode(m) if m == "auto")
    }
}

/// `GenerationOptions`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
}

impl GenerationOptions {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// `mergeGenerationOptions`: per-field last-defined wins; `None` when every field is undefined.
    pub fn merge(items: &[Option<&GenerationOptions>]) -> Option<GenerationOptions> {
        let mut out = GenerationOptions::default();
        for item in items.iter().flatten() {
            if item.max_tokens.is_some() {
                out.max_tokens = item.max_tokens;
            }
            if item.temperature.is_some() {
                out.temperature = item.temperature;
            }
            if item.top_p.is_some() {
                out.top_p = item.top_p;
            }
            if item.top_k.is_some() {
                out.top_k = item.top_k;
            }
            if item.frequency_penalty.is_some() {
                out.frequency_penalty = item.frequency_penalty;
            }
            if item.presence_penalty.is_some() {
                out.presence_penalty = item.presence_penalty;
            }
            if item.seed.is_some() {
                out.seed = item.seed;
            }
            if item.stop.is_some() {
                out.stop = item.stop.clone();
            }
        }
        if out.is_empty() { None } else { Some(out) }
    }
}

/// Milliseconds (`> 0`) or disabled (`false`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HttpTimeout {
    Millis(f64),
    Disabled(bool),
}

impl HttpTimeout {
    pub fn millis(&self) -> Option<f64> {
        match self {
            Self::Millis(ms) => Some(*ms),
            Self::Disabled(_) => None,
        }
    }
    pub fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Bool(false) => Some(Self::Disabled(false)),
            Value::Number(n) => n.as_f64().filter(|v| *v > 0.0).map(Self::Millis),
            _ => None,
        }
    }
}

/// Applied to `headerTimeout` and `chunkTimeout` when a request leaves them unset.
pub const DEFAULT_HTTP_TIMEOUT_MS: f64 = 300_000.0;

/// `HttpOptions`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Object>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<IndexMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<HttpTimeout>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_timeout: Option<HttpTimeout>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_timeout: Option<HttpTimeout>,
}

fn merge_string_maps(items: &[Option<&IndexMap<String, String>>]) -> Option<IndexMap<String, String>> {
    let defined: Vec<_> = items.iter().flatten().collect();
    if defined.is_empty() {
        return None;
    }
    if defined.len() == 1 {
        return Some((*defined[0]).clone());
    }
    let mut out = IndexMap::new();
    for item in defined {
        for (k, v) in item.iter() {
            out.insert(k.clone(), v.clone());
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

impl HttpOptions {
    pub fn headers(headers: IndexMap<String, String>) -> Self {
        Self { headers: Some(headers), ..Default::default() }
    }

    /// `mergeHttpOptions`.
    pub fn merge(items: &[Option<&HttpOptions>]) -> Option<HttpOptions> {
        let body = crate::json::merge_records(&items.iter().map(|i| i.and_then(|h| h.body.as_ref())).collect::<Vec<_>>());
        let headers = merge_string_maps(&items.iter().map(|i| i.and_then(|h| h.headers.as_ref())).collect::<Vec<_>>());
        let query = merge_string_maps(&items.iter().map(|i| i.and_then(|h| h.query.as_ref())).collect::<Vec<_>>());
        let last = |f: fn(&HttpOptions) -> Option<HttpTimeout>| items.iter().rev().flatten().find_map(|h| f(h));
        let timeout = last(|h| h.timeout);
        let header_timeout = last(|h| h.header_timeout);
        let chunk_timeout = last(|h| h.chunk_timeout);
        if body.is_none() && headers.is_none() && query.is_none() && timeout.is_none() && header_timeout.is_none() && chunk_timeout.is_none() {
            return None;
        }
        Some(HttpOptions { body, headers, query, timeout, header_timeout, chunk_timeout })
    }
}

/// `LanguageModelCompatibility`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Compatibility {
    /// `"gemini" | "moonshot" | "none"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sanitizer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_reasoning: Option<bool>,
    /// `"max_completion_tokens" | "max_tokens"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_finish_reason: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_assistant_after_tool: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_store: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_usage_in_streaming: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_strict_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_prompt_cache_key: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zai_tool_stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_signature: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_thinking_block_binding: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_effort_updates: Option<bool>,
}

impl Compatibility {
    /// Shallow merge: fields set in `over` win.
    pub fn merged(&self, over: &Compatibility) -> Compatibility {
        macro_rules! pick {
            ($($f:ident),*) => { Compatibility { $($f: over.$f.clone().or_else(|| self.$f.clone())),* } };
        }
        pick!(
            sanitizer,
            reasoning_field,
            require_reasoning,
            max_tokens_field,
            require_finish_reason,
            require_assistant_after_tool,
            supports_store,
            supports_usage_in_streaming,
            supports_strict_mode,
            supports_prompt_cache_key,
            zai_tool_stream,
            require_signature,
            supports_thinking_block_binding,
            supports_effort_updates
        )
    }
}

/// `LanguageModelDefaults` (also used as route defaults).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelDefaults {
    pub generation: Option<GenerationOptions>,
    pub provider_options: Option<ProviderOptions>,
    pub http: Option<HttpOptions>,
}

// ---------------------------------------------------------------------------------------------
// Content
// ---------------------------------------------------------------------------------------------

/// `Tool.Content`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolContent {
    Text {
        text: String,
    },
    File {
        uri: String,
        mime: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

/// `ToolResultValue`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "lowercase")]
pub enum ToolResultValue {
    Json(Value),
    Text(Value),
    Error(Value),
    Content(Vec<ToolContent>),
}

impl ToolResultValue {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Json(_) => "json",
            Self::Text(_) => "text",
            Self::Error(_) => "error",
            Self::Content(_) => "content",
        }
    }
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(Value::String(value.into()))
    }
    pub fn error(value: impl Into<String>) -> Self {
        Self::Error(Value::String(value.into()))
    }
}

/// `ToolOutput`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub structured: Value,
    pub content: Vec<ToolContent>,
}

impl ToolOutput {
    pub fn from_result_value(result: &ToolResultValue) -> Option<ToolOutput> {
        match result {
            ToolResultValue::Json(v) => Some(ToolOutput { structured: v.clone(), content: vec![] }),
            ToolResultValue::Text(v) => Some(ToolOutput {
                structured: Value::Object(Object::new()),
                content: vec![ToolContent::Text { text: value_text(v) }],
            }),
            ToolResultValue::Content(c) => Some(ToolOutput { structured: Value::Object(Object::new()), content: c.clone() }),
            ToolResultValue::Error(_) => None,
        }
    }
    pub fn to_result_value(&self) -> ToolResultValue {
        if self.content.is_empty() {
            return ToolResultValue::Json(self.structured.clone());
        }
        if self.content.len() == 1 {
            if let ToolContent::Text { text } = &self.content[0] {
                return ToolResultValue::Text(Value::String(text.clone()));
            }
        }
        ToolResultValue::Content(self.content.clone())
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => crate::json::encode(other),
    }
}

/// System prompt part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemPart {
    #[serde(rename = "type", default = "text_type")]
    pub kind: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheHint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Object>,
}

fn text_type() -> String {
    "text".into()
}

impl SystemPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self { kind: "text".into(), text: text.into(), cache: None, metadata: None }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextPart {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheHint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Object>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<ProviderMetadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaPart {
    pub media: MediaAsset,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheHint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Object>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<ProviderMetadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallPart {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default)]
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_executed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheHint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Object>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<ProviderMetadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultPart {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub result: ToolResultValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_executed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheHint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Object>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<ProviderMetadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningPart {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheHint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Object>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<ProviderMetadata>,
}

/// Provider-generated context checkpoint: exactly one of `encrypted` / `text` (text may be `null`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionPart {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted: Option<String>,
    /// `Some(None)` = provider failed to summarize (`text: null`); `None` = absent.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    pub text: Option<Option<String>>,
}

mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<S: Serializer>(value: &Option<Option<String>>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(inner) => inner.serialize(s),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
        Ok(Some(Option::<String>::deserialize(d)?))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffortPart {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheHint>,
}

/// `ContentPart`, tagged by `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ContentPart {
    Text(TextPart),
    Media(MediaPart),
    ToolCall(ToolCallPart),
    ToolResult(ToolResultPart),
    Reasoning(ReasoningPart),
    Compaction(CompactionPart),
    Effort(EffortPart),
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextPart { text: text.into(), cache: None, metadata: None, provider_metadata: None })
    }
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Media(_) => "media",
            Self::ToolCall(_) => "tool-call",
            Self::ToolResult(_) => "tool-result",
            Self::Reasoning(_) => "reasoning",
            Self::Compaction(_) => "compaction",
            Self::Effort(_) => "effort",
        }
    }
    pub fn cache(&self) -> Option<&CacheHint> {
        match self {
            Self::Text(p) => p.cache.as_ref(),
            Self::Media(p) => p.cache.as_ref(),
            Self::ToolCall(p) => p.cache.as_ref(),
            Self::ToolResult(p) => p.cache.as_ref(),
            Self::Reasoning(p) => p.cache.as_ref(),
            Self::Effort(p) => p.cache.as_ref(),
            Self::Compaction(_) => None,
        }
    }
    pub fn set_cache(&mut self, hint: CacheHint) {
        match self {
            Self::Text(p) => p.cache = Some(hint),
            Self::Media(p) => p.cache = Some(hint),
            Self::ToolCall(p) => p.cache = Some(hint),
            Self::ToolResult(p) => p.cache = Some(hint),
            Self::Reasoning(p) => p.cache = Some(hint),
            Self::Effort(p) => p.cache = Some(hint),
            Self::Compaction(_) => {}
        }
    }
    pub fn provider_metadata(&self) -> Option<&ProviderMetadata> {
        match self {
            Self::Text(p) => p.provider_metadata.as_ref(),
            Self::Media(p) => p.provider_metadata.as_ref(),
            Self::ToolCall(p) => p.provider_metadata.as_ref(),
            Self::ToolResult(p) => p.provider_metadata.as_ref(),
            Self::Reasoning(p) => p.provider_metadata.as_ref(),
            _ => None,
        }
    }
    pub fn metadata(&self) -> Option<&Object> {
        match self {
            Self::Text(p) => p.metadata.as_ref(),
            Self::Media(p) => p.metadata.as_ref(),
            Self::ToolCall(p) => p.metadata.as_ref(),
            Self::ToolResult(p) => p.metadata.as_ref(),
            Self::Reasoning(p) => p.metadata.as_ref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// `Message`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub role: Role,
    pub content: Vec<ContentPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Object>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<ProviderMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<Object>,
}

impl Message {
    pub fn new(role: Role, content: Vec<ContentPart>) -> Self {
        Self { id: None, role, content, metadata: None, provider_metadata: None, native: None }
    }
    pub fn user(content: Vec<ContentPart>) -> Self {
        Self::new(Role::User, content)
    }
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::new(Role::User, vec![ContentPart::text(text)])
    }
    pub fn assistant(content: Vec<ContentPart>) -> Self {
        Self::new(Role::Assistant, content)
    }
    /// Chronological system update.
    pub fn system(text: impl Into<String>) -> Self {
        Self::new(Role::System, vec![ContentPart::text(text)])
    }
    pub fn effort(effort: Option<String>, previous: Option<String>) -> Self {
        Self::new(Role::System, vec![ContentPart::Effort(EffortPart { effort, previous, cache: None })])
    }
    pub fn tool(result: ToolResultPart) -> Self {
        Self::new(Role::Tool, vec![ContentPart::ToolResult(result)])
    }
}

// ---------------------------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------------------------

/// `ToolDefinition`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Object,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Object>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheHint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Object>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<Object>,
}

impl ToolDefinition {
    pub fn new(name: impl Into<String>, description: impl Into<String>, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema: input_schema.as_object().cloned().unwrap_or_default(),
            output_schema: None,
            cache: None,
            metadata: None,
            native: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolNamespace {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub tools: Vec<ToolEntry>,
}

/// `ToolEntry = ToolDefinition | ToolNamespace`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolEntry {
    Tool(ToolDefinition),
    Namespace(ToolNamespace),
}

impl ToolEntry {
    pub fn name(&self) -> &str {
        match self {
            Self::Tool(t) => &t.name,
            Self::Namespace(n) => &n.name,
        }
    }
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Tool(_) => "tool",
            Self::Namespace(_) => "namespace",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoiceType {
    Auto,
    None,
    Required,
    Tool,
}

/// `ToolChoice`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolChoice {
    #[serde(rename = "type")]
    pub kind: ToolChoiceType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_parallel_tool_use: Option<bool>,
}

impl ToolChoice {
    /// String input: `auto|none|required` → mode, anything else → named tool.
    pub fn from_str(value: &str) -> Self {
        let kind = match value {
            "auto" => ToolChoiceType::Auto,
            "none" => ToolChoiceType::None,
            "required" => ToolChoiceType::Required,
            _ => return Self::named(value),
        };
        Self { kind, name: None, disable_parallel_tool_use: None }
    }
    pub fn named(name: impl Into<String>) -> Self {
        Self { kind: ToolChoiceType::Tool, name: Some(name.into()), disable_parallel_tool_use: None }
    }
}

// ---------------------------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------------------------

/// `LLMRequest`.
#[derive(Debug, Clone)]
pub struct LLMRequest {
    pub id: Option<String>,
    pub model: LanguageModel,
    pub system: Vec<SystemPart>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolEntry>,
    pub tool_choice: Option<ToolChoice>,
    pub generation: Option<GenerationOptions>,
    pub provider_options: Option<ProviderOptions>,
    pub http: Option<HttpOptions>,
    pub cache: Option<CachePolicy>,
    pub prompt_cache_key: Option<String>,
    pub metadata: Option<Object>,
}

impl LLMRequest {
    pub fn new(model: LanguageModel) -> Self {
        Self {
            id: None,
            model,
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            generation: None,
            provider_options: None,
            http: None,
            cache: None,
            prompt_cache_key: None,
            metadata: None,
        }
    }
    pub fn system(mut self, text: impl Into<String>) -> Self {
        self.system.push(SystemPart::text(text));
        self
    }
    pub fn message(mut self, message: Message) -> Self {
        self.messages.push(message);
        self
    }
    pub fn prompt(self, text: impl Into<String>) -> Self {
        self.message(Message::user_text(text))
    }
    pub fn tool(mut self, tool: ToolDefinition) -> Self {
        self.tools.push(ToolEntry::Tool(tool));
        self
    }
    pub fn generation(mut self, generation: GenerationOptions) -> Self {
        self.generation = Some(generation);
        self
    }
    pub fn provider_option(mut self, key: &str, value: Value) -> Self {
        self.provider_options.get_or_insert_with(Object::new).insert(key.into(), value);
        self
    }

    /// Generation options or the empty default.
    pub fn generation_options(&self) -> GenerationOptions {
        self.generation.clone().unwrap_or_default()
    }
    /// Provider option lookup (null treated as absent).
    pub fn po(&self, key: &str) -> Option<&Value> {
        self.provider_options.as_ref().and_then(|po| po.get(key)).filter(|v| !v.is_null())
    }
    pub fn po_str(&self, key: &str) -> Option<&str> {
        self.po(key).and_then(Value::as_str)
    }
    pub fn po_bool(&self, key: &str) -> Option<bool> {
        self.po(key).and_then(Value::as_bool)
    }
    pub fn compat(&self) -> Compatibility {
        self.model.compatibility.clone().unwrap_or_default()
    }
    /// `route.providerMetadataKey ?? provider`.
    pub fn metadata_key(&self) -> String {
        self.model.route.provider_metadata_key.clone().unwrap_or_else(|| self.model.provider.clone())
    }
    pub fn base_url(&self) -> String {
        self.model.route.endpoint.base_url.clone().unwrap_or_default()
    }
    pub fn route_id(&self) -> &str {
        &self.model.route.id
    }
}

// ---------------------------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------------------------

pub use oc_schema::types::FinishReason;

/// `Usage` (inclusive totals + non-overlapping breakdown).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<ProviderMetadata>,
}

impl Usage {
    pub fn visible_output_tokens(&self) -> u64 {
        self.output_tokens.unwrap_or(0).saturating_sub(self.reasoning_tokens.unwrap_or(0))
    }
    /// Convert into the session-level token record.
    pub fn to_token_usage(&self) -> oc_schema::types::TokenUsage {
        oc_schema::types::TokenUsage {
            input: self.non_cached_input_tokens.unwrap_or(0),
            output: self.visible_output_tokens(),
            reasoning: self.reasoning_tokens.unwrap_or(0),
            cache: oc_schema::types::CacheUsage {
                read: self.cache_read_input_tokens.unwrap_or(0),
                write: self.cache_write_input_tokens.unwrap_or(0),
            },
        }
    }
}

/// `FinishReasonDetails`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinishReasonDetails {
    pub normalized: FinishReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
}

impl FinishReasonDetails {
    pub fn new(normalized: FinishReason) -> Self {
        Self { normalized, raw: None, category: None, explanation: None }
    }
    pub fn raw(normalized: FinishReason, raw: Option<String>) -> Self {
        Self { normalized, raw, category: None, explanation: None }
    }
}

/// `LLMEvent`, tagged by `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum LLMEvent {
    Compaction(CompactionPart),
    StepStart {
        index: u32,
    },
    TextStart {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    TextDelta {
        id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    TextEnd {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ReasoningStart {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ReasoningDelta {
        id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ReasoningEnd {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ToolInputStart {
        id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ToolInputDelta {
        id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        text: String,
        /// Best-effort partial parse of all fragments so far (advisory).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<Value>,
    },
    ToolInputEnd {
        id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ToolInputError {
        id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        raw: String,
    },
    ToolCall {
        id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ToolResult {
        id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        result: ToolResultValue,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<ToolOutput>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ToolError {
        id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    Media {
        media: MediaAsset,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    StepFinish {
        index: u32,
        reason: FinishReasonDetails,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    Finish {
        reason: FinishReasonDetails,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ProviderError {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        classification: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
}

impl LLMEvent {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Compaction(_) => "compaction",
            Self::StepStart { .. } => "step-start",
            Self::TextStart { .. } => "text-start",
            Self::TextDelta { .. } => "text-delta",
            Self::TextEnd { .. } => "text-end",
            Self::ReasoningStart { .. } => "reasoning-start",
            Self::ReasoningDelta { .. } => "reasoning-delta",
            Self::ReasoningEnd { .. } => "reasoning-end",
            Self::ToolInputStart { .. } => "tool-input-start",
            Self::ToolInputDelta { .. } => "tool-input-delta",
            Self::ToolInputEnd { .. } => "tool-input-end",
            Self::ToolInputError { .. } => "tool-input-error",
            Self::ToolCall { .. } => "tool-call",
            Self::ToolResult { .. } => "tool-result",
            Self::ToolError { .. } => "tool-error",
            Self::Media { .. } => "media",
            Self::StepFinish { .. } => "step-finish",
            Self::Finish { .. } => "finish",
            Self::ProviderError { .. } => "provider-error",
        }
    }
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Finish { .. } | Self::ProviderError { .. })
    }
    pub fn usage(&self) -> Option<&Usage> {
        match self {
            Self::StepFinish { usage, .. } | Self::Finish { usage, .. } => usage.as_ref(),
            _ => None,
        }
    }
}
