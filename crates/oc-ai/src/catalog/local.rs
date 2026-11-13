//! The catalog half of the local model discovery plugins (`packages/core/src/plugin/provider/{local,lmstudio,ollama,
//! vllm}.ts`): where each plugin looks for its server (`configured`), and the provider transform that replaces the
//! models.dev definition with the discovered inventory. The discovery itself (HTTP, timers, caches, reloads) runs in
//! `oc_core::local_providers`.
//!
//! Once a server answered with at least one model, its provider becomes `enabled`, uses the OpenAI-compatible
//! package with `{baseURL, provider, apiKey}` (the configured or default endpoint, `apiKey` `""` when none is
//! configured), loses its integration (the plugin's integration transform removes it, so no credential is needed)
//! and lists exactly the discovered models; until then, and after an empty answer, the models.dev definition stays.

use super::registry::{CatalogPlugin, ProviderEditor};
use super::types::{merge_overlay, Activation, Capabilities};
use crate::json::Object;
use serde_json::Value;

/// The package every discovered provider uses.
pub const PACKAGE: &str = "@opencode/ai/providers/openai-compatible";

/// One of the three local discovery plugins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    LmStudio,
    Ollama,
    Vllm,
}

impl Kind {
    /// In plugin order (`ProviderPlugins`).
    pub const ALL: [Kind; 3] = [Kind::LmStudio, Kind::Ollama, Kind::Vllm];

    pub fn plugin_id(self) -> &'static str {
        match self {
            Kind::LmStudio => "opencode.provider.lmstudio",
            Kind::Ollama => "opencode.provider.ollama",
            Kind::Vllm => "opencode.provider.vllm",
        }
    }

    pub fn provider_id(self) -> &'static str {
        match self {
            Kind::LmStudio => "lmstudio",
            Kind::Ollama => "ollama",
            Kind::Vllm => "vllm",
        }
    }

    pub fn from_provider_id(id: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.provider_id() == id)
    }

    /// The provider's display name once discovered.
    pub fn name(self) -> &'static str {
        match self {
            Kind::LmStudio => "LM Studio",
            Kind::Ollama => "Ollama",
            Kind::Vllm => "vLLM",
        }
    }

    /// The server's default origin (used when no `baseURL` is configured).
    pub fn origin(self) -> &'static str {
        match self {
            Kind::LmStudio => "http://127.0.0.1:1234",
            Kind::Ollama => "http://127.0.0.1:11434",
            Kind::Vllm => "http://127.0.0.1:8000",
        }
    }

    /// `stripPathSuffix`: the API suffix removed from the configured path to find the server root.
    fn suffix(self) -> &'static regex::Regex {
        static LMSTUDIO: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| regex::Regex::new(r"/(?:api/)?v1$").unwrap());
        static OLLAMA: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| regex::Regex::new(r"/(?:v1|api)$").unwrap());
        static DEFAULT: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| regex::Regex::new(r"/v1$").unwrap());
        match self {
            Kind::LmStudio => &LMSTUDIO,
            Kind::Ollama => &OLLAMA,
            Kind::Vllm => &DEFAULT,
        }
    }
}

/// Where a plugin discovers (`configured(...)` in local.ts).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Source {
    /// The provider's `baseURL` (configured, else `<origin>/v1`, without trailing slashes; `""` without either).
    pub base_url: String,
    /// The configured `apiKey` when it is a string.
    pub api_key: Option<String>,
    /// The server root (`endpoint("")`), when `base_url` is an http(s) URL: the discovery cache key.
    pub root: Option<String>,
    /// The parsed base URL and the path prefix the endpoints go under.
    endpoint: Option<(String, String)>,
}

impl Source {
    /// `endpoint(path)`: the base URL with its path replaced by `<prefix><path>`, query and fragment removed.
    pub fn endpoint(&self, path: &str) -> Option<String> {
        let (url, prefix) = self.endpoint.as_ref()?;
        let mut next = url::Url::parse(url).ok()?;
        next.set_path(&format!("{prefix}{path}"));
        next.set_query(None);
        next.set_fragment(None);
        Some(next.to_string())
    }
}

/// `foldSettings(entries, providerID)`: every config document's `providers[id].settings`, merged in order.
pub fn fold_settings<'a>(documents: impl IntoIterator<Item = Option<&'a Object>>) -> Option<Object> {
    documents.into_iter().fold(None, |settings, overlay| merge_overlay(settings.as_ref(), overlay))
}

