//! Ports of the protocol tests opencode 2.2 added (packages/ai/test): video input, Anthropic fast mode, reported
//! costs, Responses reasoning mode/context, xAI top_k, Google Interactions failures, Bedrock Converse options, the
//! Open Responses routes of OpenRouter, DigitalOcean and Venice, and local file references. Recorded requests are
//! compared byte for byte with upstream's recordings.

mod common;

use common::*;
use oc_ai::protocols::{anthropic_messages as am, open_responses as orp};
use oc_ai::route::auth;
use oc_ai::testing::{compile_body, compile_text, decode, decode_ok, types};
use oc_ai::*;
use serde_json::{json, Value};

fn settings(value: Value) -> serde_json::Map<String, Value> {
    value.as_object().cloned().unwrap()
}

fn package(name: &str, id: &str, extra: Value) -> LanguageModel {
    let mut s = settings(json!({"apiKey": "test"}));
    s.extend(settings(extra));
    oc_ai::providers::language_model(&format!("@opencode/ai/providers/{name}"), id, &s).unwrap()
}

fn messages(value: Value) -> Vec<Message> {
    serde_json::from_value(value).expect("messages")
}

fn request(model: LanguageModel, msgs: Value) -> LLMRequest {
    let mut req = LLMRequest::new(model);
    req.messages = messages(msgs);
    req
}

fn sse(values: &[Value]) -> Vec<u8> {
    values.iter().map(|v| format!("data: {v}\n\n")).collect::<String>().into_bytes()
}

fn usage_of(events: &[LLMEvent]) -> Usage {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            LLMEvent::Finish { usage, .. } => usage.clone(),
            _ => None,
        })
        .expect("usage")
}

fn video_file(uri: &str) -> Value {
    json!({"type": "file", "uri": uri, "mime": "video/mp4"})
}

fn tool_call(id: &str, name: &str) -> Value {
    json!({"type": "tool-call", "id": id, "name": name, "input": {}})
}

fn tool_result(id: &str, name: &str, files: Vec<Value>) -> Value {
    json!({"role": "tool", "content": [{"type": "tool-result", "id": id, "name": name, "result": {"type": "content", "value": files}}]})
}

// ---------------------------------------------------------------------------------------------
// Video input
// ---------------------------------------------------------------------------------------------

/// anthropic-messages.test.ts "serializes Messages video inputs"
#[tokio::test]
async fn anthropic_serializes_video_inputs() {
    let video = json!({"type": "video", "source": {"type": "base64", "media_type": "video/mp4", "data": "AAECAw=="}});
    let custom = || am::route().provider("custom").auth(auth::none()).model("video-model").unwrap();
    let user = json!([{"role": "user", "content": [
        {"type": "media", "media": {"source": {"type": "base64", "data": "AAECAw==", "mediaType": "video/mp4"}}, "cache": {"type": "ephemeral"}},
        {"type": "media", "media": {"source": {"type": "ref", "provider": "custom", "id": "file_clip", "mediaType": "video/mp4"}}},
    ]}]);
    let mut req = request(custom(), user.clone());
    req.cache = Some(CachePolicy::none());
    let body = compile_body(&req).await.unwrap();
    let mut cached = video.clone();
    cached["cache_control"] = json!({"type": "ephemeral"});
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [cached, {"type": "video", "source": {"type": "file", "file_id": "file_clip"}}]}])
    );

    let mut nested = request(
        custom(),
        json!([
            {"role": "assistant", "content": [tool_call("clip", "read")]},
            tool_result("clip", "read", vec![video_file("data:video/mp4;base64,AAECAw=="), video_file("storage://clip")]),
        ]),
    );
    nested.cache = Some(CachePolicy::none());
    let body = compile_body(&nested).await.unwrap();
    assert_eq!(
        body["messages"][1]["content"][0]["content"],
        json!([video, {"type": "video", "source": {"type": "url", "url": "storage://clip"}}])
    );

    let mut foreign = request(am::route().auth(auth::none()).model("claude-x").unwrap(), user);
    foreign.cache = Some(CachePolicy::none());
    assert!(compile_body(&foreign).await.unwrap_err().message.contains("got custom:file_clip"));
}

/// "moves Meta tool result videos after consecutive results"
#[tokio::test]
async fn meta_messages_move_tool_result_videos() {
    let history = json!([
        {"role": "assistant", "content": [tool_call("first", "read"), tool_call("second", "read")]},
        tool_result("first", "read", vec![video_file("data:video/mp4;base64,AAECAw==")]),
        tool_result("second", "read", vec![video_file("data:video/mp4;base64,AAECAw==")]),
        {"role": "assistant", "content": [{"type": "text", "text": "Observed."}]},
    ]);
    let video = json!({"type": "video", "source": {"type": "base64", "media_type": "video/mp4", "data": "AAECAw=="}});
    let mut meta = request(package("meta/messages", "video-model", json!({})), history.clone());
    meta.cache = Some(CachePolicy::none());
    let body = compile_body(&meta).await.unwrap();
    let user = &body["messages"][1]["content"];
    assert_eq!(user[0]["content"], json!([]));
    assert_eq!(user[1]["content"], json!([]));
    assert_eq!((&user[2], &user[3]), (&video, &video));
    assert_eq!(body["messages"][2]["content"][0]["text"], "Observed.");

    let mut generic = request(am::route().provider("custom").auth(auth::none()).model("video-model").unwrap(), history);
    generic.cache = Some(CachePolicy::none());
    let body = compile_body(&generic).await.unwrap();
    assert_eq!(
        body["messages"][1]["content"],
        json!([
            {"type": "tool_result", "tool_use_id": "first", "content": [video]},
            {"type": "tool_result", "tool_use_id": "second", "content": [video]},
        ])
    );
}

