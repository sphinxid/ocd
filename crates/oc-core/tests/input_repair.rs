//! `opencode.tool.input.repair`: tool inputs are repaired against the tool's schema before the plugin hooks
//! and validation run, and a location can remove the plugin with `-opencode.tool.input.repair`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::{Router, body::Body, response::Response, routing::post};
use oc_core::{
    app::App,
    inbox::Scope,
    runner::Engine,
    session::{CreateInput, PromptInput},
};
use oc_schema::Message;
use oc_schema::message::{AssistantContent, ToolState};
use oc_schema::types::LocationRef;
use serde_json::{Value, json};

fn sse(chunks: &[Value]) -> Response {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    Response::builder().header("content-type", "text/event-stream").body(Body::from(body)).unwrap()
}

fn tool_call(id: &str, name: &str, arguments: &str) -> Response {
    sse(&[
        json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":""}}]}}]}),
        json!({"id":"c","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":arguments}}]}}]}),
        json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
    ])
}

fn text(content: &str) -> Response {
    sse(&[
        json!({"id":"t","choices":[{"index":0,"delta":{"role":"assistant","content":content}}]}),
        json!({"id":"t","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
    ])
}

/// The tool parts of a session: (input as the model sent it, completed content text or failure message).
fn tool_parts(messages: &[Message]) -> Vec<(Value, String)> {
    let mut out = Vec::new();
    for message in messages {
        let Message::Assistant(assistant) = message else { continue };
        for part in &assistant.content {
            let AssistantContent::Tool { state, .. } = part else { continue };
            let (input, result) = match state {
                ToolState::Completed { input, content, .. } => (input, serde_json::to_string(content).unwrap()),
                ToolState::Error { input, error, .. } => (input, error.message.clone()),
                other => panic!("{other:?}"),
            };
            out.push((serde_json::to_value(input).unwrap(), result));
        }
    }
    out
}

#[tokio::test]
async fn repairs_tool_input_unless_removed() {
    let root = std::env::temp_dir().join(format!("oc-input-repair-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for (k, v) in [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache")] {
        unsafe { std::env::set_var(k, root.join(v)) };
    }
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_TEST_HOME", root.join("home"));
    }

    // Each prompt runs: read with coercible strings and an extra key, read with an invalid path, a call to
    // `bash` (run as `shell`, #60687), then text.
    let calls = Arc::new(AtomicUsize::new(0));
    let file = Arc::new(parking_lot::Mutex::new(String::new()));
    let (c2, f2) = (calls.clone(), file.clone());
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::Json<Value>| {
            let is_title = body.0.to_string().contains("You name conversations");
            let n = if is_title { usize::MAX } else { c2.fetch_add(1, Ordering::SeqCst) % 4 };
            let file = f2.lock().clone();
            async move {
                match n {
                    usize::MAX => text("Repair"),
                    0 => tool_call("call_ok", "read", &json!({"path": file, "offset": "2", "limit": "1", "extra": true}).to_string()),
                    1 => tool_call("call_bad", "read", &json!({"path": 5, "limit": "3", "bogus": 1}).to_string()),
                    2 => tool_call("call_bash", "bash", &json!({"command": "echo aliased", "timeout": "5000"}).to_string()),
                    _ => text("Done."),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let global = oc_util::Global::acquire().unwrap();
    let db = oc_storage::Database::open(&global.database("latest"), &global.data).unwrap();
    let app = App::new(global, db, "2.2.2");
    let services = oc_tools::tools::Services {
        formatter: Arc::new(oc_tools::formatter::Formatter::new()),
        ripgrep: Arc::new(oc_tools::ripgrep::Ripgrep::new(app.global.bin.clone(), reqwest::Client::new())),
        shell: oc_tools::shell::service::ShellService::new(app.global.data.clone(), None),
        websearch: Arc::new(oc_tools::websearch::WebSearch::new(Arc::new(NoStore), reqwest::Client::new())),
        http: reqwest::Client::new(),
    };
    let engine = Engine::new(app.clone(), &services);
    engine.install();

    let mut results = Vec::new();
    for (name, plugins) in [("enabled", json!([])), ("disabled", json!(["-opencode.tool.input.repair"]))] {
        let project = root.join(name);
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        *file.lock() = project.join("a.txt").display().to_string();
        std::fs::write(
            project.join("opencode.json"),
            json!({
                "model": "mock/m1",
                "plugins": plugins,
                "providers": {"mock": {
                    "name": "Mock",
                    "package": "@opencode/ai/providers/openai-compatible",
                    "settings": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "test"},
                    "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}}}
                }}
            })
            .to_string(),
        )
        .unwrap();
        let s = app
            .sessions
            .create(CreateInput { location: Some(LocationRef { directory: project.display().to_string(), workspace_id: None }), ..Default::default() })
            .await
            .unwrap();
        app.sessions.prompt(&s.id, PromptInput { text: "read it".into(), ..Default::default() }).await.unwrap();
        app.coordinator().wake(&s.id, Scope::Input);
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::timeout(Duration::from_secs(30), app.coordinator().await_idle(&s.id)).await.unwrap();
        let messages: Vec<Message> = app.db.session_messages(&s.id).unwrap().into_iter().map(|(_, m)| m).collect();
        results.push((file.lock().clone(), tool_parts(&messages)));
    }

    // Repaired: the read runs with offset 2 / limit 1, and the stored inputs stay as the model sent them.
    let (path, parts) = &results[0];
    assert_eq!(parts.len(), 3, "{parts:#?}");
    assert_eq!(parts[0].0, json!({"path": path, "offset": "2", "limit": "1", "extra": true}));
    assert!(parts[0].1.contains("2: two") && !parts[0].1.contains("one"), "{}", parts[0].1);
    assert_eq!(parts[1].0, json!({"path": 5, "limit": "3", "bogus": 1}));
    assert_eq!(
        parts[1].1,
        "Invalid arguments for tool \"read\":\n- path: Expected string\n\nArguments provided:\n{\n  \"path\": 5,\n  \"limit\": 3\n}\n\nUpdate the arguments and call the tool again."
    );

    // `bash` ran as `shell` with the repaired timeout, and the result says so.
    assert_eq!(parts[2].0, json!({"command": "echo aliased", "timeout": "5000"}));
    assert!(parts[2].1.contains("aliased"), "{}", parts[2].1);
    let note = json!({"type": "text", "text": "There is no \"bash\" tool, so this ran as \"shell\". Call \"shell\" directly."});
    assert!(parts[2].1.ends_with(&format!(",{note}]")), "{}", parts[2].1);

    // Removed: the same calls fail validation with the model's own arguments.
    let (path, parts) = &results[1];
    assert_eq!(parts.len(), 3, "{parts:#?}");
    assert!(parts[0].1.starts_with("Invalid arguments for tool \"read\":\n- offset: Expected number\n- limit: Expected number\n"), "{}", parts[0].1);
    // The path as a JSON string (escaped backslashes on Windows).
    assert!(parts[0].1.contains(&format!("\"path\": {},\n  \"offset\": \"2\",\n  \"limit\": \"1\",\n  \"extra\": true", json!(path))), "{}", parts[0].1);
    assert!(parts[1].1.contains("\"bogus\": 1"), "{}", parts[1].1);
    // Without the repair plugin there is no alias either.
    assert_eq!(parts[2].1, "No tool named \"bash\" is currently available. Please use a tool from the available tool list.");
    std::fs::remove_dir_all(&root).ok();
}

struct NoStore;
#[async_trait::async_trait]
impl oc_tools::websearch::WebSearchStore for NoStore {
    async fn kv_get(&self, _: &str) -> Option<Value> {
        None
    }
    async fn kv_set(&self, _: &str, _: Value) {}
    async fn kv_remove(&self, _: &str) {}
    async fn api_key(&self, _: &str) -> Option<String> {
        None
    }
}
