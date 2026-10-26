//! Native provider packages (`packages/ai/src/providers/*.ts`): `settings → LanguageModel`.
//!
//! [`language_model`] is the entry point used by the catalog resolver. `package` is a native entrypoint
//! such as `"@opencode/ai/providers/anthropic"`; `settings` is the flat settings record: connection keys
//! (`apiKey`, `baseURL`, `headers`, `body`, plus provider-specific keys like `region`, `project`,
//! `resourceName`) and everything else becomes provider options for the route's protocol.

pub mod wrappers;

use crate::error::ProviderConfigurationError;
use crate::json::{merge_records, Object};
use crate::protocols::{
    anthropic_messages as am, bedrock_converse as bc, cohere_chat, gemini, google_interactions as gi, mistral_chat,
    open_responses as orp, openai_chat as oc, openai_chat_variants as ocv, openai_responses as oai, options,
    responses_variants as rv, Protocol,
};
use crate::route::auth::{self, Credential};
use crate::route::framing::Framing;
use crate::route::{Auth, Endpoint, EndpointPath, Headers, LanguageModel, Route, RouteDefaults};
use crate::types::{Compatibility, GenerationOptions, HttpOptions};
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::sync::Arc;
use wrappers::{Gateway, GatewayApi};

pub const PREFIX: &str = "@opencode/ai/providers/";

type Result<T> = std::result::Result<T, ProviderConfigurationError>;

/// Flat settings with connection keys taken out; the remainder are provider options.
pub struct Settings {
    rest: Object,
}

impl Settings {
    pub fn new(settings: &Object) -> Self {
        Self { rest: settings.clone() }
    }
    pub fn take(&mut self, key: &str) -> Option<Value> {
        self.rest.shift_remove(key).filter(|v| !v.is_null())
    }
    pub fn string(&mut self, key: &str) -> Option<String> {
        self.take(key).and_then(|v| match v {
            Value::String(s) => Some(s),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        })
    }
    pub fn headers(&mut self) -> Option<IndexMap<String, String>> {
        let v = self.take("headers")?;
        let o = v.as_object()?;
        Some(o.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
    }
    pub fn body(&mut self) -> Option<Object> {
        self.take("body").and_then(|v| v.as_object().cloned())
    }
    pub fn options(self) -> Object {
        self.rest
    }
}

/// Common route defaults: headers, body overlay, provider options.
struct Defaults {
    headers: Option<IndexMap<String, String>>,
    body: Option<Object>,
    provider_options: Object,
    generation: Option<GenerationOptions>,
}

impl Defaults {
    fn from(settings: &mut Settings) -> Self {
        Self { headers: settings.headers(), body: settings.body(), provider_options: Object::new(), generation: None }
    }
    fn apply(&self, route: Route) -> Route {
        self.apply_with_po(route, self.provider_options.clone())
    }
    fn apply_with_po(&self, route: Route, provider_options: Object) -> Route {
        route.defaults(RouteDefaults {
            headers: self.headers.clone(),
            generation: self.generation.clone(),
            provider_options: Some(provider_options),
            http: self.body.clone().map(|b| HttpOptions { body: Some(b), ..Default::default() }),
        })
    }
    /// `withOpenAIOptions(modelID, defaults)`.
    fn apply_openai(&self, route: Route, model_id: &str) -> Route {
        let base = options::openai_default_provider_options(model_id);
        let merged = merge_records(&[Some(&base), Some(&self.provider_options)]).unwrap_or_default();
        self.apply_with_po(route, merged)
    }
}

fn bearer(api_key: Option<String>, env: &[&str]) -> Arc<dyn Auth> {
    auth::bearer_options(api_key, env)
}

fn model(route: Route, id: &str) -> Result<LanguageModel> {
    route.model(id)
}

fn with_compat(m: LanguageModel, compat: Compatibility) -> LanguageModel {
    m.with_compatibility(compat)
}

fn chat_route(id: &str, provider: &str, key: &str, base: Option<&str>, protocol: Arc<dyn Protocol>) -> Route {
    Route::new(id, protocol, Endpoint::new(base, "/chat/completions"), Framing::SseWithDone).initial_provider(provider, Some(key))
}

fn responses_route(id: &str, provider: &str, key: &str, base: Option<&str>, protocol: Arc<dyn Protocol>) -> Route {
    Route::new(id, protocol, Endpoint::new(base, orp::PATH), Framing::Sse).initial_provider(provider, Some(key))
}

fn anthropic_version() -> impl Fn(&crate::types::LLMRequest) -> Headers + Send + Sync + 'static {
    |_| {
        let mut h = Headers::new();
        h.insert("anthropic-version".into(), "2023-06-01".into());
        h
    }
}

fn compat(pairs: Value) -> Compatibility {
    serde_json::from_value(pairs).unwrap_or_default()
}

