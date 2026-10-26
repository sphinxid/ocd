//! Port of `packages/core/test/models.test.ts` and `packages/core/test/plugin/models-dev.test.ts`.

use async_trait::async_trait;
use oc_ai::catalog::models_dev::{
    body_digest, bundled, cache_key, decode_catalog, filter_snapshots, integrations, normalize, released, CatalogCache,
    HttpGet, HttpResponse, IntegrationMethod, MemoryCache, ModelsDev, ModelsDevOptions, Snapshot, DEFAULT_CACHE_KEY,
};
use oc_ai::catalog::plugins::ModelsDevPlugin;
use oc_ai::catalog::registry::{Access, Catalog, CatalogPlugin, ProviderEditor};
use oc_ai::catalog::types::Activation;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;

fn fixture() -> Value {
    json!({
        "acme": {
            "id": "acme", "name": "Acme", "env": ["ACME_API_KEY"], "npm": "@ai-sdk/openai-compatible",
            "models": {
                "acme-1": {
                    "id": "acme-1", "name": "Acme One", "release_date": "2026-01-01", "attachment": false, "reasoning": false,
                    "temperature": true, "tool_call": true, "interleaved": {"field": "vendor_reasoning"},
                    "limit": {"context": 128000, "output": 8192}
                }
            }
        }
    })
}

fn fixture_snapshot() -> Value {
    json!([{
        "info": {"id": "acme", "name": "Acme", "activation": "auto", "package": "@opencode/ai/providers/openai-compatible"},
        "models": [{
            "id": "acme-1", "modelID": "acme-1", "providerID": "acme", "name": "Acme One",
            "compatibility": {"reasoningField": "vendor_reasoning"},
            "settings": {"provider": "acme"},
            "capabilities": {"tools": true, "input": [], "output": [], "temperature": true},
            "variants": [], "time": {"released": 1767225600000i64},
            "cost": [{"input": 0, "output": 0, "cache": {"read": 0, "write": 0}}],
            "status": "active", "enabled": true, "limit": {"context": 128000, "output": 8192}
        }],
        "environment": ["ACME_API_KEY"]
    }])
}

fn fixture2() -> Value {
    json!({
        "beta": {
            "id": "beta", "name": "Beta", "env": ["BETA_API_KEY"], "npm": "@ai-sdk/openai-compatible",
            "models": {
                "beta-1": {
                    "id": "beta-1", "name": "Beta One", "release_date": "2026-02-01", "attachment": false, "reasoning": true,
                    "temperature": false, "tool_call": false, "limit": {"context": 64000, "output": 4096}
                }
            }
        }
    })
}

fn fixture2_snapshot() -> Value {
    json!([{
        "info": {"id": "beta", "name": "Beta", "activation": "auto", "package": "@opencode/ai/providers/openai-compatible"},
        "models": [{
            "id": "beta-1", "modelID": "beta-1", "providerID": "beta", "name": "Beta One",
            "settings": {"provider": "beta"},
            "capabilities": {"tools": false, "input": [], "output": [], "temperature": false},
            "variants": [], "time": {"released": 1769904000000i64},
            "cost": [{"input": 0, "output": 0, "cache": {"read": 0, "write": 0}}],
            "status": "active", "enabled": true, "limit": {"context": 64000, "output": 4096}
        }],
        "environment": ["BETA_API_KEY"]
    }])
}

fn to_json(data: &[Snapshot]) -> Value {
    Value::Array(
        data.iter()
            .map(|s| json!({"info": s.info, "models": s.models, "environment": s.environment}))
            .collect(),
    )
}

struct MockHttp {
    status: Mutex<u16>,
    body: Mutex<String>,
    calls: Mutex<Vec<(String, String)>>,
}

impl MockHttp {
    fn new(body: &Value, status: u16) -> Arc<Self> {
        Arc::new(Self { status: Mutex::new(status), body: Mutex::new(body.to_string()), calls: Mutex::new(vec![]) })
    }
    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().clone()
    }
}

#[async_trait]
impl HttpGet for MockHttp {
    async fn get(&self, url: &str, user_agent: &str) -> Result<HttpResponse, String> {
        self.calls.lock().push((url.to_string(), user_agent.to_string()));
        Ok(HttpResponse { status: *self.status.lock(), body: self.body.lock().clone() })
    }
}

