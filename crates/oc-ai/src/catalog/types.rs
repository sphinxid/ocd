//! Catalog data model: `Provider.Info` / `Model.Info` (`packages/schema/src/{provider,model}.ts`) and the
//! overlay helpers of `packages/core/src/{provider,model}.ts`.
//!
//! Struct field order follows the upstream schema declaration order, so serialized JSON matches the
//! schema-encoded upstream output key for key.

use crate::json::Object;
use crate::types::{Compatibility, HttpOptions, HttpTimeout, DEFAULT_HTTP_TIMEOUT_MS};
use indexmap::IndexMap;
use oc_schema::types::ModelRef;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// Header overlay (`Record<string, string>`), insertion ordered.
pub type HeaderMap = IndexMap<String, String>;

pub(crate) fn ser_num<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    crate::json::num(*value).serialize(serializer)
}

pub(crate) fn de_num<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    let value = Value::deserialize(deserializer)?;
    value.as_f64().ok_or_else(|| serde::de::Error::custom("expected a number"))
}

// ---------------------------------------------------------------------------------------------
// Packages
// ---------------------------------------------------------------------------------------------

/// `Provider.AISDK_PREFIX`.
pub const AISDK_PREFIX: &str = "aisdk:";

/// `Provider.isAISDK`.
pub fn is_aisdk(value: Option<&str>) -> bool {
    value.map(|v| v.starts_with(AISDK_PREFIX)).unwrap_or(false)
}

/// `Provider.aisdk`: prefix an npm package name with `aisdk:` (idempotent).
pub fn aisdk(value: &str) -> String {
    if value.starts_with(AISDK_PREFIX) { value.to_string() } else { format!("{AISDK_PREFIX}{value}") }
}

/// `Provider.packageName`: strip `aisdk:`, migrate the legacy `@opencode-ai/ai/` scope.
pub fn package_name(value: &str) -> String {
    if value.starts_with("@opencode-ai/ai/") {
        return value.replacen("@opencode-ai/", "@opencode/", 1);
    }
    if !value.starts_with(AISDK_PREFIX) {
        return value.to_string();
    }
    value[AISDK_PREFIX.len()..].to_string()
}

/// Native provider entrypoints bundled with Core (`packages/core/src/provider.ts` `builtins`).
pub const BUILTIN_PACKAGES: &[&str] = &[
    "@opencode/ai/providers/amazon-bedrock",
    "@opencode/ai/providers/amazon-bedrock/mantle",
    "@opencode/ai/providers/amazon-bedrock/mantle/chat",
    "@opencode/ai/providers/amazon-bedrock/mantle/responses",
    "@opencode/ai/providers/alibaba/chat",
    "@opencode/ai/providers/alibaba/messages",
    "@opencode/ai/providers/alibaba/responses",
    "@opencode/ai/providers/anthropic",
    "@opencode/ai/providers/anthropic-compatible",
    "@opencode/ai/providers/azure",
    "@opencode/ai/providers/azure/chat",
    "@opencode/ai/providers/azure/responses",
    "@opencode/ai/providers/baseten",
    "@opencode/ai/providers/cerebras",
    "@opencode/ai/providers/cloudflare-ai-gateway",
    "@opencode/ai/providers/cloudflare-workers-ai",
    "@opencode/ai/providers/cohere",
    "@opencode/ai/providers/cohere/chat",
    "@opencode/ai/providers/deepinfra",
    "@opencode/ai/providers/deepseek",
    "@opencode/ai/providers/digitalocean",
    "@opencode/ai/providers/fireworks",
    "@opencode/ai/providers/google",
    "@opencode/ai/providers/google/interactions",
    "@opencode/ai/providers/google-vertex",
    "@opencode/ai/providers/google-vertex/gemini",
    "@opencode/ai/providers/google-vertex/interactions",
    "@opencode/ai/providers/google-vertex/chat",
    "@opencode/ai/providers/google-vertex/responses",
    "@opencode/ai/providers/google-vertex/messages",
    "@opencode/ai/providers/google-vertex/mistral",
    "@opencode/ai/providers/groq",
    "@opencode/ai/providers/meta/chat",
    "@opencode/ai/providers/meta/messages",
    "@opencode/ai/providers/meta/responses",
    "@opencode/ai/providers/minimax/chat",
    "@opencode/ai/providers/minimax/messages",
    "@opencode/ai/providers/minimax/responses",
    "@opencode/ai/providers/mistral",
    "@opencode/ai/providers/moonshot/chat",
    "@opencode/ai/providers/moonshot/messages",
    "@opencode/ai/providers/moonshot/responses",
    "@opencode/ai/providers/openai",
    "@opencode/ai/providers/openai/chat",
    "@opencode/ai/providers/openai/responses",
    "@opencode/ai/providers/openai-compatible",
    "@opencode/ai/providers/openai-compatible/responses",
    "@opencode/ai/providers/openai-compatible-responses",
    "@opencode/ai/providers/openrouter",
    "@opencode/ai/providers/togetherai",
    "@opencode/ai/providers/venice",
    "@opencode/ai/providers/vercel-ai-gateway",
    "@opencode/ai/providers/xai",
    "@opencode/ai/providers/zai/chat",
    "@opencode/ai/providers/zai-coding-plan/chat",
    "@opencode/ai/providers/zai-coding-plan/messages",
    "@opencode/ai/providers/zai-coding-plan/responses",
];

