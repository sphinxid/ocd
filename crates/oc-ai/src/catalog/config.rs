//! Custom providers from config (`packages/core/src/config/plugin/provider.ts`,
//! `packages/schema/src/config/provider.ts`) and the v1 `provider` section migration
//! (`packages/core/src/v1/config/migrate.ts:106-245`).

use super::models_dev::{IntegrationMethod, IntegrationSpec};
use super::registry::{CatalogPlugin, ModelEditor, ProviderEditor};
use super::types::{
    aisdk, catalog_compatibility, compatibility_from_interleaved, merge_capabilities, merge_headers, merge_overlay,
    parse_model_ref, Cost, CostCache, CostTier, HeaderMap, ModelInfo, PartialCapabilities, Variant,
};
use super::variant;
use crate::json::Object;
use crate::types::Compatibility;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// `Config.Model.Cost.Cache`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigCostCache {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write: Option<f64>,
}

/// `Config.Model.Cost`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigCost {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<CostTier>,
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<ConfigCostCache>,
}

/// `Cost | Cost[]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConfigCosts {
    One(ConfigCost),
    Many(Vec<ConfigCost>),
}

/// `Config.Model.Limit`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigLimit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<i64>,
}

/// `Config.Model`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigModel {
    #[serde(rename = "modelID", default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<PartialCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variants: Option<Vec<Variant>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ConfigCosts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<ConfigLimit>,
}

/// `Config.Provider`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigProvider {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Object>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeaderMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Object>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<IndexMap<String, ConfigModel>>,
}

/// Parse one `providers` config section; entries that do not decode are skipped.
pub fn parse_providers(section: &Value) -> Vec<(String, ConfigProvider)> {
    let Some(object) = section.as_object() else { return vec![] };
    object
        .iter()
        .filter_map(|(id, raw)| match serde_json::from_value::<ConfigProvider>(raw.clone()) {
            Ok(mut provider) => {
                if let Some(models) = provider.models.as_mut() {
                    for model in models.values_mut() {
                        model.compatibility = model.compatibility.as_ref().map(catalog_compatibility);
                    }
                }
                Some((id.clone(), provider))
            }
            Err(error) => {
                tracing::warn!(provider = %id, error = %error, "Invalid provider configuration");
                None
            }
        })
        .collect()
}

/// `Config.ModelSelection`: `"provider/model[#variant]"` or `{providerID, model, variant?}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelection {
    pub provider_id: String,
    pub model: String,
    pub variant: Option<String>,
}