/// openai-chat: video becomes `video_url`.
#[tokio::test]
async fn openai_chat_sends_video_url() {
    let model = oc_ai::protocols::openai_chat::route().auth(auth::none()).model("gpt-x").unwrap();
    let req = request(
        model,
        json!([{"role": "user", "content": [
            {"type": "media", "media": {"source": {"type": "url", "url": "https://example.test/clip.mp4", "mediaType": "video/mp4"}}},
            {"type": "media", "media": {"source": {"type": "base64", "data": "AAECAw==", "mediaType": "video/mp4"}}},
        ]}]),
    );
    let body = compile_body(&req).await.unwrap();
    assert_eq!(
        body["messages"][0]["content"],
        json!([
            {"type": "video_url", "video_url": {"url": "https://example.test/clip.mp4"}},
            {"type": "video_url", "video_url": {"url": "data:video/mp4;base64,AAECAw=="}},
        ])
    );
}

/// openai-responses.test.ts "lowers Responses video by provider and preserves ordered tool results"
#[tokio::test]
async fn responses_video_by_provider() {
    let video_result = json!({"role": "tool", "content": [{"type": "tool-result", "id": "call_video", "name": "record", "result": {"type": "content", "value": [
        {"type": "file", "uri": "https://example.com/clip.mp4", "mime": "video/mp4", "name": "clip.mp4"}
    ]}}]});
    let history = json!([
        {"role": "user", "content": [{"type": "media", "media": {"source": {"type": "base64", "data": "AAECAw==", "mediaType": "video/mp4"}}, "filename": "inline.mp4"}]},
        {"role": "assistant", "content": [tool_call("call_video", "record")]},
        video_result,
    ]);
    let custom = || orp::compatible_route().provider("custom").base_url(Some("https://provider.example/v1".into())).auth(auth::none()).model("video-model").unwrap();
    let input = compile_body(&request(custom(), history.clone())).await.unwrap()["input"].clone();
    assert_eq!(input[0]["content"][0], json!({"type": "input_video", "video_url": "data:video/mp4;base64,AAECAw=="}));
    assert_eq!(input[2]["output"], json!([{"type": "input_video", "video_url": "https://example.com/clip.mp4"}]));

    let input = compile_body(&request(package("xai", "grok-4", json!({})), history)).await.unwrap()["input"].clone();
    assert_eq!(input[0]["content"][0], json!({"type": "input_file", "filename": "inline.mp4", "file_data": "data:video/mp4;base64,AAECAw=="}));
    assert_eq!(input[2]["output"], json!([{"type": "input_file", "filename": "clip.mp4", "file_url": "https://example.com/clip.mp4"}]));

    let meta_history = json!([
        {"role": "assistant", "content": [tool_call("call_video", "record"), tool_call("call_text", "read")]},
        video_result,
        {"role": "tool", "content": [{"type": "tool-result", "id": "call_text", "name": "read", "result": {"type": "text", "value": "plain result"}}]},
    ]);
    let meta_input = json!([
        {"type": "function_call", "call_id": "call_video", "name": "record", "arguments": "{}"},
        {"type": "function_call", "call_id": "call_text", "name": "read", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "call_video", "output": "Video attached in the following user message."},
        {"type": "function_call_output", "call_id": "call_text", "output": "plain result"},
        {"type": "message", "role": "user", "content": [{"type": "input_video", "video_url": "https://example.com/clip.mp4"}]},
    ]);
    let meta = || package("meta/responses", "muse-spark-1.3", json!({}));
    let strip = |items: &Value| -> Value {
        Value::Array(
            items
                .as_array()
                .unwrap()
                .iter()
                .map(|i| {
                    let mut i = i.clone();
                    i.as_object_mut().unwrap().remove("id");
                    i
                })
                .collect(),
        )
    };
    let body = compile_body(&request(meta(), meta_history.clone())).await.unwrap();
    assert_eq!(strip(&body["input"]), meta_input);
    let mut continued = meta_history.as_array().unwrap().clone();
    continued.push(json!({"role": "user", "content": [{"type": "text", "text": "continue"}]}));
    let body = compile_body(&request(meta(), Value::Array(continued))).await.unwrap();
    let mut expected = meta_input.as_array().unwrap().clone();
    expected.push(json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "continue"}]}));
    assert_eq!(strip(&body["input"]), Value::Array(expected));
    let body = compile_body(&request(custom(), meta_history)).await.unwrap();
    assert_eq!(body["input"][2]["output"], json!([{"type": "input_video", "video_url": "https://example.com/clip.mp4"}]));
}

/// gemini.test.ts "forwards native video URLs in user and tool input"
#[tokio::test]
async fn gemini_forwards_native_video_urls() {
    let uri = "https://example.test/clip.mp4";
    let history = json!([
        {"role": "user", "content": [{"type": "media", "media": {"source": {"type": "url", "url": uri, "mediaType": "video/mp4"}}}]},
        {"role": "assistant", "content": [tool_call("call_video", "read")]},
        tool_result("call_video", "read", vec![video_file(uri)]),
    ]);
    let body = compile_body(&request(package("google", "gemini-3-flash-preview", json!({})), history.clone())).await.unwrap();
    assert_eq!(
        body["contents"],
        json!([
            {"role": "user", "parts": [{"fileData": {"mimeType": "video/mp4", "fileUri": uri}}]},
            {"role": "model", "parts": [{"functionCall": {"id": "call_video", "name": "read", "args": {}}, "thoughtSignature": "skip_thought_signature_validator"}]},
            {"role": "user", "parts": [{"functionResponse": {"id": "call_video", "name": "read", "response": {"name": "read", "content": ""}}}]},
            {"role": "user", "parts": [{"text": "Attached media from tool result:"}, {"fileData": {"mimeType": "video/mp4", "fileUri": uri}}]},
        ])
    );
    let body = compile_body(&request(package("google/interactions", "gemini-3-flash-preview", json!({})), history)).await.unwrap();
    assert_eq!(
        body["input"],
        json!([
            {"type": "user_input", "content": [{"type": "video", "uri": uri, "mime_type": "video/mp4"}]},
            {"type": "function_call", "id": "call_video", "name": "read", "arguments": {}},
            {"type": "function_result", "call_id": "call_video", "name": "read", "result": [{"type": "video", "uri": uri, "mime_type": "video/mp4"}]},
        ])
    );
    let image = request(
        package("google", "gemini-3-flash-preview", json!({})),
        json!([{"role": "user", "content": [{"type": "media", "media": {"source": {"type": "url", "url": "https://example.test/image.png", "mediaType": "image/png"}}}]}]),
    );
    assert_eq!(compile_body(&image).await.unwrap_err().tag(), "InvalidRequest");
}