struct FailingWrite(MemoryCache);

#[async_trait]
impl CatalogCache for FailingWrite {
    async fn get(&self, key: &str) -> Option<Value> {
        self.0.get(key).await
    }
    async fn set(&self, _: &str, _: Value) -> Result<(), String> {
        Err("Failed query: insert into \"kv\"".into())
    }
    async fn remove(&self, key: &str) {
        self.0.remove(key).await
    }
}

fn options(fetch: bool, snapshot: bool) -> ModelsDevOptions {
    ModelsDevOptions { fetch, snapshot, user_agent: "opencode/test/1.0.0/opencode".into(), ..Default::default() }
}

fn now() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64
}

async fn write_cache_text(cache: &MemoryCache, text: &str, updated_at: f64) {
    cache.set(DEFAULT_CACHE_KEY, json!({"updatedAt": updated_at, "digest": body_digest(text), "body": text})).await.unwrap();
}

async fn write_cache(cache: &MemoryCache, data: &Value, updated_at: f64) {
    write_cache_text(cache, &data.to_string(), updated_at).await
}

#[test]
fn normalizes_fixture() {
    let catalog = fixture();
    assert_eq!(to_json(&normalize(catalog.as_object().unwrap())), fixture_snapshot());
}

#[test]
fn cache_keys_and_dates() {
    assert_eq!(cache_key("https://models.opencode.ai"), "models-dev:catalog");
    assert_eq!(cache_key("https://example.com"), format!("models-dev:catalog:{}", oc_util::hash::fast("https://example.com")));
    assert_eq!(released("2026-01-01"), 1767225600000.0);
    assert_eq!(released("2025-02"), 1738368000000.0);
    assert_eq!(released("bogus"), 0.0);
}

#[tokio::test]
async fn get_returns_normalized_snapshots_from_cache() {
    let cache = Arc::new(MemoryCache::default());
    write_cache(&cache, &fixture(), now()).await;
    let http = MockHttp::new(&fixture(), 200);
    let service = ModelsDev::new(options(false, true), cache.clone(), http.clone());
    assert_eq!(to_json(&service.get().await.unwrap()), fixture_snapshot());
    assert!(http.calls().is_empty());
}

#[tokio::test]
async fn maps_models_dev_npm_packages_onto_native_packages() {
    let cache = Arc::new(MemoryCache::default());
    let base_model = fixture()["acme"]["models"]["acme-1"].clone();
    let mut acme = fixture()["acme"].clone();
    acme["models"]["acme-1"]["provider"] = json!({"npm": "@ai-sdk/openai"});
    let mut gpt5 = base_model.clone();
    gpt5["id"] = json!("gpt-5");
    let mut custom = base_model.clone();
    custom["id"] = json!("custom-chat");
    custom["provider"] = json!({"shape": "completions"});
    let data = json!({
        "acme": acme,
        "cloudflare-workers-ai": {
            "id": "cloudflare-workers-ai", "name": "Cloudflare Workers AI", "env": ["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_API_KEY"],
            "npm": "@ai-sdk/openai-compatible", "api": "https://api.cloudflare.com/client/v4/accounts/${CLOUDFLARE_ACCOUNT_ID}/ai/v1", "models": {}
        },
        "azure": {"id": "azure", "name": "Azure", "env": ["AZURE_RESOURCE_NAME", "AZURE_API_KEY"], "npm": "@ai-sdk/azure", "models": {"gpt-5": gpt5, "custom-chat": custom}}
    });
    write_cache(&cache, &data, now()).await;
    let service = ModelsDev::new(options(false, true), cache, MockHttp::new(&fixture(), 200));
    let result = service.get().await.unwrap();
    assert_eq!(result[0].info.package, "@opencode/ai/providers/openai-compatible");
    assert_eq!(result[0].models[0].package.as_deref(), Some("@opencode/ai/providers/openai"));
    assert_eq!(result[1].info.package, "@opencode/ai/providers/cloudflare-workers-ai");
    assert!(result[1].info.settings.is_none());
    assert_eq!(result[2].info.package, "@opencode/ai/providers/azure/responses");
    assert_eq!(result[2].models.iter().find(|m| m.id == "gpt-5").unwrap().package, None);
    assert_eq!(
        result[2].models.iter().find(|m| m.id == "custom-chat").unwrap().package.as_deref(),
        Some("@opencode/ai/providers/azure/chat")
    );
}