// ---------------------------------------------------------------------------------------------
// Provider.Info
// ---------------------------------------------------------------------------------------------

/// `Provider.Activation`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Activation {
    #[default]
    Auto,
    Enabled,
    Disabled,
}

/// `Provider.Info`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical: Option<String>,
    #[serde(rename = "integrationID", default, skip_serializing_if = "Option::is_none")]
    pub integration_id: Option<String>,
    pub name: String,
    pub activation: Activation,
    pub package: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Object>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeaderMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Object>,
}

impl ProviderInfo {
    /// `Provider.Info.empty(id)`.
    pub fn empty(id: &str) -> Self {
        Self {
            id: id.into(),
            canonical: None,
            integration_id: None,
            name: id.into(),
            activation: Activation::Auto,
            package: String::new(),
            settings: None,
            headers: None,
            body: None,
        }
    }

    /// A provider setting (`settings[key]`).
    pub fn setting(&self, key: &str) -> Option<&Value> {
        self.settings.as_ref().and_then(|s| s.get(key))
    }

    /// `settings.baseURL` when it is a string.
    pub fn base_url(&self) -> Option<&str> {
        self.setting("baseURL").and_then(Value::as_str)
    }

    /// `settings.transport` (`"http" | "websocket"`).
    pub fn transport(&self) -> Option<&str> {
        self.setting("transport").and_then(Value::as_str)
    }
}

// ---------------------------------------------------------------------------------------------
// Model.Info
// ---------------------------------------------------------------------------------------------

/// `Model.Capabilities`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub tools: bool,
    pub input: Vec<String>,
    pub output: Vec<String>,
}

impl Default for Capabilities {
    /// `Capabilities.default()`.
    fn default() -> Self {
        Self { tools: true, input: vec!["text".into(), "image".into()], output: vec!["text".into()] }
    }
}

/// Partial capabilities (`Config.Model.Capabilities`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PartialCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Vec<String>>,
}

/// `Model.mergeCapabilities`: partial config capabilities over a base, defaulting unset fields.
pub fn merge_capabilities(config: &PartialCapabilities, base: Option<&Capabilities>) -> Capabilities {
    let fallback = base.cloned().unwrap_or_default();
    Capabilities {
        tools: config.tools.unwrap_or(fallback.tools),
        input: config.input.clone().unwrap_or(fallback.input),
        output: config.output.clone().unwrap_or(fallback.output),
    }
}

/// `Model.Cost.tier`: `{type: "context", size}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostTier {
    #[serde(rename = "type")]
    pub kind: String,
    pub size: i64,
}

impl CostTier {
    pub fn context(size: i64) -> Self {
        Self { kind: "context".into(), size }
    }
}

/// `Model.Cost.cache`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CostCache {
    #[serde(serialize_with = "ser_num", deserialize_with = "de_num")]
    pub read: f64,
    #[serde(serialize_with = "ser_num", deserialize_with = "de_num")]
    pub write: f64,
}

/// `Model.Cost` (USD per million tokens).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<CostTier>,
    #[serde(serialize_with = "ser_num", deserialize_with = "de_num")]
    pub input: f64,
    #[serde(serialize_with = "ser_num", deserialize_with = "de_num")]
    pub output: f64,
    pub cache: CostCache,
}

/// `Model.Variant`: an overlay of settings (provider options), headers and body.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Variant {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Object>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeaderMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Object>,
}

