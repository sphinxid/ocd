//! Catalog and request effects of the OpenAI sign-ins: the Codex (legacy ChatGPT browser and device
//! code) half of `core/src/plugin/provider/openai.ts`, ChatGPT token sharing
//! (`core/src/plugin/provider/chatgpt.ts`) and the ChatGPT input caps
//! (`core/src/plugin/provider/openai-context-caps.ts`). The sign-in flows themselves are in
//! [`crate::credentials::flows::codex`] and [`crate::credentials::flows::chatgpt`].
//!
//! Each plugin reads the `openai` integration's active connection when it loads and again when the
//! integration's credential switches; [`SignIn`] is that loaded state.

use super::registry::{CatalogPlugin, ModelEditor, ProviderEditor};
use super::types::{HeaderMap, Limit, ModelInfo, merge_headers, merge_overlay};
use crate::credentials::CredentialValue;
use crate::credentials::flows::{chatgpt, codex};
use crate::json::Object;
use crate::types::Compatibility;
use indexmap::IndexMap;
use serde_json::{Value, json};

pub const PROVIDER_ID: &str = "openai";
pub const INTEGRATION_ID: &str = "openai";
/// `opencode.provider.openai` (Codex sign-in, output limit).
pub const OPENAI_PLUGIN_ID: &str = "opencode.provider.openai";
/// `opencode.provider.chatgpt` (ChatGPT token sharing).
pub const CHATGPT_PLUGIN_ID: &str = "opencode.provider.chatgpt";
/// `opencode.provider.openai-context-caps`.
pub const CONTEXT_CAPS_PLUGIN_ID: &str = "opencode.provider.openai-context-caps";

pub const CODEX_BASE_URL: &str = codex::CODEX_BASE_URL;
/// Token sharing's base URL (`resource`).
pub const TOKEN_SHARING_BASE_URL: &str = chatgpt::RESOURCE;

/// Codex rejects gpt-5.5 and older models, including gpt-5.3-codex-spark, for ChatGPT accounts; these
/// newer ones are rejected too (`codexDisallowed`).
pub const CODEX_DISALLOWED: &[&str] = &["gpt-5.6"];

/// The image models the Codex client generates and edits with on the Codex login (`codexImageModels`).
pub const CODEX_IMAGE_MODELS: &[&str] = &["gpt-image-2"];

/// The catalog models token sharing keeps (`fallbackModels`).
pub const FALLBACK_MODELS: &[&str] = &[
    "gpt-5.5",
    "gpt-5.5-fast",
    "gpt-5.6-luna",
    "gpt-5.6-luna-fast",
    "gpt-5.6-sol",
    "gpt-5.6-sol-fast",
    "gpt-5.6-terra",
    "gpt-5.6-terra-fast",
    "gpt-6-astra",
    "gpt-6-astra-fast",
    "gpt-6-luna",
    "gpt-6-luna-fast",
    "gpt-6-sol",
    "gpt-6-sol-fast",
    "gpt-6.1-sol",
    "gpt-6.1-sol-fast",
    "gpt-6.1-terra",
    "gpt-6.1-terra-fast",
];

/// The OAuth methods the context caps apply to.
pub const CAPPED_METHODS: &[&str] = &[codex::BROWSER_METHOD_ID, codex::HEADLESS_METHOD_ID, chatgpt::METHOD_ID];

/// Default ChatGPT plan input window (`legacyInput`).
pub const LEGACY_INPUT: i64 = 272_000;
/// Input window of gpt-6.1-sol and later models (`currentInput`).
pub const CURRENT_INPUT: i64 = 500_000;

/// How the `openai` integration's active connection signs in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum SignIn {
    /// No connection, or an API key or environment variable.
    #[default]
    None,
    /// The Codex browser or device-code login (`chatgpt-browser`, `chatgpt-headless`).
    Codex {
        /// `metadata.accountID` (sent as `chatgpt-account-id`).
        account_id: Option<String>,
    },
    /// ChatGPT token sharing (`chatgpt-token-sharing`).
    TokenSharing {
        /// Key of the connection the provider is bound to (`sourceConnection`).
        connection: String,
    },
}

