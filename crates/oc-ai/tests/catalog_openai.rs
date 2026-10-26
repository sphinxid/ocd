//! The OpenAI sign-in plugins' catalog effects (`packages/core/test/plugin/provider-openai.test.ts`,
//! `provider-chatgpt.test.ts`, `provider-openai-context-caps.test.ts`): the Codex model filter, base URL
//! and headers, token sharing's HTTP route and fallback models, cost zeroing, the ChatGPT input caps,
//! the compaction policy and mid-session effort switches.

use oc_ai::catalog::openai::{
    CODEX_BASE_URL, ChatGpt, FALLBACK_MODELS, OpenAi, OpenAiContextCaps, SignIn, TOKEN_SHARING_BASE_URL, legacy_token_sharing,
};
use oc_ai::catalog::plugins::ResponsesWebSockets;
use oc_ai::catalog::registry::{Access, Catalog, CatalogPlugin, ModelEditor, ProviderEditor};
use oc_ai::catalog::types::{Cost, CostCache, Limit, Variant};
use oc_ai::credentials::CredentialValue;
use oc_ai::json::Object;
use oc_ai::testing::compile_body;
use oc_ai::types::{ContentPart, LLMRequest, Message};
use serde_json::{Value, json};

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

fn obj(value: Value) -> Object {
    value.as_object().unwrap().clone()
}

const FULL: Limit = Limit { context: 1_050_000, input: Some(922_000), output: 128_000 };

fn paid() -> Vec<Cost> {
    vec![Cost { tier: None, input: 1.0, output: 2.0, cache: CostCache { read: 0.1, write: 0.0 } }]
}

const CONNECTION: &str = "credential:crd_sharing";

fn codex() -> SignIn {
    SignIn::Codex { account_id: Some("acct_123".into()) }
}

fn sharing() -> SignIn {
    SignIn::TokenSharing { connection: CONNECTION.into() }
}

/// The OpenAI, ChatGPT and context caps plugins in upstream order around a catalog setup.
fn build(sign_in: SignIn, setup: Box<dyn CatalogPlugin + '_>, caps: bool, after: Vec<Box<dyn CatalogPlugin + '_>>) -> Catalog {
    let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![
        setup,
        Box::new(ChatGpt { sign_in: sign_in.clone(), sections: vec![] }),
        Box::new(ResponsesWebSockets),
        Box::new(OpenAi { sign_in: sign_in.clone(), sections: vec![] }),
    ];
    if caps {
        plugins.push(Box::new(OpenAiContextCaps { sign_in: sign_in.clone() }));
    }
    plugins.extend(after);
    let access = match &sign_in {
        SignIn::TokenSharing { connection } => Access::new().integration("openai", vec![connection.clone()]),
        SignIn::Codex { .. } => Access::new().integration("openai", vec!["credential:crd_codex".into()]),
        SignIn::None => Access::new().integration("openai", vec!["credential:crd_key".into()]),
    };
    Catalog::build(&mut plugins, &access)
}

/// The catalog of the codex filter tests (`provider-openai.test.ts`).
fn codex_setup<'a>() -> Box<dyn CatalogPlugin + 'a> {
    providers(|e| {
        e.update("openai", |p| p.package = "@opencode/ai/providers/openai".into());
        e.update_model("openai", "gpt-5.5", |_| {});
        e.update_model("openai", "gpt-5.3-codex-spark", |_| {});
        e.update_model("openai", "gpt-5.6-sol", |m| {
            m.limit = FULL;
            m.cost = paid();
        });
        e.update_model("openai", "gpt-5.5-pro", |_| {});
        e.update_model("openai", "gpt-5.4", |m| m.limit = Limit { context: 1_050_000, input: Some(922_000), output: 64_000 });
        e.update_model("openai", "gpt-5.4-pro", |m| {
            m.model_id = "gpt-5.4".into();
            m.body = Some(obj(json!({"reasoning": {"mode": "pro"}})));
        });
        e.update_model("openai", "gpt-5.6", |_| {});
        e.update_model("openai", "gpt-4.1", |_| {});
        for id in ["gpt-6-astra", "gpt-5.10"] {
            e.update_model("openai", id, |m| m.limit = FULL);
        }
        for id in ["gpt-5", "gpt-5.04-astra", "gpt-4.99"] {
            e.update_model("openai", id, |_| {});
        }
    })
}

