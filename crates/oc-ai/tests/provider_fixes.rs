//! Request and stream fixes from upstream 2.1 (`packages/ai/test`): `store` only for OpenAI and
//! Azure, `compatibility.streamUsage` / `cacheFormat` / `supportsPromptCacheKey`, OpenRouter option
//! precedence, cache breakpoints that skip non-cacheable parts, tolerant stream decoding and the
//! Bedrock exception status.

mod common;

use common::*;
use oc_ai::compile::apply_cache_policy;
use oc_ai::protocols::open_responses_channel::decode_channel_event;
use oc_ai::protocols::{
    anthropic_messages as am, bedrock_converse as bc, cohere_chat as cc, google_interactions as gi,
    mistral_chat as mc, openai_chat as oc, openai_responses as oai,
};
use oc_ai::route::auth;
use oc_ai::testing::{compile_body, decode, decode_ok, recording};
use oc_ai::*;
use serde_json::{Value, json};

fn package(package: &str, id: &str, settings: Value) -> LanguageModel {
    let settings = settings.as_object().cloned().unwrap_or_default();
    oc_ai::providers::language_model(&format!("@opencode/ai/providers/{package}"), id, &settings)
        .unwrap()
}

fn compatible(base: &str, id: &str) -> LanguageModel {
    oc::compatible_route()
        .provider("openai-compatible")
        .base_url(Some(base.into()))
        .auth(auth::none())
        .model(id)
        .unwrap()
}

fn compat(model: LanguageModel, compatibility: Value) -> LanguageModel {
    model.with_compatibility(serde_json::from_value(compatibility).unwrap())
}

fn sse(values: &[Value]) -> Vec<u8> {
    values
        .iter()
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>()
        .into_bytes()
}

fn text_of_events(events: &[LLMEvent]) -> String {
    text_of(events)
}

// ---------------------------------------------------------------------------------------------
// OpenAI Chat: store and stream usage
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn omits_stream_usage_options_when_compatibility_disables_them() {
    let model = compat(
        compatible("https://api.compatible.test/v1", "m"),
        json!({"streamUsage": false}),
    );
    let body = compile_body(&LLMRequest::new(model).prompt("Hello"))
        .await
        .unwrap();
    assert!(body.get("stream_options").is_none());
    let body = compile_body(
        &LLMRequest::new(compatible("https://api.compatible.test/v1", "m")).prompt("Hello"),
    )
    .await
    .unwrap();
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
}

#[tokio::test]
async fn sends_store_only_to_openai_and_azure_unless_compatibility_opts_in() {
    let omitted = LLMRequest::new(compatible(
        "https://api.compatible.test/v1",
        "compatible-model",
    ))
    .prompt("Hello")
    .provider_option("store", json!(true));
    assert!(compile_body(&omitted).await.unwrap().get("store").is_none());

    let opted_in = compat(
        compatible("https://api.compatible.test/v1", "compatible-model"),
        json!({"supportsStore": true}),
    );
    assert_eq!(
        compile_body(&LLMRequest::new(opted_in).prompt("Hello"))
            .await
            .unwrap()["store"],
        false
    );

    let openai_host = compatible("https://api.openai.com/v1", "gpt-4o-mini");
    assert_eq!(
        compile_body(&LLMRequest::new(openai_host).prompt("Hello"))
            .await
            .unwrap()["store"],
        false
    );

    let azure = package(
        "azure/chat",
        "gpt-4o-mini",
        json!({"baseURL": "https://opencode-test.openai.azure.com/openai/", "apiKey": "test"}),
    );
    assert_eq!(
        compile_body(&LLMRequest::new(azure).prompt("Hello"))
            .await
            .unwrap()["store"],
        false
    );

    for (p, id) in [
        ("deepseek", "deepseek-chat"),
        ("togetherai", "m"),
        ("cerebras", "m"),
        ("groq", "m"),
        ("deepinfra", "m"),
    ] {
        let model = package(p, id, json!({"apiKey": "test"}));
        assert!(
            model
                .compatibility
                .as_ref()
                .and_then(|c| c.supports_store)
                .is_none(),
            "{p}"
        );
        assert!(
            compile_body(&LLMRequest::new(model).prompt("Hello"))
                .await
                .unwrap()
                .get("store")
                .is_none(),
            "{p}"
        );
    }
}

