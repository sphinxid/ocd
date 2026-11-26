//! Restart recovery of shell commands the process daemon kept running (`restart.ts` 2.3.1: `recoverShellJob`,
//! `settleBackgroundedCalls`, `ShellTool.reattach`), against the in-process fake daemon: a previous server left a
//! foreground shell call unsettled and a background shell job running; the next server settles the call as moved to
//! the background, watches both commands, and reports each when it ends.

#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use oc_core::app::App;
use oc_core::runner::Engine;
use oc_core::session::CreateInput;
use oc_persistent_pty::testing::FakeDaemon;
use oc_persistent_pty::{BinaryFn, Options, PersistentPty};
use oc_schema::event::SessionEvent;
use oc_schema::message::{AssistantContent, ToolState};
use oc_schema::types::{LocationRef, ModelRef};
use oc_schema::{Message, ToolContent};
use oc_tools::host::ShellInvocation;
use oc_tools::shell::daemon::{Placement, ToolCall};
use serde_json::json;

fn synthetic_texts(app: &App, session: &str) -> Vec<String> {
    let mut out: Vec<String> = app
        .db
        .session_messages(session)
        .unwrap()
        .into_iter()
        .filter_map(|(_, m)| match m {
            Message::Synthetic { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    for (_, kind, payload, ..) in app.db.session_inbox(session).unwrap() {
        if kind == "synthetic" {
            out.push(payload["text"].as_str().unwrap_or_default().to_string());
        }
    }
    out
}

async fn until(check: impl Fn() -> bool) {
    for _ in 0..1000 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition never held");
}

#[tokio::test(flavor = "multi_thread")]
async fn shell_commands_kept_by_the_daemon_are_taken_back() {
    let root = std::env::temp_dir().join(format!("oc-restart-daemon-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.canonicalize().unwrap();
    for (k, v) in [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache")] {
        unsafe { std::env::set_var(k, root.join(v)) };
    }
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_TEST_HOME", root.join("home"));
        std::env::set_var("OPENCODE_DISABLE_FILEWATCHER", "1");
    }
    let global = oc_util::Global::acquire().unwrap();
    let db = oc_storage::Database::open(&global.database("latest"), &global.data).unwrap();
    let app = App::new(global.clone(), db, "2.3.1");
    let fake = FakeDaemon::start(root.join("pty/inherited")).await;
    let no_spawn: BinaryFn = Arc::new(|| Box::pin(async { Err("spawning disabled in tests".to_string()) }));
    let pty = |handoff| {
        PersistentPty::new(&global, Options { root: Some(root.join("pty")), handoff, binary: Some(no_spawn.clone()), ..Default::default() })
    };

    let directory = project.display().to_string();
    let location = Some(LocationRef { directory: directory.clone(), workspace_id: None });
    let session = app.sessions.create(CreateInput { location, ..Default::default() }).await.unwrap();
    let sid = session.id.clone();

    // The previous server: a foreground call and a background job, both commands still running in the daemon.
    let previous = oc_tools::shell::service::ShellService::new(app.global.data.clone(), None);
    previous.attach_daemon(pty(Some(fake.handoff())).await);
    let invocation = |command: &str| ShellInvocation {
        command: command.into(),
        cwd: directory.clone(),
        timeout: 0,
        shell: "/bin/sh".into(),
        env: Default::default(),
    };
    let placement = |call: &str, waited: bool| Placement {
        project_id: session.project_id.clone(),
        directory: directory.clone(),
        workspace_id: None,
        tool_call: Some(ToolCall { session_id: sid.clone(), assistant_message_id: "msg_assistant".into(), call_id: call.into() }),
        waited,
    };
    let metadata = json!({"sessionID": sid}).as_object().cloned().unwrap();
    let foreground =
        previous.create(&invocation("sleep 60; echo fg"), metadata.clone(), &placement("call_fg", true)).await.unwrap();
    let background =
        previous.create(&invocation("sleep 60; echo bg"), metadata.clone(), &placement("call_bg", false)).await.unwrap();
    let model = ModelRef { id: "m".into(), provider_id: "p".into(), variant: None };
    let call = |id: &str| {
        vec![
            SessionEvent::ToolInputStarted { session_id: sid.clone(), assistant_message_id: "msg_assistant".into(), id: id.into(), name: "shell".into() },
            SessionEvent::ToolInputEnded { session_id: sid.clone(), assistant_message_id: "msg_assistant".into(), id: id.into(), text: "{}".into() },
            SessionEvent::ToolCalled {
                session_id: sid.clone(),
                assistant_message_id: "msg_assistant".into(),
                id: id.into(),
                input: json!({"command": "sleep 60"}).as_object().cloned().unwrap(),
                executed: false,
                native: None,
            },
        ]
    };
    app.bus
        .commit_one(SessionEvent::StepStarted {
            session_id: sid.clone(),
            assistant_message_id: "msg_assistant".into(),
            agent: "build".into(),
            model,
            snapshot: None,
            started: 1,
        })
        .unwrap();
    for event in call("call_fg") {
        app.bus.commit_one(event).unwrap();
    }
    // The background call settled before the restart; its job row is pending.
    app.db
        .with(|c| {
            c.execute(
                "INSERT INTO job (notification_id, id, session_id, child_session_id, kind, title, status, output, error, recovery, time_created, time_updated) VALUES (?, ?, ?, NULL, 'shell', ?, 'running', NULL, NULL, ?, 1, 1)",
                rusqlite::params![
                    "msg_bgnotice",
                    background.id,
                    sid,
                    background.command,
                    json!({"kind": "shell", "sessionID": sid, "shellID": background.id, "command": background.command}).to_string()
                ],
            )
        })
        .unwrap();
    // The server died mid-step: its claim is still held.
    app.db.with(|c| c.execute("UPDATE session_v2 SET time_suspended = 1 WHERE id = ?", [&sid])).unwrap();
    drop(previous);

    // The next server adopts the daemon and recovers.
    let engine = Engine::new(app.clone(), &oc_core::services::default_services(&app));
    engine.install();
    engine.shell.attach_daemon(pty(Some(fake.handoff())).await);
    engine.resume_suspended().await.unwrap();

    // The unsettled foreground call is settled as moved to the background.
    let messages = app.db.session_messages(&sid).unwrap();
    let tool = messages
        .iter()
        .find_map(|(_, m)| match m {
            Message::Assistant(a) if a.id == "msg_assistant" => a.content.iter().find_map(|c| match c {
                AssistantContent::Tool { id, state, .. } if id == "call_fg" => Some(state.clone()),
                _ => None,
            }),
            _ => None,
        })
        .unwrap();
    let ToolState::Completed { content, metadata, .. } = tool else { panic!("not settled: {tool:?}") };
    let ToolContent::Text { text } = &content[0] else { panic!() };
    assert_eq!(
        text,
        &format!("Command moved to the background (shell ID: {}).\nPID: 1001\nOutput is streaming to: {}", foreground.id, foreground.file)
    );
    assert_eq!(json!(metadata), json!({"shellID": foreground.id, "status": "running", "truncated": false, "pid": 1001}));

    // Both commands are watched as background jobs; each reports once it ends.
    fake.output(1, "fg\n");
    fake.exit(1, Some(0), None);
    fake.output(2, "bg\n");
    fake.exit(2, Some(0), None);
    until(|| synthetic_texts(&app, &sid).iter().filter(|t| t.starts_with("<background-result kind=\"shell\" ")).count() == 2).await;
    let texts: Vec<String> = synthetic_texts(&app, &sid).into_iter().filter(|t| t.starts_with("<background-result kind=\"shell\" ")).collect();
    assert!(texts.contains(&format!("<background-result kind=\"shell\" id=\"{}\" state=\"completed\" command=\"sleep 60; echo fg\">\nfg\n\n</background-result>", foreground.id)), "{texts:#?}");
    assert!(texts.contains(&format!("<background-result kind=\"shell\" id=\"{}\" state=\"completed\" command=\"sleep 60; echo bg\">\nbg\n\n</background-result>", background.id)), "{texts:#?}");
    until(|| app.jobs.pending_background().is_empty()).await;
    std::fs::remove_dir_all(&root).ok();
}
