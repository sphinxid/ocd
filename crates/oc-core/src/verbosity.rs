//! The built-in `opencode.prompt.verbosity` plugin (`core/src/plugin/verbosity.ts`): OpenAI models
//! known to support text verbosity answer with `textVerbosity: "low"` unless the request, the model or
//! the selected variant already sets it (a configured `null` counts as set). Its
//! `session.context` / `session.compaction` / `session.generate` / `session.title` hooks run in
//! [`crate::plugin_request::shape`].

use oc_ai::catalog::ModelInfo;

/// The plugin id (gated by the location's `plugins` configuration).
pub const PLUGIN_ID: &str = "opencode.prompt.verbosity";

/// Packages that call OpenAI's Responses API with the plain OpenAI model id.
const DIRECT: [&str; 4] = [
    "@opencode/ai/providers/openai",
    "@opencode/ai/providers/openai/responses",
    "@opencode/ai/providers/azure",
    "@opencode/ai/providers/azure/responses",
];

/// Gateways that route `openai/<id>` model ids to OpenAI.
const GATEWAYS: [&str; 3] =
    ["@opencode/ai/providers/cloudflare-ai-gateway", "@opencode/ai/providers/vercel-ai-gateway", "aisdk:@ai-sdk/gateway"];

/// `supportsVerbosity`: every gpt-6 model, and the GPT-5 versions whose support is known.
pub fn supports_verbosity(id: &str) -> bool {
    static MINOR: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"(?:^|[/.])gpt-5\.[1-6](?:[.:-]|$)").unwrap());
    static BASE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"(?:^|[/.])gpt-5(?:-(?:mini|nano)(?:[.:-]|$)|$)").unwrap());
    if id.contains("gpt-6") {
        return true;
    }
    if id.contains("-chat") || id.contains("-image") {
        return false;
    }
    MINOR.is_match(id) || BASE.is_match(id)
}

/// `openAIModelID`: the OpenAI model id a catalog model reaches, if it calls OpenAI's Responses API.
pub fn openai_model_id(model: &ModelInfo) -> Option<String> {
    let id = model.model_id.to_lowercase();
    let package = model.package.as_deref().unwrap_or_default();
    if DIRECT.contains(&package) {
        return Some(id);
    }
    if package == "@opencode/ai/providers/amazon-bedrock/mantle/responses" {
        return id.strip_prefix("openai.").map(String::from);
    }
    if GATEWAYS.contains(&package) {
        return id.strip_prefix("openai/").map(String::from);
    }
    None
}

/// The hook's decision for a request without `textVerbosity`: `true` when it sets `"low"`.
pub fn defaults_to_low(model: &ModelInfo, variant: Option<&str>) -> bool {
    let Some(id) = openai_model_id(model) else { return false };
    if !supports_verbosity(&id) {
        return false;
    }
    if model.settings.as_ref().is_some_and(|s| s.contains_key("textVerbosity")) {
        return false;
    }
    let selected = variant.and_then(|v| model.variants.iter().find(|item| item.id == v));
    !selected.and_then(|v| v.settings.as_ref()).is_some_and(|s| s.contains_key("textVerbosity"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_ai::catalog::Variant;
    use serde_json::json;

    fn model(package: &str, model_id: &str) -> ModelInfo {
        let mut m = ModelInfo::default_for("p", "selected");
        m.model_id = model_id.into();
        m.package = Some(package.into());
        m
    }

    /// `sets known OpenAI Responses defaults without overriding configured or unknown models`
    /// (`packages/core/test/plugin/verbosity.test.ts`).
    #[test]
    fn sets_known_openai_responses_defaults_without_overriding_configured_or_unknown_models() {
        let openai = "@opencode/ai/providers/openai";
        for id in ["gpt-5.5", "gpt-5.6-luna-fast", "gpt-5.2-codex", "gpt-5-mini-fast", "gpt-6-astra-pro"] {
            assert!(defaults_to_low(&model(openai, id), None), "{id}");
        }
        for id in ["gpt-4o", "gpt-7", "gpt-5.7", "gpt-5.5-chat", "gpt-5.4-image-2"] {
            assert!(!defaults_to_low(&model(openai, id), None), "{id}");
        }
        assert!(!defaults_to_low(&model("@opencode/ai/providers/openai/chat", "gpt-5.5"), None));
        assert!(!defaults_to_low(&model("@opencode/ai/providers/openrouter", "gpt-5.5"), None));
        let mut configured = model(openai, "gpt-5.5");
        configured.settings = Some(json!({"textVerbosity": "medium"}).as_object().unwrap().clone());
        assert!(!defaults_to_low(&configured, None));
        let mut quiet = model(openai, "gpt-6-astra");
        quiet.variants = vec![Variant {
            id: "quiet".into(),
            settings: Some(json!({"textVerbosity": null}).as_object().unwrap().clone()),
            ..Default::default()
        }];
        assert!(!defaults_to_low(&quiet, Some("quiet")));
        assert!(defaults_to_low(&quiet, None));
        let responses = "@opencode/ai/providers/openai/responses";
        assert!(defaults_to_low(&model(responses, "gpt-6-astra"), None));
        let mut no_default = model(responses, "gpt-6-astra");
        no_default.settings = Some(json!({"textVerbosity": null}).as_object().unwrap().clone());
        assert!(!defaults_to_low(&no_default, None));
        for (package, id) in [
            ("@opencode/ai/providers/azure/responses", "gpt-5.5"),
            ("@opencode/ai/providers/amazon-bedrock/mantle/responses", "openai.gpt-6-sol"),
            ("@opencode/ai/providers/cloudflare-ai-gateway", "openai/gpt-5.6-sol"),
            ("@opencode/ai/providers/vercel-ai-gateway", "openai/gpt-6-astra-fast"),
        ] {
            assert!(defaults_to_low(&model(package, id), None), "{package} {id}");
        }
        for (package, id) in [
            ("@opencode/ai/providers/azure/chat", "gpt-5.5"),
            ("@opencode/ai/providers/amazon-bedrock", "global.openai.gpt-6-sol"),
            ("@opencode/ai/providers/cloudflare-ai-gateway", "workers-ai/gpt-5.5"),
            ("@opencode/ai/providers/vercel-ai-gateway", "anthropic/gpt-5.5"),
        ] {
            assert!(!defaults_to_low(&model(package, id), None), "{package} {id}");
        }
    }
}
