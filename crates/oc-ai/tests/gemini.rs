mod common;

use common::*;
use oc_ai::protocols::gemini as g;
use oc_ai::route::auth;
use oc_ai::testing::{compile_body, decode, decode_ok, types};
use oc_ai::*;
use serde_json::json;

fn vertex(id: &str) -> LanguageModel {
    g::vertex_route().base_url(Some("https://aiplatform.googleapis.com/v1/publishers/google".into())).auth(auth::none()).model(id).unwrap()
}

fn lookup() -> ToolDefinition {
    ToolDefinition::new(
        "lookup_weather",
        "Look up the current weather for a city",
        json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}),
    )
}

#[tokio::test]
async fn vertex_text_and_tool_recordings() {
    let rec = load("gemini/streams-text.json");
    let req = LLMRequest::new(vertex("gemini-3.5-flash")).prompt("Reply with exactly one word: hello");
    let compiled = Client::new().compile(&req).await.unwrap();
    json_eq(&compiled.prepared.body, &rec.interactions[0].request.body);
    assert_eq!(compiled.prepared.url, rec.interactions[0].request.url);
    let events = decode_ok(&req, &rec.interactions[0].response.bytes());
    assert!(text_of(&events).to_lowercase().contains("hello"));

    let rec = load("gemini/calls-a-tool.json");
    let req = LLMRequest::new(vertex("gemini-3.5-flash")).prompt("What is the weather in Paris? Use the lookup_weather tool.").tool(lookup());
    let compiled = Client::new().compile(&req).await.unwrap();
    json_eq(&compiled.prepared.body, &rec.interactions[0].request.body);
    let events = decode_ok(&req, &rec.interactions[0].response.bytes());
    let call = events.iter().find(|e| matches!(e, LLMEvent::ToolCall { .. })).unwrap();
    let LLMEvent::ToolCall { id, input, provider_metadata, .. } = call else { panic!() };
    assert_eq!(id, "call_554608");
    assert_eq!(input, &json!({"city": "Paris"}));
    assert!(provider_metadata.as_ref().unwrap()["vertex"]["thoughtSignature"].is_string());
    let LLMEvent::Finish { reason, usage: Some(u), .. } = events.last().unwrap() else { panic!() };
    assert_eq!(reason.normalized, FinishReason::ToolCalls);
    assert_eq!((u.output_tokens, u.reasoning_tokens, u.total_tokens), (Some(117), Some(101), Some(156)));
}

#[tokio::test]
async fn vertex_continuation_recording() {
    let rec = load("gemini/continues-after-a-tool-result.json");
    let mut req = LLMRequest::new(vertex("gemini-3.5-flash")).prompt("What is the weather in Paris?").tool(lookup());
    req.messages.push(Message::assistant(vec![ContentPart::ToolCall(ToolCallPart {
        id: "call_paris_1".into(),
        name: "lookup_weather".into(),
        namespace: None,
        input: json!({"city": "Paris"}),
        provider_executed: None,
        cache: None,
        metadata: None,
        provider_metadata: None,
    })]));
    req.messages.push(Message::tool(ToolResultPart {
        id: "call_paris_1".into(),
        name: "lookup_weather".into(),
        namespace: None,
        result: ToolResultValue::text("18C, light rain"),
        provider_executed: None,
        cache: None,
        metadata: None,
        provider_metadata: None,
    }));
    let text = oc_ai::testing::compile_text(&req).await.unwrap();
    json_eq(&text, &rec.interactions[0].request.body);
    let events = decode_ok(&req, &rec.interactions[0].response.bytes());
    assert!(text_of(&events).contains("18"));
    let LLMEvent::TextEnd { provider_metadata, .. } = events.iter().find(|e| matches!(e, LLMEvent::TextEnd { .. })).unwrap() else { panic!() };
    assert!(provider_metadata.as_ref().unwrap()["vertex"]["thoughtSignature"].as_str().unwrap().starts_with("AY89"));
}