#[test]
fn filters_the_openai_catalog_to_codex_eligible_models_under_a_chatgpt_connection() {
    let catalog = build(codex(), codex_setup(), false, vec![]);
    let provider = catalog.provider("openai").unwrap();
    assert_eq!(provider.package, "@opencode/ai/providers/openai");
    let settings = provider.settings.as_ref().unwrap();
    assert_eq!(settings["baseURL"], CODEX_BASE_URL);
    assert_eq!(settings["encoding"], "zstd");
    assert_eq!(settings["transport"], "websocket");
    let headers = provider.headers.as_ref().unwrap();
    assert_eq!(headers["originator"], "opencode");
    assert_eq!(headers["chatgpt-account-id"], "acct_123");
    assert_eq!(headers["x-codex-beta-features"], "remote_compaction_v2");

    let eligible = catalog.model("openai", "gpt-5.6-sol").unwrap();
    assert_eq!(eligible.package.as_deref(), Some("@opencode/ai/providers/openai"));
    assert_eq!(eligible.headers.as_ref().unwrap()["originator"], "opencode");
    assert_eq!(eligible.headers.as_ref().unwrap()["chatgpt-account-id"], "acct_123");
    assert!(eligible.cost.is_empty());
    assert_eq!(eligible.limit, FULL);
    assert!(eligible.enabled);
    for id in ["gpt-6-astra", "gpt-5.10"] {
        assert!(catalog.model("openai", id).unwrap().enabled, "{id}");
    }
    for id in [
        "gpt-5.5",
        "gpt-5.3-codex-spark",
        "gpt-5.5-pro",
        "gpt-5.4-pro",
        "gpt-5.4",
        "gpt-5.6",
        "gpt-4.1",
        "gpt-5",
        "gpt-5.04-astra",
        "gpt-4.99",
    ] {
        assert!(!catalog.model("openai", id).unwrap().enabled, "{id}");
    }
}

#[test]
fn keeps_the_full_openai_catalog_under_an_api_key_connection() {
    let setup = providers(|e| {
        e.update("openai", |p| p.package = "@opencode/ai/providers/openai".into());
        e.update_model("openai", "gpt-5.5", |m| m.limit = FULL);
        e.update_model("openai", "gpt-4.1", |_| {});
    });
    let catalog = build(SignIn::None, setup, true, vec![]);
    let provider = catalog.provider("openai").unwrap();
    let model = catalog.model("openai", "gpt-5.5").unwrap();
    assert_eq!(model.package.as_deref(), Some("@opencode/ai/providers/openai"));
    assert!(model.enabled);
    assert_eq!(model.limit, FULL);
    let settings = provider.settings.as_ref().unwrap();
    assert_eq!(settings["transport"], "websocket");
    assert!(settings.get("encoding").is_none());
    assert!(settings.get("baseURL").is_none());
    assert!(model.settings.as_ref().is_none_or(|s| s.get("transport").is_none()));
    assert!(provider.headers.as_ref().is_none_or(|h| !h.contains_key("originator") && !h.contains_key("x-codex-beta-features")));
    assert!(catalog.model("openai", "gpt-4.1").unwrap().enabled);
}

/// The catalog of the token-sharing route test (`provider-chatgpt.test.ts`).
fn sharing_setup<'a>() -> Box<dyn CatalogPlugin + 'a> {
    providers(|e| {
        e.update("openai", |p| {
            p.package = "@opencode/ai/providers/openai".into();
            p.settings = Some(obj(json!({"transport": "websocket"})));
        });
        e.update_model("openai", "gpt-5.5", |m| {
            m.limit = FULL;
            m.cost = paid();
        });
        e.update_model("openai", "gpt-5.5-pro", |_| {});
        e.update_model("openai", "gpt-5.4", |m| m.limit = Limit { context: 1_050_000, input: Some(922_000), output: 64_000 });
        e.update_model("openai", "gpt-5.4-pro", |m| {
            m.model_id = "gpt-5.4".into();
            m.body = Some(obj(json!({"reasoning": {"mode": "pro"}})));
        });
        e.update_model("openai", "gpt-5.6", |_| {});
        e.update_model("openai", "gpt-5.6-sol", |m| m.limit = FULL);
        e.update_model("openai", "gpt-4.1", |_| {});
        e.update_model("openai", "gpt-6-astra", |m| m.limit = FULL);
        e.update_model("openai", "gpt-5.10", |m| m.limit = FULL);
        for id in ["gpt-5", "gpt-5.04-astra", "gpt-4.99"] {
            e.update_model("openai", id, |_| {});
        }
        for id in [
            "gpt-5.5-fast",
            "gpt-5.6-luna",
            "gpt-5.6-luna-fast",
            "gpt-5.6-sol-fast",
            "gpt-5.6-terra",
            "gpt-5.6-terra-fast",
            "gpt-6-astra-fast",
            "gpt-6-luna",
            "gpt-6-luna-fast",
            "gpt-6-sol",
            "gpt-6-sol-fast",
            "gpt-6.1-sol",
            "gpt-6.1-sol-fast",
        ] {
            e.update_model("openai", id, |_| {});
        }
    })
}

