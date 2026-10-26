//! Catalog changes from upstream 2.1 (`packages/core/test/{variant,aisdk-native,catalog,model-free}.test.ts`
//! and `config/provider.test.ts`): native entrypoints and aliases, models.dev v2 reasoning, provider
//! compatibility, configured model edits, and the free OpenCode model defaults.

use oc_ai::catalog::config::{ConfigProviders, ConfigReasoning, ConfigReasoningBudget};
use oc_ai::catalog::native::{rewrite, NativeContext, RewriteTarget};
use oc_ai::catalog::plugins::{DataPlugins, OpencodeZen};
use oc_ai::catalog::registry::{Access, Catalog, CatalogPlugin, ModelEditor, ProviderEditor};
use oc_ai::catalog::types::{Cost, CostCache, HeaderMap, ModelInfo, Variant, BUILTIN_PACKAGES};
use oc_ai::catalog::variant::{resolve, supports, Support};
use oc_ai::json::Object;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Mutex;

/// Tests that read `OPENCODE_DISABLE_FREE_MODELS` run one at a time.
static ENV: Mutex<()> = Mutex::new(());

struct Fold<P: FnMut(&mut ProviderEditor), M: FnMut(&mut ModelEditor<'_>)> {
    p: P,
    m: M,
}

impl<P: FnMut(&mut ProviderEditor), M: FnMut(&mut ModelEditor<'_>)> CatalogPlugin for Fold<P, M> {
    fn providers(&mut self, editor: &mut ProviderEditor) {
        (self.p)(editor)
    }
    fn models(&mut self, editor: &mut ModelEditor<'_>) {
        (self.m)(editor)
    }
}

fn providers<'a>(p: impl FnMut(&mut ProviderEditor) + 'a) -> Box<dyn CatalogPlugin + 'a> {
    Box::new(Fold { p, m: |_: &mut ModelEditor<'_>| {} })
}

fn models<'a>(m: impl FnMut(&mut ModelEditor<'_>) + 'a) -> Box<dyn CatalogPlugin + 'a> {
    Box::new(Fold { p: |_: &mut ProviderEditor| {}, m })
}

fn build(mut plugins: Vec<Box<dyn CatalogPlugin + '_>>) -> Catalog {
    Catalog::build(&mut plugins, &Access::new())
}

fn v<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap()
}

fn obj(value: Value) -> Object {
    value.as_object().unwrap().clone()
}

fn model(package: &str, id: &str, output: i64) -> ModelInfo {
    let mut m = ModelInfo::default_for("test", id);
    m.package = Some(package.into());
    m.limit.output = output;
    m
}

fn paid() -> Vec<Cost> {
    vec![Cost { tier: None, input: 1.0, output: 0.0, cache: CostCache::default() }]
}

// ---------------------------------------------------------------------------------------------
// variant.test.ts
// ---------------------------------------------------------------------------------------------

#[test]
fn spells_reasoning_variants_for_every_bundled_native_package() {
    let all = [Support::Toggle, Support::effort(), Support::BudgetTokens { min: None, max: None }];
    let missing: Vec<&str> =
        BUILTIN_PACKAGES.iter().copied().filter(|p| resolve(&model(p, "test-model", 32_000), &all).is_empty()).collect();
    assert_eq!(missing, Vec::<&str>::new());
}

#[test]
fn spells_vercel_ai_gateway_variants_for_their_selected_routes() {
    let pkg = "@opencode/ai/providers/vercel-ai-gateway";
    assert_eq!(
        v(&resolve(&model(&format!("{pkg}/chat"), "anthropic/claude-sonnet-4.6", 32_000), &[Support::efforts(&["high"])])),
        json!([{"id": "high", "settings": {"reasoningEffort": "high"}}])
    );
    assert_eq!(
        v(&resolve(&model(&format!("{pkg}/messages"), "anthropic/claude-sonnet-4.6", 32_000), &[Support::efforts(&["high"])])),
        json!([{"id": "high", "settings": {"effort": "high", "thinking": {"type": "adaptive", "display": "summarized"}}}])
    );
    let ids: Vec<String> =
        resolve(&model(&format!("{pkg}/responses"), "spacexai/grok-4.7", 32_000), &[Support::effort()]).into_iter().map(|v| v.id).collect();
    assert_eq!(ids, ["low", "medium", "high"]);
}