/// Build a language model for a native provider package.
pub fn language_model(package: &str, model_id: &str, settings: &Object) -> Result<LanguageModel> {
    let name = package.strip_prefix(PREFIX).unwrap_or(package);
    let mut s = Settings::new(settings);
    match name {
        "anthropic" => anthropic(model_id, &mut s),
        "anthropic-compatible" => anthropic_compatible(model_id, &mut s),
        "openai" | "openai/responses" => openai(model_id, &mut s, false),
        "openai/chat" => openai(model_id, &mut s, true),
        "openai-compatible" => openai_compatible(model_id, &mut s, false),
        "openai-compatible/responses" | "openai-compatible-responses" => openai_compatible(model_id, &mut s, true),
        "azure" | "azure/responses" => azure(model_id, &mut s, false),
        "azure/chat" => azure(model_id, &mut s, true),
        "amazon-bedrock" => amazon_bedrock(model_id, &mut s),
        "amazon-bedrock/mantle" | "amazon-bedrock/mantle/responses" | "amazon-bedrock-mantle" => mantle(model_id, &mut s, "responses"),
        "amazon-bedrock/mantle/chat" => mantle(model_id, &mut s, "chat"),
        "amazon-bedrock/mantle/messages" => mantle(model_id, &mut s, "messages"),
        "google" => google(model_id, &mut s, false),
        "google/interactions" => google(model_id, &mut s, true),
        "google-vertex" | "google-vertex/gemini" => vertex_gemini(model_id, &mut s),
        "google-vertex/interactions" | "google-vertex-interactions" => vertex_interactions(model_id, &mut s),
        "google-vertex/messages" | "google-vertex-messages" => vertex_messages(model_id, &mut s),
        "google-vertex/chat" | "google-vertex-chat" => vertex_openapi(model_id, &mut s, false),
        "google-vertex/responses" | "google-vertex-responses" => vertex_openapi(model_id, &mut s, true),
        "google-vertex/mistral" | "google-vertex-mistral" => vertex_mistral(model_id, &mut s),
        "mistral" => {
            let api_key = s.string("apiKey");
            let base = s.string("baseURL");
            let mut d = Defaults::from(&mut s);
            d.provider_options = s.options();
            let route = d.apply(mistral_chat::route()).base_url(Some(base.unwrap_or_else(|| mistral_chat::DEFAULT_BASE_URL.into()))).auth(bearer(api_key, &["MISTRAL_API_KEY"]));
            model(route, model_id)
        }
        "cohere" | "cohere/chat" => cohere(model_id, &mut s, name == "cohere/chat"),
        "xai" => xai(model_id, &mut s),
        "groq" => simple_chat(model_id, &mut s, "groq-chat", "groq", "https://api.groq.com/openai/v1", &["GROQ_API_KEY"], Arc::new(wrappers::GroqChat), compat(json!({"maxTokensField": "max_completion_tokens", "reasoningField": "reasoning", "requireReasoning": false, "supportsStrictMode": false}))),
        "deepseek" => simple_chat(model_id, &mut s, "deepseek-chat", "deepseek", "https://api.deepseek.com/v1", &["DEEPSEEK_API_KEY"], oc::protocol(), compat(json!({"maxTokensField": "max_tokens"}))),
        "cerebras" => simple_chat(model_id, &mut s, "cerebras-chat", "cerebras", "https://api.cerebras.ai/v1", &["CEREBRAS_API_KEY"], oc::protocol(), compat(json!({"maxTokensField": "max_tokens", "reasoningField": "reasoning", "supportsPromptCacheKey": true}))),
        "deepinfra" => {
            if let Some(Value::String(b)) = s.rest.get("baseURL").cloned() {
                let root = b.trim_end_matches('/').to_string();
                let fixed = if root.ends_with("/openai") { root } else { format!("{root}/openai") };
                s.rest.insert("baseURL".into(), json!(fixed));
            }
            simple_chat(model_id, &mut s, "deepinfra-chat", "deepinfra", "https://api.deepinfra.com/v1/openai", &["DEEPINFRA_API_KEY"], oc::protocol(), compat(json!({"maxTokensField": "max_tokens", "reasoningField": "reasoning_content", "supportsPromptCacheKey": true})))
        }
        "togetherai" => simple_chat(model_id, &mut s, "togetherai-chat", "togetherai", "https://api.together.xyz/v1", &["TOGETHER_API_KEY", "TOGETHER_AI_API_KEY"], oc::protocol(), compat(json!({"maxTokensField": "max_tokens", "supportsStrictMode": false}))),
        "fireworks" => simple_chat(model_id, &mut s, "fireworks-chat", "fireworks", "https://api.fireworks.ai/inference/v1", &["FIREWORKS_API_KEY"], oc::protocol(), compat(json!({"supportsPromptCacheKey": true}))),
        "baseten" => simple_chat(model_id, &mut s, "baseten-chat", "baseten", "https://inference.baseten.co/v1", &["BASETEN_API_KEY"], oc::protocol(), Compatibility::default()),
        "digitalocean" => {
            // upstream always sets `http: { body }` (an undefined body is a no-op)
            simple_chat(model_id, &mut s, "digitalocean", "digitalocean", "https://inference.do-ai.run/v1", &["DIGITALOCEAN_ACCESS_TOKEN", "DIGITALOCEAN_API_KEY", "DO_INFERENCE_API_KEY"], wrappers::digitalocean_protocol(), compat(json!({"supportsPromptCacheKey": true})))
        }
        "openrouter" => simple_chat(model_id, &mut s, "openrouter", "openrouter", "https://openrouter.ai/api/v1", &["OPENROUTER_API_KEY"], Arc::new(wrappers::OpenRouter), compat(json!({"supportsPromptCacheKey": true}))),
        "venice" => venice(model_id, &mut s),
        "zai" | "zai/chat" => {
            let api_key = s.string("apiKey");
            let base = s.string("baseURL");
            let mut d = Defaults::from(&mut s);
            d.provider_options = s.options();
            let route = d.apply(chat_route("zai-chat", "zai", "zai", Some("https://api.z.ai/api/paas/v4"), ocv::zai_protocol())).base_url(base).auth(bearer(api_key, &["ZAI_API_KEY"]));
            Ok(with_compat(model(route, model_id)?, ocv::zai_compatibility()))
        }
        "zai-coding-plan" | "zai-coding-plan/chat" | "zai-coding-plan/messages" | "zai-coding-plan/responses" => zai_coding(model_id, &mut s, name),
        "moonshot/chat" | "moonshot/messages" | "moonshot/responses" | "moonshot" => moonshot(model_id, &mut s, name),
        "minimax/chat" | "minimax/messages" | "minimax/responses" | "minimax" => minimax(model_id, &mut s, name),
        "meta/chat" | "meta/messages" | "meta/responses" | "meta" => meta(model_id, &mut s, name),
        "alibaba/chat" | "alibaba/messages" | "alibaba/responses" | "alibaba" => alibaba(model_id, &mut s, name),
        "vercel-ai-gateway" | "vercel-ai-gateway/chat" | "vercel-ai-gateway/messages" | "vercel-ai-gateway/responses" => vercel(model_id, &mut s, name),
        "cloudflare-ai-gateway" => cloudflare_gateway(model_id, &mut s),
        "cloudflare-workers-ai" => cloudflare_workers(model_id, &mut s),
        _ => Err(ProviderConfigurationError::new(package, format!("Provider package {package} is not installed"))),
    }
}

/// Generic OpenAI-Chat-family provider (`baseten`, `cerebras`, `deepseek`, ... ).
#[allow(clippy::too_many_arguments)]
fn simple_chat(
    model_id: &str,
    s: &mut Settings,
    route_id: &str,
    provider: &str,
    base: &str,
    env: &[&str],
    protocol: Arc<dyn Protocol>,
    compatibility: Compatibility,
) -> Result<LanguageModel> {
    let api_key = s.string("apiKey");
    let base_url = s.string("baseURL").unwrap_or_else(|| base.into());
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let route = d.apply(chat_route(route_id, provider, provider, Some(base), protocol)).base_url(Some(base_url)).auth(bearer(api_key, env));
    let m = model(route, model_id)?;
    Ok(if compatibility == Compatibility::default() { m } else { with_compat(m, compatibility) })
}

fn anthropic_auth(provider: &str, api_key: Option<String>, auth_token: Option<String>, env: &[&str], label: &str) -> Result<Arc<dyn Auth>> {
    if api_key.is_some() && auth_token.is_some() {
        return Err(ProviderConfigurationError::new(provider, format!("{label} apiKey cannot be combined with authToken")));
    }
    if let Some(token) = auth_token {
        return Ok(auth::bearer(Credential::value(token)));
    }
    let credential = env.iter().fold(Credential::optional(api_key, "apiKey"), |acc, e| acc.or_else(Credential::env(e)));
    Ok(auth::header("x-api-key", credential))
}

