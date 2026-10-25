//! Catalog model → runtime [`LanguageModel`] (`packages/core/src/model-resolver.ts`).

use super::registry::Catalog;
use super::types::{
    headers_to_object, is_aisdk, merge_headers, merge_overlay, model_settings, native_settings, omit, package_name,
    timeout, timeouts, Activation, Capabilities, Cost, Limit, ModelInfo, ProviderInfo, BUILTIN_PACKAGES,
};
use crate::credentials::CredentialValue;
use crate::error::ProviderConfigurationError;
use crate::json::Object;
use crate::route::{auth, LanguageModel, Route};
use crate::types::{HttpOptions, HttpTimeout, ModelDefaults};
use oc_schema::types::ModelRef;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::sync::Arc;

/// `ModelResolver.InitializationPhase`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitializationPhase {
    Load,
    Init,
    Construct,
}

impl InitializationPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::Init => "init",
            Self::Construct => "construct",
        }
    }
}

fn variables_message(variables: &[String]) -> String {
    format!("{} {}", variables.join(", "), if variables.len() == 1 { "is" } else { "are" })
}

/// Model resolution failures (messages match upstream).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ResolveError {
    /// The selected model is not in the catalog (upstream returns `undefined`).
    #[error("Model not found: {provider_id}/{model_id}")]
    ModelNotFound { provider_id: String, model_id: String },
    #[error("Variant unavailable for {provider_id}/{model_id}: {variant}")]
    VariantUnavailable { provider_id: String, model_id: String, variant: String },
    #[error("Unsupported package for {provider_id}/{model_id}: {package}")]
    UnsupportedPackage { provider_id: String, model_id: String, package: String },
    /// `aisdk:` packages need the AI SDK bridge, which the Rust port does not have.
    #[error("Unsupported package for {provider_id}/{model_id}: {package}")]
    AisdkPackage { provider_id: String, model_id: String, package: String },
    #[error("Cannot initialize {provider_id}/{model_id}: {detail}")]
    ModelConfiguration { provider_id: String, model_id: String, package: String, detail: String },
    #[error("Cannot initialize {provider_id}/{model_id}: {detail}")]
    ModelInitialization { provider_id: String, model_id: String, package: String, phase: InitializationPhase, detail: String },
    #[error("Cannot initialize {provider_id}/{model_id}: {} required to resolve the provider endpoint", variables_message(.variables))]
    UnresolvedProviderVariables { provider_id: String, model_id: String, variables: Vec<String> },
    #[error("Provider compaction is not supported by {provider_id}/{model_id} ({route})")]
    UnsupportedCompaction { provider_id: String, model_id: String, route: String },
}

impl ResolveError {
    /// Upstream `_tag`.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::ModelNotFound { .. } => "SessionRunnerModel.ModelNotFoundError",
            Self::VariantUnavailable { .. } => "SessionRunnerModel.VariantUnavailableError",
            Self::UnsupportedPackage { .. } | Self::AisdkPackage { .. } => "SessionRunnerModel.UnsupportedPackageError",
            Self::ModelConfiguration { .. } => "SessionRunnerModel.ModelConfigurationError",
            Self::ModelInitialization { .. } => "SessionRunnerModel.ModelInitializationError",
            Self::UnresolvedProviderVariables { .. } => "SessionRunnerModel.UnresolvedProviderVariablesError",
            Self::UnsupportedCompaction { .. } => "SessionRunnerModel.UnsupportedCompactionError",
        }
    }
}

/// Native package loader (`Provider.loadPackage` + `module.model(modelID, settings)`).
pub trait PackageLoader: Send + Sync {
    /// Whether the package can be loaded (else a `load` initialization error).
    fn has(&self, specifier: &str) -> bool;
    fn model(&self, specifier: &str, model_id: &str, settings: &Object) -> Result<LanguageModel, ProviderConfigurationError>;
}

