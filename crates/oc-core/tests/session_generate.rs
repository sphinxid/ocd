//! `SessionGenerate.generate` (`core/test/session-generate.test.ts`, 2.2.7): the answer comes with its tokens and
//! cost, which are recorded as `session.usage.recorded` (source `generate`) and added to the session's totals
//! without touching its history. Against an OpenAI-compatible mock provider.

use std::sync::Arc;

use axum::{Router, body::Body, response::Response, routing::post};
use oc_core::{app::App, runner::Engine, session::CreateInput};
use oc_schema::types::{CacheUsage, LocationRef, TokenUsage};
use serde_json::{Value, json};

fn sse(chunks: &[Value]) -> Response {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    Response::builder().header("content-type", "text/event-stream").body(Body::from(body)).unwrap()
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generates_and_records_only_its_usage() {
    let root = std::env::temp_dir().join(format!("oc-session-generate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    for (k, v) in [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache")] {
        unsafe { std::env::set_var(k, root.join(v)) };
    }
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_TEST_HOME", root.join("home"));
    }

    let router = Router::new().route(
        "/v1/chat/completions",
        post(|_: axum::Json<Value>| async {
            sse(&[
                json!({"id": "r", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Transient answer"}}]}),
                json!({"id": "r", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                       "usage": {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110,
                                 "prompt_tokens_details": {"cached_tokens": 30}}}),
            ])
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
                "models": {"m1": {"name": "Mock One", "limit": {"context": 200000, "output": 32000},
                                  "cost": {"input": 1, "output": 2, "cache": {"read": 0.1, "write": 0.5}}}}
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

    let session = app
        .sessions
        .create(CreateInput {
            location: Some(LocationRef { directory: project.display().to_string(), workspace_id: None }),
            agent: Some("build".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    let selected = engine.select(&session).await.unwrap();
    oc_core::instruction_state::prepare(&app.bus, &selected.sources, &session).unwrap();
    let mut events = app.bus.subscribe();
    let messages_before = app.db.session_messages(&session).unwrap().len();

    let result = engine.session_generate(&session, "Summarize privately").await.unwrap();
    assert_eq!(result.text, "Transient answer");
    let tokens = TokenUsage { input: 70, output: 10, reasoning: 0, cache: CacheUsage { read: 30, write: 0 } };
    assert_eq!(result.tokens, tokens);
    assert!((result.cost - 0.000093).abs() < 1e-12, "{}", result.cost);

    let mut recorded = Vec::new();
    while let Ok(event) = events.try_recv() {
        if event.kind == "session.usage.recorded" {
            recorded.push(event.data.clone());
        }
    }
    assert_eq!(
        recorded,
        [json!({"sessionID": session, "source": "generate", "cost": result.cost,
                "tokens": {"input": 70, "output": 10, "reasoning": 0, "cache": {"read": 30, "write": 0}}})]
    );
    let info = app.sessions.get(&session).unwrap();
    assert_eq!(serde_json::to_value(&info).unwrap()["tokens"], json!({"input": 70, "output": 10, "reasoning": 0, "cache": {"read": 30, "write": 0}}));
    assert_eq!(app.db.session_messages(&session).unwrap().len(), messages_before);

    std::fs::remove_dir_all(&root).ok();
}
