//! Ports of the protocol tests opencode 2.4.1 added (packages/ai/test): tool namespaces flattened once for protocols
//! without native ones (#65621) and undeclared replayed tool names in provider-safe form (#65292).

use oc_ai::testing::compile_body;
use oc_ai::*;
use serde_json::{json, Value};

fn settings(value: Value) -> serde_json::Map<String, Value> {
    value.as_object().cloned().unwrap()
}

fn package(name: &str, id: &str, extra: Value) -> LanguageModel {
    let mut s = settings(json!({"apiKey": "unused"}));
    s.extend(settings(extra));
    oc_ai::providers::language_model(&format!("@opencode/ai/providers/{name}"), id, &s).unwrap()
}

fn messages(value: Value) -> Vec<Message> {
    serde_json::from_value(value).expect("messages")
}

fn crm(tools: Vec<ToolEntry>) -> ToolEntry {
    ToolEntry::Namespace(ToolNamespace { name: "crm".into(), description: Some("CRM tools".into()), tools })
}

fn tool(name: &str, description: &str) -> ToolEntry {
    ToolEntry::Tool(ToolDefinition::new(name, description, json!({})))
}

/// "forces a namespaced tool under its flattened name on flat protocols"
#[tokio::test]
async fn forces_a_namespaced_tool_under_its_flattened_name_on_flat_protocols() {
    for (name, extra) in [
        ("anthropic", json!({})),
        ("amazon-bedrock", json!({"baseURL": "https://bedrock-runtime.test"})),
        ("cohere", json!({})),
        ("google", json!({})),
        ("google/interactions", json!({})),
        ("mistral", json!({})),
        ("openai-compatible", json!({"baseURL": "https://example.invalid/v1"})),
        ("openai-compatible-responses", json!({"baseURL": "https://example.invalid/v1"})),
    ] {
        let mut req = LLMRequest::new(package(name, "test-model", extra)).prompt("Look up a customer");
        req.tools = vec![crm(vec![tool("lookup", "Look up")])];
        req.tool_choice = Some(ToolChoice::from_str("crm.lookup"));
        let body = compile_body(&req).await.unwrap().to_string();
        assert!(body.contains("crm_lookup"), "{name}: {body}");
        assert!(!body.contains("crm.lookup"), "{name}: {body}");
    }
}

/// "flattens a forced choice only when it names a namespaced tool"
#[tokio::test]
async fn flattens_a_forced_choice_only_when_it_names_a_namespaced_tool() {
    let chat = |name: &str| {
        let model = package("openai-compatible", "gpt-4o-mini", json!({"baseURL": "https://api.openai.test/v1"}));
        let mut req = LLMRequest::new(model).prompt("Hi");
        req.tools = vec![tool("foo_bar", "Top level"), crm(vec![tool("customer.lookup", "Look up")])];
        req.tool_choice = Some(ToolChoice::from_str(name));
        req
    };
    // A dotted leaf flattens as flattenTools names it.
    let body = compile_body(&chat("crm.customer.lookup")).await.unwrap();
    assert_eq!(body["tool_choice"], json!({"type": "function", "function": {"name": "crm_customer.lookup"}}));
    // A dotted name that isn't a namespaced tool is left for the provider to reject, not matched to foo_bar.
    let body = compile_body(&chat("foo.bar")).await.unwrap();
    assert_eq!(body["tool_choice"], json!({"type": "function", "function": {"name": "foo.bar"}}));
}

/// "flattens namespaces when a flat protocol's body is built directly"
#[test]
fn flattens_namespaces_when_a_flat_protocols_body_is_built_directly() {
    let mut req = LLMRequest::new(package("openai-compatible", "gpt-4o-mini", json!({"baseURL": "https://api.openai.test/v1"})));
    req.tools = vec![crm(vec![tool("lookup", "Look up")])];
    req.messages = messages(json!([
        {"role": "user", "content": [{"type": "text", "text": "Look up a customer"}]},
        {"role": "assistant", "content": [{"type": "tool-call", "id": "call_1", "name": "lookup", "namespace": "crm", "input": {}}]}
    ]));
    req.tool_choice = Some(ToolChoice::from_str("crm.lookup"));
    let body = req.model.route.protocol.body(&req).unwrap();
    let text = body.to_string();
    assert!(text.contains("crm_lookup"), "{text}");
    assert!(!text.contains("crm.lookup"), "{text}");
    assert_eq!(body["messages"][1]["role"], "assistant");
    assert_eq!(body["messages"][1]["tool_calls"][0]["function"]["name"], "crm_lookup");
}

/// "replaces characters providers reject in replayed names of undeclared tool calls" (openai-chat.test.ts)
#[tokio::test]
async fn replaces_characters_providers_reject_in_replayed_names_of_undeclared_tool_calls() {
    let mut req = LLMRequest::new(package("openai-compatible", "gpt-4o-mini", json!({"baseURL": "https://api.openai.test/v1"})));
    req.tools = vec![tool("weather.get", "Weather")];
    req.messages = messages(json!([
        {"role": "assistant", "content": [
            {"type": "tool-call", "id": "call_1", "name": "opencode.session.search", "input": {}},
            {"type": "tool-call", "id": "call_2", "name": "weather.get", "input": {}}
        ]},
        {"role": "tool", "content": [{"type": "tool-result", "id": "call_1", "name": "opencode.session.search", "result": {"type": "error", "value": "unknown tool"}}]},
        {"role": "tool", "content": [{"type": "tool-result", "id": "call_2", "name": "weather.get", "result": {"type": "text", "value": "sunny"}}]}
    ]));
    let body = compile_body(&req).await.unwrap();
    // A declared name stays as declared, for providers that accept it.
    assert_eq!(body["messages"][0]["role"], "assistant");
    assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call_1");
    assert_eq!(body["messages"][0]["tool_calls"][0]["function"]["name"], "opencode_session_search");
    assert_eq!(body["messages"][0]["tool_calls"][1]["function"]["name"], "weather.get");
}

/// Bedrock Converse only truncates replayed names to 64 UTF-16 units now; the shared flattening sanitizes them.
#[tokio::test]
async fn bedrock_truncates_replayed_names_after_the_shared_sanitizing() {
    let long = format!("a.{}", "b".repeat(80));
    let mut req = LLMRequest::new(package("amazon-bedrock", "anthropic.claude-test", json!({"baseURL": "https://bedrock-runtime.test"})));
    req.messages = messages(json!([
        {"role": "user", "content": [{"type": "text", "text": "Hi"}]},
        {"role": "assistant", "content": [{"type": "tool-call", "id": "call_1", "name": long, "input": {}}]},
        {"role": "tool", "content": [{"type": "tool-result", "id": "call_1", "name": long, "result": {"type": "text", "value": "ok"}}]}
    ]));
    let body = compile_body(&req).await.unwrap();
    let name = body["messages"][1]["content"][0]["toolUse"]["name"].as_str().unwrap();
    assert_eq!(name, format!("a_{}", "b".repeat(62)));
}