fn anthropic(model_id: &str, s: &mut Settings) -> Result<LanguageModel> {
    let api_key = s.string("apiKey");
    let auth_token = s.string("authToken");
    let base = s.string("baseURL");
    let auth = anthropic_auth("anthropic", api_key, auth_token, &["ANTHROPIC_API_KEY"], "Anthropic")?;
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let route = d.apply(am::route().provider("anthropic")).base_url(Some(base.unwrap_or_else(|| am::DEFAULT_BASE_URL.into()))).auth(auth);
    model(route, model_id)
}

fn anthropic_compatible(model_id: &str, s: &mut Settings) -> Result<LanguageModel> {
    let api_key = s.string("apiKey");
    let auth_token = s.string("authToken");
    let base = s.string("baseURL");
    let provider = s.string("provider").unwrap_or_else(|| "anthropic-compatible".into());
    let auth = anthropic_auth(&provider, api_key, auth_token, &[], "Anthropic-compatible")?;
    let Some(base) = base.filter(|b| !b.is_empty()) else {
        return Err(ProviderConfigurationError::new(&provider, "Anthropic-compatible providers require a baseURL"));
    };
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let base_route = if provider == "anthropic" { am::route() } else { am::route().id("anthropic-compatible-messages").provider("anthropic-compatible") };
    let route = d.apply(base_route.provider(&provider)).base_url(Some(base)).auth(auth);
    let mut m = model(route, model_id)?;
    m.provider = provider;
    Ok(m)
}

fn openai(model_id: &str, s: &mut Settings, chat: bool) -> Result<LanguageModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL");
    let organization = s.string("organization");
    let project = s.string("project");
    let query = s.take("queryParams").and_then(|q| q.as_object().map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect::<IndexMap<_, _>>()));
    let mut d = Defaults::from(s);
    let mut headers = IndexMap::new();
    if let Some(o) = organization {
        headers.insert("OpenAI-Organization".to_string(), o);
    }
    if let Some(p) = project {
        headers.insert("OpenAI-Project".to_string(), p);
    }
    for (k, v) in d.headers.take().unwrap_or_default() {
        headers.insert(k, v);
    }
    d.headers = if headers.is_empty() { None } else { Some(headers) };
    d.provider_options = std::mem::take(&mut s.rest);
    let base_route = if chat { oc::route() } else { oai::route() };
    let mut route = base_route.base_url(base).auth(bearer(api_key, &["OPENAI_API_KEY"]));
    if let Some(q) = query {
        route = route.query(q);
    }
    let route = d.apply_openai(route, model_id);
    let m = model(route, model_id)?;
    Ok(if chat { with_compat(m, compat(json!({"supportsPromptCacheKey": true}))) } else { m })
}

fn openai_compatible(model_id: &str, s: &mut Settings, responses: bool) -> Result<LanguageModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL");
    let provider = s.string("provider").unwrap_or_else(|| "openai-compatible".into());
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let base_route = if responses { orp::compatible_route() } else { oc::compatible_route() };
    let route = d.apply(base_route.provider(&provider)).base_url(base).auth(bearer(api_key, &[]));
    model(route, model_id)
}

fn azure(model_id: &str, s: &mut Settings, chat: bool) -> Result<LanguageModel> {
    let api_key = s.string("apiKey");
    let api_version = s.string("apiVersion");
    let base = s.string("baseURL");
    let resource = s.string("resourceName");
    let deployment_urls = s.take("useDeploymentBasedUrls").and_then(|v| v.as_bool()).unwrap_or(false);
    let query_params: Option<IndexMap<String, String>> =
        s.take("queryParams").and_then(|q| q.as_object().map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect()));
    if base.is_none() && resource.is_none() {
        return Err(ProviderConfigurationError::new("azure", "Azure requires resourceName or baseURL"));
    }
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let root = base.clone().unwrap_or_else(|| format!("https://{}.openai.azure.com/openai", resource.clone().unwrap_or_default().trim())).trim_end_matches('/').to_string();
    let mut query = IndexMap::new();
    query.insert("api-version".to_string(), api_version.unwrap_or_else(|| "v1".into()));
    for (k, v) in query_params.clone().unwrap_or_default() {
        query.insert(k, v);
    }
    let custom_host = base.as_ref().and_then(|b| url::Url::parse(b).ok()).map(|u| !u.host_str().unwrap_or("").ends_with(".openai.azure.com")).unwrap_or(false);
    let (endpoint_base, endpoint_query) = if deployment_urls {
        (format!("{root}/deployments/{model_id}"), query)
    } else if base.is_some() && custom_host {
        (root, query_params.unwrap_or_default())
    } else {
        (format!("{root}/v1"), query)
    };
    let credential = Credential::optional(api_key, "apiKey").or_else(Credential::env("AZURE_OPENAI_API_KEY"));
    let azure_auth = auth::and_then(auth::remove("authorization"), auth::header("api-key", credential));
    let base_route = if chat {
        oc::route().id("azure-openai-chat").provider("azure")
    } else {
        oai::route().id("azure-openai-responses").provider("azure").channel(Some(azure_channel()))
    };
    let route = base_route.base_url(Some(endpoint_base)).query(endpoint_query).auth(azure_auth);
    let route = d.apply_openai(route, model_id);
    let m = model(route, model_id)?;
    Ok(if chat { with_compat(m, compat(json!({"supportsPromptCacheKey": true}))) } else { m })
}

/// Azure's Responses WebSocket (`providers/azure.ts` `responsesRoute.transport`): only the v1 API of an
/// `*.openai.azure.com` resource offers it, at the same path without `api-version`.
fn azure_channel() -> crate::protocols::open_responses_channel::ChannelOptions {
    crate::protocols::open_responses_channel::ChannelOptions::new("azure-openai-responses", "Azure OpenAI Responses", Some(55 * 60 * 1000))
        .enabled(|value| {
            let Ok(url) = url::Url::parse(value) else { return false };
            url.scheme() == "https"
                && url.host_str().is_some_and(|h| h.ends_with(".openai.azure.com"))
                && url.path().ends_with("/openai/v1/responses")
                && url.query_pairs().find(|(k, _)| k == "api-version").map(|(_, v)| v.into_owned()).as_deref() == Some("v1")
        })
        .url(|value| {
            let Ok(mut url) = url::Url::parse(value) else { return value.to_string() };
            let pairs: Vec<(String, String)> = url.query_pairs().filter(|(k, _)| k != "api-version").map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
            if pairs.is_empty() {
                url.set_query(None);
            } else {
                let encoded: String = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs.iter()).finish();
                url.set_query(Some(&encoded));
            }
            url.to_string()
        })
}

