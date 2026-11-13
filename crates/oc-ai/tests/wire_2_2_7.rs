//! Ports of the protocol tests opencode 2.2.6-2.2.7 changed (packages/ai/test): GitHub Copilot on the native
//! OpenAI-compatible routes (`reasoning_opaque`, top-level reasoning tokens, billing in AI credits, named content
//! filters, Responses item ids), Azure Messages for Foundry Claude, Gemini tool-use prompts and finish codes, Google
//! Interactions failures, Cohere streams that end early, predicate `criteria` and the transport's system codes.
//!
//! With `OCD_SCRATCH` (holding the 2.2.7 checkout `src/opencode-2.2.7`) and `OCD_BUN_PATH`, the corpus of recorded
//! responses also decodes with upstream 2.2.7's own routes, and a set of requests compiles with them, for comparison.

#[path = "common/http_mock.rs"]
mod http_mock;

use oc_ai::decision::{self, DecisionPart, DecisionRequest};
use oc_ai::protocols::{anthropic_messages as am, cohere_chat as cc, gemini as g, openai_chat as oc};
use oc_ai::route::auth;
use oc_ai::testing::{compile_body, decode};
use oc_ai::*;
use serde_json::{json, Map, Value};

fn sse(values: &[Value]) -> Vec<u8> {
    values.iter().map(|v| format!("data: {v}\n\n")).collect::<String>().into_bytes()
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap()
}

fn response(req: &LLMRequest, body: &[u8]) -> LLMResponse {
    let events: Vec<LLMEvent> = decode(req, body).into_iter().map(|e| e.unwrap()).collect();
    LLMResponse::from_events(events).unwrap()
}

fn failure(req: &LLMRequest, body: &[u8]) -> AIError {
    decode(req, body).into_iter().find_map(Result::err).expect("an error")
}

fn copilot_chat() -> LLMRequest {
    let model = oc::compatible_route().provider("github-copilot").base_url(Some("https://api.githubcopilot.test".into())).auth(auth::none()).model("gemini-3-pro").unwrap();
    LLMRequest::new(model).prompt("Read the project.")
}

fn chunk(delta: Value, finish: Option<&str>) -> Value {
    json!({ "id": "chatcmpl_fixture", "choices": [{ "delta": delta, "finish_reason": finish }] })
}

fn usage_chunk(usage: Value) -> Value {
    json!({ "id": "chatcmpl_fixture", "choices": [], "usage": usage })
}

/// openai-chat.test.ts "reads top-level reasoning tokens whether or not completion tokens include them".
#[test]
fn chat_reads_top_level_reasoning_tokens() {
    let req = copilot_chat();
    let usage = |u: Value| response(&req, &sse(&[chunk(json!({"content": "Hi"}), Some("stop")), usage_chunk(u)])).usage.unwrap();
    // Copilot's Gemini: total = prompt + completion + reasoning.
    let gemini = usage(json!({"prompt_tokens": 7, "completion_tokens": 2, "total_tokens": 62, "reasoning_tokens": 53}));
    assert_eq!((gemini.output_tokens, gemini.reasoning_tokens, gemini.total_tokens), (Some(55), Some(53), Some(62)));
    // Copilot's Kimi: total = prompt + completion, which already includes reasoning.
    let kimi = usage(json!({"prompt_tokens": 96, "completion_tokens": 50, "total_tokens": 146, "reasoning_tokens": 35}));
    assert_eq!((kimi.output_tokens, kimi.reasoning_tokens, kimi.total_tokens), (Some(50), Some(35), Some(146)));
}

/// openai-chat.test.ts "converts the GitHub Copilot billing beside usage to USD".
#[test]
fn chat_converts_copilot_billing() {
    let req = copilot_chat();
    let mut billed = usage_chunk(json!({"prompt_tokens": 5, "completion_tokens": 2}));
    billed["copilot_usage"] = json!({"total_nano_aiu": 4_473_525_000u64, "token_details": []});
    let usage = response(&req, &sse(&[chunk(json!({"content": "Hello"}), Some("stop")), billed])).usage.unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens, usage.cost), (Some(5), Some(2), Some(0.04473525)));
    // A reported `usage.cost` wins; a negative charge reads as absent.
    let mut both = usage_chunk(json!({"prompt_tokens": 5, "completion_tokens": 2, "cost": 0.5}));
    both["copilot_usage"] = json!({"total_nano_aiu": 4_473_525_000u64});
    assert_eq!(response(&req, &sse(&[chunk(json!({"content": "Hello"}), Some("stop")), both])).usage.unwrap().cost, Some(0.5));
    let mut negative = usage_chunk(json!({"prompt_tokens": 5}));
    negative["copilot_usage"] = json!({"total_nano_aiu": -1});
    assert_eq!(response(&req, &sse(&[chunk(json!({"content": "Hello"}), Some("stop")), negative])).usage.unwrap().cost, None);
}