/// A `gs://` tool-result video is a Google file reference.
#[tokio::test]
async fn gemini_reads_gs_tool_videos_as_file_refs() {
    let history = json!([
        {"role": "assistant", "content": [tool_call("v", "read")]},
        tool_result("v", "read", vec![video_file("gs://bucket/clip.mp4")]),
    ]);
    let body = compile_body(&request(package("google", "gemini-2.5-flash", json!({})), history)).await.unwrap();
    assert_eq!(body["contents"][2]["parts"][1], json!({"fileData": {"mimeType": "video/mp4", "fileUri": "gs://bucket/clip.mp4"}}));
}

/// media.test.ts "rejects local file references as request errors without breaking construction or decoding"
#[tokio::test]
async fn rejects_local_file_references() {
    let model = || package("openai", "video-model", json!({}));
    for source in [
        json!({"type": "url", "url": "file:///Users/example/clip.mp4", "mediaType": "video/mp4"}),
        json!({"type": "ref", "provider": "openai", "id": "file:///Users/example/clip.mp4", "mediaType": "video/mp4"}),
        json!({"type": "url", "url": " FILE:/C:/clips/video.mp4", "mediaType": "video/mp4"}),
    ] {
        let req = request(model(), json!([{"role": "user", "content": [{"type": "media", "media": {"source": source}}]}]));
        let error = compile_body(&req).await.unwrap_err();
        assert_eq!(error.tag(), "InvalidRequest");
        assert_eq!(error.message, "Local file references must be read with Media.file(path) before sending");
    }
    let req = request(
        model(),
        json!([
            {"role": "assistant", "content": [tool_call("read", "read")]},
            tool_result("read", "read", vec![video_file("file:///Users/example/clip.mp4")]),
        ]),
    );
    assert!(compile_body(&req).await.unwrap_err().message.contains("Media.file(path)"));
}

// ---------------------------------------------------------------------------------------------
// Request options
// ---------------------------------------------------------------------------------------------

/// "sends fast mode speed with its beta header"
#[tokio::test]
async fn anthropic_fast_mode_speed() {
    let model = am::route().auth(auth::none()).model("claude-sonnet-4-5").unwrap();
    let mut req = LLMRequest::new(model).prompt("hi");
    req.provider_options = Some(settings(json!({"speed": "fast"})));
    let compiled = Client::new().compile(&req).await.unwrap();
    assert_eq!(compiled.body["speed"], "fast");
    assert_eq!(compiled.prepared.headers.get("anthropic-beta").map(String::as_str), Some("interleaved-thinking-2025-05-14,fast-mode-2026-02-01"));
}

/// "lowers reasoning mode and context into the reasoning object"
#[tokio::test]
async fn responses_reasoning_mode_and_context() {
    let mut req = LLMRequest::new(package("openai", "gpt-5.6", json!({}))).prompt("hi");
    req.provider_options = Some(settings(json!({"reasoningEffort": "high", "reasoningMode": "pro", "reasoningContext": "current_turn"})));
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["reasoning"]["mode"], "pro");
    assert_eq!(body["reasoning"]["context"], "current_turn");
    assert_eq!(body["reasoning"]["effort"], "high");
    assert!(body.get("reasoningMode").is_none() && body.get("reasoningContext").is_none());
    // Without effort or summary the reasoning object holds only the mode.
    let mut req = LLMRequest::new(oc_ai::protocols::openai_responses::route().auth(auth::none()).model("o3").unwrap()).prompt("hi");
    req.provider_options = Some(settings(json!({"reasoningMode": "standard"})));
    assert_eq!(compile_body(&req).await.unwrap()["reasoning"], json!({"mode": "standard"}));
}

/// xai-responses.test.ts "sends top_k and omits unsupported penalties" / "sends min_p from provider options"
#[tokio::test]
async fn xai_top_k_min_p_and_no_penalties() {
    let mut req = LLMRequest::new(package("xai", "grok-4", json!({}))).prompt("Hello");
    req.generation = Some(GenerationOptions { top_k: Some(20.0), presence_penalty: Some(0.5), frequency_penalty: Some(0.5), ..Default::default() });
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["top_k"], 20);
    assert!(body.get("presence_penalty").is_none() && body.get("frequency_penalty").is_none());
    let mut req = LLMRequest::new(package("xai", "grok-4", json!({}))).prompt("Hello");
    req.provider_options = Some(settings(json!({"minP": 0.1})));
    assert_eq!(compile_body(&req).await.unwrap()["min_p"], 0.1);
    req.provider_options = Some(settings(json!({"minP": "x"})));
    assert_eq!(compile_body(&req).await.unwrap_err().message, "Expected number | undefined\n  at [\"minP\"]");
}

// ---------------------------------------------------------------------------------------------
// Reported costs
// ---------------------------------------------------------------------------------------------