#[test]
fn spells_mantle_variants_for_the_api_each_model_is_routed_through() {
    assert_eq!(
        v(&resolve(
            &model("@opencode/ai/providers/amazon-bedrock/mantle/messages", "anthropic.claude-opus-4-8", 32_000),
            &[Support::efforts(&["low", "max"])]
        )),
        json!([
            {"id": "low", "settings": {"effort": "low", "thinking": {"type": "adaptive", "display": "summarized"}}},
            {"id": "max", "settings": {"effort": "max", "thinking": {"type": "adaptive", "display": "summarized"}}}
        ])
    );
    assert_eq!(
        v(&resolve(&model("@opencode/ai/providers/amazon-bedrock/mantle/chat", "openai.gpt-oss-120b", 32_000), &[Support::effort()])),
        json!([
            {"id": "low", "settings": {"reasoningEffort": "low"}},
            {"id": "medium", "settings": {"reasoningEffort": "medium"}},
            {"id": "high", "settings": {"reasoningEffort": "high"}}
        ])
    );
}

#[test]
fn spells_google_interactions_variants_as_thinking_levels() {
    for package in ["@opencode/ai/providers/google/interactions", "@opencode/ai/providers/google-vertex/interactions"] {
        assert_eq!(
            v(&resolve(&model(package, "gemini-3.5-pro", 32_000), &[Support::efforts(&["low", "high"])])),
            json!([
                {"id": "low", "settings": {"thinkingLevel": "low", "thinkingSummaries": "auto"}},
                {"id": "high", "settings": {"thinkingLevel": "high", "thinkingSummaries": "auto"}}
            ])
        );
    }
}

#[test]
fn reads_models_dev_v2_reasoning_as_variant_support() {
    assert_eq!(supports(&ConfigReasoning { supported: false, ..Default::default() }), vec![]);
    assert_eq!(supports(&ConfigReasoning { supported: true, ..Default::default() }), vec![]);
    assert_eq!(
        supports(&ConfigReasoning {
            supported: true,
            toggle: Some(true),
            effort: Some(vec!["default".into(), "low".into()]),
            budget: Some(ConfigReasoningBudget { min: Some(1024), max: Some(8000) }),
        }),
        vec![Support::Toggle, Support::efforts(&["low"]), Support::BudgetTokens { min: Some(1024.0), max: Some(8000.0) }]
    );
}

// ---------------------------------------------------------------------------------------------
// aisdk-native.test.ts
// ---------------------------------------------------------------------------------------------

fn rewrite_native(specifier: &str, settings: Value, model_id: &str) -> (Option<String>, Option<Object>, Option<HeaderMap>) {
    let mut package = Some(specifier.to_string());
    let mut s = Some(obj(settings));
    let mut headers: Option<HeaderMap> = None;
    let mut body: Option<Object> = None;
    rewrite(
        RewriteTarget { package: &mut package, settings: &mut s, headers: &mut headers, body: &mut body, variants: None },
        Some(specifier),
        &NativeContext { provider_id: "test-provider", model_id: Some(model_id), ..Default::default() },
    );
    (package, s, headers)
}

#[test]
fn translates_settings_for_every_bundled_native_package_named_in_config() {
    for specifier in BUILTIN_PACKAGES {
        let (_, _, headers) = rewrite_native(specifier, json!({"headers": {"x-test": "value"}}), "test-model");
        assert_eq!(v(&headers), json!({"x-test": "value"}), "{specifier}");
    }
    let (package, settings, _) =
        rewrite_native("@opencode/ai/providers/amazon-bedrock/mantle", json!({"bearerToken": "token"}), "anthropic.claude-opus-4-8");
    assert_eq!(package.as_deref(), Some("@opencode/ai/providers/amazon-bedrock/mantle/messages"));
    assert_eq!(settings.unwrap()["apiKey"], "token");
    let (package, _, _) = rewrite_native("@opencode/ai/providers/azure", json!({"useCompletionUrls": true}), "test-model");
    assert_eq!(package.as_deref(), Some("@opencode/ai/providers/azure/chat"));
    let (package, _, _) = rewrite_native("@opencode/ai/providers/amazon-bedrock-mantle", json!({}), "anthropic.claude-opus-4-8");
    assert_eq!(package.as_deref(), Some("@opencode/ai/providers/amazon-bedrock/mantle/messages"));
}

