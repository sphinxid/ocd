//! Session warming through the real session runtime against an OpenAI-compatible mock provider: after a turn, the
//! session gets keep-alive requests (its context plus the warming prompt) that record their usage with source
//! `generate` and leave the history alone; a subagent session is never warmed and deleting a session stops its pings.

use std::sync::Arc;
use std::time::Duration;

use axum::{Router, body::Body, extract::State, response::Response, routing::post};
use oc_core::{
    app::App,
    inbox::Scope,
    runner::Engine,
    session::{CreateInput, PromptInput},
};
use oc_schema::types::LocationRef;
use parking_lot::Mutex;
use serde_json::{Value, json};

const PROMPT: &str = "Reply pong";

fn sse(text: &str, cached: u64) -> Response {
    let chunks = [
        json!({"id": "r", "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}}]}),
        json!({"id": "r", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
               "usage": {"prompt_tokens": 100, "completion_tokens": 1, "prompt_tokens_details": {"cached_tokens": cached}}}),
    ];
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

/// (`x-opencode-session`, body) of every request.
type Requests = Arc<Mutex<Vec<(String, Value)>>>;

fn is_ping(body: &Value) -> bool {
    body["messages"].as_array().and_then(|m| m.last()).is_some_and(|m| m["content"] == json!(PROMPT))
}

/// The bodies of `session`'s pings: the last message is the warming prompt.
fn pings(requests: &Requests, session: &str) -> Vec<Value> {
    requests.lock().iter().filter(|(s, body)| s == session && is_ping(body)).map(|(_, body)| body.clone()).collect()
}

async fn until(what: &str, check: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warms_a_session_through_the_runtime() {
    let root = std::env::temp_dir().join(format!("oc-warming-engine-{}", std::process::id()));
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

    let requests: Requests = Arc::default();
    let router = Router::new()
        .route(
            "/v1/chat/completions",
            post(|State(requests): State<Requests>, headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| async move {
                let ping = is_ping(&body);
                let session = headers.get("x-opencode-session").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
                requests.lock().push((session, body));
                if ping { sse("pong", 90) } else { sse("Hello there.", 0) }
            }),
        )
        .with_state(requests.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    // A window long enough for a loaded machine to send a ping; the test stops warming itself by deleting the session.
    std::fs::write(
        project.join("opencode.json"),
        json!({
            "model": "mock/m1",
            "warming": {"prompt": PROMPT, "interval": "1 second", "duration": "10 minutes"},
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
    let location = LocationRef { directory: project.display().to_string(), workspace_id: None };
    let create = |parent: Option<String>| CreateInput {
        location: Some(location.clone()),
        agent: Some("build".into()),
        title: Some("Warm".into()),
        parent_id: parent,
        ..Default::default()
    };
    let prompt = |session: String| {
        let app = app.clone();
        async move {
            app.sessions.prompt(&session, PromptInput { text: "hi".into(), ..Default::default() }).await.unwrap();
            app.coordinator().wake(&session, Scope::Input);
            tokio::time::sleep(Duration::from_millis(50)).await;
            tokio::time::timeout(Duration::from_secs(60), app.coordinator().await_idle(&session)).await.unwrap();
        }
    };

    // A subagent's session: its turn schedules nothing (its report wakes the parent, which is warmed).
    let parent = app.sessions.create(create(None)).await.unwrap().id;
    let child = app.sessions.create(create(Some(parent.clone()))).await.unwrap().id;
    prompt(child.clone()).await;
    assert!(!app.plugins.warming.warming(&child));

    let session = app.sessions.create(create(None)).await.unwrap().id;
    let mut events = app.bus.subscribe();
    prompt(session.clone()).await;
    let history = app.db.session_messages(&session).unwrap().len();
    assert!(app.plugins.warming.warming(&session));
    until("two pings", || pings(&requests, &session).len() >= 2).await;

    // The ping is the turn's request plus the prompt: same model, same leading messages.
    let turn = requests.lock().iter().find(|(s, b)| *s == session && !is_ping(b)).map(|(_, b)| b.clone()).unwrap();
    let ping = pings(&requests, &session)[0].clone();
    assert_eq!(ping["model"], turn["model"]);
    let sent = ping["messages"].as_array().unwrap();
    assert_eq!(sent.last().unwrap(), &json!({"role": "user", "content": PROMPT}));
    assert!(sent.iter().any(|m| m["content"] == json!("hi")), "{ping}");
    assert!(sent.iter().any(|m| m["content"] == json!("Hello there.")), "{ping}");
    assert_eq!(app.db.session_messages(&session).unwrap().len(), history);

    // Deleting the session stops its pings.
    app.sessions.remove(&session).unwrap();
    until("the loop to stop", || !app.plugins.warming.warming(&session)).await;
    let count = pings(&requests, &session).len();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(pings(&requests, &session).len(), count);

    // Every ping recorded its usage with source `generate`.
    let mut recorded = 0;
    while let Ok(event) = events.try_recv() {
        if event.kind == "session.usage.recorded" && event.data["sessionID"] == json!(session) && event.data["source"] == json!("generate") {
            assert_eq!(event.data["tokens"]["cache"], json!({"read": 90, "write": 0}));
            recorded += 1;
        }
    }
    assert!(recorded >= 2, "{recorded}");
    std::fs::remove_dir_all(&root).ok();
}
