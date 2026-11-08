//! opencode 2.2.5 Code Mode changes (`tool-execute.test.ts`, `tool-registry.test.ts`): `fetch` asks the webfetch
//! permission (#59954), progress is sent at most every 100 ms (#60120), nested tools cannot replace the call rows
//! (#60480), and programs see the same failure text as direct calls (#60109).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::{Router, routing::any};
use oc_core::codemode;
use oc_schema::Metadata;
use oc_tools::host::{PermissionError, PermissionRequest, ToolHost};
use oc_tools::tool::{DefinitionContext, ErrorCause, ToolConfig, ToolOptions};
use oc_tools::{Registry, Tool, ToolContext, ToolDefinition, ToolError, ToolResult};
use serde_json::{Value, json};

/// Answers every permission ask with `decision` and records the asks.
struct Host {
    decision: Result<(), PermissionError>,
    asked: Mutex<Vec<PermissionRequest>>,
}

#[async_trait]
impl ToolHost for Host {
    async fn ask_permission(&self, request: PermissionRequest) -> Result<(), PermissionError> {
        self.asked.lock().unwrap().push(request);
        self.decision.clone()
    }
}

fn host(decision: Result<(), PermissionError>) -> Arc<Host> {
    Arc::new(Host { decision, asked: Mutex::new(Vec::new()) })
}

