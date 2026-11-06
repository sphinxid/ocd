//! Ports of the protocol tests opencode 2.2.5 changed (packages/ai/test): unsigned and interrupted reasoning is no
//! longer demoted to text (#60498), LiteLLM overflow errors (#60692), Responses `file_url` without `filename`
//! (#60382), OpenRouter options on the Responses route (#60044) and the xAI in-band compaction text.

use oc_ai::protocols::{anthropic_messages as am, bedrock_converse as bc, google_interactions as gi, mistral_chat as mc};
use oc_ai::route::auth;
use oc_ai::testing::compile_body;
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

fn request(model: LanguageModel, msgs: Value) -> LLMRequest {
    let mut req = LLMRequest::new(model);
    req.messages = serde_json::from_value(msgs).expect("messages");
    req
}

fn unsigned_reasoning() -> Value {
    json!([{"role": "assistant", "content": [{"type": "reasoning", "text": "unsigned reasoning"}]}])
}

/// anthropic-messages.test.ts "drops unsigned reasoning when signatures are required"
#[tokio::test]
async fn anthropic_drops_unsigned_reasoning_when_signatures_are_required() {
    let model = am::route().auth(auth::none()).model("claude-sonnet-4-5").unwrap();
    let body = compile_body(&request(model, unsigned_reasoning())).await.unwrap();
    assert_eq!(body["messages"], json!([]));

    // An explicit `requireSignature: true` wins over the provider heuristics.
    let mut model = am::route().provider("kimi-for-coding").auth(auth::none()).model("k3").unwrap();
    model = model.with_compatibility(serde_json::from_value(json!({"requireSignature": true})).unwrap());
    let body = compile_body(&request(model, unsigned_reasoning())).await.unwrap();
    assert_eq!(body["messages"], json!([]));
}

/// anthropic-messages.test.ts "keeps unsigned reasoning as thinking where the endpoint accepts it", with the
/// Qwen and MiniMax models #60498 added.
#[tokio::test]
async fn anthropic_keeps_unsigned_thinking_for_endpoints_without_signatures() {
    let thinking = json!([{"role": "assistant", "content": [{"type": "thinking", "thinking": "unsigned reasoning", "signature": ""}]}]);
    for (provider, id) in [("kimi-for-coding", "k3"), ("opencode", "qwen3.8-flash"), ("opencode-go", "minimax-m3"), ("opencode", "MiniMax-M3")] {
        let model = am::route().provider(provider).auth(auth::none()).model(id).unwrap();
        let body = compile_body(&request(model, unsigned_reasoning())).await.unwrap();
        assert_eq!(body["messages"], thinking, "{provider}/{id}");
    }
    // Blank unsigned reasoning is dropped even where unsigned thinking is accepted.
    let model = am::route().provider("opencode").auth(auth::none()).model("qwen3.8-flash").unwrap();
    let blank = json!([{"role": "assistant", "content": [{"type": "reasoning", "text": " \n"}]}]);
    assert_eq!(compile_body(&request(model, blank)).await.unwrap()["messages"], json!([]));
}

/// MiniMax's Messages route accepts unsigned thinking; Meta's no longer opts out of signatures.
#[tokio::test]
async fn minimax_messages_accept_unsigned_thinking_and_meta_messages_do_not() {
    let body = compile_body(&request(package("minimax", "abab-7", json!({})), unsigned_reasoning())).await.unwrap();
    assert_eq!(body["messages"], json!([{"role": "assistant", "content": [{"type": "thinking", "thinking": "unsigned reasoning", "signature": ""}]}]));
    let body = compile_body(&request(package("meta/messages", "llama-5", json!({})), unsigned_reasoning())).await.unwrap();
    assert_eq!(body["messages"], json!([]));
}

fn bedrock(cache: bool) -> LLMRequest {
    let model = bc::route()
        .provider("amazon-bedrock")
        .metadata_key("bedrock")
        .base_url(Some(bc::bedrock_base_url("us-east-1")))
        .auth(auth::none())
        .model("us.anthropic.claude-sonnet-4-5-v1:0")
        .unwrap();
    let mut req = request(
        model,
        json!([
            {"role": "user", "content": [{"type": "text", "text": "Think"}]},
            {"role": "assistant", "content": [{"type": "reasoning", "text": "Partial thought"}]},
            {"role": "user", "content": [{"type": "text", "text": "Continue"}]},
        ]),
    );
    req.cache = Some(if cache { CachePolicy::auto() } else { CachePolicy::none() });
    req
}