/// The recorded DigitalOcean and OpenRouter requests carry no `store` (re-recorded upstream).
#[test]
fn recorded_compatible_requests_carry_no_store() {
    let root = format!("{}/tests/fixtures/recorded", env!("CARGO_MANIFEST_DIR"));
    let mut files: Vec<_> = std::fs::read_dir(format!("{root}/digitalocean-chat"))
        .unwrap()
        .flatten()
        .map(|f| f.path())
        .collect();
    files.push(format!("{root}/openrouter-reasoning.json").into());
    files.push(format!("{root}/openrouter-reasoning-tool-loop.json").into());
    for file in files {
        let rec = recording(&std::fs::read_to_string(&file).unwrap());
        for interaction in &rec.interactions {
            let body: Value = serde_json::from_str(&interaction.request.body).unwrap();
            assert!(body.get("store").is_none(), "{}", file.display());
        }
    }
}

#[tokio::test]
async fn openrouter_request_matches_the_recording() {
    let rec = load_recorded("openrouter-reasoning.json");
    let model = package(
        "openrouter",
        "anthropic/claude-sonnet-4.6",
        json!({"apiKey": "fixture"}),
    )
    .with_compatibility(Compatibility {
        reasoning_field: Some("reasoning".into()),
        ..Default::default()
    });
    let mut req = LLMRequest::new(model)
        .system("Think through the arithmetic, then reply with only the final integer.")
        .prompt("What is 173 multiplied by 219?")
        .provider_option("reasoning", json!({"max_tokens": 1024}));
    req.generation = Some(GenerationOptions {
        max_tokens: Some(1536.0),
        temperature: Some(0.0),
        ..Default::default()
    });
    let compiled = Client::new().compile(&req).await.unwrap();
    json_eq(&compiled.prepared.body, &rec.interactions[0].request.body);
    assert_eq!(compiled.prepared.url, rec.interactions[0].request.url);
}

