//! Prompt-cache TTLs on the wire (2.2 `packages/ai/test`: `cache-policy.test.ts` "a ttl-only policy keeps
//! automatic placement", `anthropic-messages.test.ts` "sends the hint ttl on cache_control", `openai-chat.test.ts`
//! "sends the cache TTL as a request option when no markers carry it", `openai-responses.test.ts` "sends the cache
//! TTL as prompt_cache_options, or prompt_cache_retention for 24h", `bedrock-converse-cache.test.ts`).

use oc_ai::protocols::bedrock_converse as bc;
use oc_ai::route::auth;
use oc_ai::testing::compile_body;
use oc_ai::*;
use serde_json::{json, Value};

fn package(package: &str, id: &str, settings: Value) -> LanguageModel {
    let settings = settings.as_object().cloned().unwrap_or_default();
    oc_ai::providers::language_model(&format!("@opencode/ai/providers/{package}"), id, &settings).unwrap()
}

fn ttl(value: &str) -> Option<CachePolicy> {
    Some(CachePolicy::Object(CachePolicyObject { ttl: Some(value.into()), ..Default::default() }))
}

fn anthropic() -> LanguageModel {
    package("anthropic", "claude-sonnet-4-6", json!({"apiKey": "k"}))
}

#[test]
fn cache_ttl_pattern() {
    for ok in ["5m", "30m", "1h", "24h", "500ms", "10s", "7d"] {
        assert!(is_cache_ttl(ok), "{ok}");
    }
    for bad in ["", "1 hour", "h", "1H", "1.5h", "1hm", "-1h", "1w"] {
        assert!(!is_cache_ttl(bad), "{bad}");
    }
}

#[tokio::test]
async fn a_ttl_only_policy_keeps_automatic_placement() {
    let mut request = LLMRequest::new(anthropic()).system("Sys").prompt("hi");
    request.cache = ttl("1h");
    let body = compile_body(&request).await.unwrap();
    assert_eq!(body["system"], json!([{"type": "text", "text": "Sys", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]));
    assert_eq!(body["messages"][0]["content"][0]["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
}

#[tokio::test]
async fn ttl_in_the_policy_flows_through_to_wire_markers_verbatim() {
    let mut request = LLMRequest::new(anthropic()).system("Sys").prompt("hi");
    request.cache = Some(CachePolicy::Object(CachePolicyObject { system: Some(true), ttl: Some("30m".into()), ..Default::default() }));
    let body = compile_body(&request).await.unwrap();
    assert_eq!(body["system"][0]["cache_control"], json!({"type": "ephemeral", "ttl": "30m"}));
    assert!(body["messages"][0]["content"][0].get("cache_control").is_none());
}

#[tokio::test]
async fn sends_the_hint_ttl_on_cache_control() {
    let mut request = LLMRequest::new(anthropic()).prompt("hi");
    let mut part = SystemPart::text("system");
    part.cache = Some(CacheHint::with_ttl(Some("1h".into())));
    request.system = vec![part];
    request.cache = Some(CachePolicy::none());
    let body = compile_body(&request).await.unwrap();
    assert_eq!(body["system"][0]["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
}

#[tokio::test]
async fn anthropic_accepts_any_pattern_ttl_in_cache_control_options() {
    let mut request = LLMRequest::new(anthropic()).prompt("hi");
    request.cache = Some(CachePolicy::none());
    request.provider_options = Some(serde_json::from_value(json!({"cacheControl": {"type": "ephemeral", "ttl": "30m"}})).unwrap());
    let body = compile_body(&request).await.unwrap();
    assert_eq!(body["cache_control"], json!({"type": "ephemeral", "ttl": "30m"}));
}

#[tokio::test]
async fn openai_chat_sends_the_cache_ttl_as_a_request_option_when_no_markers_carry_it() {
    let model = package("openai/chat", "gpt-5.6", json!({"apiKey": "k"}));
    let mut request = LLMRequest::new(model).prompt("hi");
    request.cache = ttl("1h");
    let body = compile_body(&request).await.unwrap();
    assert_eq!(body["prompt_cache_options"], json!({"ttl": "1h"}));
    request.cache = ttl("24h");
    let body = compile_body(&request).await.unwrap();
    assert_eq!(body["prompt_cache_retention"], json!("24h"));
    assert!(body.get("prompt_cache_options").is_none());
}

#[tokio::test]
async fn openai_chat_markers_carry_the_ttl_instead_of_a_request_option() {
    // Alibaba places inline markers, so the lifetime travels on them.
    let model = package("alibaba/chat", "qwen3-max", json!({"apiKey": "k", "baseURL": "https://dashscope.test/v1"}));
    let mut request = LLMRequest::new(model).system("Sys").prompt("hi");
    request.cache = ttl("1h");
    let body = compile_body(&request).await.unwrap();
    assert!(body.get("prompt_cache_options").is_none());
    assert!(body.get("prompt_cache_retention").is_none());
    assert!(body.to_string().contains(r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#), "{body}");
}

#[tokio::test]
async fn openai_responses_sends_prompt_cache_options_or_prompt_cache_retention_for_24h() {
    let request = |model: &str, cache: Option<CachePolicy>| {
        let mut r = LLMRequest::new(package("openai", model, json!({"apiKey": "k"}))).prompt("hi");
        r.cache = cache;
        r
    };
    let options = compile_body(&request("gpt-5.6", ttl("30m"))).await.unwrap();
    let retention = compile_body(&request("gpt-5.5", ttl("24h"))).await.unwrap();
    let unset = compile_body(&request("gpt-5.5", None)).await.unwrap();
    assert_eq!(options["prompt_cache_options"], json!({"ttl": "30m"}));
    assert!(options.get("prompt_cache_retention").is_none());
    assert_eq!(retention["prompt_cache_retention"], json!("24h"));
    assert!(retention.get("prompt_cache_options").is_none());
    assert!(unset.get("prompt_cache_options").is_none());
    assert!(unset.get("prompt_cache_retention").is_none());
}

fn bedrock(id: &str) -> LanguageModel {
    bc::route()
        .provider("amazon-bedrock")
        .metadata_key("bedrock")
        .base_url(Some(bc::bedrock_base_url("us-east-1")))
        .auth(auth::none())
        .model(id)
        .unwrap()
}

#[tokio::test]
async fn bedrock_drops_a_one_hour_ttl_where_claude_caches_five_minutes_only() {
    // [model, ttl requested, ttl on the wire]
    for (id, requested, wire) in [
        ("anthropic.claude-sonnet-4-6", "1h", Some("1h")),
        ("anthropic.claude-sonnet-4-6", "5m", Some("5m")),
        ("anthropic.claude-3-7-sonnet-20250219-v1:0", "1h", None),
        ("anthropic.claude-3-7-sonnet-20250219-v1:0", "30m", Some("30m")),
    ] {
        let mut request = LLMRequest::new(bedrock(id)).system("Sys").prompt("hi");
        request.cache = ttl(requested);
        let body = compile_body(&request).await.unwrap();
        let marker = match wire {
            Some(ttl) => json!({"cachePoint": {"type": "default", "ttl": ttl}}),
            None => json!({"cachePoint": {"type": "default"}}),
        };
        assert_eq!(body["system"], json!([{"text": "Sys"}, marker]), "{id} {requested}");
    }
}