/// openai-compatible-chat.test.ts "names the content filter that blocked a response".
#[test]
fn chat_names_the_content_filter() {
    let req = copilot_chat();
    let finish = |choice: Value| {
        let mut frame = json!({ "id": "chatcmpl_fixture", "choices": [{ "finish_reason": "content_filter" }] });
        for (k, v) in object(choice) {
            frame["choices"][0][k] = v;
        }
        response(&req, &sse(&[frame])).finish_reason
    };
    let copilot = finish(json!({"delta": {"copilot_annotations": {"TextCopyright": [{"id": 1}]}}}));
    assert_eq!((copilot.normalized, copilot.category.as_deref()), (FinishReason::ContentFilter, Some("TextCopyright")));
    let azure = finish(json!({"delta": {}, "content_filter_results": {"hate": {"filtered": false}, "violence": {"filtered": true}}}));
    assert_eq!(azure.category.as_deref(), Some("violence"));
}

/// openai-compatible-chat.test.ts "replays the latest reasoning_opaque with the concatenated reasoning text".
#[tokio::test]
async fn chat_replays_the_latest_reasoning_opaque() {
    let req = copilot_chat();
    let body = sse(&[
        chunk(json!({"reasoning_text": "Read the readme first."}), None),
        chunk(json!({"reasoning_opaque": "opaque-first", "tool_calls": [{"index": 0, "id": "call_first", "function": {"name": "read", "arguments": "{}"}}]}), None),
        chunk(json!({"reasoning_text": " Then the manifest."}), None),
        chunk(json!({"reasoning_opaque": "opaque-second", "tool_calls": [{"index": 1, "id": "call_second", "function": {"name": "read", "arguments": "{}"}}]}), None),
        chunk(json!({}), Some("tool_calls")),
    ]);
    let out = response(&req, &body);
    assert_eq!(out.reasoning(), "Read the readme first. Then the manifest.");
    let ends: Vec<Value> = out
        .events
        .iter()
        .filter_map(|e| match e {
            LLMEvent::ReasoningEnd { provider_metadata, .. } => Some(serde_json::to_value(provider_metadata).unwrap()),
            _ => None,
        })
        .collect();
    assert_eq!(ends, vec![json!({"github-copilot": {"reasoningField": "reasoning_text", "reasoningOpaque": "opaque-second"}})]);
    let mut replay = LLMRequest::new(copilot_chat().model);
    replay.messages = vec![out.message];
    let replay = compile_body(&replay).await.unwrap();
    let assistant = &replay["messages"][0];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"], Value::Null);
    assert_eq!(assistant["reasoning_text"], "Read the readme first. Then the manifest.");
    assert_eq!(assistant["reasoning_opaque"], "opaque-second");
}

/// openai-compatible-chat.test.ts "keeps a reasoning_opaque that arrives without reasoning text".
#[tokio::test]
async fn chat_keeps_an_opaque_without_reasoning_text() {
    let req = copilot_chat();
    let body = sse(&[
        chunk(json!({"content": "Checking.", "reasoning_opaque": "opaque-only"}), None),
        chunk(json!({"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "read", "arguments": "{}"}}]}), None),
        chunk(json!({}), Some("tool_calls")),
    ]);
    let out = response(&req, &body);
    assert_eq!(out.reasoning(), "");
    let mut replay = LLMRequest::new(copilot_chat().model);
    replay.messages = vec![out.message];
    let replay = compile_body(&replay).await.unwrap();
    assert_eq!(
        replay["messages"][0],
        json!({"role": "assistant", "content": "Checking.", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{}"}}], "reasoning_opaque": "opaque-only"})
    );
}

fn copilot_responses() -> LLMRequest {
    let s = object(json!({"apiKey": "test-key", "baseURL": "https://api.githubcopilot.test"}));
    let model = oc_ai::providers::language_model("@opencode/ai/providers/openai-compatible-responses", "gpt-5", &s).unwrap();
    LLMRequest::new(model).prompt("Say hello.")
}

/// openai-compatible-responses.test.ts "keeps one item when GitHub Copilot sends a new item id on every event".
#[test]
fn responses_keep_one_item_across_new_ids() {
    let body = sse(&[
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs-added"}}),
        json!({"type": "response.reasoning_summary_part.added", "output_index": 0, "item_id": "rs-part", "summary_index": 0}),
        json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "item_id": "rs-delta", "summary_index": 0, "delta": "Thinking"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "reasoning", "id": "rs-done", "encrypted_content": "opaque", "summary": []}}),
        json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "message", "id": "msg-added"}}),
        json!({"type": "response.output_text.delta", "output_index": 1, "item_id": "msg-delta", "delta": "Hello"}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "message", "id": "msg-done"}}),
        json!({"type": "response.completed", "response": {"id": "resp_1"}}),
    ]);
    let out = response(&copilot_responses(), &body);
    assert_eq!(out.events.iter().filter(|e| matches!(e, LLMEvent::ReasoningStart { .. })).count(), 1);
    let content = serde_json::to_value(&out.message.content).unwrap();
    assert_eq!(content[0]["type"], "reasoning");
    assert_eq!(content[0]["text"], "Thinking");
    assert_eq!(content[0]["providerMetadata"]["openai-compatible"]["itemId"], "rs-added");
    assert_eq!(content[0]["providerMetadata"]["openai-compatible"]["reasoningEncryptedContent"], "opaque");
    assert_eq!((content[1]["type"].clone(), content[1]["text"].clone()), (json!("text"), json!("Hello")));
}