/// The bundled native packages ([`crate::providers::language_model`]).
pub struct NativePackages;

impl PackageLoader for NativePackages {
    fn has(&self, specifier: &str) -> bool {
        BUILTIN_PACKAGES.contains(&specifier)
    }
    fn model(&self, specifier: &str, model_id: &str, settings: &Object) -> Result<LanguageModel, ProviderConfigurationError> {
        crate::providers::language_model(specifier, model_id, settings)
    }
}

pub type EnvFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;
pub type CompactionSupportFn = Arc<dyn Fn(&Route) -> bool + Send + Sync>;

/// Resolution dependencies (`ModelResolver.Dependencies`).
#[derive(Clone, Default)]
pub struct ResolveOptions {
    /// Package loader; defaults to [`NativePackages`].
    pub loader: Option<Arc<dyn PackageLoader>>,
    /// Environment lookup for `${VAR}` base URL placeholders; defaults to the process environment.
    pub env: Option<EnvFn>,
    /// Whether a route supports provider-native compaction; defaults to OpenAI Responses routes.
    pub native_compaction: Option<CompactionSupportFn>,
    /// AI SDK bridge for `aisdk:` packages (upstream `loadAISDK`); without it they are unsupported.
    pub aisdk: Option<Arc<dyn crate::aisdk::AiSdkBridge>>,
}

impl ResolveOptions {
    fn env(&self, name: &str) -> Option<String> {
        match &self.env {
            Some(f) => f(name),
            None => std::env::var(name).ok(),
        }
    }
    fn supports_native_compaction(&self, route: &Route) -> bool {
        match &self.native_compaction {
            Some(f) => f(route),
            None => route.protocol.id() == "openai-responses",
        }
    }
}

/// A resolved runtime model with its catalog identity.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    /// Route-level model; its id is the provider API model id.
    pub language_model: LanguageModel,
    /// The provider definition.
    pub provider: ProviderInfo,
    /// The selected (effective) catalog model, before the variant overlay.
    pub model: ModelInfo,
    /// The requested variant id.
    pub variant: Option<String>,
    /// Effective compaction policy (`settings.compaction.type`); omitted means summary.
    pub compaction: Option<String>,
    /// Provider transport policy; omitted means HTTP.
    pub transport: Option<String>,
    /// WebSocket idle limit (`false` disables it).
    pub chunk_timeout: Option<HttpTimeout>,
}

impl ResolvedModel {
    /// Selected catalog identity (durable records must use this, never the API model id).
    pub fn model_ref(&self) -> ModelRef {
        ModelRef { id: self.model.id.clone(), provider_id: self.model.provider_id.clone(), variant: self.variant.clone() }
    }
    pub fn capabilities(&self) -> &Capabilities {
        &self.model.capabilities
    }
    pub fn cost(&self) -> &[Cost] {
        &self.model.cost
    }
    pub fn limit(&self) -> &Limit {
        &self.model.limit
    }
    pub fn context_limit(&self) -> i64 {
        self.model.limit.context
    }
    pub fn input_limit(&self) -> Option<i64> {
        self.model.limit.input
    }
    pub fn output_limit(&self) -> i64 {
        self.model.limit.output
    }
    /// The base cost entry (no tier), or the first entry.
    pub fn base_cost(&self) -> Option<&Cost> {
        self.model.cost.iter().find(|c| c.tier.is_none()).or_else(|| self.model.cost.first())
    }
    /// Cost entry for a context size: the largest context tier whose size is exceeded, else the base.
    pub fn cost_for_context(&self, tokens: i64) -> Option<&Cost> {
        self.model
            .cost
            .iter()
            .filter(|c| c.tier.as_ref().map(|t| tokens > t.size).unwrap_or(false))
            .max_by_key(|c| c.tier.as_ref().map(|t| t.size).unwrap_or(0))
            .or_else(|| self.base_cost())
    }
}

