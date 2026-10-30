mod common;

use common::*;
use oc_ai::protocols::openai_chat as oc;
use oc_ai::route::auth;
use oc_ai::testing::{compile_body, decode, decode_ok, types};
use oc_ai::*;
use serde_json::json;

fn compat_model(provider: &str, base: &str, id: &str, compat: Compatibility) -> LanguageModel {
    oc::compatible_route().provider(provider).base_url(Some(base.into())).auth(auth::none()).model(id).unwrap().with_compatibility(compat)
}

fn openai(id: &str) -> LanguageModel {
    oc::route().auth(auth::none()).model(id).unwrap()
}

#[tokio::test]
async fn together_text_recording() {
    let rec = load("chat/togetherai-streams-text.json");
    let model = compat_model("togetherai", "https://api.together.xyz/v1", "meta-llama/Llama-3.3-70B-Instruct-Turbo", Compatibility::default());
    let mut req = LLMRequest::new(model).system("You are concise.").prompt("Reply with exactly: Hello!");
    req.cache = Some(CachePolicy::none());
    req.generation = Some(GenerationOptions { max_tokens: Some(20.0), temperature: Some(0.0), ..Default::default() });
    let compiled = Client::new().compile(&req).await.unwrap();
    json_eq(&compiled.prepared.body, &rec.interactions[0].request.body);
    assert_eq!(compiled.prepared.url, rec.interactions[0].request.url);
    let events = decode_ok(&req, &rec.interactions[0].response.bytes());
    assert!(matches!(events.last(), Some(LLMEvent::Finish { .. })));
    assert!(text_of(&events).contains("Hello"));
}

#[tokio::test]
async fn deepinfra_golden_tool_loop() {
    let rec = load("chat/deepinfra-llama-3-3-70b-tool-loop.json");
    let compat = Compatibility {
        max_tokens_field: Some("max_tokens".into()),
        reasoning_field: Some("reasoning_content".into()),
        supports_store: Some(false),
        supports_prompt_cache_key: Some(true),
        ..Default::default()
    };
    let model = compat_model("deepinfra", "https://api.deepinfra.com/v1/openai", "meta-llama/Llama-3.3-70B-Instruct-Turbo", compat);
    let events = run_loop(tool_loop_request(model, GOLDEN_SYSTEM, 80.0, Some(0.0)), &rec).await;
    assert_eq!(finish_reasons(&events), vec![FinishReason::ToolCalls, FinishReason::Stop]);
}

#[tokio::test]
async fn cerebras_golden_tool_loop_with_reasoning() {
    let rec = load("chat/cerebras-gpt-oss-120b-tool-loop.json");
    let compat = Compatibility {
        max_tokens_field: Some("max_tokens".into()),
        reasoning_field: Some("reasoning".into()),
        supports_store: Some(false),
        supports_prompt_cache_key: Some(true),
        ..Default::default()
    };
    let model = compat_model("cerebras", "https://api.cerebras.ai/v1", "gpt-oss-120b", compat);
    let events = run_loop(tool_loop_request(model, GOLDEN_SYSTEM, 512.0, None), &rec).await;
    assert_eq!(finish_reasons(&events), vec![FinishReason::ToolCalls, FinishReason::Stop]);
}

#[tokio::test]
async fn openai_body_defaults() {
    let mut req = LLMRequest::new(openai("gpt-4.1")).system("sys").prompt("hi");
    req.prompt_cache_key = Some("k".repeat(80));
    req.model = req.model.clone().with_compatibility(Compatibility { supports_prompt_cache_key: Some(true), ..Default::default() });
    req = req.tool(weather_tool());
    req.tool_choice = Some(ToolChoice::named("get_weather"));
    req.generation = Some(GenerationOptions { max_tokens: Some(100.0), ..Default::default() });
    let req = req.provider_option("reasoningEffort", json!("low"));
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["max_completion_tokens"], 100);
    assert_eq!(body["store"], false);
    assert_eq!(body["prompt_cache_key"].as_str().unwrap().len(), 64);
    assert_eq!(body["reasoning_effort"], "low");
    assert_eq!(body["tool_choice"], json!({"type": "function", "function": {"name": "get_weather"}}));
    assert_eq!(body["tools"][0]["function"]["strict"], false);
    let keys: Vec<&str> = body.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["model", "messages", "tools", "tool_choice", "stream", "stream_options", "max_completion_tokens", "store", "prompt_cache_key", "reasoning_effort"]);
}

