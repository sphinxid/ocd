mod common;

use common::*;
use oc_ai::protocols::anthropic_messages as am;
use oc_ai::route::auth;
use oc_ai::testing::{compile_body, decode_ok, types};
use oc_ai::*;
use serde_json::json;

fn model(id: &str) -> LanguageModel {
    am::route().base_url(Some("https://api.anthropic.test/v1/".into())).auth(auth::none()).model(id).unwrap()
}

fn compatible(provider: &str, base: &str, id: &str) -> LanguageModel {
    am::route().id("anthropic-compatible-messages").provider(provider).base_url(Some(base.into())).auth(auth::none()).model(id).unwrap()
}

fn base_request(model: LanguageModel) -> LLMRequest {
    let mut req = LLMRequest::new(model);
    let mut part = SystemPart::text("You are concise.");
    part.cache = Some(CacheHint::ephemeral());
    req.system.push(part);
    req = req.prompt("Say hello.");
    req.cache = Some(CachePolicy::none());
    req.generation = Some(GenerationOptions { max_tokens: Some(20.0), temperature: Some(0.0), ..Default::default() });
    req
}

#[tokio::test]
async fn prepares_body() {
    let body = compile_body(&base_request(model("claude-sonnet-4-5"))).await.unwrap();
    assert_eq!(
        body,
        json!({
            "model": "claude-sonnet-4-5",
            "system": [{"type": "text", "text": "You are concise.", "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "Say hello."}]}],
            "stream": true, "max_tokens": 20, "temperature": 0
        })
    );
}

#[tokio::test]
async fn adaptive_thinking_with_effort_and_betas() {
    let mut req = base_request(model("claude-sonnet-4-5"));
    req = req.provider_option("thinking", json!({"type": "adaptive", "display": "summarized"})).provider_option("effort", json!("low"));
    let compiled = Client::new().compile(&req).await.unwrap();
    assert_eq!(compiled.prepared.json_body["thinking"], json!({"type": "adaptive", "display": "summarized"}));
    assert_eq!(compiled.prepared.json_body["output_config"], json!({"effort": "low"}));
    assert_eq!(compiled.prepared.headers.get("anthropic-beta").unwrap(), "interleaved-thinking-2025-05-14");
    assert_eq!(compiled.prepared.headers.get("anthropic-version").unwrap(), "2023-06-01");
    assert_eq!(compiled.prepared.url, "https://api.anthropic.test/v1/messages?beta=true");
}

#[tokio::test]
async fn claude_5_1_gets_block_binding_default() {
    let req = base_request(model("claude-opus-5-1"));
    let compiled = Client::new().compile(&req).await.unwrap();
    assert_eq!(
        compiled.prepared.json_body["thinking"],
        json!({"type": "adaptive", "block_binding": {"prefix_mismatch_behavior": "drop_block"}})
    );
    assert_eq!(
        compiled.prepared.headers.get("anthropic-beta").unwrap(),
        "interleaved-thinking-2025-05-14,thinking-binding-controls-2026-08-01"
    );
}

#[tokio::test]
async fn enabled_thinking_budget_is_fitted() {
    let mut req = base_request(model("claude-sonnet-4-5"));
    req.generation.as_mut().unwrap().max_tokens = Some(4000.0);
    req = req.provider_option("thinking", json!({"type": "enabled", "budgetTokens": 16000}));
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["thinking"], json!({"type": "enabled", "budget_tokens": 2000}));
}

#[tokio::test]
async fn filters_whitespace_and_empty_messages() {
    let mut req = LLMRequest::new(model("claude-sonnet-4-5"));
    req.cache = Some(CachePolicy::none());
    req.messages = vec![
        Message::user_text(" \n\t"),
        Message::user(vec![]),
        Message::user(vec![ContentPart::text(""), ContentPart::text("  Keep this spacing.  "), ContentPart::text(" \n\t")]),
        Message::assistant(vec![ContentPart::text(" \n\t")]),
        Message::assistant(vec![ContentPart::Reasoning(ReasoningPart { text: "".into(), encrypted: None, cache: None, metadata: None, provider_metadata: None })]),
        Message::assistant(vec![
            ContentPart::text(""),
            ContentPart::Reasoning(ReasoningPart {
                text: "".into(),
                encrypted: None,
                cache: None,
                metadata: None,
                provider_metadata: Some(json!({"anthropic": {"signature": "sig_1"}}).as_object().unwrap().clone()),
            }),
        ]),
    ];
    let body = compile_body(&req).await.unwrap();
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "  Keep this spacing.  "}]},
            {"role": "assistant", "content": [{"type": "thinking", "thinking": "", "signature": "sig_1"}]}
        ])
    );
}