#[test]
fn maps_claude_on_mantle_to_messages() {
    let (package, settings, _) =
        rewrite_native("aisdk:@ai-sdk/amazon-bedrock/mantle", json!({"region": "us-east-1", "profile": "sso"}), "anthropic.claude-opus-4-8");
    assert_eq!(package.as_deref(), Some("@opencode/ai/providers/amazon-bedrock/mantle/messages"));
    assert_eq!(v(&settings), json!({"region": "us-east-1", "profile": "sso"}));
}

#[test]
fn every_bundled_package_loads_by_name() {
    for package in BUILTIN_PACKAGES {
        let settings = obj(json!({"apiKey": "k", "baseURL": "https://example.test/v1", "region": "us-east-1", "resourceName": "r", "accountId": "a", "project": "p"}));
        let vertex_openapi = package.contains("google-vertex")
            && *package != "@opencode/ai/providers/google-vertex"
            && !package.ends_with("gemini")
            && !package.ends_with("interactions");
        let result = oc_ai::providers::language_model(package, "model-x", &settings);
        if vertex_openapi {
            // API keys are rejected there, but the package itself is known.
            assert!(result.is_err_and(|e| !e.message.contains("is not installed")), "{package}");
            continue;
        }
        assert!(result.is_ok(), "{package}: {:?}", result.err());
    }
}

// ---------------------------------------------------------------------------------------------
// catalog.test.ts / model-free.test.ts
// ---------------------------------------------------------------------------------------------

#[test]
fn defaults_to_big_pickle_when_only_free_opencode_models_are_available() {
    let _env = ENV.lock().unwrap();
    let catalog = build(vec![providers(|e| {
        e.update("opencode", |_| {});
        for (index, id) in ["muse-spark-1.3-contributor-free", "new-free", "deepseek-v4.1-flash-free", "big-pickle"].iter().enumerate() {
            e.update_model("opencode", id, |m| m.time.released = 4000.0 - index as f64 * 1000.0);
        }
    })]);
    let ids: Vec<&str> = catalog.available_models().iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["big-pickle", "deepseek-v4.1-flash-free", "muse-spark-1.3-contributor-free", "new-free"]);
    assert_eq!(catalog.default_model().unwrap().id, "big-pickle");
}

#[test]
fn keeps_the_newest_default_when_a_paid_model_is_available() {
    let _env = ENV.lock().unwrap();
    let catalog = build(vec![providers(|e| {
        e.update("opencode", |_| {});
        e.update_model("opencode", "big-pickle", |m| m.time.released = 1000.0);
        e.update_model("opencode", "paid", |m| {
            m.time.released = 2000.0;
            m.cost = vec![Cost { tier: None, input: 1.0, output: 1.0, cache: CostCache::default() }];
        });
    })]);
    assert_eq!(catalog.default_model().unwrap().id, "paid");
}

