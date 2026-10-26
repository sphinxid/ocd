//! AI SDK npm package → native `@opencode/ai/providers/*` entrypoint mapping and settings translation
//! (`packages/core/src/aisdk-native.ts`).

use super::types::{headers_from_value, is_aisdk, merge_headers, merge_overlay, package_name, HeaderMap, Variant, BUILTIN_PACKAGES};
use crate::json::Object;
use serde_json::{json, Value};

const OPENAI_COMPATIBLE: &str = "@opencode/ai/providers/openai-compatible";
const BEDROCK: &str = "@opencode/ai/providers/amazon-bedrock";
const OPENROUTER: &str = "@opencode/ai/providers/openrouter";

/// npm package → native entrypoint (`PACKAGES`).
pub const PACKAGES: &[(&str, &str)] = &[
    ("@ai-sdk/amazon-bedrock", "@opencode/ai/providers/amazon-bedrock"),
    ("@ai-sdk/alibaba", "@opencode/ai/providers/alibaba/chat"),
    ("@ai-sdk/anthropic", "@opencode/ai/providers/anthropic"),
    ("@ai-sdk/azure", "@opencode/ai/providers/azure/responses"),
    ("@ai-sdk/cerebras", "@opencode/ai/providers/cerebras"),
    ("@ai-sdk/cohere", "@opencode/ai/providers/cohere"),
    ("@ai-sdk/deepinfra", "@opencode/ai/providers/deepinfra"),
    ("@ai-sdk/google", "@opencode/ai/providers/google"),
    ("@ai-sdk/google-vertex", "@opencode/ai/providers/google-vertex"),
    ("@ai-sdk/google-vertex/anthropic", "@opencode/ai/providers/google-vertex/messages"),
    ("@ai-sdk/groq", "@opencode/ai/providers/groq"),
    ("@ai-sdk/mistral", "@opencode/ai/providers/mistral"),
    ("@ai-sdk/openai", "@opencode/ai/providers/openai"),
    ("@ai-sdk/openai-compatible", "@opencode/ai/providers/openai-compatible"),
    ("@ai-sdk/togetherai", "@opencode/ai/providers/togetherai"),
    ("@ai-sdk/xai", "@opencode/ai/providers/xai"),
    ("@ai-sdk/gateway", "@opencode/ai/providers/vercel-ai-gateway"),
    ("@openrouter/ai-sdk-provider", "@opencode/ai/providers/openrouter"),
    ("ai-gateway-provider", "@opencode/ai/providers/cloudflare-ai-gateway"),
    ("venice-ai-sdk-provider", "@opencode/ai/providers/venice"),
];

fn package(npm: &str) -> Option<&'static str> {
    PACKAGES.iter().find(|(k, _)| *k == npm).map(|(_, v)| *v)
}

/// `protocols(name)`: openai-compatible → chat, anthropic → messages, openai → responses.
fn protocols(name: &'static str) -> Vec<(&'static str, String)> {
    vec![
        ("@ai-sdk/openai-compatible", format!("@opencode/ai/providers/{name}/chat")),
        ("@ai-sdk/anthropic", format!("@opencode/ai/providers/{name}/messages")),
        ("@ai-sdk/openai", format!("@opencode/ai/providers/{name}/responses")),
    ]
}

fn single(npm: &'static str, target: &str) -> Vec<(&'static str, String)> {
    vec![(npm, target.to_string())]
}

