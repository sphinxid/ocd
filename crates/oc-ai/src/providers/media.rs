//! Image and decision entrypoints of the native provider packages (`providers/<P>/images.ts` →
//! `imageModel(modelID, settings)`, `providers/<P>/decisions.ts` → `decisionModel(modelID, settings)`).

use super::{Settings, PREFIX};
use crate::decision::{openai as openai_decisions, system_one, DecisionModel};
use crate::error::ProviderConfigurationError;
use crate::image::{self, ImageModel, ImageRequest};
use crate::json::Object;
use crate::protocols::bedrock_converse as bc;
use crate::protocols::gemini;
use crate::route::auth::{self, Credential};
use crate::route::media::Deployment;
use crate::types::HttpOptions;
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};

type Result<T> = std::result::Result<T, ProviderConfigurationError>;

/// The image entrypoints (`@opencode/ai/providers/<P>/images`).
pub fn is_image_package(package: &str) -> bool {
    package.strip_prefix(PREFIX).is_some_and(|n| n.ends_with("/images"))
}

/// The decision entrypoints (`@opencode/ai/providers/<P>/decisions`).
pub fn is_decision_package(package: &str) -> bool {
    package.strip_prefix(PREFIX).is_some_and(|n| n.ends_with("/decisions"))
}

fn options(s: Settings) -> Option<Object> {
    Some(s.options())
}

fn http_body(body: Option<Object>) -> Option<HttpOptions> {
    body.map(|b| HttpOptions { body: Some(b), ..Default::default() })
}

fn bearer(api_key: Option<String>, env: &[&str]) -> std::sync::Arc<dyn crate::route::Auth> {
    auth::bearer_options(api_key, env)
}

/// Build an image model for a native `.../images` entrypoint. Entrypoints that are not image packages fail
/// with `None`: the resolver reports them as unsupported for this type.
pub fn image_model(package: &str, model_id: &str, settings: &Object) -> Option<Result<ImageModel>> {
    let name = package.strip_prefix(PREFIX)?;
    let mut s = Settings::new(settings);
    Some(match name {
        "openai/images" => openai(model_id, &mut s),
        "azure/images" => azure(model_id, &mut s),
        "openai-compatible/images" => openai_compatible(model_id, &mut s),
        "openrouter/images" => openrouter(model_id, &mut s),
        "vercel-ai-gateway/images" => vercel(model_id, &mut s),
        "digitalocean/images" => digitalocean(model_id, &mut s),
        "google/images" => google(model_id, &mut s),
        "google-vertex/images" => vertex(model_id, &mut s),
        "xai/images" => simple(model_id, &mut s, &["XAI_API_KEY"], image::xai::model),
        "zai/images" => simple(model_id, &mut s, &["ZAI_API_KEY"], image::zai::model),
        "minimax/images" => simple(model_id, &mut s, &["MINIMAX_API_KEY"], image::minimax::model),
        "meta/images" => meta(model_id, &mut s),
        "alibaba/images" => alibaba(model_id, &mut s),
        "cloudflare-workers-ai/images" => cloudflare_workers(model_id, &mut s),
        _ => return None,
    })
}

/// `config({apiKey, baseURL, body, headers, ...providerOptions})` + `MediaRoute.deployment`.
fn standard(s: &mut Settings, env: &[&str], default_base: Option<&str>) -> Deployment {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL").or(default_base.map(String::from));
    let headers = s.headers();
    let body = s.body();
    Deployment::new(bearer(api_key, env), base, headers, body)
}

fn simple(
    model_id: &str,
    s: &mut Settings,
    env: &[&str],
    build: fn(&str, &Deployment, Option<Object>) -> ImageModel,
) -> Result<ImageModel> {
    let deployment = standard(s, env, None);
    let po = options(std::mem::replace(s, Settings::new(&Object::new())));
    Ok(build(model_id, &deployment, po))
}

fn take_rest(s: &mut Settings) -> Object {
    std::mem::replace(s, Settings::new(&Object::new())).options()
}