#[tokio::test]
async fn get_returns_empty_without_cache_fetch_or_snapshot() {
    let http = MockHttp::new(&fixture(), 200);
    let service = ModelsDev::new(options(false, false), Arc::new(MemoryCache::default()), http.clone());
    assert!(service.get().await.unwrap().is_empty());
    assert!(http.calls().is_empty());
}

#[tokio::test]
async fn get_falls_back_to_bundled_snapshot() {
    let http = MockHttp::new(&fixture(), 200);
    let service = ModelsDev::new(options(false, true), Arc::new(MemoryCache::default()), http.clone());
    let result = service.get().await.unwrap();
    assert!(result.len() >= 100);
    let anthropic = result.iter().find(|s| s.info.id == "anthropic").unwrap();
    assert!(anthropic.environment.contains(&"ANTHROPIC_API_KEY".to_string()));
    assert_eq!(anthropic.info.package, "@opencode/ai/providers/anthropic");
    assert!(http.calls().is_empty());
}

#[tokio::test]
async fn get_recovers_from_corrupted_cache_by_fetching() {
    let cache = Arc::new(MemoryCache::default());
    write_cache_text(&cache, "{", now()).await;
    let http = MockHttp::new(&fixture2(), 200);
    let service = ModelsDev::new(options(true, false), cache.clone(), http.clone());
    assert_eq!(to_json(&service.get().await.unwrap()), fixture2_snapshot());
    let stored = cache.get(DEFAULT_CACHE_KEY).await.unwrap();
    assert_eq!(stored["body"], json!(fixture2().to_string()));
    assert_eq!(stored["digest"], json!(body_digest(&fixture2().to_string())));
    let keys: Vec<&String> = stored.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["updatedAt", "digest", "body"]);
    assert_eq!(http.calls().len(), 1);
}

#[tokio::test]
async fn get_populates_when_cache_write_fails() {
    let http = MockHttp::new(&fixture2(), 200);
    let cache = Arc::new(FailingWrite(MemoryCache::default()));
    let service = ModelsDev::new(options(true, false), cache.clone(), http.clone());
    assert_eq!(to_json(&service.get().await.unwrap()), fixture2_snapshot());
    assert!(cache.get(DEFAULT_CACHE_KEY).await.is_none());
    assert_eq!(http.calls().len(), 1);
}

#[tokio::test]
async fn uses_default_url_when_configured_url_is_empty() {
    let http = MockHttp::new(&fixture(), 200);
    let mut opts = options(true, false);
    opts.url = Some(String::new());
    let service = ModelsDev::new(opts, Arc::new(MemoryCache::default()), http.clone());
    service.get().await.unwrap();
    assert_eq!(http.calls()[0].0, "https://models.opencode.ai/api.json");
    assert_eq!(http.calls()[0].1, "opencode/test/1.0.0/opencode");
}

#[tokio::test]
async fn get_is_single_flight() {
    let http = MockHttp::new(&fixture(), 200);
    let service = Arc::new(ModelsDev::new(options(true, false), Arc::new(MemoryCache::default()), http.clone()));
    let results = futures::future::join_all((0..5).map(|_| {
        let s = service.clone();
        async move { s.get().await.unwrap() }
    }))
    .await;
    for result in results {
        assert_eq!(to_json(&result), fixture_snapshot());
    }
    assert_eq!(http.calls().len(), 1);
}

#[tokio::test]
async fn get_caches_until_invalidate() {
    let cache = Arc::new(MemoryCache::default());
    write_cache(&cache, &fixture(), now()).await;
    let service = ModelsDev::new(options(false, true), cache.clone(), MockHttp::new(&fixture(), 200));
    let a = service.get().await.unwrap();
    write_cache(&cache, &fixture2(), now()).await;
    let b = service.get().await.unwrap();
    assert_eq!(to_json(&a), fixture_snapshot());
    assert_eq!(to_json(&b), fixture_snapshot());
    service.invalidate();
    assert_eq!(to_json(&service.get().await.unwrap()), fixture2_snapshot());
}