fn bedrock_credentials(value: Option<Value>) -> Option<bc::AwsCredentials> {
    let v = value?;
    Some(bc::AwsCredentials {
        region: v.get("region").and_then(Value::as_str).unwrap_or("").to_string(),
        access_key_id: v.get("accessKeyId").and_then(Value::as_str)?.to_string(),
        secret_access_key: v.get("secretAccessKey").and_then(Value::as_str)?.to_string(),
        session_token: v.get("sessionToken").and_then(Value::as_str).map(String::from),
    })
}

fn amazon_bedrock(model_id: &str, s: &mut Settings) -> Result<LanguageModel> {
    let api_key = s.string("apiKey");
    let mode = s.string("auth");
    let base = s.string("baseURL");
    let credentials_value = s.take("credentials");
    let credentials = bedrock_credentials(credentials_value.clone());
    let profile = s.string("profile");
    let region = s.string("region");
    let top_p = s.take("topP").and_then(|v| v.as_f64());
    let thinking = s.take("thinking");
    let headers = s.headers();
    let body = s.body();
    if mode.as_deref() == Some("bearer") && api_key.is_none() && std::env::var("AWS_BEARER_TOKEN_BEDROCK").is_err() {
        return Err(ProviderConfigurationError::new("amazon-bedrock", "Amazon Bedrock bearer auth requires apiKey"));
    }
    if mode.as_deref() == Some("sigv4") && api_key.is_some() {
        return Err(ProviderConfigurationError::new("amazon-bedrock", "Amazon Bedrock SigV4 auth does not accept apiKey"));
    }
    let cred_region = credentials_value.as_ref().and_then(|c| c.get("region")).and_then(Value::as_str).map(String::from);
    let resolved_region = bc::resolve_region(region.as_deref(), cred_region.as_deref());
    let auth = bc::bedrock_auth(bc::BedrockAuthOptions { api_key, credentials, profile, mode, service: None, name: None }, &resolved_region);
    let mut po = Object::new();
    if let Some(t) = thinking {
        po.insert("thinking".into(), t);
    }
    let d = Defaults {
        headers,
        body,
        provider_options: po,
        generation: top_p.map(|p| GenerationOptions { top_p: Some(p), ..Default::default() }),
    };
    let route = d
        .apply(bc::route().provider("amazon-bedrock").metadata_key("bedrock"))
        .base_url(Some(base.unwrap_or_else(|| bc::bedrock_base_url(&resolved_region))))
        .auth(auth);
    model(route, model_id)
}

fn mantle(model_id: &str, s: &mut Settings, kind: &str) -> Result<LanguageModel> {
    let api_key = s.string("apiKey");
    let mode = s.string("auth");
    let base = s.string("baseURL");
    let credentials_value = s.take("credentials");
    let credentials = bedrock_credentials(credentials_value.clone());
    let profile = s.string("profile");
    let region = s.string("region");
    let top_p = s.take("topP").and_then(|v| v.as_f64());
    let headers = s.headers();
    let body = s.body();
    if mode.as_deref() == Some("bearer") && api_key.is_none() && std::env::var("AWS_BEARER_TOKEN_BEDROCK").is_err() {
        return Err(ProviderConfigurationError::new("amazon-bedrock", "Amazon Bedrock Mantle bearer auth requires apiKey"));
    }
    if mode.as_deref() == Some("sigv4") && api_key.is_some() {
        return Err(ProviderConfigurationError::new("amazon-bedrock", "Amazon Bedrock Mantle SigV4 auth does not accept apiKey"));
    }
    let cred_region = credentials_value.as_ref().and_then(|c| c.get("region")).and_then(Value::as_str).map(String::from);
    let resolved_region = bc::resolve_region(region.as_deref(), cred_region.as_deref());
    let auth = bc::bedrock_auth(
        bc::BedrockAuthOptions { api_key, credentials, profile, mode, service: Some("bedrock-mantle".into()), name: Some("Bedrock Mantle".into()) },
        &resolved_region,
    );
    let d = Defaults { headers, body, provider_options: std::mem::take(&mut s.rest), generation: top_p.map(|p| GenerationOptions { top_p: Some(p), ..Default::default() }) };
    let v1 = format!("https://bedrock-mantle.{resolved_region}.api.aws/v1");
    match kind {
        "chat" => {
            let route = oc::route().id("bedrock-mantle-chat").provider("amazon-bedrock").metadata_key("mantle").base_url(Some(base.unwrap_or(v1))).auth(auth);
            model(d.apply_openai(route, model_id), model_id)
        }
        "messages" => {
            let mut route = am::route().id("bedrock-mantle-messages").provider("amazon-bedrock").metadata_key("mantle");
            route.protocol = Arc::new(wrappers::MantleMessages);
            route.endpoint.path = EndpointPath::Static(am::PATH.into());
            let route = route.base_url(Some(base.unwrap_or_else(|| format!("https://bedrock-mantle.{resolved_region}.api.aws/anthropic/v1")))).auth(auth);
            model(d.apply(route), model_id)
        }
        _ => {
            let route = responses_route("bedrock-mantle-responses", "amazon-bedrock", "mantle", None, orp::protocol())
                .with_provider_options(orp::default_options())
                .base_url(Some(base.unwrap_or(v1)))
                .auth(auth);
            model(d.apply_openai(route, model_id), model_id)
        }
    }
}

fn google(model_id: &str, s: &mut Settings, interactions: bool) -> Result<LanguageModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL");
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let credential = Credential::optional(api_key, "apiKey").or_else(Credential::env("GOOGLE_GENERATIVE_AI_API_KEY"));
    let base_route = if interactions { gi::route() } else { gemini::route() };
    let route = d.apply(base_route).base_url(base).auth(auth::header("x-goog-api-key", credential));
    model(route, model_id)
}

struct VertexInput {
    access_token: Option<String>,
    api_key: Option<String>,
    base: Option<String>,
    location: Option<String>,
    project: Option<String>,
}

fn vertex_input(s: &mut Settings) -> VertexInput {
    VertexInput {
        access_token: s.string("accessToken"),
        api_key: s.string("apiKey"),
        base: s.string("baseURL"),
        location: s.string("location"),
        project: s.string("project"),
    }
}

fn vertex_gemini(model_id: &str, s: &mut Settings) -> Result<LanguageModel> {
    let v = vertex_input(s);
    if v.api_key.is_some() && v.access_token.is_some() {
        return Err(ProviderConfigurationError::new("google-vertex", "Google Vertex apiKey cannot be combined with accessToken or auth"));
    }
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let api_key = gemini::vertex_api_key(v.api_key.as_deref(), v.access_token.as_deref(), false, v.project.as_deref(), v.location.as_deref())?;
    if api_key.is_some() && model_id.starts_with("endpoints/") {
        return Err(ProviderConfigurationError::new("google-vertex", "Google Vertex tuned models do not support Express Mode API keys"));
    }
    let location = gemini::vertex_location(v.location.as_deref());
    let project = gemini::vertex_project(v.project.as_deref());
    let base = gemini::vertex_gemini_base_url(v.base.as_deref(), api_key.is_some(), &location, project.clone(), model_id)?;
    let auth = match api_key {
        Some(k) => auth::header("x-goog-api-key", Credential::value(k)),
        None => gemini::vertex_oauth(v.access_token.as_deref(), project),
    };
    model(d.apply(gemini::vertex_route()).base_url(Some(base)).auth(auth), model_id)
}