#[test]
fn opencode_disable_free_models_hides_zero_cost_opencode_models() {
    let _env = ENV.lock().unwrap();
    for flag in [None, Some("0"), Some("1"), Some("true")] {
        for provider_id in ["opencode", "opencode-go"] {
            // SAFETY: tests touching this variable hold `ENV`.
            unsafe {
                match flag {
                    Some(f) => std::env::set_var("OPENCODE_DISABLE_FREE_MODELS", f),
                    None => std::env::remove_var("OPENCODE_DISABLE_FREE_MODELS"),
                }
            }
            let catalog = build(vec![
                providers(|e| {
                    e.update(provider_id, |_| {});
                    e.update_model(provider_id, "big-pickle", |m| {
                        m.time.released = 3000.0;
                        m.family = Some("gpt-luna".into());
                        m.capabilities.input = vec!["text".into()];
                        m.capabilities.output = vec!["text".into()];
                    });
                    e.update_model(provider_id, "paid", |m| {
                        m.time.released = 2000.0;
                        m.cost = paid();
                    });
                    e.update("other", |_| {});
                    e.update_model("other", "local", |_| {});
                }),
                models(|e| e.set_default(provider_id, "big-pickle")),
            ]);
            let disabled = matches!(flag, Some("1" | "true"));
            let all: Vec<&str> = catalog.models().iter().map(|m| m.id.as_str()).collect();
            assert_eq!(all, if disabled { vec!["paid", "local"] } else { vec!["big-pickle", "paid", "local"] }, "{flag:?} {provider_id}");
            let available: Vec<&str> = catalog.available_models().iter().map(|m| m.id.as_str()).collect();
            assert_eq!(available, all);
            assert_eq!(catalog.default_model().unwrap().id, if disabled { "paid" } else { "big-pickle" });
            assert_eq!(catalog.model(provider_id, "big-pickle").is_some(), !disabled);
            assert_eq!(catalog.small_model(provider_id).map(|m| m.id.as_str()), if disabled { None } else { Some("big-pickle") });
        }
    }
    // The keyless `opencode` provider no longer turns itself on.
    unsafe { std::env::set_var("OPENCODE_DISABLE_FREE_MODELS", "1") };
    let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![
        providers(|e| {
            e.update("opencode", |_| {});
            e.update_model("opencode", "big-pickle", |_| {});
        }),
        Box::new(OpencodeZen { connected: false }),
    ];
    let catalog = Catalog::build(&mut plugins, &Access::new().integration("opencode", vec![]));
    assert!(catalog.provider("opencode").unwrap().setting("apiKey").is_none());
    unsafe { std::env::remove_var("OPENCODE_DISABLE_FREE_MODELS") };
    let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![
        providers(|e| {
            e.update("opencode", |_| {});
            e.update_model("opencode", "big-pickle", |_| {});
        }),
        Box::new(OpencodeZen { connected: false }),
    ];
    let catalog = Catalog::build(&mut plugins, &Access::new().integration("opencode", vec![]));
    assert_eq!(catalog.provider("opencode").unwrap().setting("apiKey"), Some(&json!("public")));
}

// ---------------------------------------------------------------------------------------------
// config/provider.test.ts
// ---------------------------------------------------------------------------------------------

#[test]
fn keeps_provider_and_model_compatibility_precedence_across_config_documents() {
    let catalog = build(vec![
        providers(|e| {
            e.update("proxy", |p| p.package = "@opencode/ai/providers/openai/chat".into());
            e.update_model("proxy", "catalog", |_| {});
        }),
        Box::new(ConfigProviders::from_documents(&[
            json!({"proxy": {
                // Model-version features are model-only and are not accepted on the provider.
                "compatibility": {"requireFinishReason": false, "supportsEffortUpdates": false},
                "models": {"claude": {"compatibility": {"supportsPromptCacheKey": true}}}
            }}),
            json!({"proxy": {"compatibility": {"supportsPromptCacheKey": false}, "models": {"alias": {"modelID": "catalog"}}}}),
        ])),
    ]);
    assert_eq!(v(&catalog.provider("proxy").unwrap().compatibility), json!({"requireFinishReason": false, "supportsPromptCacheKey": false}));
    // A later provider default does not override an earlier model setting.
    assert_eq!(v(&catalog.model("proxy", "claude").unwrap().compatibility), json!({"requireFinishReason": false, "supportsPromptCacheKey": true}));
    // A model first defined in a later document still inherits earlier provider values.
    assert_eq!(v(&catalog.model("proxy", "alias").unwrap().compatibility), json!({"requireFinishReason": false, "supportsPromptCacheKey": false}));
}

#[test]
fn lets_configured_provider_compatibility_override_built_in_provider_defaults() {
    let setup = |e: &mut ProviderEditor| {
        e.update("venice", |p| p.package = "@opencode/ai/providers/openai-compatible".into());
        e.update_model("venice", "chat", |_| {});
        e.update_model("venice", "keyed", |_| {});
    };
    let catalog = build(vec![providers(setup), Box::new(DataPlugins::new(HashSet::new()))]);
    assert_eq!(v(&catalog.model("venice", "chat").unwrap().compatibility), json!({"supportsPromptCacheKey": true}));
    let catalog = build(vec![
        providers(setup),
        Box::new(DataPlugins::new(HashSet::new())),
        Box::new(ConfigProviders::from_value(&json!({"venice": {
            "compatibility": {"supportsPromptCacheKey": false},
            "models": {"keyed": {"compatibility": {"supportsPromptCacheKey": true}}}
        }}))),
    ]);
    assert_eq!(v(&catalog.model("venice", "chat").unwrap().compatibility), json!({"supportsPromptCacheKey": false}));
    assert_eq!(v(&catalog.model("venice", "keyed").unwrap().compatibility), json!({"supportsPromptCacheKey": true}));
}