/// Per-provider host table (`HOSTS`).
pub fn host_table(provider_id: &str) -> Option<Vec<(&'static str, String)>> {
    Some(match provider_id {
        "alibaba" | "alibaba-cn" | "alibaba-coding-plan" | "alibaba-coding-plan-cn" | "alibaba-token-plan"
        | "alibaba-token-plan-cn" => protocols("alibaba"),
        "baseten" => single("@ai-sdk/openai-compatible", "@opencode/ai/providers/baseten"),
        "cloudflare-ai-gateway" => vec![
            ("@ai-sdk/anthropic", "@opencode/ai/providers/cloudflare-ai-gateway".into()),
            ("@ai-sdk/openai", "@opencode/ai/providers/cloudflare-ai-gateway".into()),
            ("@ai-sdk/openai-compatible", "@opencode/ai/providers/cloudflare-ai-gateway".into()),
            ("ai-gateway-provider", "@opencode/ai/providers/cloudflare-ai-gateway".into()),
        ],
        "cohere" => single("@ai-sdk/openai-compatible", "@opencode/ai/providers/cohere/chat"),
        "cloudflare-workers-ai" => single("@ai-sdk/openai-compatible", "@opencode/ai/providers/cloudflare-workers-ai"),
        "deepseek" => single("@ai-sdk/openai-compatible", "@opencode/ai/providers/deepseek"),
        "digitalocean" => single("@ai-sdk/openai-compatible", "@opencode/ai/providers/digitalocean"),
        "fireworks-ai" => single("@ai-sdk/openai-compatible", "@opencode/ai/providers/fireworks"),
        "google-vertex" => vec![
            ("@ai-sdk/openai-compatible", "@opencode/ai/providers/google-vertex/chat".into()),
            ("@ai-sdk/mistral", "@opencode/ai/providers/google-vertex/mistral".into()),
        ],
        "kimi-for-coding" => protocols("moonshot"),
        "meta" => protocols("meta"),
        "minimax" | "minimax-cn" | "minimax-coding-plan" | "minimax-cn-coding-plan" => protocols("minimax"),
        "moonshotai" | "moonshotai-cn" => protocols("moonshot"),
        "zai" | "zhipuai" => single("@ai-sdk/openai-compatible", "@opencode/ai/providers/zai/chat"),
        "zai-coding-plan" | "zhipuai-coding-plan" => protocols("zai-coding-plan"),
        _ => return None,
    })
}

fn host(provider_id: &str, npm: &str) -> Option<String> {
    host_table(provider_id)?.into_iter().find(|(k, _)| *k == npm).map(|(_, v)| v)
}

/// Mapping context (`Context`).
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeContext<'a> {
    pub provider_id: &'a str,
    pub canonical: Option<&'a str>,
    pub model_id: Option<&'a str>,
    /// models.dev `provider.shape`: `"responses" | "completions"`.
    pub shape: Option<&'a str>,
    pub settings: Option<&'a Object>,
}

/// `AISDKNative.native(npm, context)`: host table, Bedrock Mantle, Azure chat, then the package table.
pub fn native(npm: &str, context: &NativeContext<'_>) -> Option<String> {
    if let Some(found) = host(context.provider_id, npm) {
        return Some(found);
    }
    if npm == "@ai-sdk/amazon-bedrock/mantle" {
        return Some(mantle(context.model_id));
    }
    if npm == "@ai-sdk/azure" && azure_chat(context) {
        return Some("@opencode/ai/providers/azure/chat".into());
    }
    package(npm).map(String::from)
}

fn azure_chat(context: &NativeContext<'_>) -> bool {
    context.shape == Some("completions")
        || context.settings.and_then(|s| s.get("useCompletionUrls")) == Some(&Value::Bool(true))
}

/// Mantle serves Claude only through Anthropic Messages and GPT-OSS only through Chat.
fn mantle(model_id: Option<&str>) -> String {
    match model_id {
        None => "@opencode/ai/providers/amazon-bedrock/mantle".into(),
        Some(id) if id.contains("anthropic.") => "@opencode/ai/providers/amazon-bedrock/mantle/messages".into(),
        Some(id) => format!(
            "@opencode/ai/providers/amazon-bedrock/mantle/{}",
            if id.contains("gpt-oss") { "chat" } else { "responses" }
        ),
    }
}

/// `resolve(specifier, context)`: the native entrypoint for a stored package specifier, if any.
pub fn resolve(specifier: &str, context: &NativeContext<'_>) -> Option<String> {
    let npm = package_name(specifier);
    if is_aisdk(Some(specifier)) || package(&npm).is_some() || host(context.provider_id, &npm).is_some() {
        return native(&npm, context);
    }
    if npm == "@opencode/ai/providers/amazon-bedrock/mantle" || npm == "@opencode/ai/providers/amazon-bedrock-mantle" {
        return Some(mantle(context.model_id));
    }
    if (npm == "@opencode/ai/providers/azure" || npm == "@opencode/ai/providers/azure/responses") && azure_chat(context) {
        return Some("@opencode/ai/providers/azure/chat".into());
    }
    if BUILTIN_PACKAGES.contains(&npm.as_str()) { Some(npm) } else { None }
}