#[test]
fn routes_chatgpt_over_http_with_only_the_listed_catalog_fallback_models() {
    let catalog = build(sharing(), sharing_setup(), false, vec![]);
    let provider = catalog.provider("openai").unwrap();
    assert_eq!(provider.package, "@opencode/ai/providers/openai");
    let settings = provider.settings.as_ref().unwrap();
    assert_eq!(settings["transport"], "http");
    assert_eq!(settings["baseURL"], TOKEN_SHARING_BASE_URL);
    assert_eq!(settings["encoding"], "zstd");
    assert_eq!(settings["compaction"], json!({"type": "summary"}));
    assert!(provider.headers.as_ref().is_none_or(|h| !h.contains_key("originator")));
    assert_eq!(catalog.record("openai").unwrap().source_connection.as_deref(), Some(CONNECTION));

    let eligible = catalog.model("openai", "gpt-5.5").unwrap();
    assert_eq!(eligible.package.as_deref(), Some("@opencode/ai/providers/openai"));
    assert!(eligible.cost.is_empty());
    assert_eq!(eligible.limit, FULL);
    assert!(eligible.enabled);
    assert_eq!(eligible.compatibility.as_ref().unwrap().supports_effort_updates, Some(false));
    assert_eq!(eligible.settings.as_ref().unwrap()["compaction"], json!({"type": "summary"}));
    let gpt56 = catalog.model("openai", "gpt-5.6-sol").unwrap();
    assert!(gpt56.enabled);
    assert_eq!(gpt56.limit, FULL);
    assert!(catalog.model("openai", "gpt-6-astra").unwrap().enabled);
    for id in ["gpt-5.5-pro", "gpt-5.4-pro", "gpt-5.4", "gpt-5.6", "gpt-4.1", "gpt-5.10", "gpt-5", "gpt-5.04-astra", "gpt-4.99"] {
        assert!(catalog.model("openai", id).is_none(), "{id}");
    }
    let mut available: Vec<String> =
        catalog.available_models().into_iter().filter(|m| m.provider_id == "openai").map(|m| m.id.clone()).collect();
    available.sort();
    let mut expected: Vec<String> = FALLBACK_MODELS.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(available, expected);
}

#[test]
fn token_sharing_provider_is_bound_to_its_connection() {
    let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![sharing_setup(), Box::new(ChatGpt { sign_in: sharing(), sections: vec![] })];
    // Another connection became active: the token-sharing inventory is not offered under it.
    let other = Access::new().integration("openai", vec!["credential:crd_other".into(), CONNECTION.into()]);
    let catalog = Catalog::build(&mut plugins, &other);
    assert!(!catalog.is_available("openai"));
}

#[test]
fn a_configured_base_url_keeps_requests_uncompressed() {
    let sections = vec![json!({"openai": {"settings": {"baseURL": "http://127.0.0.1:9/v1"}}})];
    for sign_in in [codex(), sharing()] {
        let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![
            sharing_setup(),
            Box::new(ChatGpt { sign_in: sign_in.clone(), sections: sections.clone() }),
            Box::new(OpenAi { sign_in: sign_in.clone(), sections: sections.clone() }),
        ];
        let access = Access::new().integration("openai", vec![CONNECTION.into()]);
        let catalog = Catalog::build(&mut plugins, &access);
        let settings = catalog.provider("openai").unwrap().settings.clone().unwrap();
        assert!(settings.get("encoding").is_none(), "{sign_in:?}");
    }
    // An encoding the provider already has is kept.
    let setup = providers(|e| {
        e.update("openai", |p| p.settings = Some(obj(json!({"encoding": "gzip"}))));
        e.update_model("openai", "gpt-6.1-sol", |_| {});
    });
    let catalog = build(codex(), setup, false, vec![]);
    assert_eq!(catalog.provider("openai").unwrap().settings.as_ref().unwrap()["encoding"], "gzip");
}