#[tokio::test]
async fn refresh_force_fetches_and_updates_cache() {
    let cache = Arc::new(MemoryCache::default());
    write_cache(&cache, &fixture(), now()).await;
    let http = MockHttp::new(&fixture2(), 200);
    let service = ModelsDev::new(options(false, true), cache.clone(), http.clone());
    let before = service.get().await.unwrap();
    assert!(service.refresh(true).await);
    let after = service.get().await.unwrap();
    assert_eq!(to_json(&before), fixture_snapshot());
    assert_eq!(to_json(&after), fixture2_snapshot());
    assert_eq!(cache.get(DEFAULT_CACHE_KEY).await.unwrap()["digest"], json!(body_digest(&fixture2().to_string())));
    assert_eq!(http.calls().len(), 1);
    assert!(http.calls()[0].0.contains("/api.json"));
    assert!(http.calls()[0].1.starts_with("opencode/"));
}

#[tokio::test]
async fn refresh_skips_fresh_cache() {
    let cache = Arc::new(MemoryCache::default());
    write_cache(&cache, &fixture(), now() - 1000.0).await;
    let http = MockHttp::new(&fixture2(), 200);
    let service = ModelsDev::new(options(false, true), cache, http.clone());
    assert!(!service.refresh(false).await);
    assert!(http.calls().is_empty());
}

#[tokio::test]
async fn refresh_fetches_stale_cache_and_publishes() {
    let cache = Arc::new(MemoryCache::default());
    write_cache(&cache, &fixture(), now() - 10.0 * 60.0 * 1000.0).await;
    let http = MockHttp::new(&fixture2(), 200);
    let service = ModelsDev::new(options(false, true), cache.clone(), http.clone());
    let mut events = service.subscribe();
    assert!(service.refresh(false).await);
    assert!(events.try_recv().is_ok());
    assert_eq!(http.calls().len(), 1);
    assert_eq!(to_json(&service.get().await.unwrap()), fixture2_snapshot());
    assert_eq!(cache.get(DEFAULT_CACHE_KEY).await.unwrap()["body"], json!(fixture2().to_string()));
}

#[tokio::test]
async fn refresh_stays_quiet_for_identical_digest() {
    let cache = Arc::new(MemoryCache::default());
    write_cache(&cache, &fixture(), now() - 10.0 * 60.0 * 1000.0).await;
    let seeded = cache.get(DEFAULT_CACHE_KEY).await.unwrap();
    let http = MockHttp::new(&fixture(), 200);
    let service = ModelsDev::new(options(false, true), cache.clone(), http.clone());
    let mut events = service.subscribe();
    assert!(!service.refresh(false).await);
    assert!(events.try_recv().is_err());
    assert_eq!(http.calls().len(), 1);
    assert_eq!(cache.get(DEFAULT_CACHE_KEY).await.unwrap(), seeded);
}

#[tokio::test]
async fn refresh_republishes_legacy_entries_without_digest() {
    let cache = Arc::new(MemoryCache::default());
    cache
        .set(DEFAULT_CACHE_KEY, json!({"updatedAt": now() - 10.0 * 60.0 * 1000.0, "body": fixture().to_string()}))
        .await
        .unwrap();
    let service = ModelsDev::new(options(false, true), cache.clone(), MockHttp::new(&fixture(), 200));
    let mut events = service.subscribe();
    assert!(service.refresh(false).await);
    assert!(events.try_recv().is_ok());
    assert_eq!(cache.get(DEFAULT_CACHE_KEY).await.unwrap()["digest"], json!(body_digest(&fixture().to_string())));
}

#[tokio::test]
async fn refresh_swallows_http_errors_after_retries() {
    let cache = Arc::new(MemoryCache::default());
    write_cache(&cache, &fixture(), now()).await;
    let http = MockHttp::new(&json!("boom"), 500);
    let service = ModelsDev::new(options(false, true), cache.clone(), http.clone());
    assert!(!service.refresh(true).await);
    assert_eq!(to_json(&service.get().await.unwrap()), fixture_snapshot());
    assert_eq!(http.calls().len(), 3);
}

