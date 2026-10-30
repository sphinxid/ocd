//! `/goal` (`core/src/plugin/goal.ts`): upstream's `test/plugin/goal.test.ts` and `goal-watchdog.test.ts` against an
//! OpenAI-compatible mock provider. One test function: the scenarios share one engine (the XDG environment is
//! process-wide), each on its own session.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use axum::{body::Body, response::Response, routing::post, Router};
use oc_core::goal::{self, Admit};
use oc_core::{app::App, inbox::Scope, runner::Engine, session::{CreateInput, PromptInput}};
use oc_schema::types::LocationRef;
use oc_schema::Message;
use oc_tools::host::JobRecovery;
use parking_lot::Mutex;
use serde_json::{json, Value};

const REVIEW: &str = "You are now the reviewer for the session's goal";

#[derive(Clone)]
enum Reply {
    Text(String),
    Overflow,
}

/// The mock's script: reviewer replies in order (then "Out of script."), and how ordinary work is answered.
#[derive(Default)]
struct Script {
    reviews: VecDeque<Reply>,
    asked: usize,
    /// Every reviewer request's last message.
    prompts: Vec<String>,
    /// Ordinary work: a tool call each round, then text once its result is in.
    tools: bool,
    /// Ordinary work requests wait for this before answering.
    work_gate: Option<Arc<tokio::sync::Notify>>,
    /// Reviewer requests signal `judging`, then wait for `release`.
    review_gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    /// Ordinary work requests seen (full bodies).
    work: Vec<Value>,
}

fn sse(chunks: &[Value]) -> Response {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    Response::builder().header("content-type", "text/event-stream").body(Body::from(body)).unwrap()
}

fn text(t: &str) -> Response {
    sse(&[
        json!({"id": "r", "choices": [{"index": 0, "delta": {"role": "assistant", "content": t}}]}),
        json!({"id": "r", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
               "usage": {"prompt_tokens": 20, "completion_tokens": 4, "total_tokens": 24}}),
    ])
}