fn vertex_interactions(model_id: &str, s: &mut Settings) -> Result<LanguageModel> {
    let v = vertex_input(s);
    if v.api_key.is_some() && v.access_token.is_some() {
        return Err(ProviderConfigurationError::new("google-vertex", "Google Vertex apiKey cannot be combined with accessToken or auth"));
    }
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let api_key = gemini::vertex_api_key(v.api_key.as_deref(), v.access_token.as_deref(), false, v.project.as_deref(), v.location.as_deref())?;
    let location = gemini::vertex_location(v.location.as_deref());
    let project = gemini::vertex_project(v.project.as_deref());
    let base = match v.base {
        Some(b) => b,
        None => gi::vertex_base_url(&location, project.clone(), api_key.is_some())?,
    };
    let auth = match api_key {
        Some(k) => auth::header("x-goog-api-key", Credential::value(k)),
        None => gemini::vertex_oauth(v.access_token.as_deref(), project),
    };
    model(d.apply(gi::vertex_route()).base_url(Some(base)).auth(auth), model_id)
}

fn vertex_messages(model_id: &str, s: &mut Settings) -> Result<LanguageModel> {
    let v = vertex_input(s);
    if v.api_key.is_some() {
        return Err(ProviderConfigurationError::new("google-vertex", "Google Vertex Messages does not support API keys"));
    }
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let location = gemini::vertex_location(v.location.as_deref());
    let project = gemini::vertex_project(v.project.as_deref());
    let base = match v.base {
        Some(b) => b,
        None => format!(
            "https://{}/v1/projects/{}/locations/{location}/publishers/anthropic/models",
            gemini::vertex_host(&location),
            gemini::require_project(project.clone())?
        ),
    };
    let mut route = am::route().id("google-vertex-messages").initial_provider("google-vertex", Some("vertex"));
    route.protocol = am::vertex_protocol();
    route.endpoint.path = EndpointPath::Dynamic(Arc::new(|request, _| format!("/{}:streamRawPredict", request.model.id)));
    let route = route.base_url(Some(base)).auth(gemini::vertex_oauth(v.access_token.as_deref(), project));
    model(d.apply(route), model_id)
}

fn vertex_openapi(model_id: &str, s: &mut Settings, responses: bool) -> Result<LanguageModel> {
    let v = vertex_input(s);
    if v.api_key.is_some() {
        let label = if responses { "Responses" } else { "Chat" };
        return Err(ProviderConfigurationError::new("google-vertex", format!("Google Vertex {label} does not support API keys")));
    }
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let location = gemini::vertex_location(v.location.as_deref());
    let project = gemini::vertex_project(v.project.as_deref());
    let base = match v.base {
        Some(b) => b,
        None => format!("https://aiplatform.googleapis.com/v1/projects/{}/locations/{location}/endpoints/openapi", gemini::require_project(project.clone())?),
    };
    let auth = gemini::vertex_oauth(v.access_token.as_deref(), project);
    let route = if responses {
        responses_route("google-vertex-responses", "google-vertex", "vertex", None, orp::protocol()).with_provider_options(orp::default_options())
    } else {
        chat_route("google-vertex-chat", "google-vertex", "vertex", None, oc::protocol())
    };
    model(d.apply(route).base_url(Some(base)).auth(auth), model_id)
}

fn vertex_mistral(model_id: &str, s: &mut Settings) -> Result<LanguageModel> {
    let v = vertex_input(s);
    if v.api_key.is_some() {
        return Err(ProviderConfigurationError::new("google-vertex", "Google Vertex Mistral does not support API keys"));
    }
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let location = gemini::vertex_location(v.location.as_deref());
    let project = gemini::vertex_project(v.project.as_deref());
    let base = match v.base {
        Some(b) => b,
        None => format!(
            "https://{}/v1/projects/{}/locations/{location}/publishers/mistralai/models",
            gemini::vertex_host(&location),
            gemini::require_project(project.clone())?
        ),
    };
    let route = Route::new(
        "google-vertex-mistral",
        mistral_chat::vertex_protocol(),
        Endpoint::dynamic(None, |request, _| format!("/{}:streamRawPredict", request.model.id)),
        Framing::SseWithDone,
    )
    .initial_provider("google-vertex", Some("vertex"));
    let route = route.base_url(Some(base)).auth(gemini::vertex_oauth(v.access_token.as_deref(), project));
    model(d.apply(route), model_id)
}

fn cohere(model_id: &str, s: &mut Settings, chat: bool) -> Result<LanguageModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL");
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let auth = bearer(api_key, &["COHERE_API_KEY"]);
    if chat {
        let route = chat_route("cohere-chat-completions", "cohere", "cohere", Some("https://api.cohere.ai/compatibility/v1"), oc::protocol());
        let route = d.apply(route).base_url(Some(base.unwrap_or_else(|| "https://api.cohere.ai/compatibility/v1".into()))).auth(auth);
        return Ok(with_compat(
            model(route, model_id)?,
            compat(json!({"maxTokensField": "max_tokens", "reasoningField": "reasoning_content", "supportsStrictMode": false})),
        ));
    }
    let route = d.apply(cohere_chat::route()).base_url(Some(base.unwrap_or_else(|| cohere_chat::DEFAULT_BASE_URL.into()))).auth(auth);
    model(route, model_id)
}

fn xai(model_id: &str, s: &mut Settings) -> Result<LanguageModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL").unwrap_or_else(|| "https://api.x.ai/v1".into());
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let channel = crate::protocols::open_responses_channel::ChannelOptions::new("xai-responses", "xAI Responses", Some(24 * 60 * 1000))
        .continuation(crate::protocols::open_responses_channel::xai_continuation);
    let route = responses_route("xai-responses", "xai", "xai", Some("https://api.x.ai/v1"), rv::xai_protocol())
        .with_provider_options(orp::default_options())
        .channel(Some(channel));
    model(d.apply(route).base_url(Some(base)).auth(bearer(api_key, &["XAI_API_KEY"])), model_id)
}

/// `xai-chat` route (`x-grok-conv-id` session header).
pub fn xai_chat_route() -> Route {
    chat_route("xai-chat", "xai", "xai", Some("https://api.x.ai/v1"), oc::protocol()).headers_fn(|request| {
        let mut h = Headers::new();
        if let Some(k) = request.prompt_cache_key.as_ref().filter(|k| !k.is_empty()) {
            h.insert("x-grok-conv-id".into(), k.clone());
        }
        h
    })
}