fn caps_setup<'a>() -> Box<dyn CatalogPlugin + 'a> {
    providers(|e| {
        for id in ["gpt-5.5", "gpt-5.6-sol", "gpt-6-luna", "gpt-6.1-sol", "gpt-7"] {
            e.update_model("openai", id, |m| m.limit = FULL);
        }
        e.update_model("openai", "gpt-6.1-sol-fast", |m| {
            m.model_id = "gpt-6.1-sol".into();
            m.limit = FULL;
        });
        e.update_model("openai", "gpt-6.2-mini", |m| m.limit = Limit { context: 400_000, input: Some(272_000), output: 128_000 });
    })
}

#[test]
fn caps_openai_models_under_every_chatgpt_method() {
    let legacy = Limit { context: 400_000, input: Some(272_000), output: 128_000 };
    let current = Limit { context: 628_000, input: Some(500_000), output: 128_000 };
    for method in ["chatgpt-browser", "chatgpt-headless", "chatgpt-token-sharing"] {
        let credential = CredentialValue::Oauth {
            method_id: method.into(),
            refresh: "refresh".into(),
            access: "chatgpt-token".into(),
            expires: u64::MAX,
            metadata: None,
        };
        let sign_in = SignIn::from_connection(Some(CONNECTION), Some(&credential));
        assert!(sign_in.chatgpt(), "{method}");
        let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![caps_setup(), Box::new(OpenAiContextCaps { sign_in })];
        let catalog = Catalog::build(&mut plugins, &Access::new());
        let limit = |id: &str| catalog.model("openai", id).unwrap().limit;
        assert_eq!(limit("gpt-5.5"), legacy, "{method}");
        assert_eq!(limit("gpt-5.6-sol"), legacy, "{method}");
        assert_eq!(limit("gpt-6-luna"), legacy, "{method}");
        assert_eq!(limit("gpt-6.1-sol"), current, "{method}");
        assert_eq!(limit("gpt-6.1-sol-fast"), current, "{method}");
        assert_eq!(limit("gpt-7"), current, "{method}");
        assert_eq!(limit("gpt-6.2-mini"), legacy, "{method}");
    }
}

#[test]
fn keeps_catalog_limits_under_an_api_key() {
    let key = CredentialValue::key("sk-test");
    let sign_in = SignIn::from_connection(Some("credential:crd_key"), Some(&key));
    assert_eq!(sign_in, SignIn::None);
    let mut plugins: Vec<Box<dyn CatalogPlugin>> = vec![caps_setup(), Box::new(OpenAiContextCaps { sign_in })];
    let catalog = Catalog::build(&mut plugins, &Access::new());
    assert_eq!(catalog.model("openai", "gpt-5.6-sol").unwrap().limit, FULL);
    assert_eq!(catalog.model("openai", "gpt-6.1-sol").unwrap().limit, FULL);
}

#[test]
fn loads_the_sign_in_from_the_active_connection() {
    let oauth = |method: &str, metadata: Option<Value>| CredentialValue::Oauth {
        method_id: method.into(),
        refresh: "r".into(),
        access: "a".into(),
        expires: 1,
        metadata: metadata.map(obj),
    };
    let browser = oauth("chatgpt-browser", Some(json!({"accountID": "acct_1"})));
    assert_eq!(SignIn::from_connection(Some("credential:a"), Some(&browser)), SignIn::Codex { account_id: Some("acct_1".into()) });
    let headless = oauth("chatgpt-headless", None);
    assert_eq!(SignIn::from_connection(Some("credential:a"), Some(&headless)), SignIn::Codex { account_id: None });
    let shared = oauth("chatgpt-token-sharing", Some(json!({"clientID": "oaiapp_issued"})));
    assert_eq!(SignIn::from_connection(Some("credential:a"), Some(&shared)), SignIn::TokenSharing { connection: "credential:a".into() });
    assert_eq!(SignIn::from_connection(Some("credential:a"), None), SignIn::None);
    assert_eq!(SignIn::from_connection(None, Some(&shared)), SignIn::None);
    assert_eq!(SignIn::from_connection(Some("credential:a"), Some(&oauth("other", None))), SignIn::None);
}

