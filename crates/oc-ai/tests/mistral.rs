mod common;

use common::*;
use oc_ai::protocols::mistral_chat as mc;
use oc_ai::route::auth;
use oc_ai::testing::{decode, decode_ok};
use oc_ai::*;
use serde_json::json;

fn model(effort: &str) -> LanguageModel {
    let mut po = serde_json::Map::new();
    po.insert("reasoningEffort".into(), json!(effort));
    mc::route().auth(auth::none()).with_provider_options(po).model("mistral-small-latest").unwrap()
}

#[tokio::test]
async fn text_recording() {
    let rec = load("mistral/streams-text-with-usage.json");
    let mut req = LLMRequest::new(model("none")).prompt("Reply with exactly one word: hello");
    req.generation = Some(GenerationOptions { max_tokens: Some(40.0), temperature: Some(0.0), ..Default::default() });
    let response = turn(&req, &rec, 0).await;
    let t = response.text().to_lowercase(); assert!(t.contains("hello") || t.contains("hi"));
    assert_eq!(response.finish_reason.normalized, FinishReason::Stop);
    assert!(response.usage.unwrap().input_tokens.unwrap() > 0);
}

#[tokio::test]
async fn native_reasoning_replay_recording() {
    let rec = load("mistral/replays-native-reasoning.json");
    let mut req = LLMRequest::new(model("high")).prompt("Calculate 17 multiplied by 23. Think briefly, then reply with only the integer.");
    req.generation = Some(GenerationOptions { max_tokens: Some(512.0), temperature: Some(0.0), ..Default::default() });
    let first = turn(&req, &rec, 0).await;
    assert_eq!(first.text().trim(), "391");
    assert!(!first.reasoning().is_empty());
    let mut follow = req.clone();
    follow.messages.push(first.message.clone());
    follow.messages.push(Message::user_text("Reply with exactly: Done."));
    follow.generation = Some(GenerationOptions { max_tokens: Some(256.0), temperature: Some(0.0), ..Default::default() });
    let second = turn(&follow, &rec, 1).await;
    assert!(second.text().trim().ends_with("Done.") || second.text().trim().ends_with("Done"));
}

#[tokio::test]
async fn tool_loop_recording() {
    let rec = load("mistral/drives-a-tool-loop.json");
    let tool = ToolDefinition::new(
        "lookup_weather",
        "Look up the current weather for a city",
        json!({"type":"object","properties":{"city":{"type":"string","enum":["Paris"]}},"required":["city"],"additionalProperties":false}),
    );
    let mut req = LLMRequest::new(model("none")).system("Call lookup_weather exactly once with Paris.").system("After the tool result, describe the weather briefly.").prompt("What is the weather?").tool(tool);
    req.tool_choice = Some(ToolChoice::named("lookup_weather"));
    req.generation = Some(GenerationOptions { max_tokens: Some(160.0), temperature: Some(0.0), ..Default::default() });
    let first = turn(&req, &rec, 0).await;
    let calls = tool_calls(&first);
    assert_eq!(calls.len(), 1);
    let mut next = req.clone();
    next.tool_choice = Some(ToolChoice::from_str("none"));
    next.messages.push(first.message.clone());
    next.messages.push(tool_result(&calls[0].0, &calls[0].1, json!({"condition": "sunny", "temperature": "18C"})));
    let second = turn(&next, &rec, 1).await;
    assert_eq!(second.finish_reason.normalized, FinishReason::Stop);
}

#[tokio::test]
async fn options_and_errors() {
    let mut req = LLMRequest::new(model("high")).prompt("x").provider_option("promptMode", json!("reasoning"));
    req.generation = None;
    let err = Client::new().compile(&req).await.unwrap_err();
    assert_eq!(err.message, "Mistral Chat reasoningEffort and promptMode provider options are mutually exclusive");
    let req = LLMRequest::new(model("none"));
    let out = decode(&req, b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\ndata: [DONE]\n\n");
    assert_eq!(out.last().unwrap().as_ref().unwrap_err().message, "Mistral Chat stream ended without finish_reason");
    let events = decode_ok(&req, b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_long_id\",\"function\":{\"name\":\"t\",\"arguments\":{\"a\":1}}}]},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n");
    let LLMEvent::ToolCall { id, input, .. } = events.iter().find(|e| matches!(e, LLMEvent::ToolCall { .. })).unwrap() else { panic!() };
    assert_eq!(id, &oc_ai::protocols::shared::mistral_hash("call_long_id"));
    assert_eq!(input, &json!({"a": 1}));
    let LLMEvent::Finish { reason, .. } = events.last().unwrap() else { panic!() };
    assert_eq!(reason.normalized, FinishReason::ToolCalls);
}
