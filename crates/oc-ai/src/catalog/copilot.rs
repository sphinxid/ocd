//! GitHub Copilot catalog data: the live `/models` list (`packages/core/src/github-copilot/models.ts`)
//! and the provider/model transforms of the `opencode.provider.github.copilot` plugin
//! (`packages/core/src/plugin/provider/github-copilot.ts:216-244`).
//!
//! Without a synced model list every Copilot model goes through upstream's in-tree AI SDK provider
//! (`aisdk:@ai-sdk/github-copilot`, run by the plugin sidecar), pointed at the connection's API base
//! when an OAuth connection exists. With a list, the catalog is rebuilt from it: models advertising
//! `/v1/messages` use the native Anthropic package at `<base>/v1`, the others the AI SDK provider
//! with an `endpoint` setting (`responses` or `chat`) that picks the language model.

use std::sync::Arc;

use indexmap::IndexMap;
use serde_json::{json, Value};

use super::registry::{CatalogPlugin, ModelEditor, ProviderEditor};
use super::types::{merge_overlay, Capabilities, Cost, CostCache, Limit, ModelInfo, ModelStatus, ModelTime, Variant};
use crate::json::Object;

pub const PROVIDER_ID: &str = "github-copilot";
/// `Provider.aisdk("@ai-sdk/github-copilot")`.
pub const PACKAGE: &str = "aisdk:@ai-sdk/github-copilot";
/// `X-GitHub-Api-Version` of Copilot API requests.
pub const API_VERSION: &str = "2026-08-01";
/// The chat-only alias the model transform hides.
pub const CHAT_ALIAS: &str = "gpt-5-chat-latest";
/// The Copilot client's "small utility" models used for session titles.
pub const UTILITY_TITLE_MODELS: [&str; 1] = ["gpt-4o-mini"];

/// Token prices of one billing batch (`billing.token_prices`).
#[derive(Debug, Clone, PartialEq)]
pub struct Prices {
    pub batch_size: f64,
    pub cache_price: Option<f64>,
    pub cache_read_price: Option<f64>,
    pub input_price: f64,
    pub output_price: f64,
}

/// One usable entry of the `/models` answer (`RemoteModel` with output/prompt limits and tool support).
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteModel {
    pub model_picker_enabled: bool,
    pub id: String,
    pub name: String,
    pub version: String,
    pub supported_endpoints: Option<Vec<String>>,
    pub policy_state: Option<String>,
    pub prices: Option<Prices>,
    pub family: String,
    pub max_context_window_tokens: Option<f64>,
    pub max_output_tokens: Option<f64>,
    pub max_prompt_tokens: Option<f64>,
    /// `limits.vision.supported_media_types`.
    pub vision_media_types: Option<Vec<String>>,
    pub adaptive_thinking: Option<bool>,
    pub max_thinking_budget: Option<f64>,
    pub reasoning_effort: Option<Vec<String>>,
    pub tool_calls: Option<bool>,
    pub vision: Option<bool>,
}

/// Usable remote models by id, in answer order (`CopilotModels.Snapshot`).
pub type Snapshot = IndexMap<String, RemoteModel>;

// ------------------------------------------------------------------------------------------------
// Decoding (effect Schema semantics: unknown keys are ignored, an optional key may be absent but not
// null, and any mistyped field rejects the whole entry)
// ------------------------------------------------------------------------------------------------

/// Optional field: absent → `Some(None)`, present with the right type → `Some(Some(v))`, else `None`.
fn optional<T>(object: &Object, key: &str, decode: impl Fn(&Value) -> Option<T>) -> Option<Option<T>> {
    match object.get(key) {
        None => Some(None),
        Some(value) => decode(value).map(Some),
    }
}

fn string(value: &Value) -> Option<String> {
    value.as_str().map(String::from)
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64()
}

fn boolean(value: &Value) -> Option<bool> {
    value.as_bool()
}

fn strings(value: &Value) -> Option<Vec<String>> {
    value.as_array()?.iter().map(string).collect()
}

fn object(value: &Value) -> Option<&Object> {
    value.as_object()
}

fn decode_prices(value: &Value) -> Option<Prices> {
    let prices = object(value)?;
    let default = object(prices.get("default")?)?;
    Some(Prices {
        batch_size: number(prices.get("batch_size")?)?,
        cache_price: optional(default, "cache_price", number)?,
        cache_read_price: optional(default, "cache_read_price", number)?,
        input_price: number(default.get("input_price")?)?,
        output_price: number(default.get("output_price")?)?,
    })
}