fn load_recorded(name: &str) -> oc_ai::testing::Recording {
    recording(
        &std::fs::read_to_string(format!(
            "{}/tests/fixtures/recorded/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap(),
    )
}

// ---------------------------------------------------------------------------------------------
// Cache format and breakpoints
// ---------------------------------------------------------------------------------------------

fn cache_format_request(format: Option<&str>) -> LLMRequest {
    let base = oc::compatible_route()
        .provider("custom")
        .base_url(Some("https://api.custom.test/v1".into()))
        .auth(auth::none());
    let mut model = base.model("claude-sonnet-5").unwrap();
    if let Some(format) = format {
        model = model.with_compatibility(Compatibility {
            cache_format: Some(format.into()),
            ..Default::default()
        });
    }
    LLMRequest::new(model)
        .system("You are concise.")
        .prompt("Say hello.")
        .tool(ToolDefinition::new("lookup", "Lookup data", json!({})))
}

#[tokio::test]
async fn places_cache_breakpoints_only_when_compatibility_declares_the_format() {
    let declared = compile_body(&cache_format_request(Some("anthropic")))
        .await
        .unwrap();
    assert_eq!(
        declared["messages"],
        json!([
            {"role": "system", "content": [{"type": "text", "text": "You are concise.", "cache_control": {"type": "ephemeral"}}]},
            {"role": "user", "content": [{"type": "text", "text": "Say hello.", "cache_control": {"type": "ephemeral"}}]}
        ])
    );
    assert_eq!(
        declared["tools"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert!(!declared.to_string().contains("\"ttl\""));

    let alibaba = compile_body(&cache_format_request(Some("alibaba")))
        .await
        .unwrap();
    assert_eq!(alibaba["messages"], declared["messages"]);
    assert!(alibaba["tools"][0].get("cache_control").is_none());

    let undeclared = compile_body(&cache_format_request(None)).await.unwrap();
    assert!(!undeclared.to_string().contains("cache_control"));
}

#[tokio::test]
async fn openrouter_places_breakpoints_for_any_upstream_when_the_format_is_declared() {
    let model = package(
        "openrouter",
        "vendor/claude-proxy",
        json!({"apiKey": "test-key"}),
    )
    .with_compatibility(Compatibility {
        cache_format: Some("anthropic".into()),
        ..Default::default()
    });
    let req = LLMRequest::new(model)
        .system("Base agent")
        .tool(ToolDefinition::new(
            "lookup",
            "Lookup",
            json!({"type": "object", "properties": {}}),
        ))
        .prompt("Hello");
    let body = compile_body(&req).await.unwrap();
    assert_eq!(
        body["tools"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert_eq!(
        body["messages"][1]["content"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
}

fn bedrock(id: &str) -> LanguageModel {
    bc::route()
        .provider("amazon-bedrock")
        .metadata_key("bedrock")
        .base_url(Some(bc::bedrock_base_url("us-east-1")))
        .auth(auth::none())
        .model(id)
        .unwrap()
}

const ARN: &str = "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abc123";

#[tokio::test]
async fn bedrock_emits_checkpoints_for_an_opaque_arn_declaring_the_anthropic_format() {
    for ttl in [None, Some(3600.0)] {
        let model = bedrock(ARN).with_compatibility(Compatibility {
            cache_format: Some("anthropic".into()),
            ..Default::default()
        });
        let mut req = LLMRequest::new(model);
        req.system = vec![SystemPart::text("Agent"), SystemPart::text("Project")];
        req = req
            .tool(ToolDefinition::new(
                "lookup",
                "Lookup",
                json!({"type": "object"}),
            ))
            .prompt("Question");
        if ttl.is_some() {
            req.cache = Some(CachePolicy::Object(CachePolicyObject {
                tools: Some(true),
                system: Some(true),
                messages: Some(CacheMessages::Tail { tail: 1 }),
                ttl_seconds: ttl,
            }));
        }
        let body = compile_body(&req).await.unwrap();
        let marker = if ttl.is_none() {
            json!({"cachePoint": {"type": "default"}})
        } else {
            json!({"cachePoint": {"type": "default", "ttl": "1h"}})
        };
        assert_eq!(body["modelId"], ARN);
        assert_eq!(body["toolConfig"]["tools"][1], marker);
        assert_eq!(
            body["system"],
            json!([{"text": "Agent"}, marker, {"text": "Project"}, marker])
        );
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": [{"text": "Question"}, marker]}])
        );
        if ttl.is_none() {
            assert!(!body.to_string().contains("\"ttl\""));
        }
    }
}

#[tokio::test]
async fn bedrock_ignores_the_alibaba_cache_format() {
    for id in [ARN, "amazon.nova-lite-v1:0"] {
        let model = bedrock(id).with_compatibility(Compatibility {
            cache_format: Some("alibaba".into()),
            ..Default::default()
        });
        let body = compile_body(
            &LLMRequest::new(model)
                .system("System prefix")
                .prompt("Question"),
        )
        .await
        .unwrap();
        assert!(!body.to_string().contains("cachePoint"), "{id}");
    }
}

#[test]
fn skips_non_cacheable_parts_when_placing_message_breakpoints() {
    let model = am::route()
        .base_url(Some("https://api.anthropic.test/v1/".into()))
        .auth(auth::none())
        .model("claude-sonnet-4-5")
        .unwrap();
    let compaction = ContentPart::Compaction(CompactionPart {
        provider: "anthropic".into(),
        id: None,
        encrypted: None,
        text: Some(Some("summary".into())),
    });
    let tool_call: ContentPart = serde_json::from_value(
        json!({"type": "tool-call", "id": "tc_1", "name": "lookup", "input": {}}),
    )
    .unwrap();
    let policy = CachePolicy::Object(CachePolicyObject {
        messages: Some(CacheMessages::Named("latest-assistant".into())),
        ..Default::default()
    });

    let mut mixed = LLMRequest::new(model.clone());
    mixed.messages = vec![Message::assistant(vec![tool_call, compaction.clone()])];
    mixed.cache = Some(policy.clone());
    apply_cache_policy(&mut mixed);
    assert!(mixed.messages[0].content[0].cache().is_some());
    assert_eq!(mixed.messages[0].content[1], compaction);

    let mut only = LLMRequest::new(model);
    only.messages = vec![Message::assistant(vec![compaction.clone()])];
    only.cache = Some(policy);
    let before = only.messages.clone();
    apply_cache_policy(&mut only);
    assert_eq!(only.messages, before);
}

// ---------------------------------------------------------------------------------------------
// prompt_cache_key
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn responses_omits_the_prompt_cache_key_when_compatibility_disables_it() {
    let model = oai::route()
        .base_url(Some("https://api.openai.test/v1/".into()))
        .auth(auth::none())
        .model("gpt-4.1-mini")
        .unwrap();
    let mut req = LLMRequest::new(model.clone().with_compatibility(Compatibility {
        supports_prompt_cache_key: Some(false),
        ..Default::default()
    }))
    .prompt("Hello");
    req.prompt_cache_key = Some("request_cache".into());
    assert!(
        compile_body(&req)
            .await
            .unwrap()
            .get("prompt_cache_key")
            .is_none()
    );
    let mut req = LLMRequest::new(model).prompt("Hello");
    req.prompt_cache_key = Some("request_cache".into());
    assert_eq!(
        compile_body(&req).await.unwrap()["prompt_cache_key"],
        "request_cache"
    );
}

#[tokio::test]
async fn mistral_omits_the_prompt_cache_key_when_compatibility_disables_it() {
    let model = mc::route()
        .auth(auth::none())
        .model("mistral-small-latest")
        .unwrap()
        .with_compatibility(Compatibility {
            supports_prompt_cache_key: Some(false),
            ..Default::default()
        });
    let mut req = LLMRequest::new(model)
        .prompt("Hello")
        .provider_option("promptCacheKey", json!("native-key"));
    req.prompt_cache_key = Some("common-key".into());
    assert!(
        compile_body(&req)
            .await
            .unwrap()
            .get("prompt_cache_key")
            .is_none()
    );
}

// ---------------------------------------------------------------------------------------------
// OpenRouter body precedence
// ---------------------------------------------------------------------------------------------

fn openrouter(options: Value) -> LanguageModel {
    let mut settings = options.as_object().cloned().unwrap_or_default();
    settings.insert("apiKey".into(), json!("test-key"));
    oc_ai::providers::language_model(
        "@opencode/ai/providers/openrouter",
        "openai/gpt-4o-mini",
        &settings,
    )
    .unwrap()
}

#[tokio::test]
async fn openrouter_pass_through_options_cannot_override_protocol_fields() {
    let req = LLMRequest::new(openrouter(json!({})))
        .prompt("Hello")
        .provider_option("model", json!("other/model"))
        .provider_option("stream", json!(false))
        .provider_option("future_option", json!({"enabled": true}));
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["model"], "openai/gpt-4o-mini");
    assert_eq!(body["stream"], true);
    assert_eq!(body["future_option"], json!({"enabled": true}));
    let keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    // Pass-through options lead the body; the protocol fields override them in place.
    assert_eq!(&keys[..3], ["model", "stream", "future_option"]);
}

#[tokio::test]
async fn openrouter_filters_invalid_known_options_while_preserving_extensions() {
    let mut req = LLMRequest::new(openrouter(json!({}))).prompt("Hello");
    for (k, v) in [
        ("usage", json!("yes")),
        ("models", json!("anthropic/claude-sonnet-4.6")),
        ("provider", json!([])),
        ("plugins", json!({})),
        ("web_search_options", json!([])),
        ("debug", json!([])),
        ("user", json!(123)),
        ("reasoning", json!([])),
        ("promptCacheKey", json!(123)),
        ("future_option", json!({"enabled": true})),
    ] {
        req = req.provider_option(k, v);
    }
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["future_option"], json!({"enabled": true}));
    for key in [
        "usage",
        "models",
        "provider",
        "plugins",
        "web_search_options",
        "debug",
        "user",
        "reasoning",
        "prompt_cache_key",
    ] {
        assert!(body.get(key).is_none(), "{key}");
    }
    let req = LLMRequest::new(openrouter(json!({})))
        .prompt("Hello")
        .provider_option("models", json!(["a", 1]))
        .provider_option("plugins", json!([{"id": "x"}, "y"]));
    let body = compile_body(&req).await.unwrap();
    assert!(body.get("models").is_none());
    assert!(body.get("plugins").is_none());
}

#[tokio::test]
async fn openrouter_preserves_reasoning_details_after_an_empty_assistant() {
    let details = json!([
        {"type": "reasoning.text", "text": "Think", "format": "anthropic-claude-v1", "index": 0},
        {"type": "reasoning.text", "signature": "signed", "format": "anthropic-claude-v1", "index": 0}
    ]);
    let reasoning: ContentPart = serde_json::from_value(json!({
        "type": "reasoning",
        "text": "Thinking",
        "providerMetadata": {"openrouter": {"reasoningField": "reasoning", "reasoningDetails": details}}
    }))
    .unwrap();
    let model = package(
        "openrouter",
        "anthropic/claude-sonnet-4.6",
        json!({"apiKey": "test-key"}),
    );
    let mut req = LLMRequest::new(model);
    req.cache = Some(CachePolicy::none());
    req.messages = vec![
        Message::assistant(vec![ContentPart::text("")]),
        Message::assistant(vec![reasoning]),
    ];
    let body = compile_body(&req).await.unwrap();
    assert_eq!(
        body["messages"],
        json!([{"role": "assistant", "content": "", "reasoning": "Thinking", "reasoning_details": details}])
    );
}

// ---------------------------------------------------------------------------------------------
// Tolerant stream decoding
// ---------------------------------------------------------------------------------------------

fn responses_request() -> LLMRequest {
    LLMRequest::new(
        oai::route()
            .base_url(Some("https://api.openai.test/v1/".into()))
            .auth(auth::none())
            .model("gpt-4.1-mini")
            .unwrap(),
    )
    .prompt("Hello")
}

#[test]
fn responses_ignores_untyped_sse_frames() {
    let events = decode_ok(
        &responses_request(),
        &sse(&[
            json!({}),
            json!({"type": "response.created", "response": {"id": "resp_1"}}),
            json!({"type": "response.output_item.added", "item": {"type": "message", "id": "msg_1"}}),
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "PO"}),
            json!({"gateway": "keep-alive"}),
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "NG"}),
            json!({"type": "response.output_item.done", "item": {"type": "message", "id": "msg_1"}}),
            json!({"type": "response.completed", "response": {"id": "resp_1"}}),
        ]),
    );
    assert_eq!(text_of_events(&events), "PONG");
    let LLMEvent::Finish { reason, .. } = events.last().unwrap() else {
        panic!()
    };
    assert_eq!(reason.normalized, FinishReason::Stop);
}

#[test]
fn responses_treats_null_optional_fields_as_absent() {
    let call = json!({"type": "function_call", "id": null, "call_id": "call_1", "name": "lookup", "namespace": null});
    let message = json!({"type": "message", "id": "msg_1", "role": "assistant"});
    let with = |base: &Value, extra: Value| {
        let mut out = base.clone();
        for (k, v) in extra.as_object().unwrap() {
            out[k] = v.clone();
        }
        out
    };
    let req = responses_request().tool(ToolDefinition::new(
        "lookup",
        "Lookup",
        json!({"type": "object"}),
    ));
    let events = decode_ok(
        &req,
        &sse(&[
            json!({"type": "response.created", "response": null}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": with(&message, json!({"content": []}))}),
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": null, "delta": "Hi"}),
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0, "delta": null}),
            json!({"type": "response.output_text.done", "item_id": "msg_1", "output_index": 0, "text": null}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": message}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": with(&call, json!({"arguments": null}))}),
            json!({"type": "response.function_call_arguments.done", "item_id": "fc", "output_index": 1, "arguments": null}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": with(&call, json!({"arguments": "{}"}))}),
            json!({"type": "response.output_item.done", "output_index": 2, "item": {"type": "function_call", "id": "fc_2", "call_id": null, "name": null, "arguments": "{}"}}),
            json!({"type": "response.output_item.added", "output_index": 3, "item": {"type": "reasoning", "id": "rs_1"}}),
            json!({"type": "response.reasoning_summary_part.added", "item_id": "rs_1", "output_index": 3, "summary_index": null}),
            json!({"type": "response.output_item.done", "output_index": 3, "item": {"type": "reasoning", "id": "rs_1"}}),
            json!({"type": "response.completed", "response": {
                "id": null,
                "output": null,
                "incomplete_details": {"reason": null},
                "usage": {
                    "input_tokens": 5,
                    "input_tokens_details": {"cached_tokens": null, "cache_write_tokens": null},
                    "output_tokens": null,
                    "output_tokens_details": {"reasoning_tokens": null},
                    "total_tokens": "7"
                }
            }}),
        ]),
    );
    assert_eq!(text_of_events(&events), "Hi");
    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let LLMEvent::ToolCall {
                id,
                name,
                namespace,
                input,
                ..
            } = e
            {
                Some((id, name, namespace, input))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        (
            calls[0].0.as_str(),
            calls[0].1.as_str(),
            calls[0].2,
            calls[0].3
        ),
        ("call_1", "lookup", &None, &json!({}))
    );
    let LLMEvent::Finish {
        usage: Some(usage), ..
    } = events.last().unwrap()
    else {
        panic!()
    };
    assert_eq!(usage.input_tokens, Some(5));
    assert_eq!(usage.output_tokens, None);
    assert_eq!(usage.cache_read_input_tokens, None);
}

