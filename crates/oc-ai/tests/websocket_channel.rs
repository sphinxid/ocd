//! The Responses WebSocket channel of the OpenAI, Azure and xAI routes (`channelTransport` options):
//! which endpoints offer it, the socket URL, the handshake headers and the `response.create` message.

use oc_ai::providers::language_model;
use oc_ai::{Client, LLMRequest, LanguageModel};
use serde_json::{Value, json};

fn model(package: &str, settings: Value) -> LanguageModel {
    language_model(package, "m1", settings.as_object().unwrap()).unwrap()
}

async fn prepare(model: LanguageModel) -> (bool, Option<oc_ai::protocols::open_responses_channel::PreparedChannel>) {
    let channel = model.route.channel.clone().expect("route has a channel");
    let compiled = Client::new().compile(&LLMRequest::new(model).prompt("hi")).await.unwrap();
    let mut headers = compiled.prepared.headers.clone();
    headers.shift_remove("content-type");
    let offers = channel.offers(&compiled.prepared.url);
    (offers, offers.then(|| channel.prepare(&compiled.prepared.url, &compiled.prepared.json_body, &headers).unwrap()))
}

#[tokio::test]
async fn openai_channel() {
    let (offers, prepared) = prepare(model("@opencode/ai/providers/openai", json!({"apiKey": "k"}))).await;
    assert!(offers);
    let prepared = prepared.unwrap();
    assert_eq!(prepared.url, "wss://api.openai.com/v1/responses");
    assert_eq!(prepared.rotate_after_ms, Some(55 * 60 * 1000));
    assert_eq!(prepared.headers.get("authorization").map(String::as_str), Some("Bearer k"));
    assert_eq!(prepared.headers.get("openai-beta").map(String::as_str), Some("responses_websockets=2026-02-06"));
    assert!(prepared.headers.get("content-type").is_none());
    let message: Value = serde_json::from_str(&prepared.message).unwrap();
    assert_eq!(message.as_object().unwrap().keys().next().map(String::as_str), Some("type"));
    assert_eq!(message["type"], "response.create");
    assert!(message.get("stream").is_none());
    assert_eq!(message["store"], false);

    // An explicit `openai-beta` header is kept.
    let mut request = LLMRequest::new(model("@opencode/ai/providers/openai", json!({"apiKey": "k", "baseURL": "http://127.0.0.1:9/v1"}))).prompt("hi");
    request.http = Some(oc_ai::types::HttpOptions::headers([("openai-beta".to_string(), "custom".to_string())].into_iter().collect()));
    let compiled = Client::new().compile(&request).await.unwrap();
    let channel = request.model.route.channel.clone().unwrap();
    let prepared = channel.prepare(&compiled.prepared.url, &compiled.prepared.json_body, &compiled.prepared.headers).unwrap();
    assert_eq!(prepared.url, "ws://127.0.0.1:9/v1/responses");
    assert_eq!(prepared.headers.get("openai-beta").map(String::as_str), Some("custom"));
}

#[tokio::test]
async fn azure_channel_only_on_the_v1_resource_endpoint() {
    let (offers, prepared) = prepare(model("@opencode/ai/providers/azure", json!({"apiKey": "k", "resourceName": "res"}))).await;
    assert!(offers);
    let prepared = prepared.unwrap();
    assert_eq!(prepared.url, "wss://res.openai.azure.com/openai/v1/responses");
    assert_eq!(prepared.headers.get("api-key").map(String::as_str), Some("k"));
    assert!(prepared.headers.get("openai-beta").is_none());

    let (offers, _) = prepare(model("@opencode/ai/providers/azure", json!({"apiKey": "k", "resourceName": "res", "apiVersion": "2025-04-01-preview"}))).await;
    assert!(!offers);
    let (offers, _) = prepare(model("@opencode/ai/providers/azure", json!({"apiKey": "k", "baseURL": "https://gateway.example.com/openai"}))).await;
    assert!(!offers);
    // The chat route has no channel.
    assert!(model("@opencode/ai/providers/azure/chat", json!({"apiKey": "k", "resourceName": "res"})).route.channel.is_none());
}

#[tokio::test]
async fn xai_channel_and_http_only_routes() {
    let (offers, prepared) = prepare(model("@opencode/ai/providers/xai", json!({"apiKey": "k"}))).await;
    assert!(offers);
    let prepared = prepared.unwrap();
    assert_eq!(prepared.url, "wss://api.x.ai/v1/responses");
    assert_eq!(prepared.rotate_after_ms, Some(24 * 60 * 1000));
    assert!(prepared.headers.get("openai-beta").is_none());
    for package in ["@opencode/ai/providers/meta", "@opencode/ai/providers/anthropic", "@opencode/ai/providers/openai-compatible"] {
        let settings = json!({"apiKey": "k", "baseURL": "https://example.com/v1"});
        if let Ok(m) = language_model(package, "m1", settings.as_object().unwrap()) {
            assert!(m.route.channel.is_none(), "{package}");
        }
    }
}