#[test]
fn recognizes_token_sharing_credentials_saved_under_the_browser_method() {
    let snapshot = json!([{
        "slug": "gpt-5.5", "display_name": "GPT-5.5", "visibility": "list", "supported_in_api": true,
        "context_window": 272_000, "input_modalities": ["text"],
    }]);
    let sharing = obj(json!({"clientID": "oaiapp_issued", "models": snapshot}));
    let (client, models, rest) = legacy_token_sharing("chatgpt-browser", Some(&sharing)).unwrap();
    assert_eq!(client, "oaiapp_issued");
    assert_eq!(models, Some(snapshot));
    assert_eq!(Value::Object(rest), json!({"clientID": "oaiapp_issued"}));
    // Codex credentials (no client id), other methods and undecodable metadata stay.
    assert!(legacy_token_sharing("chatgpt-browser", Some(&obj(json!({"accountID": "acct_123"})))).is_none());
    assert!(legacy_token_sharing("chatgpt-token-sharing", Some(&sharing)).is_none());
    assert!(legacy_token_sharing("chatgpt-browser", Some(&obj(json!({"clientID": "c", "models": [{"slug": 1}]})))).is_none());
    assert!(legacy_token_sharing("chatgpt-browser", Some(&obj(json!({"clientID": "c", "scopes": "openid"})))).is_none());
}

/// `{chatgpt,key} compaction uses plugin defaults but allows later model and variant overrides`.
#[test]
fn compaction_uses_plugin_defaults_but_allows_later_model_and_variant_overrides() {
    let oauth = CredentialValue::Oauth {
        method_id: "chatgpt-token-sharing".into(),
        refresh: "refresh".into(),
        access: "chatgpt-token".into(),
        expires: u64::MAX,
        metadata: Some(obj(json!({"clientID": "oaiapp_issued"}))),
    };
    let key = CredentialValue::key("sk-test");
    for (sign_in, credential) in [(sharing(), oauth), (SignIn::None, key)] {
        let chatgpt = sign_in != SignIn::None;
        let setup = || {
            providers(|e| {
                e.update("openai", |p| p.package = "@opencode/ai/providers/openai/responses".into());
                // Token sharing keeps only its fallback models: gpt-6-astra and gpt-6.1-sol stand in for
                // upstream's gpt-5.6-sol and gpt-6-astra, all pointing at gpt-5.5.
                for id in ["gpt-5.5", "gpt-6-astra", "gpt-6.1-sol"] {
                    e.update_model("openai", id, |m| m.model_id = "gpt-5.5".into());
                }
                e.update_model("openai", "gpt-6-astra", |m| m.settings = Some(obj(json!({"compaction": {"type": "native"}}))));
            })
        };
        let resolve = |catalog: &Catalog, id: &str, variant: Option<&str>| {
            catalog.resolve("openai", id, variant, Some(&credential)).unwrap().compaction
        };
        let summary = Some("summary".to_string());
        let native = Some("native".to_string());

        let catalog = build(sign_in.clone(), setup(), false, vec![]);
        assert_eq!(resolve(&catalog, "gpt-5.5", None), if chatgpt { summary.clone() } else { None });
        assert_eq!(resolve(&catalog, "gpt-6-astra", None), if chatgpt { summary.clone() } else { native.clone() });

        // A later provider default does not override the ChatGPT model policy.
        let later = providers(|e| e.update("openai", |p| p.settings = Some(obj(json!({"compaction": {"type": "native"}})))));
        let catalog = build(sign_in.clone(), setup(), false, vec![later]);
        assert_eq!(resolve(&catalog, "gpt-5.5", None), if chatgpt { summary.clone() } else { native.clone() });

        let provider = providers(|e| e.update("openai", |p| p.settings = Some(obj(json!({"compaction": {"type": "summary"}})))));
        let overrides = models(|e| {
            e.update("openai", "gpt-6-astra", |m| m.settings = Some(obj(json!({"compaction": {"type": "native"}}))));
            e.update("openai", "gpt-6.1-sol", |m| {
                m.settings = Some(obj(json!({"compaction": {"type": "summary"}})));
                m.variants = vec![Variant {
                    id: "high".into(),
                    settings: Some(obj(json!({"compaction": {"type": "native"}}))),
                    ..Default::default()
                }];
            });
        });
        let catalog = build(sign_in.clone(), setup(), false, vec![provider, overrides]);
        assert_eq!(resolve(&catalog, "gpt-5.5", None), summary);
        assert_eq!(resolve(&catalog, "gpt-6-astra", None), native);
        assert_eq!(resolve(&catalog, "gpt-6.1-sol", Some("high")), native);
    }
}