/// A record whose package and overlays are rewritten on write (`Provider.Info` / `Model.Info`).
pub struct RewriteTarget<'a> {
    pub package: &'a mut Option<String>,
    pub settings: &'a mut Option<Object>,
    pub headers: &'a mut Option<HeaderMap>,
    pub body: &'a mut Option<Object>,
    pub variants: Option<&'a mut Vec<Variant>>,
}

/// `AISDKNative.rewrite`: map an AI SDK package (and its settings) onto the native package.
pub fn rewrite(target: RewriteTarget<'_>, specifier: Option<&str>, context: &NativeContext<'_>) {
    let Some(specifier) = specifier else { return };
    let plain_context = NativeContext { settings: None, ..*context };
    let Some(plain) = resolve(specifier, &plain_context) else { return };
    let settings = decode(target.settings.as_ref().unwrap_or(&Object::new()));
    let replacement = resolve(specifier, &NativeContext { settings: Some(&settings), ..*context }).unwrap_or(plain);
    let translated = options(&replacement, context.model_id, &settings);
    if target.package.is_some() || replacement != specifier {
        *target.package = Some(replacement.clone());
    }
    if target.settings.is_some() {
        let mut next = translated.settings.clone();
        // Without `provider`, the native compatible package reports every AI SDK-mapped host as
        // `openai-compatible`.
        if replacement == OPENAI_COMPATIBLE && !BUILTIN_PACKAGES.contains(&specifier) {
            next.insert("provider".into(), Value::String(context.canonical.unwrap_or(context.provider_id).into()));
        }
        *target.settings = Some(next);
    }
    *target.headers = merge_headers(translated.headers.as_ref(), target.headers.as_ref());
    *target.body = merge_overlay(translated.body.as_ref(), target.body.as_ref());
    if let Some(variants) = target.variants {
        for variant in variants.iter_mut() {
            let overlay = options(&replacement, context.model_id, &decode(variant.settings.as_ref().unwrap_or(&Object::new())));
            let headers = merge_headers(overlay.headers.as_ref(), variant.headers.as_ref());
            let body = merge_overlay(overlay.body.as_ref(), variant.body.as_ref());
            if variant.settings.is_some() {
                variant.settings = Some(overlay.settings);
            }
            variant.headers = headers;
            variant.body = body;
        }
    }
}

/// Rewrite a provider record in place.
pub fn rewrite_provider(provider: &mut super::types::ProviderInfo) {
    let specifier = provider.package.clone();
    let mut package = Some(provider.package.clone());
    let id = provider.id.clone();
    let canonical = provider.canonical.clone();
    rewrite(
        RewriteTarget {
            package: &mut package,
            settings: &mut provider.settings,
            headers: &mut provider.headers,
            body: &mut provider.body,
            variants: None,
        },
        Some(&specifier),
        &NativeContext { provider_id: &id, canonical: canonical.as_deref(), ..Default::default() },
    );
    if let Some(package) = package {
        provider.package = package;
    }
}

/// Rewrite a model record in place (`specifier = model.package ?? provider.package`).
pub fn rewrite_model(model: &mut super::types::ModelInfo, provider: Option<&super::types::ProviderInfo>) {
    let specifier = model.package.clone().or_else(|| provider.map(|p| p.package.clone()));
    let provider_id = model.provider_id.clone();
    let canonical = model.canonical.clone().or_else(|| provider.and_then(|p| p.canonical.clone()));
    let model_id = model.model_id.clone();
    rewrite(
        RewriteTarget {
            package: &mut model.package,
            settings: &mut model.settings,
            headers: &mut model.headers,
            body: &mut model.body,
            variants: Some(&mut model.variants),
        },
        specifier.as_deref(),
        &NativeContext { provider_id: &provider_id, canonical: canonical.as_deref(), model_id: Some(&model_id), ..Default::default() },
    );
}

