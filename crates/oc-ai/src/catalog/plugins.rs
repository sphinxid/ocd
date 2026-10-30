//! Data-only effects of core provider plugins (`packages/core/src/plugin/provider/*.ts`) and the
//! models.dev plugin (`packages/core/src/plugin/models-dev.ts`), as catalog plugins and pure helpers.

use super::models_dev::{filter_snapshots, Snapshot};
use super::registry::{CatalogPlugin, ModelEditor, ProviderEditor};
use super::types::{HeaderMap, ProviderInfo};
use crate::types::Compatibility;
use std::collections::HashSet;

const OPENAI_COMPATIBLE: &str = "@opencode/ai/providers/openai-compatible";
const REFERER: (&str, &str) = ("HTTP-Referer", "https://opencode.ai/");
const TITLE: (&str, &str) = ("X-Title", "opencode");

/// `{...headers, ...extra}` (plain object spread: exact key match, later wins).
fn spread(headers: Option<&HeaderMap>, extra: &[(&str, &str)]) -> HeaderMap {
    let mut out = headers.cloned().unwrap_or_default();
    for (k, v) in extra {
        out.insert((*k).to_string(), (*v).to_string());
    }
    out
}

/// Request headers a provider plugin adds to a provider definition, if any. `configured` is the set of
/// registered integration ids (LLM Gateway only attributes configured providers).
pub fn attribution_headers(provider: &ProviderInfo, configured: &HashSet<String>) -> Option<HeaderMap> {
    let package = provider.package.as_str();
    let base_url = provider.base_url();
    let headers = provider.headers.as_ref();
    // Cerebras
    let mut result: Option<HeaderMap> = None;
    let current = |result: &Option<HeaderMap>| result.clone().or_else(|| headers.cloned());
    if package == "@opencode/ai/providers/cerebras" {
        result = Some(spread(current(&result).as_ref(), &[("X-Cerebras-3rd-Party-Integration", "opencode")]));
    }
    if package == OPENAI_COMPATIBLE && base_url == Some("https://api.kilo.ai/api/gateway") {
        result = Some(spread(current(&result).as_ref(), &[REFERER, TITLE]));
    }
    if provider.activation != super::types::Activation::Disabled
        && package == OPENAI_COMPATIBLE
        && base_url == Some("https://api.llmgateway.io/v1")
        && configured.contains(&provider.id)
    {
        result = Some(spread(current(&result).as_ref(), &[REFERER, TITLE, ("X-Source", "opencode")]));
    }
    if package == OPENAI_COMPATIBLE && base_url == Some("https://integrate.api.nvidia.com/v1") {
        let existing = current(&result);
        let origin = existing
            .as_ref()
            .and_then(|h| h.get("X-BILLING-INVOKE-ORIGIN").cloned())
            .unwrap_or_else(|| "OpenCode".to_string());
        result = Some(spread(existing.as_ref(), &[REFERER, TITLE, ("X-BILLING-INVOKE-ORIGIN", origin.as_str())]));
    }
    if package == "@opencode/ai/providers/openrouter" {
        result = Some(spread(current(&result).as_ref(), &[REFERER, TITLE]));
    }
    if package == OPENAI_COMPATIBLE && base_url == Some("https://zenmux.ai/api/v1") {
        let mut out: HeaderMap = [REFERER, TITLE].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        if let Some(existing) = current(&result) {
            for (k, v) in existing {
                out.insert(k, v);
            }
        }
        result = Some(out);
    }
    result
}

/// OpenRouter-specific OpenAI chat aliases disabled on OpenRouter providers.
pub const OPENROUTER_DISABLED_MODELS: &[&str] = &["gpt-5-chat-latest", "openai/gpt-5-chat"];

/// Catalog providers whose chat-completions endpoint accepts `prompt_cache_key`.
pub const PROMPT_CACHE_KEY_PROVIDERS: &[&str] = &[
    "abliteration-ai",
    "aihubmix",
    "anyapi",
    "auriko",
    "edenai",
    "jalapeno",
    "llama",
    "morph",
    "snowflake-cortex",
    "tencent-tokenhub",
    "upstage",
    "venice",
];

