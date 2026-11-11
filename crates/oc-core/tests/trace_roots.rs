//! Trace roots (2.2.7 `SessionRunner.drain`, `{ root: true }`): each drain is one turn under a trace of its own,
//! the work it forks (the title, tools) keeps that trace, and a compaction's requests share its span.

use std::sync::Arc;
use std::time::Duration;

use axum::{Router, body::Body, http::HeaderMap, response::Response, routing::post};
use oc_core::{
    app::App,
    inbox::Scope,
    runner::Engine,
    session::{CreateInput, PromptInput},
};
use oc_schema::types::LocationRef;
use serde_json::json;

fn sse(chunks: &[serde_json::Value]) -> Response {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    Response::builder().header("content-type", "text/event-stream").body(Body::from(body)).unwrap()
}

fn text_reply(text: &str) -> Response {
    sse(&[
        json!({"id": "c", "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}}]}),
        json!({"id": "c", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
               "usage": {"prompt_tokens": 20, "completion_tokens": 4, "total_tokens": 24}}),
    ])
}

const SUMMARY: &str = "## Objective\n- Answer questions about the project\n\n## Next Move\n1. (none)";

/// One recorded request: its kind and its `b3` header split into trace, span and parent span.
#[derive(Debug, Clone)]
struct Seen {
    kind: &'static str,
    trace: String,
    span: String,
    parent: String,
    traceparent: String,
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

async fn settle(app: &Arc<App>, session: &str) {
    app.coordinator().wake(session, Scope::Input);
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::timeout(Duration::from_secs(20), app.coordinator().await_idle(session)).await.unwrap();
}

#[tokio::test]
async fn each_drain_is_a_trace_root() {
    let root = std::env::temp_dir().join(format!("oc-trace-roots-{}", std::process::id()));
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

    let seen = Arc::new(parking_lot::Mutex::new(Vec::<Seen>::new()));
    let main = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let file = project.join("hello.txt").display().to_string();
    let (s2, m2) = (seen.clone(), main.clone());
    let router = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, body: axum::Json<serde_json::Value>| {
            let raw = body.0.to_string();
            let kind = if raw.contains("You name conversations") {
                "title"
            } else if raw.contains("You MUST summarize the conversation above") || raw.contains("Update the existing checkpoint") {
                "compaction"
            } else {
                "main"
            };
            let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
            let b3 = header("b3");
            let parts: Vec<&str> = b3.split('-').collect();
            assert_eq!(parts.len(), 4, "b3: {b3}");
            assert_eq!(parts[2], "1");
            s2.lock().push(Seen {
                kind,
                trace: parts[0].into(),
                span: parts[1].into(),
                parent: parts[3].into(),
                traceparent: header("traceparent"),
            });
            let reply = match kind {
                "title" => text_reply("Title"),
                // The summary ignores the template until the reminder.
                "compaction" if raw.contains("did not fill in the required summary template") => text_reply(SUMMARY),
                "compaction" => text_reply("Sure, here is what happened."),
                _ if m2.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 => {
                    let args = json!({"path": file}).to_string();
                    sse(&[
                        json!({"id":"c1","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":args}}]}}]}),
                        json!({"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
                    ])
                }
                _ => text_reply("answer"),
            };
            async move { reply }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    std::fs::write(
        project.join("opencode.json"),
        json!({
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
    // Turn 1: a tool call, then the answer, and the title.
    app.sessions.prompt(&s.id, PromptInput { text: "what is in hello.txt?".into(), ..Default::default() }).await.unwrap();
    settle(&app, &s.id).await;
    for _ in 0..100 {
        if seen.lock().iter().any(|r| r.kind == "title") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Turn 2: one answer.
    app.sessions.prompt(&s.id, PromptInput { text: "and now?".into(), ..Default::default() }).await.unwrap();
    settle(&app, &s.id).await;
    // Turn 3: a manual compaction whose summary needs the reminder.
    app.sessions.compact(&s.id, None).await.unwrap();
    settle(&app, &s.id).await;

    let seen = seen.lock().clone();
    let of = |kind: &str| seen.iter().filter(|r| r.kind == kind).cloned().collect::<Vec<_>>();
    let (title, main, compaction) = (of("title"), of("main"), of("compaction"));
    assert_eq!((title.len(), main.len(), compaction.len()), (1, 3, 2), "{seen:#?}");
    for r in &seen {
        assert_eq!(r.traceparent, format!("00-{}-{}-01", r.trace, r.span));
        assert_ne!(r.trace, oc_util::trace::trace_id(), "a drain's request is not on the process trace");
    }
    // Turn 1's requests and its title share one trace.
    assert_eq!(main[0].trace, main[1].trace);
    assert_eq!(title[0].trace, main[0].trace);
    // Every later drain starts a new one.
    assert_ne!(main[2].trace, main[0].trace);
    assert_eq!(compaction[0].trace, compaction[1].trace);
    assert_ne!(compaction[0].trace, main[2].trace);
    assert_ne!(compaction[0].trace, main[0].trace);
    // A model request has a parent span of its own; the compaction's summary and reminder share its span.
    assert_ne!(main[0].parent, main[1].parent);
    assert_eq!(compaction[0].parent, compaction[1].parent);
    assert_ne!(compaction[0].span, compaction[1].span);
    std::fs::remove_dir_all(&root).ok();
}