/// bedrock-converse.test.ts "drops unsigned reasoning"
#[tokio::test]
async fn bedrock_drops_unsigned_reasoning() {
    let body = compile_body(&bedrock(false)).await.unwrap();
    assert_eq!(body["messages"], json!([{"role": "user", "content": [{"text": "Think"}, {"text": "Continue"}]}]));
    let body = compile_body(&bedrock(true)).await.unwrap();
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [{"text": "Think"}, {"text": "Continue"}, {"cachePoint": {"type": "default"}}]}])
    );
}

/// google-vertex-interactions.test.ts: an unsigned function call before the first signed step gets the sentinel.
#[tokio::test]
async fn interactions_sign_unsigned_calls_before_the_first_signed_step() {
    let model = gi::route().auth(auth::none()).model("gemini-3-flash-preview").unwrap();
    let key = "google".to_string();
    let call = |id: &str, city: &str, sig: Option<&str>| {
        let mut c = json!({"type": "tool-call", "id": id, "name": "lookup", "input": {"city": city}});
        if let Some(s) = sig {
            c["providerMetadata"] = json!({ key.clone(): {"interactionSignature": s} });
        }
        c
    };
    let result = |id: &str, weather: &str| {
        json!({"role": "tool", "content": [{"type": "tool-result", "id": id, "name": "lookup", "result": {"type": "json", "value": {"weather": weather}}}]})
    };
    let body = compile_body(&request(
        model.clone(),
        json!([
            {"role": "assistant", "content": [call("call_foreign", "Paris", None)]},
            result("call_foreign", "sunny"),
            {"role": "assistant", "content": [call("call_signed", "Lyon", Some("sig_1")), call("call_sibling", "Nice", None)]},
            result("call_signed", "cloudy"),
            result("call_sibling", "warm"),
        ]),
    ))
    .await
    .unwrap();
    assert_eq!(
        body["input"],
        json!([
            {"type": "function_call", "id": "call_foreign", "name": "lookup", "arguments": {"city": "Paris"}, "signature": "skip_thought_signature_validator"},
            {"type": "function_result", "call_id": "call_foreign", "name": "lookup", "result": {"weather": "sunny"}},
            {"type": "function_call", "id": "call_signed", "name": "lookup", "arguments": {"city": "Lyon"}, "signature": "sig_1"},
            {"type": "function_call", "id": "call_sibling", "name": "lookup", "arguments": {"city": "Nice"}},
            {"type": "function_result", "call_id": "call_signed", "name": "lookup", "result": {"weather": "cloudy"}},
            {"type": "function_result", "call_id": "call_sibling", "name": "lookup", "result": {"weather": "warm"}},
        ])
    );
}

fn mistral() -> LanguageModel {
    mc::route().auth(auth::none()).model("mistral-small-latest").unwrap()
}

/// mistral-chat.test.ts: reasoning without native Mistral thinking is not replayed, and `prefix` marks the trailing
/// assistant message that is actually sent.
#[tokio::test]
async fn mistral_drops_foreign_reasoning_and_prefixes_the_trailing_assistant() {
    let body = compile_body(&request(
        mistral(),
        json!([
            {"role": "user", "content": [{"type": "text", "text": "Continue"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "Prefix"}]},
            {"role": "assistant", "content": [{"type": "reasoning", "text": "Dropped unsigned reasoning"}]},
        ]),
    ))
    .await
    .unwrap();
    assert_eq!(body["messages"], json!([{"role": "user", "content": "Continue"}, {"role": "assistant", "content": "Prefix", "prefix": true}]));

    let body = compile_body(&request(
        mistral(),
        json!([
            {"role": "user", "content": [{"type": "text", "text": "Go"}]},
            {"role": "assistant", "content": [{"type": "reasoning", "text": "Think"}, {"type": "text", "text": "Calling"}]},
        ]),
    ))
    .await
    .unwrap();
    assert_eq!(body["messages"][1], json!({"role": "assistant", "content": "Calling", "prefix": true}));

    // Whitespace text and foreign reasoning alone leave nothing to send; a trailing tool call is never a prefix.
    let body = compile_body(&request(
        mistral(),
        json!([
            {"role": "assistant", "content": [{"type": "text", "text": " \n "}]},
            {"role": "assistant", "content": [{"type": "reasoning", "text": "\t"}]},
            {"role": "assistant", "content": [{"type": "reasoning", "text": "Foreign reasoning without mistral metadata"}]},
            {"role": "assistant", "content": [{"type": "tool-call", "id": "Ab12Cd34E", "name": "lookup", "input": {}}]},
        ]),
    ))
    .await
    .unwrap();
    assert_eq!(
        body["messages"][0],
        json!({"role": "assistant", "content": "", "tool_calls": [{"id": "Ab12Cd34E", "type": "function", "function": {"name": "lookup", "arguments": "{}"}}]})
    );
}

