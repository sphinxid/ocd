//! Port of `packages/core/test/aisdk-native.test.ts`.

use oc_ai::catalog::native::{native, rewrite, NativeContext, RewriteTarget};
use oc_ai::catalog::types::HeaderMap;
use oc_ai::json::Object;
use serde_json::{json, Map, Value};

/// `map(packageName, settings, modelID, providerID)`: rewrite an `aisdk:` target; `None` when unmapped.
fn map_full(package: &str, settings: Value, model_id: &str, provider_id: &str) -> Option<Value> {
    let mut pkg = Some(format!("aisdk:{package}"));
    let specifier = pkg.clone().unwrap();
    let mut s: Option<Object> = Some(settings.as_object().unwrap().clone());
    let mut headers: Option<HeaderMap> = None;
    let mut body: Option<Object> = None;
    rewrite(
        RewriteTarget { package: &mut pkg, settings: &mut s, headers: &mut headers, body: &mut body, variants: None },
        Some(&specifier),
        &NativeContext { provider_id, model_id: Some(model_id), ..Default::default() },
    );
    if pkg.as_deref().map(|p| p.starts_with("aisdk:")).unwrap_or(true) {
        return None;
    }
    let mut out = Map::new();
    out.insert("package".into(), json!(pkg));
    if let Some(s) = s {
        out.insert("settings".into(), Value::Object(s));
    }
    if let Some(h) = headers {
        out.insert("headers".into(), serde_json::to_value(h).unwrap());
    }
    if let Some(b) = body {
        out.insert("body".into(), Value::Object(b));
    }
    Some(Value::Object(out))
}

fn map(package: &str, settings: Value) -> Option<Value> {
    map_full(package, settings, "test-model", "test-provider")
}

#[test]
fn keeps_cloudflare_ai_gateway_on_native_gateway() {
    for package in ["ai-gateway-provider", "@ai-sdk/openai", "@ai-sdk/anthropic", "@ai-sdk/openai-compatible"] {
        assert_eq!(
            map_full(package, json!({}), "openai/gpt-5.4", "cloudflare-ai-gateway").unwrap()["package"],
            "@opencode/ai/providers/cloudflare-ai-gateway"
        );
    }
}

#[test]
fn maps_vercel_ai_gateway() {
    assert_eq!(
        map_full(
            "@ai-sdk/gateway",
            json!({"apiKey": "secret", "headers": {"x-gateway": "test"}, "extraBody": {"custom": true}, "gateway": {"order": ["anthropic"]}, "reasoningEffort": "high"}),
            "anthropic/claude-sonnet-5.5",
            "vercel"
        ),
        Some(json!({
            "package": "@opencode/ai/providers/vercel-ai-gateway",
            "settings": {"apiKey": "secret", "gateway": {"order": ["anthropic"]}, "reasoningEffort": "high"},
            "headers": {"x-gateway": "test"},
            "body": {"custom": true},
        }))
    );
}

#[test]
fn maps_openai_family_packages() {
    assert_eq!(
        map(
            "@ai-sdk/openai",
            json!({"apiKey": "secret", "baseURL": "https://api.meta.ai/v1", "organization": "org", "reasoningEffort": "xhigh", "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"], "truncation": "auto"})
        ),
        Some(json!({
            "package": "@opencode/ai/providers/openai",
            "settings": {"apiKey": "secret", "baseURL": "https://api.meta.ai/v1", "reasoningEffort": "xhigh", "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"], "truncation": "auto", "organization": "org"},
        }))
    );
    let compatible = map("@ai-sdk/openai-compatible", json!({"baseURL": "https://example.com/v1", "reasoningEffort": "high"})).unwrap();
    assert_eq!(
        compatible,
        json!({"package": "@opencode/ai/providers/openai-compatible", "settings": {"baseURL": "https://example.com/v1", "provider": "test-provider", "reasoningEffort": "high"}})
    );
    // `provider` is appended after the translated settings.
    assert_eq!(
        serde_json::to_string(&compatible["settings"]).unwrap(),
        r#"{"baseURL":"https://example.com/v1","reasoningEffort":"high","provider":"test-provider"}"#
    );
}

#[test]
fn maps_anthropic_settings() {
    let settings = json!({"authToken": "token", "baseURL": "https://anthropic.example/v1", "thinking": {"type": "adaptive", "display": "summarized"}, "effort": "high"});
    assert_eq!(map("@ai-sdk/anthropic", settings.clone()), Some(json!({"package": "@opencode/ai/providers/anthropic", "settings": settings})));
}