/// openai-compatible-responses.test.ts "names the GitHub Copilot content filter that blocked a response" and
/// "converts the GitHub Copilot billing on the completed event to USD".
#[test]
fn responses_name_the_filter_and_bill_copilot_credits() {
    let blocked = sse(&[json!({"type": "response.incomplete", "response": {"id": "resp_1", "incomplete_details": {"reason": "content_filter"},
        "content_filters": [{"source_type": "prompt", "blocked": false},
                            {"source_type": "completion", "blocked": true, "content_filter_raw": [{"action": "BLOCK", "label": "TextCopyright", "result": true}]}]}})]);
    let finish = response(&copilot_responses(), &blocked).finish_reason;
    assert_eq!((finish.normalized, finish.category.as_deref()), (FinishReason::ContentFilter, Some("TextCopyright")));
    // Without a blocking rule, the blocked filter's results name it.
    let results = sse(&[json!({"type": "response.incomplete", "response": {"incomplete_details": {"reason": "content_filter"},
        "content_filters": [{"blocked": true, "content_filter_results": {"self_harm": {"filtered": true}}}]}})]);
    assert_eq!(response(&copilot_responses(), &results).finish_reason.category.as_deref(), Some("self_harm"));

    let billed = sse(&[json!({"type": "response.completed", "response": {"id": "resp_1", "usage": {"input_tokens": 5, "output_tokens": 2, "total_tokens": 7}},
        "copilot_usage": {"total_nano_aiu": 4_473_525_000u64}})]);
    let usage = response(&copilot_responses(), &billed).usage.unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens, usage.cost), (Some(5), Some(2), Some(0.04473525)));
}

/// anthropic-messages.test.ts "converts the GitHub Copilot billing on the message delta to USD".
#[test]
fn anthropic_bills_copilot_credits_on_the_message_delta() {
    let req = LLMRequest::new(am::route().auth(auth::none()).model("claude-sonnet-4-5").unwrap()).prompt("Hi");
    let body = sse(&[
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 5}}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}, "copilot_usage": {"total_nano_aiu": 4_473_525_000u64}}),
        json!({"type": "message_stop"}),
    ]);
    let usage = response(&req, &body).usage.unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens, usage.cost), (Some(5), Some(2), Some(0.04473525)));
}

/// cohere.test.ts "Cohere rejects incomplete streams": the generic incomplete stream, which retries.
#[test]
fn cohere_streams_that_end_early_retry() {
    let req = LLMRequest::new(cc::route().auth(auth::none()).model("command-a-03-2025").unwrap()).prompt("Hi");
    let error = failure(&req, &sse(&[json!({"type": "message-start"})]));
    assert_eq!(error.message, "The provider response ended unexpectedly.");
    assert_eq!(error.tag(), "InvalidProviderOutput");
    assert!(error.is_retryable());
}

fn interactions() -> LLMRequest {
    let s = object(json!({"apiKey": "test", "baseURL": "https://generativelanguage.test/v1beta"}));
    LLMRequest::new(oc_ai::providers::language_model("@opencode/ai/providers/google/interactions", "gemini-3.8-flash", &s).unwrap()).prompt("Hi")
}

fn route_of(error: &AIError) -> Option<String> {
    match &error.kind {
        ErrorKind::InvalidProviderOutput { route, .. } => route.clone(),
        _ => None,
    }
}