/// "reports the cost the endpoint billed" (OpenAI Chat, Responses, Anthropic, Gemini) and "drops a malformed billed
/// cost without failing the stream".
#[test]
fn reports_the_cost_the_endpoint_billed() {
    let chat = LLMRequest::new(oc_ai::protocols::openai_chat::route().auth(auth::none()).model("gpt-x").unwrap());
    for (cost, expected) in [(json!(0), Some(0.0)), (json!(0.25), Some(0.25)), (json!(-1), None), (json!("lots"), None)] {
        let body = sse(&[
            json!({"choices": [{"delta": {"content": "Hello"}, "finish_reason": "stop"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 2, "cost": cost, "opencode": {"allowance": 0.0042}}}),
        ]);
        let usage = usage_of(&decode_ok(&chat, &body));
        assert_eq!((usage.input_tokens, usage.output_tokens, usage.cost), (Some(5), Some(2), expected));
    }

    let responses = LLMRequest::new(oc_ai::protocols::openai_responses::route().auth(auth::none()).model("gpt-x").unwrap());
    let body = sse(&[
        json!({"type": "response.output_item.added", "item": {"type": "message", "id": "msg_1"}}),
        json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "Hello"}),
        json!({"type": "response.completed", "response": {"id": "resp_1", "usage": {"input_tokens": 5, "output_tokens": 2, "cost": 0, "opencode": {"allowance": 0.0042}}}}),
    ]);
    assert_eq!(usage_of(&decode_ok(&responses, &body)).cost, Some(0.0));

    let anthropic = LLMRequest::new(am::route().auth(auth::none()).model("claude-x").unwrap());
    let body = sse(&[
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 5}}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2, "cost": 0, "opencode": {"allowance": 0.0042}}}),
        json!({"type": "message_stop"}),
    ]);
    let usage = usage_of(&decode_ok(&anthropic, &body));
    assert_eq!((usage.input_tokens, usage.output_tokens, usage.cost), (Some(5), Some(2), Some(0.0)));

    let gemini = LLMRequest::new(package("google", "gemini-2.5-flash", json!({})));
    let body = sse(&[json!({
        "candidates": [{"content": {"role": "model", "parts": [{"text": "Hello"}]}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 2, "cost": 0, "opencode": {"allowance": 0.0042}},
    })]);
    let usage = usage_of(&decode_ok(&gemini, &body));
    assert_eq!((usage.input_tokens, usage.output_tokens, usage.cost), (Some(5), Some(2), Some(0.0)));
}

// ---------------------------------------------------------------------------------------------
// Google Interactions failures
// ---------------------------------------------------------------------------------------------

/// google-vertex-interactions.test.ts "classifies failed interactions by their reported errors"
#[test]
fn interactions_failures_use_their_reported_errors() {
    let req = LLMRequest::new(package("google/interactions", "gemini-3.8-flash", json!({}))).prompt("Hi");
    for (errors, tag, message) in [
        (json!([{"code": "permission_denied", "message": "Access denied"}]), "Authentication", "Access denied"),
        (json!([{"message": "Out of credits"}, {"code": "insufficient_quota", "message": "Add billing"}]), "QuotaExceeded", "Out of credits\nAdd billing"),
        (json!([]), "UnknownProvider", "Google Interactions failed"),
    ] {
        let event = json!({"event_type": "interaction.completed", "interaction": {"status": "failed", "errors": errors}});
        let out = decode(&req, &sse(&[event.clone()]));
        let error = out.last().unwrap().as_ref().unwrap_err();
        assert_eq!((error.tag(), error.message.as_str(), error.body.as_deref()), (tag, message, Some(event.to_string().as_str())));
    }
}

// ---------------------------------------------------------------------------------------------
// Bedrock Converse options
// ---------------------------------------------------------------------------------------------

fn bedrock(id: &str, options: Value) -> LanguageModel {
    package("amazon-bedrock", id, {
        let mut s = settings(json!({"apiKey": "test-bearer", "baseURL": "https://bedrock-runtime.test"}));
        s.extend(settings(options));
        Value::Object(s)
    })
}

async fn bedrock_body(model: LanguageModel) -> Value {
    let mut req = LLMRequest::new(model).prompt("Say hello.");
    req.cache = Some(CachePolicy::none());
    req.generation = Some(GenerationOptions { max_tokens: Some(64.0), ..Default::default() });
    compile_body(&req).await.unwrap()
}