// ---------------------------------------------------------------------------------------------
// Settings translation
// ---------------------------------------------------------------------------------------------

/// Translated settings plus request overlays (`Overlay`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Translated {
    pub settings: Object,
    pub headers: Option<HeaderMap>,
    pub body: Option<Object>,
}

const OPENROUTER_KEYS: &[&str] = &["appName", "appUrl", "api_keys", "compatibility"];
const BEDROCK_KEYS: &[&str] =
    &["bearerToken", "endpoint", "credentials", "credentialProvider", "accessKeyId", "secretAccessKey", "sessionToken"];
const CONVERSE_KEYS: &[&str] = &["additionalModelRequestFields", "reasoningConfig", "anthropicBeta", "serviceTier"];

fn omit(settings: &Object, keys: &[&str]) -> Object {
    settings.iter().filter(|(k, _)| !keys.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn get_str<'a>(o: &'a Object, key: &str) -> Option<&'a str> {
    o.get(key).and_then(Value::as_str)
}

/// `options(replacement, modelID, settings)`.
pub fn options(replacement: &str, model_id: Option<&str>, settings: &Object) -> Translated {
    let converse = replacement == BEDROCK && model_id.is_some();
    let mut drop: Vec<&str> = vec!["headers", "extraBody", "useCompletionUrls"];
    drop.extend_from_slice(OPENROUTER_KEYS);
    let kept = omit(settings, &drop);
    let thinking = if converse { bedrock_thinking(model_id, settings) } else { None };
    let mut out_settings =
        if replacement.starts_with(BEDROCK) { bedrock_settings(&kept, converse) } else { kept };
    if let Some(thinking) = thinking {
        out_settings.insert("thinking".into(), thinking);
    }
    let mut translated = Translated {
        settings: out_settings,
        headers: settings.get("headers").and_then(headers_from_value),
        body: settings.get("extraBody").and_then(Value::as_object).cloned(),
    };
    if converse {
        if let Some(body) = bedrock_request(model_id, settings) {
            translated.body = Some(body);
        }
    }
    if replacement == OPENROUTER {
        if let Some(headers) = open_router_request(settings) {
            translated.headers = Some(headers);
        }
    }
    translated
}

fn bedrock_settings(settings: &Object, converse: bool) -> Object {
    let credentials_obj = settings.get("credentials").and_then(Value::as_object);
    let region = get_str(settings, "region").or_else(|| credentials_obj.and_then(|c| get_str(c, "region")));
    let credentials = credentials_obj.unwrap_or(settings);
    let base_url = get_str(settings, "baseURL").or_else(|| get_str(settings, "endpoint"));
    let mut keys: Vec<&str> = BEDROCK_KEYS.to_vec();
    if converse {
        keys.extend_from_slice(CONVERSE_KEYS);
    }
    let mut out = omit(settings, &keys);
    if let Some(base_url) = base_url {
        let value = match region {
            Some(region) => base_url.replace("${AWS_REGION}", region),
            None => base_url.to_string(),
        };
        out.insert("baseURL".into(), Value::String(value));
    }
    if settings.get("apiKey").is_none() {
        if let Some(token) = get_str(settings, "bearerToken") {
            out.insert("apiKey".into(), Value::String(token.into()));
        }
    }
    if let (Some(key), Some(secret)) = (get_str(credentials, "accessKeyId"), get_str(credentials, "secretAccessKey")) {
        let mut creds = Object::new();
        if let Some(region) = region {
            creds.insert("region".into(), Value::String(region.into()));
        }
        creds.insert("accessKeyId".into(), Value::String(key.into()));
        creds.insert("secretAccessKey".into(), Value::String(secret.into()));
        if let Some(session) = get_str(credentials, "sessionToken") {
            creds.insert("sessionToken".into(), Value::String(session.into()));
        }
        out.insert("credentials".into(), Value::Object(creds));
    }
    out
}