#[test]
fn channel_reads_untyped_error_envelopes_as_errors_and_ignores_other_untyped_frames() {
    let message = "gRPC error: Response with id=resp_missing not found";
    for error in [
        json!({"type": "api_error", "message": message}),
        json!(message),
        json!(42),
    ] {
        let frame = json!({"error": error}).to_string();
        assert_eq!(
            decode_channel_event(&frame),
            Some(json!({"error": error, "type": "error"}))
        );
    }
    for frame in [
        json!({}),
        json!({"error": null}),
        json!({"message": message}),
    ] {
        let mut expected = frame.clone();
        expected["type"] = json!("ignored");
        assert_eq!(decode_channel_event(&frame.to_string()), Some(expected));
    }
}

fn anthropic_request() -> LLMRequest {
    LLMRequest::new(
        am::route()
            .base_url(Some("https://api.anthropic.test/v1/".into()))
            .auth(auth::none())
            .model("claude-sonnet-4-5")
            .unwrap(),
    )
    .prompt("Hello")
}

#[test]
fn anthropic_ignores_untyped_frames_but_reads_untyped_error_envelopes_as_errors() {
    let events = decode_ok(
        &anthropic_request(),
        &sse(&[
            json!({}),
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 5}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"gateway": "keep-alive"}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "OK"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
            json!({"type": "message_stop"}),
        ]),
    );
    assert_eq!(text_of_events(&events), "OK");
    let out = decode(
        &anthropic_request(),
        &sse(&[json!({"error": {"type": "overloaded_error", "message": "Overloaded"}})]),
    );
    let error = out.last().unwrap().as_ref().unwrap_err();
    assert_eq!(error.tag(), "ProviderInternal");
    assert_eq!(error.message, "overloaded_error: Overloaded");
}

