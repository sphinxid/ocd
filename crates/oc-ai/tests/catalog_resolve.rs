//! Port of `packages/core/test/model-resolver.test.ts` (package-independent parts) with an injected
//! package loader that records the settings each native package receives.

use oc_ai::catalog::models_dev::bundled;
use oc_ai::catalog::native::rewrite_model;
use oc_ai::catalog::plugins::ModelsDevPlugin;
use oc_ai::catalog::registry::{Access, Catalog, CatalogPlugin, ProviderEditor};
use oc_ai::catalog::resolve::{from_catalog_model, resolve_model, with_variant, PackageLoader, ResolveError, ResolveOptions};
use oc_ai::catalog::types::{Activation, CostTier, ModelInfo, ProviderInfo, Variant};
use oc_ai::catalog::{Cost, CostCache};
use oc_ai::credentials::CredentialValue;
use oc_ai::error::{AIError, ProviderConfigurationError};
use oc_ai::json::Object;
use oc_ai::protocols::{Protocol, Step, StreamParser};
use oc_ai::route::auth::{self, AuthInput};
use oc_ai::route::framing::{Frame, Framing};
use oc_ai::route::{Endpoint, Headers, LanguageModel, Route};
use oc_ai::types::{Compatibility, HttpTimeout, LLMRequest};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;