fn openai(model_id: &str, s: &mut Settings) -> Result<ImageModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL");
    let given = s.headers();
    let body = s.body();
    let organization = s.string("organization");
    let project = s.string("project");
    let query = query_params(s.take("queryParams"));
    let mut headers = IndexMap::new();
    if let Some(o) = organization {
        headers.insert("OpenAI-Organization".to_string(), o);
    }
    if let Some(p) = project {
        headers.insert("OpenAI-Project".to_string(), p);
    }
    for (k, v) in given.unwrap_or_default() {
        headers.insert(k, v);
    }
    let headers = if headers.is_empty() { None } else { Some(headers) };
    let mut deployment = Deployment::new(bearer(api_key, &["OPENAI_API_KEY"]), base, headers, body);
    deployment.http = HttpOptions::merge(&[deployment.http.as_ref(), query.map(|q| HttpOptions { query: Some(q), ..Default::default() }).as_ref()]);
    // The ChatGPT backend (ChatGPT sign-in) rejects multipart image edits with "Unsupported content type".
    let image_input = deployment.base_url.as_deref().filter(|b| b.starts_with("https://chatgpt.com/")).map(|_| image::openai::ImageInput::JsonEdits);
    Ok(image::openai::model(model_id, &deployment, None, Some(take_rest(s)), None, image_input))
}

fn query_params(value: Option<Value>) -> Option<IndexMap<String, String>> {
    value.and_then(|q| q.as_object().map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect()))
}

fn azure(model_id: &str, s: &mut Settings) -> Result<ImageModel> {
    let api_key = s.string("apiKey");
    let api_version = s.string("apiVersion");
    let base = s.string("baseURL");
    let body = s.body();
    let headers = s.headers();
    let query = query_params(s.take("queryParams"));
    let resource = s.string("resourceName");
    let deployment_urls = s.take("useDeploymentBasedUrls").is_some_and(|v| crate::json::truthy(&v));
    if base.is_none() && resource.is_none() {
        return Err(ProviderConfigurationError::new("azure", "Azure requires resourceName or baseURL"));
    }
    // `endpoint(input, modelID)`.
    let root = base.clone().unwrap_or_else(|| format!("https://{}.openai.azure.com/openai", resource.clone().unwrap_or_default().trim()));
    let root = root.trim_end_matches('/').to_string();
    let mut full_query = IndexMap::new();
    full_query.insert("api-version".to_string(), api_version.unwrap_or_else(|| "v1".into()));
    for (k, v) in query.clone().unwrap_or_default() {
        full_query.insert(k, v);
    }
    let (target_base, target_query) = if deployment_urls {
        (format!("{root}/deployments/{model_id}"), Some(full_query))
    } else if base.as_deref().is_some_and(|b| !url::Url::parse(b).ok().and_then(|u| u.host_str().map(|h| h.ends_with(".openai.azure.com"))).unwrap_or(false)) {
        (root, query)
    } else {
        (format!("{root}/v1"), Some(full_query))
    };
    let credential = Credential::optional(api_key, "apiKey").or_else(Credential::env("AZURE_OPENAI_API_KEY"));
    let auth = auth::and_then(auth::remove("authorization"), auth::header("api-key", credential));
    let mut deployment = Deployment::new(auth, Some(target_base), headers, body);
    deployment.http = HttpOptions::merge(&[deployment.http.as_ref(), target_query.map(|q| HttpOptions { query: Some(q), ..Default::default() }).as_ref()]);
    Ok(image::openai::model(model_id, &deployment, Some("azure"), Some(take_rest(s)), None, None))
}

static GPT_IMAGE: Lazy<Regex> = Lazy::new(|| Regex::new(r"gpt-image|chatgpt-image").unwrap());

fn openai_compatible(model_id: &str, s: &mut Settings) -> Result<ImageModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL");
    let body = s.body();
    let headers = s.headers();
    let provider = s.string("provider").unwrap_or_else(|| "openai-compatible".into());
    let deployment = Deployment::new(bearer(api_key, &[]), base, headers, body);
    // Many compatible hosts default to hosted URLs. GPT Image models always return bytes and reject the field; a
    // proxy serving one under another name can drop the default with `response_format: null`.
    let defaults = (!GPT_IMAGE.is_match(model_id)).then(|| json!({ "response_format": "b64_json" }).as_object().cloned().unwrap());
    let rest = take_rest(s);
    let po = crate::json::merge_records(&[defaults.as_ref(), Some(&rest)]);
    Ok(image::openai::model(model_id, &deployment, Some(&provider), po, None, None))
}