/// `step` returns at once; `wait` sleeps `wait_ms` (or forever); `fail` fails with an unwrapped cause; `noisy`
/// reports its own progress.
struct Fixture {
    name: &'static str,
    wait_ms: Option<u64>,
    started: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Tool for Fixture {
    fn name(&self) -> &str {
        self.name
    }
    fn options(&self) -> ToolOptions {
        ToolOptions { codemode: true, ..Default::default() }
    }
    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        ToolDefinition::new(self.name, self.name, json!({ "type": "object", "properties": {} })).with_output(Some(json!({ "type": "string" })))
    }
    async fn execute(&self, _input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        match self.name {
            "wait" => {
                self.started.notify_one();
                match self.wait_ms {
                    Some(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
                    None => std::future::pending::<()>().await,
                }
            }
            "fail" => return Err(ToolError::with_cause("Unable to read thing", ErrorCause::Error("Resource not found: thing".into()))),
            "noisy" => {
                let mut stage = Metadata::new();
                stage.insert("stage".into(), json!("old"));
                ctx.progress(stage);
            }
            _ => {}
        }
        Ok(ToolResult::text("ok").with_output(json!("ok")))
    }
}

fn snapshot(wait_ms: Option<u64>, started: Arc<tokio::sync::Notify>) -> oc_tools::Snapshot {
    let mut registry = Registry::new();
    for name in ["step", "wait", "fail", "noisy"] {
        registry.add(Arc::new(Fixture { name, wait_ms, started: started.clone() })).unwrap();
    }
    registry.snapshot(&[])
}

fn context(host: Arc<Host>, progress: Arc<Mutex<Vec<Metadata>>>) -> ToolContext {
    let dir = std::env::temp_dir();
    ToolContext {
        session_id: "ses_execute".into(),
        message_id: "msg_execute".into(),
        call_id: "call_execute".into(),
        agent: "build".into(),
        directory: dir.clone(),
        worktree: dir,
        project_id: "prj".into(),
        workspace_id: None,
        global: Arc::new(oc_util::Global::resolve()),
        cancel: tokio_util::sync::CancellationToken::new(),
        progress: Arc::new(move |m| progress.lock().unwrap().push(m)),
        config: Arc::new(ToolConfig::default()),
        host,
    }
}

fn rows(progress: &Mutex<Vec<Metadata>>) -> Vec<Value> {
    progress.lock().unwrap().iter().map(|m| Value::Object(m.clone().into_iter().collect())).collect()
}

fn row(tool: &str, status: &str) -> Value {
    json!({ "tool": tool, "status": status })
}

/// Progress updates with the time each was sent.
fn timed_context(host: Arc<Host>, sent: Arc<Mutex<Vec<(std::time::Instant, Value)>>>) -> ToolContext {
    let mut ctx = context(host, Default::default());
    ctx.progress = Arc::new(move |m| sent.lock().unwrap().push((std::time::Instant::now(), Value::Object(m.into_iter().collect()))));
    ctx
}

/// The updates are at least 100 ms apart (allowing for timer granularity).
fn assert_throttled(sent: &[(std::time::Instant, Value)]) {
    for pair in sent.windows(2) {
        assert!(pair[1].0 - pair[0].0 >= Duration::from_millis(95), "{:?}", sent.iter().map(|(_, v)| v).collect::<Vec<_>>());
    }
}

/// "execute sends at most one progress update per 100 ms with the latest rows". The exact updates depend on how
/// fast the program runs, so this checks the rule rather than upstream's TestClock sequence: the first rows go out
/// at once, updates are 100 ms apart, the trailing one carries the latest rows, and a success drops a pending one.
#[tokio::test]
async fn progress_is_sent_at_most_every_100_ms() {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let result = codemode::execute(
        &snapshot(Some(250), Default::default()),
        json!({ "code": "for (const i of [1, 2, 3]) await tools.step({}); await tools.wait({}); return await tools.step({})" }),
        timed_context(host(Ok(())), sent.clone()),
    )
    .await
    .unwrap();
    let at_end = sent.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sent = sent.lock().unwrap().clone();
    assert_eq!(sent.len(), at_end, "a success drops the pending update");
    assert_eq!(sent[0].1, json!({ "toolCalls": [row("step", "running")] }));
    assert_throttled(&sent);
    let steps = [row("step", "completed"), row("step", "completed"), row("step", "completed")];
    let with = |last: Value| json!({ "toolCalls": steps.iter().cloned().chain([last]).collect::<Vec<_>>() });
    // While the 250 ms wait runs, a trailing update reports it; when it ends long after, its row goes out at once.
    assert!(sent.iter().any(|(_, v)| *v == with(row("wait", "running"))), "{sent:?}");
    assert!(sent.iter().any(|(_, v)| *v == with(row("wait", "completed"))), "{sent:?}");
    let all: Vec<Value> = steps.iter().cloned().chain([row("wait", "completed"), row("step", "completed")]).collect();
    assert_eq!(result.metadata.unwrap()["toolCalls"], json!(all));
}

/// "interrupted execute sends its latest progress rows before it settles"
#[tokio::test]
async fn interrupted_execute_sends_its_latest_rows() {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let started = Arc::new(tokio::sync::Notify::new());
    let ctx = timed_context(host(Ok(())), sent.clone());
    let cancel = ctx.cancel.clone();
    let snapshot = snapshot(None, started.clone());
    let run = tokio::spawn(async move {
        codemode::execute(&snapshot, json!({ "code": "await tools.step({}); await tools.step({}); return await tools.wait({})" }), ctx).await
    });
    started.notified().await;
    cancel.cancel();
    assert!(run.await.unwrap().is_err());
    let at_end = sent.lock().unwrap().clone();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sent = sent.lock().unwrap().clone();
    assert_eq!(sent.len(), at_end.len(), "nothing is sent after the execution settled");
    assert_eq!(sent[0].1, json!({ "toolCalls": [row("step", "running")] }));
    assert_eq!(
        sent.last().unwrap().1,
        json!({ "toolCalls": [row("step", "completed"), row("step", "completed"), row("wait", "error")] })
    );
}

/// tool-registry.test.ts: an inner tool's own progress would replace the call rows, so it is not published.
#[tokio::test]
async fn nested_progress_does_not_replace_the_call_rows() {
    let progress = Arc::new(Mutex::new(Vec::new()));
    let result = codemode::execute(&snapshot(None, Default::default()), json!({ "code": "return await tools.noisy({})" }), context(host(Ok(())), progress.clone()))
        .await
        .unwrap();
    let rows = rows(&progress);
    assert_eq!(rows[0], json!({ "toolCalls": [row("noisy", "running")] }));
    assert!(rows.iter().all(|r| r.get("stage").is_none() && r.get("toolCalls").is_some()), "{rows:?}");
    assert_eq!(result.metadata.unwrap()["toolCalls"], json!([row("noisy", "completed")]));
}

/// Programs see the failure text the model gets for a direct call: the unwrapped cause.
#[tokio::test]
async fn programs_see_the_direct_call_failure_text() {
    let result = codemode::execute(
        &snapshot(None, Default::default()),
        json!({ "code": "try { await tools.fail({}) } catch (e) { return e.message }" }),
        context(host(Ok(())), Default::default()),
    )
    .await
    .unwrap();
    assert_eq!(result.output.unwrap()["output"], json!("Resource not found: thing"));
}

/// "fetch permissions": the webfetch permission is asked for the URL as the execute call; a correction reaches
/// the program as the error message, and a denied URL is never requested.
#[tokio::test]
async fn fetch_asks_the_webfetch_permission() {
    let hits = Arc::new(Mutex::new(Vec::new()));
    let seen = hits.clone();
    let app = Router::new().fallback(any(move |request: axum::extract::Request| {
        let seen = seen.clone();
        async move {
            seen.lock().unwrap().push(format!("{} {}", request.method(), request.uri().path()));
            "ok"
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let execute = |code: String, host: Arc<Host>| async move {
        let result = codemode::execute(&snapshot(None, Default::default()), json!({ "code": code }), context(host, Default::default())).await.unwrap();
        result.output.unwrap()["output"].as_str().unwrap().to_string()
    };

    let url = format!("{origin}/allowed");
    let allowed = host(Ok(()));
    assert_eq!(execute(format!(r#"return await (await fetch("{url}", {{ method: "post" }})).text()"#), allowed.clone()).await, "ok");
    assert_eq!(*hits.lock().unwrap(), vec!["POST /allowed".to_string()]);
    let asked = allowed.asked.lock().unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!((asked[0].action.as_str(), &asked[0].resources, &asked[0].save), ("webfetch", &vec![url.clone()], &Some(vec!["*".to_string()])));
    assert_eq!(Value::Object(asked[0].metadata.clone().unwrap().into_iter().collect()), json!({ "method": "POST", "url": url }));
    assert_eq!((asked[0].session_id.as_str(), asked[0].agent.as_deref()), ("ses_execute", Some("build")));
    assert_eq!((asked[0].source.message_id.as_str(), asked[0].source.call_id.as_str()), ("msg_execute", "call_execute"));
    drop(asked);

    hits.lock().unwrap().clear();
    let url = format!("{origin}/corrected");
    let corrected = host(Err(PermissionError::Corrected { feedback: "use the docs site instead".into() }));
    assert_eq!(
        execute(format!(r#"try {{ await fetch("{url}") }} catch (e) {{ return e.message }}"#), corrected).await,
        format!("The user rejected fetching {url}: use the docs site instead")
    );
    let url = format!("{origin}/denied");
    let denied = host(Err(PermissionError::Blocked { message: "Permission denied: webfetch".into() }));
    assert_eq!(
        execute(format!(r#"const [denied] = await Promise.allSettled([fetch("{url}")]); return denied.status + ": " + denied.reason.message"#), denied).await,
        "rejected: Permission denied: webfetch"
    );
    assert!(hits.lock().unwrap().is_empty());

    // A declined ask ends the execution like a declined tool call.
    let declined = host(Err(PermissionError::Declined));
    let error = codemode::execute(
        &snapshot(None, Default::default()),
        json!({ "code": format!(r#"try {{ await fetch("{origin}/declined") }} catch {{}} return "kept going""#) }),
        context(declined, Default::default()),
    )
    .await
    .unwrap_err();
    assert!(error.is_defect());
    assert!(hits.lock().unwrap().is_empty());
}