impl SignIn {
    /// The state the plugins load from the active connection (`connection` is its key) and its
    /// resolved credential (`undefined` when it could not be resolved).
    pub fn from_connection(connection: Option<&str>, credential: Option<&CredentialValue>) -> SignIn {
        let (Some(connection), Some(CredentialValue::Oauth { method_id, metadata, .. })) = (connection, credential) else {
            return SignIn::None;
        };
        match method_id.as_str() {
            codex::BROWSER_METHOD_ID | codex::HEADLESS_METHOD_ID => {
                SignIn::Codex { account_id: metadata.as_ref().and_then(|m| m.get("accountID")).and_then(Value::as_str).map(String::from) }
            }
            chatgpt::METHOD_ID => SignIn::TokenSharing { connection: connection.to_string() },
            _ => SignIn::None,
        }
    }

    /// Signed in with one of the three ChatGPT methods (`openai-context-caps.ts` `signedIn`).
    pub fn chatgpt(&self) -> bool {
        !matches!(self, SignIn::None)
    }
}

/// `Schema.is(Schema.Struct({ mode: Schema.Literal("pro") }))(draft.body?.reasoning)`.
fn pro(model: &ModelInfo) -> bool {
    model.body.as_ref().and_then(|b| b.get("reasoning")).and_then(|r| r.get("mode")).and_then(Value::as_str) == Some("pro")
}

/// `apiID.match(/^gpt-(\d+)(?:\.(\d+))?/)` as `(major, minor)`, `minor` 0 when absent.
fn gpt_version(api_id: &str) -> Option<(f64, f64)> {
    static VERSION: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"^gpt-(\d+)(?:\.(\d+))?").unwrap());
    let captures = VERSION.captures(api_id)?;
    let major = captures[1].parse::<f64>().ok()?;
    let minor = captures.get(2).map_or(Some(0.0), |m| m.as_str().parse::<f64>().ok())?;
    Some((major, minor))
}

/// Whether Codex serves `api_id` to a ChatGPT account: newer than gpt-5.5 and not disallowed.
pub fn codex_eligible(api_id: &str) -> bool {
    if CODEX_DISALLOWED.contains(&api_id) {
        return false;
    }
    gpt_version(api_id).is_some_and(|(major, minor)| major > 5.0 || (major == 5.0 && minor > 5.0))
}

/// The input cap of a ChatGPT plan model (`cap`): 500k for gpt-6.1 and later and for ids that are not
/// `gpt-N`, else 272k.
pub fn input_cap(api_id: &str) -> i64 {
    match gpt_version(api_id) {
        None => CURRENT_INPUT,
        Some((major, minor)) if major > 6.0 || (major == 6.0 && minor >= 1.0) => CURRENT_INPUT,
        Some(_) => LEGACY_INPUT,
    }
}

/// `Provider.mergeOverlay(provider.settings, overlay)`.
fn overlay(settings: Option<&Object>, overlay: Value) -> Option<Object> {
    let Value::Object(overlay) = overlay else { return settings.cloned() };
    merge_overlay(settings, Some(&overlay))
}

/// `configured?.baseURL === undefined` is false: the provider settings or a config document's
/// `providers.openai.settings` set a base URL (`configuredSettings`). `sections` are the documents'
/// `providers` objects.
pub fn configured_base_url(settings: Option<&Object>, sections: &[Value]) -> bool {
    settings.is_some_and(|s| s.contains_key("baseURL"))
        || sections.iter().any(|section| {
            section.get(PROVIDER_ID).and_then(|p| p.get("settings")).and_then(Value::as_object).is_some_and(|s| s.contains_key("baseURL"))
        })
}

/// The ChatGPT token-sharing catalog effects (`ChatGPTPlugin` provider and model transforms).
pub struct ChatGpt {
    pub sign_in: SignIn,
    /// Config documents' `providers` objects (for a configured base URL).
    pub sections: Vec<Value>,
}