const IMAGE_RESOLUTIONS: &[&str] = &["512", "768", "1K", "1.5K", "2K", "4K"];

/// OpenRouter's sizing: an aspect ratio and a resolution tier; values OpenRouter doesn't list are left out.
fn openrouter_sizing(request: &ImageRequest) -> Option<Object> {
    let resolution = request.resolution.as_ref().map(|r| {
        let upper = r.to_uppercase();
        match upper.as_str() {
            "0.5K" => "512".to_string(),
            "0.75K" => "768".to_string(),
            _ => upper,
        }
    });
    let mut out = Object::new();
    if let Some(a) = &request.aspect {
        out.insert("aspect_ratio".into(), json!(a));
    }
    if let Some(r) = resolution.filter(|r| IMAGE_RESOLUTIONS.contains(&r.as_str())) {
        out.insert("resolution".into(), json!(r));
    }
    Some(out)
}

fn openrouter(model_id: &str, s: &mut Settings) -> Result<ImageModel> {
    let deployment = standard(s, &["OPENROUTER_API_KEY"], Some("https://openrouter.ai/api/v1"));
    let po = take_rest(s);
    Ok(image::openai::model(
        model_id,
        &deployment,
        Some("openrouter"),
        Some(po),
        Some(openrouter_sizing),
        Some(image::openai::ImageInput::References),
    ))
}

static GROK: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(spacexai|xai)/").unwrap());

/// The gateway passes `size` through to most image models, but Grok ignores it and renders 1024x1024; it only
/// reads sizing from `providerOptions.xai`.
fn vercel_sizing(request: &ImageRequest) -> Option<Object> {
    if !GROK.is_match(&request.model.id) {
        return image::openai::pixel_size(request);
    }
    if request.aspect.is_none() && request.resolution.is_none() {
        return None;
    }
    let mut xai = Object::new();
    if let Some(a) = &request.aspect {
        xai.insert("aspect_ratio".into(), json!(a));
    }
    if let Some(r) = image::xai::resolution(request) {
        xai.insert("resolution".into(), json!(r));
    }
    Some(json!({ "providerOptions": { "xai": xai } }).as_object().cloned().unwrap())
}

fn vercel(model_id: &str, s: &mut Settings) -> Result<ImageModel> {
    let deployment = standard(s, &["AI_GATEWAY_API_KEY", "VERCEL_OIDC_TOKEN"], Some("https://ai-gateway.vercel.sh/v1"));
    let po = take_rest(s);
    Ok(image::openai::model(model_id, &deployment, Some("vercel-ai-gateway"), Some(po), Some(vercel_sizing), None))
}

fn digitalocean(model_id: &str, s: &mut Settings) -> Result<ImageModel> {
    let deployment =
        standard(s, &["DIGITALOCEAN_ACCESS_TOKEN", "DIGITALOCEAN_API_KEY", "DO_INFERENCE_API_KEY"], Some("https://inference.do-ai.run/v1"));
    let po = take_rest(s);
    Ok(image::openai::model(model_id, &deployment, Some("digitalocean"), Some(po), None, None))
}

fn google(model_id: &str, s: &mut Settings) -> Result<ImageModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL");
    let headers = s.headers();
    let body = s.body();
    let credential = Credential::optional(api_key, "apiKey").or_else(Credential::env("GOOGLE_GENERATIVE_AI_API_KEY"));
    let deployment = Deployment::new(auth::header("x-goog-api-key", credential), base, headers, body);
    Ok(image::google::model(model_id, &deployment, Some(take_rest(s))))
}

