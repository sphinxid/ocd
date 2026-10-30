use oc_schema::event::{Delivery, InboxItem, SessionEvent};
use oc_schema::message::{AssistantContent, ToolState};
use oc_schema::types::*;
use oc_schema::{id, Message};
use oc_storage::projector::Committing;
use oc_storage::Database;
use serde_json::json;

fn commit(db: &Database, e: SessionEvent, created: u64) -> (String, i64) {
    let eid = id::event();
    let seq = db.commit_session_event(Committing { id: &eid, created, metadata: None, event: &e }).unwrap();
    (eid, seq)
}

fn setup() -> (Database, String) {
    let db = Database::memory().unwrap();
    db.with(|c| {
        c.execute(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) VALUES ('global', '/', 0, 0, '[]')",
            [],
        )
    })
    .unwrap();
    let sid = id::session();
    commit(
        &db,
        SessionEvent::Created {
            session_id: sid.clone(),
            project_id: "global".into(),
            location: LocationRef { directory: "/tmp/x".into(), workspace_id: None },
            subpath: None,
            parent_id: None,
            slug: "calm-otter".into(),
            title: None,
            agent: Some("build".into()),
            model: Some(ModelRef { id: "m".into(), provider_id: "p".into(), variant: None }),
            metadata: None,
            permissions: None,
            version: "2.0.26".into(),
        },
        1000,
    );
    (db, sid)
}

#[test]
fn fresh_database_has_all_migrations() {
    let db = Database::memory().unwrap();
    let n: i64 = db.with(|c| c.query_row("SELECT count(*) FROM migration", [], |r| r.get(0))).unwrap();
    assert_eq!(n, 58);
}

#[test]
fn full_turn_projects_messages() {
    let (db, sid) = setup();
    let info = db.session_get(&sid).unwrap().unwrap();
    assert_eq!(info.agent.as_deref(), Some("build"));
    assert_eq!(info.model.as_ref().unwrap().variant.as_deref(), Some("default"));

    let inbox = id::message();
    let (_, seq) = commit(
        &db,
        SessionEvent::InboxEnqueued {
            session_id: sid.clone(),
            inbox_id: inbox.clone(),
            item: InboxItem { kind: "user".into(), payload: json!({"text": "hi"}), delivery: Delivery::Queue },
        },
        1100,
    );
    assert_eq!(seq, 1);
    commit(&db, SessionEvent::InboxDelivered { session_id: sid.clone(), inbox_id: inbox.clone() }, 1200);
    let aid = id::message();
    let a = aid.clone();
    let s = sid.clone();
    let step = |e| commit(&db, e, 1300);
    step(SessionEvent::StepStarted {
        session_id: s.clone(),
        assistant_message_id: a.clone(),
        agent: "build".into(),
        model: ModelRef { id: "m".into(), provider_id: "p".into(), variant: None },
        snapshot: None,
        started: 1300,
    });
    step(SessionEvent::TextStarted { session_id: s.clone(), assistant_message_id: a.clone(), ordinal: 0 });
    step(SessionEvent::TextEnded {
        session_id: s.clone(),
        assistant_message_id: a.clone(),
        ordinal: 0,
        text: "Done.".into(),
        native: None,
    });
    step(SessionEvent::ToolInputStarted {
        session_id: s.clone(),
        assistant_message_id: a.clone(),
        id: "call_1".into(),
        name: "read".into(),
    });
    step(SessionEvent::ToolCalled {
        session_id: s.clone(),
        assistant_message_id: a.clone(),
        id: "call_1".into(),
        input: json!({"path": "a.ts"}).as_object().unwrap().clone(),
        executed: true,
        native: None,
    });
    step(SessionEvent::ToolSuccess {
        session_id: s.clone(),
        assistant_message_id: a.clone(),
        id: "call_1".into(),
        content: vec![ToolContent::Text { text: "ok".into() }],
        metadata: None,
        executed: false,
        native: None,
    });
    step(SessionEvent::StepEnded {
        session_id: s.clone(),
        assistant_message_id: a.clone(),
        finish: FinishReason::Stop,
        raw_finish: None,
        native: None,
        cost: 0.5,
        tokens: TokenUsage { input: 10, output: 2, reasoning: 0, cache: CacheUsage::default() },
        snapshot: None,
        files: None,
    });
    step(SessionEvent::ExecutionSucceeded { session_id: s.clone() });

    let msgs = db.session_messages(&sid).unwrap();
    let kinds: Vec<_> = msgs.iter().map(|(_, m)| m.kind()).collect();
    assert_eq!(kinds, vec!["user", "assistant", "idle"]);
    assert_eq!(msgs[0].1.id(), inbox);
    let Message::Assistant(asst) = &msgs[1].1 else { panic!() };
    assert_eq!(asst.finish, Some(FinishReason::Stop));
    assert!(matches!(&asst.content[0], AssistantContent::Text { text, .. } if text == "Done."));
    assert!(matches!(
        &asst.content[1],
        AssistantContent::Tool { executed: Some(true), state: ToolState::Completed { .. }, .. }
    ));
    let info = db.session_get(&sid).unwrap().unwrap();
    assert_eq!(info.cost, 0.5);
    assert_eq!(info.tokens.input, 10);
    assert_eq!(info.outcome, Some(Outcome::Succeeded));
    // stored data excludes id/type
    let data: String = db
        .with(|c| c.query_row("SELECT data FROM session_message WHERE type = 'user'", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(data, r#"{"time":{"created":1200},"text":"hi"}"#);
}

// a5bf17164: a committed revert keeps the instruction baseline unless it removed the update it was folded
// through (or the epoch start), so the prompt cache survives.
#[test]
fn committed_revert_keeps_instruction_baseline() {
    let through = |db: &Database, sid: &str| -> Option<i64> {
        db.with(|c| {
            use rusqlite::OptionalExtension;
            c.query_row("SELECT through_seq FROM instruction_state WHERE session_id = ?", [sid], |r| r.get(0)).optional()
        })
        .unwrap()
    };
    for (fold_through, survives) in [(5, true), (10, false), (12, false)] {
        let (db, sid) = setup();
        db.with(|c| {
            for (id, seq) in [("msg_a", 3), ("msg_b", 10), ("msg_c", 14)] {
                c.execute(
                    "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?, ?, 'user', ?, 0, 0, '{\"time\":{\"created\":0},\"text\":\"x\"}')",
                    rusqlite::params![id, sid, seq],
                )?;
            }
            c.execute(
                "INSERT INTO instruction_state (session_id, epoch_start, through_seq, initial_values, current_values) VALUES (?, 0, ?, '{}', '{}')",
                rusqlite::params![sid, fold_through],
            )
        })
        .unwrap();
        commit(&db, SessionEvent::RevertCommitted { session_id: sid.clone(), to: "msg_b".into() }, 2000);
        assert_eq!(through(&db, &sid), survives.then_some(fold_through), "folded through {fold_through}");
        let left: i64 = db.with(|c| c.query_row("SELECT count(*) FROM session_message", [], |r| r.get(0))).unwrap();
        assert_eq!(left, 1);
    }
}
