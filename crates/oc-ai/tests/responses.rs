mod common;

use common::*;
use oc_ai::protocols::{open_responses as orp, openai_responses as oai, responses_variants as rv};
use oc_ai::route::auth;
use oc_ai::testing::{compile_body, decode, decode_ok, types};
use oc_ai::*;
use serde_json::json;

fn model(id: &str) -> LanguageModel {
    oai::route().base_url(Some("https://api.openai.test/v1/".into())).auth(auth::none()).model(id).unwrap()
}

#[tokio::test]
async fn prepares_body() {
    let mut req = LLMRequest::new(model("gpt-4.1-mini")).system("You are concise.").prompt("Say hello.");
    req.generation = Some(GenerationOptions { max_tokens: Some(20.0), temperature: Some(0.0), ..Default::default() });
    let compiled = Client::new().compile(&req).await.unwrap();
    assert_eq!(compiled.prepared.url, "https://api.openai.test/v1/responses");
    assert_eq!(
        compiled.prepared.body,
        r#"{"model":"gpt-4.1-mini","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Say hello."}]}],"instructions":"You are concise.","stream":true,"max_output_tokens":20,"temperature":0,"store":false,"include":["reasoning.encrypted_content"]}"#
    );
}

#[tokio::test]
async fn image_generation_tool_and_namespaces() {
    let mut image = ToolDefinition::new("image_generation", "", json!({}));
    image.native = Some(json!({"openai": {"type": "image_generation", "size": "1024x1024", "action": "generate", "quality": "high"}}).as_object().unwrap().clone());
    let mut req = LLMRequest::new(model("gpt-4.1-mini")).prompt("Show me a rooftop garden.").tool(image);
    req.tool_choice = Some(ToolChoice::from_str("image_generation"));
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["tools"], json!([{"type": "image_generation", "action": "generate", "quality": "high", "size": "1024x1024"}]));
    assert_eq!(body["tool_choice"], json!({"type": "image_generation"}));

    let mut req = LLMRequest::new(model("gpt-4.1-mini")).prompt("Find a customer and their orders.");
    req.tools.push(ToolEntry::Namespace(ToolNamespace {
        name: "crm".into(),
        description: Some("Customer management".into()),
        tools: vec![
            ToolEntry::Tool(ToolDefinition::new("lookup", "Look up a customer", json!({}))),
            ToolEntry::Tool(ToolDefinition::new("orders", "List customer orders", json!({}))),
        ],
    }));
    let body = compile_body(&req).await.unwrap();
    assert_eq!(
        body["tools"],
        json!([{"type": "namespace", "name": "crm", "description": "Customer management", "tools": [
            {"type": "function", "name": "lookup", "description": "Look up a customer", "parameters": {"type": "object"}, "strict": false},
            {"type": "function", "name": "orders", "description": "List customer orders", "parameters": {"type": "object"}, "strict": false}
        ]}])
    );
}

#[tokio::test]
async fn decodes_recorded_tool_loop() {
    let rec = load("responses/openai-responses-gpt-5-5-tool-loop.json");
    let req = LLMRequest::new(model("gpt-5.5"));
    let first = decode_ok(&req, &rec.interactions[0].response.bytes());
    let calls: Vec<_> = first.iter().filter(|e| matches!(e, LLMEvent::ToolCall { .. })).collect();
    assert_eq!(calls.len(), 1);
    let LLMEvent::ToolCall { name, input, provider_metadata, .. } = calls[0] else { panic!() };
    assert_eq!(name, "get_weather");
    assert_eq!(input, &json!({"city": "Paris"}));
    assert!(provider_metadata.as_ref().unwrap()["openai"]["itemId"].as_str().unwrap().starts_with("fc_"));
    assert_eq!(finish_reasons(&first), vec![FinishReason::ToolCalls]);
    let second = decode_ok(&req, &rec.interactions[1].response.bytes());
    assert!(text_of(&second).contains("Paris is sunny"));
    let LLMEvent::Finish { usage: Some(u), provider_metadata: Some(pm), .. } = second.last().unwrap() else { panic!() };
    assert!(u.input_tokens.unwrap() > 0);
    assert!(pm["openai"]["responseId"].as_str().unwrap().starts_with("resp_"));
}

#[tokio::test]
async fn reasoning_continuation_replays_encrypted_reasoning() {
    let rec = load("responses/openai-responses-gpt-5-5-reasoning-continuation.json");
    let req = LLMRequest::new(model("gpt-5.5"));
    let events = decode_ok(&req, &rec.interactions[0].response.bytes());
    let response = LLMResponse::from_events(events).unwrap();
    assert_eq!(response.text(), "Hello!");
    let mut next = LLMRequest::new(model("gpt-5.5")).prompt("Think briefly, then reply exactly with: Hello!");
    next.messages.push(response.message.clone());
    next.messages.push(Message::user_text("Now reply exactly with: Done."));
    let body = compile_body(&next).await.unwrap();
    let input = body["input"].as_array().unwrap();
    assert_eq!(input[1]["type"], "reasoning");
    assert!(input[1]["id"].as_str().unwrap().starts_with("rs_"));
    assert!(input[1]["encrypted_content"].as_str().unwrap().starts_with("gAAAA"));
    assert_eq!(input[2]["role"], "assistant");
    assert_eq!(input[2]["content"], json!([{"type": "output_text", "text": "Hello!"}]));
}