#[tokio::test]
async fn body_options_and_thinking() {
    let model = g::route().auth(auth::none()).model("gemini-2.5-pro").unwrap();
    let mut req = LLMRequest::new(model).system("a").system("b").prompt("hi").tool(lookup());
    req.tool_choice = Some(ToolChoice::named("lookup_weather"));
    req.generation = Some(GenerationOptions { max_tokens: Some(2000.0), ..Default::default() });
    let req = req.provider_option("thinkingConfig", json!({"thinkingBudget": 5000})).provider_option("cachedContent", json!("cachedContents/x"));
    let compiled = Client::new().compile(&req).await.unwrap();
    assert_eq!(compiled.prepared.url, "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse");
    let body = compiled.prepared.json_body;
    assert_eq!(body["generationConfig"], json!({"maxOutputTokens": 2000, "thinkingConfig": {"thinkingBudget": 1000, "includeThoughts": true}}));
    assert_eq!(body["toolConfig"], json!({"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["lookup_weather"]}}));
    assert_eq!(body["systemInstruction"], json!({"parts": [{"text": "a"}, {"text": "b"}]}));
    let keys: Vec<&str> = body.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["cachedContent", "contents", "systemInstruction", "tools", "toolConfig", "generationConfig"]);
}

#[tokio::test]
async fn gemini_3_unsigned_call_fallback_and_gemini_2_omits_ids() {
    let call = |id: &str| {
        ContentPart::ToolCall(ToolCallPart {
            id: id.into(),
            name: "t".into(),
            namespace: None,
            input: json!({}),
            provider_executed: None,
            cache: None,
            metadata: None,
            provider_metadata: None,
        })
    };
    let model = g::route().auth(auth::none()).model("gemini-3-pro").unwrap();
    let mut req = LLMRequest::new(model).prompt("x");
    req.messages.push(Message::assistant(vec![call("a"), call("b")]));
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["contents"][1]["parts"][0]["thoughtSignature"], "skip_thought_signature_validator");
    assert_eq!(body["contents"][1]["parts"][1]["thoughtSignature"], "skip_thought_signature_validator");
    let model = g::route().auth(auth::none()).model("gemini-2.0-flash").unwrap();
    let mut req = LLMRequest::new(model).prompt("x");
    req.messages.push(Message::assistant(vec![call("a")]));
    let body = compile_body(&req).await.unwrap();
    assert!(body["contents"][1]["parts"][0]["functionCall"].get("id").is_none());
    assert!(body["contents"][1]["parts"][0].get("thoughtSignature").is_none());
}

#[tokio::test]
async fn stream_errors_and_blocks() {
    let model = g::route().auth(auth::none()).model("gemini-2.5-flash").unwrap();
    let req = LLMRequest::new(model);
    let out = decode(&req, b"data: {\"candidates\":[{\"finishReason\":\"MALFORMED_FUNCTION_CALL\"}]}\n\n");
    assert_eq!(out.last().unwrap().as_ref().unwrap_err().message, "Gemini stopped with MALFORMED_FUNCTION_CALL");
    let events = decode_ok(&req, b"data: {\"promptFeedback\":{\"blockReason\":\"SAFETY\"},\"usageMetadata\":{\"promptTokenCount\":3}}\n\n");
    let LLMEvent::Finish { reason, .. } = events.last().unwrap() else { panic!() };
    assert_eq!(reason.normalized, FinishReason::ContentFilter);
    assert_eq!(reason.raw.as_deref(), Some("SAFETY"));
    let out = decode(&req, b"data: {\"error\":{\"code\":429,\"message\":\"Resource exhausted\",\"status\":\"RESOURCE_EXHAUSTED\"}}\n\n");
    assert_eq!(out.last().unwrap().as_ref().unwrap_err().tag(), "RateLimit");
    let events = decode_ok(
        &req,
        b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"plan\",\"thought\":true},{\"text\":\"answer\"}]},\"finishReason\":\"STOP\"}]}\n\n",
    );
    assert_eq!(types(&events), vec!["step-start", "reasoning-start", "reasoning-delta", "reasoning-end", "text-start", "text-delta", "text-end", "step-finish", "finish"]);
}

#[test]
fn vertex_helpers() {
    assert_eq!(g::vertex_host("global"), "aiplatform.googleapis.com");
    assert_eq!(g::vertex_host("eu"), "aiplatform.eu.rep.googleapis.com");
    assert_eq!(g::vertex_host("us-central1"), "us-central1-aiplatform.googleapis.com");
    assert_eq!(
        g::vertex_gemini_base_url(None, false, "us-central1", Some("p".into()), "gemini-3").unwrap(),
        "https://us-central1-aiplatform.googleapis.com/v1beta1/projects/p/locations/us-central1/publishers/google"
    );
    assert_eq!(
        g::vertex_gemini_base_url(None, false, "global", None, "x").unwrap_err().message,
        "Google Vertex requires a project when baseURL is not configured"
    );
}