/// `decodeModel`: one `/models` entry, `None` when it does not match the schema.
pub fn decode_model(value: &Value) -> Option<RemoteModel> {
    let raw = object(value)?;
    let policy_state = match optional(raw, "policy", |v| object(v).cloned())? {
        Some(policy) => optional(&policy, "state", string)?,
        None => None,
    };
    let prices = match optional(raw, "billing", |v| object(v).cloned())? {
        Some(billing) => optional(&billing, "token_prices", decode_prices)?,
        None => None,
    };
    let capabilities = object(raw.get("capabilities")?)?;
    let family = string(capabilities.get("family")?)?;
    let mut model = RemoteModel {
        model_picker_enabled: boolean(raw.get("model_picker_enabled")?)?,
        id: string(raw.get("id")?)?,
        name: string(raw.get("name")?)?,
        version: string(raw.get("version")?)?,
        supported_endpoints: optional(raw, "supported_endpoints", strings)?,
        policy_state,
        prices,
        family,
        max_context_window_tokens: None,
        max_output_tokens: None,
        max_prompt_tokens: None,
        vision_media_types: None,
        adaptive_thinking: None,
        max_thinking_budget: None,
        reasoning_effort: None,
        tool_calls: None,
        vision: None,
    };
    if let Some(limits) = optional(capabilities, "limits", |v| object(v).cloned())? {
        model.max_context_window_tokens = optional(&limits, "max_context_window_tokens", number)?;
        model.max_output_tokens = optional(&limits, "max_output_tokens", number)?;
        model.max_prompt_tokens = optional(&limits, "max_prompt_tokens", number)?;
        if let Some(vision) = optional(&limits, "vision", |v| object(v).cloned())? {
            number(vision.get("max_prompt_image_size")?)?;
            number(vision.get("max_prompt_images")?)?;
            model.vision_media_types = Some(strings(vision.get("supported_media_types")?)?);
        }
    }
    let supports = object(capabilities.get("supports")?)?;
    model.adaptive_thinking = optional(supports, "adaptive_thinking", boolean)?;
    model.max_thinking_budget = optional(supports, "max_thinking_budget", number)?;
    optional(supports, "min_thinking_budget", number)?;
    model.reasoning_effort = optional(supports, "reasoning_effort", strings)?;
    optional(supports, "streaming", boolean)?;
    optional(supports, "structured_outputs", boolean)?;
    model.tool_calls = optional(supports, "tool_calls", boolean)?;
    model.vision = optional(supports, "vision", boolean)?;
    Some(model)
}

/// `usable`: not disabled by policy, with output and prompt limits and a tool-call flag.
pub fn usable(model: &RemoteModel) -> bool {
    model.policy_state.as_deref() != Some("disabled")
        && model.max_output_tokens.is_some()
        && model.max_prompt_tokens.is_some()
        && model.tool_calls.is_some()
}

/// `load`'s decoding of a `/models` answer: `{data: unknown[]}` (else an error), keeping usable entries.
pub fn decode_snapshot(body: &Value) -> Result<Snapshot, String> {
    let data = body
        .as_object()
        .and_then(|o| o.get("data"))
        .and_then(Value::as_array)
        .ok_or_else(|| "Expected { readonly data: ReadonlyArray<unknown> }".to_string())?;
    let mut snapshot = Snapshot::new();
    for raw in data {
        if let Some(model) = decode_model(raw).filter(usable) {
            snapshot.insert(model.id.clone(), model);
        }
    }
    Ok(snapshot)
}

// ------------------------------------------------------------------------------------------------
// derive / build
// ------------------------------------------------------------------------------------------------

