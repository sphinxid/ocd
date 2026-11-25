//! Ports of the protocol tests opencode 2.4.1 added (packages/ai/test): tool namespaces flattened once for protocols
//! without native ones (#65621), undeclared replayed tool names in provider-safe form (#65292), and HTTP timeouts on
//! every request (#64692).

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

// ---- HTTP timeouts on every request (#64692) ----

/// A server that accepts connections and answers `head` (nothing when `None`), then keeps the connection open.
async fn stalling(head: Option<&'static str>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                if let Some(head) = head {
                    let _ = socket.write_all(head.as_bytes()).await;
                }
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                drop(socket);
            });
        }
    });
    base
}

fn timeouts(header_ms: f64, chunk_ms: f64, total_ms: Option<f64>) -> oc_ai::route::transport::Timeouts {
    oc_ai::route::transport::Timeouts::from_http(Some(&HttpOptions {
        timeout: total_ms.map(HttpTimeout::Millis),
        header_timeout: Some(HttpTimeout::Millis(header_ms)),
        chunk_timeout: Some(HttpTimeout::Millis(chunk_ms)),
        ..Default::default()
    }))
}

fn timed_out(error: &AIError, operation: &str, message: &str) {
    let ErrorKind::Transport { code, operation: op, .. } = &error.kind else { panic!("{error:?}") };
    assert_eq!(code.as_deref(), Some("Timeout"), "{error:?}");
    assert_eq!(format!("{op:?}").to_lowercase(), operation, "{error:?}");
    assert_eq!(error.message, message);
}

#[tokio::test]
async fn media_requests_time_out_waiting_for_headers_and_between_chunks() {
    let client = oc_util::http::client();
    let silent = stalling(None).await;
    let error = oc_ai::route::media::download_with(&client, &format!("{silent}/a.png"), None, timeouts(200.0, 60_000.0, None))
        .await
        .unwrap_err();
    timed_out(&error, "request", "Timed out waiting for response headers");
    let partial = stalling(Some("HTTP/1.1 200 OK\r\ncontent-type: image/png\r\ncontent-length: 100\r\n\r\nPNG")).await;
    let error = oc_ai::route::media::download_with(&client, &format!("{partial}/a.png"), None, timeouts(60_000.0, 200.0, None))
        .await
        .unwrap_err();
    timed_out(&error, "read", "Timed out waiting for response data");
    assert_eq!(error.http.as_ref().map(|h| h.status), Some(200));
    let error = oc_ai::route::media::download_with(&client, &format!("{partial}/a.png"), None, timeouts(60_000.0, 60_000.0, Some(300.0)))
        .await
        .unwrap_err();
    timed_out(&error, "read", "Timed out waiting for the response to complete");
    // Unset phases take the defaults: 5 minutes for headers, 15 between chunks, no whole-request limit.
    let defaults = oc_ai::route::transport::Timeouts::from_http(None);
    assert_eq!(defaults.header, Some(std::time::Duration::from_secs(300)));
    assert_eq!(defaults.chunk, Some(std::time::Duration::from_secs(900)));
    assert_eq!(defaults.total, None);
}

/// "applies provider timeouts to resolved image, video, and decision models" (model-resolver.test.ts)
#[test]
fn applies_provider_timeouts_to_resolved_image_and_decision_models() {
    use oc_ai::catalog::resolve::{resolve_media, MediaModel, MediaType};
    use oc_ai::catalog::{ModelInfo, ProviderInfo};
    let settings = json!({"apiKey": "secret", "timeout": 12_000, "headerTimeout": 3_000, "chunkTimeout": 4_000});
    let mut provider = ProviderInfo::empty("timed");
    provider.activation = oc_ai::catalog::types::Activation::Enabled;
    provider.settings = settings.as_object().cloned();
    let expected = (Some(HttpTimeout::Millis(12_000.0)), Some(HttpTimeout::Millis(3_000.0)), Some(HttpTimeout::Millis(4_000.0)));
    for (kind, package, id) in [
        (MediaType::Image, "@opencode/ai/providers/openai/images", "gpt-image-2"),
        (MediaType::Decision, "@opencode/ai/providers/google/decisions", "gemini-2.5-flash"),
    ] {
        let mut model = ModelInfo::default_for("timed", id);
        model.kind = Some(if kind == MediaType::Image { "image" } else { "decision" }.into());
        model.package = Some(package.into());
        let http = match resolve_media(Some(&provider), &model, kind, None, None, &Default::default()).unwrap() {
            MediaModel::Image(m) => m.http,
            MediaModel::Decision(m) => m.http,
        };
        let http = http.unwrap();
        assert_eq!((http.timeout, http.header_timeout, http.chunk_timeout), expected, "{package}");
    }
}
