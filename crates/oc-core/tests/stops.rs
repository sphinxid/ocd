//! Ways a run could end before the model is done, against a scripted OpenAI-compatible mock:
//! a panicking tool fails only its call, an interrupt cuts a long retry wait short, and a location shut down
//! while a permission ask is pending continues the run (upstream `isLocationClosed` → `needsContinuation`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::{body::Body, response::Response, routing::post, Router};
use oc_core::coordinator::Reason;
use oc_core::step::{self, Outcome, Progress, ToolCall, ToolRun, ToolRunner};
use oc_core::{app::App, inbox::Scope, runner::Engine, session::{CreateInput, PromptInput}};
use oc_schema::message::{AssistantContent, ToolState};
use oc_schema::types::{LocationRef, Outcome as Idle};
use oc_schema::Message;
use parking_lot::Mutex;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn sse(chunks: &[serde_json::Value]) -> Response {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    Response::builder().header("content-type", "text/event-stream").body(Body::from(body)).unwrap()
}

fn tool_call(name: &str, args: serde_json::Value) -> Response {
    sse(&[
        json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]}),
        json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
    ])
}

fn text(t: &str) -> Response {
    sse(&[
        json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","content":t}}]}),
        json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
    ])
}

/// A tool runner whose tool panics, like a defect in a tool implementation.
struct PanickingTools;

#[async_trait]
impl ToolRunner for PanickingTools {
    async fn run(&self, _call: ToolCall, _progress: Progress, _cancel: CancellationToken) -> ToolRun {
        panic!("byte index 3 is not a char boundary");
    }
}

fn messages(app: &App, session: &str) -> Vec<Message> {
    app.db.session_messages(session).unwrap().into_iter().map(|(_, m)| m).collect()
}