fn variants(remote: &RemoteModel, messages: bool) -> Vec<Variant> {
    let efforts = remote.reasoning_effort.clone().unwrap_or_default();
    let variant = |id: &str, settings: Value| Variant { id: id.to_string(), settings: settings.as_object().cloned(), headers: None, body: None };
    if !messages && !efforts.is_empty() {
        return efforts
            .iter()
            .map(|effort| {
                variant(effort, json!({"reasoningEffort": effort, "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]}))
            })
            .collect();
    }
    if !efforts.is_empty() && remote.adaptive_thinking == Some(true) {
        return efforts
            .iter()
            .map(|effort| variant(effort, json!({"thinking": {"type": "adaptive", "display": "summarized"}, "effort": effort})))
            .collect();
    }
    let Some(max) = remote.max_thinking_budget else { return vec![] };
    vec![
        variant("max", json!({"thinking": {"type": "enabled", "budgetTokens": crate::json::num(max - 1.0)}})),
        variant("high", json!({"thinking": {"type": "enabled", "budgetTokens": crate::json::num((max / 2.0).floor())}})),
    ]
}

/// `build`: a catalog model from a remote entry, keeping a source template's identity and overlays.
pub fn build(id: &str, remote: &RemoteModel, base_url: &str, previous: Option<&ModelInfo>) -> ModelInfo {
    let endpoints = remote.supported_endpoints.as_deref().unwrap_or_default();
    let has = |e: &str| endpoints.iter().any(|x| x == e);
    let messages = has("/v1/messages");
    let endpoint = if messages {
        Some("messages")
    } else if has("/responses") {
        Some("responses")
    } else if has("/chat/completions") {
        Some("chat")
    } else {
        None
    };
    let vision = remote.vision.unwrap_or(false);
    let media = remote.vision_media_types.as_deref().unwrap_or_default();
    let image = vision || media.iter().any(|m| m.starts_with("image/"));
    let pdf = vision && remote.vision_media_types.as_ref().is_some_and(|m| m.iter().any(|x| x == "application/pdf"));
    let mut input = vec!["text".to_string()];
    if image {
        input.push("image".into());
    }
    if pdf {
        input.push("pdf".into());
    }
    let prices = remote.prices.as_ref();
    // Copilot reports AIC per billing batch; the catalog stores USD per million tokens.
    let usd_per_million = match prices {
        Some(p) if p.batch_size > 0.0 => 10_000.0 / p.batch_size,
        _ => 0.0,
    };
    let prefix = format!("{}-", remote.id);
    let version = remote.version.strip_prefix(&prefix).unwrap_or(&remote.version);
    let previous_released = previous.map(|p| p.time.released).filter(|r| *r != 0.0 && !r.is_nan());
    let released = previous_released.or_else(|| crate::js_date::parse(version)).unwrap_or(0.0);
    let mut overlay = Object::new();
    overlay.insert("baseURL".into(), json!(if messages { format!("{base_url}/v1") } else { base_url.to_string() }));
    if let Some(endpoint) = endpoint {
        overlay.insert("endpoint".into(), json!(endpoint));
    }
    let mut model = ModelInfo::default_for(PROVIDER_ID, id);
    model.id = id.to_string();
    model.model_id = remote.id.clone();
    model.provider_id = PROVIDER_ID.to_string();
    model.family = Some(previous.and_then(|p| p.family.clone()).unwrap_or_else(|| remote.family.clone()));
    model.name = previous.map(|p| p.name.clone()).unwrap_or_else(|| remote.name.clone());
    model.package = Some(if messages { "@opencode/ai/providers/anthropic".into() } else { PACKAGE.into() });
    model.settings = merge_overlay(previous.and_then(|p| p.settings.as_ref()), Some(&overlay));
    model.headers = previous.and_then(|p| p.headers.clone());
    model.body = previous.and_then(|p| p.body.clone());
    model.capabilities = Capabilities { tools: remote.tool_calls.unwrap_or(false), input, output: vec!["text".into()], temperature: None };
    model.variants = variants(remote, messages);
    model.time = ModelTime { released: if released.is_finite() { released } else { 0.0 } };
    model.cost = vec![Cost {
        tier: None,
        input: prices.map(|p| p.input_price).unwrap_or(0.0) * usd_per_million,
        output: prices.map(|p| p.output_price).unwrap_or(0.0) * usd_per_million,
        cache: CostCache {
            read: prices.and_then(|p| p.cache_read_price.or(p.cache_price)).unwrap_or(0.0) * usd_per_million,
            write: 0.0,
        },
    }];
    model.status = ModelStatus::Active;
    model.enabled = remote.model_picker_enabled;
    let prompt = remote.max_prompt_tokens.unwrap_or_default();
    model.limit = Limit {
        context: remote.max_context_window_tokens.unwrap_or(prompt) as i64,
        input: Some(prompt as i64),
        output: remote.max_output_tokens.unwrap_or_default() as i64,
    };
    model
}

/// `derive`: remote facts over the source templates. Templates whose API model is gone are dropped,
/// the rest keep their position; remote models without a template are appended.
pub fn derive(base_url: &str, remote: &Snapshot, existing: &[ModelInfo]) -> Vec<ModelInfo> {
    let mut result: IndexMap<String, ModelInfo> = IndexMap::new();
    for model in existing {
        result.insert(model.id.clone(), model.clone());
    }
    let ids: Vec<String> = result.keys().cloned().collect();
    for id in ids {
        let template = result[&id].clone();
        match remote.get(&template.model_id) {
            None => {
                result.shift_remove(&id);
            }
            Some(current) => {
                result.insert(id.clone(), build(&id, current, base_url, Some(&template)));
            }
        }
    }
    for (id, model) in remote {
        if result.contains_key(id) {
            continue;
        }
        result.insert(id.clone(), build(id, model, base_url, None));
    }
    result.into_values().collect()
}

// ------------------------------------------------------------------------------------------------
// The catalog plugin
// ------------------------------------------------------------------------------------------------

/// The plugin's loaded state (`loaded` in github-copilot.ts): set from the active OAuth connection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    /// API base of the connection (`copilotBaseURL(metadata)`).
    pub base_url: Option<String>,
    /// The OAuth refresh token (utility title requests).
    pub token: Option<String>,
    /// The synced `/models` list, when the fetch succeeded.
    pub models: Option<Arc<Snapshot>>,
    /// Key of the connection the list was fetched with (`sourceConnection`).
    pub connection: Option<String>,
}

/// The provider and model transforms of `opencode.provider.github.copilot`.
pub struct GithubCopilot {
    pub state: State,
}

impl CatalogPlugin for GithubCopilot {
    fn providers(&mut self, editor: &mut ProviderEditor) {
        let Some(record) = editor.get(PROVIDER_ID) else { return };
        if let Some(models) = &self.state.models {
            let existing: Vec<ModelInfo> = record.models.values().cloned().collect();
            let base_url = self.state.base_url.clone().unwrap_or_else(|| "https://api.githubcopilot.com".into());
            let derived = derive(&base_url, models, &existing);
            editor.add(record.provider.clone(), derived, self.state.connection.clone());
            return;
        }
        // Without the live model list, the models.dev package says which Copilot API serves each model:
        // Anthropic Messages stays native, OpenAI means Responses, and anything else goes through the
        // bundled Copilot SDK's model ID fallback.
        let ids: Vec<String> = record.models.keys().cloned().collect();
        for id in ids {
            editor.update_model(PROVIDER_ID, &id, |model| {
                if model.package.as_deref() == Some("@opencode/ai/providers/anthropic") {
                    if let Some(base_url) = &self.state.base_url {
                        let mut overlay = Object::new();
                        overlay.insert("baseURL".into(), json!(format!("{base_url}/v1")));
                        model.settings = merge_overlay(model.settings.as_ref(), Some(&overlay));
                    }
                    return;
                }
                let responses = model.package.as_deref() == Some("@opencode/ai/providers/openai");
                model.package = Some(PACKAGE.into());
                let mut overlay = Object::new();
                if let Some(base_url) = &self.state.base_url {
                    overlay.insert("baseURL".into(), json!(base_url));
                }
                if responses {
                    overlay.insert("endpoint".into(), json!("responses"));
                }
                model.settings = merge_overlay(model.settings.as_ref(), Some(&overlay));
            });
        }
    }

    fn models(&mut self, editor: &mut ModelEditor<'_>) {
        // This chat-only alias conflicts with the Copilot GPT-5 Responses route.
        if editor.get(PROVIDER_ID, CHAT_ALIAS).is_some() {
            editor.update(PROVIDER_ID, CHAT_ALIAS, |model| model.enabled = false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(id: &str, extra: Value) -> Value {
        let base = json!({
            "model_picker_enabled": true, "id": id, "name": id, "version": id,
            "capabilities": {"family": "fam", "limits": {"max_output_tokens": 100, "max_prompt_tokens": 1000}, "supports": {"tool_calls": true}},
        });
        crate::json::deep_merge(&base, &extra)
    }

    #[test]
    fn decodes_and_filters_like_the_schema() {
        let body = json!({"data": [
            remote("ok", json!({})),
            remote("disabled", json!({"policy": {"state": "disabled"}})),
            remote("null-endpoints", json!({"supported_endpoints": null})),
            {"id": "no-caps", "name": "x", "version": "x", "model_picker_enabled": true},
            remote("bad-vision", json!({"capabilities": {"limits": {"vision": {"supported_media_types": []}}}})),
        ]});
        let mut no_tools = remote("no-tools", json!({}));
        no_tools["capabilities"]["supports"].as_object_mut().unwrap().remove("tool_calls");
        let mut body = body;
        body["data"].as_array_mut().unwrap().push(no_tools);
        let snapshot = decode_snapshot(&body).unwrap();
        assert_eq!(snapshot.keys().collect::<Vec<_>>(), vec!["ok"]);
        assert!(decode_snapshot(&json!({"models": []})).is_err());
    }

    #[test]
    fn builds_routes_variants_and_prices() {
        let messages = decode_model(&remote(
            "claude",
            json!({"supported_endpoints": ["/v1/messages", "/chat/completions"], "version": "2025-10-01",
                   "capabilities": {"supports": {"reasoning_effort": ["low", "high"], "adaptive_thinking": true, "vision": true},
                                    "limits": {"vision": {"max_prompt_image_size": 1, "max_prompt_images": 1, "supported_media_types": ["image/png", "application/pdf"]}}},
                   "billing": {"token_prices": {"batch_size": 2000000, "default": {"input_price": 3, "output_price": 15, "cache_price": 0.3}}}}),
        ))
        .unwrap();
        let model = build("claude", &messages, "http://x", None);
        assert_eq!(model.package.as_deref(), Some("@opencode/ai/providers/anthropic"));
        assert_eq!(serde_json::to_value(&model.settings).unwrap(), json!({"baseURL": "http://x/v1", "endpoint": "messages"}));
        assert_eq!(model.capabilities.input, vec!["text", "image", "pdf"]);
        assert_eq!(model.variants[0].settings, json!({"thinking": {"type": "adaptive", "display": "summarized"}, "effort": "low"}).as_object().cloned());
        assert_eq!(model.cost[0].input, 3.0 * (10_000.0 / 2_000_000.0));
        assert_eq!(model.cost[0].cache.read, 0.3 * (10_000.0 / 2_000_000.0));
        assert_eq!(model.time.released, 1759276800000.0);
        assert_eq!(model.limit, Limit { context: 1000, input: Some(1000), output: 100 });

        let budget = decode_model(&remote("haiku", json!({"capabilities": {"supports": {"max_thinking_budget": 32000}}}))).unwrap();
        let model = build("haiku", &budget, "http://x", None);
        assert_eq!(model.package.as_deref(), Some(PACKAGE));
        assert_eq!(serde_json::to_value(&model.settings).unwrap(), json!({"baseURL": "http://x"}));
        assert_eq!(
            serde_json::to_value(&model.variants).unwrap(),
            json!([{"id": "max", "settings": {"thinking": {"type": "enabled", "budgetTokens": 31999}}},
                   {"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 16000}}}])
        );

        let responses = decode_model(&remote("gpt-5.4", json!({"supported_endpoints": ["/responses"], "version": "gpt-5.4-2026-03-05",
            "capabilities": {"supports": {"reasoning_effort": ["low"]}}}))).unwrap();
        let mut template = ModelInfo::default_for(PROVIDER_ID, "gpt-5.4");
        template.name = "GPT-5.4 (catalog)".into();
        template.time.released = 5.0;
        template.settings = json!({"baseURL": "https://api.githubcopilot.com", "provider": "github-copilot"}).as_object().cloned();
        let model = build("gpt-5.4", &responses, "http://x", Some(&template));
        assert_eq!(model.name, "GPT-5.4 (catalog)");
        assert_eq!(model.time.released, 5.0);
        assert_eq!(
            serde_json::to_value(&model.settings).unwrap(),
            json!({"baseURL": "http://x", "provider": "github-copilot", "endpoint": "responses"})
        );
        assert_eq!(model.variants[0].settings, json!({"reasoningEffort": "low", "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]}).as_object().cloned());
    }

    fn copilot_editor() -> ProviderEditor {
        let mut editor = ProviderEditor::new();
        let mut info = super::super::types::ProviderInfo::empty(PROVIDER_ID);
        info.package = "@opencode/ai/providers/openai-compatible".into();
        let mut claude = ModelInfo::default_for(PROVIDER_ID, "claude-sonnet-4.6");
        claude.package = Some("@opencode/ai/providers/anthropic".into());
        claude.settings = json!({"baseURL": "https://api.githubcopilot.com/v1"}).as_object().cloned();
        let gpt = ModelInfo::default_for(PROVIDER_ID, "gpt-5.4");
        let mut responses = ModelInfo::default_for(PROVIDER_ID, "gpt-5.5");
        responses.package = Some("@opencode/ai/providers/openai".into());
        editor.add(info, vec![claude, gpt, responses, ModelInfo::default_for(PROVIDER_ID, CHAT_ALIAS)], None);
        editor
    }

    #[test]
    fn without_a_list_models_follow_their_models_dev_package() {
        // No connection: Anthropic Messages stays native at models.dev's endpoint, OpenAI models use the
        // Copilot SDK's Responses endpoint, and the rest the Copilot SDK fallback.
        let mut editor = copilot_editor();
        GithubCopilot { state: State::default() }.providers(&mut editor);
        let record = editor.get(PROVIDER_ID).unwrap();
        let claude = &record.models["claude-sonnet-4.6"];
        assert_eq!(claude.package.as_deref(), Some("@opencode/ai/providers/anthropic"));
        assert_eq!(serde_json::to_value(&claude.settings).unwrap(), json!({"baseURL": "https://api.githubcopilot.com/v1"}));
        assert_eq!(record.models["gpt-5.4"].package.as_deref(), Some(PACKAGE));
        assert_eq!(serde_json::to_value(&record.models["gpt-5.4"].settings).unwrap(), json!({}));
        assert_eq!(record.models["gpt-5.5"].package.as_deref(), Some(PACKAGE));
        assert_eq!(serde_json::to_value(&record.models["gpt-5.5"].settings).unwrap(), json!({"endpoint": "responses"}));
        // A connection whose model sync failed: its API base for every model (`/v1` for Messages).
        let mut editor = copilot_editor();
        let state = State { base_url: Some("http://x".into()), token: Some("t".into()), models: None, connection: Some("credential:c".into()) };
        GithubCopilot { state }.providers(&mut editor);
        let record = editor.get(PROVIDER_ID).unwrap();
        assert_eq!(serde_json::to_value(&record.models["claude-sonnet-4.6"].settings).unwrap(), json!({"baseURL": "http://x/v1"}));
        assert_eq!(serde_json::to_value(&record.models["gpt-5.4"].settings).unwrap(), json!({"baseURL": "http://x"}));
        assert_eq!(serde_json::to_value(&record.models["gpt-5.5"].settings).unwrap(), json!({"baseURL": "http://x", "endpoint": "responses"}));
        assert_eq!(record.source_connection, None);
    }

    #[test]
    fn a_synced_list_replaces_the_inventory_for_its_connection() {
        let mut editor = copilot_editor();
        let snapshot = decode_snapshot(&json!({"data": [remote("gpt-5.4", json!({"supported_endpoints": ["/responses"]})), remote("new", json!({}))]})).unwrap();
        let state = State { base_url: Some("http://x".into()), token: None, models: Some(Arc::new(snapshot)), connection: Some("credential:c".into()) };
        GithubCopilot { state }.providers(&mut editor);
        let record = editor.get(PROVIDER_ID).unwrap();
        assert_eq!(record.models.keys().collect::<Vec<_>>(), vec!["gpt-5.4", "new"]);
        assert_eq!(record.source_connection.as_deref(), Some("credential:c"));
        assert_eq!(record.provider.package, "@opencode/ai/providers/openai-compatible");
    }

    #[test]
    fn derive_keeps_templates_with_remote_models_and_appends_the_rest() {
        let snapshot = decode_snapshot(&json!({"data": [remote("b", json!({})), remote("c", json!({}))]})).unwrap();
        let mut alias = ModelInfo::default_for(PROVIDER_ID, "alias");
        alias.model_id = "b".into();
        let existing = vec![ModelInfo::default_for(PROVIDER_ID, "a"), alias, ModelInfo::default_for(PROVIDER_ID, "c")];
        let derived = derive("http://x", &snapshot, &existing);
        let ids: Vec<(&str, &str)> = derived.iter().map(|m| (m.id.as_str(), m.model_id.as_str())).collect();
        assert_eq!(ids, vec![("alias", "b"), ("c", "c"), ("b", "b")]);
    }
}