struct TestProtocol(&'static str);

struct NoParser;
impl StreamParser for NoParser {
    fn step(&mut self, _: &Frame) -> Result<Step, AIError> {
        Ok(Step::default())
    }
}

impl Protocol for TestProtocol {
    fn id(&self) -> &str {
        self.0
    }
    fn body(&self, _: &LLMRequest) -> Result<Value, AIError> {
        Ok(json!({}))
    }
    fn parser(&self, _: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(NoParser)
    }
}

#[derive(Default)]
struct FakeLoader {
    calls: Mutex<Vec<(String, String, Object)>>,
}

impl FakeLoader {
    fn last(&self) -> (String, String, Object) {
        self.calls.lock().last().cloned().expect("package was called")
    }
}

impl PackageLoader for FakeLoader {
    fn has(&self, specifier: &str) -> bool {
        specifier.starts_with("@opencode/ai/providers/")
    }
    fn model(&self, specifier: &str, model_id: &str, settings: &Object) -> Result<LanguageModel, ProviderConfigurationError> {
        self.calls.lock().push((specifier.into(), model_id.into(), settings.clone()));
        if let Some(Value::String(message)) = settings.get("fail") {
            return Err(ProviderConfigurationError::new("test", message.clone()));
        }
        let protocol = if specifier.contains("openai") { "openai-responses" } else { "test-protocol" };
        let base = settings.get("baseURL").and_then(Value::as_str).unwrap_or("https://default.example/v1");
        let api_key = settings.get("apiKey").and_then(Value::as_str).map(String::from);
        let route = Route::new("test-route", Arc::new(TestProtocol(protocol)), Endpoint::new(Some(base), "/chat"), Framing::Sse)
            .initial_provider("test", None)
            .auth(auth::bearer_options(api_key, &["OC_AI_CATALOG_TEST_UNSET_KEY"]));
        let model = route.model(model_id).map_err(|e| ProviderConfigurationError::new("test", e.message))?;
        Ok(model.with_compatibility(Compatibility { supports_store: Some(false), reasoning_field: Some("package".into()), ..Default::default() }))
    }
}

fn options(loader: &Arc<FakeLoader>) -> ResolveOptions {
    ResolveOptions {
        loader: Some(loader.clone()),
        env: Some(Arc::new(|name: &str| match name {
            "ACME_HOST" => Some("api.acme.test".to_string()),
            "SNEAKY" => Some("${INNER}".to_string()),
            _ => None,
        })),
        native_compaction: None,
        aisdk: None,
    }
}

fn obj(v: Value) -> Object {
    v.as_object().unwrap().clone()
}

/// `model(packageName, options)` of the upstream test (rewritten like a catalog write).
fn model(package: &str, settings: Value) -> ModelInfo {
    let mut m = ModelInfo::default_for("test-provider", "test-model");
    m.model_id = "api-test-model".into();
    m.name = "Test model".into();
    m.package = Some(package.into());
    m.settings = Some(obj(settings));
    m.headers = Some([("x-test".to_string(), "header".to_string())].into_iter().collect());
    m.body = Some(obj(json!({"custom_extension": {"enabled": true}})));
    m.capabilities.input = vec!["text".into()];
    m.limit.context = 100;
    m.limit.output = 20;
    rewrite_model(&mut m, None);
    m
}

async fn authorization(lm: &LanguageModel) -> Result<Headers, AIError> {
    auth::apply(lm.route.auth.as_ref(), AuthInput { method: "POST", url: "https://x", body: "{}", headers: Headers::new() }).await
}

#[test]
fn passes_flat_settings_with_headers_body_and_api_model_id() {
    let loader = Arc::new(FakeLoader::default());
    let lm = from_catalog_model(
        &model("aisdk:@ai-sdk/openai", json!({"apiKey": "secret", "baseURL": "https://openai.example/v1", "timeout": 5, "transport": "websocket", "compaction": {"type": "summary"}})),
        None,
        &options(&loader),
    )
    .unwrap();
    let (specifier, model_id, settings) = loader.last();
    assert_eq!(specifier, "@opencode/ai/providers/openai");
    assert_eq!(model_id, "api-test-model");
    assert_eq!(
        Value::Object(settings),
        json!({"apiKey": "secret", "baseURL": "https://openai.example/v1", "headers": {"x-test": "header"}, "body": {"custom_extension": {"enabled": true}}})
    );
    assert_eq!(lm.id, "api-test-model");
    assert_eq!(lm.provider, "test-provider");
    let http = lm.defaults.as_ref().unwrap().http.as_ref().unwrap();
    assert_eq!(http.timeout, Some(HttpTimeout::Millis(5.0)));
    assert_eq!(http.header_timeout, Some(HttpTimeout::Millis(300_000.0)));
    assert_eq!(http.chunk_timeout, Some(HttpTimeout::Millis(900_000.0)));
}

#[test]
fn timeouts_from_settings() {
    let loader = Arc::new(FakeLoader::default());
    let lm = from_catalog_model(
        &model("@opencode/ai/providers/openai", json!({"headerTimeout": 10, "chunkTimeout": false, "timeout": -1})),
        None,
        &options(&loader),
    )
    .unwrap();
    let http = lm.defaults.unwrap().http.unwrap();
    assert_eq!(http.timeout, None);
    assert_eq!(http.header_timeout, Some(HttpTimeout::Millis(10.0)));
    assert_eq!(http.chunk_timeout, Some(HttpTimeout::Disabled(false)));
}

#[tokio::test]
async fn treats_empty_api_key_as_omitted() {
    let loader = Arc::new(FakeLoader::default());
    let lm = from_catalog_model(&model("aisdk:@ai-sdk/openai", json!({"apiKey": ""})), None, &options(&loader)).unwrap();
    assert!(loader.last().2.get("apiKey").is_none());
    assert_eq!(authorization(&lm).await.unwrap_err().message, "OC_AI_CATALOG_TEST_UNSET_KEY is not set");
}

#[test]
fn resolves_environment_templates_without_mutating_the_catalog() {
    let loader = Arc::new(FakeLoader::default());
    let catalog = model("aisdk:@ai-sdk/openai-compatible", json!({"baseURL": "https://${ACME_HOST}/v1"}));
    let lm = from_catalog_model(&catalog, None, &options(&loader)).unwrap();
    assert_eq!(lm.route.endpoint.base_url.as_deref(), Some("https://api.acme.test/v1"));
    assert_eq!(catalog.settings.as_ref().unwrap()["baseURL"], "https://${ACME_HOST}/v1");
    // openai-compatible catalog writes name the provider.
    assert_eq!(loader.last().2["provider"], "test-provider");
}

#[test]
fn rejects_unresolved_variables() {
    let loader = Arc::new(FakeLoader::default());
    let failure = from_catalog_model(
        &model("aisdk:@ai-sdk/openai-compatible", json!({"baseURL": "https://${REQUIRED_HOST}/${REQUIRED_PATH}/v1"})),
        None,
        &options(&loader),
    )
    .unwrap_err();
    assert_eq!(failure.tag(), "SessionRunnerModel.UnresolvedProviderVariablesError");
    assert_eq!(
        failure.to_string(),
        "Cannot initialize test-provider/test-model: REQUIRED_HOST, REQUIRED_PATH are required to resolve the provider endpoint"
    );
    assert!(loader.calls.lock().is_empty());
    let failure = from_catalog_model(&model("@opencode/ai/providers/openai", json!({"baseURL": "https://${SNEAKY}/v1"})), None, &options(&loader)).unwrap_err();
    assert_eq!(failure.to_string(), "Cannot initialize test-provider/test-model: INNER is required to resolve the provider endpoint");
}

#[test]
fn rejects_placeholders_left_on_constructed_routes() {
    struct Templated;
    impl PackageLoader for Templated {
        fn has(&self, _: &str) -> bool {
            true
        }
        fn model(&self, _: &str, id: &str, _: &Object) -> Result<LanguageModel, ProviderConfigurationError> {
            Route::new("r", Arc::new(TestProtocol("p")), Endpoint::new(Some("https://${ROUTE_HOST}/v1"), "/x"), Framing::Sse).model(id)
        }
    }
    let failure = from_catalog_model(
        &model("@opencode/ai/providers/openai", json!({})),
        None,
        &ResolveOptions { loader: Some(Arc::new(Templated)), ..Default::default() },
    )
    .unwrap_err();
    assert_eq!(failure.to_string(), "Cannot initialize test-provider/test-model: ROUTE_HOST is required to resolve the provider endpoint");
}

#[test]
fn injects_credentials() {
    let loader = Arc::new(FakeLoader::default());
    let opts = options(&loader);
    let key = CredentialValue::Key {
        key: "stored".into(),
        metadata: Some(obj(json!({"accountID": "acct", "resourceName": "res"}))),
    };
    from_catalog_model(&model("@opencode/ai/providers/openai", json!({"apiKey": "configured", "authToken": "t"})), Some(&key), &opts).unwrap();
    let settings = loader.last().2;
    assert_eq!(settings["apiKey"], "stored");
    assert!(settings.get("authToken").is_none());
    assert_eq!(settings["accountID"], "acct");
    assert_eq!(settings["resourceName"], "res");

    let oauth = |access: &str| CredentialValue::Oauth { method_id: "m".into(), refresh: "r".into(), access: access.into(), expires: 0, metadata: None };
    from_catalog_model(&model("@opencode/ai/providers/anthropic", json!({})), Some(&oauth("tok")), &opts).unwrap();
    assert_eq!(loader.last().2["authToken"], "tok");
    from_catalog_model(&model("@opencode/ai/providers/anthropic-compatible", json!({})), Some(&oauth("tok")), &opts).unwrap();
    assert_eq!(loader.last().2["authToken"], "tok");
    from_catalog_model(&model("@opencode/ai/providers/google-vertex/messages", json!({})), Some(&oauth("tok")), &opts).unwrap();
    assert_eq!(loader.last().2["accessToken"], "tok");
    from_catalog_model(&model("@opencode/ai/providers/openai", json!({})), Some(&oauth("tok")), &opts).unwrap();
    assert_eq!(loader.last().2["apiKey"], "tok");

    let external = CredentialValue::External { method_id: "aws-profile".into(), metadata: Some(obj(json!({"profile": "work"}))) };
    from_catalog_model(&model("@opencode/ai/providers/amazon-bedrock", json!({"apiKey": "x"})), Some(&external), &opts).unwrap();
    let settings = loader.last().2;
    assert_eq!(settings["auth"], "sigv4");
    assert_eq!(settings["profile"], "work");
    assert!(settings.get("apiKey").is_none());

    let mut azure = model("@opencode/ai/providers/azure/responses", json!({}));
    azure.provider_id = "azure".into();
    let cli = CredentialValue::External { method_id: "azure-cli".into(), metadata: None };
    from_catalog_model(&azure, Some(&cli), &opts).unwrap();
    assert_eq!(loader.last().2["apiKey"], "azure-cli");
}

#[test]
fn canonical_provider_and_compatibility_merge() {
    let loader = Arc::new(FakeLoader::default());
    let mut m = model("aisdk:@ai-sdk/openai-compatible", json!({"apiKey": "settings-secret", "baseURL": "https://compatible.example/v1", "compatibility": "strict"}));
    m.canonical = Some("deepseek".into());
    m.compatibility = Some(Compatibility { reasoning_field: Some("vendor_reasoning".into()), require_reasoning: Some(true), ..Default::default() });
    let lm = from_catalog_model(&m, None, &options(&loader)).unwrap();
    let settings = loader.last().2;
    assert_eq!(settings["provider"], "deepseek");
    assert!(settings.get("compatibility").is_none());
    assert_eq!(lm.provider, "deepseek");
    let compat = lm.compatibility.unwrap();
    assert_eq!(compat.reasoning_field.as_deref(), Some("vendor_reasoning"));
    assert_eq!(compat.require_reasoning, Some(true));
    assert_eq!(compat.supports_store, Some(false));
}

#[test]
fn package_errors() {
    let loader = Arc::new(FakeLoader::default());
    let opts = options(&loader);
    let failure = from_catalog_model(&model("aisdk:custom-sdk", json!({})), None, &opts).unwrap_err();
    assert!(matches!(failure, ResolveError::AisdkPackage { .. }));
    assert_eq!(failure.to_string(), "Unsupported package for test-provider/test-model: aisdk:custom-sdk");
    assert_eq!(failure.tag(), "SessionRunnerModel.UnsupportedPackageError");

    let mut none = model("@opencode/ai/providers/openai", json!({}));
    none.package = None;
    assert_eq!(from_catalog_model(&none, None, &opts).unwrap_err().to_string(), "Unsupported package for test-provider/test-model: unknown");

    let failure = from_catalog_model(&model("file:///custom.js", json!({})), None, &opts).unwrap_err();
    assert_eq!(failure.to_string(), "Cannot initialize test-provider/test-model: Provider package file:///custom.js is not installed");
    assert_eq!(failure.tag(), "SessionRunnerModel.ModelInitializationError");

    let failure = from_catalog_model(&model("@opencode/ai/providers/openai", json!({"fail": "Azure requires resourceName or baseURL"})), None, &opts).unwrap_err();
    assert_eq!(failure.to_string(), "Cannot initialize test-provider/test-model: Azure requires resourceName or baseURL");
    assert_eq!(failure.tag(), "SessionRunnerModel.ModelConfigurationError");

    // Legacy scope migration of native entrypoints.
    from_catalog_model(&model("@opencode-ai/ai/providers/openai", json!({})), None, &opts).unwrap();
    assert_eq!(loader.last().0, "@opencode/ai/providers/openai");
}

#[test]
fn rejects_native_compaction_on_unsupported_routes() {
    let loader = Arc::new(FakeLoader::default());
    let failure = from_catalog_model(&model("@opencode/ai/providers/anthropic", json!({"compaction": {"type": "native"}})), None, &options(&loader)).unwrap_err();
    assert_eq!(failure.to_string(), "Provider compaction is not supported by test-provider/test-model (test-route)");
    from_catalog_model(&model("@opencode/ai/providers/openai", json!({"compaction": {"type": "native"}})), None, &options(&loader)).unwrap();
}

#[test]
fn variants() {
    let mut m = model("@opencode/ai/providers/openai", json!({"reasoningEffort": "low"}));
    m.variants = vec![Variant {
        id: "high".into(),
        settings: Some(obj(json!({"reasoningEffort": "high", "timeout": 5}))),
        headers: Some([("X-Test".to_string(), "variant".to_string())].into_iter().collect()),
        body: Some(obj(json!({"custom_extension": {"level": 2}}))),
    }];
    let selected = with_variant(&m, Some("high")).unwrap();
    assert_eq!(serde_json::to_value(&selected.settings).unwrap(), json!({"reasoningEffort": "high"}));
    assert_eq!(serde_json::to_value(&selected.headers).unwrap(), json!({"X-Test": "variant"}));
    assert_eq!(serde_json::to_value(&selected.body).unwrap(), json!({"custom_extension": {"enabled": true, "level": 2}}));
    assert_eq!(with_variant(&m, Some("default")).unwrap(), m);
    assert_eq!(with_variant(&m, None).unwrap(), m);
    let failure = with_variant(&m, Some("missing")).unwrap_err();
    assert_eq!(failure.to_string(), "Variant unavailable for test-provider/test-model: missing");
    assert_eq!(failure.tag(), "SessionRunnerModel.VariantUnavailableError");
}

#[tokio::test]
async fn keyless_auth_for_explicitly_enabled_providers() {
    let loader = Arc::new(FakeLoader::default());
    let opts = options(&loader);
    for package in [
        "aisdk:@ai-sdk/google",
        "aisdk:@ai-sdk/mistral",
        "@opencode/ai/providers/mistral",
        "@opencode/ai/providers/baseten",
        "@opencode/ai/providers/cloudflare-ai-gateway",
        "@opencode/ai/providers/cloudflare-workers-ai",
        "@opencode/ai/providers/deepseek",
        "@opencode/ai/providers/fireworks",
    ] {
        let mut selected = model(package, json!({"baseURL": "https://gateway.example.com/v1"}));
        selected.provider_id = "gateway".into();
        let mut provider = ProviderInfo::empty("gateway");
        provider.activation = Activation::Enabled;
        provider.package = selected.package.clone().unwrap();
        let resolved = resolve_model(Some(&provider), &selected, None, None, &opts).unwrap();
        let headers = authorization(&resolved.language_model).await.unwrap();
        assert!(headers.get("authorization").is_none(), "{package}");
        assert_eq!(resolved.limit().output, 20);
        assert_eq!(resolved.model_ref().provider_id, "gateway");
    }
    // Auto providers keep strict auth.
    let selected = model("@opencode/ai/providers/google", json!({}));
    let provider = ProviderInfo { package: "@opencode/ai/providers/google".into(), ..ProviderInfo::empty("test-provider") };
    let resolved = resolve_model(Some(&provider), &selected, None, None, &opts).unwrap();
    assert!(authorization(&resolved.language_model).await.is_err());
    // A configured key keeps package auth.
    let selected = model("@opencode/ai/providers/openai", json!({"apiKey": "k"}));
    let provider = ProviderInfo { activation: Activation::Enabled, ..ProviderInfo::empty("test-provider") };
    let resolved = resolve_model(Some(&provider), &selected, None, None, &opts).unwrap();
    assert_eq!(authorization(&resolved.language_model).await.unwrap()["authorization"], "Bearer k");
}

#[test]
fn resolve_model_merges_provider_settings_and_reports_policies() {
    let loader = Arc::new(FakeLoader::default());
    let mut provider = ProviderInfo::empty("test-provider");
    provider.settings = Some(obj(json!({"chunkTimeout": 1234, "transport": "websocket", "baseURL": "https://p.example", "compaction": {"type": "summary"}})));
    let selected = model("@opencode/ai/providers/openai", json!({"reasoningEffort": "low"}));
    let resolved = resolve_model(Some(&provider), &selected, Some("default"), None, &options(&loader)).unwrap();
    let settings = loader.last().2;
    assert_eq!(settings["baseURL"], "https://p.example");
    assert_eq!(settings["reasoningEffort"], "low");
    assert!(settings.get("transport").is_none());
    assert_eq!(resolved.transport.as_deref(), Some("websocket"));
    assert_eq!(resolved.chunk_timeout, Some(HttpTimeout::Millis(1234.0)));
    assert_eq!(resolved.compaction.as_deref(), Some("summary"));
    assert_eq!(resolved.variant.as_deref(), Some("default"));
    assert_eq!(resolved.language_model.defaults.unwrap().http.unwrap().chunk_timeout, Some(HttpTimeout::Millis(1234.0)));
}

#[test]
fn cost_helpers() {
    let loader = Arc::new(FakeLoader::default());
    let mut selected = model("@opencode/ai/providers/openai", json!({}));
    let cost = |tier: Option<i64>, input: f64| Cost { tier: tier.map(CostTier::context), input, output: 0.0, cache: CostCache::default() };
    selected.cost = vec![cost(None, 1.0), cost(Some(272_000), 3.0), cost(Some(200_000), 2.0)];
    let resolved = resolve_model(None, &selected, None, None, &options(&loader)).unwrap();
    assert_eq!(resolved.base_cost().unwrap().input, 1.0);
    assert_eq!(resolved.cost_for_context(100).unwrap().input, 1.0);
    assert_eq!(resolved.cost_for_context(250_000).unwrap().input, 2.0);
    assert_eq!(resolved.cost_for_context(300_000).unwrap().input, 3.0);
    assert_eq!(resolved.context_limit(), 100);
    assert_eq!(resolved.provider.id, "test-provider");
}

// ---------------------------------------------------------------------------------------------
// Against the real bundled snapshot
// ---------------------------------------------------------------------------------------------

struct Enable(&'static str);
impl CatalogPlugin for Enable {
    fn providers(&mut self, editor: &mut ProviderEditor) {
        editor.update(self.0, |p| p.activation = Activation::Enabled);
    }
}

#[test]
fn resolves_snapshot_models_with_variants() {
    let data = bundled();
    let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![Box::new(ModelsDevPlugin::new(&data))];
    let access = Access::new().integration("anthropic", vec!["key-1".into()]).integration("openai", vec![]);
    let catalog = Catalog::build(&mut plugins, &access);
    assert!(catalog.is_available("anthropic"));
    assert!(!catalog.is_available("openai"));
    assert_eq!(catalog.integration_id("anthropic"), "anthropic");

    let model = catalog
        .provider_models("anthropic")
        .into_iter()
        .find(|m| m.variants.iter().any(|v| v.id == "high"))
        .expect("anthropic model with a high variant")
        .clone();
    let loader = Arc::new(FakeLoader::default());
    let credential = CredentialValue::Key { key: "sk-ant".into(), metadata: None };
    let resolved = catalog
        .resolve_with("anthropic", &model.id, Some("high"), Some(&credential), &options(&loader))
        .unwrap();
    let (specifier, model_id, settings) = loader.last();
    assert_eq!(specifier, "@opencode/ai/providers/anthropic");
    assert_eq!(model_id, model.model_id);
    assert_eq!(settings["apiKey"], "sk-ant");
    assert!(settings.contains_key("thinking"));
    assert_eq!(resolved.model_ref().variant.as_deref(), Some("high"));
    assert_eq!(resolved.provider.id, "anthropic");
    assert_eq!(resolved.language_model.provider, "anthropic");

    // Default selection falls back to the newest available text model with a package.
    let selected = catalog.select(None).unwrap();
    assert_eq!(selected.provider_id, "anthropic");
    assert!(catalog.select(Some(("openai", "gpt-5"))).is_none());
    let failure = catalog.resolve_with("anthropic", "nope", None, None, &options(&loader)).unwrap_err();
    assert_eq!(failure.to_string(), "Model not found: anthropic/nope");

    // Explicitly enabled without a connection.
    let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![Box::new(ModelsDevPlugin::new(&data)), Box::new(Enable("openai"))];
    let catalog = Catalog::build(&mut plugins, &Access::new().integration("openai", vec![]));
    assert!(catalog.is_available("openai"));
    assert!(catalog.provider_models("openai").iter().all(|m| m.package.as_deref().is_some()));
}
