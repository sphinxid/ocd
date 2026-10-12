//! Restart recovery of persisted background jobs (`restart.ts`).

use oc_core::app::App;
use oc_core::runner::Engine;
use oc_core::session::CreateInput;
use oc_schema::types::LocationRef;
use oc_schema::Message;
use serde_json::json;

/// Synthetic notices, delivered (messages) or still queued (inbox).
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

#[tokio::test]
async fn pending_background_jobs_are_settled() {
    let root = std::env::temp_dir().join(format!("oc-restart-{}", std::process::id()));
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
    let global = oc_util::Global::acquire().unwrap();
    let db = oc_storage::Database::open(&global.database("latest"), &global.data).unwrap();
    let app = App::new(global, db, "2.0.26");
    let engine = Engine::new(app.clone(), &oc_core::services::default_services(&app));
    engine.install();

    let location = Some(LocationRef { directory: project.display().to_string(), workspace_id: None });
    let parent = app.sessions.create(CreateInput { location: location.clone(), ..Default::default() }).await.unwrap();
    let child = app
        .sessions
        .create(CreateInput { location, parent_id: Some(parent.id.clone()), ..Default::default() })
        .await
        .unwrap();

    app.db
        .kv_set(
            "job.background/msg_shell",
            &json!({"id": "job_1", "notificationID": "msg_shell", "status": "running",
                    "recovery": {"kind": "shell", "sessionID": parent.id, "shellID": "sh_1", "command": "sleep 100"}}),
        )
        .unwrap();
    app.db
        .kv_set(
            "job.background/msg_sub",
            &json!({"id": child.id, "notificationID": "msg_sub", "status": "completed", "output": "all done",
                    "recovery": {"kind": "subagent", "parentSessionID": parent.id, "childSessionID": child.id,
                                 "agent": "general", "description": "do work"}}),
        )
        .unwrap();

    engine.resume_suspended().await.unwrap();

    let texts = synthetic_texts(&app, &parent.id);
    assert_eq!(texts.len(), 2, "{texts:#?}");
    assert_eq!(
        texts[0],
        "<shell id=\"job_1\" state=\"cancelled\" command=\"sleep 100\">\nCommand cancelled because the server restarted\n</shell>"
    );
    assert_eq!(
        texts[1],
        format!("<subagent sessionID=\"{}\" state=\"completed\" description=\"do work\">\nall done\n</subagent>", child.id)
    );
    assert!(app.jobs.pending_background().is_empty());
    std::fs::remove_dir_all(&root).ok();
}
