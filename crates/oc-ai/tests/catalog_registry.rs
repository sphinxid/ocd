//! Ports of `packages/core/test/catalog.test.ts`, `config/provider.test.ts`, provider plugin tests and the
//! v1 provider migration tests (`config/normalization.test.ts`).

use oc_ai::catalog::config::{migrate_v1_providers, ConfigProviders, ModelSelection};
use oc_ai::catalog::models_dev::IntegrationMethod;
use oc_ai::catalog::plugins::{attribution_headers, DataPlugins};
use oc_ai::catalog::registry::{Access, Catalog, CatalogPlugin, ModelEditor, ProviderEditor, ProviderRecord};
use oc_ai::catalog::types::{Activation, Capabilities, ModelInfo, ProviderInfo, Variant};
use serde_json::{json, Value};
use std::collections::HashSet;

/// A plugin from two closures.
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

// ---------------------------------------------------------------------------------------------
// catalog.test.ts
// ---------------------------------------------------------------------------------------------

#[test]
fn stores_provider_package_settings() {
    let catalog = build(vec![providers(|e| {
        e.update("test", |p| {
            p.package = "aisdk:custom-sdk".into();
            p.settings = Some(json!({"baseURL": "https://override.example.com"}).as_object().unwrap().clone());
        })
    })]);
    let p = catalog.provider("test").unwrap();
    assert_eq!(p.package, "aisdk:custom-sdk");
    assert_eq!(v(&p.settings), json!({"baseURL": "https://override.example.com"}));
    assert_eq!(p.name, "test");
}

#[test]
fn model_package_settings_win_and_defaults_inherit() {
    let catalog = build(vec![
        providers(|e| {
            e.update("test", |p| {
                p.package = "aisdk:custom-sdk".into();
                p.settings = Some(json!({"baseURL": "https://provider.example.com"}).as_object().unwrap().clone());
            });
            e.update_model("test", "inherited", |_| {});
        }),
        models(|e| {
            e.update("test", "model", |m| {
                m.model_id = "upstream-model".into();
                m.package = Some("aisdk:custom-sdk".into());
                m.settings = Some(json!({"baseURL": "https://override.example.com"}).as_object().unwrap().clone());
            })
        }),
    ]);
    let m = catalog.model("test", "model").unwrap();
    assert_eq!(m.model_id, "upstream-model");
    assert_eq!(m.package.as_deref(), Some("aisdk:custom-sdk"));
    assert_eq!(v(&m.settings), json!({"baseURL": "https://override.example.com"}));
    let inherited = catalog.model("test", "inherited").unwrap();
    assert_eq!(inherited.package.as_deref(), Some("aisdk:custom-sdk"));
    assert_eq!(v(&inherited.settings), json!({"baseURL": "https://provider.example.com"}));
}

#[test]
fn rewrites_ai_sdk_packages_and_options_on_write() {
    let catalog = build(vec![
        providers(|e| {
            e.update("bedrock", |p| {
                p.package = "aisdk:@ai-sdk/amazon-bedrock".into();
                p.settings = Some(json!({"region": "us-east-1", "extraBody": {"tag": "provider"}}).as_object().unwrap().clone());
            });
            e.update_model("bedrock", "claude", |_| {});
            e.update("gitlab", |p| {
                p.package = "aisdk:@ai-sdk/gitlab".into();
                p.settings = Some(json!({"extraBody": {"kept": true}}).as_object().unwrap().clone());
            });
        }),
        models(|e| {
            e.update("bedrock", "claude", |m| {
                m.model_id = "anthropic.claude-opus-4-8".into();
                m.package = Some("aisdk:@ai-sdk/amazon-bedrock".into());
                m.settings = Some(json!({"reasoningConfig": {"type": "adaptive", "maxReasoningEffort": "high"}}).as_object().unwrap().clone());
                m.variants = vec![Variant {
                    id: "max".into(),
                    settings: Some(json!({"reasoningConfig": {"type": "adaptive", "maxReasoningEffort": "max"}}).as_object().unwrap().clone()),
                    ..Default::default()
                }];
            })
        }),
    ]);
    let p = catalog.provider("bedrock").unwrap();
    assert_eq!(p.package, "@opencode/ai/providers/amazon-bedrock");
    assert_eq!(v(&p.settings), json!({"region": "us-east-1"}));
    assert_eq!(v(&p.body), json!({"tag": "provider"}));
    let m = catalog.model("bedrock", "claude").unwrap();
    assert_eq!(m.package.as_deref(), Some("@opencode/ai/providers/amazon-bedrock"));
    assert!(m.settings.as_ref().unwrap().get("reasoningConfig").is_none());
    assert_eq!(
        v(&m.body),
        json!({"tag": "provider", "additionalModelRequestFields": {"thinking": {"type": "adaptive"}, "output_config": {"effort": "high"}}})
    );
    assert_eq!(
        v(&m.variants),
        json!([{"id": "max", "settings": {}, "body": {"additionalModelRequestFields": {"thinking": {"type": "adaptive"}, "output_config": {"effort": "max"}}}}])
    );
    let gitlab = catalog.provider("gitlab").unwrap();
    assert_eq!(gitlab.package, "aisdk:@ai-sdk/gitlab");
    assert_eq!(v(&gitlab.settings), json!({"extraBody": {"kept": true}}));
}