#[test]
fn cohere_ignores_unknown_stream_events_but_rejects_malformed_known_ones() {
    let req = LLMRequest::new(
        cc::route()
            .auth(auth::none())
            .model("command-a-03-2025")
            .unwrap(),
    )
    .prompt("Hi");
    let events = decode_ok(
        &req,
        &sse(&[
            json!({"type": "message-start"}),
            json!({"type": "debug", "detail": "new event type"}),
            json!({"type": "content-delta", "index": 0, "delta": {"message": {"content": {"text": "OK"}}}}),
            json!({"type": "content-end", "index": 0}),
            json!({"type": "message-end", "delta": {"finish_reason": "COMPLETE"}}),
        ]),
    );
    assert_eq!(text_of_events(&events), "OK");
    let out = decode(
        &req,
        &sse(&[
            json!({"type": "message-start"}),
            json!({"type": "content-delta", "delta": {"message": {"content": {"text": "OK"}}}}),
            json!({"type": "message-end", "delta": {"finish_reason": "COMPLETE"}}),
        ]),
    );
    assert_eq!(
        out.last().unwrap().as_ref().unwrap_err().tag(),
        "InvalidProviderOutput"
    );
}

#[test]
fn interactions_ignore_unknown_stream_events_but_reject_malformed_known_ones() {
    let req = LLMRequest::new(
        gi::route()
            .auth(auth::none())
            .model("gemini-3.8-flash")
            .unwrap(),
    )
    .prompt("Hi");
    let events = decode_ok(
        &req,
        &sse(&[
            json!({"event_type": "step.start", "index": 0, "step": {"type": "model_output"}}),
            json!({"event_type": "interaction.heartbeat"}),
            json!({"event_type": "step.delta", "index": 0, "delta": {"type": "text", "text": "OK"}}),
            json!({"event_type": "step.stop", "index": 0}),
            json!({"event_type": "interaction.completed", "interaction": {"status": "completed"}}),
        ]),
    );
    assert_eq!(text_of_events(&events), "OK");
    let out = decode(
        &req,
        &sse(&[
            json!({"event_type": "step.delta", "delta": {"type": "text", "text": "OK"}}),
            json!({"event_type": "interaction.completed", "interaction": {"status": "completed"}}),
        ]),
    );
    assert_eq!(
        out.last().unwrap().as_ref().unwrap_err().tag(),
        "InvalidProviderOutput"
    );
    // Unknown deltas still fail explicitly, so new output modalities never disappear.
    let out = decode(
        &req,
        &sse(&[
            json!({"event_type": "step.start", "index": 0, "step": {"type": "model_output"}}),
            json!({"event_type": "step.delta", "index": 0, "delta": {"type": "image", "data": "x"}}),
        ]),
    );
    let error = out.last().unwrap().as_ref().unwrap_err();
    assert_eq!(error.message, "Unsupported Interactions delta: image");
}