/// Env method overrides for integrations whose models.dev env list includes non-key values.
pub fn integration_env_override(integration_id: &str) -> Option<&'static [&'static str]> {
    match integration_id {
        "databricks" => Some(&["DATABRICKS_TOKEN"]),
        "neon" => Some(&["NEON_AI_GATEWAY_TOKEN"]),
        _ => None,
    }
}

/// models.dev plugin: adds filtered models.dev providers with `integrationID = provider id`.
pub struct ModelsDevPlugin {
    data: Vec<Snapshot>,
}

impl ModelsDevPlugin {
    pub fn new(data: &[Snapshot]) -> Self {
        Self { data: filter_snapshots(data) }
    }

    pub fn snapshots(&self) -> &[Snapshot] {
        &self.data
    }
}

impl CatalogPlugin for ModelsDevPlugin {
    fn providers(&mut self, editor: &mut ProviderEditor) {
        for provider in &self.data {
            let mut info = provider.info.clone();
            info.integration_id = Some(provider.info.id.clone());
            editor.add(info, provider.models.clone(), None);
        }
    }
}

/// Attribution / integration headers of the Cerebras, Kilo, LLM Gateway, Nvidia, OpenRouter and Zenmux
/// plugins, plus OpenRouter alias disabling and the prompt-cache-key compatibility flag.
pub struct DataPlugins {
    configured: HashSet<String>,
}

impl DataPlugins {
    /// `configured`: registered integration ids (for LLM Gateway).
    pub fn new(configured: HashSet<String>) -> Self {
        Self { configured }
    }
}

impl CatalogPlugin for DataPlugins {
    fn providers(&mut self, editor: &mut ProviderEditor) {
        for id in editor.ids() {
            let Some(record) = editor.get(&id) else { continue };
            if let Some(headers) = attribution_headers(&record.provider, &self.configured) {
                editor.update(&id, |provider| provider.headers = Some(headers));
            }
        }
        // prompt-cache-key.ts: set on the provider so configured provider and model compatibility can
        // override it.
        for id in PROMPT_CACHE_KEY_PROVIDERS {
            if editor.get(id).is_none() {
                continue;
            }
            editor.update(id, |provider| {
                let over = Compatibility { supports_prompt_cache_key: Some(true), ..Default::default() };
                provider.compatibility = Some(provider.compatibility.clone().unwrap_or_default().merged(&over));
            });
        }
    }

    fn models(&mut self, editor: &mut ModelEditor<'_>) {
        let openrouter: Vec<String> = editor
            .providers()
            .filter(|r| r.provider.package == "@opencode/ai/providers/openrouter")
            .map(|r| r.provider.id.clone())
            .collect();
        for pid in openrouter {
            for mid in OPENROUTER_DISABLED_MODELS {
                if editor.get(&pid, mid).is_none() {
                    continue;
                }
                editor.update(&pid, mid, |model| model.enabled = false);
            }
        }
        // databricks.ts: Databricks caches Claude only through explicit `cache_control` breakpoints.
        for (pid, mid) in editor.list(Some("databricks")) {
            let claude = editor.get(&pid, &mid).is_some_and(|m| m.model_id.contains("claude"));
            if !claude {
                continue;
            }
            editor.update(&pid, &mid, |model| {
                let base = Compatibility { cache_format: Some("anthropic".into()), ..Default::default() };
                model.compatibility = Some(base.merged(&model.compatibility.clone().unwrap_or_default()));
            });
        }
    }
}

/// The keyless part of the built-in OpenCode Zen provider plugin (`core/src/plugin/provider/opencode.ts`):
/// without `OPENCODE_API_KEY`, an active `opencode` connection or a configured `apiKey`, the `opencode`
/// provider is enabled with the `"public"` key and every model with a positive input price is disabled,
/// leaving the free models usable. Runs after models.dev and before configured providers, like upstream.
pub struct OpencodeZen {
    /// `OPENCODE_API_KEY` is set or the `opencode` integration has an active connection.
    pub connected: bool,
}