/// `Model.Info.status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelStatus {
    Alpha,
    Beta,
    Deprecated,
    #[default]
    Active,
}

impl ModelStatus {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "alpha" => Some(Self::Alpha),
            "beta" => Some(Self::Beta),
            "deprecated" => Some(Self::Deprecated),
            "active" => Some(Self::Active),
            _ => None,
        }
    }
}

/// `Model.Info.time`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelTime {
    /// Epoch milliseconds (`Date.parse(release_date)`, `0` when unknown).
    #[serde(serialize_with = "ser_num", deserialize_with = "de_num")]
    pub released: f64,
}

/// `Model.Info.limit`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Limit {
    pub context: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<i64>,
    pub output: i64,
}

impl Default for Limit {
    fn default() -> Self {
        Self { context: 200_000, input: None, output: 32_000 }
    }
}

/// `Model.Info`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    /// API model id sent to the provider.
    #[serde(rename = "modelID")]
    pub model_id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<Compatibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Object>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeaderMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Object>,
    pub capabilities: Capabilities,
    pub variants: Vec<Variant>,
    pub time: ModelTime,
    pub cost: Vec<Cost>,
    pub status: ModelStatus,
    pub enabled: bool,
    pub limit: Limit,
}

impl ModelInfo {
    /// `Model.Info.default(providerID, id)`.
    pub fn default_for(provider_id: &str, id: &str) -> Self {
        Self {
            id: id.into(),
            model_id: id.into(),
            provider_id: provider_id.into(),
            canonical: None,
            family: None,
            name: id.into(),
            compatibility: None,
            package: None,
            settings: None,
            headers: None,
            body: None,
            capabilities: Capabilities::default(),
            variants: vec![],
            time: ModelTime { released: 0.0 },
            cost: vec![],
            status: ModelStatus::Active,
            enabled: true,
            limit: Limit::default(),
        }
    }

    /// `"<providerID>/<id>"`.
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider_id, self.id)
    }

    /// `ModelResolver.hasPackage`.
    pub fn has_package(&self) -> bool {
        self.package.as_deref().map(|p| !p.is_empty()).unwrap_or(false)
    }

    pub fn supports_text(&self) -> bool {
        supports_text(&self.capabilities)
    }

    pub fn variant(&self, id: &str) -> Option<&Variant> {
        self.variants.iter().find(|v| v.id == id)
    }
}

/// `Model.supportsText`: empty modality lists count as text.
pub fn supports_text(capabilities: &Capabilities) -> bool {
    let text = |items: &[String]| items.is_empty() || items.iter().any(|i| i.starts_with("text"));
    text(&capabilities.input) && text(&capabilities.output)
}

/// `Model.compatibility(interleaved)`: a string or `{field}` names the reasoning field.
pub fn compatibility_from_interleaved(input: &Value) -> Option<Compatibility> {
    let field = match input {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => match o.get("field") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }?;
    Some(Compatibility { reasoning_field: Some(field), ..Default::default() })
}

/// Keep only the catalog-level `Model.Compatibility` fields (schema decoding drops the rest).
pub fn catalog_compatibility(input: &Compatibility) -> Compatibility {
    Compatibility {
        reasoning_field: input.reasoning_field.clone(),
        require_reasoning: input.require_reasoning,
        max_tokens_field: input.max_tokens_field.clone(),
        require_finish_reason: input.require_finish_reason,
        require_assistant_after_tool: input.require_assistant_after_tool,
        supports_prompt_cache_key: input.supports_prompt_cache_key,
        supports_thinking_block_binding: input.supports_thinking_block_binding,
        supports_effort_updates: input.supports_effort_updates,
        ..Default::default()
    }
}

/// Decode a catalog `Model.Compatibility` JSON object (unknown keys dropped, invalid → `None`).
pub fn decode_compatibility(value: &Value) -> Option<Compatibility> {
    let decoded: Compatibility = serde_json::from_value(value.clone()).ok()?;
    Some(catalog_compatibility(&decoded))
}

// ---------------------------------------------------------------------------------------------
// Model refs
// ---------------------------------------------------------------------------------------------

/// `Model.parse` (core): split at the first `/` only.
pub fn parse_model(input: &str) -> (String, String) {
    match input.find('/') {
        None => (input.to_string(), String::new()),
        Some(index) => (input[..index].to_string(), input[index + 1..].to_string()),
    }
}