impl CatalogPlugin for ChatGpt {
    fn providers(&mut self, editor: &mut ProviderEditor) {
        let SignIn::TokenSharing { connection } = &self.sign_in else { return };
        let Some(record) = editor.get(PROVIDER_ID) else { return };
        // Config applies after provider plugins, so read it here: a custom baseURL may not decode zstd.
        let configured = configured_base_url(record.provider.settings.as_ref(), &self.sections);
        let models: Vec<ModelInfo> = record.models.values().filter(|m| FALLBACK_MODELS.contains(&m.id.as_str())).cloned().collect();
        editor.update(PROVIDER_ID, |provider| {
            let encoding = provider.settings.as_ref().and_then(|s| s.get("encoding")).filter(|v| !v.is_null()).cloned();
            let mut patch = json!({
                "baseURL": TOKEN_SHARING_BASE_URL,
                "transport": "http",
                "compaction": { "type": "summary" },
            });
            // ChatGPT sign-in rejects uncompressed bodies over ~1 MiB and decodes zstd.
            if !configured {
                patch["encoding"] = encoding.unwrap_or_else(|| json!("zstd"));
            }
            provider.settings = overlay(provider.settings.as_ref(), patch);
        });
        let Some(updated) = editor.get(PROVIDER_ID) else { return };
        let info = updated.provider.clone();
        editor.add(info, models, Some(connection.clone()));
    }

    fn models(&mut self, editor: &mut ModelEditor<'_>) {
        if !matches!(self.sign_in, SignIn::TokenSharing { .. }) {
            return;
        }
        for (pid, mid) in editor.list(Some(PROVIDER_ID)) {
            editor.update(&pid, &mid, |model| {
                let over = Compatibility { supports_effort_updates: Some(false), ..Default::default() };
                model.compatibility = Some(model.compatibility.clone().unwrap_or_default().merged(&over));
                // Token sharing does not support native /responses/compact.
                let mut settings = model.settings.clone().unwrap_or_default();
                settings.insert("compaction".into(), json!({ "type": "summary" }));
                model.settings = Some(settings);
                if pro(model) {
                    model.enabled = false;
                    return;
                }
                model.cost = vec![];
            });
        }
    }
}

/// The Codex catalog effects of `OpenAIPlugin` (its WebSocket transport default is
/// [`super::plugins::ResponsesWebSockets`]).
pub struct OpenAi {
    pub sign_in: SignIn,
    /// Config documents' `providers` objects (for a configured base URL).
    pub sections: Vec<Value>,
}

impl CatalogPlugin for OpenAi {
    fn providers(&mut self, editor: &mut ProviderEditor) {
        let SignIn::Codex { account_id } = &self.sign_in else { return };
        let Some(record) = editor.get(PROVIDER_ID) else { return };
        let configured = configured_base_url(record.provider.settings.as_ref(), &self.sections);
        editor.update(PROVIDER_ID, |provider| {
            let current = provider.settings.as_ref();
            let transport =
                current.and_then(|s| s.get("transport")).filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!("websocket"));
            let mut patch = json!({ "transport": transport, "baseURL": CODEX_BASE_URL });
            // The ChatGPT backend rejects uncompressed bodies over ~1 MiB and decodes zstd.
            if !configured {
                patch["encoding"] =
                    current.and_then(|s| s.get("encoding")).filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!("zstd"));
            }
            provider.settings = overlay(current, patch);
            let mut headers = HeaderMap::new();
            headers.insert("originator".into(), "opencode".into());
            headers.insert("x-codex-beta-features".into(), "remote_compaction_v2".into());
            if let Some(account) = account_id {
                headers.insert("chatgpt-account-id".into(), account.clone());
            }
            provider.headers = merge_headers(provider.headers.as_ref(), Some(&headers));
        });
    }

    fn models(&mut self, editor: &mut ModelEditor<'_>) {
        if !matches!(self.sign_in, SignIn::Codex { .. }) {
            return;
        }
        // ChatGPT-plan tokens only authorize codex-eligible models, and the subscription covers
        // usage, so hide the rest and zero the cost.
        for (pid, mid) in editor.list(Some(PROVIDER_ID)) {
            editor.update(&pid, &mid, |model| {
                if pro(model) {
                    model.enabled = false;
                    return;
                }
                // The image models the Codex client generates and edits with on the Codex login.
                if model.kind.as_deref() == Some("image") {
                    if CODEX_IMAGE_MODELS.contains(&model.model_id.as_str()) {
                        model.cost = vec![];
                    } else {
                        model.enabled = false;
                    }
                    return;
                }
                if !codex_eligible(&model.model_id) {
                    model.enabled = false;
                    return;
                }
                model.cost = vec![];
            });
        }
    }
}