impl ModelSelection {
    pub fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::String(s) => {
                let r = parse_model_ref(s).ok()?;
                Some(Self { provider_id: r.provider_id, model: r.id, variant: r.variant })
            }
            Value::Object(o) => Some(Self {
                provider_id: o.get("providerID")?.as_str()?.to_string(),
                model: o.get("model")?.as_str()?.to_string(),
                variant: o.get("variant").and_then(Value::as_str).map(String::from),
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct Definition {
    inherit: bool,
    base: Option<ModelInfo>,
}

/// The config provider plugin: configured providers (one entry per document occurrence, in order)
/// and the configured default model.
#[derive(Debug, Clone, Default)]
pub struct ConfigProviders {
    entries: Vec<(String, ConfigProvider)>,
    default_model: Option<ModelSelection>,
    sources: Vec<(usize, IndexMap<String, Definition>)>,
}

/// `Object.assign(target, structuredClone(base))`: required fields replaced, optional fields replaced
/// only when the base defines them.
fn assign_model(target: &mut ModelInfo, base: &ModelInfo) {
    let b = base.clone();
    target.id = b.id;
    target.model_id = b.model_id;
    target.provider_id = b.provider_id;
    target.name = b.name;
    target.capabilities = b.capabilities;
    target.variants = b.variants;
    target.time = b.time;
    target.cost = b.cost;
    target.status = b.status;
    target.enabled = b.enabled;
    target.limit = b.limit;
    macro_rules! opt {
        ($($f:ident),*) => { $( if b.$f.is_some() { target.$f = b.$f; } )* };
    }
    opt!(canonical, family, compatibility, package, settings, headers, body);
}

impl ConfigProviders {
    /// From one `providers` section.
    pub fn from_value(section: &Value) -> Self {
        Self::from_documents(std::slice::from_ref(section))
    }

    /// From the `providers` sections of every config document, in precedence order.
    pub fn from_documents(sections: &[Value]) -> Self {
        Self { entries: sections.iter().flat_map(parse_providers).collect(), ..Default::default() }
    }

    pub fn from_entries(entries: Vec<(String, ConfigProvider)>) -> Self {
        Self { entries, ..Default::default() }
    }

    /// The latest configured top-level `model`.
    pub fn with_default_model(mut self, selection: Option<ModelSelection>) -> Self {
        self.default_model = selection;
        self
    }

    pub fn entries(&self) -> &[(String, ConfigProvider)] {
        &self.entries
    }

    /// Integration registrations: a manual key method for new integrations and an env method when
    /// `env` is configured. `exists` reports integrations registered by earlier plugins.
    pub fn integrations(&self, exists: impl Fn(&str) -> bool) -> Vec<IntegrationSpec> {
        let mut known: Vec<String> = vec![];
        let mut out: Vec<IntegrationSpec> = vec![];
        for (id, provider) in &self.entries {
            let mut methods = vec![];
            if !exists(id) && !known.contains(id) {
                methods.push(IntegrationMethod::Key { label: Some("Manually enter API Key".into()) });
            }
            if let Some(env) = &provider.env {
                methods.push(IntegrationMethod::Env { names: env.clone() });
            }
            known.push(id.clone());
            out.push(IntegrationSpec { id: id.clone(), name: provider.name.clone(), methods });
        }
        out
    }
}

impl CatalogPlugin for ConfigProviders {
    fn providers(&mut self, editor: &mut ProviderEditor) {
        let mut next = vec![];
        for (index, (id, item)) in self.entries.iter().enumerate() {
            let current = editor.get(id).map(|r| r.provider.canonical.clone());
            let current_exists = current.is_some();
            let current_canonical = current.flatten();
            let source_id = item.canonical.clone().or_else(|| current_canonical.clone()).unwrap_or_else(|| id.clone());
            let source_exists = editor.get(&source_id).is_some();
            let changed = item.canonical.is_some() && item.canonical != current_canonical;
            let source_provider = if changed && source_exists && source_id != *id {
                editor.get(&source_id).map(|r| r.provider.clone())
            } else {
                None
            };
            editor.update(id, |provider| {
                if let Some(source) = source_provider {
                    let own_id = provider.id.clone();
                    let own_integration = provider.integration_id.clone();
                    provider.name = source.name;
                    provider.activation = source.activation;
                    provider.package = source.package;
                    if source.canonical.is_some() {
                        provider.canonical = source.canonical;
                    }
                    if source.settings.is_some() {
                        provider.settings = source.settings;
                    }
                    if source.headers.is_some() {
                        provider.headers = source.headers;
                    }
                    if source.body.is_some() {
                        provider.body = source.body;
                    }
                    provider.id = own_id;
                    provider.integration_id = own_integration;
                }
                provider.activation = super::types::Activation::Enabled;
                if item.canonical.is_some() {
                    provider.canonical = item.canonical.clone();
                }
                if let Some(name) = &item.name {
                    provider.name = name.clone();
                }
                if let Some(package) = &item.package {
                    provider.package = package.clone();
                }
                if item.settings.is_some() {
                    provider.settings = merge_overlay(provider.settings.as_ref(), item.settings.as_ref());
                }
                if item.headers.is_some() {
                    provider.headers = merge_headers(provider.headers.as_ref(), item.headers.as_ref());
                }
                if item.body.is_some() {
                    provider.body = merge_overlay(provider.body.as_ref(), item.body.as_ref());
                }
            });
            let mut definitions: IndexMap<String, Definition> = IndexMap::new();
            for (mid, config) in item.models.iter().flatten() {
                let base = if source_exists {
                    editor.get(&source_id).and_then(|source| {
                        config
                            .model_id
                            .as_ref()
                            .and_then(|m| source.models.get(m))
                            .or_else(|| source.models.get(mid))
                            .cloned()
                    })
                } else {
                    None
                };
                let has = current_exists && editor.get(id).map(|r| r.models.contains_key(mid)).unwrap_or(false);
                let inherit = changed || !has;
                definitions.insert(mid.clone(), Definition { inherit, base: base.clone() });
                if !inherit {
                    continue;
                }
                editor.update_model(id, mid, |model| {
                    if let Some(base) = &base {
                        assign_model(model, base);
                    }
                    if item.package.is_some() {
                        model.package = None;
                    }
                    if item.settings.as_ref().map(|s| s.contains_key("baseURL")).unwrap_or(false) {
                        if let Some(settings) = model.settings.as_mut() {
                            settings.shift_remove("baseURL");
                        }
                    }
                });
            }
            next.push((index, definitions));
        }
        self.sources = next;
    }

    fn models(&mut self, editor: &mut ModelEditor<'_>) {
        if let Some(default) = &self.default_model {
            editor.set_default(&default.provider_id, &default.model);
        }
        for (index, definitions) in &self.sources {
            let (provider_id, item) = &self.entries[*index];
            for (mid, config) in item.models.iter().flatten() {
                let source = definitions.get(mid);
                let inherit = source.map(|s| s.inherit).unwrap_or(false) || editor.get(provider_id, mid).is_none();
                let base = source.and_then(|s| s.base.as_ref());
                editor.update(provider_id, mid, |model| {
                    if inherit {
                        if let Some(base) = base {
                            assign_model(model, base);
                            if item.package.is_some() {
                                model.package = None;
                            }
                            if item.settings.as_ref().map(|s| s.contains_key("baseURL")).unwrap_or(false) {
                                if let Some(settings) = model.settings.as_mut() {
                                    settings.shift_remove("baseURL");
                                }
                            }
                        }
                    }
                    apply_model_config(model, config);
                });
                if config.variants.is_none() && base.is_none() {
                    let provider_package = editor.provider(provider_id).map(|r| r.provider.package.clone());
                    editor.update(provider_id, mid, |model| {
                        let mut probe = model.clone();
                        probe.package = model.package.clone().or(provider_package);
                        model.variants = variant::resolve_default(&probe);
                    });
                }
            }
        }
    }
}

/// Overlay one configured model onto a model definition.
pub fn apply_model_config(model: &mut ModelInfo, config: &ConfigModel) {
    if let Some(family) = &config.family {
        model.family = Some(family.clone());
    }
    if let Some(name) = &config.name {
        model.name = name.clone();
    }
    if let Some(model_id) = &config.model_id {
        model.model_id = model_id.clone();
    }
    if let Some(compatibility) = &config.compatibility {
        model.compatibility = Some(model.compatibility.clone().unwrap_or_default().merged(compatibility));
    }
    if let Some(package) = &config.package {
        model.package = Some(package.clone());
    }
    if config.settings.is_some() {
        model.settings = merge_overlay(model.settings.as_ref(), config.settings.as_ref());
    }
    if config.headers.is_some() {
        model.headers = merge_headers(model.headers.as_ref(), config.headers.as_ref());
    }
    if config.body.is_some() {
        model.body = merge_overlay(model.body.as_ref(), config.body.as_ref());
    }
    if let Some(capabilities) = &config.capabilities {
        model.capabilities = merge_capabilities(capabilities, Some(&model.capabilities));
    }
    if let Some(variants) = &config.variants {
        for variant in variants {
            let position = match model.variants.iter().position(|v| v.id == variant.id) {
                Some(p) => p,
                None => {
                    model.variants.push(Variant { id: variant.id.clone(), ..Default::default() });
                    model.variants.len() - 1
                }
            };
            let existing = &mut model.variants[position];
            if variant.settings.is_some() {
                existing.settings = merge_overlay(existing.settings.as_ref(), variant.settings.as_ref());
            }
            if variant.headers.is_some() {
                existing.headers = merge_headers(existing.headers.as_ref(), variant.headers.as_ref());
            }
            if variant.body.is_some() {
                existing.body = merge_overlay(existing.body.as_ref(), variant.body.as_ref());
            }
        }
    }
    if let Some(cost) = &config.cost {
        let items: Vec<&ConfigCost> = match cost {
            ConfigCosts::One(c) => vec![c],
            ConfigCosts::Many(c) => c.iter().collect(),
        };
        model.cost = items
            .into_iter()
            .map(|c| Cost {
                tier: c.tier.clone(),
                input: c.input,
                output: c.output,
                cache: CostCache {
                    read: c.cache.as_ref().and_then(|c| c.read).unwrap_or(0.0),
                    write: c.cache.as_ref().and_then(|c| c.write).unwrap_or(0.0),
                },
            })
            .collect();
    }
    if let Some(disabled) = config.disabled {
        model.enabled = !disabled;
    }
    if let Some(limit) = &config.limit {
        if let Some(context) = limit.context {
            model.limit.context = context;
        }
        if let Some(input) = limit.input {
            model.limit.input = Some(input);
        }
        if let Some(output) = limit.output {
            model.limit.output = output;
        }
    }
}

/// `ConfigProviderPlugin.foldSettings` (`plugin/provider/configured.ts`): fold a provider's settings
/// across config documents over the current settings.
pub fn fold_settings(sections: &[Value], id: &str, initial: Option<&Object>) -> Option<Object> {
    sections.iter().fold(initial.cloned(), |settings, section| {
        let overlay = section.get(id).and_then(|p| p.get("settings")).and_then(Value::as_object);
        merge_overlay(settings.as_ref(), overlay)
    })
}

// ---------------------------------------------------------------------------------------------
// v1 migration
// ---------------------------------------------------------------------------------------------

/// `ConfigMigrateV1.providerID`: renamed retired provider ids.
pub fn migrate_provider_id(input: &str) -> &str {
    match input {
        "azure-cognitive-services" => "azure",
        "google-vertex-anthropic" => "google-vertex",
        other => other,
    }
}

fn obj(v: Option<&Value>) -> Option<&Object> {
    v.and_then(Value::as_object)
}

fn put(out: &mut Object, key: &str, value: Option<Value>) {
    if let Some(v) = value {
        if !v.is_null() {
            out.insert(key.into(), v);
        }
    }
}

const SETTINGS_KNOWN: &[&str] = &["timeout", "headerTimeout", "chunkTimeout", "compaction", "transport"];
const MODEL_SETTINGS_KNOWN: &[&str] = &["compaction"];

/// Schema-encode settings: declared keys first, then the rest in input order.
fn canonical_settings(settings: &Object, known: &[&str]) -> Object {
    let mut out = Object::new();
    for k in known {
        if let Some(v) = settings.get(*k) {
            out.insert((*k).to_string(), v.clone());
        }
    }
    for (k, v) in settings {
        if !known.contains(&k.as_str()) {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// `ConfigProviderOptionsV1.provider`.
fn split_options(options: &Object) -> (Object, Option<Value>, Option<Value>) {
    let settings: Object =
        options.iter().filter(|(k, _)| k.as_str() != "headers" && k.as_str() != "body").map(|(k, v)| (k.clone(), v.clone())).collect();
    let headers = obj(options.get("headers")).map(|h| {
        Value::Object(h.iter().filter(|(_, v)| v.is_string()).map(|(k, v)| (k.clone(), v.clone())).collect())
    });
    let body = obj(options.get("body")).map(|b| Value::Object(b.clone()));
    (settings, headers, body)
}

fn int(value: &Value) -> Option<Value> {
    value.as_f64().map(|v| json!(v.trunc() as i64))
}

fn migrate_model(info: &Object) -> Object {
    let options = obj(info.get("options"));
    let block_binding_off = |key: &str| {
        options.and_then(|o| o.get(key)).and_then(|v| v.get("blockBinding")) == Some(&Value::Bool(false))
    };
    let disable_binding = block_binding_off("thinking") || block_binding_off("reasoningConfig");
    let mut settings: Option<Object> = options.cloned();
    if let (Some(settings), true) = (settings.as_mut(), disable_binding) {
        for key in ["thinking", "reasoningConfig"] {
            let Some(Value::Object(inner)) = settings.get(key) else { continue };
            if inner.get("blockBinding") != Some(&Value::Bool(false)) {
                continue;
            }
            let rest: Object = inner.iter().filter(|(k, _)| k.as_str() != "blockBinding").map(|(k, v)| (k.clone(), v.clone())).collect();
            if rest.is_empty() {
                settings.shift_remove(key);
            } else {
                settings.insert(key.into(), Value::Object(rest));
            }
        }
    }
    let mut compatibility = info.get("interleaved").and_then(compatibility_from_interleaved);
    if disable_binding {
        let mut c = compatibility.unwrap_or_default();
        c.supports_thinking_block_binding = Some(false);
        compatibility = Some(c);
    }
    let cost = obj(info.get("cost")).map(|cost| {
        let entry = |c: &Object, tier: Option<Value>| {
            let mut e = Object::new();
            put(&mut e, "tier", tier);
            put(&mut e, "input", c.get("input").cloned());
            put(&mut e, "output", c.get("output").cloned());
            let mut cache = Object::new();
            put(&mut cache, "read", c.get("cache_read").cloned());
            put(&mut cache, "write", c.get("cache_write").cloned());
            e.insert("cache".into(), Value::Object(cache));
            Value::Object(e)
        };
        let mut items = vec![entry(cost, None)];
        if let Some(over) = obj(cost.get("context_over_200k")) {
            items.push(entry(over, Some(json!({"type": "context", "size": 200000}))));
        }
        Value::Array(items)
    });
    let modalities = obj(info.get("modalities"));
    let capabilities = if info.get("tool_call").is_some()
        || modalities.and_then(|m| m.get("input")).is_some()
        || modalities.and_then(|m| m.get("output")).is_some()
    {
        Some(json!({
            "tools": info.get("tool_call").cloned().unwrap_or(json!(true)),
            "input": modalities.and_then(|m| m.get("input")).cloned().unwrap_or(json!(["text", "image"])),
            "output": modalities.and_then(|m| m.get("output")).cloned().unwrap_or(json!(["text"])),
        }))
    } else {
        None
    };
    let provider = obj(info.get("provider"));
    let package = provider.and_then(|p| p.get("npm")).and_then(Value::as_str).filter(|n| !n.is_empty()).map(|n| json!(aisdk(n)));
    let api = provider.and_then(|p| p.get("api")).and_then(Value::as_str).filter(|a| !a.is_empty());
    let settings = match (api, settings) {
        (Some(api), s) => {
            let mut s = s.unwrap_or_default();
            s.insert("baseURL".into(), json!(api));
            Some(s)
        }
        (None, s) => s,
    };
    let variants = obj(info.get("variants")).map(|variants| {
        Value::Array(
            variants
                .iter()
                .map(|(id, options)| {
                    let mut v = Object::new();
                    v.insert("id".into(), json!(id));
                    if let Some(o) = options.as_object() {
                        v.insert("settings".into(), Value::Object(canonical_settings(o, MODEL_SETTINGS_KNOWN)));
                    }
                    Value::Object(v)
                })
                .collect(),
        )
    });
    let limit = obj(info.get("limit")).map(|l| {
        let mut out = Object::new();
        put(&mut out, "context", l.get("context").and_then(int));
        put(&mut out, "input", l.get("input").and_then(int));
        put(&mut out, "output", l.get("output").and_then(int));
        Value::Object(out)
    });
    let mut out = Object::new();
    put(&mut out, "modelID", info.get("id").cloned());
    put(&mut out, "family", info.get("family").cloned());
    put(&mut out, "name", info.get("name").cloned());
    put(&mut out, "compatibility", compatibility.map(|c| serde_json::to_value(c).unwrap_or(Value::Null)));
    put(&mut out, "package", package);
    put(&mut out, "settings", settings.map(|s| Value::Object(canonical_settings(&s, MODEL_SETTINGS_KNOWN))));
    put(&mut out, "headers", info.get("headers").cloned());
    put(&mut out, "body", None);
    put(&mut out, "capabilities", capabilities);
    put(&mut out, "variants", variants);
    put(&mut out, "cost", cost);
    put(&mut out, "disabled", (info.get("status").and_then(Value::as_str) == Some("deprecated")).then(|| json!(true)));
    put(&mut out, "limit", limit);
    out
}

fn migrate_standard(info: &Object) -> Object {
    let options = obj(info.get("options"));
    let split = options.map(split_options);
    let api = info.get("api").and_then(Value::as_str).filter(|a| !a.is_empty());
    let settings = match (api, &split) {
        (Some(api), split) => {
            let mut s = split.as_ref().map(|s| s.0.clone()).unwrap_or_default();
            s.insert("baseURL".into(), json!(api));
            Some(s)
        }
        (None, Some(split)) => Some(split.0.clone()),
        (None, None) => None,
    };
    let package = info.get("npm").and_then(Value::as_str).filter(|n| !n.is_empty()).map(|n| json!(aisdk(n)));
    let models = obj(info.get("models")).map(|models| {
        Value::Object(
            models
                .iter()
                .filter_map(|(name, model)| model.as_object().map(|m| (name.clone(), Value::Object(migrate_model(m)))))
                .collect(),
        )
    });
    let mut out = Object::new();
    put(&mut out, "name", info.get("name").cloned());
    put(&mut out, "env", info.get("env").cloned());
    put(&mut out, "package", package);
    put(&mut out, "settings", settings.map(|s| Value::Object(canonical_settings(&s, SETTINGS_KNOWN))));
    put(&mut out, "headers", split.as_ref().and_then(|s| s.1.clone()));
    put(&mut out, "body", split.as_ref().and_then(|s| s.2.clone()));
    put(&mut out, "models", models);
    out
}

/// Insert `key` keeping the schema field order of `Config.Provider`.
fn reorder_provider(input: Object) -> Object {
    const ORDER: &[&str] = &["canonical", "name", "env", "package", "settings", "headers", "body", "models"];
    let mut out = Object::new();
    for key in ORDER {
        if let Some(v) = input.get(*key) {
            out.insert((*key).to_string(), v.clone());
        }
    }
    out
}

/// `ConfigMigrateV1.migrateProvider(sourceID, info)` (canonically encoded).
pub fn migrate_provider(source_id: &str, info: &Object) -> Object {
    let mut migrated = migrate_standard(info);
    match source_id {
        "azure-cognitive-services" => {
            if let Some(Value::Array(env)) = migrated.get("env").cloned() {
                let filtered: Vec<Value> =
                    env.into_iter().filter(|v| v.as_str() != Some("AZURE_COGNITIVE_SERVICES_RESOURCE_NAME")).collect();
                migrated.insert("env".into(), Value::Array(filtered));
            }
            let npm = info.get("npm").and_then(Value::as_str);
            let api = info.get("api").and_then(Value::as_str).filter(|a| !a.is_empty());
            if npm == Some("@ai-sdk/openai-compatible") && api.is_none() {
                let mut settings = obj(migrated.get("settings")).cloned().unwrap_or_default();
                settings.insert(
                    "baseURL".into(),
                    json!("https://${AZURE_COGNITIVE_SERVICES_RESOURCE_NAME}.cognitiveservices.azure.com/openai"),
                );
                migrated.insert("settings".into(), Value::Object(canonical_settings(&settings, SETTINGS_KNOWN)));
            }
        }
        "google-vertex-anthropic" => {
            let package = migrated.shift_remove("package").unwrap_or_else(|| json!(aisdk("@ai-sdk/google-vertex/anthropic")));
            if let Some(Value::Object(models)) = migrated.get_mut("models") {
                for model in models.values_mut() {
                    if let Value::Object(m) = model {
                        if !m.contains_key("package") {
                            m.insert("package".into(), package.clone());
                            *m = reorder_model(std::mem::take(m));
                        }
                    }
                }
            }
        }
        _ => {}
    }
    reorder_provider(migrated)
}

fn reorder_model(input: Object) -> Object {
    const ORDER: &[&str] = &[
        "modelID",
        "family",
        "name",
        "compatibility",
        "package",
        "settings",
        "headers",
        "body",
        "capabilities",
        "variants",
        "cost",
        "disabled",
        "limit",
    ];
    let mut out = Object::new();
    for key in ORDER {
        if let Some(v) = input.get(*key) {
            out.insert((*key).to_string(), v.clone());
        }
    }
    out
}

/// Structural validation of a v1 provider entry (`ConfigProviderV1.Info` decoding plus the overlay
/// checks of `invalidProviderOverlays`); invalid entries are skipped.
fn valid_v1_provider(info: &Object) -> bool {
    let string = |key: &str| info.get(key).map(Value::is_string).unwrap_or(true);
    let strings = |key: &str| match info.get(key) {
        None => true,
        Some(Value::Array(items)) => items.iter().all(Value::is_string),
        Some(_) => false,
    };
    if !(string("api") && string("name") && string("id") && string("npm")) {
        return false;
    }
    if !(strings("env") && strings("whitelist") && strings("blacklist")) {
        return false;
    }
    if let Some(options) = info.get("options") {
        let Some(options) = options.as_object() else { return false };
        if let Some(headers) = options.get("headers") {
            match headers.as_object() {
                Some(h) if h.values().all(Value::is_string) => {}
                _ => return false,
            }
        }
        if let Some(body) = options.get("body") {
            if !body.is_object() {
                return false;
            }
        }
    }
    match info.get("models") {
        None => true,
        Some(Value::Object(models)) => models.values().all(Value::is_object),
        Some(_) => false,
    }
}

/// Migrate a v1 `provider` section into a v2 `providers` section. A renamed provider is skipped when
/// the destination id is configured directly.
pub fn migrate_v1_providers(section: &Value) -> Value {
    let Some(object) = section.as_object() else { return json!({}) };
    let candidates: Vec<(String, String, Object)> = object
        .iter()
        .filter_map(|(name, raw)| {
            let info = raw.as_object()?;
            if !valid_v1_provider(info) {
                return None;
            }
            Some((name.clone(), migrate_provider_id(name).to_string(), migrate_provider(name, info)))
        })
        .collect();
    let current: Vec<String> = candidates.iter().filter(|(n, d, _)| n == d).map(|(_, d, _)| d.clone()).collect();
    let mut result = Object::new();
    for (name, destination, provider) in candidates {
        if name != destination && current.contains(&destination) {
            continue;
        }
        result.insert(destination, Value::Object(provider));
    }
    Value::Object(result)
}