/// `Model.Ref.parse`: `"<providerID>/<modelID>[#<variant>]"`.
pub fn parse_model_ref(input: &str) -> Result<ModelRef, String> {
    let invalid = || format!("Invalid model reference: {input}");
    let provider_end = match input.find('/') {
        Some(i) if i > 0 => i,
        _ => return Err(invalid()),
    };
    let provider_id = &input[..provider_end];
    let variant_start = input[provider_end + 1..].find('#').map(|i| i + provider_end + 1);
    let id = match variant_start {
        Some(v) => &input[provider_end + 1..v],
        None => &input[provider_end + 1..],
    };
    let variant = variant_start.map(|v| &input[v + 1..]);
    if id.is_empty() || provider_id.contains('#') || variant.map(|v| v.is_empty() || v.contains('#')).unwrap_or(false) {
        return Err(invalid());
    }
    Ok(ModelRef { id: id.into(), provider_id: provider_id.into(), variant: variant.map(String::from) })
}

// ---------------------------------------------------------------------------------------------
// Overlays
// ---------------------------------------------------------------------------------------------

/// `Provider.mergeOverlay`: keys of `base` then new keys of `overlay`; nested records merge, everything
/// else is replaced by the overlay value.
pub fn merge_overlay(base: Option<&Object>, overlay: Option<&Object>) -> Option<Object> {
    match (base, overlay) {
        (None, None) => None,
        (None, Some(o)) => Some(o.clone()),
        (Some(b), None) => Some(b.clone()),
        (Some(b), Some(o)) => Some(merge_objects(b, o)),
    }
}

fn merge_objects(base: &Object, overlay: &Object) -> Object {
    let mut out = base.clone();
    for (key, right) in overlay {
        let merged = match (out.get(key), right) {
            (Some(Value::Object(left)), Value::Object(right)) => Value::Object(merge_objects(left, right)),
            _ => right.clone(),
        };
        out.insert(key.clone(), merged);
    }
    out
}

/// `Provider.mergeHeaders`: case-insensitive merge; a header keeps its first position and takes the
/// later spelling and value.
pub fn merge_headers(base: Option<&HeaderMap>, overlay: Option<&HeaderMap>) -> Option<HeaderMap> {
    match (base, overlay) {
        (None, None) => None,
        (None, Some(o)) => Some(o.clone()),
        (Some(b), None) => Some(b.clone()),
        (Some(b), Some(o)) => {
            let mut by_lower: IndexMap<String, (String, String)> = IndexMap::new();
            for (k, v) in b.iter().chain(o.iter()) {
                by_lower.insert(k.to_lowercase(), (k.clone(), v.clone()));
            }
            Some(by_lower.into_values().collect())
        }
    }
}

/// Settings consumed by Core; native packages never receive them.
pub const CORE_KEYS: &[&str] = &["chunkTimeout", "compaction", "fetch", "headerTimeout", "timeout", "transport"];
/// Provider-only settings that models do not inherit.
pub const PROVIDER_ONLY_KEYS: &[&str] = &["chunkTimeout", "headerTimeout", "timeout", "transport"];