/// `withVariant`: overlay a variant (`"default"` = none) onto the model.
pub fn with_variant(model: &ModelInfo, variant: Option<&str>) -> Result<ModelInfo, ResolveError> {
    let id = variant.filter(|v| *v != "default");
    let Some(id) = id else { return Ok(model.clone()) };
    let Some(selected) = model.variants.iter().find(|v| v.id == id) else {
        return Err(ResolveError::VariantUnavailable {
            provider_id: model.provider_id.clone(),
            model_id: model.id.clone(),
            variant: id.to_string(),
        });
    };
    let mut out = model.clone();
    out.settings = merge_overlay(model.settings.as_ref(), model_settings(selected.settings.as_ref()).as_ref());
    out.headers = merge_headers(model.headers.as_ref(), selected.headers.as_ref());
    out.body = merge_overlay(model.body.as_ref(), selected.body.as_ref());
    Ok(out)
}

static PLACEHOLDER: Lazy<Regex> = Lazy::new(|| Regex::new(r"\$\{([^}]+)\}").unwrap());

fn unresolved(model: &ModelInfo, base_url: &str) -> Option<ResolveError> {
    let mut variables: Vec<String> = vec![];
    for capture in PLACEHOLDER.captures_iter(base_url) {
        let name = capture[1].to_string();
        if !variables.contains(&name) {
            variables.push(name);
        }
    }
    if variables.is_empty() {
        return None;
    }
    Some(ResolveError::UnresolvedProviderVariables {
        provider_id: model.provider_id.clone(),
        model_id: model.id.clone(),
        variables,
    })
}