const OPENCODE_PROVIDER: &str = "opencode";

/// `OPENCODE_DISABLE_FREE_MODELS=1|true` hides zero-cost OpenCode models and keeps the keyless
/// `opencode` provider from turning itself on.
pub fn free_models_disabled() -> bool {
    std::env::var("OPENCODE_DISABLE_FREE_MODELS").is_ok_and(|v| matches!(v.to_lowercase().as_str(), "1" | "true"))
}

impl CatalogPlugin for OpencodeZen {
    fn providers(&mut self, editor: &mut ProviderEditor) {
        let Some(record) = editor.get(OPENCODE_PROVIDER) else { return };
        let has_key = self.connected || record.provider.setting("apiKey").is_some_and(truthy);
        if has_key || free_models_disabled() {
            return;
        }
        editor.update(OPENCODE_PROVIDER, |provider| {
            provider.activation = super::types::Activation::Enabled;
            let mut settings = provider.settings.clone().unwrap_or_default();
            settings.insert("apiKey".into(), serde_json::Value::String("public".into()));
            provider.settings = Some(settings);
        });
    }

    fn models(&mut self, editor: &mut ModelEditor<'_>) {
        let Some(record) = editor.provider(OPENCODE_PROVIDER) else { return };
        let real_key = record.provider.setting("apiKey").is_some_and(|k| truthy(k) && k.as_str() != Some("public"));
        if self.connected || real_key {
            return;
        }
        for (pid, mid) in editor.list(Some(OPENCODE_PROVIDER)) {
            let paid = editor.get(&pid, &mid).is_some_and(|m| m.cost.iter().any(|c| c.input > 0.0));
            if paid {
                editor.update(&pid, &mid, |model| model.enabled = false);
            }
        }
    }
}

/// The Responses WebSocket default of the OpenAI, xAI and Azure provider plugins
/// (`core/src/plugin/provider/{openai,xai,azure}.ts` provider transforms): `settings.transport` becomes
/// `"websocket"` unless already set. Runs among the provider plugins, before configured providers.
pub struct ResponsesWebSockets;

fn default_websocket(provider: &mut ProviderInfo) {
    let mut settings = provider.settings.clone().unwrap_or_default();
    if settings.get("transport").is_none_or(serde_json::Value::is_null) {
        settings.insert("transport".into(), serde_json::Value::String("websocket".into()));
    }
    provider.settings = Some(settings);
}

/// `responsesWebSocketCapable` (azure.ts): the v1 Responses API of an `*.openai.azure.com` resource.
pub fn azure_websocket_capable(provider: &ProviderInfo) -> bool {
    if provider.package != "@opencode/ai/providers/azure/responses" {
        return false;
    }
    if provider.setting("useDeploymentBasedUrls") == Some(&serde_json::Value::Bool(true)) {
        return false;
    }
    if provider.setting("apiVersion").is_some_and(|v| v.as_str() != Some("v1")) {
        return false;
    }
    let Some(base) = provider.setting("baseURL").and_then(serde_json::Value::as_str) else { return true };
    static AZURE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"(?i)^https://[^/]+\.openai\.azure\.com(?:/|$)").unwrap());
    AZURE.is_match(base)
}

impl CatalogPlugin for ResponsesWebSockets {
    fn providers(&mut self, editor: &mut ProviderEditor) {
        for id in ["openai", "xai"] {
            if editor.get(id).is_some() {
                editor.update(id, default_websocket);
            }
        }
        for id in editor.ids() {
            let Some(record) = editor.get(&id) else { continue };
            let provider = &record.provider;
            if provider.id != "azure" && !provider.package.starts_with("@opencode/ai/providers/azure/") {
                continue;
            }
            if azure_websocket_capable(provider) {
                editor.update(&id, default_websocket);
            }
        }
    }
}

/// JS truthiness of a settings value.
fn truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        serde_json::Value::String(s) => !s.is_empty(),
        _ => true,
    }
}