#[test]
fn maps_simple_chat_providers() {
    for name in ["cerebras", "deepinfra", "groq", "togetherai"] {
        assert_eq!(
            map(
                &format!("@ai-sdk/{name}"),
                json!({"apiKey": "secret", "baseURL": format!("https://{name}.example/v1"), "headers": {"x-provider": name}, "reasoningEffort": "high", "customOption": {"enabled": true}})
            ),
            Some(json!({
                "package": format!("@opencode/ai/providers/{name}"),
                "settings": {"apiKey": "secret", "baseURL": format!("https://{name}.example/v1"), "reasoningEffort": "high", "customOption": {"enabled": true}},
                "headers": {"x-provider": name},
            }))
        );
        assert_eq!(map(&format!("@ai-sdk/{name}"), json!({})), Some(json!({"package": format!("@opencode/ai/providers/{name}"), "settings": {}})));
    }
}

#[test]
fn maps_google_vertex_and_mistral() {
    let vertex = json!({"project": "project", "location": "us-central1", "labels": {"environment": "test"}, "thinkingConfig": {"thinkingLevel": "high"}});
    assert_eq!(map("@ai-sdk/google-vertex", vertex.clone()), Some(json!({"package": "@opencode/ai/providers/google-vertex", "settings": vertex})));
    assert_eq!(
        map(
            "@ai-sdk/mistral",
            json!({"apiKey": "secret", "baseURL": "https://mistral.example/v1", "headers": {"x-provider": "mistral"}, "extraBody": {"custom": {"enabled": true}}, "safePrompt": false, "documentImageLimit": 4, "documentPageLimit": 12, "parallelToolCalls": false, "promptCacheKey": "session-123", "reasoningEffort": "high", "promptMode": "reasoning"})
        ),
        Some(json!({
            "package": "@opencode/ai/providers/mistral",
            "settings": {"apiKey": "secret", "baseURL": "https://mistral.example/v1", "safePrompt": false, "documentImageLimit": 4, "documentPageLimit": 12, "parallelToolCalls": false, "promptCacheKey": "session-123", "reasoningEffort": "high", "promptMode": "reasoning"},
            "headers": {"x-provider": "mistral"},
            "body": {"custom": {"enabled": true}},
        }))
    );
}

#[test]
fn maps_both_cohere_packages() {
    assert_eq!(
        map_full("@ai-sdk/cohere", json!({"apiKey": "secret", "thinking": {"type": "enabled", "tokenBudget": 1024}}), "command-a-reasoning-08-2025", "cohere"),
        Some(json!({"package": "@opencode/ai/providers/cohere", "settings": {"apiKey": "secret", "thinking": {"type": "enabled", "tokenBudget": 1024}}}))
    );
    assert_eq!(
        map_full("@ai-sdk/openai-compatible", json!({"baseURL": "https://api.cohere.ai/compatibility/v1", "reasoningEffort": "high"}), "north-mini-code-1-0", "cohere"),
        Some(json!({"package": "@opencode/ai/providers/cohere/chat", "settings": {"baseURL": "https://api.cohere.ai/compatibility/v1", "reasoningEffort": "high"}}))
    );
}

#[test]
fn maps_both_bedrock_packages() {
    assert_eq!(
        map("@ai-sdk/amazon-bedrock", json!({"region": "us-east-1"})),
        Some(json!({"package": "@opencode/ai/providers/amazon-bedrock", "settings": {"region": "us-east-1"}}))
    );
    assert_eq!(
        map_full("@ai-sdk/amazon-bedrock/mantle", json!({"region": "us-east-1"}), "openai.gpt-oss-120b", "test-provider"),
        Some(json!({"package": "@opencode/ai/providers/amazon-bedrock/mantle/chat", "settings": {"region": "us-east-1"}}))
    );
}