/// bedrock-converse-options.test.ts
#[tokio::test]
async fn bedrock_effort_per_model_family() {
    let fields = |id: &str, options: Value| {
        let model = bedrock(id, options);
        async move { bedrock_body(model).await["additionalModelRequestFields"].clone() }
    };
    let binding = json!({"prefix_mismatch_behavior": "drop_block"});
    assert_eq!(fields("us.anthropic.claude-sonnet-4-6", json!({"effort": "high"})).await, json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "high"}}));
    assert_eq!(
        fields("global.anthropic.claude-opus-5-5", json!({"effort": "high"})).await,
        json!({"thinking": {"type": "adaptive", "block_binding": binding}, "output_config": {"effort": "high"}, "anthropic_beta": ["thinking-binding-controls-2026-08-01"]})
    );
    assert_eq!(fields("openai.gpt-oss-120b-1:0", json!({"effort": "low"})).await, json!({"reasoning_effort": "low"}));
    assert_eq!(fields("openai.gpt-5.6", json!({"effort": "low"})).await, json!({"reasoning": {"effort": "low"}}));
    assert_eq!(fields("us.xai.grok-4.6", json!({"effort": "low"})).await, json!({"reasoning": {"effort": "low"}}));
    assert_eq!(fields("us.amazon.nova-2-lite-v1:0", json!({"effort": "medium"})).await, json!({"reasoningConfig": {"type": "enabled", "maxReasoningEffort": "medium"}}));
    for id in ["us.anthropic.claude-opus-4-5-20251101-v1:0", "us.anthropic.claude-3-7-sonnet-20250219-v1:0"] {
        assert_eq!(fields(id, json!({"effort": "high"})).await, json!({"output_config": {"effort": "high"}}));
        assert_eq!(
            fields(id, json!({"effort": "high", "thinking": {"type": "enabled", "budgetTokens": 1024}})).await,
            json!({"thinking": {"type": "enabled", "budget_tokens": 1024}, "output_config": {"effort": "high"}})
        );
    }
    assert_eq!(
        fields("us.anthropic.claude-sonnet-4-6", json!({"effort": "low", "thinking": {"type": "adaptive", "display": "summarized"}})).await,
        json!({"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "low"}})
    );
    assert_eq!(fields("global.anthropic.claude-opus-5-5", json!({"thinking": {"type": "disabled"}})).await, json!({"thinking": {"type": "disabled"}}));
    assert!(bedrock_body(bedrock("us.amazon.nova-2-lite-v1:0", json!({"effort": "high"}))).await.get("inferenceConfig").is_none());
}

/// "sends service tier, guardrail, structured output and request metadata"
#[tokio::test]
async fn bedrock_service_tier_guardrail_output_and_metadata() {
    let body = bedrock_body(bedrock(
        "us.anthropic.claude-sonnet-4-6",
        json!({
            "serviceTier": "priority",
            "guardrail": {"identifier": "abc123", "version": "1", "trace": "enabled", "streamProcessingMode": "async"},
            "outputFormat": {"type": "json_schema", "name": "answer", "schema": {"type": "object", "properties": {"product": {"type": "integer"}}}},
            "requestMetadata": {"suite": "opencode-ai"},
        }),
    ))
    .await;
    assert_eq!(
        body,
        json!({
            "modelId": "us.anthropic.claude-sonnet-4-6",
            "messages": [{"role": "user", "content": [{"text": "Say hello."}]}],
            "inferenceConfig": {"maxTokens": 64},
            "serviceTier": {"type": "priority"},
            "guardrailConfig": {"guardrailIdentifier": "abc123", "guardrailVersion": "1", "trace": "enabled", "streamProcessingMode": "async"},
            "outputConfig": {"textFormat": {"type": "json_schema", "structure": {"jsonSchema": {"schema": "{\"type\":\"object\",\"properties\":{\"product\":{\"type\":\"integer\"}}}", "name": "answer"}}}},
            "requestMetadata": {"suite": "opencode-ai"},
        })
    );
}

/// bedrock-converse-options.recorded.test.ts: request bodies equal the recordings, responses decode.
#[tokio::test]
async fn bedrock_options_recordings() {
    let cases = [
        (
            "sends-claude-effort-with-adaptive-thinking-service-tier-and-request-metadata",
            "us.anthropic.claude-sonnet-4-6",
            json!({"effort": "high", "thinking": {"type": "adaptive", "display": "summarized"}, "serviceTier": "default", "requestMetadata": {"suite": "opencode-ai"}}),
            "How many primes are below 60? Reply with the number only.",
            4000.0,
        ),
        (
            "sends-gpt-oss-reasoning-effort-with-the-priority-tier",
            "openai.gpt-oss-120b-1:0",
            json!({"effort": "low", "serviceTier": "priority", "requestMetadata": {"suite": "opencode-ai"}}),
            "What is 17*23? Reply with the number only.",
            1000.0,
        ),
        ("sends-nova-2-reasoning-effort", "us.amazon.nova-2-lite-v1:0", json!({"effort": "low"}), "What is 17*23? Reply with the number only.", 1000.0),
        (
            "constrains-claude-output-to-a-json-schema",
            "us.anthropic.claude-sonnet-4-6",
            json!({"outputFormat": {"type": "json_schema", "name": "answer", "schema": {"type": "object", "properties": {"product": {"type": "integer"}}, "required": ["product"], "additionalProperties": false}}}),
            "What is 17*23?",
            200.0,
        ),
    ];
    for (name, id, options, prompt, max_tokens) in cases {
        let rec = load(&format!("recorded/bedrock-converse-options/{name}.json"));
        let mut s = settings(json!({"apiKey": "fixture", "region": "us-east-1"}));
        s.extend(settings(options));
        let model = oc_ai::providers::language_model("@opencode/ai/providers/amazon-bedrock", id, &s).unwrap();
        let mut req = LLMRequest::new(model).prompt(prompt);
        req.cache = Some(CachePolicy::none());
        req.generation = Some(GenerationOptions { max_tokens: Some(max_tokens), ..Default::default() });
        let compiled = Client::new().compile(&req).await.unwrap();
        json_eq(&compiled.prepared.body, &rec.interactions[0].request.body);
        assert_eq!(compiled.prepared.url, rec.interactions[0].request.url, "{name}");
        let events = decode_ok(&req, &rec.interactions[0].response.bytes());
        let LLMEvent::Finish { reason, .. } = events.last().unwrap() else { panic!("{name}") };
        assert_eq!(reason.normalized, FinishReason::Stop, "{name}");
    }
}

// ---------------------------------------------------------------------------------------------
// Open Responses routes of OpenRouter, DigitalOcean and Venice
// ---------------------------------------------------------------------------------------------

/// responses-routes.recorded.test.ts
#[tokio::test]
async fn gateway_responses_routes_match_the_recordings() {
    for (dir, package_name, model_id) in [
        ("openrouter-responses", "openrouter/responses", "openai/gpt-5.4-mini"),
        ("digitalocean-responses", "digitalocean/responses", "openai-gpt-5.4-mini"),
        ("venice-responses", "venice/responses", "openai-gpt-54-mini"),
    ] {
        let model = || package(package_name, model_id, json!({"apiKey": "fixture"}));
        // "streams text"
        let rec = load(&format!("recorded/{dir}/streams-text.json"));
        let mut req = LLMRequest::new(model()).prompt("Reply exactly: OK");
        req.generation = Some(GenerationOptions { max_tokens: Some(64.0), ..Default::default() });
        let compiled = Client::new().compile(&req).await.unwrap();
        json_eq(&compiled.prepared.body, &rec.interactions[0].request.body);
        assert_eq!(compiled.prepared.url, rec.interactions[0].request.url);
        assert_eq!(compiled.prepared.headers.get("authorization").map(String::as_str), Some("Bearer fixture"));
        let events = decode_ok(&req, &rec.interactions[0].response.bytes());
        assert_eq!(text_of(&events).trim().trim_end_matches('.'), "OK", "{dir}");
        assert!(usage_of(&events).output_tokens.unwrap_or(0) > 0);

        // "continues a tool call"
        let rec = load(&format!("recorded/{dir}/continues-a-tool-call.json"));
        let mut req = tool_loop_request(model(), LOOP_SYSTEM, 256.0, None);
        req.cache = None;
        let events = run_loop(req, &rec).await;
        assert_eq!(finish_reasons(&events), vec![FinishReason::ToolCalls, FinishReason::Stop], "{dir}");

        // "calls a freeform text tool": freeform tools are library-only; the stream still decodes to its end.
        let rec = load(&format!("recorded/{dir}/calls-a-freeform-text-tool.json"));
        let out = decode(&LLMRequest::new(model()), &rec.interactions[0].response.bytes());
        assert!(matches!(out.last(), Some(Ok(LLMEvent::Finish { .. }))), "{dir}: {:?}", types(&out.iter().filter_map(|e| e.as_ref().ok().cloned()).collect::<Vec<_>>()));
    }
}

#[tokio::test]
async fn gateway_responses_settings_override_the_defaults() {
    let model = package("venice/responses", "m", json!({"store": true, "queryParams": {"a": "b"}, "body": {"x": 1}}));
    let req = LLMRequest::new(model).prompt("hi");
    let compiled = Client::new().compile(&req).await.unwrap();
    assert_eq!(compiled.prepared.url, "https://api.venice.ai/api/v1/responses?a=b");
    let body: Value = serde_json::from_str(&compile_text(&req).await.unwrap()).unwrap();
    assert_eq!((&body["store"], &body["include"], &body["x"]), (&json!(true), &json!(["reasoning.encrypted_content"]), &json!(1)));
}

// ---------------------------------------------------------------------------------------------
// Cross-check with upstream: the new 2.2 recordings decode to the same events
// ---------------------------------------------------------------------------------------------

/// Numbers compared as f64; `undefined`-valued keys are absent on both sides after JSON encoding.
fn numbers(value: Value) -> Value {
    match value {
        Value::Number(n) => json!(n.as_f64().unwrap()),
        Value::Array(items) => Value::Array(items.into_iter().map(numbers).collect()),
        Value::Object(o) => Value::Object(o.into_iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k, numbers(v))).collect()),
        other => other,
    }
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