#[tokio::test]
async fn reads_local_file_override() {
    let dir = std::env::temp_dir().join(format!("oc-ai-catalog-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("api.json");
    std::fs::write(&file, fixture2().to_string()).unwrap();
    let cache = Arc::new(MemoryCache::default());
    write_cache(&cache, &fixture(), now()).await;
    let mut opts = options(false, true);
    opts.file = Some(file);
    let service = ModelsDev::new(opts, cache, MockHttp::new(&fixture(), 200));
    assert_eq!(to_json(&service.get().await.unwrap()), fixture2_snapshot());
}

// ---------------------------------------------------------------------------------------------
// Experimental modes, costs, filtering
// ---------------------------------------------------------------------------------------------

#[test]
fn experimental_modes_add_models_with_merged_costs_and_overlays() {
    let data = json!({
        "acme": {
            "id": "acme", "name": "Acme", "env": [], "npm": "@ai-sdk/openai", "api": "https://api.acme.test/v1",
            "models": {
                "gpt-5.4": {
                    "id": "gpt-5.4", "name": "GPT-5.4", "family": "gpt", "release_date": "2026-01-01", "attachment": false,
                    "reasoning": true, "tool_call": true,
                    "reasoning_options": [{"type": "effort", "values": ["low", null, "null", "high"]}],
                    "cost": {"input": 2.5, "output": 15, "tiers": [
                        {"input": 3, "output": 18, "cache_read": 0.25, "tier": {"type": "context", "size": 272000}},
                        {"input": 5, "output": 22.5, "cache_read": 0.5, "tier": {"type": "context", "size": 200000}}
                    ]},
                    "limit": {"context": 1050000, "input": 922000, "output": 128000},
                    "experimental": {"modes": {"fast": {
                        "cost": {"input": 5, "output": 30, "cache_read": 0.5},
                        "provider": {"headers": {"x-mode": "fast"}, "body": {"service_tier": "priority"}}
                    }}}
                }
            }
        }
    });
    let result = normalize(data.as_object().unwrap());
    let models = &result[0].models;
    assert_eq!(result[0].info.settings.as_ref().unwrap()["baseURL"], "https://api.acme.test/v1");
    assert_eq!(models.len(), 2);
    let base = &models[0];
    assert_eq!(base.variants.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), ["low", "high"]);
    assert!(base.body.is_none());
    let fast = &models[1];
    assert_eq!(fast.id, "gpt-5.4-fast");
    assert_eq!(fast.model_id, "gpt-5.4");
    assert_eq!(fast.name, "GPT-5.4 Fast");
    assert_eq!(serde_json::to_value(&fast.headers).unwrap(), json!({"x-mode": "fast"}));
    assert_eq!(serde_json::to_value(&fast.body).unwrap(), json!({"service_tier": "priority"}));
    assert_eq!(fast.variants, base.variants);
    assert_eq!(
        serde_json::to_value(&fast.cost).unwrap(),
        json!([
            {"input": 5, "output": 30, "cache": {"read": 0.5, "write": 0}},
            {"tier": {"type": "context", "size": 272000}, "input": 3, "output": 18, "cache": {"read": 0.25, "write": 0}},
            {"tier": {"type": "context", "size": 200000}, "input": 5, "output": 22.5, "cache": {"read": 0.5, "write": 0}},
        ])
    );
    assert_eq!(serde_json::to_value(&base.cost).unwrap()[0], json!({"input": 2.5, "output": 15, "cache": {"read": 0, "write": 0}}));
}

#[test]
fn filters_aliases_deprecated_non_text_and_bedrock_profile_models() {
    let model = |id: &str, extra: Value| {
        let mut m = json!({"id": id, "name": id, "release_date": "2026-01-01", "attachment": false, "reasoning": false, "tool_call": true, "limit": {"context": 1, "output": 1}});
        for (k, v) in extra.as_object().unwrap() {
            m[k] = v.clone();
        }
        m
    };
    let data = json!({
        "acme": {"id": "acme", "name": "Acme", "env": [], "npm": "@ai-sdk/openai-compatible", "models": {
            "current": model("current", json!({})),
            "legacy": model("legacy", json!({"status": "deprecated"})),
            "video-gen": model("video-gen", json!({"modalities": {"input": ["text"], "output": ["video"]}})),
            "transcribe": model("transcribe", json!({"modalities": {"input": ["audio"], "output": ["text"]}})),
        }},
        "azure-cognitive-services": {"id": "azure-cognitive-services", "name": "x", "env": [], "npm": "@ai-sdk/azure", "models": {}},
        "google-vertex-anthropic": {"id": "google-vertex-anthropic", "name": "x", "env": [], "npm": "@ai-sdk/google-vertex/anthropic", "models": {}},
        "amazon-bedrock": {"id": "amazon-bedrock", "name": "Bedrock", "env": ["AWS_BEARER_TOKEN_BEDROCK"], "npm": "@ai-sdk/amazon-bedrock", "models": {
            "deepseek.r1-v1:0": model("deepseek.r1-v1:0", json!({})),
            "anthropic.claude-sonnet-4-6": model("anthropic.claude-sonnet-4-6", json!({})),
        }},
    });
    let filtered = filter_snapshots(&normalize(data.as_object().unwrap()));
    assert_eq!(filtered.iter().map(|s| s.info.id.as_str()).collect::<Vec<_>>(), ["acme", "amazon-bedrock"]);
    assert_eq!(filtered[0].models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["current"]);
    assert_eq!(filtered[1].models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["anthropic.claude-sonnet-4-6"]);
    let specs = integrations(&filtered);
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].id, "amazon-bedrock");
    assert_eq!(
        specs[0].methods,
        [IntegrationMethod::Key { label: None }, IntegrationMethod::Env { names: vec!["AWS_BEARER_TOKEN_BEDROCK".into()] }]
    );
}

