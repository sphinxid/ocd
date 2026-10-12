mod common;

use common::*;
use oc_ai::protocols::bedrock_converse as bc;
use oc_ai::route::auth;
use oc_ai::testing::{compile_body, decode, decode_ok};
use oc_ai::*;
use serde_json::json;

fn model(id: &str) -> LanguageModel {
    bc::route()
        .provider("amazon-bedrock")
        .metadata_key("bedrock")
        .base_url(Some(bc::bedrock_base_url("us-east-1")))
        .auth(auth::none())
        .model(id)
        .unwrap()
}

#[tokio::test]
async fn streams_text_recording() {
    let rec = load("bedrock/streams-text.json");
    let mut req = LLMRequest::new(model("us.amazon.nova-micro-v1:0")).system("Reply with the single word 'Hello'.").prompt("Say hello.");
    req.cache = Some(CachePolicy::none());
    req.generation = Some(GenerationOptions { max_tokens: Some(16.0), temperature: Some(0.0), ..Default::default() });
    let compiled = Client::new().compile(&req).await.unwrap();
    json_eq(&compiled.prepared.body, &rec.interactions[0].request.body);
    assert_eq!(compiled.prepared.url, rec.interactions[0].request.url);
    let events = decode_ok(&req, &rec.interactions[0].response.bytes());
    assert_eq!(text_of(&events), "Hello");
    let LLMEvent::Finish { usage: Some(u), reason, .. } = events.last().unwrap() else { panic!() };
    assert_eq!(reason.normalized, FinishReason::Stop);
    assert_eq!((u.input_tokens, u.output_tokens, u.total_tokens), (Some(12), Some(2), Some(14)));
}

#[tokio::test]
async fn tool_loop_recording() {
    let rec = load("bedrock/drives-a-tool-loop.json");
    let events = run_loop(tool_loop_request(model("us.amazon.nova-micro-v1:0"), LOOP_SYSTEM, 80.0, Some(0.0)), &rec).await;
    assert_eq!(finish_reasons(&events), vec![FinishReason::ToolCalls, FinishReason::Stop]);
}

#[tokio::test]
async fn named_tool_call_recording() {
    let rec = load("bedrock/streams-a-tool-call.json");
    let mut req = LLMRequest::new(model("us.amazon.nova-micro-v1:0")).system("Call tools exactly as requested.").prompt("Call get_weather with city exactly Paris.").tool(weather_tool());
    req.tool_choice = Some(ToolChoice::named(WEATHER));
    req.cache = Some(CachePolicy::none());
    req.generation = Some(GenerationOptions { max_tokens: Some(80.0), temperature: Some(0.0), ..Default::default() });
    let compiled = Client::new().compile(&req).await.unwrap();
    json_eq(&compiled.prepared.body, &rec.interactions[0].request.body);
    let events = decode_ok(&req, &rec.interactions[0].response.bytes());
    let LLMEvent::Finish { usage: Some(u), reason, .. } = events.last().unwrap() else { panic!() };
    assert_eq!(reason.normalized, FinishReason::ToolCalls);
    assert_eq!((u.input_tokens, u.output_tokens, u.total_tokens), (Some(419), Some(16), Some(435)));
}