#[test]
fn carries_provider_compatibility_to_the_runtime_model() {
    let catalog = build(vec![
        providers(|e| {
            e.update("proxy", |p| p.package = "@opencode/ai/providers/openai/chat".into());
            e.update_model("proxy", "catalog", |m| {
                m.compatibility = Some(serde_json::from_value(json!({"maxTokensField": "max_tokens"})).unwrap())
            });
            e.update("strict", |p| {
                p.package = "@opencode/ai/providers/openai-compatible".into();
                p.settings = Some(obj(json!({"baseURL": "https://strict.test/v1"})));
            });
            e.update_model("strict", "catalog", |_| {});
        }),
        Box::new(ConfigProviders::from_value(&json!({
            "proxy": {"compatibility": {"supportsPromptCacheKey": false, "requireFinishReason": false},
                "models": {"configured": {"compatibility": {"supportsPromptCacheKey": true}}}},
            "strict": {"compatibility": {"streamUsage": false}},
            "litellm": {"package": "@opencode/ai/providers/openai-compatible", "settings": {"baseURL": "https://litellm.test/v1"},
                "compatibility": {"cacheFormat": "anthropic"}, "models": {"claude-sonnet-5": {}}}
        }))),
    ]);
    let proxy = catalog.model("proxy", "catalog").unwrap();
    assert_eq!(v(&proxy.compatibility), json!({"maxTokensField": "max_tokens", "supportsPromptCacheKey": false, "requireFinishReason": false}));
    let configured = catalog.model("proxy", "configured").unwrap();
    assert_eq!(v(&configured.compatibility), json!({"supportsPromptCacheKey": true, "requireFinishReason": false}));
    let resolved = |m: &ModelInfo| {
        let mut s = obj(json!({"apiKey": "k", "baseURL": "https://example.test/v1"}));
        if let Some(settings) = &m.settings {
            s.extend(settings.clone());
        }
        let lm = oc_ai::providers::language_model(m.package.as_deref().unwrap(), &m.model_id, &s).unwrap();
        lm.with_compatibility(m.compatibility.clone().unwrap_or_default()).compatibility.unwrap()
    };
    assert_eq!(resolved(proxy).supports_prompt_cache_key, Some(false));
    assert_eq!(resolved(configured).supports_prompt_cache_key, Some(true));
    let strict = catalog.model("strict", "catalog").unwrap();
    assert_eq!(v(&strict.compatibility), json!({"streamUsage": false}));
    assert_eq!(resolved(strict).stream_usage, Some(false));
    let litellm = catalog.model("litellm", "claude-sonnet-5").unwrap();
    assert_eq!(v(&litellm.compatibility), json!({"cacheFormat": "anthropic"}));
    assert_eq!(resolved(litellm).cache_format.as_deref(), Some("anthropic"));
}

#[test]
fn lets_a_configured_provider_base_url_override_catalog_model_base_urls() {
    let catalog = build(vec![
        providers(|e| {
            for id in ["chat", "pinned"] {
                e.update_model("gateway", id, |m| m.settings = Some(obj(json!({"baseURL": "https://catalog.example/v1"}))));
            }
        }),
        Box::new(ConfigProviders::from_value(&json!({"gateway": {
            "settings": {"baseURL": "http://127.0.0.1:17322"},
            "models": {"pinned": {"settings": {"baseURL": "http://127.0.0.1:9000"}}}
        }}))),
    ]);
    assert_eq!(catalog.model("gateway", "chat").unwrap().settings.as_ref().unwrap()["baseURL"], "http://127.0.0.1:17322");
    assert_eq!(catalog.model("gateway", "pinned").unwrap().settings.as_ref().unwrap()["baseURL"], "http://127.0.0.1:9000");
}