#[test]
fn preserves_url_templates_and_proto_keys() {
    let data = json!({
        "acme": {"id": "acme", "name": "Acme", "env": [], "npm": "@ai-sdk/openai-compatible", "api": "https://${ACME_HOST}/v1", "models": {
            "m": {"id": "m", "name": "M", "release_date": "", "tool_call": true, "limit": {"context": 1, "output": 1},
                  "provider": {"api": "https://${ACME_HOST}/${ACME_MODEL_PATH}/v1"},
                  "experimental": {"modes": {"x": {"provider": {"body": serde_json::from_str::<Value>(r#"{"__proto__":{"service_tier":"priority"},"keep":true}"#).unwrap()}}}}}
        }}
    });
    let result = normalize(data.as_object().unwrap());
    assert_eq!(result[0].info.settings.as_ref().unwrap()["baseURL"], "https://${ACME_HOST}/v1");
    assert_eq!(result[0].models[0].settings.as_ref().unwrap()["baseURL"], "https://${ACME_HOST}/${ACME_MODEL_PATH}/v1");
    assert_eq!(result[0].models[0].time.released, 0.0);
    let body = result[0].models[1].body.as_ref().unwrap();
    assert_eq!(body.keys().collect::<Vec<_>>(), ["__proto__", "keep"]);
}

// ---------------------------------------------------------------------------------------------
// Reasoning fixture through the catalog (`converts reasoning options into settings variants`)
// ---------------------------------------------------------------------------------------------

struct Activate;
impl CatalogPlugin for Activate {
    fn providers(&mut self, editor: &mut ProviderEditor) {
        for id in editor.ids() {
            editor.update(&id, |p| p.activation = Activation::Enabled);
        }
    }
}

fn reasoning_catalog() -> Catalog {
    let text = include_str!("fixtures/catalog/models-dev-reasoning.json");
    let data = normalize(&decode_catalog(text).unwrap());
    let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![Box::new(ModelsDevPlugin::new(&data)), Box::new(Activate)];
    Catalog::build(&mut plugins, &Access::new())
}

fn variants(catalog: &Catalog, p: &str, m: &str) -> Value {
    serde_json::to_value(&catalog.model(p, m).unwrap_or_else(|| panic!("{p}/{m}")).variants).unwrap()
}

#[test]
fn converts_reasoning_options_into_settings_variants() {
    let c = reasoning_catalog();
    let responses = |e: &str| json!({"id": e, "settings": {"reasoningEffort": e, "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]}});
    assert_eq!(variants(&c, "openai", "gpt-reasoning"), json!([responses("low"), responses("high")]));
    let mode = c.model("openai", "gpt-reasoning-high").unwrap();
    assert_eq!(mode.name, "GPT Reasoning High");
    assert_eq!(serde_json::to_value(&mode.headers).unwrap(), json!({"x-mode": "high"}));
    assert_eq!(serde_json::to_value(&mode.body).unwrap(), json!({"service_tier": "priority"}));
    assert_eq!(variants(&c, "openai", "gpt-reasoning-high"), json!([responses("low"), responses("high")]));
    assert_eq!(serde_json::to_value(&c.model("openai", "gpt-reasoning-pro").unwrap().body).unwrap(), json!({"reasoning": {"mode": "pro"}}));

    let budget = variants(&c, "anthropic", "claude-budget");
    assert!(budget.as_array().unwrap().contains(&json!({"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 16000}}})));
    assert!(budget.as_array().unwrap().contains(&json!({"id": "max", "settings": {"thinking": {"type": "enabled", "budgetTokens": 31999}}})));
    assert_eq!(
        variants(&c, "anthropic", "claude-opus-4.7"),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "low", "settings": {"thinking": {"type": "adaptive", "display": "summarized"}, "effort": "low"}},
        ])
    );
    assert_eq!(
        variants(&c, "anthropic", "claude-toggle"),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "thinking", "settings": {"thinking": {"type": "adaptive", "display": "summarized"}}},
        ])
    );
    assert_eq!(
        variants(&c, "anthropic", "claude-opus-4-5"),
        json!([
            {"id": "low", "settings": {"effort": "low", "thinking": {"type": "enabled", "budgetTokens": 8191}}},
            {"id": "high", "settings": {"effort": "high", "thinking": {"type": "enabled", "budgetTokens": 8191}}},
        ])
    );
    assert_eq!(variants(&c, "xai", "grok-4.5"), json!([responses("low").clone(), responses("medium"), responses("high")]));
    assert_eq!(
        variants(&c, "opencode-go", "minimax-m3"),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "thinking", "settings": {"thinking": {"type": "adaptive", "display": "summarized"}}},
        ])
    );
    assert_eq!(
        variants(&c, "alibaba", "toggle-only"),
        json!([{"id": "none", "settings": {"enableThinking": false}}, {"id": "thinking", "settings": {"enableThinking": true}}])
    );
    assert_eq!(
        variants(&c, "alibaba", "toggle-budget"),
        json!([
            {"id": "none", "settings": {"enableThinking": false}},
            {"id": "high", "settings": {"enableThinking": true, "thinkingBudget": 8000}},
            {"id": "max", "settings": {"enableThinking": true, "thinkingBudget": 16000}},
        ])
    );
    assert_eq!(
        variants(&c, "vercel", "alibaba/qwen-toggle"),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 8000}}},
            {"id": "max", "settings": {"thinking": {"type": "enabled", "budgetTokens": 16000}}},
        ])
    );
    let gateway_effort = json!([
        {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
        {"id": "low", "settings": {"reasoningEffort": "low"}},
        {"id": "high", "settings": {"reasoningEffort": "high"}},
    ]);
    assert_eq!(variants(&c, "vercel", "amazon/nova-2-lite"), gateway_effort);
    assert_eq!(variants(&c, "vercel", "deepseek/deepseek-toggle"), gateway_effort);
    assert_eq!(
        variants(&c, "openrouter", "openrouter-toggle"),
        json!([{"id": "none", "settings": {"reasoning": {"enabled": false}}}, {"id": "thinking", "settings": {"reasoning": {"enabled": true}}}])
    );
    let gemini = json!([
        {"id": "none", "settings": {"thinkingConfig": {"includeThoughts": false, "thinkingBudget": 0}}},
        {"id": "high", "settings": {"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 8000}}},
        {"id": "max", "settings": {"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 16000}}},
    ]);
    assert_eq!(variants(&c, "google", "gemini-2.5-flash"), gemini);
    assert_eq!(variants(&c, "google-vertex", "gemini-2.5-flash-lite"), gemini);
    assert_eq!(
        variants(&c, "amazon-bedrock", "us.amazon.nova-2-lite-v1:0"),
        json!([
            {"id": "none", "body": {"additionalModelRequestFields": {"reasoningConfig": {"type": "disabled"}}}},
            {"id": "low", "body": {"additionalModelRequestFields": {"reasoningConfig": {"type": "enabled", "maxReasoningEffort": "low"}}}},
            {"id": "high", "body": {"additionalModelRequestFields": {"reasoningConfig": {"type": "enabled", "maxReasoningEffort": "high"}}}},
        ])
    );
    let sap = |v: Value| json!({"modelParams": v});
    assert_eq!(
        variants(&c, "sap-ai-core", "gemini-2.5-flash"),
        json!([
            {"id": "none", "settings": sap(json!({"thinkingConfig": {"includeThoughts": false, "thinkingBudget": 0}}))},
            {"id": "high", "settings": sap(json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 8000}}))},
            {"id": "max", "settings": sap(json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 16000}}))},
        ])
    );
    assert_eq!(
        variants(&c, "sap-ai-core", "amazon--nova-lite"),
        json!([
            {"id": "none", "settings": sap(json!({"additionalModelRequestFields": {"thinking": {"type": "disabled"}}}))},
            {"id": "low", "settings": sap(json!({"additionalModelRequestFields": {"output_config": {"effort": "low"}}}))},
            {"id": "high", "settings": sap(json!({"additionalModelRequestFields": {"output_config": {"effort": "high"}}}))},
        ])
    );
    assert_eq!(
        variants(&c, "sap-ai-core", "cohere--command-a-reasoning"),
        json!([
            {"id": "none", "settings": sap(json!({"thinking": {"type": "disabled"}}))},
            {"id": "low", "settings": sap(json!({"reasoning_effort": "low"}))},
            {"id": "high", "settings": sap(json!({"reasoning_effort": "high"}))},
        ])
    );
    assert_eq!(
        variants(&c, "sap-ai-core", "anthropic--claude-4.7-opus"),
        json!([{"id": "low", "settings": sap(json!({"additionalModelRequestFields": {"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "low"}}}))}])
    );
    assert_eq!(
        variants(&c, "sap-ai-core", "anthropic--claude-4-sonnet"),
        json!([
            {"id": "high", "settings": sap(json!({"additionalModelRequestFields": {"thinking": {"type": "enabled", "budget_tokens": 8000}}}))},
            {"id": "max", "settings": sap(json!({"additionalModelRequestFields": {"thinking": {"type": "enabled", "budget_tokens": 16000}}}))},
        ])
    );
}

// ---------------------------------------------------------------------------------------------
// Real bundled snapshot
// ---------------------------------------------------------------------------------------------

#[test]
fn bundled_snapshot_normalizes() {
    let data = bundled();
    assert!(data.len() >= 100, "providers: {}", data.len());
    let anthropic = data.iter().find(|s| s.info.id == "anthropic").unwrap();
    assert_eq!(anthropic.info.package, "@opencode/ai/providers/anthropic");
    assert!(anthropic.info.settings.is_none());
    let deepseek = data.iter().find(|s| s.info.id == "deepseek").unwrap();
    assert_eq!(deepseek.info.package, "@opencode/ai/providers/deepseek");
    assert_eq!(deepseek.info.settings.as_ref().and_then(|s| s.get("baseURL")).and_then(Value::as_str), Some("https://api.deepseek.com"));
    let openrouter = data.iter().find(|s| s.info.id == "openrouter").unwrap();
    assert_eq!(openrouter.info.package, "@opencode/ai/providers/openrouter");
    assert!(anthropic.models.iter().any(|m| !m.variants.is_empty()));
    for snapshot in data.iter() {
        for model in &snapshot.models {
            assert_eq!(model.provider_id, snapshot.info.id);
            assert!(!model.cost.is_empty());
        }
    }
    // Every Anthropic model with reasoning options gets Messages variants.
    let opus = anthropic.models.iter().find(|m| m.id.contains("opus") && !m.variants.is_empty()).unwrap();
    assert!(opus.variants.iter().all(|v| v.settings.as_ref().map(|s| s.contains_key("thinking")).unwrap_or(false)));
    // The Cloudflare Workers AI provider keeps its URL template out of provider settings.
    if let Some(workers) = data.iter().find(|s| s.info.id == "cloudflare-workers-ai") {
        assert_eq!(workers.info.package, "@opencode/ai/providers/cloudflare-workers-ai");
        assert!(workers.info.settings.is_none());
    }
    // Filtering drops the legacy aliases and keeps only text models.
    let filtered = filter_snapshots(&data);
    assert!(filtered.iter().all(|s| s.info.id != "azure-cognitive-services" && s.info.id != "google-vertex-anthropic"));
    assert!(filtered.iter().flat_map(|s| s.models.iter()).all(|m| m.status != oc_ai::catalog::ModelStatus::Deprecated));
    // Serialization round-trips.
    let encoded = serde_json::to_string(&anthropic.models).unwrap();
    let decoded: Vec<oc_ai::catalog::ModelInfo> = serde_json::from_str(&encoded).unwrap();
    assert_eq!(&decoded, &anthropic.models);
}
