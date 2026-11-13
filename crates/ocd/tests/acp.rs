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

    // `_session/steering` (opencode 2.2.7): advertised, refused for an idle session, params checked.
    assert_eq!(init["result"]["_meta"], json!({"steering": {"supported": true}}));
    send(json!({"jsonrpc": "2.0", "id": 7, "method": "_session/steering", "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "redirect"}]}}));
    let steer = read_until_id(7, &mut seen);
    assert_eq!(steer["result"], json!({"outcome": "promptRequired", "reason": "noRunningTurn"}), "{steer}");
    send(json!({"jsonrpc": "2.0", "id": 8, "method": "_session/steering", "params": {"sessionId": sid}}));
    let bad = read_until_id(8, &mut seen);
    assert_eq!(bad["error"], json!({"code": -32602, "message": "Invalid params", "data": {"details": "Missing key\n  at [\"prompt\"]"}}));

    // Closing a session nobody prompted removes it; a prompted one stays.
    send(json!({"jsonrpc": "2.0", "id": 9, "method": "session/new", "params": {"cwd": cwd, "mcpServers": []}}));
    let unused = read_until_id(9, &mut seen)["result"]["sessionId"].as_str().unwrap().to_string();
    for (id, session) in [(10, &unused), (11, &sid)] {
        send(json!({"jsonrpc": "2.0", "id": id, "method": "session/close", "params": {"sessionId": session}}));
        assert_eq!(read_until_id(id, &mut seen)["result"], json!({}));
    }
    send(json!({"jsonrpc": "2.0", "id": 12, "method": "session/list", "params": {"cwd": cwd}}));
    let list = read_until_id(12, &mut seen);
    let ids: Vec<&str> = list["result"]["sessions"].as_array().unwrap().iter().filter_map(|s| s["sessionId"].as_str()).collect();
    assert_eq!(ids, [sid.as_str()]);

    drop(send);
    drop(stdin); // EOF ends the connection
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0));
}

// ---------------------------------------------------------------------------------------------
// Background following of child sessions (`turn.ts` `handoff`/`followChildren`, 2.2.7)

/// One scripted answer: the first unused turn whose `matches` occurs in the request body answers it.
#[derive(Clone)]
struct Turn {
    matches: &'static str,
    delay_ms: u64,
    hang: bool,
    text: Option<String>,
    tool: Option<(&'static str, &'static str, Value)>,
}

fn text_turn(matches: &'static str, text: &str) -> Turn {
    Turn { matches, delay_ms: 0, hang: false, text: Some(text.into()), tool: None }
}

fn tool_turn(matches: &'static str, id: &'static str, name: &'static str, args: Value) -> Turn {
    Turn { matches, delay_ms: 0, hang: false, text: None, tool: Some((id, name, args)) }
}

type Script = std::sync::Arc<std::sync::Mutex<Vec<(Turn, bool)>>>;

/// A chat-completions mock answering from a script; title requests always answer "Title".
fn scripted_provider(turns: Vec<Turn>) -> (u16, Script) {
    use axum::{Router, body::Body, extract::State, response::Response, routing::post};
    let script: Script = std::sync::Arc::new(std::sync::Mutex::new(turns.into_iter().map(|t| (t, false)).collect()));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(|State(script): State<Script>, body: String| async move {
                let chunk = |delta: Value, finish: Value| json!({"id": "c", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
                let title = body.contains("title generator") || body.contains("You name conversations");
                let turn = if title {
                    Some(text_turn("", "Title"))
                } else {
                    let mut script = script.lock().unwrap();
                    script.iter_mut().find(|(t, used)| !*used && body.contains(t.matches)).map(|(t, used)| {
                        *used = true;
                        t.clone()
                    })
                };
                let turn = turn.unwrap_or_else(|| text_turn("", "(script exhausted)"));
                if turn.hang {
                    tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(turn.delay_ms)).await;
                let mut chunks = vec![];
                if let Some(text) = &turn.text {
                    chunks.push(chunk(json!({"role": "assistant", "content": text}), Value::Null));
                    chunks.push(chunk(json!({}), json!("stop")));
                }
                if let Some((id, name, args)) = &turn.tool {
                    let call = json!({"index": 0, "id": id, "type": "function", "function": {"name": name, "arguments": args.to_string()}});
                    chunks.push(chunk(json!({"role": "assistant", "tool_calls": [call]}), Value::Null));
                    chunks.push(chunk(json!({}), json!("tool_calls")));
                }
                let mut out = String::new();
                for c in chunks {
                    out.push_str(&format!("data: {c}\n\n"));
                }
                out.push_str(&format!("data: {}\n\n", json!({"id": "c", "choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}})));
                out.push_str("data: [DONE]\n\n");
                Response::builder().header("content-type", "text/event-stream").body(Body::from(out)).unwrap()
            }),
        )
        .with_state(script.clone());
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async move {
            axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), app).await.unwrap();
        })
    });
    (port, script)
}