/// `OpenAIContextCapsPlugin`: under a ChatGPT sign-in, OpenAI models take the plan's input window.
pub struct OpenAiContextCaps {
    pub sign_in: SignIn,
}

/// The capped limit of a model under a ChatGPT sign-in (`None`: unchanged).
pub fn capped_limit(api_id: &str, limit: &Limit) -> Option<Limit> {
    let input = input_cap(api_id);
    let current = limit.input.filter(|i| *i != 0).unwrap_or(limit.context);
    if current <= input {
        return None;
    }
    Some(Limit { input: Some(input), context: input + limit.output, output: limit.output })
}

impl CatalogPlugin for OpenAiContextCaps {
    fn models(&mut self, editor: &mut ModelEditor<'_>) {
        if !self.sign_in.chatgpt() {
            return;
        }
        for (pid, mid) in editor.list(Some(PROVIDER_ID)) {
            let Some(limit) = editor.get(&pid, &mid).and_then(|m| capped_limit(&m.model_id, &m.limit)) else { continue };
            editor.update(&pid, &mid, |model| model.limit = limit);
        }
    }
}

/// The plugin storage key of a token-sharing client's saved models (`modelCacheKey`).
pub fn model_cache_key(client_id: &str) -> String {
    format!("models:{client_id}")
}

/// `RemoteModel`: a ChatGPT `/models` entry.
fn remote_model(value: &Value) -> bool {
    let Some(o) = value.as_object() else { return false };
    let string = |k: &str| o.get(k).is_some_and(Value::is_string);
    let strings = |v: Option<&Value>| v.and_then(Value::as_array).is_some_and(|a| a.iter().all(Value::is_string));
    let window = o.get("context_window").and_then(Value::as_f64).is_some_and(|w| w.fract() == 0.0 && w > 1.0);
    let levels = match o.get("supported_reasoning_levels") {
        None => true,
        Some(v) => v.as_array().is_some_and(|a| a.iter().all(|l| l.get("effort").is_some_and(Value::is_string))),
    };
    string("slug")
        && string("display_name")
        && string("visibility")
        && o.get("supported_in_api").is_some_and(Value::is_boolean)
        && window
        && strings(o.get("input_modalities"))
        && levels
}

/// `decodeCachedModels`: an array of `RemoteModel`.
pub fn remote_models(value: Option<&Value>) -> bool {
    value.and_then(Value::as_array).is_some_and(|a| a.iter().all(remote_model))
}

/// A token-sharing credential saved before it had its own method id: a `chatgpt-browser` OAuth
/// credential whose metadata decodes as token sharing's (`{clientID, scopes?, models?}`). Returns the
/// client id, the saved models and the metadata without them.
pub fn legacy_token_sharing(method_id: &str, metadata: Option<&Object>) -> Option<(String, Option<Value>, Object)> {
    if method_id != codex::BROWSER_METHOD_ID {
        return None;
    }
    let metadata = metadata?;
    let client_id = metadata.get("clientID")?.as_str()?.to_string();
    if metadata.get("scopes").is_some_and(|s| !s.as_array().is_some_and(|a| a.iter().all(Value::is_string))) {
        return None;
    }
    let models = metadata.get("models").cloned();
    if models.as_ref().is_some_and(|m| !remote_models(Some(m))) {
        return None;
    }
    let mut rest = metadata.clone();
    rest.shift_remove("models");
    Some((client_id, models, rest))
}

