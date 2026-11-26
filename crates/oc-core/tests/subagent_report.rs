//! Subagent reports per busy period (2.3.0): `subagent-idle-e2e.test.ts`, `subagent-report.test.ts`,
//! `tool-subagent.test.ts` and the subagent cases of `session-execution.test.ts`, against an OpenAI-compatible mock
//! provider. One test function: the scenarios share one engine (the XDG environment is process-wide), each on its own
//! sessions.

use std::sync::Arc;
use std::time::Duration;

use axum::{body::Body, response::Response, routing::post, Router};
use oc_core::job::{Jobs, SubagentOutboxReport};
use oc_core::subagent_report::{Outcome, SubagentReport};
use oc_core::{app::App, coordinator::Reason, inbox::Scope, runner::Engine, session::{CreateInput, PromptInput}};
use oc_schema::event::SessionEvent;
use oc_schema::types::LocationRef;
use oc_schema::Message;
use oc_tools::host::{JobRecovery, JobStatus};
use parking_lot::Mutex;
use serde_json::{json, Value};

enum Reply {
    Text(String),
    Tool(&'static str, Value),
    /// Wait for the notify, then answer with text.
    Gated(Arc<tokio::sync::Notify>, String),
}

/// A request as the responder sees it: whether it is a subagent's (its history holds the subagent prefix), the last
/// message (JSON) and the whole body.
struct Request {
    child: bool,
    last: String,
    raw: String,
}

type Responder = Box<dyn FnMut(&Request) -> Reply + Send>;

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
        json!({"id": "r", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    ])
}

