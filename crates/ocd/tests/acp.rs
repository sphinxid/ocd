//! ACP over stdio against a standalone server and a mock provider.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;

use serde_json::{Value, json};

fn mock_provider() -> u16 {
    use axum::{Router, body::Body, response::Response, routing::post};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let mut body = String::new();
            for c in [
                json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","content":"Hi "}}]}),
                json!({"id":"c","choices":[{"index":0,"delta":{"content":"there"}}]}),
                json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14}}),
            ] {
                body.push_str(&format!("data: {c}\n\n"));
            }
            body.push_str("data: [DONE]\n\n");
            Response::builder().header("content-type", "text/event-stream").body(Body::from(body)).unwrap()
        }),
    );
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move {
            axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), app).await.unwrap();
        })
    });
    port
}

#[test]
fn acp_initialize_new_session_prompt_and_eof() {
    let env = common::Env::new();
    let port = mock_provider();
    std::fs::write(
        env.path("work/opencode.json"),
        json!({"model": "mock/m1", "providers": {"mock": {"name": "Mock", "package": "@opencode/ai/providers/openai-compatible",
            "settings": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "k"},
            "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}}}}}})
        .to_string(),
    )
    .unwrap();
    let mut child = env.command(&["acp"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut send = |v: Value| {
        stdin.write_all(format!("{v}\n").as_bytes()).unwrap();
        stdin.flush().unwrap();
    };
    let mut read_until_id = |id: i64, seen: &mut Vec<Value>| -> Value {
        loop {
            let mut line = String::new();
            assert!(stdout.read_line(&mut line).unwrap() > 0, "stdout closed");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v.get("id") == Some(&json!(id)) && v.get("method").is_none() {
                return v;
            }
            seen.push(v);
        }
    };
    let mut seen = vec![];
    send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 99, "clientCapabilities": {}}}));
    let init = read_until_id(1, &mut seen);
    assert_eq!(init["result"]["protocolVersion"], 1);
    assert_eq!(init["result"]["authMethods"][0]["id"], "opencode-login");
    assert_eq!(init["result"]["agentInfo"]["name"], "OpenCode");

    send(json!({"jsonrpc": "2.0", "id": 2, "method": "authenticate", "params": {"methodId": "nope"}}));
    let auth = read_until_id(2, &mut seen);
    assert_eq!(auth["error"]["code"], -32602);
    assert_eq!(auth["error"]["message"], "Invalid params: unknown auth method: nope");

    let cwd = env.path("work").display().to_string();
    send(json!({"jsonrpc": "2.0", "id": 3, "method": "session/new", "params": {"cwd": cwd, "mcpServers": []}}));
    let created = read_until_id(3, &mut seen);
    let sid = created["result"]["sessionId"].as_str().unwrap().to_string();
    assert!(sid.starts_with("ses_"));
    assert_eq!(created["result"]["configOptions"][0]["currentValue"], "mock/m1");

    send(json!({"jsonrpc": "2.0", "id": 4, "method": "session/prompt", "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "hello"}]}}));
    let mut updates = vec![];
    let response = read_until_id(4, &mut updates);
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(response["result"]["usage"]["totalTokens"], 14);
    let kinds: Vec<&str> = updates.iter().filter_map(|u| u.pointer("/params/update/sessionUpdate").and_then(Value::as_str)).collect();
    assert!(kinds.contains(&"available_commands_update"), "{kinds:?}");
    let text: String = updates
        .iter()
        .filter(|u| u.pointer("/params/update/sessionUpdate") == Some(&json!("agent_message_chunk")))
        .map(|u| u.pointer("/params/update/content/text").and_then(Value::as_str).unwrap_or("").to_string())
        .collect();
    assert_eq!(text, "Hi there");
    assert!(kinds.contains(&"usage_update"), "{kinds:?}");

    send(json!({"jsonrpc": "2.0", "id": 5, "method": "session/set_model", "params": {}}));
    let missing = read_until_id(5, &mut seen);
    assert_eq!(missing["error"]["code"], -32601);

    send(json!({"jsonrpc": "2.0", "id": 6, "method": "session/list", "params": {"cwd": cwd}}));
    let list = read_until_id(6, &mut seen);
    assert_eq!(list["result"]["sessions"][0]["sessionId"], sid);

    drop(send);
    drop(stdin); // EOF ends the connection
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0));
}