/// google-interactions.test.ts (new in 2.2.7).
#[test]
fn interactions_failures_and_finishes() {
    let req = interactions();
    // Premature EOF: the generic incomplete stream.
    let error = failure(&req, &sse(&[
        json!({"event_type": "interaction.created", "interaction": {"id": "int_1"}}),
        json!({"event_type": "step.start", "index": 0, "step": {"type": "model_output", "content": []}}),
        json!({"event_type": "step.delta", "index": 0, "delta": {"type": "text", "text": "Partial"}}),
    ]));
    assert_eq!(error.message, "The provider response ended unexpectedly.");
    assert!(error.is_retryable());
    assert_eq!(route_of(&error).as_deref(), Some("google/google-interactions"));

    // Completed with function calls finishes as tool calls.
    let with_call = response(&req, &sse(&[
        json!({"event_type": "step.start", "index": 0, "step": {"type": "function_call", "id": "call_1", "name": "lookup", "arguments": {"query": "weather"}}}),
        json!({"event_type": "step.stop", "index": 0}),
        json!({"event_type": "interaction.completed", "interaction": {"id": "int_1", "status": "completed"}}),
    ]));
    assert_eq!((with_call.finish_reason.normalized, with_call.finish_reason.raw.as_deref()), (FinishReason::ToolCalls, Some("completed")));

    for code in ["malformed_function_call", "malformed_tool_call", "unexpected_tool_call", "malformed_response"] {
        let completed = json!({"event_type": "interaction.completed", "interaction": {"status": "failed", "errors": [{"code": code, "message": format!("Stopped: {code}")}]}});
        let error = failure(&req, &sse(std::slice::from_ref(&completed)));
        assert_eq!((error.tag(), error.message.clone(), error.is_retryable()), ("InvalidProviderOutput", format!("Stopped: {code}"), true), "{code}");
        assert_eq!(error.body.as_deref(), Some(completed.to_string().as_str()));
        assert_eq!(route_of(&error).as_deref(), Some("google/google-interactions"));
        let streamed = json!({"event_type": "error", "error": {"code": code, "message": format!("Stream error: {code}")}});
        let error = failure(&req, &sse(&[streamed]));
        assert_eq!((error.message.clone(), error.is_retryable()), (format!("Stream error: {code}"), true));
    }
    for code in ["missing_thought_signature", "too_many_tool_calls", "no_image"] {
        let streamed = json!({"event_type": "error", "error": {"code": code, "message": format!("Stream rejected: {code}")}});
        let error = failure(&req, &sse(&[streamed]));
        assert_eq!((error.tag(), error.is_retryable()), ("InvalidProviderOutput", false), "{code}");
    }
    for code in ["safety", "recitation", "prohibited_content", "spii", "blocklist", "image_safety", "image_prohibited_content", "image_recitation", "content_blocked"] {
        let streamed = json!({"event_type": "error", "error": {"error_type": code, "message": format!(" Stream blocked: {code} ")}});
        let error = failure(&req, &sse(&[streamed]));
        assert_eq!((error.tag(), error.message.clone(), error.is_retryable()), ("ContentPolicy", format!("Stream blocked: {code}"), false), "{code}");
    }
    let rate = failure(&req, &sse(&[json!({"event_type": "error", "error": "Rate limit exceeded, too many requests"})]));
    assert_eq!((rate.tag(), rate.message.as_str()), ("RateLimit", "Rate limit exceeded, too many requests"));
    let bare = failure(&req, &sse(&[json!({"event_type": "error"})]));
    assert_eq!((bare.tag(), bare.message.as_str()), ("UnknownProvider", "Google Interactions stream error"));
}

/// gemini.test.ts "counts cumulative tool-use prompts once alongside cached input".
#[test]
fn gemini_counts_tool_use_prompts() {
    let req = LLMRequest::new(g::route().auth(auth::none()).model("gemini-2.5-flash").unwrap()).prompt("Hi");
    let body = sse(&[
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "Working"}]}}], "usageMetadata": {"promptTokenCount": 271, "cachedContentTokenCount": 100, "toolUsePromptTokenCount": 200}}),
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": " done"}]}, "finishReason": "STOP"}],
               "usageMetadata": {"promptTokenCount": 271, "cachedContentTokenCount": 100, "toolUsePromptTokenCount": 480, "candidatesTokenCount": 130, "thoughtsTokenCount": 158}}),
    ]);
    let usage = response(&req, &body).usage.unwrap();
    assert_eq!(
        (usage.input_tokens, usage.non_cached_input_tokens, usage.cache_read_input_tokens, usage.output_tokens, usage.reasoning_tokens, usage.total_tokens),
        (Some(751), Some(651), Some(100), Some(288), Some(158), Some(1039))
    );
    let google = &usage.provider_metadata.unwrap()["google"];
    assert_eq!((google["promptTokenCount"].clone(), google["toolUsePromptTokenCount"].clone()), (json!(271), json!(480)));
}

fn defect() -> Map<String, Value> {
    object(json!({"name": "defect", "type": "predicate", "instructions": "Is the customer reporting a product defect?",
                  "criteria": {"true": "The product itself is broken or faulty.", "false": "Preference, sizing, shipping, or pricing complaint."}}))
}

async fn decide(package: &str, id: &str, questions: Vec<Map<String, Value>>, answer: Value) -> Value {
    let server = http_mock::mock(move |_, _| http_mock::Resp::json(200, answer.clone())).await;
    let settings = object(json!({"apiKey": "test", "baseURL": format!("{}/v1", server.base)}));
    let model = match package {
        "openai" => decision::openai::route()
            .with_base_url(Some(format!("{}/v1", server.base)))
            .with_auth(auth::bearer_options(Some("openai-key".into()), &[]))
            .model(id, None, Some(Map::new())),
        _ => oc_ai::providers::media::decision_model(&format!("@opencode/ai/providers/{package}"), id, &settings).unwrap().unwrap(),
    };
    let request = DecisionRequest { model, input: vec![DecisionPart::Text("I returned the sweater because I prefer green.".into())], questions, provider_options: None, http: None };
    decision::generate(&reqwest::Client::new(), request).await.unwrap();
    server.requests().pop().unwrap().json()["questions"].clone()
}

