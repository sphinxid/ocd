//! `experimental.full_codemode` (2.3.1): the built-in tools Code Mode programs call (`tools.shell`,
//! `read`, `edit`, `write`, `patch`, `websearch`, the first five pinned), grep, glob and webfetch hidden, and slow
//! scripts moved to the background as `codemode` jobs whose result arrives as a `<codemode>` message
//! (`tool-shell.test.ts`, `tool-read.test.ts`, `tool-search.test.ts`, `tool-webfetch.test.ts`,
//! `subagent-idle-e2e.test.ts` "Code Mode session jobs").

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::{body::Body, response::Response, routing::post, Router};
use oc_core::{app::App, inbox::Scope, runner::Engine, session::{CreateInput, PromptInput}};
use oc_schema::message::{AssistantContent, ToolState};
use oc_schema::types::LocationRef;
use oc_schema::Message;
use oc_tools::tool::{DefinitionContext, ToolOptions};
use oc_tools::{Tool, ToolContext, ToolDefinition, ToolError, ToolResult};
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

fn tool_call(id: &str, name: &str, args: Value) -> Vec<Value> {
    vec![
        json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":""}}]}}]}),
        json!({"id":"c","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":args.to_string()}}]}}]}),
        json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
    ]
}

fn text(text: &str) -> Vec<Value> {
    vec![
        json!({"id":"t","choices":[{"index":0,"delta":{"role":"assistant","content":text}}]}),
        json!({"id":"t","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}),
    ]
}

/// The scripted provider: each main request takes the next turn and is recorded.
#[derive(Default)]
struct Mock {
    turns: Mutex<VecDeque<Vec<Value>>>,
    requests: Mutex<Vec<Value>>,
}

/// A Code Mode tool that waits for a release (upstream's `hold`).
struct Hold {
    started: tokio::sync::mpsc::UnboundedSender<()>,
    release: Arc<tokio::sync::Semaphore>,
}

#[async_trait]
impl Tool for Hold {
    fn name(&self) -> &str {
        "hold"
    }
    fn options(&self) -> ToolOptions {
        ToolOptions { codemode: true, ..ToolOptions::default() }
    }
    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        ToolDefinition::new("hold", "Wait for release", json!({"type": "object", "properties": {}}))
    }
    async fn execute(&self, _input: Value, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let _ = self.started.send(());
        self.release.acquire().await.unwrap().forget();
        Ok(ToolResult::text("done"))
    }
}

fn tool_part(app: &App, session: &str, call: &str) -> Option<AssistantContent> {
    app.db.session_messages(session).unwrap().into_iter().find_map(|(_, m)| match m {
        Message::Assistant(a) => a.content.into_iter().find(|p| matches!(p, AssistantContent::Tool { id, .. } if id == call)),
        _ => None,
    })
}

fn completed_text(part: Option<AssistantContent>) -> String {
    match part {
        Some(AssistantContent::Tool { state: ToolState::Completed { content, .. }, .. }) => content
            .iter()
            .filter_map(|c| match c {
                oc_schema::ToolContent::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("not completed: {other:?}"),
    }
}

async fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn idle(app: &App, session: &str) {
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::timeout(Duration::from_secs(60), app.coordinator().await_idle(session)).await.unwrap();
}

#[tokio::test]
async fn full_codemode_end_to_end() {
    let root = std::env::temp_dir().join(format!("oc-full-codemode-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for (k, v) in [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache")] {
        unsafe { std::env::set_var(k, root.join(v)) };
    }
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_TEST_HOME", root.join("home"));
        std::env::set_var("OPENCODE_DISABLE_FILEWATCHER", "1");
        std::env::remove_var("OPENCODE_FLAGS");
    }

    let mock = Arc::new(Mock::default());
    let m = mock.clone();
    let router = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::Json<Value>| {
            let m = m.clone();
            async move {
                if body.0.to_string().contains("You name conversations") {
                    return sse(&text("Title"));
                }
                m.requests.lock().push(body.0.clone());
                let turn = m.turns.lock().pop_front().unwrap_or_else(|| text("Out of turns."));
                sse(&turn)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let project = |name: &str, flag: Option<bool>| {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("README.md"), "hello").unwrap();
        let mut config = json!({
            "model": "mock/m1",
            "providers": {"mock": {
                "name": "Mock",
                "package": "@opencode/ai/providers/openai-compatible",
                "settings": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "test"},
                "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}}}
            }}
        });
        if let Some(flag) = flag {
            config["experimental"] = json!({ "full_codemode": flag, "all_flags": true });
        }
        std::fs::write(dir.join("opencode.json"), config.to_string()).unwrap();
        dir
    };
    let on = project("on", Some(true));
    // `all_flags` turns every flag on, `full_codemode` too since 2.4.1, but the flag's own `false` wins.
    let off = project("off", Some(false));
    let default = project("default", None);

    let global = oc_util::Global::acquire().unwrap();
    let db = oc_storage::Database::open(&global.database("latest"), &global.data).unwrap();
    let app = App::new(global, db, "2.3.1");
    let engine = Engine::new(app.clone(), &oc_core::services::default_services(&app));
    engine.install();
    let (started_tx, mut started) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    engine.extra_tools.lock().push(Arc::new(Hold { started: started_tx, release: release.clone() }));

    let session = |dir: &std::path::Path| {
        let app = app.clone();
        let directory = dir.display().to_string();
        async move {
            app.sessions
                .create(CreateInput { location: Some(LocationRef { directory, workspace_id: None }), ..Default::default() })
                .await
                .unwrap()
        }
    };

    // The tools the model sees.
    let listings = |snapshot: &oc_tools::Snapshot| -> Vec<oc_codemode::catalog::Listing> {
        let inventory = oc_core::codemode::catalog_inventory(snapshot);
        oc_codemode::catalog::summarize(&inventory, Some(1_000_000)).namespaces.into_iter().flat_map(|n| n.entries).collect()
    };
    let s_on = session(&on).await;
    let selected = engine.select(&s_on.id).await.unwrap();
    let direct: Vec<&str> = selected.snapshot.direct.iter().map(|(n, _)| n.as_str()).collect();
    for name in ["shell", "read", "edit", "write", "patch", "websearch", "grep", "glob", "webfetch"] {
        assert!(!direct.contains(&name), "{name} is direct: {direct:?}");
    }
    let registered: Vec<&str> = selected.snapshot.registered.iter().map(|(n, _)| n.as_str()).collect();
    for name in ["grep", "glob", "webfetch"] {
        assert!(!registered.contains(&name), "{name} is registered: {registered:?}");
    }
    let paths: Vec<String> = listings(&selected.snapshot).into_iter().map(|l| l.path).collect();
    for name in ["shell", "read", "edit", "write", "patch", "websearch", "hold"] {
        assert!(paths.contains(&name.to_string()), "{name} is not listed: {paths:?}");
    }
    // Pinned: listed even when the catalog has no room for other tools, with the result contract in the field
    // annotations and no background parameter.
    let pinned: Vec<oc_codemode::catalog::Listing> =
        oc_codemode::catalog::summarize(&oc_core::codemode::catalog_inventory(&selected.snapshot), Some(0))
            .namespaces
            .into_iter()
            .flat_map(|n| n.entries)
            .collect();
    let builtins: Vec<&str> = pinned.iter().map(|l| l.path.as_str()).filter(|p| !p.contains('.')).collect();
    assert_eq!(builtins, ["edit", "patch", "read", "shell", "write"]);
    let shell = pinned.iter().find(|l| l.path == "shell").unwrap();
    assert!(shell.line.contains("600000"), "{}", shell.line);
    assert!(shell.line.contains("Execute a shell command and return its output."), "{}", shell.line);
    assert!(!shell.line.contains("background?: boolean"), "{}", shell.line);
    assert!(!shell.line.contains("Background commands return immediately"), "{}", shell.line);
    assert!(shell.line.contains("A result without shellID is final"), "{}", shell.line);
    assert!(shell.line.contains("there are no separate stdout or stderr fields"), "{}", shell.line);
    let read = pinned.iter().find(|l| l.path == "read").unwrap();
    assert!(read.line.contains("Reading an image or PDF automatically attaches it to the execute result"), "{}", read.line);
    assert!(read.line.contains("even if the program doesn't return or print the media reference"), "{}", read.line);
    assert!(!read.line.contains("Each text line is prefixed"), "{}", read.line);
    let read_tool = selected.snapshot.codemode_tool("read").unwrap();
    assert_eq!(read_tool.definition(&DefinitionContext::default()).description, oc_tools::tools::code_mode::READ_DESCRIPTION);
    assert!(read_tool.options().pinned);
    assert!(!selected.snapshot.codemode_tool("websearch").unwrap().options().pinned);
    assert_eq!(selected.snapshot.codemode_tool("edit").unwrap().options().permission.as_deref(), Some("edit"));

    // Without the flag (off, or left to `all_flags`, or unset) nothing moves.
    for dir in [&off, &default] {
        let s = session(dir).await;
        let selected = engine.select(&s.id).await.unwrap();
        let direct: Vec<&str> = selected.snapshot.direct.iter().map(|(n, _)| n.as_str()).collect();
        for name in ["shell", "read", "edit", "write", "patch", "grep", "glob", "webfetch"] {
            assert!(direct.contains(&name), "{name} is not direct: {direct:?}");
        }
        let paths: Vec<String> = listings(&selected.snapshot).into_iter().map(|l| l.path).filter(|p| !p.contains('.')).collect();
        assert_eq!(paths, ["hold"]);
        assert_eq!(
            selected.snapshot.direct_tool("read").unwrap().definition(&DefinitionContext::default()).description,
            oc_tools::tools::read::DESCRIPTION
        );
    }

    // A shell and a read through Code Mode: parallel shells, each with the Code Mode default timeout.
    let shells = "return await Promise.all([tools.shell({ command: \"echo one\" }), tools.shell({ command: \"echo two\" })])";
    mock.turns.lock().extend([
        tool_call("call_shells", "execute", json!({ "code": shells })),
        tool_call("call_read", "execute", json!({ "code": "return await tools.read({ path: \"README.md\" })" })),
        text("Done."),
    ]);
    let s = session(&on).await;
    app.sessions.prompt(&s.id, PromptInput { text: "run".into(), ..Default::default() }).await.unwrap();
    app.coordinator().wake(&s.id, Scope::Input);
    idle(&app, &s.id).await;
    let results: Value = serde_json::from_str(&completed_text(tool_part(&app, &s.id, "call_shells"))).unwrap();
    for (result, expected) in results.as_array().unwrap().iter().zip(["one", "two"]) {
        assert_eq!(result["output"].as_str().map(str::trim_end), Some(expected), "{results}");
        assert_eq!(result["exitCode"], json!(0), "{results}");
        assert!(result["wallTimeSeconds"].is_number(), "{results}");
        assert!(result.get("shellID").is_none(), "{results}");
    }
    let read: Value = serde_json::from_str(&completed_text(tool_part(&app, &s.id, "call_read"))).unwrap();
    assert_eq!(read, json!({ "type": "text", "content": "hello", "offset": 1, "lineCount": 1 }));
    let tools: Vec<String> = mock.requests.lock()[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_string())
        .collect();
    for name in ["shell", "read", "edit", "write", "patch", "grep", "glob", "webfetch", "websearch"] {
        assert!(!tools.contains(&name.to_string()), "{name} offered: {tools:?}");
    }
    assert!(tools.contains(&"execute".to_string()));

    // tool-registry.test.ts "Code Mode path": a session's `execute` gets `path` only with the flag.
    for (dir, expected) in [(&on, "[\"object\"]"), (&off, "[\"undefined\"]")] {
        mock.turns.lock().extend([tool_call("call_path", "execute", json!({ "code": "return [typeof path]" })), text("Done.")]);
        let s = session(dir).await;
        app.sessions.prompt(&s.id, PromptInput { text: "path".into(), ..Default::default() }).await.unwrap();
        app.coordinator().wake(&s.id, Scope::Input);
        idle(&app, &s.id).await;
        assert_eq!(completed_text(tool_part(&app, &s.id, "call_path")), expected);
    }

    let script = json!({ "code": "const prefix = \"result: \"; return prefix + await tools.hold({})" });
    let launch = |dir: std::path::PathBuf, turns: Vec<Vec<Value>>| {
        let (app, mock) = (app.clone(), mock.clone());
        async move {
            mock.turns.lock().extend(turns);
            let s = app
                .sessions
                .create(CreateInput { location: Some(LocationRef { directory: dir.display().to_string(), workspace_id: None }), ..Default::default() })
                .await
                .unwrap();
            app.sessions.prompt(&s.id, PromptInput { text: "Run the script".into(), ..Default::default() }).await.unwrap();
            app.coordinator().wake(&s.id, Scope::Input);
            s.id
        }
    };

    // A fast script returns normally.
    let id = launch(on.clone(), vec![tool_call("call_fast", "execute", script.clone()), text("Finished the step")]).await;
    started.recv().await.unwrap();
    release.add_permits(1);
    idle(&app, &id).await;
    assert_eq!(completed_text(tool_part(&app, &id, "call_fast")), "result: done");
    assert!(app.jobs.pending_background().is_empty());

    // With the flag off a script is no job: a session-wide background request leaves it in the foreground.
    let id = launch(off.clone(), vec![tool_call("call_off", "execute", script.clone()), text("Finished the step")]).await;
    started.recv().await.unwrap();
    assert!(app.jobs.background_all(&id, None).is_empty());
    release.add_permits(1);
    idle(&app, &id).await;
    assert_eq!(completed_text(tool_part(&app, &id, "call_off")), "result: done");
    assert!(app.jobs.pending_background().is_empty());

    // A slow script (a session-wide background request, then the 10 s limit) returns a receipt, then wakes its
    // session with the result.
    for by_request in [true, false] {
        let call = if by_request { "call_requested" } else { "call_slow" };
        let requests = mock.requests.lock().len();
        let id = launch(
            on.clone(),
            vec![tool_call(call, "execute", script.clone()), text("Finished the step"), text("Received the result")],
        )
        .await;
        started.recv().await.unwrap();
        if by_request {
            let moved = app.jobs.background_all(&id, None);
            assert_eq!(moved.len(), 1);
            assert_eq!(moved[0].kind, "codemode");
        }
        wait_until("the receipt step", || mock.requests.lock().len() >= requests + 2).await;
        idle(&app, &id).await;
        let pending = app.jobs.pending_background();
        assert_eq!(pending.len(), 1, "{pending:?}");
        let job = pending[0]["id"].as_str().unwrap().to_string();
        assert!(job.starts_with("cm_"), "{job}");
        assert_eq!(pending[0]["status"], json!("running"));
        assert_eq!(pending[0]["recovery"], json!({ "kind": "codemode", "sessionID": id, "jobID": job }));
        assert_eq!(
            completed_text(tool_part(&app, &id, call)),
            format!("Script {job} is still running; its result will arrive in this session.")
        );
        let listed = app.jobs.background_jobs();
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].kind(), listed[0].title.as_deref(), listed[0].session_id()), ("codemode", Some("Code Mode script"), id.as_str()));
        assert_eq!(oc_core::job::outstanding(&app, &id, &[]), vec![json!({ "kind": "codemode", "sessionID": id, "jobID": job })]);

        release.add_permits(1);
        wait_until("the result step", || mock.requests.lock().len() >= requests + 3).await;
        idle(&app, &id).await;
        assert!(app.jobs.pending_background().is_empty());
        let notice = format!("<background-result kind=\"codemode\" id=\"{job}\" state=\"completed\">\nresult: done\n</background-result>");
        let last = mock.requests.lock().last().unwrap().to_string();
        let escaped = serde_json::to_string(&notice).unwrap();
        assert!(last.contains(escaped.trim_matches('"')), "{last}");
        let synthetic = app.db.session_messages(&id).unwrap().into_iter().find_map(|(_, m)| match m {
            Message::Synthetic { text, metadata, description, .. } if text == notice => Some((metadata, description)),
            _ => None,
        });
        let (metadata, description) = synthetic.expect("the <codemode> message");
        assert_eq!(description.as_deref(), Some("Script"));
        assert_eq!(serde_json::to_value(metadata).unwrap(), json!({ "source": "codemode", "jobID": job, "state": "completed" }));
    }

    // A backgrounded script's file results are saved under the tool-output directory and named in its message.
    use base64::Engine as _;
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
    std::fs::write(on.join("pixel.png"), base64::engine::general_purpose::STANDARD.decode(png).unwrap()).unwrap();
    let requests = mock.requests.lock().len();
    let media = json!({ "code": "await tools.hold({}); await tools.read({ path: \"pixel.png\" }); return \"done\"" });
    let id = launch(on.clone(), vec![tool_call("call_media", "execute", media), text("Finished the step"), text("Received the result")]).await;
    started.recv().await.unwrap();
    assert_eq!(app.jobs.background_all(&id, None).len(), 1);
    wait_until("the receipt step", || mock.requests.lock().len() >= requests + 2).await;
    idle(&app, &id).await;
    release.add_permits(1);
    wait_until("the result step", || mock.requests.lock().len() >= requests + 3).await;
    idle(&app, &id).await;
    let text = app
        .db
        .session_messages(&id)
        .unwrap()
        .into_iter()
        .find_map(|(_, m)| match m {
            Message::Synthetic { text, .. } if text.starts_with("<background-result kind=\"codemode\"") => Some(text),
            _ => None,
        })
        .unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!((lines.len(), lines[1]), (4, "done"), "{text}");
    let saved = std::path::Path::new(lines[2]);
    assert!(saved.starts_with(oc_tools::tool_output::directory(&app.global.data)), "{text}");
    assert_eq!(saved.extension().and_then(|e| e.to_str()), Some("png"));
    assert_eq!(base64::engine::general_purpose::STANDARD.encode(std::fs::read(saved).unwrap()), png);

    std::fs::remove_dir_all(&root).ok();
}