/// Replace `${VAR}` in `baseURL` from the environment; unresolved placeholders fail.
fn prepare_provider_settings(model: &ModelInfo, mut settings: Object, options: &ResolveOptions) -> Result<Object, ResolveError> {
    let Some(base_url) = settings.get("baseURL").and_then(Value::as_str).map(String::from) else { return Ok(settings) };
    if !base_url.contains("${") {
        return Ok(settings);
    }
    let prepared = PLACEHOLDER
        .replace_all(&base_url, |c: &regex::Captures<'_>| options.env(&c[1]).unwrap_or_else(|| c[0].to_string()))
        .into_owned();
    if let Some(failure) = unresolved(model, &prepared) {
        return Err(failure);
    }
    if prepared != base_url {
        settings.insert("baseURL".into(), Value::String(prepared));
    }
    Ok(settings)
}

fn token_settings(specifier: &str, token: &str) -> Object {
    let key = if specifier == "@opencode/ai/providers/anthropic" || specifier == "@opencode/ai/providers/anthropic-compatible" {
        "authToken"
    } else if specifier == "@opencode/ai/providers/google-vertex" || specifier.starts_with("@opencode/ai/providers/google-vertex/") {
        "accessToken"
    } else {
        "apiKey"
    };
    let mut o = Object::new();
    o.insert(key.into(), Value::String(token.into()));
    o
}

/// Credential → package settings (`nativeCredentialSettings`).
pub fn native_credential_settings(provider_id: &str, specifier: &str, credential: Option<&CredentialValue>) -> Object {
    match credential {
        None => Object::new(),
        Some(CredentialValue::Key { key, .. }) => {
            let mut o = Object::new();
            o.insert("apiKey".into(), Value::String(key.clone()));
            o
        }
        Some(CredentialValue::Oauth { access, .. }) => token_settings(specifier, access),
        Some(CredentialValue::External { .. }) => {
            if specifier.starts_with("@opencode/ai/providers/amazon-bedrock") {
                let mut o = Object::new();
                o.insert("auth".into(), Value::String("sigv4".into()));
                return o;
            }
            if provider_id == "azure" {
                return token_settings(specifier, "azure-cli");
            }
            Object::new()
        }
    }
}

fn credential_metadata(credential: Option<&CredentialValue>) -> Option<&Object> {
    match credential? {
        CredentialValue::Oauth { metadata, .. } | CredentialValue::Key { metadata, .. } | CredentialValue::External { metadata, .. } => {
            metadata.as_ref()
        }
    }
}

/// `fromCatalogModel`: build the runtime model for an effective catalog model (provider settings merged).
pub fn from_catalog_model(
    model: &ModelInfo,
    credential: Option<&CredentialValue>,
    options: &ResolveOptions,
) -> Result<LanguageModel, ResolveError> {
    let mut resolved = model.clone();
    if resolved.settings.as_ref().and_then(|s| s.get("apiKey")) == Some(&Value::String(String::new())) {
        if let Some(settings) = resolved.settings.as_mut() {
            settings.shift_remove("apiKey");
        }
    }
    let package = resolved.package.clone().unwrap_or_default();
    let unsupported = || ResolveError::UnsupportedPackage {
        provider_id: resolved.provider_id.clone(),
        model_id: resolved.id.clone(),
        package: resolved.package.clone().unwrap_or_else(|| "unknown".into()),
    };
    let mut configured = resolved.settings.clone().unwrap_or_default();
    if let Some(metadata) = credential_metadata(credential) {
        for (k, v) in metadata {
            configured.insert(k.clone(), v.clone());
        }
    }
    if is_aisdk(Some(&package)) {
        let Some(bridge) = options.aisdk.clone() else {
            return Err(ResolveError::AisdkPackage {
                provider_id: resolved.provider_id.clone(),
                model_id: resolved.id.clone(),
                package,
            });
        };
        let mut overlay = native_credential_settings(&resolved.provider_id, &package, credential);
        if let Some(metadata) = credential_metadata(credential) {
            for (k, v) in metadata {
                overlay.insert(k.clone(), v.clone());
            }
        }
        let merged = merge_overlay(resolved.settings.as_ref(), Some(&overlay)).unwrap_or_default();
        let mut info = resolved.clone();
        info.settings = Some(prepare_provider_settings(&resolved, merged, options)?);
        let runtime = crate::aisdk::language_model(&info, bridge);
        // The AI SDK route has no provider compaction operations.
        let native = resolved.settings.as_ref().and_then(|s| s.get("compaction")).and_then(|c| c.get("type")).and_then(Value::as_str)
            == Some("native");
        if native {
            return Err(ResolveError::UnsupportedCompaction {
                provider_id: resolved.provider_id.clone(),
                model_id: resolved.id.clone(),
                route: runtime.route.id.clone(),
            });
        }
        return Ok(runtime);
    }
    let specifier = package_name(&package);
    if specifier.is_empty() {
        return Err(unsupported());
    }
    let mapped = prepare_provider_settings(&resolved, native_settings(&configured), options)?;
    let loader: Arc<dyn PackageLoader> = options.loader.clone().unwrap_or_else(|| Arc::new(NativePackages));
    if !loader.has(&specifier) {
        return Err(ResolveError::ModelInitialization {
            provider_id: resolved.provider_id.clone(),
            model_id: resolved.id.clone(),
            package: package.clone(),
            phase: InitializationPhase::Load,
            detail: format!("Provider package {specifier} is not installed"),
        });
    }
    let mut settings = if credential.is_some() { omit(&mapped, &["accessToken", "apiKey", "authToken"]) } else { mapped };
    if let Some(canonical) = &resolved.canonical {
        settings.insert("provider".into(), Value::String(canonical.clone()));
    }
    for (k, v) in native_credential_settings(&resolved.provider_id, &specifier, credential) {
        settings.insert(k, v);
    }
    if let Some(headers) = &resolved.headers {
        settings.insert("headers".into(), Value::Object(headers_to_object(headers)));
    }
    if let Some(body) = &resolved.body {
        settings.insert("body".into(), Value::Object(body.clone()));
    }
    let runtime = loader.model(&specifier, &resolved.model_id, &settings).map_err(|cause| ResolveError::ModelConfiguration {
        provider_id: resolved.provider_id.clone(),
        model_id: resolved.id.clone(),
        package: if package.is_empty() { "unknown".into() } else { package.clone() },
        detail: cause.message,
    })?;
    let provider = resolved.canonical.clone().unwrap_or_else(|| resolved.provider_id.clone());
    let mut runtime = runtime.with_provider(&provider);
    if let Some(compatibility) = &resolved.compatibility {
        runtime = runtime.with_compatibility(compatibility.clone());
    }
    let defaults = runtime.defaults.clone().unwrap_or_default();
    let http = HttpOptions::merge(&[defaults.http.as_ref(), Some(&timeouts(&configured))]);
    let runtime = runtime.with_defaults(ModelDefaults { http, ..defaults });

    if let Some(base_url) = runtime.route.endpoint.base_url.as_deref() {
        if let Some(failure) = unresolved(&resolved, base_url) {
            return Err(failure);
        }
    }
    let native = resolved
        .settings
        .as_ref()
        .and_then(|s| s.get("compaction"))
        .and_then(|c| c.get("type"))
        .and_then(Value::as_str)
        == Some("native");
    if native && !options.supports_native_compaction(&runtime.route) {
        return Err(ResolveError::UnsupportedCompaction {
            provider_id: resolved.provider_id.clone(),
            model_id: resolved.id.clone(),
            route: runtime.route.id.clone(),
        });
    }
    Ok(runtime)
}

impl ResolvedModel {
    /// Whether the model runs through the AI SDK bridge.
    pub fn is_aisdk(&self) -> bool {
        crate::aisdk::bridge_of(&self.language_model).is_some()
    }

    /// The eager part of resolution that needs I/O: for AI SDK packages, upstream's `AISDK.model`
    /// loads the package and constructs the language model while resolving
    /// (`ModelInitializationError` phase `init` on failure). A no-op for native packages.
    pub async fn initialize(&self) -> Result<(), ResolveError> {
        let Some((bridge, info)) = crate::aisdk::bridge_of(&self.language_model) else { return Ok(()) };
        let package = self.model.package.clone().unwrap_or_else(|| "unknown".into());
        bridge.load(info).await.map_err(|detail| ResolveError::ModelInitialization {
            provider_id: self.model.provider_id.clone(),
            model_id: self.model.id.clone(),
            detail: detail.unwrap_or_else(|| format!("init failed for {package}")),
            package,
            phase: InitializationPhase::Init,
        })
    }
}

/// `hasConfiguredAuth`.
pub fn has_configured_auth(model: &ModelInfo) -> bool {
    ["apiKey", "authToken", "accessToken"].iter().any(|key| {
        model.settings.as_ref().and_then(|s| s.get(*key)).and_then(Value::as_str).map(|v| !v.is_empty()).unwrap_or(false)
    })
}

/// `usesAPIKeyAuth`: packages whose auth is a plain API key (keyless when explicitly enabled).
pub fn uses_api_key_auth(package: Option<&str>) -> bool {
    let Some(package) = package else { return false };
    let name = package_name(package);
    matches!(
        name.as_str(),
        "@ai-sdk/openai"
            | "@ai-sdk/anthropic"
            | "@ai-sdk/cerebras"
            | "@ai-sdk/deepinfra"
            | "@ai-sdk/openai-compatible"
            | "@ai-sdk/google"
            | "@ai-sdk/groq"
            | "@ai-sdk/mistral"
            | "@ai-sdk/togetherai"
            | "@ai-sdk/xai"
            | "@openrouter/ai-sdk-provider"
            | "@ai-sdk/azure"
            | "@opencode/ai/providers/openai"
            | "@opencode/ai/providers/anthropic"
            | "@opencode/ai/providers/anthropic-compatible"
            | "@opencode/ai/providers/baseten"
            | "@opencode/ai/providers/cerebras"
            | "@opencode/ai/providers/cloudflare-ai-gateway"
            | "@opencode/ai/providers/cloudflare-workers-ai"
            | "@opencode/ai/providers/deepinfra"
            | "@opencode/ai/providers/deepseek"
            | "@opencode/ai/providers/fireworks"
            | "@opencode/ai/providers/openai-compatible"
            | "@opencode/ai/providers/google"
            | "@opencode/ai/providers/google/interactions"
            | "@opencode/ai/providers/groq"
            | "@opencode/ai/providers/mistral"
            | "@opencode/ai/providers/togetherai"
            | "@opencode/ai/providers/vercel-ai-gateway"
            | "@opencode/ai/providers/xai"
            | "@opencode/ai/providers/openrouter"
            | "@opencode/ai/providers/azure"
    ) || name.starts_with("@opencode/ai/providers/openai/")
        || name.starts_with("@opencode/ai/providers/azure/")
}

impl Catalog {
    /// `ModelResolver.resolve` selection: the requested model, else the default model with a package,
    /// else the newest available text model with a package.
    pub fn select(&self, requested: Option<(&str, &str)>) -> Option<&ModelInfo> {
        if let Some((provider_id, model_id)) = requested {
            return self.model(provider_id, model_id);
        }
        match self.default_model() {
            Some(model) if model.has_package() => Some(model),
            _ => self.available_models().into_iter().find(|m| m.has_package() && m.supports_text()),
        }
    }

    /// Resolve a catalog model into a runtime model with the native packages.
    pub fn resolve(
        &self,
        provider_id: &str,
        model_id: &str,
        variant: Option<&str>,
        credential: Option<&CredentialValue>,
    ) -> Result<ResolvedModel, ResolveError> {
        self.resolve_with(provider_id, model_id, variant, credential, &ResolveOptions::default())
    }

    pub fn resolve_with(
        &self,
        provider_id: &str,
        model_id: &str,
        variant: Option<&str>,
        credential: Option<&CredentialValue>,
        options: &ResolveOptions,
    ) -> Result<ResolvedModel, ResolveError> {
        let selected = self.model(provider_id, model_id).ok_or_else(|| ResolveError::ModelNotFound {
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
        })?;
        resolve_model(self.provider(&selected.provider_id), selected, variant, credential, options)
    }
}

/// `ModelResolver.resolveModel` for a selected effective model and its provider definition.
pub fn resolve_model(
    provider: Option<&ProviderInfo>,
    selected: &ModelInfo,
    variant: Option<&str>,
    credential: Option<&CredentialValue>,
    options: &ResolveOptions,
) -> Result<ResolvedModel, ResolveError> {
    let selected_variant = with_variant(selected, variant)?;
    let mut runtime_info = selected_variant.clone();
    runtime_info.settings = merge_overlay(
        provider.and_then(|p| p.settings.as_ref()),
        model_settings(selected_variant.settings.as_ref()).as_ref(),
    );
    let model = from_catalog_model(&runtime_info, credential, options)?;
    let keyless = provider.map(|p| p.activation == Activation::Enabled).unwrap_or(false)
        && credential.is_none()
        && !has_configured_auth(&runtime_info)
        && uses_api_key_auth(runtime_info.package.as_deref());
    let language_model = if keyless { model.map_route(|route| route.auth(auth::none())) } else { model };
    Ok(ResolvedModel {
        language_model,
        provider: provider.cloned().unwrap_or_else(|| ProviderInfo::empty(&selected.provider_id)),
        model: selected.clone(),
        variant: variant.map(String::from),
        compaction: runtime_info
            .settings
            .as_ref()
            .and_then(|s| s.get("compaction"))
            .and_then(|c| c.get("type"))
            .and_then(Value::as_str)
            .map(String::from),
        transport: provider.and_then(|p| p.transport()).map(String::from),
        chunk_timeout: timeout(provider.and_then(|p| p.setting("chunkTimeout"))),
    })
}