/// Decodes every recorded success response of the corpus (`tests/fixtures/recorded`, upstream's recordings) with
/// upstream's own route (Bun on the 2.2.2 checkout, `packages/ai`) and with ocd, and compares the events. Needs
/// `OCD_SCRATCH` and `OCD_BUN_PATH`; skipped otherwise.
#[test]
fn recordings_decode_like_upstream() {
    let (Ok(scratch), Ok(bun)) = (std::env::var("OCD_SCRATCH"), std::env::var("OCD_BUN_PATH")) else { return };
    let ai = std::path::PathBuf::from(scratch).join("src/opencode-2.2.2/packages/ai");
    if !ai.join("src/providers/openrouter/responses.ts").is_file() {
        return;
    }
    let root = format!("{}/tests/fixtures/recorded", env!("CARGO_MANIFEST_DIR"));
    let mut jobs = vec![];
    let mut ours = vec![];
    let mut names = vec![];
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
                let bytes = interaction.response.bytes();
                jobs.push(json!({
                    "package": package_name,
                    "model": id,
                    "settings": settings,
                    "body": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes),
                    "contentType": interaction.response.headers.get("content-type").cloned().unwrap_or(json!("text/event-stream")),
                }));
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("decode.ts");
    std::fs::write(
        &script,
        format!(
            r#"import {{ Effect, Stream }} from "effect"
import {{ readFileSync, writeFileSync }} from "node:fs"
const ai = {ai:?}
const {{ LLM }} = await import(ai + "/src/effect.ts")
const {{ LLMClient }} = await import(ai + "/src/route.ts")
const {{ fixedResponse }} = await import(ai + "/test/lib/http.ts")
const jobs = JSON.parse(readFileSync(process.argv[2], "utf8"))
const out = []
for (const job of jobs) {{
  const pkg = await import(ai + "/src/providers/" + job.package + ".ts")
  const model = pkg.model(job.model, job.settings)
  const body = Buffer.from(job.body, "base64")
  const events = []
  const exit = await Effect.runPromiseExit(
    LLMClient.stream(LLM.request({{ model, prompt: "x" }})).pipe(
      Stream.runForEach((event) => Effect.sync(() => events.push(event))),
      Effect.provide(fixedResponse(body, {{ headers: {{ "content-type": job.contentType }} }})),
    ),
  )
  if (exit._tag === "Failure") {{
    const error = exit.cause.reasons?.[0]?.error
    events.push({{ error: error?.reason?._tag ?? "Defect", message: error?.reason?.message ?? String(exit.cause) }})
  }}
  out.push(JSON.parse(JSON.stringify(events)))
}}
writeFileSync(process.argv[3], JSON.stringify(out))
"#,
            ai = ai.display().to_string()
        ),
    )
    .unwrap();
    let input = dir.path().join("jobs.json");
    std::fs::write(&input, serde_json::to_string(&jobs).unwrap()).unwrap();
    let output = std::process::Command::new(bun)
        .arg("run")
        .arg(&script)
        .arg(&input)
        .arg(dir.path().join("out.json"))
        .current_dir(&ai)
        .output()
        .unwrap();
    assert!(output.status.success(), "bun failed: {}", String::from_utf8_lossy(&output.stderr));
    let theirs: Vec<Value> = serde_json::from_slice(&std::fs::read(dir.path().join("out.json")).unwrap()).unwrap();
    assert_eq!(theirs.len(), ours.len());
    let differing: Vec<String> = theirs
        .into_iter()
        .zip(ours.iter())
        .enumerate()
        .filter(|(_, (theirs, ours))| numbers(theirs.clone()) != **ours)
        .map(|(i, (theirs, ours))| format!("{}\n  upstream: {}\n  ocd:      {}", names[i], numbers(theirs), ours))
        .collect();
    eprintln!("compared {} recorded responses with upstream", ours.len());
    assert!(differing.is_empty(), "{} differ:\n{}", differing.len(), differing.join("\n"));
}