fn venice(model_id: &str, s: &mut Settings) -> Result<LanguageModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL");
    let query: Option<IndexMap<String, String>> =
        s.take("queryParams").and_then(|q| q.as_object().map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect()));
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let mut route = d
        .apply(chat_route("venice-chat", "venice", "venice", Some("https://api.venice.ai/api/v1"), ocv::venice_protocol()))
        .base_url(base)
        .auth(bearer(api_key, &["VENICE_API_KEY"]));
    if let Some(q) = query {
        route = route.query(q);
    }
    Ok(with_compat(model(route, model_id)?, ocv::venice_compatibility()))
}

fn zai_coding(model_id: &str, s: &mut Settings, name: &str) -> Result<LanguageModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL");
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let auth = bearer(api_key, &["ZAI_API_KEY"]);
    match name {
        "zai-coding-plan/messages" => {
            let route = Route::new("zai-coding-messages", am::zai_protocol(), Endpoint::new(Some("https://api.z.ai/api/anthropic/v1"), "/messages"), am::framing())
                .initial_provider("zai-coding-plan", Some("zai"))
                .headers_fn(anthropic_version());
            Ok(with_compat(model(d.apply(route).base_url(base).auth(auth), model_id)?, compat(json!({"requireSignature": false}))))
        }
        "zai-coding-plan/responses" => {
            let route = responses_route("zai-coding-responses", "zai-coding-plan", "zai", Some("https://api.z.ai/api/v1"), orp::protocol());
            model(d.apply(route).base_url(base).auth(auth), model_id)
        }
        _ => {
            let route = chat_route("zai-coding-chat", "zai-coding-plan", "zai", Some("https://api.z.ai/api/coding/paas/v4"), ocv::zai_protocol());
            Ok(with_compat(model(d.apply(route).base_url(base).auth(auth), model_id)?, ocv::zai_compatibility()))
        }
    }
}

fn moonshot(model_id: &str, s: &mut Settings, name: &str) -> Result<LanguageModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL");
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let auth = bearer(api_key, &["MOONSHOT_API_KEY", "MOONSHOTAI_API_KEY"]);
    match name {
        "moonshot/messages" => {
            let route = Route::new("moonshot-messages", am::protocol(), Endpoint::new(Some("https://api.moonshot.ai/anthropic/v1"), "/messages"), am::framing())
                .initial_provider("moonshotai", Some("moonshot"));
            Ok(with_compat(model(d.apply(route).base_url(base).auth(auth), model_id)?, compat(json!({"requireSignature": false, "sanitizer": "moonshot"}))))
        }
        "moonshot/responses" => {
            let route = responses_route("moonshot-responses", "moonshotai", "moonshot", Some("https://api.moonshot.ai/v1"), orp::protocol());
            Ok(with_compat(model(d.apply(route).base_url(base).auth(auth), model_id)?, compat(json!({"sanitizer": "moonshot"}))))
        }
        _ => {
            let route = chat_route("moonshot-chat", "moonshotai", "moonshot", Some("https://api.moonshot.ai/v1"), Arc::new(wrappers::MoonshotChat));
            Ok(with_compat(
                model(d.apply(route).base_url(base).auth(auth), model_id)?,
                compat(json!({"maxTokensField": "max_tokens", "supportsStrictMode": false, "supportsPromptCacheKey": true, "sanitizer": "moonshot", "reasoningField": "reasoning_content"})),
            ))
        }
    }
}

fn minimax(model_id: &str, s: &mut Settings, name: &str) -> Result<LanguageModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL");
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let auth = bearer(api_key, &["MINIMAX_API_KEY"]);
    match name {
        "minimax/chat" => {
            let route = chat_route("minimax-chat", "minimax", "minimax", Some("https://api.minimax.io/v1"), Arc::new(wrappers::MinimaxChat));
            Ok(with_compat(model(d.apply(route).base_url(base).auth(auth), model_id)?, compat(json!({"supportsStrictMode": false}))))
        }
        "minimax/responses" => {
            let route = responses_route("minimax-responses", "minimax", "minimax", Some("https://api.minimax.io/v1"), orp::protocol());
            model(d.apply(route).base_url(base).auth(auth), model_id)
        }
        _ => {
            let route = Route::new("minimax-messages", am::protocol(), Endpoint::new(Some("https://api.minimax.io/anthropic/v1"), "/messages"), am::framing())
                .initial_provider("minimax", Some("minimax"))
                .headers_fn(anthropic_version());
            model(d.apply(route).base_url(base).auth(auth), model_id)
        }
    }
}

fn meta(model_id: &str, s: &mut Settings, name: &str) -> Result<LanguageModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL").unwrap_or_else(|| "https://api.meta.ai/v1".into());
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let auth = bearer(api_key, &["META_API_KEY"]);
    match name {
        "meta/chat" => {
            let route = chat_route("meta-chat", "meta", "meta", None, oc::protocol());
            Ok(with_compat(
                model(d.apply(route).base_url(Some(base)).auth(auth), model_id)?,
                compat(json!({"maxTokensField": "max_completion_tokens", "supportsPromptCacheKey": true})),
            ))
        }
        "meta/messages" => {
            let mut po = Object::new();
            po.insert("thinking".into(), json!({"type": "adaptive", "display": "omitted"}));
            let route = Route::new("meta-messages", am::meta_protocol(), Endpoint::new(None, "/messages"), am::framing())
                .initial_provider("meta", Some("meta"))
                .with_provider_options(po);
            Ok(with_compat(model(d.apply(route).base_url(Some(base)).auth(auth), model_id)?, compat(json!({"requireSignature": false}))))
        }
        _ => {
            let route = responses_route("meta-responses", "meta", "meta", None, rv::meta_protocol()).with_provider_options(orp::default_options());
            model(d.apply(route).base_url(Some(base)).auth(auth), model_id)
        }
    }
}

