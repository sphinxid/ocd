//! `Session.switchAgent` (2.4.1, #65122, session-create.test.ts): selecting the agent a session already runs as
//! records nothing; without a stored agent, the one its last step ran as counts.

use oc_core::app::App;
use oc_core::session::CreateInput;
use oc_schema::event::SessionEvent;
use oc_schema::types::LocationRef;
use oc_schema::Message;
use serde_json::json;

/// Records one completed step, the way the runner does, so the session's history shows which agent ran.
fn complete_step(app: &App, session: &str, agent: &str) {
    let id = oc_schema::id::ascending("msg");
    let started: SessionEvent = serde_json::from_value(json!({
        "type": "session.step.started", "sessionID": session, "assistantMessageID": id, "agent": agent,
        "model": {"id": "model", "providerID": "provider"}, "started": 0
    }))
    .unwrap();
    let ended: SessionEvent = serde_json::from_value(json!({
        "type": "session.step.ended", "sessionID": session, "assistantMessageID": id, "finish": "stop", "cost": 0,
        "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}}
    }))
    .unwrap();
    app.bus.commit_one(started).unwrap();
    app.bus.commit_one(ended).unwrap();
}

fn types(app: &App, session: &str) -> Vec<String> {
    app.db
        .session_messages(session)
        .unwrap()
        .into_iter()
        .map(|(_, m)| serde_json::to_value(&m).unwrap()["type"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn skips_selecting_the_agent_a_session_already_runs_as() {
    let root = std::env::temp_dir().join(format!("oc-agent-selected-{}", std::process::id()));
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
    let global = oc_util::Global::acquire().unwrap();
    let db = oc_storage::Database::open(&global.database("latest"), &global.data).unwrap();
    let app = App::new(global, db, "2.4.1");
    let location = || Some(LocationRef { directory: project.display().to_string(), workspace_id: None });

    // Its stored agent, and the default agent it ran as without a stored one.
    for (agent, ran) in [(Some("plan"), "plan"), (None, "build")] {
        let s = app.sessions.create(CreateInput { location: location(), agent: agent.map(str::to_string), ..Default::default() }).await.unwrap();
        complete_step(&app, &s.id, ran);
        app.sessions.switch_agent(&s.id, ran).unwrap();
        assert_eq!(app.sessions.get(&s.id).unwrap().agent.as_deref(), agent);
        assert_eq!(types(&app, &s.id), ["assistant"]);
    }

    // Another agent for a session without a stored one is recorded.
    let s = app.sessions.create(CreateInput { location: location(), ..Default::default() }).await.unwrap();
    complete_step(&app, &s.id, "build");
    app.sessions.switch_agent(&s.id, "plan").unwrap();
    let last = app.db.session_messages(&s.id).unwrap().pop().unwrap().1;
    assert!(matches!(&last, Message::AgentSwitched { agent, .. } if agent == "plan"), "{last:?}");
    std::fs::remove_dir_all(&root).ok();
}