/// decision.test.ts "sends predicate criteria as System One criteria" and "appends predicate criteria to OpenAI
/// Decisions instructions".
#[tokio::test]
async fn predicate_criteria() {
    let sent = decide("typesafe-ai/decisions", "jev-latest", vec![defect()], json!({"model": "jev-latest", "answers": {"defect": {"type": "noul", "noul": 0.03}}})).await;
    assert_eq!(
        sent,
        json!({"defect": {"type": "noul", "instructions": "Is the customer reporting a product defect?",
                           "criteria": {"true": "The product itself is broken or faulty.", "false": "Preference, sizing, shipping, or pricing complaint."}}})
    );
    let mut only_true = defect();
    only_true.insert("name".into(), json!("only_true"));
    only_true.insert("criteria".into(), json!({"true": "The product is broken."}));
    let sent = decide(
        "openai",
        "gpt-6-luna",
        vec![defect(), only_true],
        json!({"model": "gpt-6-luna", "answers": [{"type": "predicate", "name": "defect", "probability": 0.03}, {"type": "predicate", "name": "only_true", "probability": 0.04}]}),
    )
    .await;
    assert_eq!(
        sent,
        json!([
            {"name": "defect", "type": "predicate", "instructions": "Is the customer reporting a product defect?\n\nTrue means: The product itself is broken or faulty.\nFalse means: Preference, sizing, shipping, or pricing complaint."},
            {"name": "only_true", "type": "predicate", "instructions": "Is the customer reporting a product defect?\n\nTrue means: The product is broken."},
        ])
    );
}

/// azure.test.ts "Azure Messages".
#[tokio::test]
async fn azure_messages_for_foundry_claude() {
    let messages = |settings: Value| {
        oc_ai::providers::language_model("@opencode/ai/providers/azure/messages", "claude-haiku-4-5", &object(settings)).unwrap()
    };
    let compiled = Client::new().compile(&LLMRequest::new(messages(json!({"resourceName": "fixture", "apiKey": "key"}))).prompt("Hi")).await.unwrap();
    let header = |name: &str| compiled.prepared.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
    assert_eq!(compiled.prepared.url, "https://fixture.services.ai.azure.com/anthropic/v1/messages");
    assert_eq!(header("x-api-key").as_deref(), Some("key"));
    assert_eq!(header("api-key"), None);
    assert_eq!(header("anthropic-version").as_deref(), Some("2023-06-01"));
    assert_eq!(compiled.request.model.route.id, "azure-anthropic-messages");
    assert_eq!(compiled.request.model.provider, "azure");
    let custom = Client::new().compile(&LLMRequest::new(messages(json!({"baseURL": "https://gateway.example/anthropic/v1/", "apiKey": "key"}))).prompt("Hi")).await.unwrap();
    assert_eq!(custom.prepared.url, "https://gateway.example/anthropic/v1/messages");
}

/// Bun's fetch reports a refused connection as `ECONNREFUSED` with its own message (route/executor.ts lifts the
/// system code), which the session's retry policy and error message read.
#[tokio::test]
async fn refused_connections_carry_bun_codes() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let model = oc::compatible_route().base_url(Some(format!("http://127.0.0.1:{port}/v1"))).auth(auth::none()).model("m").unwrap();
    let error = match Client::new().stream(LLMRequest::new(model).prompt("Hi"), CancellationToken::new()).await {
        Err(error) => error,
        Ok(_) => panic!("expected a refused connection"),
    };
    assert_eq!(error.message, "ECONNREFUSED: Unable to connect. Is the computer able to access the url?");
    assert!(matches!(&error.kind, ErrorKind::Transport { code: Some(c), .. } if c == "ECONNREFUSED"));
    assert_eq!(oc_ai::route::transport::system_message("ENOTFOUND", "api.example.test"), "getaddrinfo ENOTFOUND api.example.test");
    assert_eq!(oc_ai::route::transport::system_message("ENETUNREACH", "x"), "ENETUNREACH: Was there a typo in the url or port?");
}

// ---------------------------------------------------------------------------------------------
// Cross-checks with upstream 2.2.7's own routes (Bun on the checkout)
// ---------------------------------------------------------------------------------------------

/// The 2.2.7 checkout's `packages/ai` and the Bun binary, when both are available.
fn upstream() -> Option<(std::path::PathBuf, String)> {
    let (Ok(scratch), Ok(bun)) = (std::env::var("OCD_SCRATCH"), std::env::var("OCD_BUN_PATH")) else { return None };
    let ai = std::path::PathBuf::from(scratch).join("src/opencode-2.2.7/packages/ai");
    ai.join("src/providers/azure/messages.ts").is_file().then_some((ai, bun))
}

fn numbers(value: Value) -> Value {
    match value {
        Value::Number(n) => json!(n.as_f64().unwrap()),
        Value::Array(items) => Value::Array(items.into_iter().map(numbers).collect()),
        Value::Object(o) => Value::Object(o.into_iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k, numbers(v))).collect()),
        other => other,
    }
}