#[test]
fn respells_inherited_variants_when_config_moves_a_catalog_model_to_another_package() {
    let model_id = "anthropic.claude-opus-5-5";
    let catalog = build(vec![
        providers(|e| {
            e.update_model("bedrock", model_id, |m| {
                m.package = Some("@opencode/ai/providers/amazon-bedrock".into());
                m.variants = vec![Variant {
                    id: "high".into(),
                    body: Some(obj(json!({"additionalModelRequestFields": {"output_config": {"effort": "high"}}}))),
                    ..Default::default()
                }];
            });
            e.update_model("bedrock", "anthropic.claude-plain", |m| m.package = Some("@opencode/ai/providers/amazon-bedrock".into()));
        }),
        Box::new(ConfigProviders::from_value(&json!({"bedrock": {"models": {
            model_id: {"package": "@opencode/ai/providers/amazon-bedrock/mantle"},
            "anthropic.claude-plain": {"package": "@opencode/ai/providers/amazon-bedrock/mantle"}
        }}}))),
    ]);
    let model = catalog.model("bedrock", model_id).unwrap();
    assert_eq!(model.package.as_deref(), Some("@opencode/ai/providers/amazon-bedrock/mantle/messages"));
    assert_eq!(
        v(&model.variants.iter().find(|v| v.id == "high")),
        json!({"id": "high", "settings": {"effort": "high", "thinking": {"type": "adaptive", "display": "summarized"}}})
    );
    // A moved catalog model without reasoning variants stays free of variants.
    assert_eq!(v(&catalog.model("bedrock", "anthropic.claude-plain").unwrap().variants), json!([]));
}