#[test]
fn maps_azure_deployments() {
    let settings = json!({"apiKey": "secret", "resourceName": "resource", "apiVersion": "2025-01-01-preview", "queryParams": {"feature": "enabled"}, "useDeploymentBasedUrls": true, "reasoningEffort": "high"});
    assert_eq!(
        map_full("@ai-sdk/azure", settings.clone(), "deployment", "test-provider"),
        Some(json!({"package": "@opencode/ai/providers/azure/responses", "settings": settings}))
    );
    let mut chat_settings = settings.clone();
    chat_settings["useCompletionUrls"] = json!(true);
    let chat = map_full("@ai-sdk/azure", chat_settings, "custom-deployment", "test-provider").unwrap();
    assert_eq!(chat["package"], "@opencode/ai/providers/azure/chat");
    assert!(chat["settings"].get("useCompletionUrls").is_none());
    assert_eq!(
        native("@ai-sdk/azure", &NativeContext { provider_id: "azure", shape: Some("completions"), ..Default::default() }).as_deref(),
        Some("@opencode/ai/providers/azure/chat")
    );
}

#[test]
fn maps_bedrock_request_options() {
    assert_eq!(
        map_full(
            "@ai-sdk/amazon-bedrock",
            json!({
                "region": "us-east-1",
                "topP": 0.8,
                "headers": {"x-test": "value"},
                "additionalModelRequestFields": {"existing": true, "anthropic_beta": ["existing-beta"], "output_config": {"format": "text"}},
                "reasoningConfig": {"type": "adaptive", "display": "summarized", "maxReasoningEffort": "high"},
                "anthropicBeta": ["context-1m-2025-08-07"],
                "serviceTier": "priority",
            }),
            "anthropic.claude-sonnet-4-6-v1",
            "test-provider"
        ),
        Some(json!({
            "package": "@opencode/ai/providers/amazon-bedrock",
            "settings": {"region": "us-east-1", "topP": 0.8},
            "headers": {"x-test": "value"},
            "body": {
                "additionalModelRequestFields": {
                    "existing": true,
                    "anthropic_beta": ["existing-beta", "context-1m-2025-08-07"],
                    "thinking": {"type": "adaptive", "display": "summarized"},
                    "output_config": {"format": "text", "effort": "high"},
                },
                "serviceTier": {"type": "priority"},
            },
        }))
    );
    assert_eq!(
        map_full("@ai-sdk/amazon-bedrock", json!({"reasoningConfig": {"type": "enabled", "maxReasoningEffort": "max"}}), "amazon.nova-2-lite-v1:0", "p")
            .unwrap()["body"],
        json!({"additionalModelRequestFields": {"reasoningConfig": {"type": "enabled", "maxReasoningEffort": "max"}}})
    );
    assert_eq!(
        map_full(
            "@ai-sdk/amazon-bedrock",
            json!({"reasoningConfig": {"type": "enabled", "budgetTokens": 12000}}),
            "anthropic.claude-sonnet-4-5-20250929-v1:0",
            "p"
        ),
        Some(json!({"package": "@opencode/ai/providers/amazon-bedrock", "settings": {"thinking": {"type": "enabled", "budgetTokens": 12000}}}))
    );
    assert_eq!(
        map_full("@ai-sdk/amazon-bedrock", json!({"reasoningConfig": {"maxReasoningEffort": "high"}}), "openai.gpt-oss-120b-1:0", "p").unwrap()["body"],
        json!({"additionalModelRequestFields": {"reasoning_effort": "high"}})
    );
    for model_id in ["global.openai.gpt-5.6-sol", "us.openai.gpt-5.6-sol", "us.openai.gpt-6-astra"] {
        assert_eq!(
            map_full("@ai-sdk/amazon-bedrock", json!({"reasoningConfig": {"maxReasoningEffort": "none"}}), model_id, "p").unwrap()["body"],
            json!({"additionalModelRequestFields": {"reasoning": {"effort": "none"}}})
        );
    }
    assert_eq!(
        map_full(
            "@ai-sdk/amazon-bedrock",
            json!({"reasoningConfig": {"maxReasoningEffort": "high"}, "additionalModelRequestFields": {"reasoning": {"summary": "auto"}}}),
            "us.openai.gpt-5.6-sol",
            "p"
        )
        .unwrap()["body"],
        json!({"additionalModelRequestFields": {"reasoning": {"summary": "auto", "effort": "high"}}})
    );
}

