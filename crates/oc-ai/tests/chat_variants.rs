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