fn alibaba(model_id: &str, s: &mut Settings, name: &str) -> Result<LanguageModel> {
    // upstream drops `body` for Alibaba settings
    let _ = s.take("body");
    let api_key = s.string("apiKey");
    let base = s.string("baseURL");
    let region = s.string("region");
    let workspace = s.string("workspaceID");
    let headers = s.headers();
    let host = match (&region, &workspace) {
        (None, _) => None,
        (Some(r), None) => match r.as_str() {
            "ap-southeast-1" => Some("dashscope-intl.aliyuncs.com".to_string()),
            "cn-beijing" => Some("dashscope.aliyuncs.com".to_string()),
            "cn-hongkong" => Some("cn-hongkong.dashscope.aliyuncs.com".to_string()),
            "us-east-1" => Some("dashscope-us.aliyuncs.com".to_string()),
            _ => None,
        },
        (Some(r), Some(w)) => Some(format!("{w}.{r}.maas.aliyuncs.com")),
    };
    if base.is_none() {
        let Some(r) = &region else {
            return Err(ProviderConfigurationError::new("alibaba", "Alibaba requires region or baseURL"));
        };
        if host.is_none() {
            return Err(ProviderConfigurationError::new("alibaba", format!("Alibaba region {r} requires workspaceID or baseURL")));
        }
    }
    let d = Defaults { headers, body: None, provider_options: std::mem::take(&mut s.rest), generation: None };
    let auth = bearer(api_key, &["DASHSCOPE_API_KEY", "ALIBABA_API_KEY"]);
    let host = host.unwrap_or_default();
    match name {
        "alibaba/messages" => {
            let route = Route::new("alibaba-messages", am::alibaba_protocol(), Endpoint::new(None, "/messages"), Framing::Sse)
                .initial_provider("alibaba", Some("alibaba"))
                .headers_fn(anthropic_version());
            let route = d.apply(route).base_url(Some(base.unwrap_or_else(|| format!("https://{host}/apps/anthropic/v1")))).auth(auth);
            Ok(with_compat(model(route, model_id)?, compat(json!({"requireSignature": false}))))
        }
        "alibaba/responses" => {
            let route = Route::new("alibaba-responses", rv::alibaba_protocol(), Endpoint::new(None, "/responses"), Framing::Sse).initial_provider("alibaba", Some("alibaba"));
            model(d.apply(route).base_url(Some(base.unwrap_or_else(|| format!("https://{host}/compatible-mode/v1")))).auth(auth), model_id)
        }
        _ => {
            let route = Route::new("alibaba-chat", ocv::alibaba_protocol(), Endpoint::new(None, "/chat/completions"), Framing::Sse).initial_provider("alibaba", Some("alibaba"));
            let route = d.apply(route).base_url(Some(base.unwrap_or_else(|| format!("https://{host}/compatible-mode/v1")))).auth(auth);
            Ok(with_compat(model(route, model_id)?, ocv::alibaba_compatibility()))
        }
    }
}

fn vercel(model_id: &str, s: &mut Settings, name: &str) -> Result<LanguageModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL").unwrap_or_else(|| "https://ai-gateway.vercel.sh/v1".into());
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let auth = bearer(api_key, &["AI_GATEWAY_API_KEY", "VERCEL_OIDC_TOKEN"]);
    let kind = match name {
        "vercel-ai-gateway/messages" => "messages",
        "vercel-ai-gateway/responses" => "responses",
        "vercel-ai-gateway/chat" => "chat",
        _ => {
            if model_id.starts_with("openai/gpt-") || model_id.starts_with("spacexai/grok-") {
                "responses"
            } else if model_id.starts_with("meta/muse-") {
                "chat"
            } else {
                "messages"
            }
        }
    };
    let affinity = |route: Route| {
        route.headers_fn(|request| {
            let mut h = Headers::new();
            if let Some(k) = request.prompt_cache_key.as_ref().filter(|k| !k.is_empty()) {
                h.insert("x-session-affinity".into(), k.clone());
            }
            h
        })
    };
    let (route, compatibility) = match kind {
        "messages" => {
            let protocol: Arc<dyn Protocol> = Arc::new(Gateway { id: "vercel-ai-gateway-messages", inner: am::protocol(), api: GatewayApi::Messages });
            let mut headers = IndexMap::new();
            headers.insert("anthropic-version".to_string(), "2023-06-01".to_string());
            let route = Route::new("vercel-ai-gateway-messages", protocol, Endpoint::new(None, "/messages"), am::framing())
                .initial_provider("vercel-ai-gateway", Some("vercel-ai-gateway"))
                .with_headers(headers);
            (route, compat(json!({"requireSignature": model_id.starts_with("anthropic/")})))
        }
        "responses" => {
            let protocol: Arc<dyn Protocol> = Arc::new(Gateway { id: "vercel-ai-gateway-responses", inner: orp::protocol(), api: GatewayApi::Responses });
            let route = Route::new("vercel-ai-gateway-responses", protocol, Endpoint::new(None, orp::PATH), Framing::Sse)
                .initial_provider("vercel-ai-gateway", Some("vercel-ai-gateway"))
                .with_provider_options(orp::default_options());
            (route, Compatibility::default())
        }
        _ => {
            let protocol: Arc<dyn Protocol> = Arc::new(Gateway { id: "vercel-ai-gateway-chat", inner: oc::protocol(), api: GatewayApi::Chat });
            let route = Route::new("vercel-ai-gateway-chat", protocol, Endpoint::new(None, "/chat/completions"), Framing::SseWithDone)
                .initial_provider("vercel-ai-gateway", Some("vercel-ai-gateway"));
            (route, compat(json!({"reasoningField": "reasoning"})))
        }
    };
    let route = d.apply(affinity(route)).base_url(Some(base)).auth(auth);
    let m = model(route, model_id)?;
    Ok(if compatibility == Compatibility::default() { m } else { with_compat(m, compatibility) })
}