fn tool(name: &str, args: &Value, n: usize) -> Response {
    sse(&[
        json!({"id": "r", "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{"index": 0, "id": format!("call_{n}"), "type": "function", "function": {"name": name, "arguments": args.to_string()}}]}}]}),
        json!({"id": "r", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

struct Harness {
    app: Arc<App>,
    engine: Arc<Engine>,
    project: std::path::PathBuf,
    responder: Arc<Mutex<Responder>>,
}

/// The `<subagent>` notices delivered to a session: (message id, text, metadata).
type Report = (String, String, Value);

impl Harness {
    fn respond(&self, f: impl FnMut(&Request) -> Reply + Send + 'static) {
        *self.responder.lock() = Box::new(f);
    }

    async fn session(&self, title: &str, parent: Option<&str>) -> String {
        let location = parent.is_none().then(|| LocationRef { directory: self.project.display().to_string(), workspace_id: None });
        self.app
            .sessions
            .create(CreateInput {
                location,
                parent_id: parent.map(str::to_string),
                title: Some(title.into()),
                agent: Some(if parent.is_some() { "general" } else { "build" }.into()),
                ..Default::default()
            })
            .await
            .unwrap()
            .id
    }

    async fn prompt(&self, session: &str, text: &str) {
        self.app.sessions.prompt(session, PromptInput { text: text.into(), ..Default::default() }).await.unwrap();
        self.app.coordinator().wake(session, Scope::Input);
    }

    async fn idle(&self, session: &str) {
        tokio::time::sleep(Duration::from_millis(30)).await;
        tokio::time::timeout(Duration::from_secs(30), self.app.coordinator().await_idle(session)).await.expect("session went idle");
    }

    fn messages(&self, session: &str) -> Vec<Message> {
        self.app.db.session_messages(session).unwrap().into_iter().map(|(_, m)| m).collect()
    }

    fn reports(&self, session: &str) -> Vec<Report> {
        self.app
            .db
            .session_messages(session)
            .unwrap()
            .into_iter()
            .filter_map(|(_, m)| match m {
                Message::Synthetic { id, text, metadata, .. } => {
                    let metadata = serde_json::to_value(metadata).unwrap_or(Value::Null);
                    (metadata["source"] == "subagent").then_some((id, text, metadata))
                }
                _ => None,
            })
            .collect()
    }

    /// `waitForReports`: at least `count` reports delivered, then the session idle.
    async fn wait_reports(&self, session: &str, count: usize) -> Vec<Report> {
        for _ in 0..1500 {
            if self.reports(session).len() >= count {
                self.idle(session).await;
                return self.reports(session);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{session}: expected {count} reports, got {:#?}", self.reports(session));
    }

    fn child_of(&self, parent: &str) -> String {
        self.app.db.with(|c| c.query_row("SELECT id FROM session_v2 WHERE parent_id = ? ORDER BY time_created DESC LIMIT 1", [parent], |r| r.get(0))).unwrap()
    }

    fn job_rows(&self, session: &str) -> Vec<(String, String)> {
        self.app
            .db
            .with(|c| {
                let mut s = c.prepare("SELECT notification_id, status FROM job WHERE session_id = ?1 OR child_session_id = ?1")?;
                let r = s.query_map([session], |r| Ok((r.get(0)?, r.get(1)?)))?.collect();
                r
            })
            .unwrap()
    }

    fn suspended(&self, session: &str) -> bool {
        self.app.db.with(|c| c.query_row("SELECT time_suspended IS NOT NULL FROM session_v2 WHERE id = ?", [session], |r| r.get(0))).unwrap()
    }
}

/// The parent's usual script: launch on its prompt, acknowledge the launch and every report.
fn parent(req: &Request, launch: &Value) -> Reply {
    if req.last.contains(r#"<background-result kind=\"subagent\" "#) || req.last.contains("\"tool\"") {
        return Reply::Text("parent ack".into());
    }
    Reply::Tool("subagent", launch.clone())
}

fn launch(description: &str, prompt: &str) -> Value {
    json!({"agent": "general", "description": description, "prompt": prompt})
}

/// A background command that runs until `release` exists, in the shell the tool picks (`compat`): PowerShell, as on
/// Windows by default (upstream's test assumes it there), or a POSIX shell.
fn wait_command(app: &App, release: &std::path::Path) -> String {
    use oc_tools::shell::select;
    if select::ps(&select::resolve(select::Priority::Compat, None, Some(&app.global.bin))) {
        format!("while (!(Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 25 }}; Write-Output done", release.display())
    } else {
        format!("while [ ! -e '{}' ]; do sleep 0.025; done; printf done", release.display())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subagent_reports() {
    let root = std::env::temp_dir().join(format!("oc-subreport-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    for (k, v) in [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache")] {
        unsafe { std::env::set_var(k, root.join(v)) };
    }
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_TEST_HOME", root.join("home"));
        std::env::set_var("OPENCODE_DISABLE_FILEWATCHER", "1");
    }

    let responder: Arc<Mutex<Responder>> = Arc::new(Mutex::new(Box::new(|_: &Request| Reply::Text("idle".into()))));
    let mock = responder.clone();
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let router = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::Json<Value>| {
            let (mock, counter) = (mock.clone(), counter.clone());
            async move {
                let raw = body.0.to_string();
                if raw.contains("You name conversations") {
                    return text("Title");
                }
                let last = body.0["messages"].as_array().and_then(|m| m.last()).cloned().unwrap_or(Value::Null).to_string();
                let req = Request { child: raw.contains("You are a subagent spawned by another session."), last, raw };
                let reply = (mock.lock())(&req);
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                match reply {
                    Reply::Text(t) => text(&t),
                    Reply::Tool(name, args) => tool(name, &args, n),
                    Reply::Gated(gate, t) => {
                        gate.notified().await;
                        text(&t)
                    }
                }
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
            "experimental": {"subagent_depth": 2},
            "permissions": [{"action": "*", "resource": "*", "effect": "allow"}],
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
    let app = App::new(global, db, "2.3.0");
    let engine = Engine::new(app.clone(), &oc_core::services::default_services(&app));
    engine.install();
    let h = Harness { app, engine, project: project.clone(), responder };

    // ---- tool-subagent.test.ts "starts a child session and returns the running handle", and the first report
    let p = h.session("Parent", None).await;
    let args = launch("check things", "look around");
    h.respond(move |req| if req.child { Reply::Text("child done".into()) } else { parent(req, &args) });
    h.prompt(&p, "start").await;
    let reports = h.wait_reports(&p, 1).await;
    let child = h.child_of(&p);
    let msgs = h.messages(&p);
    let Message::Assistant(first) = &msgs[1] else { panic!("{msgs:#?}") };
    let oc_schema::message::AssistantContent::Tool { name, state: oc_schema::message::ToolState::Completed { content, metadata, .. }, .. } = &first.content[0] else {
        panic!("{:?}", first.content)
    };
    assert_eq!(name, "subagent");
    let metadata = metadata.clone().unwrap_or_default();
    assert_eq!((metadata.get("sessionID"), metadata.get("status")), (Some(&json!(child)), Some(&json!("running"))));
    let launched = serde_json::to_value(content).unwrap()[0]["text"].as_str().unwrap().to_string();
    assert_eq!(
        launched,
        [
            format!("The subagent is working in the background (sessionID: {child}). You will receive a <background-result kind=\"subagent\"> message when it goes idle, and again after each later round of work."),
            "DO NOT sleep, poll, proactively check on its progress, or duplicate this subagent's work.".into(),
            "Work on non-overlapping tasks, or briefly tell the user what you launched and end your response.".into(),
            "To send it follow-ups or change its instructions while it runs, call subagent again with this sessionID.".into(),
        ]
        .join("\n")
    );
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].1, format!("<background-result kind=\"subagent\" sessionID=\"{child}\" state=\"completed\" description=\"check things\">\nchild done\n</background-result>"));
    assert_eq!(reports[0].2, json!({"source": "subagent", "childID": child, "agent": "General", "state": "completed"}));
    let info = h.app.sessions.get(&child).unwrap();
    assert_eq!((info.parent_id.as_deref(), info.title.as_deref(), info.agent.as_deref()), (Some(p.as_str()), Some("check things"), Some("general")));
    // The parent answered the report; the outbox row is acknowledged and no claim is left.
    assert!(matches!(h.messages(&p).iter().rev().find(|m| matches!(m, Message::Assistant(_) | Message::Synthetic { .. })), Some(Message::Assistant(a)) if a.content.iter().any(|c| matches!(c, oc_schema::message::AssistantContent::Text { text, .. } if text == "parent ack"))));
    h.idle(&child).await;
    assert!(h.job_rows(&p).is_empty());
    assert!(!h.suspended(&child) && !h.suspended(&p));

    // ---- "reports every directly prompted busy period on a finished background child"
    let answers = Arc::new(Mutex::new(vec!["third answer", "second answer"]));
    let a = answers.clone();
    h.respond(move |req| if req.child { Reply::Text(a.lock().pop().unwrap_or("extra").into()) } else { Reply::Text("parent ack".into()) });
    h.prompt(&child, "Follow up").await;
    let reports = h.wait_reports(&p, 2).await;
    assert!(reports[1].1.contains("second answer") && !reports[1].1.contains("child done"), "{:?}", reports[1]);
    h.prompt(&child, "Follow up again").await;
    let reports = h.wait_reports(&p, 3).await;
    assert!(reports[2].1.contains("third answer") && !reports[2].1.contains("second answer"));
    let ids: std::collections::HashSet<&String> = reports.iter().map(|r| &r.0).collect();
    assert_eq!(ids.len(), 3);
    assert!(h.app.jobs.pending_background().is_empty());

    // ---- "reports once for each background follow-up to an idle child": the parent continues the child by ID
    let p = h.session("Parent", None).await;
    let args = launch("review", "first task");
    let answers = Arc::new(Mutex::new(vec!["second answer", "first answer"]));
    let (a, followed) = (answers.clone(), Arc::new(Mutex::new(false)));
    let f = followed.clone();
    h.respond(move |req| {
        if req.child {
            return Reply::Text(a.lock().pop().unwrap_or("extra").into());
        }
        if req.last.contains(r#"<background-result kind=\"subagent\" "#) && !*f.lock() {
            *f.lock() = true;
            let sid = req.last.split("sessionID=\\\"").nth(1).and_then(|r| r.split("\\\"").next()).unwrap().to_string();
            return Reply::Tool("subagent", json!({"agent": "general", "description": "review", "prompt": "follow-up task", "sessionID": sid}));
        }
        parent(req, &args)
    });
    h.prompt(&p, "start").await;
    let reports = h.wait_reports(&p, 2).await;
    let child = h.child_of(&p);
    assert!(reports[0].1.contains("first answer"));
    assert!(reports[1].1.contains("second answer"));
    assert_ne!(reports[0].0, reports[1].0);
    let child_msgs = h.messages(&child);
    // The follow-up is the prompt itself, without the subagent prefix.
    assert!(child_msgs.iter().any(|m| matches!(m, Message::User { text, .. } if text == "follow-up task")));
    h.idle(&child).await;
    assert!(h.app.jobs.pending_background().is_empty());

    // ---- "reports immediately with running background work and again after the shell exits", with the
    // `state="running"` tag (#63675) only in the model-facing text
    let release = root.join("release-a");
    let command = wait_command(&h.app, &release);
    let p = h.session("Parent", None).await;
    let args = launch("run tests", "start the tests");
    let cmd = command.clone();
    h.respond(move |req| {
        if !req.child {
            return parent(req, &args);
        }
        if req.last.contains(r#"<background-result kind=\"shell\" "#) {
            Reply::Text("Tests green".into())
        } else if req.last.contains("\"tool\"") {
            Reply::Text("Started tests".into())
        } else {
            Reply::Tool("shell", json!({"command": cmd, "background": true, "description": "Wait for release"}))
        }
    });
    h.prompt(&p, "start").await;
    let first = h.wait_reports(&p, 1).await;
    let child = h.child_of(&p);
    assert!(first[0].1.starts_with(&format!("<background-result kind=\"subagent\" sessionID=\"{child}\" state=\"running\" description=\"run tests\">\nStarted tests\n\nBackground work still running:\n- shell (")), "{}", first[0].1);
    assert!(first[0].1.contains(&format!("): {command}\n</background-result>")), "{}", first[0].1);
    assert_eq!(first[0].2["state"], "completed");
    std::fs::write(&release, "go").unwrap();
    let second = h.wait_reports(&p, 2).await;
    assert_ne!(second[1].0, first[0].0);
    assert_eq!(second[1].1, format!("<background-result kind=\"subagent\" sessionID=\"{child}\" state=\"completed\" description=\"run tests\">\nTests green\n</background-result>"));
    h.idle(&child).await;
    assert!(h.app.jobs.pending_background().is_empty());

    // ---- "nested subagents report one level at a time"
    let release = root.join("release-b");
    let command = wait_command(&h.app, &release);
    let p = h.session("Parent", None).await;
    let args = launch("child review", "review the child fixture");
    let cmd = command.clone();
    h.respond(move |req| {
        if !req.child {
            return parent(req, &args);
        }
        // The grandchild's history holds its own prompt; the child's holds its launch of the grandchild.
        let grandchild = req.raw.contains("review the grandchild fixture") && !req.raw.contains("review the child fixture");
        if grandchild {
            if req.last.contains(r#"<background-result kind=\"shell\" "#) {
                Reply::Text("grandchild finished".into())
            } else if req.last.contains("\"tool\"") {
                Reply::Text("grandchild started tests".into())
            } else {
                Reply::Tool("shell", json!({"command": cmd, "background": true, "description": "Wait"}))
            }
        } else if req.last.contains("grandchild finished") {
            Reply::Text("child saw grandchild finish".into())
        } else if req.last.contains(r#"<background-result kind=\"subagent\" "#) {
            Reply::Text("child saw first grandchild report".into())
        } else if req.last.contains("\"tool\"") {
            Reply::Text("child launched grandchild".into())
        } else {
            Reply::Tool("subagent", json!({"agent": "general", "description": "grandchild review", "prompt": "review the grandchild fixture"}))
        }
    });
    h.prompt(&p, "start").await;
    let first_root = h.wait_reports(&p, 1).await;
    let child = h.child_of(&p);
    assert!(first_root[0].1.contains("child launched grandchild") && first_root[0].1.contains("Background work still running:"), "{}", first_root[0].1);
    let first_child = h.wait_reports(&child, 1).await;
    let grandchild = first_child[0].2["childID"].as_str().unwrap().to_string();
    assert!(first_child[0].1.contains("grandchild started tests") && first_child[0].1.contains("Background work still running:\n- shell ("));
    let second_root = h.wait_reports(&p, 2).await;
    assert!(second_root[1].1.contains("child saw first grandchild report"), "{}", second_root[1].1);
    assert!(second_root[1].1.contains("Background work still running:\n- shell (") && second_root[1].1.contains(&format!(", in {grandchild})")), "{}", second_root[1].1);
    std::fs::write(&release, "go").unwrap();
    let child_reports = h.wait_reports(&child, 2).await;
    for _ in 0..500 {
        if h.reports(&p).iter().any(|r| r.1.contains("child saw grandchild finish")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.idle(&p).await;
    let root_reports = h.reports(&p);
    assert!(child_reports[1].1.contains("grandchild finished") && !child_reports[1].1.contains("Background work still running:"));
    assert!(child_reports.iter().all(|r| r.2["childID"] == grandchild));
    assert!(root_reports.iter().any(|r| r.1.contains("child saw grandchild finish")));
    assert!(root_reports.iter().all(|r| r.2["childID"] == child));
    assert!(!root_reports.iter().any(|r| r.1.contains(&format!("<background-result kind=\"subagent\" sessionID=\"{grandchild}\""))));
    h.idle(&child).await;
    h.idle(&grandchild).await;
    assert!(h.app.jobs.pending_background().is_empty());

    // ---- a user interruption of a running child reports "Subagent cancelled" and releases its claim
    let gate = Arc::new(tokio::sync::Notify::new());
    let p = h.session("Parent", None).await;
    let args = launch("slow work", "take your time");
    let g = gate.clone();
    h.respond(move |req| if req.child { Reply::Gated(g.clone(), "too late".into()) } else { parent(req, &args) });
    h.prompt(&p, "start").await;
    let mut child = String::new();
    for _ in 0..500 {
        child = h.app.db.with(|c| c.query_row("SELECT coalesce(max(id), '') FROM session_v2 WHERE parent_id = ?", [&p], |r| r.get(0))).unwrap();
        if !child.is_empty() && h.app.coordinator().is_active(&child) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(h.job_rows(&p).len(), 1, "the job row is written when the job starts");
    assert_eq!(h.job_rows(&p)[0].1, "running");
    h.app.coordinator().interrupt(&child, Some(Reason::User), true).await;
    let reports = h.wait_reports(&p, 1).await;
    assert_eq!(reports[0].1, format!("<background-result kind=\"subagent\" sessionID=\"{child}\" state=\"cancelled\" description=\"slow work\">\nSubagent cancelled\n</background-result>"));
    assert_eq!(reports[0].2["state"], "cancelled");
    gate.notify_waiters();
    assert!(!h.suspended(&child));
    assert!(h.job_rows(&p).is_empty());

    // ---- "reports a directly prompted child when it goes idle": a child created over the API, without a job
    let p = h.session("Parent", None).await;
    let child = h.session("Direct child", Some(&p)).await;
    h.respond(|req| if req.child || req.last.contains("direct work") { Reply::Text("direct answer".into()) } else { Reply::Text("parent ack".into()) });
    h.prompt(&child, "direct work").await;
    let reports = h.wait_reports(&p, 1).await;
    assert_eq!(reports[0].1, format!("<background-result kind=\"subagent\" sessionID=\"{child}\" state=\"completed\" description=\"Direct child\">\ndirect answer\n</background-result>"));
    assert_eq!(reports[0].2, json!({"source": "subagent", "childID": child, "agent": "general", "state": "completed"}));
    assert!(h.job_rows(&p).is_empty());
    // A control-only period (nothing new from the model) stays quiet.
    let before = h.reports(&p).len();
    h.app.coordinator().wake(&child, Scope::Input);
    h.idle(&child).await;
    assert_eq!(h.reports(&p).len(), before);

    // ---- session-background.test.ts: `SessionBackground.outstanding` with active executions
    outstanding_cases(&h).await;

    // ---- subagent-report.test.ts: failures, a deleted parent, and the outbox written in the commit
    settlement_cases(&h).await;

    // ---- session-execution.test.ts restart recovery of subagents
    restart_cases(&h).await;

    std::fs::remove_dir_all(&root).ok();
}

async fn outstanding_cases(h: &Harness) {
    let create = |parent: Option<String>, agent: Option<&str>, title: &str| {
        let location = parent.is_none().then(|| LocationRef { directory: h.project.display().to_string(), workspace_id: None });
        h.app.sessions.create(CreateInput { location, parent_id: parent, title: Some(title.into()), agent: agent.map(str::to_string), ..Default::default() })
    };
    let root = create(None, Some("build"), "Root").await.unwrap().id;
    let child = create(Some(root.clone()), Some("explore"), "Child work").await.unwrap().id;
    // A child without an agent or a title falls back to "subagent" / "Subagent".
    let grandchild = create(Some(child.clone()), None, "").await.unwrap().id;
    h.app.db.with(|c| c.execute("UPDATE session_v2 SET agent = NULL, title = NULL WHERE id = ?", [&grandchild])).unwrap();
    let other = create(None, Some("build"), "Unrelated").await.unwrap().id;
    let item = |session: &str, child: &str, agent: &str, description: &str| {
        json!({"kind": "subagent", "sessionID": session, "childSessionID": child, "agent": agent, "description": description})
    };
    // "includes active descendants with their agent and title"
    let active = [root.clone(), child.clone(), grandchild.clone(), other.clone()];
    assert_eq!(
        oc_core::job::outstanding(&h.app, &root, &active),
        vec![item(&root, &child, "explore", "Child work"), item(&child, &grandchild, "subagent", "Subagent")]
    );
    // "does not duplicate running background children or include itself and unrelated sessions"
    h.app.jobs.start_input(oc_core::job::StartInput {
        id: child.clone(),
        kind: "subagent".into(),
        title: None,
        metadata_session: None,
        recovery: Some(JobRecovery::Subagent { parent_session_id: root.clone(), child_session_id: child.clone(), agent: "explore".into(), description: "Already running".into(), outstanding: None }),
        notification_id: None,
        background: true,
        run: Box::pin(futures::future::pending()),
    });
    assert_eq!(oc_core::job::outstanding(&h.app, &root, &[root.clone(), child.clone(), other.clone()]), vec![item(&root, &child, "explore", "Already running")]);
    assert!(oc_core::job::outstanding(&h.app, &other, &[root.clone(), other.clone()]).is_empty());
    // "skips cancelled descendants when job state is available"
    h.app.jobs.cancel(&child);
    assert_eq!(oc_core::job::outstanding(&h.app, &root, &[child.clone(), grandchild.clone()]), vec![item(&child, &grandchild, "subagent", "Subagent")]);
    h.app.jobs.cancel_session(&root);
}

fn recovery(parent: &str, child: &str, description: &str) -> JobRecovery {
    JobRecovery::Subagent { parent_session_id: parent.into(), child_session_id: child.into(), agent: "general".into(), description: description.into(), outstanding: None }
}

fn insert_row(app: &App, nid: &str, recovery: &JobRecovery, status: &str, output: Option<&str>, error: Option<&str>) {
    let JobRecovery::Subagent { parent_session_id, child_session_id, description, .. } = recovery else { unreachable!() };
    app.db
        .with(|c| {
            c.execute(
                "INSERT INTO job (notification_id, id, session_id, child_session_id, kind, title, status, output, error, recovery, time_created, time_updated) VALUES (?1, ?2, ?3, ?2, 'subagent', ?4, ?5, ?6, ?7, ?8, 1, 1)",
                rusqlite::params![nid, child_session_id, parent_session_id, description, status, output, error, serde_json::to_string(recovery).unwrap()],
            )
        })
        .unwrap();
}

fn claim(app: &App, session: &str, attempts: i64) {
    app.db.with(|c| c.execute("UPDATE session_v2 SET time_suspended = 1, resume_attempts = ? WHERE id = ?", rusqlite::params![attempts, session])).unwrap();
}

/// Settles a child's busy period by hand: the report's prepare, the terminal commit and delivery.
async fn settle(h: &Harness, report: &SubagentReport, child: &str, outcome: Outcome) -> anyhow::Result<()> {
    let event = match &outcome {
        Outcome::Succeeded => SessionEvent::ExecutionSucceeded { session_id: child.into() },
        Outcome::Failed(m) => SessionEvent::ExecutionFailed { session_id: child.into(), error: oc_schema::types::SessionError::new("unknown", m) },
        Outcome::Interrupted => SessionEvent::ExecutionInterrupted { session_id: child.into(), reason: oc_schema::event::InterruptReason::User },
    };
    let settlement = report.prepare(&h.app, child, outcome);
    h.app.bus.commit_with(vec![event.into()], |c, _, _| settlement.commit(c))?;
    settlement.deliver(&h.app).await
}

async fn settlement_cases(h: &Harness) {
    h.respond(|_| Reply::Text("parent ack".into()));
    let report = SubagentReport::default();

    // "reports failures and user interruptions": the body is the error, the job row is the outbox until delivery.
    let p = h.session("Parent", None).await;
    let child = h.session("Failing child", Some(&p)).await;
    claim(&h.app, &child, 0);
    settle(h, &report, &child, Outcome::Failed("provider exploded".into())).await.unwrap();
    let reports = h.wait_reports(&p, 1).await;
    assert_eq!(reports[0].1, format!("<background-result kind=\"subagent\" sessionID=\"{child}\" state=\"error\" description=\"Failing child\">\nprovider exploded\n</background-result>"));
    assert!(!h.suspended(&child));
    assert!(h.job_rows(&p).is_empty());

    // A backgrounded job's report keeps its notification ID and start time.
    let rec = recovery(&p, &child, "Failing child");
    let started = Arc::new(tokio::sync::Notify::new());
    h.app.jobs.start_input(oc_core::job::StartInput {
        id: child.clone(),
        kind: "subagent".into(),
        title: Some("Failing child".into()),
        metadata_session: None,
        recovery: Some(rec.clone()),
        notification_id: Some("msg_report_fixed".into()),
        background: true,
        run: {
            let s = started.clone();
            Box::pin(async move {
                s.notified().await;
                Ok(String::new())
            })
        },
    });
    assert_eq!(h.job_rows(&p), vec![("msg_report_fixed".to_string(), "running".to_string())]);
    settle(h, &report, &child, Outcome::Interrupted).await.unwrap();
    let reports = h.wait_reports(&p, 2).await;
    assert_eq!(reports[1].0, "msg_report_fixed");
    assert_eq!(reports[1].2["state"], "cancelled");
    assert!(h.job_rows(&p).is_empty());
    started.notify_waiters();

    // "keeps an automation run's child quiet unless the parent followed up through a Job" (2.3.1): the child an
    // automation starts for a run (`ses_<aut>_<run>`) finishes without reporting to or waking its parent.
    let p = h.session("Automation parent", None).await;
    let child = h
        .app
        .sessions
        .create(CreateInput {
            id: Some(oc_core::automation::run_session_id("aut_quiet", 1)),
            parent_id: Some(p.clone()),
            title: Some("Morning triage".into()),
            agent: Some("general".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    claim(&h.app, &child, 0);
    // A failure carries a body that a regular child would report.
    settle(h, &report, &child, Outcome::Failed("triage failed".into())).await.unwrap();
    let inbox = |session: &str| -> i64 {
        h.app.db.with(|c| c.query_row("SELECT count(*) FROM session_inbox WHERE session_id = ?", [session], |r| r.get(0))).unwrap()
    };
    assert_eq!((inbox(&p), h.reports(&p).len()), (0, 0));
    assert!(h.job_rows(&p).is_empty());
    assert!(!h.suspended(&child));
    // A follow-up the parent sends with the subagent tool runs as a Job, and that reports as usual.
    let started = Arc::new(tokio::sync::Notify::new());
    h.app.jobs.start_input(oc_core::job::StartInput {
        id: child.clone(),
        kind: "subagent".into(),
        title: Some("Morning triage".into()),
        metadata_session: None,
        recovery: Some(recovery(&p, &child, "Morning triage")),
        notification_id: None,
        background: true,
        run: {
            let s = started.clone();
            Box::pin(async move {
                s.notified().await;
                Ok(String::new())
            })
        },
    });
    settle(h, &report, &child, Outcome::Failed("follow-up failed".into())).await.unwrap();
    let reports = h.wait_reports(&p, 1).await;
    assert!(reports[0].1.contains("follow-up failed"), "{reports:?}");
    started.notify_waiters();

    // "acknowledges a parent deleted before or after commit": a report owed to nobody is dropped with its row.
    let p = h.session("Doomed parent", None).await;
    let child = h.session("Orphan", Some(&p)).await;
    h.app.jobs.start_input(oc_core::job::StartInput {
        id: child.clone(),
        kind: "subagent".into(),
        title: None,
        metadata_session: None,
        recovery: Some(recovery(&p, &child, "Orphan")),
        notification_id: None,
        background: true,
        run: Box::pin(futures::future::pending()),
    });
    assert_eq!(h.job_rows(&child).len(), 1);
    h.app.bus.commit_one(SessionEvent::Deleted { session_id: p.clone() }).unwrap();
    settle(h, &report, &child, Outcome::Succeeded).await.unwrap();
    assert!(h.job_rows(&child).is_empty());

    // "rolls back the idle projection and claim release when the report insert fails"
    let p = h.session("Parent", None).await;
    let child = h.session("Child", Some(&p)).await;
    claim(&h.app, &child, 0);
    h.app.db.with(|c| c.execute_batch("CREATE TRIGGER fail_job BEFORE INSERT ON job BEGIN SELECT RAISE(ABORT, 'job insert failed'); END;")).unwrap();
    let failed = settle(h, &report, &child, Outcome::Failed("boom".into())).await;
    h.app.db.with(|c| c.execute_batch("DROP TRIGGER fail_job;")).unwrap();
    assert!(failed.is_err());
    assert!(h.suspended(&child), "the claim release rolled back with the report");
    assert!(h.messages(&child).iter().all(|m| !matches!(m, Message::Idle { .. })));
    settle(h, &report, &child, Outcome::Failed("boom".into())).await.unwrap();
    assert!(!h.suspended(&child));

    // "writes subagent outbox rows with pure SQL upserts outside the live registry"
    let rec = recovery("ses_outbox_parent", "ses_outbox_child", "Outbox report");
    let row = |status: JobStatus, output: Option<&str>, error: Option<&str>, started: Option<u64>| SubagentOutboxReport {
        notification_id: "msg_outbox".into(),
        recovery: rec.clone(),
        status,
        output: output.map(str::to_string),
        error: error.map(str::to_string),
        started_at: started,
    };
    h.app.db.with(|c| Jobs::record_subagent_report(c, &row(JobStatus::Completed, Some("report body"), None, Some(1234)))).unwrap();
    assert!(h.app.jobs.get("ses_outbox_child").is_none());
    h.app.db.with(|c| Jobs::record_subagent_report(c, &row(JobStatus::Error, None, Some("failed"), None))).unwrap();
    let stored: (String, String, String, String, Option<String>, Option<String>, i64) = h
        .app
        .db
        .with(|c| c.query_row("SELECT id, session_id, child_session_id, status, output, error, time_created FROM job WHERE notification_id = 'msg_outbox'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))))
        .unwrap();
    assert_eq!(stored, ("ses_outbox_child".into(), "ses_outbox_parent".into(), "ses_outbox_child".into(), "error".into(), None, Some("failed".into()), 1234));
    h.app.db.with(|c| Jobs::delete_background_row(c, "msg_outbox")).unwrap();
    assert!(h.app.jobs.pending_background().iter().all(|v| v["notificationID"] != "msg_outbox"));
}

async fn restart_cases(h: &Harness) {
    // "resumes a background subagent and notifies its parent exactly once": a child killed mid-turn keeps its claim
    // and running row; the sweep counts its attempt, starts its job with the row's notification ID, and the report
    // reaches the parent once.
    let p = h.session("Parent", None).await;
    let child = h.session("Resumed child", Some(&p)).await;
    h.app.sessions.prompt(&child, PromptInput { text: "You are a subagent spawned by another session.\nresume me".into(), ..Default::default() }).await.unwrap();
    claim(&h.app, &child, 0);
    insert_row(&h.app, "msg_resumed_row", &recovery(&p, &child, "Resumed child"), "running", None, None);
    h.respond(|req| if req.child { Reply::Text("resumed answer".into()) } else { Reply::Text("parent ack".into()) });
    let resumed = h.engine.resume_suspended().await.unwrap();
    assert_eq!(resumed, 1);
    let reports = h.wait_reports(&p, 1).await;
    h.idle(&child).await;
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].0, "msg_resumed_row");
    assert_eq!(reports[0].1, format!("<background-result kind=\"subagent\" sessionID=\"{child}\" state=\"completed\" description=\"Resumed child\">\nresumed answer\n</background-result>"));
    let notices: Vec<String> = h.messages(&child).into_iter().filter_map(|m| match m { Message::Synthetic { text, .. } => Some(text), _ => None }).collect();
    assert_eq!(notices, vec![oc_core::restart::RESTART_TEXT.to_string()]);
    assert!(h.job_rows(&p).is_empty());
    assert!(!h.suspended(&child));

    // "delivers a completed outbox row at boot without touching the child"
    let p = h.session("Parent", None).await;
    let child = h.session("Finished child", Some(&p)).await;
    // 2.3.1 (#64026): the row keeps the raw body and the outstanding-work snapshot the replay formats from.
    let JobRecovery::Subagent { parent_session_id, child_session_id, agent, description, .. } = recovery(&p, &child, "Finished child") else { unreachable!() };
    let snapshot = JobRecovery::Subagent {
        outstanding: Some(vec![serde_json::json!({"kind": "shell", "sessionID": child.clone(), "shellID": "sh_1", "command": "sleep 9"})]),
        parent_session_id,
        child_session_id,
        agent,
        description,
    };
    insert_row(&h.app, "msg_outbox_row", &snapshot, "completed", Some("stored answer"), None);
    h.engine.resume_suspended().await.unwrap();
    let reports = h.wait_reports(&p, 1).await;
    assert_eq!(reports[0].0, "msg_outbox_row");
    assert_eq!(
        reports[0].1,
        format!("<background-result kind=\"subagent\" sessionID=\"{child}\" state=\"running\" description=\"Finished child\">\nstored answer\n\nBackground work still running:\n- shell (sh_1): sleep 9\n</background-result>")
    );
    assert!(h.messages(&child).is_empty());
    assert!(h.job_rows(&p).is_empty());

    // An older row without the snapshot replays its recorded status and keeps its full body.
    let p = h.session("Parent", None).await;
    let child = h.session("Older child", Some(&p)).await;
    let body = "stored answer\n\nBackground work still running:\n- shell (sh_1): sleep 9";
    insert_row(&h.app, "msg_older_row", &recovery(&p, &child, "Older child"), "completed", Some(body), None);
    h.engine.resume_suspended().await.unwrap();
    let reports = h.wait_reports(&p, 1).await;
    assert_eq!(reports[0].1, format!("<background-result kind=\"subagent\" sessionID=\"{child}\" state=\"completed\" description=\"Older child\">\n{body}\n</background-result>"));

    // "resumes a directly driven child after execution teardown": a claimed child without a job resumes like a root
    // and reports with a fresh notification ID.
    let p = h.session("Parent", None).await;
    let child = h.session("Direct child", Some(&p)).await;
    h.app.sessions.prompt(&child, PromptInput { text: "direct work".into(), ..Default::default() }).await.unwrap();
    claim(&h.app, &child, 0);
    h.respond(|req| if req.last.contains(r#"<background-result kind=\"subagent\" "#) { Reply::Text("parent ack".into()) } else { Reply::Text("direct resumed".into()) });
    assert_eq!(h.engine.resume_suspended().await.unwrap(), 1);
    let reports = h.wait_reports(&p, 1).await;
    assert!(reports[0].1.contains("direct resumed"), "{}", reports[0].1);
    assert!(!h.suspended(&child));

    // "leaves a child parked when its parent is gone": an orphaned child's claim is released without a resume.
    let p = h.session("Gone parent", None).await;
    let child = h.session("Orphan", Some(&p)).await;
    claim(&h.app, &child, 0);
    h.app.db.with(|c| c.execute("DELETE FROM session_v2 WHERE id = ?", [&p])).unwrap();
    assert_eq!(h.engine.resume_suspended().await.unwrap(), 0);
    assert!(!h.suspended(&child));
    assert!(h.messages(&child).is_empty());

    // "terminalizes a recovered subagent that exhausts its resume budget"
    let p = h.session("Parent", None).await;
    let child = h.session("Exhausted", Some(&p)).await;
    claim(&h.app, &child, 10);
    insert_row(&h.app, "msg_exhausted_row", &recovery(&p, &child, "Exhausted"), "running", None, None);
    h.respond(|_| Reply::Text("parent ack".into()));
    assert_eq!(h.engine.resume_suspended().await.unwrap(), 0);
    let reports = h.wait_reports(&p, 1).await;
    assert_eq!(
        reports[0].1,
        format!("<background-result kind=\"subagent\" sessionID=\"{child}\" state=\"error\" description=\"Exhausted\">\nExecution was interrupted repeatedly and will not be resumed automatically.\n</background-result>")
    );
    assert!(!h.suspended(&child));
    assert!(h.job_rows(&p).is_empty());

    // "honors a job cancellation persisted before its child could release the claim"
    let p = h.session("Parent", None).await;
    let child = h.session("Cancelled", Some(&p)).await;
    claim(&h.app, &child, 0);
    insert_row(&h.app, "msg_cancelled_row", &recovery(&p, &child, "Cancelled"), "cancelled", None, None);
    assert_eq!(h.engine.resume_suspended().await.unwrap(), 0);
    let reports = h.wait_reports(&p, 1).await;
    assert_eq!(reports[0].1, format!("<background-result kind=\"subagent\" sessionID=\"{child}\" state=\"cancelled\" description=\"Cancelled\">\nSubagent cancelled\n</background-result>"));
    assert!(!h.suspended(&child));
    assert!(h.messages(&child).is_empty());

    // "charges a suspended parent's restart attempt before a fast child can wake it": the parent is suspended too;
    // the replayed report does not wake it before its own resume was counted, and it resumes once.
    let p = h.session("Suspended parent", None).await;
    h.app.sessions.prompt(&p, PromptInput { text: "keep going".into(), ..Default::default() }).await.unwrap();
    let child = h.session("Fast child", Some(&p)).await;
    claim(&h.app, &p, 0);
    insert_row(&h.app, "msg_fast_row", &recovery(&p, &child, "Fast child"), "completed", Some("fast answer"), None);
    h.respond(|_| Reply::Text("parent resumed".into()));
    assert_eq!(h.engine.resume_suspended().await.unwrap(), 1);
    h.wait_reports(&p, 1).await;
    let attempts: i64 = h.app.db.with(|c| c.query_row("SELECT resume_attempts FROM session_v2 WHERE id = ?", [&p], |r| r.get(0))).unwrap();
    assert_eq!(attempts, 0, "the resumed turn's terminal reset the counter");
    let restarts = h
        .messages(&p)
        .iter()
        .filter(|m| matches!(m, Message::Synthetic { text, .. } if text == oc_core::restart::RESTART_TEXT))
        .count();
    assert_eq!(restarts, 1);
    assert!(!h.suspended(&p));
}