// ---------------------------------------------------------------------------------------------
// Bedrock stream exceptions
// ---------------------------------------------------------------------------------------------

#[test]
fn bedrock_uses_original_message_and_status_code_from_stream_exceptions() {
    use oc_ai::route::framing::encode_aws_message;
    let req = LLMRequest::new(bedrock("us.amazon.nova-micro-v1:0"));
    let frame = |payload: &str| {
        encode_aws_message(
            &[
                (":exception-type", "modelStreamErrorException"),
                (":content-type", "application/json"),
                (":message-type", "exception"),
            ],
            payload.as_bytes(),
        )
    };
    let out = decode(
        &req,
        &frame(r#"{"originalMessage":"Upstream model failed","originalStatusCode":500}"#),
    );
    let error = out.last().unwrap().as_ref().unwrap_err();
    assert_eq!(
        (error.tag(), error.message.as_str()),
        ("ProviderInternal", "Upstream model failed")
    );
    let out = decode(
        &req,
        &frame(r#"{"originalMessage":"Upstream rejected request","originalStatusCode":400}"#),
    );
    let error = out.last().unwrap().as_ref().unwrap_err();
    assert_eq!(
        (error.tag(), error.message.as_str()),
        ("InvalidRequest", "Upstream rejected request")
    );
    let out = decode(
        &req,
        &frame(r#"{"originalMessage":"Slow down","originalStatusCode":429}"#),
    );
    assert_eq!(out.last().unwrap().as_ref().unwrap_err().tag(), "RateLimit");
}
