//! End-to-end: prompt → runner → OpenAI-compatible mock provider → tool call → final answer.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::{body::Body, response::Response, routing::post, Router};
use oc_core::{app::App, inbox::Scope, runner::Engine, session::{CreateInput, PromptInput}};
use oc_schema::message::{AssistantContent, ToolState};
use oc_schema::types::{FinishReason, LocationRef, Outcome};
use oc_schema::Message;

fn sse(chunks: &[serde_json::Value]) -> Response {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    Response::builder().header("content-type", "text/event-stream").body(Body::from(body)).unwrap()
}

#[tokio::test]
async fn prompt_runs_tool_then_answers() {
    let root = std::env::temp_dir().join(format!("oc-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("hello.txt"), "hello from file\n").unwrap();
    for (k, v) in [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache")] {
        unsafe { std::env::set_var(k, root.join(v)) };
    }
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_TEST_HOME", root.join("home"));
    }

    // mock provider
    let calls = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(parking_lot::Mutex::new(Vec::<serde_json::Value>::new()));
    let file = project.join("hello.txt").display().to_string();
    let (c2, b2) = (calls.clone(), bodies.clone());
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::Json<serde_json::Value>| {
            let is_title = body.0.to_string().contains("You are a title generator");
            let n = if is_title { usize::MAX } else { c2.fetch_add(1, Ordering::SeqCst) };
            if !is_title {
                b2.lock().push(body.0.clone());
            }
            let file = file.clone();
            async move {
                if is_title {
                    return sse(&[
                        serde_json::json!({"id":"t","choices":[{"index":0,"delta":{"role":"assistant","content":"\nhello.txt contents\nextra"}}]}),
                        serde_json::json!({"id":"t","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
                    ]);
                }
                if n == 0 {
                    let args = serde_json::json!({"path": file}).to_string();
                    sse(&[
                        serde_json::json!({"id":"c1","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":""}}]}}]}),
                        serde_json::json!({"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":args}}]}}]}),
                        serde_json::json!({"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
                    ])
                } else {
                    sse(&[
                        serde_json::json!({"id":"c2","choices":[{"index":0,"delta":{"role":"assistant","content":"The file says "}}]}),
                        serde_json::json!({"id":"c2","choices":[{"index":0,"delta":{"content":"hello."}}]}),
                        serde_json::json!({"id":"c2","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":20,"completion_tokens":4,"total_tokens":24}}),
                    ])
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    std::fs::write(
        project.join("opencode.json"),
        serde_json::json!({
            "model": "mock/m1",
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

    let global = oc_util::Global::acquire().unwrap();
    let db = oc_storage::Database::open(&global.database("latest"), &global.data).unwrap();
    let app = App::new(global, db, "2.0.26");
    let services = oc_tools::tools::Services {
        formatter: Arc::new(oc_tools::formatter::Formatter::new()),
        ripgrep: Arc::new(oc_tools::ripgrep::Ripgrep::new(app.global.bin.clone(), reqwest::Client::new())),
        shell: oc_tools::shell::service::ShellService::new(app.global.data.clone(), None),
        websearch: Arc::new(oc_tools::websearch::WebSearch::new(Arc::new(NoStore), reqwest::Client::new())),
        http: reqwest::Client::new(),
    };
    let engine = Engine::new(app.clone(), &services);
    engine.install();

    let s = app
        .sessions
        .create(CreateInput { location: Some(LocationRef { directory: project.display().to_string(), workspace_id: None }), ..Default::default() })
        .await
        .unwrap();
    app.sessions.prompt(&s.id, PromptInput { text: "what is in hello.txt?".into(), ..Default::default() }).await.unwrap();
    app.coordinator().wake(&s.id, Scope::Input);
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::timeout(Duration::from_secs(20), app.coordinator().await_idle(&s.id)).await.unwrap();

    let msgs: Vec<Message> = app.db.session_messages(&s.id).unwrap().into_iter().map(|(_, m)| m).collect();
    let kinds: Vec<&str> = msgs.iter().map(|m| m.kind()).collect();
    assert_eq!(kinds, vec!["user", "assistant", "assistant", "idle"], "{msgs:#?}");
    let Message::Assistant(first) = &msgs[1] else { panic!() };
    assert_eq!(first.finish, Some(FinishReason::ToolCalls));
    match &first.content[0] {
        AssistantContent::Tool { name, state: ToolState::Completed { content, .. }, .. } => {
            assert_eq!(name, "read");
            let text = serde_json::to_string(content).unwrap();
            assert!(text.contains("hello from file"), "{text}");
        }
        other => panic!("{other:?}"),
    }
    let Message::Assistant(second) = &msgs[2] else { panic!() };
    assert!(matches!(&second.content[0], AssistantContent::Text { text, .. } if text == "The file says hello."));
    let info = app.sessions.get(&s.id).unwrap();
    assert_eq!(info.outcome, Some(Outcome::Succeeded));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let info = app.sessions.get(&s.id).unwrap();
    assert_eq!(info.title.as_deref(), Some("hello.txt contents"));
    assert_eq!(info.tokens.input + info.tokens.cache.read, 33);
    // the second request carried the tool result back
    let second_body = bodies.lock()[1].to_string();
    assert!(second_body.contains("hello from file"), "{second_body}");
    assert!(second_body.contains("\"role\":\"tool\""));
    std::fs::remove_dir_all(&root).ok();
}

struct NoStore;
#[async_trait::async_trait]
impl oc_tools::websearch::WebSearchStore for NoStore {
    async fn kv_get(&self, _: &str) -> Option<serde_json::Value> {
        None
    }
    async fn kv_set(&self, _: &str, _: serde_json::Value) {}
    async fn kv_remove(&self, _: &str) {}
    async fn api_key(&self, _: &str) -> Option<String> {
        None
    }
}