#[tokio::test]
async fn parallel_results_merge_and_cache_points() {
    let call = |id: &str, city: &str| {
        ContentPart::ToolCall(ToolCallPart {
            id: id.into(),
            name: "lookup".into(),
            namespace: None,
            input: json!({"city": city}),
            provider_executed: None,
            cache: None,
            metadata: None,
            provider_metadata: None,
        })
    };
    let mut req = LLMRequest::new(model("us.anthropic.claude-sonnet-4-5-20250929-v1:0"));
    req.cache = Some(CachePolicy::none());
    req.messages = vec![
        Message::user_text("Compare the weather."),
        Message::assistant(vec![call("tool_paris", "Paris"), call("tool_london", "London")]),
        tool_result("tool_paris", "lookup", json!({"forecast": "sunny"})),
        tool_result("tool_london", "lookup", json!({"forecast": "rainy"})),
    ];
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["messages"][2]["content"].as_array().unwrap().len(), 2);
    // auto cache policy on Claude: cachePoint after system/last message
    let mut req = LLMRequest::new(model("us.anthropic.claude-sonnet-4-5-20250929-v1:0")).system("s").prompt("hi");
    req.cache = None;
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["system"], json!([{"text": "s"}, {"cachePoint": {"type": "default"}}]));
    assert_eq!(body["messages"][0]["content"], json!([{"text": "hi"}, {"cachePoint": {"type": "default"}}]));
}

#[tokio::test]
async fn thinking_and_block_binding() {
    let mut req = LLMRequest::new(model("us.anthropic.claude-sonnet-5-1")).prompt("hi").provider_option("thinking", json!({"type": "enabled", "budgetTokens": 4000}));
    req.cache = Some(CachePolicy::none());
    req.generation = Some(GenerationOptions { max_tokens: Some(4000.0), top_k: Some(5.0), ..Default::default() });
    let body = compile_body(&req).await.unwrap();
    assert_eq!(
        body["additionalModelRequestFields"],
        json!({"top_k": 5, "thinking": {"type": "enabled", "budget_tokens": 2000, "block_binding": {"prefix_mismatch_behavior": "drop_block"}}, "anthropic_beta": ["thinking-binding-controls-2026-08-01"]})
    );
}

#[tokio::test]
async fn hoists_images_for_non_claude_models_and_mistral_ids() {
    let mut req = LLMRequest::new(model("global.openai.gpt-6-sol"));
    req.cache = Some(CachePolicy::none());
    req.messages = vec![
        Message::assistant(vec![ContentPart::ToolCall(ToolCallPart {
            id: "tool_1".into(),
            name: "first".into(),
            namespace: None,
            input: json!({}),
            provider_executed: None,
            cache: None,
            metadata: None,
            provider_metadata: None,
        })]),
        Message::tool(ToolResultPart {
            id: "tool_1".into(),
            name: "first".into(),
            namespace: None,
            result: ToolResultValue::Content(vec![ToolContent::File { uri: "data:image/png;base64,AAAA".into(), mime: "image/png".into(), name: None }]),
            provider_executed: None,
            cache: None,
            metadata: None,
            provider_metadata: None,
        }),
    ];
    let body = compile_body(&req).await.unwrap();
    assert_eq!(
        body["messages"][1]["content"],
        json!([
            {"toolResult": {"toolUseId": "tool_1", "content": [{"text": "See attached image."}], "status": "success"}},
            {"image": {"format": "png", "source": {"bytes": "AAAA"}}}
        ])
    );
    let mut req2 = req.clone();
    req2.model = model("mistral.mistral-large-3");
    let body = compile_body(&req2).await.unwrap();
    assert_eq!(body["messages"][0]["content"][0]["toolUse"]["toolUseId"], oc_ai::protocols::shared::mistral_hash("tool_1"));
}

#[tokio::test]
async fn exceptions_and_incomplete_frames() {
    use oc_ai::route::framing::encode_aws_message;
    let req = LLMRequest::new(model("us.amazon.nova-micro-v1:0"));
    let msg = encode_aws_message(
        &[(":exception-type", "throttlingException"), (":content-type", "application/json"), (":message-type", "exception")],
        br#"{"message":"Too many requests, please wait before trying again."}"#,
    );
    let out = decode(&req, &msg);
    let err = out.last().unwrap().as_ref().unwrap_err();
    assert_eq!(err.tag(), "RateLimit");
    let out = decode(&req, &msg[..20]);
    let err = out.last().unwrap().as_ref().unwrap_err();
    assert!(err.message.starts_with("Incomplete Bedrock Converse event-stream frame"));
}
