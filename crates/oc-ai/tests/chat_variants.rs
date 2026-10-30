mod common;

use common::*;
use oc_ai::protocols::{openai_chat as oc, openai_chat_variants as v};
use oc_ai::route::auth;
use oc_ai::testing::compile_body;
use oc_ai::*;
use serde_json::json;

fn venice(id: &str) -> LanguageModel {
    let mut route = oc::compatible_route().id("venice-chat").provider("venice").base_url(Some("https://api.venice.ai/api/v1".into())).auth(auth::none());
    route.protocol = v::venice_protocol();
    route.model(id).unwrap().with_compatibility(v::venice_compatibility())
}

fn lookup() -> ToolDefinition {
    ToolDefinition::new(
        "lookup_rate",
        "Retrieve the private conversion rate for a code. Only this tool knows the rate.",
        json!({"type":"object","properties":{"code":{"type":"string","enum":["ZEBRA","MOOSE"]}},"required":["code"],"additionalProperties":false}),
    )
}

async fn venice_three_turns(file: &str, model: &str, effort: Option<&str>) -> Vec<LLMResponse> {
    let rec = load(file);
    let mut req = LLMRequest::new(venice(model))
        .system("Use the provided tool for private data. After receiving the result, give only the requested number; do not call the tool again.")
        .prompt("Look up conversion rate for code ZEBRA. Then multiply that rate by 17 and add 9. You must use lookup_rate before answering.")
        .tool(lookup());
    req.generation = Some(GenerationOptions { max_tokens: Some(4096.0), ..Default::default() });
    if let Some(e) = effort {
        req = req.provider_option("reasoningEffort", json!(e));
    }
    let first = turn(&req, &rec, 0).await;
    assert_eq!(first.finish_reason.normalized, FinishReason::ToolCalls);
    let mut cont = req.clone();
    cont.tool_choice = Some(ToolChoice::from_str("none"));
    cont.messages.push(first.message.clone());
    for (id, name, _) in tool_calls(&first) {
        cont.messages.push(tool_result(&id, &name, json!({"code": "ZEBRA", "rate": 23})));
    }
    let second = turn(&cont, &rec, 1).await;
    assert!(second.text().contains("400"));
    assert!(!second.reasoning().contains("__ENCRYPTED_REASONING__"));
    let mut follow = cont.clone();
    follow.messages.push(second.message.clone());
    follow.messages.push(Message::user_text("Add 7 to your previous final answer. Reply with just the number."));
    let third = turn(&follow, &rec, 2).await;
    assert!(third.text().contains("407"));
    vec![first, second, third]
}

#[tokio::test]
async fn venice_scalar_reasoning_tool_loop() {
    venice_three_turns("chat/venice-deepseek.json", "deepseek-v4-flash", None).await;
}

#[tokio::test]
async fn venice_encrypted_reasoning_tool_loop() {
    let turns = venice_three_turns("chat/venice-encrypted.json", "openai-gpt-54-mini", None).await;
    assert!(!turns[0].reasoning().contains("__ENCRYPTED_REASONING__"));
}