fn vertex(model_id: &str, s: &mut Settings) -> Result<ImageModel> {
    let access_token = s.string("accessToken");
    let api_key = s.string("apiKey");
    let base = s.string("baseURL");
    let body = s.body();
    let headers = s.headers();
    let location = s.string("location");
    let project = s.string("project");
    if api_key.is_some() && access_token.is_some() {
        return Err(ProviderConfigurationError::new("google-vertex", "Google Vertex apiKey cannot be combined with accessToken or auth"));
    }
    let key = gemini::vertex_api_key(api_key.as_deref(), access_token.as_deref(), false, project.as_deref(), location.as_deref())?;
    if key.is_some() && model_id.starts_with("endpoints/") {
        return Err(ProviderConfigurationError::new("google-vertex", "Google Vertex tuned models do not support Express Mode API keys"));
    }
    let location = gemini::vertex_location(location.as_deref());
    let project = gemini::vertex_project(project.as_deref());
    let base = gemini::vertex_gemini_base_url(base.as_deref(), key.is_some(), &location, project.clone(), model_id)?;
    let auth = match key {
        Some(k) => auth::header("x-goog-api-key", Credential::value(k)),
        None => gemini::vertex_oauth(access_token.as_deref(), project),
    };
    let deployment = Deployment::new(auth, Some(base), headers, body);
    Ok(image::google::model(model_id, &deployment, Some(take_rest(s))))
}

fn meta(model_id: &str, s: &mut Settings) -> Result<ImageModel> {
    // Meta's image factory takes no provider options.
    let deployment = standard(s, &["META_API_KEY"], None);
    Ok(image::meta::model(model_id, &deployment, None))
}

fn alibaba(model_id: &str, s: &mut Settings) -> Result<ImageModel> {
    let body = s.body();
    let api_key = s.string("apiKey");
    let base = s.string("baseURL");
    let headers = s.headers();
    let region = s.string("region");
    let workspace = s.string("workspaceID");
    let host = match (&region, &workspace) {
        (None, _) => None,
        (Some(r), None) => super::alibaba_host(r).map(String::from),
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
    let base = base.unwrap_or_else(|| format!("https://{}/api/v1", host.unwrap_or_default()));
    let deployment = Deployment::new(bearer(api_key, &["DASHSCOPE_API_KEY", "ALIBABA_API_KEY"]), Some(base), headers, body);
    Ok(image::alibaba::model(model_id, &deployment, Some(take_rest(s))))
}

/// `CloudflareWorkersAI.baseURL` / `runBaseURL` (and the gateway's, which differs only in the provider id).
fn cloudflare_run_base(base: Option<String>, account: Option<String>, provider: &str, label: &str) -> Result<String> {
    let base = match base.filter(|b| !b.is_empty()) {
        Some(b) => b,
        None => match account.filter(|a| !a.is_empty()) {
            Some(a) => format!("https://api.cloudflare.com/client/v4/accounts/{}/ai/v1", bc::sigv4::encode_uri_component(&a)),
            None => {
                return Err(ProviderConfigurationError::new(provider, format!("{label}.configure requires accountId unless baseURL is supplied")));
            }
        },
    };
    let url = base.trim_end_matches('/');
    if !url.ends_with("/ai/v1") {
        return Err(ProviderConfigurationError::new(
            provider,
            format!("{label} needs accountId or a baseURL ending in /ai/v1 to derive the Run API base"),
        ));
    }
    Ok(format!("{}/run", &url[..url.len() - "/v1".len()]))
}

const WORKERS_ENV: &[&str] = &["CLOUDFLARE_API_KEY", "CLOUDFLARE_WORKERS_AI_TOKEN", "CLOUDFLARE_API_TOKEN"];

fn cloudflare_workers(model_id: &str, s: &mut Settings) -> Result<ImageModel> {
    let account = s.string("accountId");
    let api_key = s.string("apiKey");
    let base = s.string("baseURL");
    let body = s.body();
    let headers = s.headers();
    let run = cloudflare_run_base(base, account, "cloudflare-workers-ai", "CloudflareWorkersAI")?;
    let deployment = Deployment::new(bearer(api_key, WORKERS_ENV), Some(run), headers, body);
    Ok(image::cloudflare::model(model_id, &deployment, Some(take_rest(s))))
}

// ---------------------------------------------------------------------------------------------
// Decisions
// ---------------------------------------------------------------------------------------------

/// Build a decision model for a native `.../decisions` entrypoint (`None`: not a decision package).
pub fn decision_model(package: &str, model_id: &str, settings: &Object) -> Option<Result<DecisionModel>> {
    let name = package.strip_prefix(PREFIX)?;
    let mut s = Settings::new(settings);
    Some(match name {
        "vercel-ai-gateway/decisions" => vercel_decision(model_id, &mut s),
        "typesafe-ai/decisions" => typesafe_like(model_id, &mut s, "typesafe-ai", "typesafe", "https://api.typesafe.ai/v1", "TYPESAFE_API_KEY"),
        "opencode/decisions" => {
            typesafe_like(model_id, &mut s, "opencode", "opencode", "https://opencode.ai/inference/systemone/v1", "OPENCODE_API_KEY")
        }
        "openrouter/decisions" => openrouter_decision(model_id, &mut s),
        "digitalocean/decisions" => digitalocean_decision(model_id, &mut s),
        "baseten/decisions" => baseten_decision(model_id, &mut s),
        "cloudflare-workers-ai/decisions" => cloudflare_workers_decision(model_id, &mut s),
        "cloudflare-ai-gateway/decisions" => cloudflare_gateway_decision(model_id, &mut s),
        _ => return None,
    })
}

fn vercel_decision(model_id: &str, s: &mut Settings) -> Result<DecisionModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL").unwrap_or_else(|| "https://ai-gateway.vercel.sh/v1".into());
    let headers = s.headers();
    let body = s.body();
    let route = openai_decisions::route()
        .with_provider("vercel-ai-gateway", None)
        .with_base_url(Some(base))
        .with_auth(bearer(api_key, &["AI_GATEWAY_API_KEY", "VERCEL_OIDC_TOKEN"]))
        .with_headers(headers)
        .with_http(http_body(body));
    Ok(route.model(model_id, None, Some(take_rest(s))))
}