#[tokio::test]
async fn auto_cache_policy_places_markers() {
    let mut req = tool_loop_request(model("claude-sonnet-4-5"), LOOP_SYSTEM, 80.0, Some(0.0));
    req.cache = None;
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["tools"][0]["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(body["system"][0]["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(body["messages"][0]["content"][0]["cache_control"], json!({"type": "ephemeral"}));
}

#[tokio::test]
async fn minimax_compatible_text_recording() {
    let rec = load("anthropic/minimax-m3-anthropic-compatible-text.json");
    let model = compatible("minimax", "https://api.minimax.io/anthropic/v1", "MiniMax-M3");
    let mut req = LLMRequest::new(model).system("You are concise.").prompt("Reply exactly with: Hello!");
    req.cache = Some(CachePolicy::none());
    req.generation = Some(GenerationOptions { max_tokens: Some(40.0), temperature: Some(0.0), ..Default::default() });
    let compiled = Client::new().compile(&req).await.unwrap();
    json_eq(&compiled.prepared.body, &rec.interactions[0].request.body);
    assert_eq!(compiled.prepared.url, rec.interactions[0].request.url);
    let events = decode_ok(&req, &rec.interactions[0].response.bytes());
    assert_eq!(
        types(&events),
        vec!["step-start", "text-start", "text-delta", "text-delta", "text-end", "step-finish", "finish"]
    );
    let LLMEvent::Finish { usage: Some(usage), reason, .. } = events.last().unwrap() else { panic!() };
    assert_eq!(reason.normalized, FinishReason::Stop);
    assert_eq!(usage.input_tokens, Some(167));
    assert_eq!(usage.cache_read_input_tokens, Some(114));
    assert_eq!(usage.output_tokens, Some(2));
    assert_eq!(text_of(&events), "Hello!");
}

#[tokio::test]
async fn minimax_compatible_golden_tool_loop() {
    let rec = load("anthropic/minimax-m3-anthropic-compatible-tool-loop.json");
    let model = compatible("minimax", "https://api.minimax.io/anthropic/v1", "MiniMax-M3");
    let events = run_loop(tool_loop_request(model, GOLDEN_SYSTEM, 80.0, Some(0.0)), &rec).await;
    assert_eq!(finish_reasons(&events), vec![FinishReason::ToolCalls, FinishReason::Stop]);
    assert!(text_of(&events).contains("Paris is sunny"));
}

#[tokio::test]
async fn decodes_opus_image_tool_result_stream() {
    let rec = load("anthropic/anthropic-opus-4-7-image-tool-result.json");
    let req = LLMRequest::new(model("claude-opus-4-7"));
    let events = decode_ok(&req, &rec.interactions[0].response.bytes());
    assert!(matches!(events.last(), Some(LLMEvent::Finish { .. })));
    assert!(!text_of(&events).is_empty());
}

#[tokio::test]
async fn stream_tool_use_and_signatures() {
    let req = LLMRequest::new(model("claude-sonnet-4-5"));
    let sse = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":5,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\",\"input\":{}}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\\\"Paris\\\"}\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":30,\"output_tokens_details\":{\"thinking_tokens\":12}}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let events = decode_ok(&req, sse.as_bytes());
    assert_eq!(
        types(&events),
        vec![
            "step-start", "reasoning-start", "reasoning-delta", "reasoning-end", "tool-input-start", "tool-input-delta",
            "tool-input-end", "tool-call", "step-finish", "finish"
        ]
    );
    let LLMEvent::ReasoningEnd { provider_metadata, .. } = &events[3] else { panic!() };
    assert_eq!(provider_metadata.as_ref().unwrap()["anthropic"]["signature"], "sig");
    let LLMEvent::ToolCall { input, .. } = &events[7] else { panic!() };
    assert_eq!(input, &json!({"city": "Paris"}));
    let LLMEvent::Finish { usage: Some(u), reason, .. } = events.last().unwrap() else { panic!() };
    assert_eq!(reason.normalized, FinishReason::ToolCalls);
    assert_eq!((u.input_tokens, u.output_tokens, u.reasoning_tokens), (Some(15), Some(30), Some(12)));
}

#[tokio::test]
async fn stream_error_event_is_classified() {
    let req = LLMRequest::new(model("claude-sonnet-4-5"));
    let sse = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
    let out = oc_ai::testing::decode(&req, sse.as_bytes());
    let err = out.last().unwrap().as_ref().unwrap_err();
    assert_eq!(err.tag(), "ProviderInternal");
    assert_eq!(err.message, "overloaded_error: Overloaded");
}

#[tokio::test]
async fn incomplete_stream_is_error() {
    let req = LLMRequest::new(model("claude-sonnet-4-5"));
    let sse = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n";
    let out = oc_ai::testing::decode(&req, sse.as_bytes());
    let err = out.last().unwrap().as_ref().unwrap_err();
    assert_eq!(err.message, "The provider response ended unexpectedly.");
    assert!(err.is_retryable());
}

#[tokio::test]
async fn derivative_bodies() {
    // zai: thinking passed verbatim
    let mut req = base_request(model("glm-5").map_route(|r| r.id("zai-coding-messages")));
    req.model = req.model.clone().map_route(|mut r| {
        r.protocol = am::zai_protocol();
        r
    });
    let req = req.provider_option("thinking", json!({"type": "enabled", "budgetTokens": 10}));
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["thinking"], json!({"type": "enabled"}));
    // vertex: no model, anthropic_version last
    let mut req = base_request(model("claude-sonnet-4-5"));
    req.model = req.model.clone().map_route(|mut r| {
        r.protocol = am::vertex_protocol();
        r
    });
    let text = oc_ai::testing::compile_text(&req).await.unwrap();
    assert!(!text.contains("\"model\""));
    assert!(text.ends_with("\"anthropic_version\":\"vertex-2023-10-16\"}"));
}
