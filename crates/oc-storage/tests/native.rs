//! v2.1 native data (`core/test/session-projector.test.ts`, `session-compaction.test.ts`): rows written
//! before the rename read in the current shape, projection writes only the current shape, tool result
//! native data merges into the call's, and forks re-point a summary's `metadata.keepID`.

use oc_schema::event::SessionEvent;
use oc_schema::message::{AssistantContent, CompactionReason, CompactionStatus};
use oc_schema::types::*;
use oc_schema::{id, Message};
use oc_storage::projector::Committing;
use oc_storage::Database;
use serde_json::{json, Value};

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
    commit(&db, created(&sid), 1000);
    (db, sid)
}

fn created(sid: &str) -> SessionEvent {
    SessionEvent::Created {
        session_id: sid.into(),
        project_id: "global".into(),
        location: LocationRef { directory: "/tmp/x".into(), workspace_id: None },
        subpath: None,
        parent_id: None,
        slug: "calm-otter".into(),
        title: None,
        agent: Some("build".into()),
        model: Some(model()),
        metadata: None,
        permissions: None,
        version: "2.0.26".into(),
    }
}

fn model() -> ModelRef {
    ModelRef { id: "m".into(), provider_id: "p".into(), variant: None }
}

fn insert_row(db: &Database, session: &str, id: &str, kind: &str, seq: i64, data: Value) {
    db.with(|c| {
        c.execute(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?, ?, ?, ?, 0, 0, ?)",
            rusqlite::params![id, session, kind, seq, data.to_string()],
        )
    })
    .unwrap();
    db.with(|c| {
        c.execute(
            "INSERT INTO event_sequence (aggregate_id, seq) VALUES (?1, ?2) ON CONFLICT(aggregate_id) DO UPDATE SET seq = max(seq, ?2)",
            rusqlite::params![session, seq],
        )
    })
    .unwrap();
}

fn row_data(db: &Database, id: &str) -> Value {
    let data: String =
        db.with(|c| c.query_row("SELECT data FROM session_message WHERE id = ?", [id], |r| r.get(0))).unwrap();
    serde_json::from_str(&data).unwrap()
}

fn obj(v: Value) -> Metadata {
    v.as_object().unwrap().clone()
}

#[test]
fn reads_and_rewrites_native_data_stored_under_pre_v2_1_names() {
    let (db, sid) = setup();
    let assistant = "msg_legacy_assistant";
    insert_row(
        &db,
        &sid,
        assistant,
        "assistant",
        1,
        json!({
            "time": { "created": 0 },
            "agent": "build",
            "model": { "providerID": "p", "id": "m" },
            "providerState": { "step": "kept" },
            "content": [
                { "type": "text", "text": "hi", "state": { "signature": "text" } },
                { "type": "reasoning", "text": "thinking", "state": { "signature": "reasoning" } },
                {
                    "type": "tool", "id": "call", "name": "search", "executed": true,
                    "providerState": { "call": "kept" }, "providerResultState": { "result": "kept" },
                    "state": { "status": "completed", "input": {}, "content": [{ "type": "text", "text": "done" }] },
                    "time": { "created": 0, "ran": 0, "completed": 0 },
                },
            ],
        }),
    );
    insert_row(
        &db,
        &sid,
        "msg_legacy_compaction",
        "compaction",
        2,
        json!({ "status": "completed", "reason": "manual", "summary": "summary", "providerState": { "compaction": "kept" }, "time": { "created": 0 } }),
    );

    let msgs = db.session_messages(&sid).unwrap();
    let Message::Assistant(a) = &msgs[0].1 else { panic!("assistant") };
    assert_eq!(a.native, Some(obj(json!({ "step": "kept" }))));
    assert!(matches!(&a.content[0], AssistantContent::Text { native: Some(n), .. } if n == &obj(json!({ "signature": "text" }))));
    assert!(matches!(&a.content[1], AssistantContent::Reasoning { native: Some(n), .. } if n == &obj(json!({ "signature": "reasoning" }))));
    assert!(matches!(&a.content[2], AssistantContent::Tool { native: Some(n), .. } if n == &obj(json!({ "call": "kept", "result": "kept" }))));
    let Message::Compaction(c) = &msgs[1].1 else { panic!("compaction") };
    assert!(matches!(&c.status, CompactionStatus::Completed { native: Some(n), .. } if n == &obj(json!({ "compaction": "kept" }))));

    // Projecting onto the legacy row writes it back in the current shape.
    commit(
        &db,
        SessionEvent::from_parts(
            "session.step.ended",
            json!({
                "sessionID": sid,
                "assistantMessageID": assistant,
                "finish": "stop",
                "providerState": { "step": "replayed" },
                "cost": 0,
                "tokens": { "input": 0, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
            }),
        )
        .unwrap(),
        2000,
    );
    let rewritten = row_data(&db, assistant);
    assert!(rewritten.get("providerState").is_none());
    assert_eq!(rewritten["native"], json!({ "step": "replayed" }));
    assert_eq!(rewritten["content"][0]["native"], json!({ "signature": "text" }));
    assert_eq!(rewritten["content"][1]["native"], json!({ "signature": "reasoning" }));
    assert_eq!(rewritten["content"][2]["native"], json!({ "call": "kept", "result": "kept" }));
    let keys: Vec<Vec<String>> = rewritten["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            let mut k: Vec<String> = p.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            vec!["native", "text", "type"],
            vec!["native", "text", "type"],
            vec!["executed", "id", "name", "native", "state", "time", "type"],
        ]
    );
}