#[test]
fn keeps_settings_scoped_while_merging_overlays() {
    let o = |v: Value| Some(v.as_object().unwrap().clone());
    let catalog = build(vec![
        providers(|e| {
            e.update("test", |p| {
                p.settings = o(json!({"provider": true, "shared": "provider", "timeout": 5, "transport": "websocket"}));
                p.headers = Some([("provider".to_string(), "provider".to_string()), ("Shared".into(), "provider".into())].into_iter().collect());
                p.body = o(json!({"provider": true, "shared": "provider"}));
            })
        }),
        models(|e| {
            e.update("test", "model", |m| {
                m.settings = o(json!({"model": true, "shared": "model"}));
                m.headers = Some([("model".to_string(), "model".to_string()), ("shared".into(), "model".into())].into_iter().collect());
                m.body = o(json!({"model": true, "shared": "model"}));
            })
        }),
    ]);
    let m = catalog.model("test", "model").unwrap();
    assert_eq!(serde_json::to_string(&m.settings).unwrap(), r#"{"provider":true,"shared":"model","model":true}"#);
    assert_eq!(serde_json::to_string(&m.headers).unwrap(), r#"{"provider":"provider","shared":"model","model":"model"}"#);
    assert_eq!(serde_json::to_string(&m.body).unwrap(), r#"{"provider":true,"shared":"model","model":true}"#);
    assert_eq!(
        v(&catalog.provider("test").unwrap().settings),
        json!({"provider": true, "shared": "provider", "timeout": 5, "transport": "websocket"})
    );
}

#[test]
fn default_falls_back_to_newest_available_text_model() {
    let catalog = build(vec![providers(|e| {
        e.update("test", |_| {});
        e.update_model("test", "old", |m| m.time.released = 1000.0);
        e.update_model("test", "new", |m| m.time.released = 2000.0);
        e.update_model("test", "newest-video", |m| {
            m.capabilities.input = vec!["text".into(), "image".into()];
            m.capabilities.output = vec!["video".into()];
            m.time.released = 3000.0;
        });
        e.update_model("test", "newest-stt", |m| {
            m.capabilities.input = vec!["audio".into()];
            m.capabilities.output = vec!["text".into()];
            m.time.released = 4000.0;
        });
    })]);
    assert_eq!(catalog.default_model().unwrap().id, "new");
    assert_eq!(catalog.models().iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["newest-stt", "newest-video", "new", "old"]);
}

#[test]
fn transform_default_and_disabled_provider_default() {
    let setup = |e: &mut ProviderEditor| {
        e.update("test", |_| {});
        e.update_model("test", "old", |m| m.time.released = 1000.0);
        e.update_model("test", "new", |m| m.time.released = 2000.0);
    };
    let catalog = build(vec![providers(setup), models(|e| e.set_default("test", "old"))]);
    assert_eq!(catalog.default_model().unwrap().id, "old");
    let catalog = build(vec![providers(setup)]);
    assert_eq!(catalog.default_model().unwrap().id, "new");

    let catalog = build(vec![
        providers(|e| {
            e.update("disabled", |p| p.activation = Activation::Disabled);
            e.update_model("disabled", "configured", |_| {});
            e.update("enabled", |_| {});
            e.update_model("enabled", "fallback", |_| {});
        }),
        models(|e| e.set_default("disabled", "configured")),
    ]);
    let d = catalog.default_model().unwrap();
    assert_eq!((d.provider_id.as_str(), d.id.as_str()), ("enabled", "fallback"));
    assert!(catalog.model("disabled", "configured").is_none());
    assert!(catalog.provider("disabled").is_some());
}

#[test]
fn small_model_uses_family_priority() {
    let text = |m: &mut ModelInfo, family: &str, released: f64| {
        m.family = Some(family.into());
        m.capabilities.input = vec!["text".into()];
        m.capabilities.output = vec!["text".into()];
        m.time.released = released;
    };
    let catalog = build(vec![providers(|e| {
        e.update("test", |_| {});
        e.update_model("test", "newer-flash", |m| text(m, "gemini-flash", 3000.0));
        e.update_model("test", "older-luna", |m| text(m, "gpt-luna", 1000.0));
        e.update_model("test", "newer-luna", |m| text(m, "gpt-luna", 2000.0));
    })]);
    assert_eq!(catalog.small_model("test").unwrap().id, "newer-luna");
    let catalog = build(vec![providers(|e| {
        e.update("test", |_| {});
        e.update_model("test", "large", |m| text(m, "gpt", 0.0));
    })]);
    assert!(catalog.small_model("test").is_none());
}

#[test]
fn availability_from_integrations() {
    let record = |activation: Activation, integration: Option<&str>, source: Option<&str>| ProviderRecord {
        provider: ProviderInfo { activation, integration_id: integration.map(String::from), ..ProviderInfo::empty("p") },
        models: Default::default(),
        source_connection: source.map(String::from),
    };
    use oc_ai::catalog::registry::is_available;
    let none = Access::new();
    let registered = Access::new().integration("p", vec![]);
    let connected = Access::new().integration("p", vec!["conn-a".into()]);
    assert!(is_available(&record(Activation::Auto, None, None), &none));
    assert!(!is_available(&record(Activation::Auto, None, None), &registered));
    assert!(is_available(&record(Activation::Auto, None, None), &connected));
    assert!(!is_available(&record(Activation::Auto, Some("p"), None), &none));
    assert!(is_available(&record(Activation::Auto, Some("p"), None), &connected));
    assert!(is_available(&record(Activation::Enabled, Some("p"), None), &none));
    assert!(!is_available(&record(Activation::Disabled, None, None), &connected));
    assert!(is_available(&record(Activation::Auto, Some("p"), Some("conn-a")), &connected));
    assert!(!is_available(&record(Activation::Enabled, Some("p"), Some("conn-b")), &connected));
}

#[test]
fn foreign_definitions_take_registering_provider_identity() {
    let catalog = build(vec![providers(|e| {
        e.add(ProviderInfo::empty("mine"), vec![ModelInfo::default_for("other", "m")], None);
    })]);
    assert_eq!(catalog.model("mine", "m").unwrap().provider_id, "mine");
}

// ---------------------------------------------------------------------------------------------
// Data plugins
// ---------------------------------------------------------------------------------------------

fn provider(id: &str, package: &str, base_url: Option<&str>, headers: &[(&str, &str)]) -> ProviderInfo {
    let mut p = ProviderInfo::empty(id);
    p.package = package.into();
    if let Some(url) = base_url {
        p.settings = Some(json!({"baseURL": url}).as_object().unwrap().clone());
    }
    if !headers.is_empty() {
        p.headers = Some(headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect());
    }
    p
}

fn headers_of(p: &ProviderInfo) -> String {
    serde_json::to_string(&attribution_headers(p, &HashSet::from(["llmgateway".to_string()]))).unwrap()
}

#[test]
fn attribution_headers_per_plugin() {
    let oc = "@opencode/ai/providers/openai-compatible";
    assert_eq!(
        headers_of(&provider("openrouter", "@opencode/ai/providers/openrouter", None, &[("Existing", "value")])),
        r#"{"Existing":"value","HTTP-Referer":"https://opencode.ai/","X-Title":"opencode"}"#
    );
    assert_eq!(
        headers_of(&provider("cerebras", "@opencode/ai/providers/cerebras", None, &[])),
        r#"{"X-Cerebras-3rd-Party-Integration":"opencode"}"#
    );
    assert_eq!(headers_of(&provider("kilo", oc, Some("https://api.kilo.ai/api/gateway"), &[])), r#"{"HTTP-Referer":"https://opencode.ai/","X-Title":"opencode"}"#);
    assert_eq!(
        headers_of(&provider("nvidia", oc, Some("https://integrate.api.nvidia.com/v1"), &[("Existing", "value")])),
        r#"{"Existing":"value","HTTP-Referer":"https://opencode.ai/","X-Title":"opencode","X-BILLING-INVOKE-ORIGIN":"OpenCode"}"#
    );
    assert_eq!(
        headers_of(&provider("nvidia", oc, Some("https://integrate.api.nvidia.com/v1"), &[("X-BILLING-INVOKE-ORIGIN", "CustomOrigin")])),
        r#"{"X-BILLING-INVOKE-ORIGIN":"CustomOrigin","HTTP-Referer":"https://opencode.ai/","X-Title":"opencode"}"#
    );
    assert_eq!(
        headers_of(&provider("llmgateway", oc, Some("https://api.llmgateway.io/v1"), &[])),
        r#"{"HTTP-Referer":"https://opencode.ai/","X-Title":"opencode","X-Source":"opencode"}"#
    );
    assert_eq!(headers_of(&provider("other", oc, Some("https://api.llmgateway.io/v1"), &[])), "null");
    assert_eq!(headers_of(&provider("zenmux", oc, Some("https://zenmux.ai/api/v1"), &[])), r#"{"HTTP-Referer":"https://opencode.ai/","X-Title":"opencode"}"#);
    assert_eq!(
        headers_of(&provider("zenmux", oc, Some("https://zenmux.ai/api/v1"), &[("HTTP-Referer", "https://example.com/"), ("X-Title", "custom-title")])),
        r#"{"HTTP-Referer":"https://example.com/","X-Title":"custom-title"}"#
    );
    assert_eq!(
        headers_of(&provider("vercel", "aisdk:@ai-sdk/vercel", None, &[])),
        r#"{"http-referer":"https://opencode.ai/","x-title":"opencode"}"#
    );
    assert_eq!(headers_of(&provider("nvidia", "@opencode/ai/providers/openrouter", None, &[])).contains("X-BILLING"), false);
    assert_eq!(headers_of(&provider("x", oc, Some("https://example.com"), &[])), "null");
}

#[test]
fn data_plugins_disable_openrouter_aliases_and_flag_prompt_cache_key() {
    let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![
        providers(|e| {
            e.update("openrouter", |p| {
                p.package = "@opencode/ai/providers/openrouter".into();
                p.activation = Activation::Enabled;
            });
            e.update_model("openrouter", "openai/gpt-5-chat", |_| {});
            e.update_model("openrouter", "openai/gpt-5", |_| {});
            e.update("venice", |p| p.activation = Activation::Enabled);
            e.update_model("venice", "v", |m| {
                m.compatibility = Some(oc_ai::Compatibility { reasoning_field: Some("r".into()), ..Default::default() })
            });
        }),
        Box::new(DataPlugins::new(HashSet::new())),
    ];
    let catalog = Catalog::build(&mut plugins, &Access::new());
    assert!(!catalog.model("openrouter", "openai/gpt-5-chat").unwrap().enabled);
    assert!(catalog.model("openrouter", "openai/gpt-5").unwrap().enabled);
    assert!(catalog.model("openrouter", "gpt-5-chat-latest").is_none());
    assert_eq!(v(&catalog.model("venice", "v").unwrap().compatibility), json!({"reasoningField": "r", "supportsPromptCacheKey": true}));
    assert_eq!(
        v(&catalog.provider("openrouter").unwrap().headers),
        json!({"HTTP-Referer": "https://opencode.ai/", "X-Title": "opencode"})
    );
}

// ---------------------------------------------------------------------------------------------
// config/provider.test.ts
// ---------------------------------------------------------------------------------------------

#[test]
fn adds_key_auth_for_custom_providers() {
    let config = ConfigProviders::from_value(&json!({"litellm": {"package": "aisdk:@ai-sdk/openai-compatible", "models": {"chat": {}}}}));
    let specs = config.integrations(|_| false);
    assert_eq!(specs[0].id, "litellm");
    assert_eq!(specs[0].methods, [IntegrationMethod::Key { label: Some("Manually enter API Key".into()) }]);
    let config = ConfigProviders::from_value(&json!({"anthropic": {"env": ["X_KEY"]}}));
    assert_eq!(config.integrations(|_| true)[0].methods, [IntegrationMethod::Env { names: vec!["X_KEY".into()] }]);
}

#[test]
fn defaults_custom_model_metadata_and_generates_variants() {
    let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![Box::new(ConfigProviders::from_value(&json!({
        "custom": {"package": "aisdk:@ai-sdk/openai-compatible", "models": {"chat": {}}}
    })))];
    let catalog = Catalog::build(&mut plugins, &Access::new());
    let p = catalog.provider("custom").unwrap();
    assert_eq!(p.package, "@opencode/ai/providers/openai-compatible");
    assert_eq!(p.activation, Activation::Enabled);
    let m = catalog.model("custom", "chat").unwrap();
    assert_eq!(m.capabilities, Capabilities::default());
    assert_eq!(v(&m.limit), json!({"context": 200000, "output": 32000}));
    assert_eq!(m.variants.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), ["low", "medium", "high"]);
    assert_eq!(m.package.as_deref(), Some("@opencode/ai/providers/openai-compatible"));
}

#[test]
fn preserves_catalog_capabilities_unless_overridden() {
    let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![
        providers(|e| {
            for id in ["inherited", "overridden", "partial"] {
                e.update_model("custom", id, |m| {
                    m.capabilities = Capabilities { tools: false, input: vec!["text".into()], output: vec!["text".into()] }
                });
            }
        }),
        Box::new(ConfigProviders::from_value(&json!({
            "custom": {"package": "aisdk:@ai-sdk/openai-compatible", "models": {
                "inherited": {"name": "Inherited"},
                "overridden": {"capabilities": {"tools": true, "input": ["text", "image"], "output": ["text"]}},
                "partial": {"capabilities": {"input": ["text", "image"]}},
                "demo": {"capabilities": {"input": ["text"], "output": ["text"]}},
            }}
        }))),
    ];
    let catalog = Catalog::build(&mut plugins, &Access::new());
    assert_eq!(v(&catalog.model("custom", "inherited").unwrap().capabilities), json!({"tools": false, "input": ["text"], "output": ["text"]}));
    assert_eq!(v(&catalog.model("custom", "overridden").unwrap().capabilities), json!({"tools": true, "input": ["text", "image"], "output": ["text"]}));
    assert_eq!(v(&catalog.model("custom", "partial").unwrap().capabilities), json!({"tools": false, "input": ["text", "image"], "output": ["text"]}));
    assert_eq!(v(&catalog.model("custom", "demo").unwrap().capabilities), json!({"tools": true, "input": ["text"], "output": ["text"]}));
}

#[test]
fn generates_variants_after_rewriting_configured_model_package() {
    let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![Box::new(ConfigProviders::from_value(&json!({
        "custom": {"package": "aisdk:@ai-sdk/openai", "models": {"claude": {"modelID": "claude-opus-4-8", "package": "aisdk:@ai-sdk/anthropic"}}}
    })))];
    let catalog = Catalog::build(&mut plugins, &Access::new());
    let m = catalog.model("custom", "claude").unwrap();
    assert_eq!(m.package.as_deref(), Some("@opencode/ai/providers/anthropic"));
    assert_eq!(m.variants.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), ["low", "medium", "high", "xhigh", "max"]);
}

#[test]
fn keeps_configured_and_layered_variant_bodies() {
    let body = json!({"reasoningEffort": "high", "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]});
    let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![Box::new(ConfigProviders::from_value(&json!({
        "opencode": {"package": "aisdk:@ai-sdk/openai", "settings": {"baseURL": "https://opencode.test/v1"},
            "models": {"alpha-gpt-next": {"variants": [{"id": "high", "body": body}]}}}
    })))];
    let catalog = Catalog::build(&mut plugins, &Access::new());
    assert_eq!(v(&catalog.model("opencode", "alpha-gpt-next").unwrap().variants), json!([{"id": "high", "body": body}]));

    let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![Box::new(ConfigProviders::from_documents(&[
        json!({"opencode": {"package": "aisdk:@ai-sdk/openai", "settings": {"baseURL": "https://opencode.test/v1"}}}),
        json!({"opencode": {"models": {"alpha-gpt-next": {"variants": [{"id": "high", "body": {"reasoningEffort": "high"}}]}}}}),
    ]))];
    let catalog = Catalog::build(&mut plugins, &Access::new());
    assert_eq!(v(&catalog.model("opencode", "alpha-gpt-next").unwrap().variants[0]), json!({"id": "high", "body": {"reasoningEffort": "high"}}));
}

#[test]
fn loads_configured_providers_and_applies_later_model_overrides() {
    let config = ConfigProviders::from_documents(&[
        json!({"custom": {
            "name": "Configured", "canonical": "anthropic", "env": ["CUSTOM_API_KEY"], "package": "native",
            "headers": {"first": "first", "shared": "first"},
            "models": {"chat": {
                "name": "First",
                "compatibility": {"reasoningField": "vendor_reasoning", "maxTokensField": "max_completion_tokens", "requireFinishReason": false, "supportsEffortUpdates": false, "supportsThinkingBlockBinding": false},
                "capabilities": {"tools": true, "input": ["text"], "output": ["text"]},
                "disabled": true, "limit": {"context": 100, "output": 50}, "cost": {"input": 1, "output": 2},
                "settings": {"retained": true}, "headers": {"first": "first", "shared": "first"},
                "variants": [{"id": "fast", "headers": {"first": "first", "shared": "first"}}]
            }}
        }}),
        json!({"custom": {
            "package": "aisdk:custom-sdk", "canonical": "anthropic", "settings": {"baseURL": "https://example.test"},
            "headers": {"last": "last", "shared": "last"},
            "models": {
                "default": {"name": "Default"},
                "chat": {"modelID": "api-chat", "name": "Last", "limit": {"output": 75}, "headers": {"last": "last", "shared": "last"},
                    "variants": [{"id": "fast", "headers": {"last": "last", "shared": "last"}}, {"id": "slow", "headers": {"slow": "slow"}}]}
            }
        }}),
        json!({"custom": {"name": "Renamed"}}),
    ])
    .with_default_model(ModelSelection::from_value(&json!("custom/default")));
    let specs = config.integrations(|_| false);
    assert!(specs.iter().any(|s| s.methods.contains(&IntegrationMethod::Env { names: vec!["CUSTOM_API_KEY".into()] })));
    assert_eq!(specs.last().unwrap().name.as_deref(), Some("Renamed"));

    let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![
        providers(|e| {
            e.update("anthropic", |p| p.package = "aisdk:@ai-sdk/anthropic".into());
            e.update_model("anthropic", "chat", |m| {
                m.variants = vec![Variant { id: "fast".into(), settings: Some(json!({"effort": "high"}).as_object().unwrap().clone()), ..Default::default() }]
            });
        }),
        Box::new(config),
    ];
    let catalog = Catalog::build(&mut plugins, &Access::new());
    assert_eq!(catalog.default_model().unwrap().id, "default");
    let provider = catalog.provider("custom").unwrap();
    let model = catalog.model("custom", "chat").unwrap();
    assert_eq!(provider.name, "Renamed");
    assert_eq!(provider.canonical.as_deref(), Some("anthropic"));
    assert_eq!(model.canonical.as_deref(), Some("anthropic"));
    assert_eq!(model.provider_id, "custom");
    assert_eq!(provider.activation, Activation::Enabled);
    assert_eq!(provider.package, "aisdk:custom-sdk");
    assert_eq!(model.package.as_deref(), Some("aisdk:custom-sdk"));
    assert_eq!(v(&provider.settings), json!({"baseURL": "https://example.test"}));
    assert_eq!(serde_json::to_string(&provider.headers).unwrap(), r#"{"first":"first","shared":"last","last":"last"}"#);
    assert_eq!(model.id, "chat");
    assert_eq!(model.model_id, "api-chat");
    assert_eq!(model.name, "Last");
    assert_eq!(
        v(&model.compatibility),
        json!({"reasoningField": "vendor_reasoning", "maxTokensField": "max_completion_tokens", "requireFinishReason": false, "supportsEffortUpdates": false, "supportsThinkingBlockBinding": false})
    );
    assert_eq!(v(&model.capabilities), json!({"tools": true, "input": ["text"], "output": ["text"]}));
    assert!(!model.enabled);
    assert_eq!(v(&model.limit), json!({"context": 100, "output": 75}));
    assert_eq!(v(&model.cost), json!([{"input": 1, "output": 2, "cache": {"read": 0, "write": 0}}]));
    assert_eq!(v(&model.settings), json!({"baseURL": "https://example.test", "retained": true}));
    assert_eq!(v(&model.headers), json!({"first": "first", "shared": "last", "last": "last"}));
    assert_eq!(model.variants.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), ["fast", "slow"]);
    assert_eq!(v(&model.variants[0].headers), json!({"first": "first", "shared": "last", "last": "last"}));
    assert_eq!(v(&model.variants[0].settings), json!({"effort": "high"}));
    assert_eq!(v(&model.variants[1].headers), json!({"slow": "slow"}));
}

#[test]
fn provider_base_url_clears_inherited_model_base_urls() {
    let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![
        providers(|e| {
            e.update("source", |p| p.package = "@opencode/ai/providers/openai-compatible".into());
            e.update_model("source", "m", |m| m.settings = Some(json!({"baseURL": "https://model.example", "x": 1}).as_object().unwrap().clone()));
        }),
        Box::new(ConfigProviders::from_value(&json!({
            "alias": {"canonical": "source", "settings": {"baseURL": "https://alias.example"}, "models": {"m": {}}}
        }))),
    ];
    let catalog = Catalog::build(&mut plugins, &Access::new());
    let m = catalog.model("alias", "m").unwrap();
    assert_eq!(serde_json::to_string(&m.settings).unwrap(), r#"{"baseURL":"https://alias.example","x":1}"#);
    assert_eq!(catalog.provider("alias").unwrap().canonical.as_deref(), Some("source"));
}

// ---------------------------------------------------------------------------------------------
// v1 migration
// ---------------------------------------------------------------------------------------------

#[test]
fn migrates_v1_providers() {
    let migrated = migrate_v1_providers(&json!({
        "custom": {
            "npm": "@ai-sdk/openai-compatible", "api": "https://api.example/v1", "name": "Custom", "env": ["CUSTOM_KEY"],
            "options": {"apiKey": "k", "timeout": 1000, "headers": {"x": "y"}, "body": {"trace": true}},
            "models": {"m": {
                "id": "api-m", "name": "M", "tool_call": false, "interleaved": "reasoning_content",
                "cost": {"input": 1, "output": 2, "cache_read": 0.1, "context_over_200k": {"input": 3, "output": 4}},
                "limit": {"context": 1000.7, "output": 10},
                "options": {"reasoningEffort": "high"}, "variants": {"fast": {"reasoningEffort": "low", "disabled": false}},
                "status": "deprecated", "provider": {"npm": "@ai-sdk/anthropic", "api": "https://m.example"}
            }}
        }
    }));
    assert_eq!(
        serde_json::to_string(&migrated).unwrap(),
        serde_json::to_string(&json!({"custom": {
            "name": "Custom", "env": ["CUSTOM_KEY"], "package": "aisdk:@ai-sdk/openai-compatible",
            "settings": {"timeout": 1000, "apiKey": "k", "baseURL": "https://api.example/v1"},
            "headers": {"x": "y"}, "body": {"trace": true},
            "models": {"m": {
                "modelID": "api-m", "name": "M", "compatibility": {"reasoningField": "reasoning_content"},
                "package": "aisdk:@ai-sdk/anthropic",
                "settings": {"reasoningEffort": "high", "baseURL": "https://m.example"},
                "capabilities": {"tools": false, "input": ["text", "image"], "output": ["text"]},
                "variants": [{"id": "fast", "settings": {"reasoningEffort": "low", "disabled": false}}],
                "cost": [
                    {"input": 1, "output": 2, "cache": {"read": 0.1}},
                    {"tier": {"type": "context", "size": 200000}, "input": 3, "output": 4, "cache": {}}
                ],
                "disabled": true,
                "limit": {"context": 1000, "output": 10}
            }}
        }}))
        .unwrap()
    );
}

#[test]
fn migrates_retired_provider_aliases() {
    let migrated = migrate_v1_providers(&json!({
        "azure-cognitive-services": {"npm": "@ai-sdk/openai-compatible", "env": ["AZURE_COGNITIVE_SERVICES_RESOURCE_NAME", "AZURE_KEY"], "models": {"deployment": {}}},
        "google-vertex-anthropic": {"models": {"claude": {}, "other": {"provider": {"npm": "@ai-sdk/x"}}}},
    }));
    assert_eq!(
        migrated["azure"],
        json!({"env": ["AZURE_KEY"], "package": "aisdk:@ai-sdk/openai-compatible", "settings": {"baseURL": "https://${AZURE_COGNITIVE_SERVICES_RESOURCE_NAME}.cognitiveservices.azure.com/openai"}, "models": {"deployment": {}}})
    );
    assert_eq!(
        migrated["google-vertex"],
        json!({"models": {"claude": {"package": "aisdk:@ai-sdk/google-vertex/anthropic"}, "other": {"package": "aisdk:@ai-sdk/x"}}})
    );
    // A valid retired alias survives a malformed canonical entry; a valid canonical entry wins.
    let migrated = migrate_v1_providers(&json!({"azure-cognitive-services": {"models": {"deployment": {}}}, "azure": {"env": [1]}}));
    assert!(migrated["azure"]["models"].get("deployment").is_some());
    let migrated = migrate_v1_providers(&json!({"azure-cognitive-services": {"name": "old"}, "azure": {"name": "new"}}));
    assert_eq!(migrated, json!({"azure": {"name": "new"}}));
}

#[test]
fn migrates_block_binding_opt_out_and_skips_invalid_overlays() {
    let migrated = migrate_v1_providers(&json!({"gateway": {"models": {
        "anthropic": {"options": {"thinking": {"type": "adaptive", "blockBinding": false}, "effort": "high"}},
        "bedrock": {"options": {"reasoningConfig": {"blockBinding": false}}},
        "untouched": {"options": {"thinking": {"type": "adaptive"}}},
    }}}));
    let models = &migrated["gateway"]["models"];
    assert_eq!(models["anthropic"], json!({"compatibility": {"supportsThinkingBlockBinding": false}, "settings": {"thinking": {"type": "adaptive"}, "effort": "high"}}));
    assert_eq!(models["bedrock"], json!({"compatibility": {"supportsThinkingBlockBinding": false}, "settings": {}}));
    assert_eq!(models["untouched"], json!({"settings": {"thinking": {"type": "adaptive"}}}));

    let migrated = migrate_v1_providers(&json!({
        "headers": {"options": {"headers": {"valid": "yes", "invalid": 1}}},
        "body": {"options": {"body": "not-an-object"}},
        "valid": {"options": {"headers": {"valid": "yes"}, "body": {"trace": true}}},
    }));
    assert_eq!(migrated, json!({"valid": {"settings": {}, "headers": {"valid": "yes"}, "body": {"trace": true}}}));
}

#[test]
fn model_selection_parsing() {
    assert_eq!(
        ModelSelection::from_value(&json!("anthropic/claude#high")),
        Some(ModelSelection { provider_id: "anthropic".into(), model: "claude".into(), variant: Some("high".into()) })
    );
    assert_eq!(
        ModelSelection::from_value(&json!({"providerID": "a", "model": "b"})),
        Some(ModelSelection { provider_id: "a".into(), model: "b".into(), variant: None })
    );
    assert_eq!(ModelSelection::from_value(&json!("nope")), None);
}