#[tokio::test]
async fn stream_state_machine() {
    let req = LLMRequest::new(model("gpt-5"));
    let sse = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[],\"encrypted_content\":\"enc\"}}\n\n",
        "data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":0,\"delta\":\"think\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"think\"}],\"encrypted_content\":\"enc\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"content\":[]}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":1,\"delta\":\"Hi\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hi\"}]}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":2,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"get_weather\",\"arguments\":\"\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"output_index\":2,\"delta\":\"{\\\"city\\\":\"}\n\n",
        "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\"output_index\":2,\"arguments\":\"{\\\"city\\\":\\\"Paris\\\"}\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":2,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"Paris\\\"}\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"input_tokens_details\":{\"cached_tokens\":2},\"output_tokens_details\":{\"reasoning_tokens\":3}}}}\n\n",
    );
    let events = decode_ok(&req, sse.as_bytes());
    assert_eq!(
        types(&events),
        vec![
            "step-start", "reasoning-start", "reasoning-delta", "reasoning-end", "text-start", "text-delta", "text-end",
            "tool-input-start", "tool-input-delta", "tool-input-delta", "tool-input-end", "tool-call", "step-finish", "finish"
        ]
    );
    let LLMEvent::ReasoningEnd { text, provider_metadata, .. } = &events[3] else { panic!() };
    assert_eq!(text.as_deref(), Some("think"));
    assert_eq!(provider_metadata.as_ref().unwrap()["openai"], json!({"itemId": "rs_1", "reasoningEncryptedContent": "enc"}));
    let LLMEvent::Finish { reason, usage: Some(u), .. } = events.last().unwrap() else { panic!() };
    assert_eq!(reason.normalized, FinishReason::ToolCalls);
    assert_eq!((u.non_cached_input_tokens, u.reasoning_tokens, u.total_tokens), (Some(8), Some(3), Some(15)));
}

#[tokio::test]
async fn hosted_tools_and_errors() {
    let req = LLMRequest::new(model("gpt-5"));
    let sse = concat!(
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"ws_1\",\"type\":\"web_search_call\",\"status\":\"completed\",\"action\":{\"query\":\"x\"}}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\"}}\n\n",
    );
    let events = decode_ok(&req, sse.as_bytes());
    let LLMEvent::ToolResult { result: ToolResultValue::Json(v), provider_executed, .. } = &events[2] else { panic!("{events:?}") };
    assert_eq!(provider_executed, &Some(true));
    // schema decoding orders declared keys first
    assert_eq!(serde_json::to_string(v).unwrap(), r#"{"type":"web_search_call","id":"ws_1","status":"completed","action":{"query":"x"}}"#);

    let out = decode(&req, b"data: {\"type\":\"error\",\"code\":\"rate_limit_exceeded\",\"message\":\"Slow down\"}\n\n");
    let err = out.last().unwrap().as_ref().unwrap_err();
    assert_eq!(err.tag(), "RateLimit");
    assert_eq!(err.message, "rate_limit_exceeded: Slow down");
    let out = decode(&req, b"data: {\"type\":\"error\"}\n\n");
    assert_eq!(out.last().unwrap().as_ref().unwrap_err().tag(), "ProviderInternal");
    let out = decode(&req, b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"Your input exceeds the context window\"}}}\n\n");
    assert!(out.last().unwrap().as_ref().unwrap_err().is_context_overflow());
}

#[tokio::test]
async fn effort_updates_freeze_top_level_effort() {
    let mut req = LLMRequest::new(model("gpt-6")).prompt("a");
    req = req.provider_option("reasoningEffort", json!("high"));
    req.messages.push(Message::effort(Some("high".into()), Some("low".into())));
    req.messages.push(Message::user_text("b"));
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["reasoning"]["effort"], "low");
    assert_eq!(body["input"][1], json!({"type": "configuration_update", "reasoning": {"effort": "high"}}));
    // unsupported models strip effort markers
    let mut req2 = req.clone();
    req2.model = model("gpt-5");
    let body = compile_body(&req2).await.unwrap();
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["input"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn variants() {
    let mut route = orp::compatible_route().provider("xai").base_url(Some("https://api.x.ai/v1".into())).auth(auth::none());
    route.protocol = rv::xai_protocol();
    let model = route.model("grok-4.5").unwrap();
    let req = LLMRequest::new(model).prompt("x").provider_option("contextManagement", json!([{"type": "compaction"}]));
    let err = Client::new().compile(&req).await.unwrap_err();
    assert_eq!(err.tag(), "UnsupportedOperation");

    let mut route = orp::compatible_route().provider("alibaba").base_url(Some("https://dashscope.test/v1".into())).auth(auth::none());
    route.protocol = rv::alibaba_protocol();
    let model = route.model("qwen3-max").unwrap();
    let mut req = LLMRequest::new(model).prompt("x").tool(weather_tool()).provider_option("enableThinking", json!(true));
    req.tool_choice = Some(ToolChoice::named("get_weather"));
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["tool_choice"], json!({"type": "allowed_tools", "mode": "required", "tools": [{"type": "function", "name": "get_weather"}]}));
    assert_eq!(body["enable_thinking"], true);
}