#[tokio::test]
async fn zai_and_alibaba_bodies() {
    let mut route = oc::compatible_route().provider("zai").base_url(Some("https://api.z.ai/api/paas/v4".into())).auth(auth::none());
    route.protocol = v::zai_protocol();
    let model = route.model("glm-4.7").unwrap().with_compatibility(v::zai_compatibility());
    let req = LLMRequest::new(model).prompt("hi").tool(weather_tool()).provider_option("thinking", json!({"clear_thinking": false, "type": "enabled"}));
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["thinking"], json!({"type": "enabled", "clear_thinking": false}));
    assert_eq!(body["tool_stream"], true);
    assert!(body.get("store").is_none());
    assert!(body["tools"][0]["function"].get("strict").is_none());

    let mut route = oc::compatible_route().id("alibaba-chat").provider("alibaba").base_url(Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1".into())).auth(auth::none());
    route.protocol = v::alibaba_protocol();
    let model = route.model("qwen3-max").unwrap().with_compatibility(v::alibaba_compatibility());
    let mut req = LLMRequest::new(model).system("s").prompt("hi").provider_option("enableThinking", json!(true)).provider_option("thinkingBudget", json!(9000));
    req.generation = Some(GenerationOptions { max_tokens: Some(10000.0), top_k: Some(20.0), ..Default::default() });
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["thinking_budget"], 5000);
    assert_eq!(body["enable_thinking"], true);
    assert_eq!(body["top_k"], 20);
    // qwen auto cache policy places cache markers on system + last message
    assert_eq!(body["messages"][0]["content"][0]["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(body["messages"][1]["content"][0]["cache_control"], json!({"type": "ephemeral"}));
}

fn groq(id: &str, settings: serde_json::Value) -> LanguageModel {
    let mut settings = settings.as_object().cloned().unwrap_or_default();
    settings.insert("apiKey".into(), json!("fixture"));
    oc_ai::providers::language_model("@opencode/ai/providers/groq", id, &settings).unwrap()
}

/// groq.test.ts "Groq reuses Chat streaming and defaults to parsed reasoning"
#[tokio::test]
async fn groq_defaults_to_parsed_reasoning() {
    let mut req = LLMRequest::new(groq("llama-3.3-70b-versatile", json!({}))).prompt("Hello").tool(weather_tool());
    req.generation = Some(GenerationOptions { max_tokens: Some(64.0), ..Default::default() });
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["max_completion_tokens"], json!(64));
    assert_eq!(body["reasoning_format"], json!("parsed"));
    for key in ["store", "max_tokens", "include_reasoning", "parallel_tool_calls", "service_tier", "user"] {
        assert!(body.get(key).is_none(), "{key}");
    }
}

/// "Groq lowers its own options for custom catalog identities and endpoints": `parallelToolCalls` and
/// `serviceTier` go through the shared OpenAI Chat options (#58871).
#[tokio::test]
async fn groq_lowers_shared_options() {
    let model = groq(
        "qwen/qwen3.6-27b",
        json!({"reasoningEffort": "default", "parallelToolCalls": true, "serviceTier": "flex", "user": "test-user"}),
    );
    let mut req = LLMRequest::new(model).prompt("Hello").tool(weather_tool());
    req.provider_options =
        Some(json!({"parallelToolCalls": false, "includeReasoning": false}).as_object().cloned().unwrap());
    let body = compile_body(&req).await.unwrap();
    assert_eq!(body["reasoning_effort"], json!("default"));
    assert_eq!(body["reasoning_format"], json!("parsed"));
    assert_eq!(body["parallel_tool_calls"], json!(false));
    assert_eq!(body["service_tier"], json!("flex"));
    assert_eq!(body["user"], json!("test-user"));
    assert!(body.get("include_reasoning").is_none());
}

/// "Groq honors disableParallelToolUse and sends parallel_tool_calls only with tools"
#[tokio::test]
async fn groq_parallel_tool_calls_only_with_tools() {
    let mut with_tools = LLMRequest::new(groq("openai/gpt-oss-20b", json!({}))).prompt("Hello").tool(weather_tool());
    with_tools.tool_choice = Some(ToolChoice { disable_parallel_tool_use: Some(true), ..ToolChoice::from_str("auto") });
    assert_eq!(compile_body(&with_tools).await.unwrap()["parallel_tool_calls"], json!(false));
    let mut without_tools = LLMRequest::new(groq("openai/gpt-oss-20b", json!({}))).prompt("Hello");
    without_tools.provider_options = Some(json!({"parallelToolCalls": true}).as_object().cloned().unwrap());
    assert!(compile_body(&without_tools).await.unwrap().get("parallel_tool_calls").is_none());
}

/// "Groq validates option types": only its own `includeReasoning` (and `user`) are checked strictly now.
#[tokio::test]
async fn groq_validates_its_own_option_types() {
    let mut req = LLMRequest::new(groq("qwen", json!({}))).prompt("Hello");
    req.provider_options = Some(json!({"includeReasoning": "false"}).as_object().cloned().unwrap());
    assert_eq!(compile_body(&req).await.unwrap_err().tag(), "InvalidRequest");
    req.provider_options = Some(json!({"parallelToolCalls": "false"}).as_object().cloned().unwrap());
    assert!(compile_body(&req).await.is_ok());
}