/// Runs `script` (TypeScript) under Bun in the checkout with `input` as JSON and returns its JSON output.
fn run_bun(ai: &std::path::Path, bun: &str, script: &str, input: &Value) -> Value {
    // In the checkout, so its packages resolve (Bun never installs them).
    let dir = tempfile::tempdir_in(ai).unwrap();
    let path = dir.path().join("check.ts");
    std::fs::write(&path, script.replace("$AI", &format!("{:?}", ai.display().to_string()))).unwrap();
    let data = dir.path().join("in.json");
    std::fs::write(&data, serde_json::to_string(input).unwrap()).unwrap();
    let output = std::process::Command::new(bun)
        .arg("run")
        .arg("--no-install")
        .arg(&path)
        .arg(&data)
        .arg(dir.path().join("out.json"))
        .current_dir(ai)
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("AZURE_OPENAI_API_KEY")
        .output()
        .unwrap();
    assert!(output.status.success(), "bun failed: {}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&std::fs::read(dir.path().join("out.json")).unwrap()).unwrap()
}

const DECODE_SCRIPT: &str = r#"import { Effect, Stream } from "effect"
import { readFileSync, writeFileSync } from "node:fs"
const ai = $AI
const { ai: AI } = await import(ai + "/src/effect.ts")
const { LLMClient } = await import(ai + "/src/route.ts")
const { fixedResponse } = await import(ai + "/test/lib/http.ts")
const jobs = JSON.parse(readFileSync(process.argv[2], "utf8"))
const out = []
for (const job of jobs) {
  const pkg = await import(ai + "/src/providers/" + job.package + ".ts")
  const model = pkg.model(job.model, job.settings)
  const body = Buffer.from(job.body, "base64")
  const events = []
  const exit = await Effect.runPromiseExit(
    LLMClient.stream(AI.request({ model, prompt: "x" })).pipe(
      Stream.runForEach((event) => Effect.sync(() => events.push(event))),
      Effect.provide(fixedResponse(body, { headers: { "content-type": job.contentType } })),
    ),
  )
  if (exit._tag === "Failure") {
    const error = exit.cause.reasons?.[0]?.error
    events.push({ error: error?.reason?._tag ?? "Defect", message: error?.reason?.message ?? String(exit.cause) })
  }
  out.push(JSON.parse(JSON.stringify(events)))
}
writeFileSync(process.argv[3], JSON.stringify(out))
"#;

/// Decodes every recorded success response of the corpus (`tests/fixtures/recorded`, upstream's recordings, with
/// 2.2.7's Azure Messages and Foundry recordings) with upstream 2.2.7's own route and with ocd, and compares the
/// events. Needs `OCD_SCRATCH` and `OCD_BUN_PATH`; skipped otherwise.
#[test]
fn recordings_decode_like_upstream_2_2_7() {
    let Some((ai, bun)) = upstream() else { return };
    let root = format!("{}/tests/fixtures/recorded", env!("CARGO_MANIFEST_DIR"));
    let (mut jobs, mut ours, mut names) = (vec![], vec![], vec![]);
    let mut dirs: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().collect();
    dirs.sort_by_key(|d| d.file_name());
    for dir in dirs {
        let entry_name = dir.file_name().to_string_lossy().to_string();
        let (dir_name, files) = if dir.path().is_dir() {
            (entry_name, std::fs::read_dir(dir.path()).unwrap().flatten().map(|f| f.path()).collect::<Vec<_>>())
        } else {
            (entry_name.split('-').next().unwrap_or_default().to_string(), vec![dir.path()])
        };
        // Freeform (custom) tools are library-only (core never declares them), so their recordings are left out.
        let mut files: Vec<_> = files.into_iter().filter(|p| !p.to_string_lossy().contains("freeform")).collect();
        files.sort();
        for file in files {
            let rec = oc_ai::testing::recording(&std::fs::read_to_string(&file).unwrap());
            for (i, interaction) in rec.interactions.iter().enumerate() {
                if interaction.response.status != 200 || interaction.response.body.len() < 10 {
                    continue;
                }
                let url = url::Url::parse(&interaction.request.url).unwrap();
                let Some(package_name) = corpus_package(&dir_name, url.path()) else { continue };
                let body: Value = serde_json::from_str(&interaction.request.body).unwrap_or(json!({}));
                let id = body.get("model").or_else(|| body.get("modelId")).and_then(Value::as_str).unwrap_or("gemini-2.5-flash").to_string();
                let settings = json!({"apiKey": "fixture", "region": "us-east-1", "resourceName": "fixture", "accountId": "fixture"});
                let model = oc_ai::providers::language_model(&format!("@opencode/ai/providers/{package_name}"), &id, settings.as_object().unwrap()).unwrap();
                let events: Vec<Value> = decode(&LLMRequest::new(model), &interaction.response.bytes())
                    .into_iter()
                    .map(|e| match e {
                        Ok(event) => serde_json::to_value(event).unwrap(),
                        Err(error) => json!({"error": error.tag(), "message": error.message}),
                    })
                    .collect();
                ours.push(numbers(Value::Array(events)));
                names.push(format!("{dir_name}/{}#{i}", file.file_name().unwrap_or_default().to_string_lossy()));
                jobs.push(json!({
                    "package": package_name,
                    "model": id,
                    "settings": settings,
                    "body": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, interaction.response.bytes()),
                    "contentType": interaction.response.headers.get("content-type").cloned().unwrap_or(json!("text/event-stream")),
                }));
            }
        }
    }
    let theirs = run_bun(&ai, &bun, DECODE_SCRIPT, &Value::Array(jobs));
    let theirs = theirs.as_array().unwrap();
    assert_eq!(theirs.len(), ours.len());
    let differing: Vec<String> = theirs
        .iter()
        .zip(ours.iter())
        .enumerate()
        .filter(|(_, (theirs, ours))| numbers((*theirs).clone()) != **ours)
        .map(|(i, (theirs, ours))| format!("{}\n  upstream: {}\n  ocd:      {}", names[i], numbers(theirs.clone()), ours))
        .collect();
    eprintln!("compared {} recorded responses with upstream 2.2.7", ours.len());
    assert!(names.iter().any(|n| n.starts_with("azure/messages")), "the Azure Messages recordings are in the corpus");
    assert!(differing.is_empty(), "{} differ:\n{}", differing.len(), differing.join("\n"));
}