#[test]
fn maps_bedrock_mantle() {
    let settings = json!({"bearerToken": "token", "region": "us-west-2", "baseURL": "https://mantle.test/v1", "headers": {"x-test": "value"}, "reasoningEffort": "high", "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]});
    assert_eq!(
        map_full("@ai-sdk/amazon-bedrock/mantle", settings.clone(), "openai.gpt-oss-120b", "p"),
        Some(json!({
            "package": "@opencode/ai/providers/amazon-bedrock/mantle/chat",
            "settings": {"apiKey": "token", "baseURL": "https://mantle.test/v1", "region": "us-west-2", "reasoningEffort": "high", "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]},
            "headers": {"x-test": "value"},
        }))
    );
    for model_id in ["openai.gpt-oss-safeguard-20b", "openai.gpt-oss-safeguard-120b"] {
        assert_eq!(map_full("@ai-sdk/amazon-bedrock/mantle", settings.clone(), model_id, "p").unwrap()["package"], "@opencode/ai/providers/amazon-bedrock/mantle/chat");
    }
    assert_eq!(
        map_full("@ai-sdk/amazon-bedrock/mantle", json!({"region": "us-west-2", "baseURL": "https://bedrock-mantle.${AWS_REGION}.api.aws/openai/v1"}), "openai.gpt-5.5", "p")
            .unwrap()["settings"]["baseURL"],
        "https://bedrock-mantle.us-west-2.api.aws/openai/v1"
    );
    assert_eq!(
        map_full(
            "@ai-sdk/amazon-bedrock/mantle",
            json!({"credentials": {"accessKeyId": "key", "secretAccessKey": "secret", "sessionToken": "session", "region": "eu-west-1"}, "baseURL": "https://bedrock-mantle.${AWS_REGION}.api.aws/v1", "credentialProvider": "ignored", "store": false}),
            "openai.gpt-oss-120b",
            "p"
        ),
        Some(json!({
            "package": "@opencode/ai/providers/amazon-bedrock/mantle/chat",
            "settings": {
                "credentials": {"accessKeyId": "key", "secretAccessKey": "secret", "sessionToken": "session", "region": "eu-west-1"},
                "baseURL": "https://bedrock-mantle.eu-west-1.api.aws/v1",
                "store": false,
            },
        }))
    );
    assert_eq!(
        map_full("@ai-sdk/amazon-bedrock/mantle", json!({"bearerToken": "token", "endpoint": "https://mantle.private/v1", "region": "us-east-1"}), "openai.gpt-oss-120b", "p")
            .unwrap()["settings"]["baseURL"],
        "https://mantle.private/v1"
    );
}

#[test]
fn forwards_bedrock_profile_and_auth_mode() {
    assert_eq!(
        map_full("@ai-sdk/amazon-bedrock", json!({"profile": "work", "auth": "sigv4", "region": "eu-west-1"}), "anthropic.claude", "p"),
        Some(json!({"package": "@opencode/ai/providers/amazon-bedrock", "settings": {"profile": "work", "auth": "sigv4", "region": "eu-west-1"}}))
    );
    assert_eq!(
        map_full("@ai-sdk/amazon-bedrock", json!({"auth": "bogus"}), "anthropic.claude", "p"),
        Some(json!({"package": "@opencode/ai/providers/amazon-bedrock", "settings": {}}))
    );
}

#[test]
fn maps_openrouter_settings() {
    let out = map(
        "@openrouter/ai-sdk-provider",
        json!({
            "appName": "OpenCode",
            "appUrl": "https://opencode.ai",
            "headers": {"x-openrouter-title": "Configured", "x-provider-api-keys": "Configured BYOK"},
            "api_keys": {"anthropic": "provider-key"},
            "extraBody": {"transforms": ["middle-out"]},
            "models": ["anthropic/claude-sonnet-4.6"],
            "provider": {"only": ["anthropic"], "require_parameters": true},
            "reasoning": {"effort": "high"},
            "future_option": {"enabled": true},
        }),
    )
    .unwrap();
    assert_eq!(
        out,
        json!({
            "package": "@opencode/ai/providers/openrouter",
            "settings": {"models": ["anthropic/claude-sonnet-4.6"], "provider": {"only": ["anthropic"], "require_parameters": true}, "reasoning": {"effort": "high"}, "future_option": {"enabled": true}},
            "headers": {"x-openrouter-title": "Configured", "HTTP-Referer": "https://opencode.ai", "x-provider-api-keys": "Configured BYOK"},
            "body": {"transforms": ["middle-out"]},
        })
    );
    assert_eq!(
        serde_json::to_string(&out["headers"]).unwrap(),
        r#"{"x-openrouter-title":"Configured","HTTP-Referer":"https://opencode.ai","x-provider-api-keys":"Configured BYOK"}"#
    );
    let generated = map("@openrouter/ai-sdk-provider", json!({"appName": "A", "api_keys": {"anthropic": "k"}})).unwrap();
    assert_eq!(generated["headers"], json!({"X-OpenRouter-Title": "A", "X-Provider-API-Keys": "{\"anthropic\":\"k\"}"}));
}

#[test]
fn maps_google_settings() {
    let all = json!({"cachedContent": "cachedContents/example", "safetySettings": [{"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE"}], "serviceTier": "flex", "thinkingConfig": {"thinkingBudget": 0, "includeThoughts": false, "thinkingLevel": "high"}});
    assert_eq!(map("@ai-sdk/google", all.clone()), Some(json!({"package": "@opencode/ai/providers/google", "settings": all})));
    for thinking in [json!({"thinkingBudget": -1}), json!({"includeThoughts": true}), json!({"thinkingLevel": "medium"})] {
        assert_eq!(map("@ai-sdk/google", json!({"thinkingConfig": thinking})).unwrap()["settings"]["thinkingConfig"], thinking);
    }
}

#[test]
fn maps_vertex_routes() {
    assert_eq!(
        map(
            "@ai-sdk/google-vertex",
            json!({"accessToken": "vertex-token", "baseURL": "https://vertex.example/v1", "headers": {"x-test": "value"}, "labels": {"component": "opencode", "environment": "test"}, "location": "eu", "project": "vertex-project", "thinkingConfig": {"thinkingLevel": "high"}})
        ),
        Some(json!({
            "package": "@opencode/ai/providers/google-vertex",
            "settings": {"accessToken": "vertex-token", "baseURL": "https://vertex.example/v1", "location": "eu", "project": "vertex-project", "labels": {"component": "opencode", "environment": "test"}, "thinkingConfig": {"thinkingLevel": "high"}},
            "headers": {"x-test": "value"},
        }))
    );
    assert_eq!(
        map(
            "@ai-sdk/google-vertex/anthropic",
            json!({"accessToken": "vertex-token", "baseURL": "https://vertex.example/v1", "headers": {"x-test": "value"}, "location": "eu", "project": "vertex-project", "thinking": {"type": "adaptive", "display": "summarized"}, "effort": "high"})
        ),
        Some(json!({
            "package": "@opencode/ai/providers/google-vertex/messages",
            "settings": {"accessToken": "vertex-token", "baseURL": "https://vertex.example/v1", "location": "eu", "project": "vertex-project", "thinking": {"type": "adaptive", "display": "summarized"}, "effort": "high"},
            "headers": {"x-test": "value"},
        }))
    );
}

#[test]
fn maps_xai_and_leaves_unknown_packages() {
    let settings = json!({"apiKey": "secret", "baseURL": "https://xai.example/v1", "reasoningEffort": "custom", "store": true});
    assert_eq!(map("@ai-sdk/xai", settings.clone()), Some(json!({"package": "@opencode/ai/providers/xai", "settings": settings})));
    assert_eq!(map("@ai-sdk/gitlab", json!({"extraBody": {"kept": true}})), None);
}

#[test]
fn host_overrides() {
    let ctx = |p: &'static str| NativeContext { provider_id: p, ..Default::default() };
    assert_eq!(native("@ai-sdk/anthropic", &ctx("minimax-cn")).as_deref(), Some("@opencode/ai/providers/minimax/messages"));
    assert_eq!(native("@ai-sdk/openai", &ctx("kimi-for-coding")).as_deref(), Some("@opencode/ai/providers/moonshot/responses"));
    assert_eq!(native("@ai-sdk/openai-compatible", &ctx("zhipuai")).as_deref(), Some("@opencode/ai/providers/zai/chat"));
    assert_eq!(native("@ai-sdk/openai-compatible", &ctx("fireworks-ai")).as_deref(), Some("@opencode/ai/providers/fireworks"));
    assert_eq!(native("@ai-sdk/mistral", &ctx("google-vertex")).as_deref(), Some("@opencode/ai/providers/google-vertex/mistral"));
    assert_eq!(native("@ai-sdk/amazon-bedrock/mantle", &ctx("x")).as_deref(), Some("@opencode/ai/providers/amazon-bedrock/mantle"));
    assert_eq!(native("@ai-sdk/perplexity", &ctx("perplexity")), None);
}
