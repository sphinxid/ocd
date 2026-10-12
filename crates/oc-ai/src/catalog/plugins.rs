//! Data-only effects of core provider plugins (`packages/core/src/plugin/provider/*.ts`) and the
//! models.dev plugin (`packages/core/src/plugin/models-dev.ts`), as catalog plugins and pure helpers.

use super::models_dev::{filter_snapshots, Snapshot};
use super::registry::{CatalogPlugin, ModelEditor, ProviderEditor};
use super::types::{is_aisdk, package_name, HeaderMap, ProviderInfo};
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
    if is_aisdk(Some(package)) && package_name(package) == "@ai-sdk/vercel" {
        result = Some(spread(current(&result).as_ref(), &[("http-referer", "https://opencode.ai/"), ("x-title", "opencode")]));
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

/// Attribution / integration headers of the Cerebras, Kilo, LLM Gateway, Nvidia, OpenRouter, Vercel and
/// Zenmux plugins, plus OpenRouter alias disabling and the prompt-cache-key compatibility flag.
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
        for pid in PROMPT_CACHE_KEY_PROVIDERS {
            for (pid, mid) in editor.list(Some(pid)) {
                editor.update(&pid, &mid, |model| {
                    let over = Compatibility { supports_prompt_cache_key: Some(true), ..Default::default() };
                    model.compatibility = Some(model.compatibility.clone().unwrap_or_default().merged(&over));
                });
            }
        }
    }
}