/// `{chatgpt,key} sends mid-session reasoning effort switches in a form its backend accepts`.
#[tokio::test]
async fn sends_mid_session_reasoning_effort_switches_in_a_form_its_backend_accepts() {
    let oauth = CredentialValue::Oauth {
        method_id: "chatgpt-token-sharing".into(),
        refresh: "refresh".into(),
        access: "chatgpt-token".into(),
        expires: u64::MAX,
        metadata: Some(obj(json!({"clientID": "oaiapp_issued"}))),
    };
    let key = CredentialValue::key("sk-test");
    let user = |text: &str| json!({"role": "user", "content": [{"type": "input_text", "text": text}]});
    let assistant = |text: &str| json!({"role": "assistant", "content": [{"type": "output_text", "text": text}]});
    let update = |effort: &str| json!({"type": "configuration_update", "reasoning": {"effort": effort}});
    for (sign_in, credential) in [(sharing(), oauth), (SignIn::None, key)] {
        let chatgpt = sign_in != SignIn::None;
        let setup = providers(|e| {
            e.update("openai", |p| p.package = "@opencode/ai/providers/openai".into());
            e.update_model("openai", "gpt-6.1-sol", |m| {
                m.variants = ["high", "max"]
                    .iter()
                    .map(|effort| Variant {
                        id: effort.to_string(),
                        settings: Some(obj(json!({"reasoningEffort": effort}))),
                        ..Default::default()
                    })
                    .collect();
            });
        });
        let catalog = build(sign_in, setup, false, vec![]);
        let body = async |variant: &str, messages: Vec<Message>| {
            let resolved = catalog.resolve("openai", "gpt-6.1-sol", Some(variant), Some(&credential)).unwrap();
            let mut req = LLMRequest::new(resolved.language_model);
            req.messages = messages;
            let body = compile_body(&req).await.unwrap();
            let input: Vec<Value> = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| {
                    let mut item = item.clone();
                    // Upstream matches a subset (`toMatchObject`): the message `type` and an assistant's
                    // `status` are not part of it.
                    if let Some(o) = item.as_object_mut() {
                        o.remove("type").filter(|t| t != "message").map(|t| o.insert("type".into(), t));
                        o.remove("status");
                    }
                    item
                })
                .collect();
            (body["reasoning"]["effort"].clone(), input)
        };
        let text = |role: &str, value: &str| {
            if role == "user" { Message::user_text(value) } else { Message::assistant(vec![ContentPart::text(value)]) }
        };
        let switched =
            vec![text("user", "a"), text("assistant", "b"), Message::effort(Some("max".into()), Some("high".into())), text("user", "c")];
        let mut switched_back = switched.clone();
        switched_back.extend([text("assistant", "d"), Message::effort(Some("high".into()), Some("max".into())), text("user", "e")]);
        let first = body("max", switched).await;
        let second = body("high", switched_back).await;
        if chatgpt {
            assert_eq!(first, (json!("max"), vec![user("a"), assistant("b"), user("c")]));
            assert_eq!(second, (json!("high"), vec![user("a"), assistant("b"), user("c"), assistant("d"), user("e")]));
        } else {
            assert_eq!(first, (json!("high"), vec![user("a"), assistant("b"), update("max"), user("c")]));
            assert_eq!(
                second,
                (json!("high"), vec![user("a"), assistant("b"), update("max"), user("c"), assistant("d"), update("high"), user("e")])
            );
        }
    }
}