pub(crate) fn omit(settings: &Object, keys: &[&str]) -> Object {
    settings.iter().filter(|(k, _)| !keys.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// `Provider.nativeSettings`.
pub fn native_settings(settings: &Object) -> Object {
    omit(settings, CORE_KEYS)
}

/// `Provider.modelSettings`.
pub fn model_settings(settings: Option<&Object>) -> Option<Object> {
    settings.map(|s| omit(s, PROVIDER_ONLY_KEYS))
}

/// `Provider.DEFAULT_TIMEOUT_MS`.
pub const DEFAULT_TIMEOUT_MS: f64 = DEFAULT_HTTP_TIMEOUT_MS;

/// `Provider.timeout`: one timeout setting; invalid values are dropped.
pub fn timeout(value: Option<&Value>) -> Option<HttpTimeout> {
    value.and_then(HttpTimeout::from_value)
}

/// `Provider.timeouts`: `{timeout?, headerTimeout ?? 300000, chunkTimeout ?? 300000}`.
pub fn timeouts(settings: &Object) -> HttpOptions {
    HttpOptions {
        timeout: timeout(settings.get("timeout")),
        header_timeout: Some(timeout(settings.get("headerTimeout")).unwrap_or(HttpTimeout::Millis(DEFAULT_TIMEOUT_MS))),
        chunk_timeout: Some(timeout(settings.get("chunkTimeout")).unwrap_or(HttpTimeout::Millis(DEFAULT_TIMEOUT_MS))),
        ..Default::default()
    }
}

/// Convert a header map to a JSON object (for `settings.headers`).
pub fn headers_to_object(headers: &HeaderMap) -> Object {
    headers.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect()
}

/// Read a `Record<string, string>` (non-string values make the whole record invalid).
pub fn headers_from_value(value: &Value) -> Option<HeaderMap> {
    let object = value.as_object()?;
    let mut out = HeaderMap::new();
    for (k, v) in object {
        out.insert(k.clone(), v.as_str()?.to_string());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Object {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn parse_at_first_slash() {
        for (input, p, m) in [
            ("vendor/model", "vendor", "model"),
            ("vendor/team/model", "vendor", "team/model"),
            ("vendor", "vendor", ""),
            ("", "", ""),
            ("/model", "", "model"),
            ("vendor/", "vendor", ""),
            ("vendor//model/", "vendor", "/model/"),
        ] {
            assert_eq!(parse_model(input), (p.to_string(), m.to_string()), "{input}");
        }
    }

    #[test]
    fn parse_refs() {
        let r = parse_model_ref("anthropic/claude#high").unwrap();
        assert_eq!((r.provider_id.as_str(), r.id.as_str(), r.variant.as_deref()), ("anthropic", "claude", Some("high")));
        let r = parse_model_ref("a/b/c").unwrap();
        assert_eq!(r.id, "b/c");
        assert_eq!(r.variant, None);
        for bad in ["/x", "x", "a/", "a/b#", "a/b#c#d", "a#b/c"] {
            assert_eq!(parse_model_ref(bad).unwrap_err(), format!("Invalid model reference: {bad}"), "{bad}");
        }
    }

    #[test]
    fn overlay_and_headers() {
        let merged = merge_overlay(Some(&obj(json!({"a": {"x": 1}, "b": 1}))), Some(&obj(json!({"a": {"y": 2}, "c": 3})))).unwrap();
        assert_eq!(serde_json::to_string(&merged).unwrap(), r#"{"a":{"x":1,"y":2},"b":1,"c":3}"#);
        let a: HeaderMap = [("X-A".to_string(), "1".to_string()), ("b".into(), "1".into())].into_iter().collect();
        let b: HeaderMap = [("x-a".to_string(), "2".to_string()), ("c".into(), "3".into())].into_iter().collect();
        let h = merge_headers(Some(&a), Some(&b)).unwrap();
        assert_eq!(h.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(), ["x-a=2", "b=1", "c=3"]);
    }

    #[test]
    fn native_and_model_settings() {
        let s = obj(json!({"apiKey":"secret","reasoningEffort":"high","chunkTimeout":1000,"compaction":{"type":"native"},"transport":"websocket"}));
        assert_eq!(Value::Object(native_settings(&s)), json!({"apiKey":"secret","reasoningEffort":"high"}));
        let s = obj(json!({"timeout":60000,"chunkTimeout":30000,"transport":"websocket","compaction":{"type":"native"},"reasoningEffort":"high"}));
        assert_eq!(Value::Object(model_settings(Some(&s)).unwrap()), json!({"compaction":{"type":"native"},"reasoningEffort":"high"}));
    }

    #[test]
    fn interleaved_compat() {
        assert_eq!(compatibility_from_interleaved(&json!("reasoning_text")).unwrap().reasoning_field.as_deref(), Some("reasoning_text"));
        assert_eq!(
            compatibility_from_interleaved(&json!({"field": "vendor_reasoning"})).unwrap().reasoning_field.as_deref(),
            Some("vendor_reasoning")
        );
        assert!(compatibility_from_interleaved(&json!(true)).is_none());
        assert!(compatibility_from_interleaved(&json!(false)).is_none());
    }

    #[test]
    fn default_model_json_shape() {
        let m = ModelInfo::default_for("p", "m");
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"id":"m","modelID":"m","providerID":"p","name":"m","capabilities":{"tools":true,"input":["text","image"],"output":["text"]},"variants":[],"time":{"released":0},"cost":[],"status":"active","enabled":true,"limit":{"context":200000,"output":32000}}"#
        );
    }
}