#[test]
fn merges_tool_result_native_data_into_the_calls_native_data() {
    let (db, sid) = setup();
    let a = id::message();
    commit(
        &db,
        SessionEvent::StepStarted {
            session_id: sid.clone(),
            assistant_message_id: a.clone(),
            agent: "build".into(),
            model: model(),
            snapshot: None,
            started: 1100,
        },
        1100,
    );
    let call = |name: &str| {
        commit(
            &db,
            SessionEvent::ToolInputStarted {
                session_id: sid.clone(),
                assistant_message_id: a.clone(),
                id: name.into(),
                name: name.into(),
            },
            1200,
        );
        commit(
            &db,
            SessionEvent::ToolCalled {
                session_id: sid.clone(),
                assistant_message_id: a.clone(),
                id: name.into(),
                input: Metadata::new(),
                executed: true,
                native: Some(obj(json!({ "call": name }))),
            },
            1200,
        );
    };
    call("succeeded");
    commit(
        &db,
        SessionEvent::ToolSuccess {
            session_id: sid.clone(),
            assistant_message_id: a.clone(),
            id: "succeeded".into(),
            content: vec![ToolContent::Text { text: "done".into() }],
            metadata: None,
            executed: true,
            native: Some(obj(json!({ "result": "succeeded" }))),
        },
        1300,
    );
    call("failed");
    commit(
        &db,
        SessionEvent::ToolFailed {
            session_id: sid.clone(),
            assistant_message_id: a.clone(),
            id: "failed".into(),
            error: SessionError::new("tool.execution", "failed"),
            content: None,
            metadata: None,
            executed: true,
            native: None,
        },
        1300,
    );
    let data = row_data(&db, &a);
    assert_eq!(data["content"][0]["native"], json!({ "call": "succeeded", "result": "succeeded" }));
    assert_eq!(data["content"][0]["state"]["status"], json!("completed"));
    assert_eq!(data["content"][1]["native"], json!({ "call": "failed" }));
    assert_eq!(data["content"][1]["state"]["status"], json!("error"));
}

#[test]
fn compaction_rows_store_no_recent_text() {
    let (db, sid) = setup();
    commit(&db, SessionEvent::CompactionStarted { session_id: sid.clone(), reason: CompactionReason::Manual, input_id: None }, 1100);
    let id: String = db
        .with(|c| c.query_row("SELECT id FROM session_message WHERE type = 'compaction'", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(row_data(&db, &id), json!({ "time": { "created": 1100 }, "status": "running", "reason": "manual", "summary": "" }));
    commit(
        &db,
        SessionEvent::CompactionEnded {
            session_id: sid.clone(),
            reason: CompactionReason::Manual,
            model: None,
            native: None,
            text: "summary".into(),
            cost: None,
            tokens: None,
        },
        1200,
    );
    let data = row_data(&db, &id);
    assert_eq!(data["summary"], json!("summary"));
    assert!(data.get("recent").is_none());
}

/// `forks rebuild the recent text of a summary they copied` (the projector half) and
/// `ignores non-message keepID strings when loading history or forking`.
#[test]
fn forks_rewrite_keep_id_to_the_copied_message() {
    let (db, sid) = setup();
    let kept = "msg_kept";
    insert_row(&db, &sid, "msg_old", "user", 1, json!({ "time": { "created": 0 }, "text": "old" }));
    insert_row(&db, &sid, kept, "user", 2, json!({ "time": { "created": 0 }, "text": "kept" }));
    insert_row(
        &db,
        &sid,
        "msg_summary",
        "compaction",
        3,
        json!({ "metadata": { "keepID": kept }, "time": { "created": 0 }, "status": "completed", "reason": "auto", "summary": "s" }),
    );
    insert_row(
        &db,
        &sid,
        "msg_invalid",
        "compaction",
        4,
        json!({ "metadata": { "keepID": "not-a-msg-id" }, "time": { "created": 0 }, "status": "completed", "reason": "manual", "summary": "p" }),
    );
    let child = id::session();
    let (eid, _) = commit(
        &db,
        SessionEvent::Forked {
            session_id: child.clone(),
            parent_id: sid.clone(),
            boundary: ForkBoundary { kind: ForkBoundaryType::Through, message_id: "msg_invalid".into() },
            instructions: None,
            instruction_entries: None,
        },
        2000,
    );
    let prefix = id::message_from_event(&eid);
    let rows: Vec<(String, String)> = db
        .with(|c| {
            let mut s = c.prepare("SELECT id, data FROM session_message WHERE session_id = ? AND type = 'compaction' ORDER BY seq")?;
            let r = s.query_map([&child], |r| Ok((r.get(0)?, r.get(1)?)))?.collect();
            r
        })
        .unwrap();
    let summary: Value = serde_json::from_str(&rows[0].1).unwrap();
    assert_eq!(rows[0].0, format!("{prefix}_3"));
    assert_eq!(summary["metadata"]["keepID"], json!(format!("{prefix}_2")));
    let invalid: Value = serde_json::from_str(&rows[1].1).unwrap();
    assert_eq!(invalid["metadata"]["keepID"], json!("not-a-msg-id"));
}