/// The request one of the cross-check cases sends: `{prompt, system?, providerOptions?, generation?, tools?,
/// toolChoice?, cache?}` (tools are `{name, description, inputSchema}`).
fn case_request(model: LanguageModel, spec: &Value) -> LLMRequest {
    let mut req = LLMRequest::new(model);
    if let Some(system) = spec.get("system").and_then(Value::as_str) {
        req = req.system(system);
    }
    req = req.prompt(spec["prompt"].as_str().unwrap());
    req.provider_options = spec.get("providerOptions").and_then(Value::as_object).cloned();
    req.generation = spec.get("generation").map(|g| serde_json::from_value(g.clone()).unwrap());
    req.tool_choice = spec.get("toolChoice").map(|c| serde_json::from_value(c.clone()).unwrap());
    req.cache = spec.get("cache").map(|c| serde_json::from_value(c.clone()).unwrap());
    for tool in spec.get("tools").and_then(Value::as_array).into_iter().flatten() {
        req = req.tool(ToolDefinition::new(tool["name"].as_str().unwrap(), tool["description"].as_str().unwrap(), tool["inputSchema"].clone()));
    }
    req
}

/// Compiles the 2.2 request options with upstream's routes (Bun on the 2.2.2 checkout, capturing the HTTP request)
/// and with ocd, and compares the URL, the body text (key order included) and the headers that carry protocol
/// state. Needs `OCD_SCRATCH` and `OCD_BUN_PATH`; skipped otherwise.
#[tokio::test]
async fn request_bodies_match_upstream() {
    let (Ok(scratch), Ok(bun)) = (std::env::var("OCD_SCRATCH"), std::env::var("OCD_BUN_PATH")) else { return };
    let ai = std::path::PathBuf::from(scratch).join("src/opencode-2.2.2/packages/ai");
    if !ai.join("src/providers/openrouter/responses.ts").is_file() {
        return;
    }
    let lookup = json!({"name": "lookup", "description": "Lookup data", "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}}});
    let base = json!({"apiKey": "fixture", "region": "us-east-1"});
    let bedrock = |id: &str, options: Value| {
        let mut s = settings(base.clone());
        s.extend(settings(options));
        json!({"package": "amazon-bedrock", "model": id, "settings": s, "request": {"prompt": "Say hello.", "cache": "none", "generation": {"maxTokens": 64}}})
    };
    let cases = vec![
        json!({"package": "openai/chat", "model": "gpt-5-nano", "settings": base, "request": {"prompt": "Hello", "tools": [lookup],
            "providerOptions": {"store": true, "serviceTier": "flex", "textVerbosity": "low", "parallelToolCalls": false, "metadata": {"source": "test"}, "safetyIdentifier": "user-hash"}}}),
        json!({"package": "openai/chat", "model": "gpt-5-nano", "settings": base, "request": {"prompt": "Hello", "providerOptions": {"parallelToolCalls": true, "textVerbosity": ""}}}),
        json!({"package": "groq", "model": "qwen/qwen3.6-27b", "settings": {"apiKey": "fixture", "reasoningEffort": "default", "parallelToolCalls": true, "serviceTier": "flex", "user": "test-user"},
            "request": {"prompt": "Hello", "tools": [lookup], "providerOptions": {"parallelToolCalls": false, "includeReasoning": false}}}),
        json!({"package": "groq", "model": "openai/gpt-oss-20b", "settings": base, "request": {"prompt": "Hello", "tools": [lookup], "toolChoice": {"type": "auto", "disableParallelToolUse": true}}}),
        json!({"package": "anthropic", "model": "claude-sonnet-4-5", "settings": base, "request": {"prompt": "hi", "providerOptions": {"speed": "fast"}}}),
        json!({"package": "openai", "model": "gpt-5.6", "settings": base, "request": {"prompt": "hi", "providerOptions": {"reasoningEffort": "high", "reasoningMode": "pro", "reasoningContext": "current_turn"}}}),
        json!({"package": "openai", "model": "o3", "settings": base, "request": {"prompt": "hi", "providerOptions": {"reasoningContext": "all_turns"}, "cache": {"ttl": "30m"}}}),
        json!({"package": "xai", "model": "grok-4", "settings": base, "request": {"prompt": "Hello", "generation": {"topK": 20, "presencePenalty": 0.5, "frequencyPenalty": 0.5, "maxTokens": 10}, "providerOptions": {"minP": 0.1}}}),
        json!({"package": "openrouter/responses", "model": "openai/gpt-5.4-mini", "settings": {"apiKey": "fixture", "store": true, "body": {"x": 1}}, "request": {"prompt": "Reply exactly: OK", "tools": [lookup]}}),
        json!({"package": "digitalocean/responses", "model": "openai-gpt-5.4-mini", "settings": base, "request": {"prompt": "hi", "generation": {"maxTokens": 64}}}),
        json!({"package": "venice/responses", "model": "openai-gpt-54-mini", "settings": {"apiKey": "fixture", "queryParams": {"a": "b"}}, "request": {"prompt": "hi"}}),
        json!({"package": "moonshot/chat", "model": "kimi-k2.6", "settings": base, "request": {"prompt": "hi", "tools": [{"name": "tuple", "description": "Tuple", "inputSchema": {
            "type": "object",
            "properties": {
                "pair": {"type": "array", "prefixItems": [{"type": "string"}], "items": {"type": "number"}},
                "items": {"type": "array", "items": {"type": "string"}},
                "cart": {"type": "object", "default": {"items": ["a"]}},
                "mode": {"enum": ["a", null]},
            },
        }}]}}),
        bedrock("us.anthropic.claude-sonnet-4-6", json!({"effort": "high"})),
        bedrock("global.anthropic.claude-opus-5-5", json!({"effort": "high"})),
        bedrock("openai.gpt-oss-120b-1:0", json!({"effort": "low", "serviceTier": "priority"})),
        bedrock("openai.gpt-5.6", json!({"effort": "low"})),
        bedrock("us.xai.grok-4.6", json!({"effort": "low"})),
        bedrock("us.amazon.nova-2-lite-v1:0", json!({"effort": "high"})),
        bedrock("us.anthropic.claude-opus-4-5-20251101-v1:0", json!({"effort": "high", "thinking": {"type": "enabled", "budgetTokens": 1024}})),
        bedrock("global.anthropic.claude-opus-5-5", json!({"thinking": {"type": "disabled"}})),
        bedrock("us.anthropic.claude-sonnet-4-6", json!({"thinking": {"type": "adaptive", "display": "omitted", "extra": 1}, "unknownOption": true})),
        bedrock("us.anthropic.claude-sonnet-4-6", json!({
            "serviceTier": "priority",
            "guardrail": {"identifier": "abc123", "version": "1", "trace": "enabled", "streamProcessingMode": "async"},
            "outputFormat": {"type": "json_schema", "description": "d", "name": "answer", "schema": {"type": "object"}},
            "requestMetadata": {"suite": "opencode-ai"},
            "body": {"additionalModelRequestFields": {"output_config": {"effort": "max"}}, "serviceTier": {"type": "flex"}},
        })),
    ];
    let mut ours = vec![];
    for case in &cases {
        let package_name = case["package"].as_str().unwrap();
        let model = oc_ai::providers::language_model(&format!("@opencode/ai/providers/{package_name}"), case["model"].as_str().unwrap(), case["settings"].as_object().unwrap()).unwrap();
        let req = case_request(model, &case["request"]);
        ours.push(match Client::new().compile(&req).await {
            Ok(compiled) => {
                let headers: serde_json::Map<String, Value> = ["anthropic-beta", "authorization", "x-api-key", "content-type"]
                    .iter()
                    .filter_map(|h| compiled.prepared.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(h)).map(|(_, v)| (h.to_string(), json!(v))))
                    .collect();
                json!({"url": compiled.prepared.url, "body": compiled.prepared.body, "headers": headers})
            }
            Err(error) => json!({"error": error.message}),
        });
    }
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("compile.ts");
    std::fs::write(
        &script,
        format!(
            r#"import {{ Effect }} from "effect"
import {{ HttpClientRequest }} from "effect/http"
import {{ readFileSync, writeFileSync }} from "node:fs"
const ai = {ai:?}
const {{ LLM, ToolDefinition }} = await import(ai + "/src/effect.ts")
const {{ LLMClient }} = await import(ai + "/src/route.ts")
const {{ dynamicResponse }} = await import(ai + "/test/lib/http.ts")
const cases = JSON.parse(readFileSync(process.argv[2], "utf8"))
const out = []
for (const item of cases) {{
  const pkg = await import(ai + "/src/providers/" + item.package + ".ts")
  const model = pkg.model(item.model, item.settings)
  const {{ tools, ...rest }} = item.request
  const request = LLM.request({{ model, ...rest, ...(tools ? {{ tools: tools.map((t) => ToolDefinition.make(t)) }} : {{}}) }})
  let sent
  const exit = await Effect.runPromiseExit(
    LLMClient.generate(request).pipe(
      Effect.provide(
        dynamicResponse((input) =>
          Effect.gen(function* () {{
            const web = yield* HttpClientRequest.toWeb(input.request).pipe(Effect.orDie)
            const headers = {{}}
            for (const name of ["anthropic-beta", "authorization", "x-api-key", "content-type"]) {{
              const value = web.headers.get(name)
              if (value !== null) headers[name] = value
            }}
            sent = {{ url: web.url, body: input.text, headers }}
            return input.respond("{{}}", {{ status: 400 }})
          }}),
        ),
      ),
    ),
  )
  if (sent) out.push(sent)
  else out.push({{ error: exit.cause?.reasons?.[0]?.error?.reason?.message ?? String(exit.cause) }})
}}
writeFileSync(process.argv[3], JSON.stringify(out))
"#,
            ai = ai.display().to_string()
        ),
    )
    .unwrap();
    let input = dir.path().join("cases.json");
    std::fs::write(&input, serde_json::to_string(&cases).unwrap()).unwrap();
    let output = std::process::Command::new(bun)
        .arg("run")
        .arg(&script)
        .arg(&input)
        .arg(dir.path().join("out.json"))
        .current_dir(&ai)
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .unwrap();
    assert!(output.status.success(), "bun failed: {}", String::from_utf8_lossy(&output.stderr));
    let theirs: Vec<Value> = serde_json::from_slice(&std::fs::read(dir.path().join("out.json")).unwrap()).unwrap();
    let differing: Vec<String> = theirs
        .iter()
        .zip(ours.iter())
        .enumerate()
        .filter(|(_, (theirs, ours))| theirs != ours)
        .map(|(i, (theirs, ours))| format!("case {i} ({} {})\n  upstream: {theirs}\n  ocd:      {ours}", cases[i]["package"], cases[i]["model"]))
        .collect();
    eprintln!("compared {} requests with upstream", theirs.len());
    assert!(differing.is_empty(), "{} of {} differ:\n{}", differing.len(), cases.len(), differing.join("\n"));
}