/// provider-error.test.ts: LiteLLM's ContextWindowExceededError is a context overflow (#60692).
#[test]
fn litellm_context_window_errors_are_overflows() {
    assert!(oc_ai::error::is_context_overflow(
        "litellm.ContextWindowExceededError: litellm.BadRequestError: litellm._pre_call_checks: Context Window exceeded for given call. No models have context window large enough for this call."
    ));
}

/// openai-responses.test.ts: a file given by URL is sent as `file_url` without a `filename` (#60382).
#[tokio::test]
async fn responses_send_file_url_without_filename() {
    let msgs = json!([{"role": "user", "content": [
        {"type": "media", "media": {"source": {"type": "url", "url": "https://example.com/image.png", "mediaType": "image/png"}}},
        {"type": "media", "media": {"source": {"type": "url", "url": "https://example.com/report.pdf", "mediaType": "application/pdf"}}, "filename": "report.pdf"},
        {"type": "media", "media": {"source": {"type": "base64", "data": "cGRm", "mediaType": "application/pdf"}}, "filename": "inline.pdf"},
    ]}]);
    let body = compile_body(&request(package("openai/responses", "gpt-5.4", json!({})), msgs)).await.unwrap();
    assert_eq!(
        body["input"][0]["content"],
        json!([
            {"type": "input_image", "image_url": "https://example.com/image.png"},
            {"type": "input_file", "file_url": "https://example.com/report.pdf"},
            {"type": "input_file", "filename": "inline.pdf", "file_data": "data:application/pdf;base64,cGRm"},
        ])
    );
}

/// openrouter.test.ts "sends OpenRouter routing options on the Responses route" (#60044).
#[tokio::test]
async fn openrouter_responses_send_routing_options() {
    let model = package(
        "openrouter/responses",
        "openai/gpt-5.4",
        json!({
            "user": "user_123",
            "debug": {"echo_upstream_body": true},
            "plugins": [{"id": "response-healing"}],
            "provider": {"only": ["openai"], "allow_fallbacks": false},
            "models": ["openai/gpt-5.4-mini"],
            "reasoningEffort": "low",
            "usage": true,
            "web_search_options": {"max_results": 2},
        }),
    );
    let compiled = Client::new().compile(&LLMRequest::new(model).prompt("Hello")).await.unwrap();
    assert_eq!(compiled.prepared.url, "https://openrouter.ai/api/v1/responses");
    let body: Value = serde_json::from_str(&compiled.prepared.body).unwrap();
    assert_eq!(body["model"], "openai/gpt-5.4");
    assert_eq!(body["store"], false);
    assert_eq!(body["reasoning"]["effort"], "low");
    // The shared options follow the Open Responses fields, in OpenRouter's order; Chat-only options stay out.
    let keys: Vec<&str> = body.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(&keys[keys.len() - 5..], ["models", "provider", "plugins", "debug", "user"]);
    assert!(body.get("usage").is_none() && body.get("web_search_options").is_none());

    // Malformed values are dropped leniently.
    let model = package("openrouter/responses", "openai/gpt-5.4", json!({"models": "openai/gpt-5.4-mini", "user": 1, "plugins": [1]}));
    let body = compile_body(&LLMRequest::new(model).prompt("Hello")).await.unwrap();
    assert!(body.get("models").is_none() && body.get("user").is_none() && body.get("plugins").is_none());
}

/// xai-responses.test.ts: the in-band compaction error names `ai.compact`.
#[tokio::test]
async fn xai_in_band_compaction_names_ai_compact() {
    let model = package("xai", "grok-4", json!({"contextManagement": {"compactThreshold": 1000}}));
    let error = compile_body(&LLMRequest::new(model).prompt("hi")).await.unwrap_err();
    assert_eq!(error.message, "xAI requires explicit compaction through ai.compact; automatic context management is not supported");
}