fn tool(name: &str, id: &str) -> Response {
    sse(&[
        json!({"id": "r", "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{"index": 0, "id": id, "type": "function", "function": {"name": name, "arguments": "{}"}}]}}]}),
        json!({"id": "r", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

fn overflow() -> Response {
    let body = json!({"error": {"message": "This model's maximum context length is 1000 tokens.", "type": "invalid_request_error", "code": "context_length_exceeded"}});
    Response::builder().status(400).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap()
}

fn respond(reply: Reply) -> Response {
    match reply {
        Reply::Text(t) => text(&t),
        Reply::Overflow => overflow(),
    }
}

struct Harness {
    app: Arc<App>,
    engine: Arc<Engine>,
    project: std::path::PathBuf,
    script: Arc<Mutex<Script>>,
}

impl Harness {
    fn reset(&self, f: impl FnOnce(&mut Script)) {
        let mut s = self.script.lock();
        *s = Script::default();
        f(&mut s);
    }

    async fn session(&self, title: Option<&str>, parent: Option<&str>) -> String {
        let location = parent.is_none().then(|| LocationRef { directory: self.project.display().to_string(), workspace_id: None });
        self.app
            .sessions
            .create(CreateInput {
                location,
                parent_id: parent.map(str::to_string),
                title: title.map(str::to_string),
                agent: Some("build".into()),
                ..Default::default()
            })
            .await
            .unwrap()
            .id
    }

    /// `session.command {name: "goal", text}` as the backend runs it.
    async fn goal(&self, session: &str, text: &str) -> Result<(), String> {
        match self.engine.goal_command(session, text, false).await? {
            Admit::None => Ok(()),
            Admit::Prompt { .. } => unreachable!("no attachments in these tests"),
            Admit::Synthetic { text, description, metadata, .. } => {
                self.engine.goal_synthetic(session, text, description, metadata, None).await.map_err(|e| e.to_string())
            }
        }
    }

    async fn wait(&self, session: &str) {
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::timeout(Duration::from_secs(30), self.app.coordinator().await_idle(session)).await.expect("session went idle");
    }

    async fn prompt(&self, session: &str, text: &str) {
        self.app.sessions.prompt(session, PromptInput { text: text.into(), ..Default::default() }).await.unwrap();
        self.app.coordinator().wake(session, Scope::Input);
    }

    fn goal_of(&self, session: &str) -> Option<Value> {
        goal::read(&self.app.db, session).unwrap().map(|g| g.encoded())
    }

    fn set_goal(&self, session: &str, state: Value) {
        self.app.db.kv_set(&goal::key(session), &state).unwrap();
    }

    fn messages(&self, session: &str) -> Vec<Message> {
        self.app.db.session_messages(session).unwrap().into_iter().map(|(_, m)| m).collect()
    }

    fn asked(&self) -> usize {
        self.script.lock().asked
    }
}

fn subset(actual: &Option<Value>, expected: Value) {
    let actual = actual.clone().unwrap_or(Value::Null);
    for (k, v) in expected.as_object().unwrap() {
        assert_eq!(actual.get(k), Some(v), "{k} in {actual}");
    }
}

async fn eventually(mut check: impl FnMut() -> bool) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn goal_plugin() {
    let root = std::env::temp_dir().join(format!("oc-goal-{}", std::process::id()));
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

    let script = Arc::new(Mutex::new(Script::default()));
    let mock = script.clone();
    let router = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::Json<Value>| {
            let mock = mock.clone();
            async move {
                let raw = body.0.to_string();
                let last = body.0["messages"].as_array().and_then(|m| m.last()).cloned().unwrap_or(Value::Null).to_string();
                if raw.contains("You name conversations") {
                    return text("Title");
                }
                if last.contains("You MUST summarize the conversation above") || last.contains("Update the existing checkpoint") {
                    return text("## Objective\n- Make CI green");
                }
                if last.contains(REVIEW) {
                    let (reply, gate) = {
                        let mut s = mock.lock();
                        s.asked += 1;
                        s.prompts.push(last.clone());
                        let reply = s.reviews.pop_front().unwrap_or_else(|| Reply::Text(r#"{"verdict":"done","reason":"Out of script."}"#.into()));
                        (reply, s.review_gate.clone())
                    };
                    if let Some((judging, release)) = gate {
                        judging.notify_one();
                        release.notified().await;
                    }
                    return respond(reply);
                }
                let (tools, gate, n) = {
                    let mut s = mock.lock();
                    s.work.push(body.0.clone());
                    (s.tools, s.work_gate.clone(), s.asked)
                };
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                if tools && !(last.contains("\"tool\"") && !last.contains("untrusted_objective")) {
                    return tool("missing_tool", &format!("call-{n}"));
                }
                text(if tools { "Checked." } else { "Working on it" })
            }
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
                "models": {"m1": {"name": "Mock One", "limit": {"context": 200000, "output": 32000}}}
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
    let h = Harness { app, engine, project, script };
    let continue_ = |reason: &str| Reply::Text(json!({"verdict": "continue", "reason": reason}).to_string());

    // ---- keeps working in one execution until the reviewer finds the goal met
    h.reset(|s| {
        s.reviews = VecDeque::from([
            Reply::Text(r#"{"verdict":"continue","reason":"The parser test still fails.","next_step":"Fix the date parser."}"#.into()),
            Reply::Text("```json\n{\"verdict\":\"done\",\"reason\":\"All 12 tests pass.\"}\n```".into()),
        ])
    });
    let s = h.session(None, None).await;
    h.goal(&s, "  Make the test suite pass  ").await.unwrap();
    h.wait(&s).await;
    assert_eq!(h.asked(), 2);
    subset(&h.goal_of(&s), json!({"objective": "Make the test suite pass", "status": "complete", "reason": "All 12 tests pass.", "rounds": 1}));
    // An untitled session is named after the objective.
    assert_eq!(h.app.sessions.get(&s).unwrap().title.as_deref(), Some("Make the test suite pass"));
    let messages = h.messages(&s);
    assert!(!messages.iter().any(|m| matches!(m, Message::User { .. })));
    let notices: Vec<(Option<String>, String, Value)> = messages
        .iter()
        .filter_map(|m| match m {
            Message::Synthetic { description, text, metadata, .. } => Some((description.clone(), text.clone(), json!(metadata))),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 2);
    assert_eq!(notices[0].0.as_deref(), Some("Goal: Make the test suite pass"));
    assert_eq!(notices[1].0, None);
    for n in &notices {
        assert_eq!(n.2["source"], "goal");
        assert!(n.2["goalID"].is_string());
    }
    assert!(notices[0].1.contains("<untrusted_objective>\nMake the test suite pass\n</untrusted_objective>"));
    assert!(notices[1].1.contains("The parser test still fails.\nNext step: Fix the date parser."));
    // Continuations run inside the same execution, so the session went idle once.
    assert_eq!(messages.iter().filter(|m| matches!(m, Message::Idle { .. })).count(), 1);

    // ---- pauses when the reviewer keeps giving the same reason, even with tool calls
    h.reset(|s| {
        s.tools = true;
        s.reviews = std::iter::repeat_n(continue_("The flaky test still fails."), 4).collect();
        s.reviews.push_back(Reply::Text(r#"{"verdict":"done","reason":"Unreachable."}"#.into()));
    });
    let s = h.session(Some("Goal session"), None).await;
    h.goal(&s, "Fix the flaky test").await.unwrap();
    h.wait(&s).await;
    assert_eq!(h.asked(), 4);
    subset(&h.goal_of(&s), json!({"status": "paused", "reason": "Not making progress: The flaky test still fails.", "rounds": 3}));

    // ---- pauses when rounds keep passing without tool calls
    h.reset(|s| {
        s.reviews = ["Two tests fail.", "One test fails.", "The lint step fails."].iter().map(|r| continue_(r)).collect();
        s.reviews.push_back(Reply::Text(r#"{"verdict":"done","reason":"Unreachable."}"#.into()));
    });
    let s = h.session(Some("Goal session"), None).await;
    h.goal(&s, "Fix the flaky test").await.unwrap();
    h.wait(&s).await;
    assert_eq!(h.asked(), 3);
    subset(&h.goal_of(&s), json!({"status": "paused", "reason": "Not making progress: The lint step fails.", "rounds": 2}));

    // ---- keeps going while each round calls tools and finds a new reason
    h.reset(|s| {
        s.tools = true;
        s.reviews = ["Two tests fail.", "One test fails.", "The lint step fails."].iter().map(|r| continue_(r)).collect();
        s.reviews.push_back(Reply::Text(r#"{"verdict":"done","reason":"Everything passes."}"#.into()));
    });
    let s = h.session(Some("Goal session"), None).await;
    h.goal(&s, "Make CI green").await.unwrap();
    h.wait(&s).await;
    assert_eq!(h.asked(), 4);
    subset(&h.goal_of(&s), json!({"status": "complete", "rounds": 3, "stalls": 0}));

    // ---- pauses when the reviewer gives no usable verdict twice
    h.reset(|s| s.reviews = VecDeque::from([Reply::Text("Looks good to me.".into()), Reply::Text("I think so.".into())]));
    let s = h.session(Some("Goal session"), None).await;
    h.goal(&s, "Ship it").await.unwrap();
    h.wait(&s).await;
    assert_eq!(h.asked(), 2);
    subset(&h.goal_of(&s), json!({"status": "paused", "reason": "The goal reviewer gave no usable verdict"}));

    // ---- waits for a background subagent instead of judging
    h.reset(|s| s.reviews = VecDeque::from([Reply::Text(r#"{"verdict":"done","reason":"The report is written."}"#.into())]));
    let s = h.session(Some("Goal session"), None).await;
    let child = h.session(Some("Explore"), Some(&s)).await;
    let (done, finished) = tokio::sync::oneshot::channel::<()>();
    h.app.jobs.start(
        child.clone(),
        "subagent".into(),
        None,
        Some(JobRecovery::Subagent { parent_session_id: s.clone(), child_session_id: child.clone(), agent: "explore".into(), description: "Explore".into() }),
        Box::pin(async move {
            let _ = finished.await;
            Ok("Found everything".to_string())
        }),
    );
    h.app.jobs.background(&child);
    h.goal(&s, "Write the report").await.unwrap();
    h.wait(&s).await;
    assert_eq!(h.asked(), 0);
    subset(&h.goal_of(&s), json!({"status": "active", "rounds": 0}));
    // The subagent's notice wakes the session, and the reviewer runs once nothing is outstanding.
    done.send(()).unwrap();
    h.app.jobs.wait(&child).await;
    h.engine.goal_synthetic(&s, "Subagent finished".into(), None, Default::default(), None).await.unwrap();
    h.wait(&s).await;
    assert_eq!(h.asked(), 1);
    subset(&h.goal_of(&s), json!({"status": "complete"}));

    // ---- does not wait on work a descendant owns, since only the descendant is woken
    h.reset(|s| {
        s.reviews = VecDeque::from([
            Reply::Text(r#"{"verdict":"wait","reason":"bun run build is still running."}"#.into()),
            Reply::Text(r#"{"verdict":"done","reason":"The report is written."}"#.into()),
        ])
    });
    let s = h.session(Some("Goal session"), None).await;
    let child = h.session(Some("Explore"), Some(&s)).await;
    let grandchild = h.session(Some("Dig"), Some(&child)).await;
    let never = || -> futures::future::BoxFuture<'static, Result<String, String>> { Box::pin(futures::future::pending()) };
    h.app.jobs.start(
        grandchild.clone(),
        "subagent".into(),
        None,
        Some(JobRecovery::Subagent { parent_session_id: child.clone(), child_session_id: grandchild.clone(), agent: "explore".into(), description: "Dig".into() }),
        never(),
    );
    h.app.jobs.background(&grandchild);
    h.app.jobs.start(
        "child-shell".into(),
        "shell".into(),
        None,
        Some(JobRecovery::Shell { session_id: child.clone(), shell_id: "child-shell".into(), command: "bun run build".into() }),
        never(),
    );
    h.app.jobs.background("child-shell");
    let background = h.engine.outstanding(&s);
    assert_eq!(
        background,
        vec![
            json!({"kind": "subagent", "sessionID": child, "childSessionID": grandchild, "agent": "explore", "description": "Dig"}),
            json!({"kind": "shell", "sessionID": child, "shellID": "child-shell", "command": "bun run build"}),
        ]
    );
    h.goal(&s, "Write the report").await.unwrap();
    h.wait(&s).await;
    // With none of the session's own shells running, "wait" is not offered and counts as an unusable answer.
    assert_eq!(h.asked(), 2);
    assert!(h.script.lock().prompts[0].contains("No background shells are running."));
    subset(&h.goal_of(&s), json!({"status": "complete", "reason": "The report is written."}));
    h.app.jobs.cancel(&grandchild);
    h.app.jobs.cancel("child-shell");

    // ---- drops the reviewer's verdict when the goal is paused while it runs
    let (judging, release) = (Arc::new(tokio::sync::Notify::new()), Arc::new(tokio::sync::Notify::new()));
    h.reset(|s| {
        s.reviews = VecDeque::from([continue_("More to do.")]);
        s.review_gate = Some((judging.clone(), release.clone()));
    });
    let s = h.session(Some("Goal session"), None).await;
    h.goal(&s, "Rename the module").await.unwrap();
    judging.notified().await;
    h.goal(&s, "pause").await.unwrap();
    release.notify_one();
    h.wait(&s).await;
    subset(&h.goal_of(&s), json!({"status": "paused", "reason": "Paused by the user", "rounds": 0}));
    let descriptions: Vec<Option<String>> =
        h.messages(&s).iter().filter_map(|m| if let Message::Synthetic { description, .. } = m { Some(description.clone()) } else { None }).collect();
    assert_eq!(descriptions, vec![Some("Goal: Rename the module".to_string())]);

    // ---- reminds the model of the goal after a compaction until a continuation restates it
    h.reset(|_| {});
    let s = h.session(Some("Goal session"), None).await;
    h.prompt(&s, "Look at the failing CI job").await;
    h.wait(&s).await;
    h.set_goal(&s, json!({"id": "goal-compacted", "objective": "Make CI green", "status": "active", "rounds": 2, "maxRounds": 50, "boot": "an-earlier-process", "updatedAt": 0}));
    h.app.sessions.compact(&s, None).await.unwrap();
    h.app.coordinator().wake(&s, Scope::Input);
    h.wait(&s).await;
    let before = h.script.lock().work.len();
    h.prompt(&s, "Carry on").await;
    h.wait(&s).await;
    let next = h.script.lock().work[before].clone();
    let messages = next["messages"].as_array().unwrap();
    assert!(next["messages"].to_string().contains("A goal is active for this session. Keep working toward it."));
    // The reminder goes before the user's prompt and is not saved to the session.
    assert!(messages.last().unwrap().to_string().contains("Carry on"));
    assert!(messages[messages.len() - 2].to_string().contains("A goal is active for this session."));
    assert!(!h.messages(&s).iter().any(|m| matches!(m, Message::Synthetic { text, .. } if text.contains("Keep working toward it."))));
    // The earlier process's goal was paused instead of judged.
    subset(&h.goal_of(&s), json!({"status": "paused", "reason": "The server restarted while the goal was active"}));

    // ---- compacts and asks again when the reviewer's request is too large
    h.reset(|s| s.reviews = VecDeque::from([Reply::Overflow, Reply::Text(r#"{"verdict":"done","reason":"CI is green."}"#.into())]));
    let s = h.session(Some("Goal session"), None).await;
    h.goal(&s, "Make CI green").await.unwrap();
    h.wait(&s).await;
    assert_eq!(h.asked(), 2);
    subset(&h.goal_of(&s), json!({"status": "complete", "reason": "CI is green."}));
    let compactions: Vec<Message> = h.messages(&s).into_iter().filter(|m| matches!(m, Message::Compaction(_))).collect();
    assert_eq!(compactions.len(), 1);
    assert!(matches!(&compactions[0], Message::Compaction(c) if matches!(c.status, oc_schema::message::CompactionStatus::Completed { .. })));

    // ---- pauses when the reviewer's request is too large even after compacting
    h.reset(|s| s.reviews = std::iter::repeat_n(Reply::Overflow, 6).collect());
    let s = h.session(Some("Goal session"), None).await;
    h.goal(&s, "Make CI green").await.unwrap();
    h.wait(&s).await;
    subset(&h.goal_of(&s), json!({"status": "paused", "reason": "The goal reviewer's request is too large for the model"}));

    // ---- pauses an active goal when the user stops the session
    let gate = Arc::new(tokio::sync::Notify::new());
    h.reset(|s| s.work_gate = Some(gate.clone()));
    let s = h.session(Some("Goal session"), None).await;
    h.goal(&s, "Refactor the store").await.unwrap();
    eventually(|| !h.script.lock().work.is_empty()).await;
    h.app.coordinator().interrupt(&s, Some(oc_core::coordinator::Reason::User), true).await;
    eventually(|| h.goal_of(&s).is_some_and(|g| g["status"] == "paused")).await;
    subset(&h.goal_of(&s), json!({"reason": "Stopped by the user"}));
    gate.notify_waiters();

    // ---- pauses a goal set by an earlier server process instead of continuing it
    h.reset(|s| s.reviews = VecDeque::from([continue_("Not yet.")]));
    let s = h.session(Some("Goal session"), None).await;
    h.set_goal(&s, json!({"id": "goal-earlier", "objective": "Migrate the database", "status": "active", "rounds": 3, "maxRounds": 20, "boot": "an-earlier-process", "updatedAt": 0}));
    h.prompt(&s, "Server came back").await;
    h.wait(&s).await;
    assert_eq!(h.asked(), 0);
    subset(&h.goal_of(&s), json!({"status": "paused", "reason": "The server restarted while the goal was active"}));

    // ---- does not give a child session its parent's goal
    let gate = Arc::new(tokio::sync::Notify::new());
    h.reset(|s| {
        s.reviews = VecDeque::from([Reply::Text(r#"{"verdict":"blocked","reason":"Waiting on the user."}"#.into())]);
        s.work_gate = Some(gate.clone());
    });
    let s = h.session(Some("Goal session"), None).await;
    h.goal(&s, "Tidy the docs").await.unwrap();
    eventually(|| !h.script.lock().work.is_empty()).await;
    // Created while the parent's goal is active.
    let child = h.session(Some("Helper"), Some(&s)).await;
    h.script.lock().work_gate = None;
    gate.notify_waiters();
    h.wait(&s).await;
    assert_eq!(h.asked(), 1);
    assert_eq!(h.goal_of(&child), None);
    h.prompt(&child, "Help out").await;
    h.wait(&child).await;
    assert_eq!(h.asked(), 1);

    // ---- pauses, resumes and clears a goal through its subcommands
    h.reset(|s| {
        s.reviews = VecDeque::from([
            Reply::Text(r#"{"verdict":"paused","reason":"The user asked to stop for now."}"#.into()),
            Reply::Text(r#"{"verdict":"done","reason":"The changelog lists every PR."}"#.into()),
        ])
    });
    let s = h.session(Some("Goal session"), None).await;
    assert_eq!(h.goal(&s, "").await, Err(goal::USAGE.to_string()));
    assert_eq!(h.goal(&s, "pause").await, Err("No active goal to pause.".into()));
    assert_eq!(h.goal(&s, "resume").await, Err("No paused goal to resume.".into()));
    assert_eq!(h.goal(&s, "clear").await, Err("No goal is set.".into()));
    assert_eq!(h.goal(&s, &"x".repeat(4001)).await, Err("A goal objective is limited to 4000 characters (got 4001).".into()));
    h.goal(&s, "Update the changelog").await.unwrap();
    h.wait(&s).await;
    subset(&h.goal_of(&s), json!({"status": "paused", "reason": "The user asked to stop for now."}));
    assert_eq!(
        h.goal(&s, "").await,
        Err(format!("Goal paused: Update the changelog\nThe user asked to stop for now.\n{}", goal::USAGE))
    );
    assert!(h.goal(&s, "pause").await.is_err());
    h.goal(&s, "Resume").await.unwrap();
    subset(&h.goal_of(&s), json!({"status": "active", "rounds": 1}));
    h.wait(&s).await;
    subset(&h.goal_of(&s), json!({"status": "complete", "rounds": 1}));
    let descriptions: Vec<Option<String>> =
        h.messages(&s).iter().filter_map(|m| if let Message::Synthetic { description, .. } = m { Some(description.clone()) } else { None }).collect();
    assert_eq!(descriptions, vec![Some("Goal: Update the changelog".to_string()), None]);
    h.goal(&s, "clear").await.unwrap();
    assert_eq!(h.goal_of(&s), None);

    // ---- serves the goal over RPC and publishes every change
    let s = h.session(Some("Goal session"), None).await;
    let mut events = h.app.bus.subscribe();
    assert_eq!(goal::rpc(&h.app.db, "get", &json!({"sessionID": s})), Ok(Value::Null));
    assert_eq!(goal::rpc(&h.app.db, "set", &json!({})), Err(("rpc.method_not_found".into(), "Unknown RPC method: opencode.goal.set".into())));
    assert_eq!(goal::rpc(&h.app.db, "get", &json!({})), Err(("rpc.invalid_input".into(), "Missing key\n  at [\"sessionID\"]".into())));
    // Not started: the goal is armed but nothing runs it.
    let created = h.engine.goal_command(&s, "Ship the release", false).await.unwrap();
    assert!(matches!(created, Admit::Synthetic { .. }));
    h.goal(&s, "pause").await.unwrap();
    subset(&goal::rpc(&h.app.db, "get", &json!({"sessionID": s})).ok(), json!({"objective": "Ship the release", "status": "paused"}));
    h.goal(&s, "clear").await.unwrap();
    assert_eq!(goal::rpc(&h.app.db, "get", &json!({"sessionID": s})), Ok(Value::Null));
    let mut changed = vec![];
    while let Ok(event) = events.try_recv() {
        if event.kind == goal::CHANGED {
            assert_eq!(event.location.as_ref().map(|l| l.directory.clone()), Some(h.project.display().to_string()));
            changed.push(event.data.clone());
        }
    }
    assert_eq!(changed.len(), 3);
    assert_eq!(changed[0]["sessionID"], json!(s));
    assert_eq!(changed[0]["goal"]["status"], "active");
    assert_eq!(changed[1]["goal"]["reason"], "Paused by the user");
    assert_eq!(changed[2]["goal"], Value::Null);
    // Key order: storage keeps the plugin's spread order, the event the schema order.
    let keys: Vec<String> = changed[1]["goal"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["id", "objective", "status", "reason", "rounds", "maxRounds", "boot", "updatedAt"]);

    // ---- watchdog: pauses a goal that sits idle past the limit, and not while the session keeps running
    h.engine.goals.set_wait_limit(Duration::from_millis(400));
    let s = h.session(Some("Goal session"), None).await;
    let emit = |kind: &str, session: &str| {
        h.app.bus.publish(kind, json!({"sessionID": session}), None);
    };
    let armed = h.engine.goal_command(&s, "Ship the release", false).await.unwrap();
    assert!(matches!(armed, Admit::Synthetic { .. }));
    emit("session.execution.succeeded", &s);
    tokio::time::sleep(Duration::from_millis(250)).await;
    emit("session.execution.started", &s);
    tokio::time::sleep(Duration::from_millis(400)).await;
    subset(&h.goal_of(&s), json!({"status": "active"}));
    emit("session.execution.succeeded", &s);
    tokio::time::sleep(Duration::from_millis(700)).await;
    subset(&h.goal_of(&s), json!({"status": "paused", "reason": "Nothing ran for 30 minutes while the goal waited on background work"}));

    // ---- watchdog: keeps waiting while a descendant session keeps running steps
    let s = h.session(Some("Goal session"), None).await;
    let child = h.session(Some("child"), Some(&s)).await;
    let grandchild = h.session(Some("grandchild"), Some(&child)).await;
    let unrelated = h.session(Some("unrelated"), None).await;
    h.engine.goal_command(&s, "Ship the release", false).await.unwrap();
    emit("session.execution.succeeded", &s);
    tokio::time::sleep(Duration::from_millis(250)).await;
    emit("session.step.started", &child);
    tokio::time::sleep(Duration::from_millis(250)).await;
    emit("session.step.started", &grandchild);
    tokio::time::sleep(Duration::from_millis(250)).await;
    subset(&h.goal_of(&s), json!({"status": "active"}));
    emit("session.step.started", &unrelated);
    tokio::time::sleep(Duration::from_millis(500)).await;
    subset(&h.goal_of(&s), json!({"status": "paused", "reason": "Nothing ran for 30 minutes while the goal waited on background work"}));

    // ---- watchdog: never overwrites what the user did after the loop decided
    let s = h.session(Some("Goal session"), None).await;
    h.engine.goal_command(&s, "Ship the release", false).await.unwrap();
    emit("session.execution.succeeded", &s);
    tokio::time::sleep(Duration::from_millis(50)).await;
    h.goal(&s, "pause").await.unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    subset(&h.goal_of(&s), json!({"status": "paused", "reason": "Paused by the user"}));

    // ---- session.deleted drops the goal without publishing a change
    let s = h.session(Some("Goal session"), None).await;
    h.engine.goal_command(&s, "Ship the release", false).await.unwrap();
    h.app.sessions.remove(&s).unwrap();
    eventually(|| h.goal_of(&s).is_none()).await;

    // ---- a goal an earlier process left active is paused when the plugin starts (location activation), and goals of
    // deleted sessions are dropped
    let s = h.session(Some("Goal session"), None).await;
    h.set_goal(&s, json!({"id": "goal-earlier", "objective": "Ship the release", "status": "active", "rounds": 1, "maxRounds": 50, "boot": "an-earlier-process", "updatedAt": 0}));
    h.set_goal("ses_gone", json!({"id": "x", "objective": "x", "status": "active", "rounds": 1, "maxRounds": 50, "boot": "b", "updatedAt": 0}));
    h.engine.invalidate_locations();
    h.engine.location(&h.project.display().to_string()).await.unwrap();
    subset(&h.goal_of(&s), json!({"status": "paused", "reason": "The server restarted while the goal was active"}));
    assert_eq!(h.app.db.kv_get(&goal::key("ses_gone")).unwrap(), None);

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