/// `configured(entries, providerID, origin, suffix)` from the folded settings and the default origin (`None`: no
/// default discovery).
pub fn configured(kind: Kind, settings: Option<&Object>, origin: Option<&str>) -> Source {
    let base = match settings.and_then(|s| s.get("baseURL")) {
        Some(Value::String(s)) => s.clone(),
        _ => origin.map(|o| format!("{}/v1", o.trim_end_matches('/'))).unwrap_or_default(),
    };
    let base_url = base.trim_end_matches('/').to_string();
    let api_key = match settings.and_then(|s| s.get("apiKey")) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    let mut source = Source { base_url, api_key, root: None, endpoint: None };
    let Ok(url) = url::Url::parse(&source.base_url) else { return source };
    if url.scheme() != "http" && url.scheme() != "https" {
        return source;
    }
    let path = url.path().trim_end_matches('/');
    let prefix = kind.suffix().replace(path, "").into_owned();
    source.endpoint = Some((url.to_string(), prefix));
    source.root = source.endpoint("");
    source
}

/// What a plugin maps a discovered item to (`LocalModel`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LocalModel {
    pub id: String,
    pub name: Option<String>,
    pub family: Option<String>,
    pub tools: Option<bool>,
    pub vision: Option<bool>,
    pub context: Option<f64>,
}

/// A location's discovered provider: the source it was discovered at and its models (never empty).
#[derive(Debug, Clone, PartialEq)]
pub struct Discovered {
    pub kind: Kind,
    pub base_url: String,
    pub api_key: Option<String>,
    pub models: Vec<LocalModel>,
}

/// The provider transforms of the discovery plugins with a non-empty inventory, in plugin order.
pub struct LocalProviders {
    pub providers: Vec<Discovered>,
}

/// `ctx.provider.transform` of one discovery plugin.
pub fn apply(editor: &mut ProviderEditor, discovered: &Discovered) {
    if discovered.models.is_empty() {
        return;
    }
    let id = discovered.kind.provider_id();
    let existing: Vec<String> = editor.get(id).map(|r| r.models.keys().cloned().collect()).unwrap_or_default();
    for model in existing {
        editor.remove_model(id, &model);
    }
    editor.update(id, |provider| {
        provider.name = discovered.kind.name().to_string();
        provider.activation = Activation::Enabled;
        provider.package = PACKAGE.to_string();
        let mut settings = Object::new();
        settings.insert("baseURL".into(), Value::String(discovered.base_url.clone()));
        settings.insert("provider".into(), Value::String(id.to_string()));
        settings.insert("apiKey".into(), Value::String(discovered.api_key.clone().unwrap_or_default()));
        provider.settings = Some(settings);
        provider.integration_id = None;
    });
    for spec in &discovered.models {
        editor.update_model(id, &spec.id, |model| {
            model.model_id = spec.id.clone();
            model.name = spec.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| spec.id.clone());
            model.family = spec.family.clone().filter(|f| !f.is_empty());
            let mut input = vec!["text".to_string()];
            if spec.vision == Some(true) {
                input.push("image".into());
            }
            model.capabilities = Capabilities { tools: spec.tools.unwrap_or(false), input, output: vec!["text".into()], temperature: None };
            if let Some(context) = spec.context.filter(|c| *c > 0.0) {
                model.limit.context = context as i64;
            }
        });
    }
}