#[tokio::test]
async fn tool_history_without_tools_sends_empty_array_and_ids() {
    let mut req = LLMRequest::new(compat_model("mistral", "https://x.test/v1", "mistral-large", Compatibility::default()));
    req.messages = vec![
        Message::user_text("q"),
        Message::assistant(vec![ContentPart::ToolCall(ToolCallPart {
            id: "call_abc-def_123456".into(),
            name: "t".into(),
            namespace: None,
            input: json!({"a": 1}),
            provider_executed: None,
            cache: None,
            metadata: None,
            provider_metadata: None,
        })]),
        Message::tool(ToolResultPart {
            id: "call_abc-def_123456".into(),
            name: "t".into(),
            namespace: None,
            result: ToolResultValue::text("ok"),
            provider_executed: None,
            cache: None,
            metadata: None,
            provider_metadata: None,
        }),
        Message::user_text("next"),
    ];
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["tools"], json!([]));
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[1]["tool_calls"][0]["id"], "callabcde");
    assert_eq!(messages[1]["content"], serde_json::Value::Null);
    assert_eq!(messages[2], json!({"role": "tool", "tool_call_id": "callabcde", "content": "ok"}));
    assert_eq!(messages[3], json!({"role": "assistant", "content": "Done."}));
}

#[tokio::test]
async fn stream_reasoning_and_parallel_tools() {
    let req = LLMRequest::new(openai("gpt-x"));
    let sse = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"a\",\"arguments\":\"{\\\"x\\\":\"}},{\"index\":1,\"id\":\"c2\",\"function\":{\"name\":\"b\",\"arguments\":\"{}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"prompt_tokens_details\":{\"cached_tokens\":4},\"completion_tokens_details\":{\"reasoning_tokens\":2}}}\n\n",
        "data: [DONE]\n\n",
    );
    let events = decode_ok(&req, sse.as_bytes());
    assert_eq!(
        types(&events),
        vec![
            "step-start", "reasoning-start", "reasoning-delta", "text-start", "text-delta", "tool-input-start", "tool-input-delta",
            "tool-input-start", "tool-input-delta", "tool-input-delta", "reasoning-end", "tool-input-end", "tool-call", "tool-input-end",
            "tool-call", "text-end", "step-finish", "finish"
        ]
    );
    let LLMEvent::Finish { reason, usage: Some(u), .. } = events.last().unwrap() else { panic!() };
    assert_eq!(reason.normalized, FinishReason::ToolCalls);
    assert_eq!(reason.raw.as_deref(), Some("stop"));
    assert_eq!((u.input_tokens, u.non_cached_input_tokens, u.cache_read_input_tokens, u.reasoning_tokens, u.total_tokens), (Some(10), Some(6), Some(4), Some(2), Some(15)));
    let LLMEvent::ReasoningEnd { provider_metadata, .. } = &events[10] else { panic!() };
    assert_eq!(provider_metadata.as_ref().unwrap()["openai"]["reasoningField"], "reasoning_content");
}

#[tokio::test]
async fn stream_errors() {
    let req = LLMRequest::new(openai("gpt-x"));
    let out = decode(&req, b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\ndata: [DONE]\n\n");
    let err = out.last().unwrap().as_ref().unwrap_err();
    assert_eq!(err.message, "OpenAI Chat stream ended without finish_reason");
    let out = decode(&req, b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"error\"}]}\n\n");
    assert_eq!(out.last().unwrap().as_ref().unwrap_err().message, "Provider reported an error (finish_reason: error)");
    let out = decode(&req, b"data: {\"error\":{\"message\":\"This model's maximum context length is 128000 tokens\",\"code\":400}}\n\n");
    assert!(out.last().unwrap().as_ref().unwrap_err().is_context_overflow());
    let out = decode(
        &req,
        b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\"}]}}]}\n\n",
    );
    assert_eq!(out.last().unwrap().as_ref().unwrap_err().message, "OpenAI Chat received content after the finish reason");
}

/// 2.2: text, refusal and reasoning after the finish reason keep streaming (#58851); the first finish reason
/// sticks.
#[tokio::test]
async fn content_after_the_finish_reason_keeps_streaming() {
    let req = LLMRequest::new(openai("gpt-x"));
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\",\"reasoning_content\":\"late\"},\"finish_reason\":\"length\"}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0}]}}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n"
    );
    let events = decode_ok(&req, body.as_bytes());
    assert_eq!(text_of(&events), "Hello");
    assert!(events.iter().any(|e| matches!(e, LLMEvent::ReasoningDelta { text, .. } if text == "late")));
    let LLMEvent::Finish { reason, usage, .. } = events.last().unwrap() else { panic!() };
    assert_eq!((reason.normalized, reason.raw.as_deref()), (FinishReason::Stop, Some("stop")));
    assert_eq!(usage.as_ref().unwrap().input_tokens, Some(5));
}

/// "preserves metadata-only reasoning arriving after the finish reason"
#[tokio::test]
async fn metadata_only_reasoning_after_the_finish_reason() {
    let req = LLMRequest::new(openai("gpt-x"));
    let details = json!([{"type": "reasoning.encrypted", "data": "opaque", "format": "openai-responses-v1", "index": 0}]);
    let body = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        json!({"choices": [{"delta": {"reasoning_details": details}}]}),
    );
    let events = decode_ok(&req, body.as_bytes());
    let t = types(&events);
    assert_eq!(t.iter().filter(|t| **t == "reasoning-start").count(), 1, "{t:?}");
    assert_eq!(t.iter().filter(|t| **t == "reasoning-end").count(), 1, "{t:?}");
    let end = events.iter().find_map(|e| match e {
        LLMEvent::ReasoningEnd { provider_metadata, .. } => provider_metadata.clone(),
        _ => None,
    });
    assert_eq!(end.unwrap()["openai"]["reasoningDetails"], details);
}