/// TypeSafe and OpenCode Zen: System One with TypeSafe's image opt-in header.
fn typesafe_like(model_id: &str, s: &mut Settings, provider: &str, key: &str, default_base: &str, env: &str) -> Result<DecisionModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL").unwrap_or_else(|| default_base.into());
    let headers = s.headers();
    let body = s.body();
    let mut merged = IndexMap::new();
    merged.insert("TypeSafe-Danger-Zone-Images".to_string(), "true".to_string());
    for (k, v) in headers.unwrap_or_default() {
        merged.insert(k, v);
    }
    let route = system_one::route()
        .with_provider(provider, Some(key))
        .with_base_url(Some(base))
        .with_auth(bearer(api_key, &[env]))
        .with_headers(Some(merged))
        .with_http(http_body(body));
    Ok(route.model(model_id, None, Some(take_rest(s))))
}

fn openrouter_decision(model_id: &str, s: &mut Settings) -> Result<DecisionModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL").unwrap_or_else(|| "https://openrouter.ai/api/v1".into());
    let headers = s.headers();
    let body = s.body();
    let route = system_one::route()
        .with_provider("openrouter", Some("openrouter"))
        .with_base_url(Some(base))
        .with_auth(bearer(api_key, &["OPENROUTER_API_KEY"]))
        .with_headers(headers)
        .with_http(http_body(body));
    Ok(route.model(model_id, Some("state"), Some(take_rest(s))))
}

fn digitalocean_decision(model_id: &str, s: &mut Settings) -> Result<DecisionModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL").unwrap_or_else(|| "https://inference.do-ai.run/v1".into());
    let headers = s.headers();
    let body = s.body();
    let mut merged = IndexMap::new();
    // DigitalOcean serves TypeSafe's Jev, which needs this header for images.
    merged.insert("TypeSafe-Danger-Zone-Images".to_string(), "true".to_string());
    for (k, v) in headers.unwrap_or_default() {
        merged.insert(k, v);
    }
    let route = system_one::route()
        .with_provider("digitalocean", Some("digitalocean"))
        .with_base_url(Some(base))
        .with_auth(bearer(api_key, &["DIGITALOCEAN_ACCESS_TOKEN", "DIGITALOCEAN_API_KEY", "DO_INFERENCE_API_KEY"]))
        .with_headers(Some(merged))
        // `config` always sets `http: {body}`.
        .with_http(http_body(body));
    Ok(route.model(model_id, None, Some(take_rest(s))))
}

