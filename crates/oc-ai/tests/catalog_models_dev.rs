//! Port of `packages/core/test/models.test.ts` and `packages/core/test/plugin/models-dev.test.ts` (2.2.2:
//! models.dev v2 catalog).

use async_trait::async_trait;
use oc_ai::catalog::models_dev::{
    body_digest, bundled, cache_key, decode_catalog, decode_snapshot, filter_snapshots, integrations, normalize,
    released, CatalogCache, HttpGet, HttpResponse, IntegrationMethod, MemoryCache, ModelsDev, ModelsDevOptions, Snapshot,
    CATALOG_PATH, DEFAULT_CACHE_KEY,
};
use oc_ai::catalog::plugins::ModelsDevPlugin;
use oc_ai::catalog::registry::{Access, Catalog, CatalogPlugin, ProviderEditor};
use oc_ai::catalog::types::Activation;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;

const META_KEY: &str = "models-dev:v2:catalog:meta";

/// A chat model in the models.dev v2 shape, served over one OpenAI-compatible API.
fn source_model(id: &str, extra: Value) -> Value {
    let mut m = json!({
        "id": id, "type": "chat", "name": id, "description": "A model", "release_date": "2026-01-01",
        "modalities": {"input": ["text"], "output": ["text"]},
        "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": false}, "temperature": true},
        "limit": {"context": 128000, "output": 8192},
        "api": {"chat-completions": {
            "base_url": "https://api.example.test/v1",
            "sdk": {"aisdk": "@ai-sdk/openai-compatible", "opencodeai": "@opencode/ai/providers/openai-compatible"}
        }}
    });
    for (k, v) in extra.as_object().unwrap() {
        m[k] = v.clone();
    }
    m
}

fn fixture() -> Value {
    json!({
        "acme": {
            "id": "acme", "name": "Acme", "env": ["ACME_API_KEY"],
            "models": {
                "acme-1": source_model("acme-1", json!({
                    "name": "Acme One",
                    "api": {"chat-completions": {
                        "base_url": "https://api.acme.test/v1",
                        "sdk": {"aisdk": "@ai-sdk/openai-compatible", "opencodeai": "@opencode/ai/providers/openai-compatible"},
                        "compatibility": {"reasoning_field": "vendor_reasoning"}
                    }}
                }))
            }
        }
    })
}

fn zero_cost() -> Value {
    json!([{"input": 0, "output": 0, "cache": {"read": 0, "write": 0}}])
}

fn fixture_snapshot() -> Value {
    json!([{
        "info": {"id": "acme", "name": "Acme", "activation": "auto", "package": "@opencode/ai/providers/openai-compatible", "settings": {"baseURL": "https://api.acme.test/v1"}},
        "models": [{
            "id": "acme-1", "modelID": "acme-1", "providerID": "acme", "type": "chat", "name": "Acme One",
            "compatibility": {"reasoningField": "vendor_reasoning"},
            "settings": {"provider": "acme"},
            "capabilities": {"tools": true, "input": ["text"], "output": ["text"], "temperature": true},
            "variants": [], "time": {"released": 1767225600000i64},
            "cost": zero_cost(),
            "status": "active", "enabled": true, "limit": {"context": 128000, "output": 8192}
        }],
        "environment": ["ACME_API_KEY"]
    }])
}

fn fixture2() -> Value {
    json!({
        "beta": {
            "id": "beta", "name": "Beta", "env": ["BETA_API_KEY"],
            "models": {
                "beta-1": source_model("beta-1", json!({
                    "name": "Beta One", "release_date": "2026-02-01",
                    "capabilities": {"tools": {"supported": false}, "reasoning": {"supported": false}, "temperature": false},
                    "limit": {"context": 64000, "output": 4096}
                }))
            }
        }
    })
}