/// `true` when `url` parses and its origin is `https://api.openai.com`.
fn openai_origin(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| u.origin().ascii_serialization() == "https://api.openai.com")
}

/// The Codex `session.model.request` hook: a direct OpenAI base URL goes to Codex, and requests
/// carry `originator` and the session's prompt-cache affinity as `session-id`. Returns the new base
/// URL, if it changes.
pub fn codex_model_request(base_url: Option<&str>, headers: &mut IndexMap<String, String>, affinity: &str) -> Option<String> {
    let next = base_url.filter(|u| openai_origin(u)).map(|_| CODEX_BASE_URL.to_string());
    headers.insert("originator".into(), "opencode".into());
    // ChatGPT routes its prompt cache on this header, so children share the parent's.
    headers.insert("session-id".into(), affinity.into());
    next
}

/// The token-sharing `session.model.request` hook: the Codex client's session headers (ChatGPT
/// derives prompt-cache affinity from `session-id`).
pub fn token_sharing_model_request(headers: &mut IndexMap<String, String>, session_id: &str, affinity: &str) {
    headers.insert("session-id".into(), affinity.into());
    headers.insert("thread-id".into(), session_id.into());
    headers.insert("x-client-request-id".into(), session_id.into());
}

/// The token-sharing `session.retry` hook: a failure whose response body names a sharing error that
/// will not clear by itself is not retried.
pub fn token_sharing_forbids_retry(response_body: Option<&str>) -> bool {
    chatgpt::forbids_retry(response_body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_eligibility() {
        for id in ["gpt-5.6-sol", "gpt-6-astra", "gpt-5.10", "gpt-6.1-sol", "gpt-7"] {
            assert!(codex_eligible(id), "{id}");
        }
        for id in ["gpt-5.5", "gpt-5.3-codex-spark", "gpt-5.4", "gpt-5.6", "gpt-4.1", "gpt-5", "gpt-5.04-astra", "gpt-4.99", "o3"] {
            assert!(!codex_eligible(id), "{id}");
        }
    }

    #[test]
    fn caps() {
        assert_eq!(input_cap("gpt-5.5"), LEGACY_INPUT);
        assert_eq!(input_cap("gpt-6-luna"), LEGACY_INPUT);
        assert_eq!(input_cap("gpt-6.1-sol"), CURRENT_INPUT);
        assert_eq!(input_cap("gpt-7"), CURRENT_INPUT);
        assert_eq!(input_cap("o3"), CURRENT_INPUT);
        let full = Limit { context: 1_050_000, input: Some(922_000), output: 128_000 };
        assert_eq!(capped_limit("gpt-5.6-sol", &full), Some(Limit { context: 400_000, input: Some(272_000), output: 128_000 }));
        let small = Limit { context: 400_000, input: Some(272_000), output: 128_000 };
        assert_eq!(capped_limit("gpt-6.2-mini", &small), None);
        let no_input = Limit { context: 300_000, input: None, output: 10_000 };
        assert_eq!(capped_limit("gpt-5.6", &no_input), Some(Limit { context: 282_000, input: Some(272_000), output: 10_000 }));
    }

    #[test]
    fn codex_request_hook() {
        let mut headers = IndexMap::new();
        assert_eq!(codex_model_request(Some("https://api.openai.com/v1"), &mut headers, "ses_a").as_deref(), Some(CODEX_BASE_URL));
        assert_eq!(headers.get("originator").map(String::as_str), Some("opencode"));
        assert_eq!(headers.get("session-id").map(String::as_str), Some("ses_a"));
        assert_eq!(codex_model_request(Some("https://proxy.example/v1?region=us"), &mut IndexMap::new(), "ses_a"), None);
        assert_eq!(codex_model_request(Some("not a url"), &mut IndexMap::new(), "ses_a"), None);
        assert_eq!(codex_model_request(None, &mut IndexMap::new(), "ses_a"), None);
    }
}