fn bedrock_thinking(model_id: Option<&str>, settings: &Object) -> Option<Value> {
    let reasoning = settings.get("reasoningConfig").and_then(Value::as_object)?;
    if !model_id.map(|m| m.contains("anthropic")).unwrap_or(false) {
        return None;
    }
    if get_str(reasoning, "type") != Some("enabled") {
        return None;
    }
    let budget = reasoning.get("budgetTokens").filter(|v| v.is_number())?;
    Some(json!({"type": "enabled", "budgetTokens": budget}))
}

fn bedrock_request(model_id: Option<&str>, settings: &Object) -> Option<Object> {
    let empty = Object::new();
    let additional = settings.get("additionalModelRequestFields").and_then(Value::as_object).unwrap_or(&empty);
    let reasoning = settings.get("reasoningConfig").and_then(Value::as_object);
    let id = model_id.unwrap_or("");
    let anthropic = id.contains("anthropic");
    let openai = id.contains("openai.");
    let harmony = id.contains("openai.gpt-oss");
    let effort = reasoning.and_then(|r| r.get("maxReasoningEffort")).cloned();
    let kind = reasoning.and_then(|r| get_str(r, "type"));
    let budget = reasoning.and_then(|r| r.get("budgetTokens")).cloned();
    let display = reasoning.and_then(|r| r.get("display")).cloned();
    let betas: Vec<Value> = settings.get("anthropicBeta").and_then(Value::as_array).cloned().unwrap_or_default();

    let mut patch = Object::new();
    if !betas.is_empty() {
        let mut merged: Vec<Value> = additional.get("anthropic_beta").and_then(Value::as_array).cloned().unwrap_or_default();
        merged.extend(betas);
        patch.insert("anthropic_beta".into(), Value::Array(merged));
    }
    if anthropic && kind == Some("adaptive") {
        let mut thinking = Object::new();
        thinking.insert("type".into(), json!("adaptive"));
        if let Some(display) = display {
            thinking.insert("display".into(), display);
        }
        patch.insert("thinking".into(), Value::Object(thinking));
    }
    if let Some(effort) = effort.clone() {
        if anthropic {
            let mut output = additional.get("output_config").and_then(Value::as_object).cloned().unwrap_or_default();
            output.insert("effort".into(), effort);
            patch.insert("output_config".into(), Value::Object(output));
        } else if openai && harmony {
            patch.insert("reasoning_effort".into(), effort);
        } else if openai {
            let mut r = additional.get("reasoning").and_then(Value::as_object).cloned().unwrap_or_default();
            r.insert("effort".into(), effort);
            patch.insert("reasoning".into(), Value::Object(r));
        } else {
            let mut config = Object::new();
            if let Some(kind) = kind.filter(|k| *k != "adaptive") {
                config.insert("type".into(), json!(kind));
            }
            if let Some(budget) = budget {
                config.insert("budgetTokens".into(), budget);
            }
            config.insert("maxReasoningEffort".into(), effort);
            patch.insert("reasoningConfig".into(), Value::Object(config));
        }
    }
    let fields = merge_overlay(Some(additional), Some(&patch)).unwrap_or_default();
    let mut body = Object::new();
    if !fields.is_empty() {
        body.insert("additionalModelRequestFields".into(), Value::Object(fields));
    }
    if let Some(tier) = settings.get("serviceTier") {
        body.insert("serviceTier".into(), json!({"type": tier}));
    }
    if body.is_empty() { None } else { Some(body) }
}

fn open_router_request(settings: &Object) -> Option<HeaderMap> {
    let mut generated = HeaderMap::new();
    if let Some(name) = get_str(settings, "appName") {
        generated.insert("X-OpenRouter-Title".into(), name.into());
    }
    if let Some(url) = get_str(settings, "appUrl") {
        generated.insert("HTTP-Referer".into(), url.into());
    }
    if let Some(keys) = settings.get("api_keys").and_then(Value::as_object) {
        if !keys.is_empty() {
            generated.insert("X-Provider-API-Keys".into(), crate::json::encode(&Value::Object(keys.clone())));
        }
    }
    let user = settings.get("headers").and_then(headers_from_value);
    let headers = merge_headers(Some(&generated), user.as_ref()).unwrap_or_default();
    if headers.is_empty() { None } else { Some(headers) }
}