fn fixture2_snapshot() -> Value {
    json!([{
        "info": {"id": "beta", "name": "Beta", "activation": "auto", "package": "@opencode/ai/providers/openai-compatible", "settings": {"baseURL": "https://api.example.test/v1"}},
        "models": [{
            "id": "beta-1", "modelID": "beta-1", "providerID": "beta", "type": "chat", "name": "Beta One",
            "settings": {"provider": "beta"},
            "capabilities": {"tools": false, "input": ["text"], "output": ["text"], "temperature": false},
            "variants": [], "time": {"released": 1769904000000i64},
            "cost": zero_cost(),
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

#[derive(Debug, Clone, PartialEq)]
struct Call {
    url: String,
    user_agent: String,
    if_none_match: Option<String>,
}

struct MockHttp {
    status: Mutex<u16>,
    body: Mutex<String>,
    etag: Mutex<Option<String>>,
    calls: Mutex<Vec<Call>>,
}

impl MockHttp {
    fn new(body: &Value, status: u16) -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(status),
            body: Mutex::new(body.to_string()),
            etag: Mutex::new(None),
            calls: Mutex::new(vec![]),
        })
    }
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().clone()
    }
}

#[async_trait]
impl HttpGet for MockHttp {
    async fn get(&self, url: &str, user_agent: &str, if_none_match: Option<&str>) -> Result<HttpResponse, String> {
        self.calls.lock().push(Call {
            url: url.to_string(),
            user_agent: user_agent.to_string(),
            if_none_match: if_none_match.map(String::from),
        });
        let status = *self.status.lock();
        let body = if status == 304 { String::new() } else { self.body.lock().clone() };
        Ok(HttpResponse { status, body, etag: self.etag.lock().clone() })
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

async fn cached(data: &Value) -> Vec<Snapshot> {
    let cache = Arc::new(MemoryCache::default());
    write_cache(&cache, data, now()).await;
    let service = ModelsDev::new(options(false, true), cache, MockHttp::new(&fixture(), 200));
    service.get().await.unwrap().as_ref().clone()
}

#[test]
fn cache_keys_and_dates() {
    assert_eq!(cache_key("https://models.opencode.ai"), "models-dev:v2:catalog");
    assert_eq!(cache_key("https://example.com"), format!("models-dev:v2:catalog:{}", oc_util::hash::fast("https://example.com")));
    assert_eq!(CATALOG_PATH, "/experimental/v2.0/api.json?type=chat,decision,image");
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
async fn removes_the_unversioned_v1_cache_rows_at_startup() {
    let cache = Arc::new(MemoryCache::default());
    cache.set("models-dev:catalog", json!({"updatedAt": 1, "body": "{}"})).await.unwrap();
    cache.set("models-dev:catalog:meta", json!({"updatedAt": 1})).await.unwrap();
    let mirror = format!("models-dev:catalog:{}", oc_util::hash::fast("https://mirror.example"));
    cache.set(&mirror, json!({"updatedAt": 1, "body": "{}"})).await.unwrap();
    write_cache(&cache, &fixture(), now()).await;
    let service = ModelsDev::new(options(false, true), cache.clone(), MockHttp::new(&fixture(), 200));
    service.get().await.unwrap();
    assert!(cache.get("models-dev:catalog").await.is_none());
    assert!(cache.get("models-dev:catalog:meta").await.is_none());
    // Only the rows of this service's source go.
    assert!(cache.get(&mirror).await.is_some());
    assert!(cache.get(DEFAULT_CACHE_KEY).await.is_some());
    let mut opts = options(false, true);
    opts.url = Some("https://mirror.example".into());
    ModelsDev::new(opts, cache.clone(), MockHttp::new(&fixture(), 200)).refresh(false).await;
    assert!(cache.get(&mirror).await.is_none());
}

#[tokio::test]
async fn takes_each_models_package_from_its_first_api_and_skips_urls_packages_build_themselves() {
    let azure_api = |protocol: &str, opencodeai: &str| {
        json!({protocol: {"base_url": "https://${AZURE_RESOURCE_NAME}.openai.azure.com/openai/v1", "sdk": {"aisdk": "@ai-sdk/azure", "opencodeai": opencodeai}}})
    };
    let mut acme = fixture()["acme"].clone();
    acme["models"]["acme-responses"] = source_model("acme-responses", json!({"api": {
        "responses": {"base_url": "https://api.acme.test/v1", "sdk": {"aisdk": "@ai-sdk/openai", "opencodeai": "@opencode/ai/providers/openai/responses"}},
        "chat-completions": {"base_url": "https://other.acme.test/v1", "sdk": {"aisdk": "@ai-sdk/openai"}}
    }}));
    acme["models"]["acme-fallback"] = source_model("acme-fallback", json!({"api": {"messages": {"base_url": "https://claude.acme.test/v1", "sdk": {"aisdk": "@ai-sdk/anthropic"}}}}));
    let data = json!({
        "acme": acme,
        "azure": {"id": "azure", "name": "Azure", "env": ["AZURE_RESOURCE_NAME", "AZURE_API_KEY"], "models": {
            "gpt-5": source_model("gpt-5", json!({"api": azure_api("responses", "@opencode/ai/providers/azure/responses")})),
            "custom-chat": source_model("custom-chat", json!({"api": azure_api("chat-completions", "@opencode/ai/providers/azure/chat")})),
            "claude": source_model("claude", json!({"api": {"messages": {"base_url": "https://${AZURE_RESOURCE_NAME}.services.ai.azure.com/anthropic/v1", "sdk": {"aisdk": "@ai-sdk/anthropic", "opencodeai": "@opencode/ai/providers/anthropic-compatible"}}}})),
        }}
    });
    let result = cached(&data).await;
    let model = |p: usize, id: &str| result[p].models.iter().find(|m| m.id == id).unwrap().clone();
    assert_eq!(model(0, "acme-responses").package.as_deref(), Some("@opencode/ai/providers/openai/responses"));
    assert!(model(0, "acme-responses").settings.is_none());
    // Without an opencode package, the AI SDK package maps to the native one as before.
    assert_eq!(model(0, "acme-fallback").package.as_deref(), Some("@opencode/ai/providers/anthropic"));
    assert_eq!(Value::Object(model(0, "acme-fallback").settings.unwrap()), json!({"baseURL": "https://claude.acme.test/v1"}));
    assert_eq!(result[1].info.package, "@opencode/ai/providers/azure/responses");
    assert!(result[1].info.settings.is_none());
    assert_eq!(model(1, "gpt-5").package, None);
    assert!(model(1, "gpt-5").settings.is_none());
    assert_eq!(model(1, "custom-chat").package.as_deref(), Some("@opencode/ai/providers/azure/chat"));
    assert!(model(1, "custom-chat").settings.is_none());
    assert_eq!(
        Value::Object(model(1, "claude").settings.unwrap()),
        json!({"baseURL": "https://${AZURE_RESOURCE_NAME}.services.ai.azure.com/anthropic/v1", "provider": "azure"})
    );
}

#[tokio::test]
async fn keeps_non_chat_models_with_their_type_maps_mime_modalities_and_skips_malformed_models() {
    let mut acme = fixture()["acme"].clone();
    acme["models"]["acme-image"] = source_model("acme-image", json!({
        "type": "image",
        "modalities": {"input": ["text", "image/png", "application/pdf"], "output": ["image/png", "image/webp"]},
        "api": {
            "chat-completions": {"base_url": "https://api.example.test/v1", "sdk": {"aisdk": "@acme/ai-sdk-provider"}},
            "images": {"base_url": "https://api.example.test/v1", "sdk": {"aisdk": "@acme/ai-sdk-provider", "opencodeai": "@opencode/ai/providers/openai-compatible/images"}}
        },
        "experimental": {"modes": {"fast": {}}},
        "future_field": {"anything": true}
    }));
    acme["models"]["acme-broken"] = json!({"id": "acme-broken", "type": "chat"});
    acme["models"]["acme-sdk-image"] = source_model("acme-sdk-image", json!({"type": "image", "api": {"images": {"base_url": "https://api.example.test/v1", "sdk": {"aisdk": "@acme/ai-sdk-provider"}}}}));
    let result = cached(&json!({"acme": acme})).await;
    let models = &result[0].models;
    assert_eq!(
        models.iter().map(|m| (m.id.as_str(), m.kind.as_deref().unwrap())).collect::<Vec<_>>(),
        [("acme-1", "chat"), ("acme-image", "image"), ("acme-image-fast", "image")]
    );
    assert_eq!(models[1].capabilities.input, ["text", "image", "pdf"]);
    assert_eq!(models[1].capabilities.output, ["image"]);
    assert_eq!(models[1].package.as_deref(), Some("@opencode/ai/providers/openai-compatible/images"));
}

#[tokio::test]
async fn maps_per_token_text_prices_context_tiers_and_mode_multipliers_onto_cost() {
    let data = json!({"acme": {"id": "acme", "name": "Acme", "env": ["ACME_API_KEY"], "models": {
        "acme-1": source_model("acme-1", json!({
            "cost": {
                "text": {
                    "input": {"price": 2, "cache": {"read": 0.2, "write": {"5m": 2.5, "1h": 4}}, "overrides": [
                        {"when": {"context": {"gt": 200000}}, "price": 4, "cache": {"read": 0.4}},
                        {"when": {"service_tier": {"equals": "flex"}}, "price": 1}
                    ]},
                    "output": {"price": 10, "overrides": [{"when": {"context": {"gt": 200000}}, "price": 15}]}
                },
                "image": {"output": {"unit": "image", "price": 0.04}},
                "multipliers": [{"when": {"service_tier": {"equals": "priority"}}, "factor": {"text": 2}}]
            },
            "experimental": {"modes": {"fast": {"service_tier": "priority", "body": {"service_tier": "priority"}}}}
        }))
    }}});
    let result = cached(&data).await;
    let tier = json!({"tier": {"type": "context", "size": 200000}, "input": 4, "output": 15, "cache": {"read": 0.4, "write": 2.5}});
    assert_eq!(
        serde_json::to_value(&result[0].models[0].cost).unwrap(),
        json!([{"input": 2, "output": 10, "cache": {"read": 0.2, "write": 2.5}}, tier])
    );
    assert_eq!(
        serde_json::to_value(&result[0].models[1].cost).unwrap(),
        json!([{"input": 4, "output": 20, "cache": {"read": 0.4, "write": 5}}, tier])
    );
    assert_eq!(serde_json::to_value(&result[0].models[1].body).unwrap(), json!({"service_tier": "priority"}));
}

#[test]
fn skips_apis_whose_opencode_entrypoint_this_build_doesnt_have() {
    let unknown = |path: &str| json!({"base_url": "https://new.acme.test/v1", "sdk": {"opencodeai": format!("@opencode/ai/providers/acme/{path}")}});
    let image = |id: &str, api: Value| source_model(id, json!({"type": "image", "modalities": {"input": ["text"], "output": ["image/png"]}, "api": api}));
    let mut next_api = json!({"new-api": unknown("new-api")});
    for (k, v) in source_model("next", json!({}))["api"].as_object().unwrap() {
        next_api[k] = v.clone();
    }
    let mut acme = fixture()["acme"].clone();
    acme["models"] = json!({
        "next": source_model("next", json!({"api": next_api})),
        "sdk-fallback": source_model("sdk-fallback", json!({"api": {"new-api": {"base_url": "https://claude.acme.test/v1", "sdk": {"aisdk": "@ai-sdk/anthropic", "opencodeai": "@opencode/ai/providers/acme/new-api"}}}})),
        "unknown-only": source_model("unknown-only", json!({"api": {"new-api": unknown("new-api")}})),
        "image-unknown": image("image-unknown", json!({"images": unknown("images")})),
        "image-known": image("image-known", json!({"images": unknown("images"), "openai-images": {"base_url": "https://api.acme.test/v1", "sdk": {"opencodeai": "@opencode/ai/providers/openai-compatible/images"}}})),
    });
    let result = decode_snapshot(&json!({"acme": acme}).to_string()).unwrap();
    let acme = &result[0];
    assert_eq!(acme.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["next", "sdk-fallback", "image-known"]);
    assert_eq!(acme.info.package, "@opencode/ai/providers/openai-compatible");
    assert_eq!(acme.info.settings.as_ref().unwrap()["baseURL"], "https://api.example.test/v1");
    let model = |id: &str| acme.models.iter().find(|m| m.id == id).unwrap();
    assert_eq!(model("next").package, None);
    assert_eq!(Value::Object(model("next").settings.clone().unwrap()), json!({"provider": "acme"}));
    assert_eq!(model("sdk-fallback").package.as_deref(), Some("@opencode/ai/providers/anthropic"));
    assert_eq!(model("sdk-fallback").settings.as_ref().unwrap()["baseURL"], "https://claude.acme.test/v1");
    assert_eq!(model("image-known").package.as_deref(), Some("@opencode/ai/providers/openai-compatible/images"));
    assert_eq!(model("image-known").settings.as_ref().unwrap()["baseURL"], "https://api.acme.test/v1");
}

#[test]
fn skips_non_chat_models_whose_only_opencode_package_serves_another_type() {
    let api = |opencodeai: &str| json!({"chat-completions": {"base_url": "https://api.acme.test/v1", "sdk": {"opencodeai": opencodeai}}});
    let chat_package = "@opencode/ai/providers/openai-compatible";
    let mut image_api = api(chat_package);
    image_api["images"] = api("@opencode/ai/providers/openai-compatible/images")["chat-completions"].clone();
    let mut acme = fixture()["acme"].clone();
    acme["models"] = json!({
        "chat": source_model("chat", json!({"api": api(chat_package)})),
        "image-chat-only": source_model("image-chat-only", json!({"type": "image", "api": api(chat_package)})),
        "image": source_model("image", json!({"type": "image", "api": image_api})),
        "decision-chat-only": source_model("decision-chat-only", json!({"type": "decision", "api": api(chat_package)})),
        "decision-image-only": source_model("decision-image-only", json!({"type": "decision", "api": api("@opencode/ai/providers/openrouter/images")})),
        "decision": source_model("decision", json!({"type": "decision", "api": api("@opencode/ai/providers/openrouter/decisions")})),
    });
    let result = decode_snapshot(&json!({"acme": acme}).to_string()).unwrap();
    let acme = &result[0];
    assert_eq!(
        acme.models
            .iter()
            .map(|m| (m.id.as_str(), m.kind.as_deref().unwrap(), m.package.clone().unwrap_or_else(|| acme.info.package.clone())))
            .collect::<Vec<_>>(),
        [
            ("chat", "chat", chat_package.to_string()),
            ("image", "image", "@opencode/ai/providers/openai-compatible/images".to_string()),
            ("decision", "decision", "@opencode/ai/providers/openrouter/decisions".to_string()),
        ]
    );
}

#[tokio::test]
async fn drops_model_types_opencode_doesnt_keep() {
    let api = json!({"images": {"base_url": "https://api.acme.test/v1", "sdk": {"opencodeai": "@opencode/ai/providers/openai/images"}}});
    let model = |id: &str, kind: &str| {
        json!({"id": id, "type": kind, "name": id, "release_date": "2026-01-01", "modalities": {"input": ["text"], "output": ["image"]},
               "capabilities": {"tools": {"supported": false}, "reasoning": {"supported": false}}, "api": api})
    };
    let data = json!({"acme": {"id": "acme", "name": "Acme", "env": ["ACME_API_KEY"], "models": {
        "acme-image": model("acme-image", "image"), "acme-video": model("acme-video", "video")
    }}});
    let result = cached(&data).await;
    assert_eq!(result[0].models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["acme-image"]);
}

#[tokio::test]
async fn skips_api_entries_and_modes_it_cant_read_and_keeps_the_model() {
    let data = json!({"acme": {"id": "acme", "name": "Acme", "env": ["ACME_API_KEY"], "models": {
        "acme-1": {
            "id": "acme-1", "type": "chat", "name": "Acme One", "release_date": "2026-01-01",
            "modalities": {"input": ["text"], "output": ["text"]},
            "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": false}},
            "api": {
                "future": {"url": 42},
                "chat-completions": {"base_url": "https://api.acme.test/v1", "sdk": {"opencodeai": "@opencode/ai/providers/openai-compatible"}}
            },
            "experimental": {"modes": {"future": {"cost": "free"}, "fast": {"body": {"service_tier": "priority"}}}}
        }
    }}});
    let result = cached(&data).await;
    assert_eq!(result[0].info.package, "@opencode/ai/providers/openai-compatible");
    assert_eq!(result[0].info.settings.as_ref().unwrap()["baseURL"], "https://api.acme.test/v1");
    assert_eq!(result[0].models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["acme-1", "acme-1-fast"]);
}

#[tokio::test]
async fn reads_a_models_dev_v1_catalog_by_converting_it_to_v2() {
    let data = json!({"acme": {
        "id": "acme", "name": "Acme", "env": ["ACME_API_KEY"], "npm": "@ai-sdk/openai-compatible", "api": "https://api.acme.test/v1",
        "models": {"acme-1": {
            "id": "acme-1", "name": "Acme One", "release_date": "2026-01-01", "attachment": false, "reasoning": true,
            "reasoning_options": [{"type": "effort", "values": [null, "low", "high"]}, {"type": "budget_tokens", "min": 1024}],
            "temperature": true, "tool_call": true, "interleaved": {"field": "vendor_reasoning"},
            "cost": {"input": 1, "output": 2, "tiers": [{"tier": {"type": "context", "size": 200000}, "input": 3, "output": 4}]},
            "limit": {"context": 128000, "output": 8192}
        }}
    }});
    let result = cached(&data).await;
    let acme = &result[0];
    assert_eq!(acme.info.package, "@opencode/ai/providers/openai-compatible");
    assert_eq!(acme.info.settings.as_ref().unwrap()["baseURL"], "https://api.acme.test/v1");
    let model = &acme.models[0];
    assert_eq!(model.kind.as_deref(), Some("chat"));
    assert_eq!(model.compatibility.as_ref().unwrap().reasoning_field.as_deref(), Some("vendor_reasoning"));
    assert_eq!(model.capabilities.input, ["text"]);
    assert_eq!(model.capabilities.output, ["text"]);
    assert_eq!(model.capabilities.temperature, Some(true));
    assert!(model.capabilities.tools);
    assert_eq!(model.variants.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), ["low", "high"]);
    assert_eq!(
        model.cost.iter().map(|c| (c.tier.as_ref().map(|t| t.size), c.input, c.output)).collect::<Vec<_>>(),
        [(None, 1.0, 2.0), (Some(200000), 3.0, 4.0)]
    );
}

#[tokio::test]
async fn falls_back_to_api_json_when_a_custom_mirror_has_no_v2_catalog() {
    struct Mirror(Mutex<Vec<String>>);
    #[async_trait]
    impl HttpGet for Mirror {
        async fn get(&self, url: &str, _: &str, _: Option<&str>) -> Result<HttpResponse, String> {
            self.0.lock().push(url.to_string());
            let v1 = url.ends_with("/api.json");
            let body = json!({"beta": {"id": "beta", "name": "Beta", "env": ["BETA_API_KEY"], "npm": "@ai-sdk/openai-compatible",
                "api": "https://api.beta.test/v1", "models": {"beta-1": {"id": "beta-1", "name": "Beta One", "release_date": "2026-02-01",
                "tool_call": true, "limit": {"context": 64000, "output": 4096}}}}});
            Ok(HttpResponse { status: if v1 { 200 } else { 404 }, body: if v1 { body.to_string() } else { String::new() }, etag: None })
        }
    }
    let http = Arc::new(Mirror(Mutex::new(vec![])));
    let mut opts = options(true, false);
    opts.url = Some("https://mirror.example".into());
    let service = ModelsDev::new(opts, Arc::new(MemoryCache::default()), http.clone());
    let result = service.get().await.unwrap();
    assert_eq!(
        *http.0.lock(),
        ["https://mirror.example/experimental/v2.0/api.json?type=chat,decision,image", "https://mirror.example/api.json"]
    );
    assert_eq!(result[0].models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["beta-1"]);
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
    let meta = cache.get(META_KEY).await.unwrap();
    assert_eq!(meta.as_object().unwrap().keys().collect::<Vec<_>>(), ["updatedAt", "digest"]);
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
    assert_eq!(http.calls()[0].url, "https://models.opencode.ai/experimental/v2.0/api.json?type=chat,decision,image");
    assert_eq!(http.calls()[0].user_agent, "opencode/test/1.0.0/opencode");
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
    assert!(http.calls()[0].url.contains(CATALOG_PATH));
    assert!(http.calls()[0].user_agent.ends_with("/opencode"));
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
    let meta = cache.get(META_KEY).await.unwrap();
    assert_eq!(meta["digest"], json!(body_digest(&fixture().to_string())));
    assert!(now() - meta["updatedAt"].as_f64().unwrap() < 60_000.0);
}

#[tokio::test]
async fn refresh_revalidates_with_the_stored_etag_and_treats_304_as_unchanged() {
    let cache = Arc::new(MemoryCache::default());
    let http = MockHttp::new(&fixture(), 200);
    *http.etag.lock() = Some("W/\"v1\"".into());
    let service = ModelsDev::new(options(false, true), cache.clone(), http.clone());
    assert!(service.refresh(true).await);
    let mut meta = cache.get(META_KEY).await.unwrap();
    assert_eq!(meta.as_object().unwrap().keys().collect::<Vec<_>>(), ["updatedAt", "digest", "etag"]);
    meta["updatedAt"] = json!(now() - 10.0 * 60.0 * 1000.0);
    cache.set(META_KEY, meta).await.unwrap();
    let body = cache.get(DEFAULT_CACHE_KEY).await.unwrap();
    *http.status.lock() = 304;
    let mut events = service.subscribe();
    assert!(!service.refresh(false).await);
    assert!(events.try_recv().is_err());
    assert_eq!(to_json(&service.get().await.unwrap()), fixture_snapshot());
    assert_eq!(http.calls().iter().map(|c| c.if_none_match.clone()).collect::<Vec<_>>(), [None, Some("W/\"v1\"".into())]);
    assert_eq!(cache.get(DEFAULT_CACHE_KEY).await.unwrap(), body);
    let meta = cache.get(META_KEY).await.unwrap();
    assert_eq!(meta["etag"], "W/\"v1\"");
    assert!(now() - meta["updatedAt"].as_f64().unwrap() < 60_000.0);
    // A forced refresh never revalidates.
    *http.status.lock() = 200;
    service.refresh(true).await;
    assert_eq!(http.calls()[2].if_none_match, None);
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
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("api.json");
    std::fs::write(&file, fixture2().to_string()).unwrap();
    let cache = Arc::new(MemoryCache::default());
    write_cache(&cache, &fixture(), now()).await;
    let mut opts = options(false, true);
    opts.file = Some(file);
    let service = ModelsDev::new(opts, cache, MockHttp::new(&fixture(), 200));
    assert_eq!(to_json(&service.get().await.unwrap()), fixture2_snapshot());
}

#[test]
fn normalizes_fixture() {
    assert_eq!(to_json(&normalize(fixture().as_object().unwrap())), fixture_snapshot());
    assert_eq!(to_json(&decode_snapshot(&fixture2().to_string()).unwrap()), fixture2_snapshot());
}

// ---------------------------------------------------------------------------------------------
// Filtering, integrations
// ---------------------------------------------------------------------------------------------

#[test]
fn filters_aliases_deprecated_textless_chat_and_bedrock_profile_models() {
    let model = |id: &str, extra: Value| source_model(id, extra);
    let data = json!({
        "acme": {"id": "acme", "name": "Acme", "env": [], "models": {
            "current": model("current", json!({})),
            "legacy": model("legacy", json!({"status": "deprecated"})),
            "video-gen": model("video-gen", json!({"modalities": {"input": ["text"], "output": ["video/mp4"]}})),
            "transcribe": model("transcribe", json!({"modalities": {"input": ["audio/wav"], "output": ["text"]}})),
            "image": model("image", json!({"type": "image", "modalities": {"input": ["text"], "output": ["image/png"]},
                "api": {"images": {"base_url": "https://api.example.test/v1", "sdk": {"opencodeai": "@opencode/ai/providers/openai-compatible/images"}}}})),
        }},
        "azure-cognitive-services": {"id": "azure-cognitive-services", "name": "x", "env": [], "models": {"m": model("m", json!({}))}},
        "google-vertex-anthropic": {"id": "google-vertex-anthropic", "name": "x", "env": [], "models": {"m": model("m", json!({}))}},
        "amazon-bedrock": {"id": "amazon-bedrock", "name": "Bedrock", "env": ["AWS_BEARER_TOKEN_BEDROCK"], "models": {
            "deepseek.r1-v1:0": model("deepseek.r1-v1:0", json!({})),
            "anthropic.claude-sonnet-4-6": model("anthropic.claude-sonnet-4-6", json!({})),
        }},
    });
    let filtered = filter_snapshots(&normalize(data.as_object().unwrap()));
    assert_eq!(filtered.iter().map(|s| s.info.id.as_str()).collect::<Vec<_>>(), ["acme", "amazon-bedrock"]);
    // Non-chat models are kept for lookups; the Model service never offers them for sessions.
    assert_eq!(filtered[0].models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["current", "image"]);
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
    let body: Value = serde_json::from_str(r#"{"__proto__":{"service_tier":"priority"},"keep":true}"#).unwrap();
    let data = json!({
        "acme": {"id": "acme", "name": "Acme", "env": [], "models": {
            "m": source_model("m", json!({"release_date": "", "api": {"chat-completions": {
                "base_url": "https://${ACME_HOST}/${ACME_MODEL_PATH}/v1", "sdk": {"opencodeai": "@opencode/ai/providers/openai-compatible"}
            }}, "experimental": {"modes": {"x": {"body": body}}}})),
        }}
    });
    let result = normalize(data.as_object().unwrap());
    assert_eq!(result[0].info.settings.as_ref().unwrap()["baseURL"], "https://${ACME_HOST}/${ACME_MODEL_PATH}/v1");
    assert_eq!(result[0].models[0].time.released, 0.0);
    let body = result[0].models[1].body.as_ref().unwrap();
    assert_eq!(body.keys().collect::<Vec<_>>(), ["__proto__", "keep"]);
}

// ---------------------------------------------------------------------------------------------
// Model types through the catalog (`lists non-chat models only by type, so sessions never see them`)
// ---------------------------------------------------------------------------------------------

#[test]
fn lists_non_chat_models_only_by_type_so_sessions_never_see_them() {
    let model = |id: &str, kind: Option<&str>| {
        let mut m = oc_ai::catalog::ModelInfo::default_for("acme", id);
        m.capabilities.input = vec!["text".into()];
        m.kind = kind.map(String::from);
        m.time.released = 1767225600000.0;
        m.limit = oc_ai::catalog::Limit { context: 128_000, input: None, output: 32_000 };
        m
    };
    let snapshot = Snapshot {
        info: oc_ai::catalog::ProviderInfo {
            package: "@opencode/ai/providers/openai-compatible".into(),
            name: "Acme".into(),
            ..oc_ai::catalog::ProviderInfo::empty("acme")
        },
        environment: vec![],
        models: vec![model("gpt-5.5", None), model("gpt-audio", Some("chat")), model("jev-1.13", Some("decision")), model("gpt-image-2", Some("image"))],
    };
    let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![Box::new(ModelsDevPlugin::new(&[snapshot])), Box::new(Activate)];
    let catalog = Catalog::build(&mut plugins, &Access::new());
    let ids = |models: Vec<&oc_ai::catalog::ModelInfo>| {
        let mut ids: Vec<String> = models.into_iter().map(|m| m.id.clone()).collect();
        ids.sort();
        ids
    };
    let types = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(ids(catalog.available_models()), ["gpt-5.5", "gpt-audio"]);
    assert_eq!(ids(catalog.models().iter().collect()), ["gpt-5.5", "gpt-audio"]);
    assert!(catalog.model("acme", "jev-1.13").is_none());
    assert_eq!(ids(catalog.available_models_of(&types(&["decision"]))), ["jev-1.13"]);
    assert_eq!(ids(catalog.available_models_of(&types(&["image"]))), ["gpt-image-2"]);
    assert_eq!(ids(catalog.available_models_of(&types(&["chat", "image", "image"]))), ["gpt-5.5", "gpt-audio", "gpt-image-2"]);
    assert!(catalog.available_models_of(&types(&["embedding"])).is_empty());
    assert_eq!(ids(catalog.available_models_of(&[])), ["gpt-5.5", "gpt-audio"]);
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
    assert_eq!(anthropic.info.base_url(), Some("https://api.anthropic.com/v1"));
    assert_eq!(data.len(), 230);
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