/// "maps unknown finish reasons to unknown and keeps the raw value" (#58608)
#[tokio::test]
async fn unknown_finish_reasons_map_to_unknown() {
    let req = LLMRequest::new(openai("gpt-x"));
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"future_reason\"}]}\n\n";
    let events = decode_ok(&req, body.as_bytes());
    assert_eq!(text_of(&events), "Hello");
    let LLMEvent::Finish { reason, .. } = events.last().unwrap() else { panic!() };
    assert_eq!((reason.normalized, reason.raw.as_deref()), (FinishReason::Unknown, Some("future_reason")));
}

fn lookup_tool() -> ToolDefinition {
    ToolDefinition::new("lookup", "Lookup data", json!({"type": "object"}))
}

/// "maps shared OpenAI options to Chat fields"
#[tokio::test]
async fn maps_shared_openai_options_to_chat_fields() {
    let mut req = LLMRequest::new(openai("gpt-5-nano")).prompt("Hello").tool(lookup_tool());
    req.provider_options = Some(
        json!({
            "store": true,
            "serviceTier": "flex",
            "textVerbosity": "low",
            "parallelToolCalls": false,
            "metadata": {"source": "test"},
            "safetyIdentifier": "user-hash",
        })
        .as_object()
        .cloned()
        .unwrap(),
    );
    let body = compile_body(&req).await.unwrap();
    for (key, value) in [
        ("store", json!(true)),
        ("service_tier", json!("flex")),
        ("verbosity", json!("low")),
        ("parallel_tool_calls", json!(false)),
        ("metadata", json!({"source": "test"})),
        ("safety_identifier", json!("user-hash")),
    ] {
        assert_eq!(body[key], value, "{key}");
    }
    assert!(body.get("text").is_none());
    let keys: Vec<&str> = body.as_object().unwrap().keys().map(String::as_str).collect();
    let tail = &keys[keys.iter().position(|k| *k == "store").unwrap()..];
    assert_eq!(tail, ["store", "verbosity", "service_tier", "parallel_tool_calls", "metadata", "safety_identifier"]);
}

/// "maps disableParallelToolUse to parallel_tool_calls only when tools are present"
#[tokio::test]
async fn maps_disable_parallel_tool_use_only_with_tools() {
    let choice = ToolChoice { disable_parallel_tool_use: Some(true), ..ToolChoice::from_str("auto") };
    let mut with_tools = LLMRequest::new(openai("gpt-x")).prompt("Hello").tool(lookup_tool());
    with_tools.tool_choice = Some(choice.clone());
    let mut without_tools = LLMRequest::new(openai("gpt-x")).prompt("Hello");
    without_tools.tool_choice = Some(choice);
    let unset = LLMRequest::new(openai("gpt-x")).prompt("Hello").tool(lookup_tool());
    assert_eq!(compile_body(&with_tools).await.unwrap()["parallel_tool_calls"], json!(false));
    assert!(compile_body(&without_tools).await.unwrap().get("parallel_tool_calls").is_none());
    let unset = compile_body(&unset).await.unwrap();
    for key in ["parallel_tool_calls", "service_tier", "verbosity", "metadata", "safety_identifier"] {
        assert!(unset.get(key).is_none(), "{key}");
    }
}

/// One read can carry several frames: the events of the frames before a failing one are still delivered
/// (upstream's stream emits them before the failure), and the error body is the failing frame as sent.
#[tokio::test]
async fn events_before_a_failing_frame_in_the_same_read_survive() {
    let req = LLMRequest::new(openai("gpt-x"));
    let finish = "{\"id\": \"c\", \"choices\": [{\"index\": 0, \"delta\": {}, \"finish_reason\": \"error\"}]}";
    let body = format!("data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"partial\"}}}}]}}\n\ndata: {finish}\n\n");
    let out = decode(&req, body.as_bytes());
    let ok: Vec<LLMEvent> = out.iter().filter_map(|r| r.as_ref().ok().cloned()).collect();
    assert_eq!(types(&ok), vec!["step-start", "text-start", "text-delta"]);
    let err = out.last().unwrap().as_ref().unwrap_err();
    assert_eq!(err.message, "Provider reported an error (finish_reason: error)");
    assert!(err.is_retryable());
    assert_eq!(err.body.as_deref(), Some(finish));

    // The same through the live-stream entry point, one read holding both frames.
    let mut driver = StreamDriver::new(&req);
    let (events, failure) = driver.push_events(body.as_bytes());
    assert_eq!(types(&events), vec!["step-start", "text-start", "text-delta"]);
    assert_eq!(failure.unwrap().message, "Provider reported an error (finish_reason: error)");
}