const COMPILE_SCRIPT: &str = r#"import { Effect } from "effect"
import { HttpClientRequest } from "effect/http"
import { readFileSync, writeFileSync } from "node:fs"
const ai = $AI
const { ai: AI, ToolDefinition } = await import(ai + "/src/effect.ts")
const { LLMClient } = await import(ai + "/src/route.ts")
const { dynamicResponse } = await import(ai + "/test/lib/http.ts")
const cases = JSON.parse(readFileSync(process.argv[2], "utf8"))
const out = []
for (const item of cases) {
  const pkg = await import(ai + "/src/providers/" + item.package + ".ts")
  const model = pkg.model(item.model, item.settings)
  const { tools, generation, ...rest } = item.request
  const request = AI.request({ model, ...rest, ...generation, ...(tools ? { tools: tools.map((t) => ToolDefinition.make(t)) } : {}) })
  let sent
  const exit = await Effect.runPromiseExit(
    LLMClient.generate(request).pipe(
      Effect.provide(
        dynamicResponse((input) =>
          Effect.gen(function* () {
            const web = yield* HttpClientRequest.toWeb(input.request).pipe(Effect.orDie)
            const headers = {}
            for (const name of ["anthropic-beta", "anthropic-version", "authorization", "x-api-key", "api-key", "content-type"]) {
              const value = web.headers.get(name)
              if (value !== null) headers[name] = value
            }
            sent = { url: web.url, body: input.text, headers }
            return input.respond("{}", { status: 400 })
          }),
        ),
      ),
    ),
  )
  if (sent) out.push(sent)
  else out.push({ error: exit.cause?.reasons?.[0]?.error?.reason?.message ?? String(exit.cause) })
}
writeFileSync(process.argv[3], JSON.stringify(out))
"#;