#[test]
fn builds_configured_reasoning_variants_after_provider_and_model_package_normalization() {
    let catalog = build(vec![Box::new(ConfigProviders::from_value(&json!({"custom": {
        "package": "aisdk:@ai-sdk/openai",
        "models": {
            "effort": {
                "capabilities": {"reasoning": {"supported": true, "effort": ["default", "low", "high"]}},
                "variants": [{"id": "high", "headers": {"x-tier": "high"}}]
            },
            "gpt": {
                "package": "aisdk:@ai-sdk/openai",
                "capabilities": {"reasoning": {"supported": true, "effort": ["low", "high"]}},
                "variants": [{"id": "high", "headers": {"x-tier": "high"}}]
            },
            "claude-sonnet-4-5": {
                "package": "aisdk:@ai-sdk/anthropic",
                "capabilities": {"reasoning": {"supported": true, "budget": {"min": 1024}}},
                "limit": {"output": 64000}
            },
            "gemini-3.1-pro": {"package": "aisdk:@ai-sdk/google", "capabilities": {"reasoning": {"supported": true, "effort": ["low", "high"]}}},
            "magistral-medium": {"package": "aisdk:@ai-sdk/mistral", "capabilities": {"reasoning": {"supported": true, "effort": ["low", "high"]}}},
            "fixed": {"capabilities": {"reasoning": {"supported": true}}},
            "plain": {"capabilities": {"reasoning": {"supported": false}}}
        }
    }})))]);
    let effort = catalog.model("custom", "effort").unwrap();
    let responses = |e: &str| json!({"reasoningEffort": e, "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]});
    assert_eq!(
        v(&effort.variants),
        json!([{"id": "low", "settings": responses("low")}, {"id": "high", "settings": responses("high"), "headers": {"x-tier": "high"}}])
    );
    let gpt = catalog.model("custom", "gpt").unwrap();
    assert_eq!(gpt.package.as_deref(), Some("@opencode/ai/providers/openai"));
    assert_eq!(v(&gpt.variants), v(&effort.variants));
    let claude = catalog.model("custom", "claude-sonnet-4-5").unwrap();
    assert_eq!(claude.package.as_deref(), Some("@opencode/ai/providers/anthropic"));
    assert_eq!(
        v(&claude.variants),
        json!([
            {"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 16000}}},
            {"id": "max", "settings": {"thinking": {"type": "enabled", "budgetTokens": 31999}}}
        ])
    );
    let gemini = catalog.model("custom", "gemini-3.1-pro").unwrap();
    assert_eq!(gemini.package.as_deref(), Some("@opencode/ai/providers/google"));
    assert_eq!(
        v(&gemini.variants),
        json!([
            {"id": "low", "settings": {"thinkingConfig": {"includeThoughts": true, "thinkingLevel": "low"}}},
            {"id": "high", "settings": {"thinkingConfig": {"includeThoughts": true, "thinkingLevel": "high"}}}
        ])
    );
    let magistral = catalog.model("custom", "magistral-medium").unwrap();
    assert_eq!(magistral.package.as_deref(), Some("@opencode/ai/providers/mistral"));
    assert_eq!(v(&magistral.variants), json!([{"id": "low", "settings": {"reasoningEffort": "low"}}, {"id": "high", "settings": {"reasoningEffort": "high"}}]));
    assert_eq!(v(&catalog.model("custom", "fixed").unwrap().variants), json!([]));
    assert_eq!(v(&catalog.model("custom", "plain").unwrap().variants), json!([]));
}

#[test]
fn applies_configured_release_date_temperature_support_and_reasoning_field() {
    let catalog = build(vec![Box::new(ConfigProviders::from_value(&json!({"custom": {
        "package": "aisdk:@ai-sdk/openai-compatible",
        "models": {"reasoner": {
            "releaseDate": "2026-02-03",
            "capabilities": {"temperature": false},
            "compatibility": {"reasoningField": "reasoning_content"}
        }}
    }})))]);
    let model = catalog.model("custom", "reasoner").unwrap();
    assert_eq!(model.time.released, 1770076800000.0);
    assert_eq!(model.capabilities.temperature, Some(false));
    assert_eq!(model.compatibility.as_ref().unwrap().reasoning_field.as_deref(), Some("reasoning_content"));
}

#[test]
fn seeds_configured_limits_before_plugin_model_transforms() {
    let transformed: std::cell::RefCell<Option<Value>> = Default::default();
    let mut plugins: Vec<Box<dyn CatalogPlugin + '_>> = vec![
        providers(|e| e.update_model("custom", "chat", |m| m.limit.output = 50)),
        Box::new(ConfigProviders::from_value(&json!({"custom": {"models": {"chat": {"limit": {"context": 100, "output": 75}}}}}))),
    ];
    // A plugin model transform runs between the provider fold and the configured model overrides.
    let mut staged = Catalog::build(&mut plugins, &Access::new());
    assert_eq!(v(&staged.model("custom", "chat").unwrap().limit), json!({"context": 100, "output": 75}));
    let mut plugins: Vec<Box<dyn CatalogPlugin + Send>> = vec![
        Box::new(Fold {
            p: |e: &mut ProviderEditor| e.update_model("custom", "chat", |m| m.limit.output = 50),
            m: |_: &mut ModelEditor<'_>| {},
        }),
        Box::new(ConfigProviders::from_value(&json!({"custom": {"models": {"chat": {"limit": {"context": 100, "output": 75}}}}}))),
    ];
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    staged = runtime.block_on(Catalog::build_staged(
        &mut plugins,
        1,
        &Access::new(),
        async |records| records,
        async |models, default, _| {
            *transformed.borrow_mut() = models.get("custom").and_then(|m| m.get("chat")).map(|m| v(&m.limit));
            (models, default)
        },
    ));
    assert_eq!(*transformed.borrow(), Some(json!({"context": 100, "output": 75})));
    assert_eq!(v(&staged.model("custom", "chat").unwrap().limit), json!({"context": 100, "output": 75}));
}

#[test]
fn empty_names_count_as_omitted() {
    let config = ConfigProviders::from_value(&json!({"custom": {"name": "", "models": {"chat": {"name": ""}}}}));
    let catalog = build(vec![
        providers(|e| {
            e.update("custom", |p| p.name = "Catalog".into());
            e.update_model("custom", "chat", |m| m.name = "Catalog Chat".into());
        }),
        Box::new(config),
    ]);
    assert_eq!(catalog.provider("custom").unwrap().name, "Catalog");
    assert_eq!(catalog.model("custom", "chat").unwrap().name, "Catalog Chat");
}

#[test]
fn databricks_claude_models_default_to_the_anthropic_cache_format() {
    let catalog = build(vec![
        providers(|e| {
            e.update("databricks", |p| p.package = "@opencode/ai/providers/openai-compatible".into());
            e.update_model("databricks", "databricks-claude-sonnet-5", |_| {});
            e.update_model("databricks", "databricks-gpt", |_| {});
            e.update_model("databricks", "pinned-claude", |m| {
                m.compatibility = Some(serde_json::from_value(json!({"cacheFormat": "alibaba"})).unwrap())
            });
        }),
        Box::new(DataPlugins::new(HashSet::new())),
    ]);
    assert_eq!(v(&catalog.model("databricks", "databricks-claude-sonnet-5").unwrap().compatibility), json!({"cacheFormat": "anthropic"}));
    assert_eq!(catalog.model("databricks", "databricks-gpt").unwrap().compatibility, None);
    assert_eq!(v(&catalog.model("databricks", "pinned-claude").unwrap().compatibility), json!({"cacheFormat": "alibaba"}));
}