fn used(script: &Script, index: usize) -> bool {
    script.lock().unwrap()[index].1
}

/// An ACP client on the agent's stdio: every received message is kept; permission requests are answered "once".
struct Client {
    child: std::process::Child,
    stdin: std::sync::Arc<std::sync::Mutex<Option<std::process::ChildStdin>>>,
    rx: std::sync::mpsc::Receiver<Value>,
    seen: Vec<Value>,
    next: i64,
}

impl Client {
    fn start(env: &common::Env, port: u16, extra: Value) -> Client {
        let mut config = json!({"model": "mock/m1", "providers": {"mock": {"name": "Mock", "package": "@opencode/ai/providers/openai-compatible",
            "settings": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "k"},
            "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}}}}}});
        config.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        std::fs::write(env.path("work/opencode.json"), config.to_string()).unwrap();
        let mut child = env.command(&["acp"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let stdin = std::sync::Arc::new(std::sync::Mutex::new(child.stdin.take()));
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        let writer = stdin.clone();
        std::thread::spawn(move || {
            for line in stdout.lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                if v.get("method") == Some(&json!("session/request_permission")) {
                    let reply = json!({"jsonrpc": "2.0", "id": v["id"], "result": {"outcome": {"outcome": "selected", "optionId": "once"}}});
                    if let Some(w) = writer.lock().unwrap().as_mut() {
                        let _ = w.write_all(format!("{reply}\n").as_bytes());
                        let _ = w.flush();
                    }
                }
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        Client { child, stdin, rx, seen: vec![], next: 0 }
    }

    fn send(&self, v: Value) {
        let mut w = self.stdin.lock().unwrap();
        let w = w.as_mut().unwrap();
        w.write_all(format!("{v}\n").as_bytes()).unwrap();
        w.flush().unwrap();
    }

    fn start_request(&mut self, method: &str, params: Value) -> i64 {
        self.next += 1;
        self.send(json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params}));
        self.next
    }

    /// Waits for a message matching `pred` (keeping everything received), up to `secs`.
    fn wait(&mut self, secs: u64, pred: impl Fn(&Value) -> bool) -> Option<Value> {
        if let Some(v) = self.seen.iter().find(|v| pred(v)) {
            return Some(v.clone());
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            let left = deadline.checked_duration_since(std::time::Instant::now())?;
            let v = self.rx.recv_timeout(left).ok()?;
            self.seen.push(v.clone());
            if pred(&v) {
                return Some(v);
            }
        }
    }

    fn response(&mut self, id: i64) -> Value {
        self.wait(60, |v| v.get("id") == Some(&json!(id)) && v.get("method").is_none()).unwrap_or_else(|| panic!("no response to {id}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.start_request(method, params);
        self.response(id)
    }

    /// Keeps reading for `ms` milliseconds.
    fn drain(&mut self, ms: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match self.rx.recv_timeout(left) {
                Ok(v) => self.seen.push(v),
                Err(_) => break,
            }
        }
    }

    fn open_session(&mut self, env: &common::Env) -> String {
        let init = self.request("initialize", json!({"protocolVersion": 1, "clientCapabilities": {"_meta": {"opencode/child-session-updates": true}}}));
        assert_eq!(init["result"]["agentCapabilities"]["_meta"]["opencode/child-session-updates"], true);
        let cwd = env.path("work").display().to_string();
        self.request("session/new", json!({"cwd": cwd, "mcpServers": []}))["result"]["sessionId"].as_str().unwrap().to_string()
    }

    /// Messages received after the response to request `id`.
    fn after(&self, id: i64) -> Vec<Value> {
        let at = self.seen.iter().position(|v| v.get("id") == Some(&json!(id)) && v.get("method").is_none()).unwrap();
        self.seen[at + 1..].to_vec()
    }

    /// EOF ends the connection.
    fn close(mut self) {
        self.stdin.lock().unwrap().take();
        let status = self.child.wait().unwrap();
        assert_eq!(status.code(), Some(0));
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn child_status(status: &'static str) -> impl Fn(&Value) -> bool {
    move |v| v.get("method") == Some(&json!("opencode/session/child_update")) && v.pointer("/params/status") == Some(&json!(status))
}

fn child_kinds(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter(|v| v.get("method") == Some(&json!("opencode/session/child_update")))
        .map(|v| v["params"]["status"].as_str().or(v.pointer("/params/update/sessionUpdate").and_then(Value::as_str)).unwrap_or("").to_string())
        .collect()
}

fn launch(task: &str) -> Turn {
    tool_turn("start the research", "call_launch", "subagent", json!({"agent": "general", "description": "Research notes", "prompt": task, "background": true}))
}

/// The root turn starts a background subagent and ends. The agent keeps following the child: its updates and its
/// permission ask (`read: ask`) reach the client after the prompt's response; the root's answer to the child's
/// report is not streamed.
#[test]
fn acp_follows_a_background_child_after_the_turn() {
    let env = common::Env::new();
    std::fs::write(env.path("work/notes.txt"), "note line one\n").unwrap();
    let (port, script) = scripted_provider(vec![
        launch("CHILD-A: read notes.txt"),
        text_turn("working in the background", "Launched the research."),
        Turn { delay_ms: 1500, ..tool_turn("CHILD-A: read notes", "call_notes", "read", json!({"path": "notes.txt"})) },
        text_turn("note line one", "Child report: done."),
        text_turn("Child report: done.", "ROOT-REACTION"),
    ]);
    let mut acp = Client::start(&env, port, json!({"permission": {"read": "ask"}}));
    let sid = acp.open_session(&env);
    let id = acp.start_request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "start the research"}]}));
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    assert!(acp.wait(30, child_status("completed")).is_some(), "the child's completion was not sent");
    // The root reacts to the report (the mock answered it), but that is not streamed.
    for _ in 0..100 {
        if used(&script, 4) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(used(&script, 4), "the root did not react to the report");
    acp.drain(1500);
    let after = acp.after(id);
    assert_eq!(child_kinds(&after), ["tool_call", "tool_call_update", "tool_call_update", "agent_message_chunk", "completed"], "{after:#?}");
    let ask = after.iter().find(|v| v.get("method") == Some(&json!("session/request_permission"))).expect("the child's ask reaches the client");
    assert_eq!(ask["params"]["sessionId"], json!(sid));
    let child = ask["params"]["toolCall"]["_meta"]["opencode/child-session"].clone();
    assert_eq!(child["title"], "Research notes");
    assert_eq!(ask["params"]["toolCall"]["toolCallId"], json!(format!("{}:call_notes", child["id"].as_str().unwrap())));
    let text: String = after.iter().filter_map(|v| v.pointer("/params/update/content/text").and_then(Value::as_str)).collect();
    assert_eq!(text, "Child report: done.");
    assert!(after.iter().all(|v| v.get("method") != Some(&json!("session/update"))), "{after:#?}");
    assert!(!acp.seen.iter().any(|v| v.to_string().contains("ROOT-REACTION")));
    acp.close();
}

/// A cancelled root turn hands off too: the background child outlives it and its updates follow the `cancelled`
/// response. Closing a session stops following: nothing about its child arrives after the close response.
#[test]
fn acp_follows_a_child_past_cancel_and_stops_on_close() {
    let env = common::Env::new();
    let (port, script) = scripted_provider(vec![
        launch("CHILD-B: summarize"),
        Turn { hang: true, ..text_turn("working in the background", "never") },
        Turn { delay_ms: 2000, ..text_turn("CHILD-B: summarize", "Child report: B.") },
        text_turn("Child report: B.", "Noted."),
    ]);
    let mut acp = Client::start(&env, port, json!({}));
    let sid = acp.open_session(&env);
    let id = acp.start_request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "start the research"}]}));
    assert!(acp.wait(30, child_status("running")).is_some());
    for _ in 0..300 {
        if used(&script, 1) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    acp.send(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}}));
    assert_eq!(acp.response(id)["result"]["stopReason"], "cancelled");
    assert!(acp.wait(30, child_status("completed")).is_some(), "the child's completion was not sent");
    assert_eq!(child_kinds(&acp.after(id)), ["agent_message_chunk", "completed"]);

    // A second turn whose child is still running when the session closes.
    let env2 = common::Env::new();
    let (port, script) = scripted_provider(vec![
        launch("CHILD-C: list"),
        text_turn("working in the background", "Launched."),
        Turn { delay_ms: 2500, ..text_turn("CHILD-C: list", "Child report: C.") },
        text_turn("Child report: C.", "Noted."),
    ]);
    let mut acp2 = Client::start(&env2, port, json!({}));
    let sid = acp2.open_session(&env2);
    let id = acp2.start_request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "start the research"}]}));
    assert_eq!(acp2.response(id)["result"]["stopReason"], "end_turn");
    let close = acp2.request("session/close", json!({"sessionId": sid}));
    assert_eq!(close["result"], json!({}));
    for _ in 0..100 {
        if used(&script, 2) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    acp2.drain(3500);
    let close_id = close["id"].as_i64().unwrap();
    assert_eq!(child_kinds(&acp2.after(close_id)), Vec::<String>::new());
    acp.close();
    acp2.close();
}