fn baseten_decision(model_id: &str, s: &mut Settings) -> Result<DecisionModel> {
    let api_key = s.string("apiKey");
    let base = s.string("baseURL").unwrap_or_else(|| "https://inference.baseten.co/v1".into());
    let headers = s.headers();
    let body = s.body();
    let route = system_one::baseten_route(&base)
        .with_auth(bearer(api_key, &["BASETEN_API_KEY"]))
        .with_headers(headers)
        .with_http(http_body(body));
    Ok(route.model(model_id, None, Some(take_rest(s))))
}

fn cloudflare_workers_decision(model_id: &str, s: &mut Settings) -> Result<DecisionModel> {
    let account = s.string("accountId");
    let api_key = s.string("apiKey");
    let base = s.string("baseURL");
    let headers = s.headers();
    let body = s.body();
    let run = cloudflare_run_base(base, account, "cloudflare-workers-ai", "CloudflareWorkersAI")?;
    let route = system_one::cloudflare_route()
        .with_provider("cloudflare-workers-ai", None)
        .with_base_url(Some(run))
        .with_auth(bearer(api_key, WORKERS_ENV))
        .with_headers(headers)
        .with_http(http_body(body));
    Ok(route.model(model_id, None, Some(take_rest(s))))
}

fn cloudflare_gateway_decision(model_id: &str, s: &mut Settings) -> Result<DecisionModel> {
    let account = s.string("accountId");
    let api_key = s.string("apiKey");
    let base = s.string("baseURL");
    let cache_key = s.string("cacheKey");
    let cache_ttl = s.take("cacheTtl");
    let collect_log = s.take("collectLog");
    let gateway_key = s.string("gatewayApiKey");
    let gateway_id = s.string("gatewayId");
    let user_headers = s.headers();
    let metadata = s.take("metadata");
    let skip_cache = s.take("skipCache");
    let body = s.body();
    let provider = "cloudflare-ai-gateway";
    let gid = gateway_id.map(|g| g.trim().to_string()).filter(|g| !g.is_empty()).unwrap_or_else(|| "default".into());
    let mut headers = IndexMap::new();
    headers.insert("cf-aig-gateway-id".to_string(), gid);
    if let Some(m) = metadata {
        headers.insert("cf-aig-metadata".to_string(), crate::json::encode(&m));
    }
    if let Some(t) = cache_ttl {
        headers.insert("cf-aig-cache-ttl".to_string(), crate::json::js_string(&t));
    }
    if let Some(k) = cache_key {
        headers.insert("cf-aig-cache-key".to_string(), k);
    }
    if let Some(v) = skip_cache {
        headers.insert("cf-aig-skip-cache".to_string(), crate::json::js_string(&v));
    }
    if let Some(v) = collect_log {
        headers.insert("cf-aig-collect-log".to_string(), crate::json::js_string(&v));
    }
    for (k, v) in user_headers.unwrap_or_default() {
        headers.insert(k, v);
    }
    let credential = Credential::optional(gateway_key.or(api_key), "apiKey")
        .or_else(Credential::env("CLOUDFLARE_API_TOKEN"))
        .or_else(Credential::env("CF_AIG_TOKEN"));
    let run = cloudflare_run_base(base, account, provider, "CloudflareAIGateway")?;
    let route = system_one::cloudflare_route()
        .with_provider(provider, None)
        .with_base_url(Some(run))
        .with_auth(auth::bearer(credential))
        .with_headers(Some(headers))
        .with_http(http_body(body));
    Ok(route.model(model_id, None, Some(take_rest(s))))
}