fn tool_state<'a>(msgs: &'a [Message], call: &str) -> Option<&'a ToolState> {
    msgs.iter().find_map(|m| match m {
        Message::Assistant(a) => a.content.iter().find_map(|c| match c {
            AssistantContent::Tool { id, state, .. } if id == call => Some(state),
            _ => None,
        }),
        _ => None,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_do_not_stop_early() {
    let root = std::env::temp_dir().join(format!("oc-stops-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join(".env"), "SECRET=1\n").unwrap();
    for (k, v) in [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache")] {
        unsafe { std::env::set_var(k, root.join(v)) };
    }
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_TEST_HOME", root.join("home"));
    }

    // Requests are answered per case (the session's `x-opencode-session-id`), in order; titles get a title.
    let counts: Arc<Mutex<HashMap<&'static str, usize>>> = Arc::default();
    let cases: Arc<Mutex<HashMap<String, &'static str>>> = Arc::default();
    let env_file = project.join(".env").display().to_string();
    let (c2, k2) = (counts.clone(), cases.clone());
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: axum::http::HeaderMap, body: axum::Json<serde_json::Value>| {
            let title = body.0.to_string().contains("title generator");
            let session = headers.get("x-opencode-session-id").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
            let case = if title { None } else { k2.lock().get(&session).copied() };
            let n = case.map(|c| {
                let mut counts = c2.lock();
                let n = counts.entry(c).or_default();
                *n += 1;
                *n
            });
            let env_file = env_file.clone();
            async move {
                match (case, n) {
                    (None, _) => text("Title"),
                    (Some("case-panic"), _) => tool_call("read", json!({"path": "x"})),
                    (Some("case-retry"), _) => Response::builder()
                        .status(429)
                        .header("content-type", "application/json")
                        .header("retry-after", "60")
                        .body(Body::from(r#"{"error":{"message":"slow down","type":"rate_limit"}}"#))
                        .unwrap(),
                    (Some(_), Some(1)) => tool_call("read", json!({"path": env_file})),
                    (Some(_), _) => text("after close"),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
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
    let location = LocationRef { directory: project.display().to_string(), workspace_id: None };
    let session = |title: &str| {
        let app = app.clone();
        let location = location.clone();
        let title = title.to_string();
        async move {
            app.sessions.create(CreateInput { location: Some(location), title: Some(title), ..Default::default() }).await.unwrap()
        }
    };

    // 1. A tool that panics fails its own call (the panic message as an `unknown` error) and the step still asks for
    //    a continuation, like a tool fiber that dies upstream; it used to read as an interrupted step.
    let s = session("panic").await;
    cases.lock().insert(s.id.clone(), "case-panic");
    let selected = engine.select(&s.id).await.unwrap();
    oc_core::instruction_state::prepare(&app.bus, &selected.sources, &s.id).unwrap();
    let loaded = engine.load(selected).await.unwrap();
    let (request, _) = engine.request(&loaded, false);
    let mut policy = oc_core::retry::Policy::default();
    let outcome = step::attempt(step::Input {
        bus: app.bus.clone(),
        client: engine.client.clone(),
        session_id: s.id.clone(),
        assistant_message_id: oc_schema::id::message(),
        agent: loaded.sel.agent.id.clone(),
        model: loaded.resolved.model_ref(),
        provider_metadata_key: "mock".into(),
        cost: vec![],
        request,
        tool_choice_none: false,
        tools: Arc::new(PanickingTools),
        snapshots: None,
        recover_continuation: true,
        retry: &mut policy,
        cancel: CancellationToken::new(),
        location_closed: Arc::new(|| false),
        retry_hook: None,
        http_hook: None,
        web_socket: None,
        recover_overflow: Box::new(|| Box::pin(async { false })),
    })
    .await;
    let shown = match &outcome {
        Ok(Outcome::Completed { needs_continuation }) => format!("completed {needs_continuation}"),
        Ok(Outcome::Retry { error, .. }) => format!("retry {error:?}"),
        Ok(Outcome::Continue { error, .. }) => format!("continue {error:?}"),
        Ok(_) => "other".into(),
        Err(e) => format!("error {e}"),
    };
    assert_eq!(shown, "completed true", "{:#?}", messages(&app, &s.id));
    let msgs = messages(&app, &s.id);
    match tool_state(&msgs, "call_1") {
        Some(ToolState::Error { error, .. }) => {
            assert_eq!(error.kind, "unknown");
            assert_eq!(error.message, "byte index 3 is not a char boundary");
        }
        other => panic!("{other:?}"),
    }

    // 2. An interrupt during a long provider-requested retry wait (Retry-After: 60) ends the run at once.
    let s = session("retry").await;
    cases.lock().insert(s.id.clone(), "case-retry");
    app.sessions.prompt(&s.id, PromptInput { text: "case-retry".into(), ..Default::default() }).await.unwrap();
    app.coordinator().wake(&s.id, Scope::Input);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while counts.lock().get("case-retry").is_none() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let started = tokio::time::Instant::now();
    assert!(app.coordinator().interrupt(&s.id, Some(Reason::User), true).await);
    assert!(started.elapsed() < Duration::from_secs(5), "interrupt waited out the retry delay: {:?}", started.elapsed());
    assert_eq!(app.sessions.get(&s.id).unwrap().outcome, Some(Idle::Interrupted));
    assert_eq!(counts.lock().get("case-retry"), Some(&1));

    // 3. The location shuts down (`location.reload`, `debug.location.evict`, `vcs.init`) while a permission ask is
    //    pending: the cancelled ask is not a user decline, so the run continues on a fresh load of the location.
    let s = session("close").await;
    cases.lock().insert(s.id.clone(), "case-close");
    app.sessions.prompt(&s.id, PromptInput { text: "case-close".into(), ..Default::default() }).await.unwrap();
    app.coordinator().wake(&s.id, Scope::Input);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while app.permissions.list().is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(app.permissions.list().len(), 1, "the .env read should ask");
    engine.close_location(&project.display().to_string()).await;
    tokio::time::timeout(Duration::from_secs(20), app.coordinator().await_idle(&s.id)).await.unwrap();
    let msgs = messages(&app, &s.id);
    match tool_state(&msgs, "call_1") {
        Some(ToolState::Error { error, .. }) => {
            assert_eq!(error.message, "Interaction cancelled because the location shut down");
        }
        other => panic!("{other:?}"),
    }
    let last_text = msgs.iter().rev().find_map(|m| match m {
        Message::Assistant(a) => a.content.iter().find_map(|c| match c {
            AssistantContent::Text { text, .. } => Some(text.clone()),
            _ => None,
        }),
        _ => None,
    });
    assert_eq!(last_text.as_deref(), Some("after close"));
    assert_eq!(app.sessions.get(&s.id).unwrap().outcome, Some(Idle::Succeeded));

    std::fs::remove_dir_all(&root).ok();
}

struct NoStore;
#[async_trait]
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