// ---------------------------------------------------------------------------------------------
// Lenient decoding of AI SDK settings (`Legacy` schema)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Kind {
    Str,
    Bool,
    StrRecord,
    Record,
    StrArray,
    Auth,
    Credentials,
    Reasoning,
    Additional,
}

const LEGACY: &[(&str, Kind)] = &[
    ("apiKey", Kind::Str),
    ("baseURL", Kind::Str),
    ("headers", Kind::StrRecord),
    ("extraBody", Kind::Record),
    ("useCompletionUrls", Kind::Bool),
    ("auth", Kind::Auth),
    ("bearerToken", Kind::Str),
    ("endpoint", Kind::Str),
    ("region", Kind::Str),
    ("credentials", Kind::Credentials),
    ("accessKeyId", Kind::Str),
    ("secretAccessKey", Kind::Str),
    ("sessionToken", Kind::Str),
    ("anthropicBeta", Kind::StrArray),
    ("serviceTier", Kind::Str),
    ("reasoningConfig", Kind::Reasoning),
    ("additionalModelRequestFields", Kind::Additional),
    ("appName", Kind::Str),
    ("appUrl", Kind::Str),
    ("api_keys", Kind::StrRecord),
];

fn decode_kind(kind: Kind, value: &Value) -> Option<Value> {
    match kind {
        Kind::Str => value.as_str().map(|_| value.clone()),
        Kind::Bool => value.as_bool().map(|_| value.clone()),
        Kind::StrRecord => headers_from_value(value).map(|_| value.clone()),
        Kind::Record => value.as_object().map(|_| value.clone()),
        Kind::StrArray => {
            let items = value.as_array()?;
            if items.iter().all(Value::is_string) { Some(value.clone()) } else { None }
        }
        Kind::Auth => match value.as_str() {
            Some("bearer") | Some("sigv4") => Some(value.clone()),
            _ => None,
        },
        Kind::Credentials => {
            let o = value.as_object()?;
            let key = o.get("accessKeyId").and_then(Value::as_str)?;
            let secret = o.get("secretAccessKey").and_then(Value::as_str)?;
            let mut out = Object::new();
            out.insert("accessKeyId".into(), json!(key));
            out.insert("secretAccessKey".into(), json!(secret));
            for k in ["sessionToken", "region"] {
                if let Some(v) = o.get(k).filter(|v| v.is_string()) {
                    out.insert(k.into(), v.clone());
                }
            }
            Some(Value::Object(out))
        }
        Kind::Reasoning => {
            let o = value.as_object()?;
            let mut out = Object::new();
            for k in ["type", "display", "maxReasoningEffort"] {
                if let Some(v) = o.get(k).filter(|v| v.is_string()) {
                    out.insert(k.into(), v.clone());
                }
            }
            if let Some(v) = o.get("budgetTokens").filter(|v| v.is_number()) {
                out.insert("budgetTokens".into(), v.clone());
            }
            Some(Value::Object(out))
        }
        Kind::Additional => {
            let o = value.as_object()?;
            let mut out = Object::new();
            if let Some(v) = o.get("anthropic_beta").and_then(|v| decode_kind(Kind::StrArray, v)) {
                out.insert("anthropic_beta".into(), v);
            }
            for k in ["output_config", "reasoning"] {
                if let Some(v) = o.get(k).filter(|v| v.is_object()) {
                    out.insert(k.into(), v.clone());
                }
            }
            for (k, v) in o {
                if !matches!(k.as_str(), "anthropic_beta" | "output_config" | "reasoning") {
                    out.insert(k.clone(), v.clone());
                }
            }
            Some(Value::Object(out))
        }
    }
}

/// `decode(Legacy)`: known keys first (in schema order, invalid values dropped), then the rest in input order.
pub fn decode(settings: &Object) -> Object {
    let mut out = Object::new();
    for (key, kind) in LEGACY {
        if let Some(value) = settings.get(*key) {
            if let Some(decoded) = decode_kind(*kind, value) {
                out.insert((*key).to_string(), decoded);
            }
        }
    }
    for (key, value) in settings {
        if !LEGACY.iter().any(|(k, _)| k == key) {
            out.insert(key.clone(), value.clone());
        }
    }
    out
}
