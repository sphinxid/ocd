//! Compaction sized from the provider's reported token count (#59441) and background summaries
//! (`experimental.background_compaction`): upstream's `test/session-compaction.test.ts` cases for `measured` and
//! `precompute`, against an OpenAI-compatible mock. One test function: the cases share one engine (the XDG
//! environment is process-wide), each on its own session.

use std::sync::Arc;
use std::time::Duration;

use axum::{body::Body, response::Response, routing::post, Router};
use oc_core::compaction::{build_prompt, CompactOutcome, Trigger};
use oc_core::runner::{Engine, Loaded};
use oc_core::{app::App, session::CreateInput};
use oc_schema::event::SessionEvent;
use oc_schema::message::CompactionReason;
use oc_schema::types::LocationRef;
use oc_schema::Message;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const SUMMARY: &str = "## Objective\n- manual summary";

#[derive(Default)]
struct Mock {
    requests: Vec<Value>,
    /// Requests whose body contains this text are rejected as too long.
    reject: Option<String>,
    /// Requests wait for this before answering.
    gate: Option<Arc<tokio::sync::Notify>>,
    /// Requests signal this, then fail with a non-retryable error.
    fail: Option<Arc<tokio::sync::Notify>>,
}

fn summary_reply() -> Response {
    let chunks = [
        json!({"id": "s", "choices": [{"index": 0, "delta": {"role": "assistant", "content": SUMMARY}}]}),
        json!({"id": "s", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
               "usage": {"prompt_tokens": 15, "completion_tokens": 6, "total_tokens": 21,
                         "prompt_tokens_details": {"cached_tokens": 3}, "completion_tokens_details": {"reasoning_tokens": 2}}}),
    ];
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    Response::builder().header("content-type", "text/event-stream").body(Body::from(body)).unwrap()
}

fn error(status: u16, code: &str, message: &str) -> Response {
    let body = json!({"error": {"message": message, "type": "invalid_request_error", "code": code}});
    Response::builder().status(status).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap()
}

fn msg(v: Value) -> Message {
    serde_json::from_value(v).unwrap()
}

fn text_user(text: &str) -> Message {
    msg(json!({"type": "user", "id": oc_schema::id::message(), "time": {"created": 0}, "text": text}))
}

fn measured(text: &str, input: u64) -> Message {
    msg(json!({
        "type": "assistant", "id": oc_schema::id::message(), "agent": "build",
        "model": {"id": "m1", "providerID": "mock"},
        "content": [{"type": "text", "text": text}],
        "tokens": {"input": input, "output": 100, "reasoning": 0, "cache": {"read": 0, "write": 0}},
        "time": {"created": 0, "completed": 1}
    }))
}

const WINDOW: u64 = 200_000;
const BUFFER: u64 = WINDOW / 8;
const CEILING: u64 = WINDOW - BUFFER;
/// Past precompute's trigger, below the compaction ceiling.
const INPUT: u64 = CEILING * 9 / 10;

fn token_text(tokens: u64) -> String {
    "text".repeat(tokens as usize)
}

fn old_reasoning(input: u64) -> Message {
    msg(json!({
        "type": "assistant", "id": oc_schema::id::message(), "agent": "build",
        "model": {"id": "m1", "providerID": "mock"},
        "content": [{"type": "reasoning", "text": token_text(WINDOW * 3)}],
        "tokens": {"input": input, "output": 100, "reasoning": 0, "cache": {"read": 0, "write": 0}},
        "time": {"created": 0, "completed": 1}
    }))
}

/// A 200k window compacts at 170k and starts a background summary at 85% of that, 144.5k. `near` ends past that
/// point; `due` continues it past the threshold.
struct History {
    near: Vec<Message>,
    due: Vec<Message>,
    anchor: Message,
}

