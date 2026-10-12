mod common;

use common::*;
use oc_ai::protocols::{cohere_chat as cc, google_interactions as gi};
use oc_ai::route::auth;
use oc_ai::*;
use serde_json::json;

fn cohere(id: &str) -> LanguageModel {
    cc::route().auth(auth::none()).model(id).unwrap()
}

#[tokio::test]
async fn cohere_text_recording() {
    let rec = load("cohere/streams-native-text-and-usage.json");
    let mut req = LLMRequest::new(cohere("command-a-03-2025")).prompt("Reply exactly: OK");
    req.generation = Some(GenerationOptions { max_tokens: Some(64.0), ..Default::default() });
    let response = turn(&req, &rec, 0).await;
    assert_eq!(response.text().trim(), "OK");
    assert_eq!(response.finish_reason, FinishReasonDetails::raw(FinishReason::Stop, Some("COMPLETE".into())));
    let usage = response.usage.unwrap();
    assert_eq!((usage.input_tokens, usage.cache_read_input_tokens, usage.non_cached_input_tokens), (Some(499), Some(464), Some(35)));
    assert!(usage.provider_metadata.unwrap()["cohere"]["billed_units"].is_object());
}

#[tokio::test]
async fn cohere_thinking_recording() {
    let rec = load("cohere/streams-native-thinking-with-a-budget.json");
    let mut req = LLMRequest::new(cohere("command-a-reasoning-08-2025"))
        .prompt("What is 17 times 23? Answer briefly.")
        .provider_option("thinking", json!({"type": "enabled", "tokenBudget": 128}));
    req.generation = Some(GenerationOptions { max_tokens: Some(2048.0), ..Default::default() });
    let response = turn(&req, &rec, 0).await;
    assert!(!response.reasoning().is_empty());
    assert!(response.text().contains("391"));
}

#[tokio::test]
async fn cohere_tool_loop_recording() {
    let rec = load("cohere/continues-a-native-tool-call.json");
    let mut req = tool_loop_request(cohere("command-a-plus-05-2026"), GOLDEN_SYSTEM, 2048.0, None);
    req.system = vec![SystemPart::text("Use the get_weather tool exactly once."), SystemPart::text("After the tool result, reply exactly: Paris is sunny.")];
    // This cassette predates upstream's current tool-message key order; compare as JSON values.
    let events = run_loop_with(req, &rec, false).await;
    assert_eq!(finish_reasons(&events), vec![FinishReason::ToolCalls, FinishReason::Stop]);
    assert!(events.iter().any(|e| matches!(e, LLMEvent::ToolInputDelta { .. })));
}

fn interactions(id: &str) -> LanguageModel {
    gi::route().auth(auth::none()).model(id).unwrap()
}

#[tokio::test]
async fn interactions_text_recording() {
    let rec = load("interactions/streams-text-and-reports-usage.json");
    let mut req = LLMRequest::new(interactions("gemini-3.8-flash")).prompt("Reply with exactly one word: hello").provider_option("thinkingLevel", json!("low"));
    req.generation = Some(GenerationOptions { max_tokens: Some(2048.0), ..Default::default() });
    let compiled = Client::new().compile(&req).await.unwrap();
    assert_eq!(compiled.prepared.url, rec.interactions[0].request.url);
    let response = turn(&req, &rec, 0).await;
    assert_eq!(response.text().trim(), "hello");
    let usage = response.usage.unwrap();
    assert_eq!((usage.input_tokens, usage.context_tokens), (Some(8), Some(39)));
    let reasoning_end = response.events.iter().find(|e| matches!(e, LLMEvent::ReasoningEnd { .. })).unwrap();
    let LLMEvent::ReasoningEnd { provider_metadata, .. } = reasoning_end else { panic!() };
    assert!(provider_metadata.as_ref().unwrap()["google"]["interactionSignature"].is_string());
}

#[tokio::test]
async fn interactions_recordings_decode() {
    for file in ["continues-tool-results-with-previous-interaction-id", "replays-native-tool-results-and-signatures-statelessly", "streams-reasoning-and-retains-thought-signatures"] {
        let rec = load(&format!("interactions/{file}.json"));
        let req = LLMRequest::new(interactions("gemini-3.8-flash"));
        for interaction in &rec.interactions {
            if interaction.response.status != 200 || interaction.response.body.len() < 10 {
                continue;
            }
            let events = oc_ai::testing::decode_ok(&req, &interaction.response.bytes());
            assert!(matches!(events.last(), Some(LLMEvent::Finish { .. })), "{file}");
        }
    }
}
