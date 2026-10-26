//! Continuing after the output token limit (`session-runner.test.ts` in 2.1): a response that finishes with
//! `length` gets a synthetic nudge and continues, three in a row fail with `output-limit`, and tool arguments cut
//! off or malformed fail with an excerpt of the input. Against a scripted OpenAI-compatible mock.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use axum::{body::Body, response::Response, routing::post, Router};
use oc_core::runner::CONTINUE_AFTER_OUTPUT_LIMIT;
use oc_core::{app::App, inbox::Scope, runner::Engine, session::{CreateInput, PromptInput}};
use oc_schema::message::{AssistantContent, ToolState};
use oc_schema::types::{FinishReason, LocationRef};
use oc_schema::Message;
use parking_lot::Mutex;
use serde_json::{json, Value};

fn sse(chunks: &[Value]) -> Response {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    Response::builder().header("content-type", "text/event-stream").body(Body::from(body)).unwrap()
}

fn finish(reason: &str) -> Value {
    json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":reason}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}})
}

fn text(t: &str, reason: &str) -> Vec<Value> {
    vec![json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","content":t}}]}), finish(reason)]
}

fn tool_args(args: &str, reason: &str) -> Vec<Value> {
    vec![
        json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":args}}]}}]}),
        finish(reason),
    ]
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

struct Harness {
    app: Arc<App>,
    _engine: Arc<Engine>,
    session: String,
    script: Arc<Mutex<VecDeque<Vec<Value>>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    root: std::path::PathBuf,
}

impl Harness {
    async fn new() -> Harness {
        let root = std::env::temp_dir().join(format!("oc-output-limit-{}", std::process::id()));
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
        let script: Arc<Mutex<VecDeque<Vec<Value>>>> = Arc::default();
        let requests: Arc<Mutex<Vec<Value>>> = Arc::default();
        let (s2, r2) = (script.clone(), requests.clone());
        let mock = Router::new().route(
            "/v1/chat/completions",
            post(move |body: axum::Json<Value>| {
                let chunks = if body.0.to_string().contains("You name conversations") {
                    text("Title", "stop")
                } else {
                    r2.lock().push(body.0.clone());
                    s2.lock().pop_front().unwrap_or_else(|| text("unscripted", "stop"))
                };
                async move { sse(&chunks) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        std::fs::write(
            project.join("opencode.json"),
            json!({
                "model": "mock/m1",
                "small_model": "mock/m1",
                "agents": {"reviewer": {"mode": "primary", "model": "mock/m2"}},
                "providers": {"mock": {
                    "name": "Mock",
                    "package": "@opencode/ai/providers/openai-compatible",
                    "settings": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "test"},
                    "models": {
                        "m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}},
                        "m2": {"name": "Mock Two", "limit": {"context": 100000, "output": 4096}}
                    }
                }}
            })
            .to_string(),
        )
        .unwrap();
        let global = oc_util::Global::acquire().unwrap();
        let db = oc_storage::Database::open(&global.database("latest"), &global.data).unwrap();
        let app = App::new(global, db, "2.1.4");
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
        Harness { app, _engine: engine, session: s.id, script, requests, root }
    }

    fn push(&self, responses: Vec<Vec<Value>>) {
        self.script.lock().extend(responses);
    }

    async fn prompt(&self, text: &str) {
        self.app.sessions.prompt(&self.session, PromptInput { text: text.into(), ..Default::default() }).await.unwrap();
        self.app.coordinator().wake(&self.session, Scope::Input);
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::timeout(Duration::from_secs(30), self.app.coordinator().await_idle(&self.session)).await.unwrap();
    }

    fn messages(&self) -> Vec<Message> {
        self.app.db.session_messages(&self.session).unwrap().into_iter().map(|(_, m)| m).collect()
    }

    fn synthetic(&self) -> usize {
        self.messages().iter().filter(|m| matches!(m, Message::Synthetic { .. })).count()
    }
}

fn last_two(body: &Value) -> (Value, Value) {
    let messages = body["messages"].as_array().unwrap();
    (messages[messages.len() - 2].clone(), messages[messages.len() - 1].clone())
}

fn content_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(""),
        _ => String::new(),
    }
}

fn tool_error(messages: &[Message]) -> Option<oc_schema::types::SessionError> {
    messages.iter().rev().find_map(|m| match m {
        Message::Assistant(a) => a.content.iter().find_map(|c| match c {
            AssistantContent::Tool { state: ToolState::Error { error, .. }, .. } => Some(error.clone()),
            _ => None,
        }),
        _ => None,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continues_after_output_limits() {
    let h = Harness::new().await;

    // "continues text after an output token limit"
    h.push(vec![text("Partial", "length"), text("Finished", "stop")]);
    h.prompt("Complete the response").await;
    {
        let requests = h.requests.lock();
        assert_eq!(requests.len(), 2);
        let (assistant, nudge) = last_two(&requests[1]);
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(content_text(&assistant), "Partial");
        assert_eq!(nudge["role"], "user");
        assert_eq!(content_text(&nudge), CONTINUE_AFTER_OUTPUT_LIMIT);
    }
    let messages = h.messages();
    let kinds: Vec<String> = messages
        .iter()
        .map(|m| match m {
            Message::User { .. } => "user".into(),
            Message::Assistant(a) => format!("assistant:{:?}", a.finish),
            Message::Synthetic { text, .. } => format!("synthetic:{}", text == CONTINUE_AFTER_OUTPUT_LIMIT),
            other => other.kind().to_string(),
        })
        .filter(|k| k != "idle")
        .collect();
    assert_eq!(
        kinds,
        vec![
            "user".to_string(),
            format!("assistant:{:?}", Some(FinishReason::Length)),
            "synthetic:true".into(),
            format!("assistant:{:?}", Some(FinishReason::Stop)),
        ],
        "{messages:#?}"
    );

    // "caps output token limit continuations and resets for new input": three `length` finishes in a row fail.
    h.requests.lock().clear();
    let before = h.synthetic();
    h.push(vec![text("Partial", "length"), text("Partial", "length"), text("Partial", "length")]);
    h.prompt("Keep going").await;
    assert_eq!(h.requests.lock().len(), 3);
    assert_eq!(h.synthetic() - before, 2);
    // The run settles as failed.
    let failed = h.messages().into_iter().rev().find_map(|m| match m {
        Message::Idle { error, .. } => error,
        _ => None,
    });
    assert_eq!(failed.as_ref().map(|e| e.kind.as_str()), Some("output-limit"), "{:#?}", h.messages());
    assert_eq!(
        failed.unwrap().message,
        "Response still truncated after two output token limit continuations"
    );
    // New input breaks the streak.
    h.push(vec![text("Partial", "length"), text("Finished", "stop")]);
    h.prompt("Try a new response").await;
    assert_eq!(h.requests.lock().len(), 5);
    assert_eq!(h.synthetic() - before, 3);

    // "settles unfinished tool input after an output limit": arguments cut off by the limit are not executed,
    // fail with the length guidance and an excerpt, and the run continues without a nudge.
    h.requests.lock().clear();
    let before = h.synthetic();
    h.push(vec![tool_args("{\"path\":\"partial", "length"), text("Recovered", "stop")]);
    h.prompt("Recover unfinished tool input").await;
    assert_eq!(h.requests.lock().len(), 2);
    assert_eq!(h.synthetic(), before);
    let error = tool_error(&h.messages()).expect("failed tool");
    assert_eq!(error.kind, "tool.input-incomplete");
    assert_eq!(
        error.message,
        "The tool call was not executed because the output token limit (stop reason: length) was reached before you could complete the arguments. Break large payloads into smaller tool calls and reissue the call with complete arguments.\n\nInput excerpt (first 16 of 16 characters):\n{\"path\":\"partial"
    );

    // "continues malformed local tool input with a bounded excerpt (tool-calls)"
    h.requests.lock().clear();
    let raw = format!("{{\"path\":\"raw-malformed-marker{}omitted-marker", "x".repeat(3000));
    h.push(vec![tool_args(&raw, "tool_calls"), text("Recovered", "stop")]);
    h.prompt("Recover malformed tool input").await;
    assert_eq!(h.requests.lock().len(), 2);
    let error = tool_error(&h.messages()).expect("failed tool");
    let units: Vec<u16> = raw.encode_utf16().collect();
    assert_eq!(error.kind, "tool.input-json");
    assert_eq!(
        error.message,
        format!(
            "The tool input could not be parsed as JSON, so the call was not executed. Reissue the call with valid JSON arguments.\n\nInput excerpt (first 2048 of {} characters):\n{}\n[truncated]",
            units.len(),
            String::from_utf16_lossy(&units[..2048])
        )
    );
    let sent = h.requests.lock()[1].to_string();
    assert!(sent.contains("raw-malformed-marker"));
    assert!(!sent.contains("omitted-marker"));

    // A session without a model uses its agent's configured model before the default (4d30794bc); a session
    // model still wins.
    for (model, expected) in [(None, "m2"), (Some("m1"), "m1")] {
        let s = h
            .app
            .sessions
            .create(CreateInput {
                location: Some(LocationRef { directory: h.root.join("project").display().to_string(), workspace_id: None }),
                agent: Some("reviewer".into()),
                model: model.map(|id| oc_schema::types::ModelRef { id: id.into(), provider_id: "mock".into(), variant: None }),
                ..Default::default()
            })
            .await
            .unwrap();
        h.requests.lock().clear();
        h.push(vec![text("Reviewed", "stop")]);
        h.app.sessions.prompt(&s.id, PromptInput { text: "Review it".into(), ..Default::default() }).await.unwrap();
        h.app.coordinator().wake(&s.id, Scope::Input);
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::timeout(Duration::from_secs(30), h.app.coordinator().await_idle(&s.id)).await.unwrap();
        assert_eq!(h.requests.lock()[0]["model"], expected);
    }

    std::fs::remove_dir_all(&h.root).ok();
}