/// Compiles requests on the routes 2.2.7 added or changed with upstream's routes (capturing the HTTP request) and
/// with ocd, and compares the URL, the body text (key order included) and the auth headers. Needs `OCD_SCRATCH`
/// and `OCD_BUN_PATH`; skipped otherwise.
#[tokio::test]
async fn request_bodies_match_upstream_2_2_7() {
    let Some((ai, bun)) = upstream() else { return };
    let lookup = json!({"name": "lookup", "description": "Lookup data", "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}}});
    let cases = vec![
        json!({"package": "azure/messages", "model": "claude-haiku-4-5", "settings": {"resourceName": "fixture", "apiKey": "key"}, "request": {"prompt": "Hi", "tools": [lookup]}}),
        json!({"package": "azure/messages", "model": "claude-haiku-4-5", "settings": {"baseURL": "https://gateway.example/anthropic/v1/", "apiKey": "key", "queryParams": {"a": "b"}},
            "request": {"prompt": "Hi", "generation": {"maxTokens": 64}, "providerOptions": {"thinking": {"type": "enabled", "budgetTokens": 1024}}}}),
        json!({"package": "azure/chat", "model": "Kimi-K2.5", "settings": {"baseURL": "https://fixture.services.ai.azure.com/models", "apiKey": "key"}, "request": {"prompt": "Hi"}}),
        json!({"package": "openai-compatible", "model": "gemini-3-pro", "settings": {"baseURL": "https://api.githubcopilot.test", "apiKey": "gho", "provider": "github-copilot"},
            "request": {"prompt": "Hi", "tools": [lookup], "providerOptions": {"reasoningEffort": "high"}}}),
        json!({"package": "openai-compatible-responses", "model": "gpt-5.4", "settings": {"baseURL": "https://api.githubcopilot.test", "apiKey": "gho", "provider": "github-copilot"},
            "request": {"prompt": "Hi", "tools": [lookup], "providerOptions": {"reasoningEffort": "low", "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]}}}),
        json!({"package": "anthropic", "model": "claude-sonnet-4.6", "settings": {"baseURL": "https://api.githubcopilot.test/v1", "apiKey": "gho"},
            "request": {"prompt": "Hi", "providerOptions": {"thinking": {"type": "adaptive", "display": "summarized"}, "effort": "high"}}}),
        json!({"package": "cohere", "model": "command-a-03-2025", "settings": {"apiKey": "key"}, "request": {"prompt": "Hi", "generation": {"maxTokens": 64, "topK": 5}}}),
        json!({"package": "google", "model": "gemini-2.5-flash", "settings": {"apiKey": "key"}, "request": {"prompt": "Hi", "generation": {"maxTokens": 64}}}),
    ];
    let mut ours = vec![];
    for case in &cases {
        let package_name = case["package"].as_str().unwrap();
        let model = oc_ai::providers::language_model(&format!("@opencode/ai/providers/{package_name}"), case["model"].as_str().unwrap(), case["settings"].as_object().unwrap()).unwrap();
        let spec = &case["request"];
        let mut req = LLMRequest::new(model).prompt(spec["prompt"].as_str().unwrap());
        req.provider_options = spec.get("providerOptions").and_then(Value::as_object).cloned();
        req.generation = spec.get("generation").map(|g| serde_json::from_value(g.clone()).unwrap());
        for tool in spec.get("tools").and_then(Value::as_array).into_iter().flatten() {
            req = req.tool(ToolDefinition::new(tool["name"].as_str().unwrap(), tool["description"].as_str().unwrap(), tool["inputSchema"].clone()));
        }
        ours.push(match Client::new().compile(&req).await {
            Ok(compiled) => {
                let headers: Map<String, Value> = ["anthropic-beta", "anthropic-version", "authorization", "x-api-key", "api-key", "content-type"]
                    .iter()
                    .filter_map(|h| compiled.prepared.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(h)).map(|(_, v)| (h.to_string(), json!(v))))
                    .collect();
                json!({"url": compiled.prepared.url, "body": compiled.prepared.body, "headers": headers})
            }
            Err(error) => json!({"error": error.message}),
        });
    }
    let theirs = run_bun(&ai, &bun, COMPILE_SCRIPT, &Value::Array(cases.clone()));
    let theirs = theirs.as_array().unwrap();
    let differing: Vec<String> = theirs
        .iter()
        .zip(ours.iter())
        .enumerate()
        .filter(|(_, (theirs, ours))| *theirs != *ours)
        .map(|(i, (theirs, ours))| format!("case {i} ({} {})\n  upstream: {theirs}\n  ocd:      {ours}", cases[i]["package"], cases[i]["model"]))
        .collect();
    eprintln!("compared {} requests with upstream 2.2.7", theirs.len());
    assert!(differing.is_empty(), "{} of {} differ:\n{}", differing.len(), cases.len(), differing.join("\n"));
}

/// The provider entrypoint of a recording directory (`recorded_streams.rs`), by the endpoint path.
fn corpus_package(dir: &str, path: &str) -> Option<&'static str> {
    let kind = if path.ends_with("/chat/completions") {
        "chat"
    } else if path.ends_with("/messages") {
        "messages"
    } else if path.ends_with("/responses") {
        "responses"
    } else {
        "native"
    };
    Some(match (dir, kind) {
        ("alibaba-chat" | "alibaba-messages" | "alibaba-responses", "chat") => "alibaba/chat",
        ("alibaba-chat" | "alibaba-messages" | "alibaba-responses", "messages") => "alibaba/messages",
        ("alibaba-chat" | "alibaba-messages" | "alibaba-responses", "responses") => "alibaba/responses",
        ("azure", "chat") => "azure/chat",
        ("azure", "responses") => "azure/responses",
        ("azure", "messages") => "azure/messages",
        ("bedrock-mantle" | "bedrock-mantle-messages", "responses") => "amazon-bedrock/mantle/responses",
        ("bedrock-mantle" | "bedrock-mantle-messages", "chat") => "amazon-bedrock/mantle/chat",
        ("bedrock-mantle" | "bedrock-mantle-messages", "messages") => "amazon-bedrock/mantle/messages",
        ("cerebras-chat", _) => "cerebras",
        ("cloudflare-workers-ai", _) => "cloudflare-workers-ai",
        ("deepinfra-chat", _) => "deepinfra",
        ("digitalocean-chat", _) => "digitalocean",
        ("groq-chat", _) => "groq",
        (d, "chat") if d.starts_with("meta") => "meta/chat",
        (d, "messages") if d.starts_with("meta") => "meta/messages",
        (d, "responses") if d.starts_with("meta") => "meta/responses",
        (d, "chat") if d.starts_with("minimax") => "minimax/chat",
        (d, "messages") if d.starts_with("minimax") => "minimax/messages",
        (d, "responses") if d.starts_with("minimax") => "minimax/responses",
        (d, "chat") if d.starts_with("moonshot") => "moonshot/chat",
        (d, "messages") if d.starts_with("moonshot") => "moonshot/messages",
        (d, "responses") if d.starts_with("moonshot") => "moonshot/responses",
        ("venice-chat", _) => "venice",
        ("zai-chat", _) => "zai/chat",
        ("vercel-native", "chat") => "vercel-ai-gateway/chat",
        ("vercel-native", "messages") => "vercel-ai-gateway/messages",
        ("vercel-native", "responses") => "vercel-ai-gateway/responses",
        ("openai-responses-cache", _) => "openai/responses",
        ("gemini-cache", _) => "google",
        ("anthropic-messages-cache", _) => "anthropic",
        ("bedrock-converse-cache" | "bedrock-converse-options", _) => "amazon-bedrock",
        ("openrouter", "chat") => "openrouter",
        ("openrouter-responses", _) => "openrouter/responses",
        ("digitalocean-responses", _) => "digitalocean/responses",
        ("venice-responses", _) => "venice/responses",
        _ => return None,
    })
}