fn cloudflare_gateway(model_id: &str, s: &mut Settings) -> Result<LanguageModel> {
    let account = s.string("accountId");
    let api_key = s.string("apiKey");
    let base = s.string("baseURL");
    let cache_key = s.string("cacheKey");
    let cache_ttl = s.take("cacheTtl");
    let collect_log = s.take("collectLog");
    let gateway_key = s.string("gatewayApiKey");
    let gateway_id = s.string("gatewayId");
    let metadata = s.take("metadata");
    let skip_cache = s.take("skipCache");
    let user_headers = s.headers();
    let body = s.body();
    let po = std::mem::take(&mut s.rest);
    let provider = "cloudflare-ai-gateway";
    let account_id = || -> Result<String> {
        account.clone().filter(|a| !a.is_empty()).ok_or_else(|| ProviderConfigurationError::new(provider, "CloudflareAIGateway.configure requires accountId unless baseURL is supplied"))
    };
    let gid = gateway_id.map(|g| g.trim().to_string()).filter(|g| !g.is_empty()).unwrap_or_else(|| "default".into());
    let mut cf_headers = IndexMap::new();
    if let Some(m) = metadata {
        cf_headers.insert("cf-aig-metadata".to_string(), crate::json::encode(&m));
    }
    if let Some(t) = cache_ttl {
        cf_headers.insert("cf-aig-cache-ttl".to_string(), crate::json::js_string(&t));
    }
    if let Some(k) = cache_key {
        cf_headers.insert("cf-aig-cache-key".to_string(), k);
    }
    if let Some(v) = skip_cache {
        cf_headers.insert("cf-aig-skip-cache".to_string(), crate::json::js_string(&v));
    }
    if let Some(v) = collect_log {
        cf_headers.insert("cf-aig-collect-log".to_string(), crate::json::js_string(&v));
    }
    for (k, v) in user_headers.unwrap_or_default() {
        cf_headers.insert(k, v);
    }
    let credential = Credential::optional(gateway_key.or(api_key), "apiKey").or_else(Credential::env("CLOUDFLARE_API_TOKEN")).or_else(Credential::env("CF_AIG_TOKEN"));
    let native = base.is_none();
    let rest_base = match &base {
        Some(b) => b.clone(),
        None => format!("https://api.cloudflare.com/client/v4/accounts/{}/ai/v1", bc::sigv4::encode_uri_component(&account_id()?)),
    };
    let mut rest_headers = IndexMap::new();
    rest_headers.insert("cf-aig-gateway-id".to_string(), gid.clone());
    for (k, v) in &cf_headers {
        rest_headers.insert(k.clone(), v.clone());
    }
    let rest_defaults = |route: Route, headers: IndexMap<String, String>| {
        route.defaults(RouteDefaults {
            headers: Some(headers),
            generation: None,
            provider_options: Some(po.clone()),
            http: body.clone().map(|b| HttpOptions { body: Some(b), ..Default::default() }),
        })
    };
    let passthrough = |route: Route, kind: &str| -> Result<Route> {
        if native {
            let url = format!(
                "https://gateway.ai.cloudflare.com/v1/{}/{}/{kind}",
                bc::sigv4::encode_uri_component(&account_id()?),
                bc::sigv4::encode_uri_component(&gid)
            );
            Ok(rest_defaults(route, cf_headers.clone()).base_url(Some(url)).auth(auth::bearer_header("cf-aig-authorization", credential.clone())))
        } else {
            Ok(rest_defaults(route, rest_headers.clone()).base_url(Some(rest_base.clone())).auth(auth::bearer(credential.clone())))
        }
    };
    if let Some(rest) = model_id.strip_prefix("openai/") {
        let route = passthrough(oai::route().id("cloudflare-ai-gateway-responses").provider(provider), "openai")?;
        return model(route, if native { rest } else { model_id });
    }
    if let Some(rest) = model_id.strip_prefix("anthropic/") {
        let route = passthrough(am::route().id("cloudflare-ai-gateway-messages").provider(provider), "anthropic/v1")?;
        let id = if native { rest.replace('.', "-") } else { model_id.to_string() };
        return model(route, &id);
    }
    let route = rest_defaults(oc::route().id("cloudflare-ai-gateway-chat").provider(provider), rest_headers.clone()).base_url(Some(rest_base)).auth(auth::bearer(credential));
    let id = model_id.strip_prefix("workers-ai/").unwrap_or(model_id);
    model(route, id)
}

fn cloudflare_workers(model_id: &str, s: &mut Settings) -> Result<LanguageModel> {
    let account = s.string("accountId");
    let api_key = s.string("apiKey");
    let base = match s.string("baseURL") {
        Some(b) => b,
        None => match account.filter(|a| !a.is_empty()) {
            Some(a) => format!("https://api.cloudflare.com/client/v4/accounts/{}/ai/v1", bc::sigv4::encode_uri_component(&a)),
            None => {
                return Err(ProviderConfigurationError::new(
                    "cloudflare-workers-ai",
                    "CloudflareWorkersAI.configure requires accountId unless baseURL is supplied",
                ));
            }
        },
    };
    let mut d = Defaults::from(s);
    d.provider_options = std::mem::take(&mut s.rest);
    let route = chat_route("cloudflare-workers-ai", "cloudflare-workers-ai", "cloudflare-workers-ai", None, oc::protocol());
    let route = d
        .apply(route)
        .base_url(Some(base))
        .auth(bearer(api_key, &["CLOUDFLARE_API_KEY", "CLOUDFLARE_WORKERS_AI_TOKEN", "CLOUDFLARE_API_TOKEN"]));
    model(route, model_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(v: Value) -> Object {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn builds_known_packages() {
        let m = language_model("@opencode/ai/providers/anthropic", "claude-sonnet-4-5", &settings(json!({"apiKey": "k", "thinking": {"type": "adaptive"}}))).unwrap();
        assert_eq!(m.route.id, "anthropic-messages");
        assert_eq!(m.provider, "anthropic");
        assert_eq!(m.route.defaults.provider_options.as_ref().unwrap()["thinking"], json!({"type": "adaptive"}));
        let m = language_model("@opencode/ai/providers/openai", "gpt-5", &settings(json!({"apiKey": "k"}))).unwrap();
        assert_eq!(m.route.id, "openai-responses");
        let po = m.route.defaults.provider_options.clone().unwrap();
        assert_eq!(po["reasoningEffort"], "medium");
        assert_eq!(po["store"], false);
        let m = language_model("@opencode/ai/providers/openai-compatible", "llama", &settings(json!({"baseURL": "http://localhost:1234/v1", "provider": "lmstudio"}))).unwrap();
        assert_eq!(m.route.provider_metadata_key.as_deref(), Some("lmstudio"));
        assert_eq!(m.provider, "lmstudio");
        let err = language_model("@opencode/ai/providers/azure", "gpt", &Object::new()).unwrap_err();
        assert_eq!(err.message, "Azure requires resourceName or baseURL");
        let err = language_model("@opencode/ai/providers/anthropic", "c", &settings(json!({"apiKey": "a", "authToken": "b"}))).unwrap_err();
        assert_eq!(err.message, "Anthropic apiKey cannot be combined with authToken");
        let m = language_model("@opencode/ai/providers/azure", "gpt-5", &settings(json!({"resourceName": "res"}))).unwrap();
        assert_eq!(m.route.endpoint.base_url.as_deref(), Some("https://res.openai.azure.com/openai/v1"));
        assert_eq!(m.route.endpoint.query["api-version"], "v1");
        let m = language_model("@opencode/ai/providers/amazon-bedrock", "anthropic.claude", &settings(json!({"region": "eu-west-1", "apiKey": "x"}))).unwrap();
        assert_eq!(m.route.endpoint.base_url.as_deref(), Some("https://bedrock-runtime.eu-west-1.amazonaws.com"));
        assert_eq!(m.provider, "amazon-bedrock");
        assert_eq!(m.route.provider_metadata_key.as_deref(), Some("bedrock"));
        for package in crate::catalog::types::BUILTIN_PACKAGES {
            let s = settings(json!({"apiKey": "k", "baseURL": "https://example.test/v1", "region": "us-east-1", "resourceName": "r", "accountId": "a", "project": "p"}));
            let result = language_model(package, "model-x", &s);
            if package.contains("google-vertex") && package != &"@opencode/ai/providers/google-vertex" && !package.ends_with("gemini") && !package.ends_with("interactions") {
                continue; // API keys rejected
            }
            assert!(result.is_ok(), "{package}: {:?}", result.err());
        }
    }
}
