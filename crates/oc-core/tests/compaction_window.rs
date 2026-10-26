//! Compaction requests near the model's window (`session-compaction.test.ts` in 2.1): the whole conversation is
//! sent while the window can still fit the summary, the history before the retained tail otherwise, and the
//! output limit is fitted to the request actually sent. Runs against an OpenAI-compatible mock provider with a
//! 200k window and a 32k output limit.

use std::sync::Arc;
use std::time::Duration;

use axum::{Router, body::Body, response::Response, routing::post};
use oc_core::compaction::*;
use oc_core::{app::App, runner::Engine, session::CreateInput};
use oc_schema::Message;
use oc_schema::event::SessionEvent;
use oc_schema::message::CompactionReason;
use oc_schema::types::LocationRef;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const SUMMARY: &str = "## Objective\n- Recovered";

fn sse(chunks: &[Value]) -> Response {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(body))
        .unwrap()
}

fn text_reply(text: &str) -> Response {
    sse(&[
        json!({"id": "s", "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}}]}),
        json!({"id": "s", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
               "usage": {"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14}}),
    ])
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

fn msg(v: Value) -> Message {
    serde_json::from_value(v).unwrap()
}

fn user(text: &str) -> Message {
    msg(
        json!({"type": "user", "id": oc_schema::id::message(), "time": {"created": 1}, "text": text}),
    )
}

fn summary(text: &str, recent: Option<&str>) -> Message {
    let mut v = json!({
        "type": "compaction", "id": oc_schema::id::message(), "time": {"created": 1},
        "status": "completed", "reason": "manual", "summary": text
    });
    if let Some(r) = recent {
        v["metadata"] = json!({"recent": r});
    }
    msg(v)
}

struct Harness {
    app: Arc<App>,
    engine: Arc<Engine>,
    session: String,
    bodies: Arc<parking_lot::Mutex<Vec<Value>>>,
    root: std::path::PathBuf,
}

impl Harness {
    async fn new() -> Harness {
        let root =
            std::env::temp_dir().join(format!("oc-compaction-window-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        for (k, v) in [
            ("XDG_DATA_HOME", "data"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
        ] {
            unsafe { std::env::set_var(k, root.join(v)) };
        }
        unsafe {
            std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
            std::env::set_var("OPENCODE_TEST_HOME", root.join("home"));
        }
        let bodies = Arc::new(parking_lot::Mutex::new(Vec::<Value>::new()));
        let seen = bodies.clone();
        let router = Router::new().route(
            "/v1/chat/completions",
            post(move |body: axum::Json<Value>| {
                seen.lock().push(body.0.clone());
                async move { text_reply(SUMMARY) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        std::fs::write(
            project.join("opencode.json"),
            json!({
                "model": "mock/summary-model",
                // Keep the system prompt and tool definitions small, as upstream's test context does, so the
                // conversation is what the sizes below measure.
                "agents": {"build": {"system": "You are a test agent."}},
                "permissions": [{"action": "*", "resource": "*", "effect": "deny"}],
                "providers": {"mock": {
                    "name": "Mock",
                    "package": "@opencode/ai/providers/openai-compatible",
                    "settings": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "test"},
                    "models": {"summary-model": {"name": "Summary", "limit": {"context": 200000, "output": 32000}}}
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
            ripgrep: Arc::new(oc_tools::ripgrep::Ripgrep::new(
                app.global.bin.clone(),
                reqwest::Client::new(),
            )),
            shell: oc_tools::shell::service::ShellService::new(app.global.data.clone(), None),
            websearch: Arc::new(oc_tools::websearch::WebSearch::new(
                Arc::new(NoStore),
                reqwest::Client::new(),
            )),
            http: reqwest::Client::new(),
        };
        let engine = Engine::new(app.clone(), &services);
        engine.install();
        let s = app
            .sessions
            .create(CreateInput {
                location: Some(LocationRef {
                    directory: project.display().to_string(),
                    workspace_id: None,
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        let sel = engine.select(&s.id).await.unwrap();
        oc_core::instruction_state::prepare(&app.bus, &sel.sources, &s.id).unwrap();
        Harness {
            app,
            engine,
            session: s.id,
            bodies,
            root,
        }
    }

    /// A manual compaction over `messages` as the loaded history.
    async fn compact_manually(&self, messages: Vec<Message>) -> CompactOutcome {
        self.bodies.lock().clear();
        let input_id = oc_schema::id::message();
        self.app
            .bus
            .commit_one(SessionEvent::CompactionStarted {
                session_id: self.session.clone(),
                reason: CompactionReason::Manual,
                input_id: Some(input_id.clone()),
            })
            .unwrap();
        let mut loaded = self
            .engine
            .load(self.engine.select(&self.session).await.unwrap())
            .await
            .unwrap();
        loaded.messages = messages;
        tokio::time::timeout(
            Duration::from_secs(30),
            self.engine.compact(
                &loaded,
                Trigger::Manual { input_id },
                CancellationToken::new(),
            ),
        )
        .await
        .unwrap()
        .unwrap()
    }

    fn requests(&self) -> Vec<Value> {
        self.bodies.lock().clone()
    }
}

/// The chat messages after the system prompt.
fn conversation(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] != "system")
        .cloned()
        .collect()
}

fn text(message: &Value) -> String {
    match &message["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

#[tokio::test]
async fn compaction_requests_near_the_window() {
    let h = Harness::new().await;

    // "summary updates include the retained tail in compaction hooks and the model request"
    let previous = summary(
        "## Objective\n- Previous objective",
        Some("[User]: Previous exact request"),
    );
    let latest = user("The latest request supersedes the previous objective.");
    let latest_id = latest.id().to_string();
    assert_eq!(
        h.compact_manually(vec![previous, latest]).await,
        CompactOutcome::Completed
    );
    let requests = h.requests();
    assert_eq!(requests.len(), 1);
    let messages = conversation(&requests[0]);
    assert_eq!(messages.len(), 3, "{messages:#?}");
    assert!(text(&messages[0]).contains("<conversation-checkpoint>"));
    assert!(text(&messages[0]).contains("[User]: Previous exact request"));
    assert_eq!(
        text(&messages[1]),
        "The latest request supersedes the previous objective."
    );
    assert_eq!(text(&messages[2]), build_prompt(true, false));
    let Message::Compaction(c) = h
        .app
        .db
        .session_messages(&h.session)
        .unwrap()
        .into_iter()
        .map(|(_, m)| m)
        .last()
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        c.metadata.as_ref().and_then(|m| m.get("keepID")),
        Some(&json!(latest_id))
    );
    // 200k window, 170k ceiling: room for 30k of the 32k output.
    assert_eq!(requests[0]["max_completion_tokens"], 30_000);

    // "summary compaction past the ceiling keeps the conversation unchanged while the window fits": about 182k
    // estimated tokens, past the 170k ceiling but within the 184k left after reserving room for the summary.
    let first = format!("first {}", "x".repeat(364_000));
    let second = format!("second {}", "x".repeat(364_000));
    assert_eq!(
        h.compact_manually(vec![user(&first), user(&second)]).await,
        CompactOutcome::Completed
    );
    let requests = h.requests();
    assert_eq!(requests.len(), 1);
    let messages = conversation(&requests[0]);
    assert_eq!(messages.len(), 3);
    assert_eq!(text(&messages[0]), first);
    assert!(requests[0]["max_completion_tokens"].as_u64().unwrap() < 20_000);

    // "summary compaction sends the history before the tail unchanged when the whole conversation does not fit":
    // about 190k in total, past the 184k left for the summary; the 160k before the retained 30k fits the ceiling.
    let older = format!("older {}", "x".repeat(640_000));
    let newest = format!("latest {}", "x".repeat(120_000));
    assert_eq!(
        h.compact_manually(vec![user(&older), user(&newest)]).await,
        CompactOutcome::Completed
    );
    let requests = h.requests();
    assert_eq!(requests.len(), 1);
    let messages = conversation(&requests[0]);
    assert_eq!(messages.len(), 2);
    assert_eq!(text(&messages[0]), older);
    assert_eq!(text(&messages[1]), build_prompt(false, false));

    // "compaction lowers the output limit when it sends a request it could not shrink": a previous summary of
    // about 250k tokens, past the 200k window, always stays whole.
    let huge = summary(&"x".repeat(1_000_000), None);
    assert_eq!(
        h.compact_manually(vec![huge, user("Next question")]).await,
        CompactOutcome::Completed
    );
    let requests = h.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["max_completion_tokens"], 1_024);

    // "compaction keeps the output limit for a request that fits its target": about 160k, under the 170k target.
    assert_eq!(
        h.compact_manually(vec![user(&"x".repeat(640_000))]).await,
        CompactOutcome::Completed
    );
    let requests = h.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["max_completion_tokens"], 30_000);

    // Automatic compaction publishes `compaction.started` before anything else, so a compaction that has nothing to
    // compact still shows as one that started and failed.
    let mut loaded = h
        .engine
        .load(h.engine.select(&h.session).await.unwrap())
        .await
        .unwrap();
    loaded.messages = vec![];
    let out = h
        .engine
        .compact(&loaded, Trigger::Overflow, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        out,
        CompactOutcome::Failed(oc_schema::types::SessionError::new(
            "compaction.unavailable",
            "Nothing to compact yet"
        ))
    );
    let last = h
        .app
        .db
        .session_messages(&h.session)
        .unwrap()
        .into_iter()
        .map(|(_, m)| m)
        .last()
        .unwrap();
    assert!(
        matches!(&last, Message::Compaction(c) if matches!(c.status, oc_schema::message::CompactionStatus::Failed { .. })),
        "{last:#?}"
    );

    std::fs::remove_dir_all(&h.root).ok();
}