fn precompute_history() -> History {
    let mut near = vec![text_user("First question"), measured("First answer", 80_000), text_user("Second question")];
    let anchor = measured("Second answer", 156_000);
    near.push(anchor.clone());
    let mut due = near.clone();
    due.extend([text_user("Third question"), measured("Third answer", 182_000)]);
    History { near, due, anchor }
}

struct Harness {
    app: Arc<App>,
    engine: Arc<Engine>,
    mock: Arc<Mutex<Mock>>,
}

impl Harness {
    /// A fresh session in `project` and its loaded context with `messages` in place of its history.
    async fn context(&self, project: &std::path::Path, messages: Vec<Message>) -> Loaded {
        let s = self
            .app
            .sessions
            .create(CreateInput {
                location: Some(LocationRef { directory: project.display().to_string(), workspace_id: None }),
                title: Some("Compaction".into()),
                agent: Some("build".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let selected = self.engine.select(&s.id).await.unwrap();
        oc_core::instruction_state::prepare(&self.app.bus, &selected.sources, &s.id).unwrap();
        let mut loaded = self.engine.load(selected).await.unwrap();
        loaded.messages = messages;
        loaded
    }

    fn with(&self, base: &Loaded, messages: Vec<Message>) -> Loaded {
        Loaded { messages, ..base.clone() }
    }

    async fn manual(&self, loaded: &Loaded) -> CompactOutcome {
        let input_id = oc_schema::id::message();
        self.app
            .bus
            .commit_one(SessionEvent::CompactionStarted {
                session_id: loaded.sel.session.id.clone(),
                reason: CompactionReason::Manual,
                input_id: Some(input_id.clone()),
            })
            .unwrap();
        self.engine.compact(loaded, Trigger::Manual { input_id }, CancellationToken::new()).await.unwrap()
    }

    async fn auto(&self, loaded: &Loaded) -> CompactOutcome {
        self.engine.compact(loaded, Trigger::Auto, CancellationToken::new()).await.unwrap()
    }

    fn reset(&self) {
        *self.mock.lock() = Mock::default();
    }

    fn requests(&self) -> Vec<Value> {
        self.mock.lock().requests.clone()
    }

}

fn last_text(request: &Value) -> String {
    let last = request["messages"].as_array().unwrap().last().unwrap();
    last["content"].as_str().map(str::to_string).unwrap_or_else(|| last["content"][0]["text"].as_str().unwrap_or_default().to_string())
}

fn write_config(dir: &std::path::Path, port: u16, extra: Value) {
    std::fs::create_dir_all(dir).unwrap();
    let mut config = json!({
        "model": "mock/m1",
        "providers": {"mock": {
            "name": "Mock",
            "package": "@opencode/ai/providers/openai-compatible",
            "settings": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "test"},
            "models": {"m1": {"name": "Mock One", "limit": {"context": WINDOW, "output": 32000}}}
        }}
    });
    for (k, v) in extra.as_object().unwrap() {
        config[k] = v.clone();
    }
    std::fs::write(dir.join("opencode.json"), config.to_string()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn measured_and_background_compaction() {
    // The whole scenario is one large future: keep it off the test thread's stack.
    Box::pin(scenarios()).await
}

async fn scenarios() {
    let root = std::env::temp_dir().join(format!("oc-compaction-bg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for (k, v) in [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache")] {
        unsafe { std::env::set_var(k, root.join(v)) };
    }
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_TEST_HOME", root.join("home"));
    }
    let mock = Arc::new(Mutex::new(Mock::default()));
    let state = mock.clone();
    let router = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::Json<Value>| {
            let state = state.clone();
            async move {
                let raw = body.0.to_string();
                let (reject, gate, fail) = {
                    let mut m = state.lock();
                    m.requests.push(body.0.clone());
                    (m.reject.clone(), m.gate.clone(), m.fail.clone())
                };
                if reject.is_some_and(|r| raw.contains(&r)) {
                    return error(400, "context_length_exceeded", "prompt too long");
                }
                if let Some(fail) = fail {
                    fail.notify_one();
                    return error(400, "invalid_request", "provider down");
                }
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                summary_reply()
            }
        }),
    );
    // Requests carry megabytes of old reasoning.
    let router = router.layer(axum::extract::DefaultBodyLimit::disable());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    // `summary`: a 25k buffer with background summaries on; `on`: background summaries on; `off`: the defaults.
    let summary = root.join("summary");
    let on = root.join("on");
    let off = root.join("off");
    write_config(&summary, port, json!({"compaction": {"buffer": BUFFER}, "experimental": {"background_compaction": true}}));
    write_config(&on, port, json!({"experimental": {"background_compaction": true}}));
    write_config(&off, port, json!({}));

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
    let h = Harness { app, engine, mock };

    // ---- summary cache prefix uses measured input with old reasoning (background = false, true)
    Box::pin(async {
        for background in [false, true] {
            h.reset();
            let messages = vec![text_user("Earlier question"), old_reasoning(INPUT), text_user("Latest question"), measured("Latest answer", INPUT)];
            let context = h.context(&summary, messages.clone()).await;
            let summarized = h.with(&context, if background { messages[..3].to_vec() } else { messages.clone() });
            let (normal, _) = h.engine.request(&summarized, false);
            if background {
                h.engine.precompute(&context, false).await.unwrap();
            }
            assert_eq!(h.manual(&context).await, CompactOutcome::Completed, "background={background}");
            let requests = h.requests();
            assert_eq!(requests.len(), 1, "background={background}");
            let request = &requests[0];
            assert!(!request["tools"].as_array().unwrap().is_empty(), "the unchanged request keeps its tools");
            // The normal request's system message, its conversation, then the summary prompt.
            assert_eq!(request["messages"].as_array().unwrap().len(), 1 + normal.messages.len() + 1, "background={background}");
            assert_eq!(last_text(request), build_prompt(false, false));
            assert_eq!(request["max_completion_tokens"], json!(BUFFER), "background={background}");
        }
    })
    .await;

    // ---- summary cache prefix still flattens genuinely oversized measured input
    Box::pin(async {
        h.reset();
        let context = h.context(&summary, vec![text_user(&token_text(CEILING)), measured("Latest answer", WINDOW + BUFFER)]).await;
        assert_eq!(h.manual(&context).await, CompactOutcome::Completed);
        let requests = h.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].get("tools").is_none_or(|t| t.as_array().is_some_and(Vec::is_empty)));
        let conversation: Vec<&Value> = requests[0]["messages"].as_array().unwrap().iter().filter(|m| m["role"] != "system").collect();
        assert_eq!(conversation.len(), 2);
        assert!(conversation[0].to_string().contains("characters omitted"));
    })
    .await;

    // ---- summary measured recovery shrinks from the rejected measured size
    Box::pin(async {
        h.reset();
        let text = token_text(CEILING / 2);
        let context = h
            .context(&summary, vec![text_user(&text), old_reasoning(CEILING / 2), text_user(&text), measured("Latest answer", CEILING)])
            .await;
        // Reject until the original exchange is actually shortened.
        h.mock.lock().reject = Some(text.clone());
        assert_eq!(h.manual(&context).await, CompactOutcome::Completed);
        let requests = h.requests();
        // The retained exchange fits the 85% and 70% targets; 50% of the measured input forces truncation.
        assert_eq!(requests.len(), 4);
        assert!(requests[3]["messages"].to_string().contains("characters omitted"));
    })
    .await;

    // ---- summary measured recovery reuses usage only for the matching agent
    Box::pin(async {
        h.reset();
        let context = h.context(&summary, vec![text_user("Earlier question"), old_reasoning(INPUT)]).await;
        let mut other = context.clone();
        other.sel.agent.id = "other".into();
        other.sel.agent.system = Some(token_text(WINDOW - BUFFER / 5));
        assert_eq!(h.manual(&other).await, CompactOutcome::Completed);
        let requests = h.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0]["max_completion_tokens"].as_u64().unwrap() <= BUFFER / 5, "{}", requests[0]["max_completion_tokens"]);
        // Switching back restores the measured prefix.
        assert_eq!(h.manual(&context).await, CompactOutcome::Completed);
        let requests = h.requests();
        assert_eq!(requests.len(), 2);
        assert!(!requests[1]["tools"].as_array().unwrap().is_empty());
        assert_eq!(requests[1]["max_completion_tokens"], json!(BUFFER));
        assert_eq!(last_text(&requests[1]), build_prompt(false, false));
    })
    .await;

    // ---- background summaries stay off unless enabled
    Box::pin(async {
        h.reset();
        let history = precompute_history();
        let near = h.context(&off, history.near.clone()).await;
        h.engine.precompute(&near, false).await.unwrap();
        assert_eq!(h.auto(&h.with(&near, history.due.clone())).await, CompactOutcome::Completed);
        let requests = h.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0]["messages"].to_string().contains("Third answer"));
    })
    .await;

    // ---- automatic compaction swaps in the background summary without another model call
    Box::pin(async {
        h.reset();
        let history = precompute_history();
        let near = h.context(&on, history.near.clone()).await;
        let session = near.sel.session.id.clone();
        let mut events = h.app.bus.subscribe();
        h.engine.precompute(&near, false).await.unwrap();
        assert_eq!(h.auto(&h.with(&near, history.due.clone())).await, CompactOutcome::Completed);
        // The one request summarized the prefix the latest response was generated from.
        let requests = h.requests();
        assert_eq!(requests.len(), 1);
        let summarized = requests[0]["messages"].to_string();
        assert!(summarized.contains("Second question") && !summarized.contains("Second answer"), "{summarized}");
        // The background run streams no deltas.
        let mut deltas = 0;
        let mut types = vec![];
        while let Ok(event) = events.try_recv() {
            if event.kind == "session.compaction.delta" {
                deltas += 1;
            }
            if event.durable.is_some() && event.data["sessionID"] == json!(session) {
                types.push(event.kind.clone());
            }
            if event.kind == "session.compaction.ended" {
                assert_eq!(event.metadata.as_ref().and_then(|m| m.get("keepID")), Some(&json!(history.anchor.id())));
                assert_eq!(event.data["text"], SUMMARY);
                assert_eq!(event.data["tokens"]["input"], json!(12));
                assert_eq!(event.data["tokens"]["cache"]["read"], json!(3));
            }
        }
        assert_eq!(deltas, 0);
        // The background call is billed once, when it finishes, and the compaction ends after it.
        assert_eq!(types.last().map(String::as_str), Some("session.compaction.ended"));
        types.sort();
        assert_eq!(types, ["session.compaction.ended", "session.compaction.started", "session.usage.recorded"]);
    })
    .await;

    // ---- automatic compaction waits for a background summary still being written
    Box::pin(async {
        h.reset();
        let gate = Arc::new(tokio::sync::Notify::new());
        h.mock.lock().gate = Some(gate.clone());
        let history = precompute_history();
        let near = h.context(&on, history.near.clone()).await;
        let session = near.sel.session.id.clone();
        let mut events = h.app.bus.subscribe();
        h.engine.precompute(&near, false).await.unwrap();
        let (engine, due) = (h.engine.clone(), h.with(&near, history.due.clone()));
        let compacting = tokio::spawn(async move { engine.compact(&due, Trigger::Auto, CancellationToken::new()).await.unwrap() });
        // The compaction shows as running while the summary it waits for is still being written.
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), events.recv()).await.unwrap().unwrap();
            if event.kind == "session.compaction.started" && event.data["sessionID"] == json!(session) {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!compacting.is_finished());
        gate.notify_one();
        assert_eq!(compacting.await.unwrap(), CompactOutcome::Completed);
        assert_eq!(h.requests().len(), 1);
    })
    .await;

    // ---- a model switch waits for the new model's first response before summarizing in the background
    Box::pin(async {
        h.reset();
        let history = precompute_history();
        let mut switched = h.context(&on, history.near.clone()).await;
        switched.resolved.model.id = "other".into();
        h.engine.precompute(&switched, false).await.unwrap();
        assert_eq!(h.auto(&h.with(&switched, history.due.clone())).await, CompactOutcome::Completed);
        let requests = h.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0]["messages"].to_string().contains("Third answer"));
    })
    .await;

    // ---- a background summary that fell too far behind is replaced from a newer response
    Box::pin(async {
        h.reset();
        let history = precompute_history();
        let near = h.context(&on, history.near.clone()).await;
        // About 35k tokens arrived after the summarized prefix, past the 30k that may stay verbatim beside it.
        let mut behind = history.near.clone();
        behind.extend([text_user(&"x".repeat(140_000)), measured("Burst handled", 160_000)]);
        h.engine.precompute(&near, false).await.unwrap();
        h.engine.precompute(&h.with(&near, behind.clone()), false).await.unwrap();
        let mut due = behind.clone();
        due.extend([text_user("Next question"), measured("Next answer", 182_000)]);
        assert_eq!(h.auto(&h.with(&near, due)).await, CompactOutcome::Completed);
        // Compaction swapped in the newer summary, which ends before the latest response, instead of writing one.
        let summarized = h.requests().last().unwrap()["messages"].to_string();
        assert!(summarized.contains(&"x".repeat(1_000)));
        assert!(!summarized.contains("Burst handled"));
    })
    .await;

    // ---- a failed background summary is retried from a newer response, then left until compaction
    Box::pin(async {
        h.reset();
        let history = precompute_history();
        let near = h.context(&on, history.near.clone()).await;
        let mut later = history.near.clone();
        later.extend([text_user("Third question"), measured("Third answer", 160_000)]);
        let mut latest = later.clone();
        latest.extend([text_user("Fourth question"), measured("Fourth answer", 165_000)]);
        let failing = |messages: Vec<Message>| {
            let (h, near) = (&h, &near);
            async move {
                let reached = Arc::new(tokio::sync::Notify::new());
                h.mock.lock().fail = Some(reached.clone());
                h.engine.precompute(&h.with(near, messages), false).await.unwrap();
                reached.notified().await;
                // Let the failed task settle.
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        };
        failing(history.near.clone()).await;
        assert_eq!(h.requests().len(), 1);
        // The same response would send the same request again.
        h.engine.precompute(&near, false).await.unwrap();
        assert_eq!(h.requests().len(), 1);
        failing(later).await;
        let requests = h.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[1]["messages"].to_string().contains("Second answer"));
        // Two failures in a row stop background summaries until the next compaction.
        h.engine.precompute(&h.with(&near, latest), false).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(h.requests().len(), 2);
        h.mock.lock().fail = None;
    })
    .await;

    // ---- compaction summarizes as usual when the background summary no longer fits the conversation
    Box::pin(async {
        let history = precompute_history();
        let near = h.context(&on, history.near.clone()).await;
        let mut burst = history.near.clone();
        burst.extend([text_user(&"x".repeat(140_000)), measured("Burst handled", 182_000)]);
        h.reset();
        h.engine.precompute(&near, false).await.unwrap();
        assert_eq!(h.auto(&h.with(&near, burst)).await, CompactOutcome::Completed);
        assert!(h.requests().last().unwrap()["messages"].to_string().contains("Burst handled"));
        // A different model would replay a summary written for the previous one.
        h.reset();
        h.engine.precompute(&near, false).await.unwrap();
        let mut switched = h.with(&near, history.due.clone());
        switched.resolved.model.id = "other".into();
        assert_eq!(h.auto(&switched).await, CompactOutcome::Completed);
        assert!(h.requests().last().unwrap()["messages"].to_string().contains("Third answer"));
    })
    .await;

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