impl CatalogPlugin for LocalProviders {
    fn providers(&mut self, editor: &mut ProviderEditor) {
        for discovered in &self.providers {
            apply(editor, discovered);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::types::{ModelInfo, ProviderInfo};
    use serde_json::json;

    fn obj(v: Value) -> Object {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn default_and_configured_endpoints() {
        let lm = configured(Kind::LmStudio, None, Some("http://127.0.0.1:1234"));
        assert_eq!(lm.base_url, "http://127.0.0.1:1234/v1");
        assert_eq!(lm.api_key, None);
        assert_eq!(lm.root.as_deref(), Some("http://127.0.0.1:1234/"));
        assert_eq!(lm.endpoint("/api/v1/models").as_deref(), Some("http://127.0.0.1:1234/api/v1/models"));

        // provider-lmstudio.test.ts: `/proxy/v1` and `/proxy/api/v1` both discover at `/proxy/api/v1/models`.
        let settings = obj(json!({"baseURL": "http://h:9/proxy/v1/", "apiKey": "secret"}));
        let lm = configured(Kind::LmStudio, Some(&settings), Some("http://127.0.0.1:1234"));
        assert_eq!(lm.base_url, "http://h:9/proxy/v1");
        assert_eq!(lm.api_key.as_deref(), Some("secret"));
        assert_eq!(lm.endpoint("/api/v1/models").as_deref(), Some("http://h:9/proxy/api/v1/models"));
        let settings = obj(json!({"baseURL": "http://h:9/proxy/api/v1?x=1#f"}));
        let lm = configured(Kind::LmStudio, Some(&settings), None);
        assert_eq!(lm.base_url, "http://h:9/proxy/api/v1?x=1#f");
        assert_eq!(lm.endpoint("/api/v1/models").as_deref(), Some("http://h:9/proxy/api/v1/models"));
        assert_eq!(lm.root.as_deref(), Some("http://h:9/proxy"));

        // provider-ollama.test.ts: `/proxy/v1` and `/proxy/api` are both the root `/proxy`.
        for base in ["http://h:9/proxy/v1", "http://h:9/proxy/api"] {
            let settings = obj(json!({ "baseURL": base }));
            let ollama = configured(Kind::Ollama, Some(&settings), None);
            assert_eq!(ollama.endpoint("/api/tags").as_deref(), Some("http://h:9/proxy/api/tags"));
        }
        // vLLM strips only `/v1`.
        let settings = obj(json!({"baseURL": "http://h:9/proxy/v1"}));
        let vllm = configured(Kind::Vllm, Some(&settings), None);
        assert_eq!(vllm.endpoint("/health").as_deref(), Some("http://h:9/proxy/health"));
        assert_eq!(vllm.endpoint("/v1/models").as_deref(), Some("http://h:9/proxy/v1/models"));

        // No origin and no baseURL: nothing to discover; non-http URLs neither.
        let none = configured(Kind::Vllm, None, None);
        assert_eq!((none.base_url.as_str(), none.root), ("", None));
        let settings = obj(json!({"baseURL": "unix:///tmp/socket", "apiKey": 3}));
        let unix = configured(Kind::Vllm, Some(&settings), Some("http://127.0.0.1:8000"));
        assert_eq!((unix.base_url.as_str(), unix.api_key, unix.root), ("unix:///tmp/socket", None, None));
        // A non-string baseURL falls back to the origin.
        let settings = obj(json!({"baseURL": null}));
        assert_eq!(configured(Kind::Vllm, Some(&settings), Some("http://127.0.0.1:8000/")).base_url, "http://127.0.0.1:8000/v1");
    }

    #[test]
    fn layered_settings() {
        let a = obj(json!({"baseURL": "http://a/v1", "apiKey": "old"}));
        let b = obj(json!({"apiKey": "secret"}));
        let c = obj(json!({"apiKey": null}));
        let folded = fold_settings([Some(&a), None, Some(&b)]).unwrap();
        assert_eq!(Value::Object(folded), json!({"baseURL": "http://a/v1", "apiKey": "secret"}));
        let folded = fold_settings([Some(&a), Some(&c)]).unwrap();
        assert_eq!(configured(Kind::Ollama, Some(&folded), None).api_key, None);
    }

    #[test]
    fn replaces_the_models_dev_definition() {
        let mut editor = ProviderEditor::new();
        let mut info = ProviderInfo::empty("lmstudio");
        info.name = "LMStudio".into();
        info.integration_id = Some("lmstudio".into());
        info.package = PACKAGE.into();
        editor.add(info, vec![ModelInfo::default_for("lmstudio", "static-model")], None);
        let discovered = Discovered {
            kind: Kind::LmStudio,
            base_url: "http://127.0.0.1:5/v1".into(),
            api_key: None,
            models: vec![
                LocalModel {
                    id: "google/gemma".into(),
                    name: Some("Gemma".into()),
                    family: Some("gemma4".into()),
                    tools: Some(true),
                    vision: Some(true),
                    context: Some(16_384.0),
                },
                LocalModel { id: "unknown-context".into(), family: Some(String::new()), context: Some(0.0), ..Default::default() },
            ],
        };
        apply(&mut editor, &discovered);
        let record = editor.get("lmstudio").unwrap();
        assert_eq!(record.provider.name, "LM Studio");
        assert_eq!(record.provider.activation, Activation::Enabled);
        assert_eq!(record.provider.integration_id, None);
        assert_eq!(
            record.provider.settings.clone().map(Value::Object),
            Some(json!({"baseURL": "http://127.0.0.1:5/v1", "provider": "lmstudio", "apiKey": ""}))
        );
        assert_eq!(record.models.keys().collect::<Vec<_>>(), ["google/gemma", "unknown-context"]);
        let gemma = &record.models["google/gemma"];
        assert_eq!((gemma.name.as_str(), gemma.family.as_deref()), ("Gemma", Some("gemma4")));
        assert_eq!(gemma.capabilities, Capabilities { tools: true, input: vec!["text".into(), "image".into()], output: vec!["text".into()], temperature: None });
        assert_eq!((gemma.limit.context, gemma.limit.output), (16_384, 32_000));
        let unknown = &record.models["unknown-context"];
        assert_eq!((unknown.name.as_str(), unknown.family.as_deref()), ("unknown-context", None));
        assert_eq!(unknown.capabilities.input, ["text"]);
        assert!(!unknown.capabilities.tools);
        assert_eq!(unknown.limit.context, 200_000);

        // An empty inventory leaves the definition alone.
        let mut editor = ProviderEditor::new();
        apply(&mut editor, &Discovered { models: vec![], ..discovered });
        assert!(editor.get("lmstudio").is_none());
    }
}
