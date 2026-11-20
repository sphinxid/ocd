//! The `experimental.part_rows` writer (2.2.7, unchanged in 2.3.1; `core/test/session-projector.test.ts` and
//! `session-store.test.ts` cases run with `part_rows: true`): every assistant part is its own `session_part` row,
//! appended, updated and replaced by the part events while the message row keeps an empty `content` envelope; a legacy
//! inline row moves to part rows on its first part write; reads fold the rows back the same with the flag on or off.

use base64::Engine;
use oc_schema::event::SessionEvent;
use oc_schema::types::*;
use oc_schema::{Message, id};
use oc_storage::Database;
use oc_storage::projector::Committing;
use serde_json::{Value, json};

/// The flags an event projects with.
#[derive(Clone, Copy)]
struct Flags {
    part_rows: bool,
    media: bool,
}

const OFF: Flags = Flags { part_rows: false, media: false };
const ROWS: Flags = Flags { part_rows: true, media: false };
const MEDIA_ROWS: Flags = Flags { part_rows: true, media: true };

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

fn commit(db: &Database, e: SessionEvent, created: u64, flags: Flags) -> i64 {
    let eid = id::event();
    db.commit_session_event(Committing {
        id: &eid,
        created,
        metadata: None,
        event: &e,
        media: flags.media,
        part_rows: flags.part_rows,
    })
    .unwrap()
}

/// Commits `kind` with `data` (camelCase, as upstream publishes it) at `created`.
fn publish(db: &Database, kind: &str, data: Value, created: u64, flags: Flags) -> i64 {
    commit(db, SessionEvent::from_parts(kind, data).unwrap(), created, flags)
}

fn created(sid: &str) -> SessionEvent {
    SessionEvent::Created {
        session_id: sid.into(),
        project_id: "global".into(),
        location: LocationRef { directory: "/project".into(), workspace_id: None },
        subpath: None,
        parent_id: None,
        slug: "test".into(),
        title: Some("test".into()),
        agent: Some("build".into()),
        model: Some(model()),
        metadata: None,
        permissions: None,
        version: "test".into(),
    }
}

fn model() -> ModelRef {
    ModelRef { id: "model".into(), provider_id: "provider".into(), variant: None }
}

fn setup() -> (Database, String) {
    let db = Database::memory().unwrap();
    exec(&db, "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) VALUES ('global', '/project', 0, 0, '[]')", []);
    let sid = "ses_projector_test".to_string();
    commit(&db, created(&sid), 0, OFF);
    (db, sid)
}

fn add_session(db: &Database, sid: &str) {
    commit(db, created(sid), 0, OFF);
}

fn exec(db: &Database, sql: &str, p: impl rusqlite::Params) {
    db.with(|c| c.execute(sql, p)).unwrap();
}

/// `assistantRow`: an empty assistant message row at `seq`.
fn assistant_row(db: &Database, session: &str, id: &str, seq: i64, completed: Option<u64>, content: Value) {
    let mut time = json!({ "created": 0 });
    if let Some(c) = completed {
        time["completed"] = json!(c);
    }
    let data = json!({ "time": time, "agent": "build", "model": { "id": "model", "providerID": "provider" }, "content": content });
    insert_row(db, session, id, "assistant", seq, 0, data);
}

fn insert_row(db: &Database, session: &str, id: &str, kind: &str, seq: i64, created: i64, data: Value) {
    exec(
        db,
        "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?, ?, ?)",
        rusqlite::params![id, session, kind, seq, created, created, data.to_string()],
    );
    exec(
        db,
        "INSERT INTO event_sequence (aggregate_id, seq) VALUES (?1, ?2) ON CONFLICT(aggregate_id) DO UPDATE SET seq = max(seq, ?2)",
        rusqlite::params![session, seq],
    );
}

#[allow(clippy::too_many_arguments)]
fn insert_part(db: &Database, session: &str, message: &str, ordinal: i64, kind: &str, call: Option<&str>, time: i64, data: Value) {
    exec(
        db,
        "INSERT INTO session_part (message_id, session_id, ordinal, type, call_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        rusqlite::params![message, session, ordinal, kind, call, time, time, data.to_string()],
    );
}

fn row_data(db: &Database, id: &str) -> Value {
    let data: String = db.with(|c| c.query_row("SELECT data FROM session_message WHERE id = ?", [id], |r| r.get(0))).unwrap();
    serde_json::from_str(&data).unwrap()
}

/// A message's part rows `(ordinal, type, call_id, time_created, time_updated, data)` in ordinal order.
#[allow(clippy::type_complexity)]
fn parts(db: &Database, message: &str) -> Vec<(i64, String, Option<String>, i64, i64, Value)> {
    db.with(|c| {
        let mut s = c.prepare(
            "SELECT ordinal, type, call_id, time_created, time_updated, data FROM session_part WHERE message_id = ? ORDER BY ordinal",
        )?;
        let rows = s.query_map([message], |r| {
            let data: String = r.get(5)?;
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, serde_json::from_str(&data).unwrap()))
        })?;
        rows.collect()
    })
    .unwrap()
}

fn count(db: &Database, sql: &str) -> i64 {
    db.with(|c| c.query_row(sql, [], |r| r.get(0))).unwrap()
}

/// The session's messages as read (`store.messages`), as JSON.
fn messages(db: &Database, sid: &str) -> Vec<Value> {
    db.session_messages(sid).unwrap().into_iter().map(|(_, m)| serde_json::to_value(m).unwrap()).collect()
}

/// `withoutIDs`: messages compared across sessions.
fn without_ids(mut list: Vec<Value>) -> Vec<Value> {
    for m in &mut list {
        m.as_object_mut().unwrap().shift_remove("id");
    }
    list
}

fn assistant(db: &Database, sid: &str, id: &str) -> Value {
    messages(db, sid).into_iter().find(|m| m["id"] == id).expect("message")
}

/// The subset check of `toMatchObject`.
fn matches(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(a), Value::Object(e)) => e.iter().all(|(k, v)| a.get(k).is_some_and(|x| matches(x, v))),
        (Value::Array(a), Value::Array(e)) => a.len() == e.len() && a.iter().zip(e).all(|(x, y)| matches(x, y)),
        _ => actual == expected,
    }
}

#[track_caller]
fn assert_matches(actual: &Value, expected: Value) {
    assert!(matches(actual, &expected), "expected {actual:#} to match {expected:#}");
}

fn usage() -> Value {
    json!({ "cost": 0, "tokens": { "input": 1, "output": 1, "reasoning": 0, "cache": { "read": 0, "write": 0 } } })
}

fn step_started(db: &Database, sid: &str, msg: &str, started: u64, created: u64, flags: Flags) {
    publish(db, "session.step.started", json!({ "sessionID": sid, "assistantMessageID": msg, "agent": "build", "model": model(), "started": started }), created, flags);
}

/// "reads and replays native data stored under its pre-v2.1 names" (part-row half): an envelope update writes the
/// legacy row back in the current shape but leaves its inline parts where they are; the first part write moves them
/// to `session_part` in the current shape.
#[test]
fn an_envelope_update_keeps_legacy_inline_parts_and_the_first_part_write_splits_them() {
    let (db, sid) = setup();
    let msg = "msg_legacy_assistant";
    insert_row(
        &db,
        &sid,
        msg,
        "assistant",
        0,
        0,
        json!({
            "agent": "build",
            "model": { "id": "model", "providerID": "provider" },
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
            "time": { "created": 0 },
            "providerState": { "step": "kept" },
        }),
    );
    publish(
        &db,
        "session.step.ended",
        json!({
            "sessionID": sid, "assistantMessageID": msg, "finish": "stop", "providerState": { "step": "replayed" },
            "cost": 0, "tokens": { "input": 0, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
        }),
        0,
        ROWS,
    );
    let rewritten = row_data(&db, msg);
    assert!(rewritten.get("providerState").is_none());
    assert_eq!(rewritten["native"], json!({ "step": "replayed" }));
    assert_eq!(rewritten["content"].as_array().unwrap().len(), 3);
    // Left exactly as stored: still the pre-v2.1 names.
    assert_eq!(rewritten["content"][2]["providerState"], json!({ "call": "kept" }));
    assert!(parts(&db, msg).is_empty());

    publish(&db, "session.text.started", json!({ "sessionID": sid, "assistantMessageID": msg, "ordinal": 3 }), 0, ROWS);
    assert_matches(&row_data(&db, msg), json!({ "native": { "step": "replayed" }, "content": [] }));
    let rows = parts(&db, msg);
    let split: Vec<Value> = rows.iter().take(3).map(|p| p.5.clone()).collect();
    assert_matches(
        &Value::Array(split.clone()),
        json!([
            { "type": "text", "native": { "signature": "text" } },
            { "type": "reasoning", "native": { "signature": "reasoning" } },
            { "type": "tool", "native": { "call": "kept", "result": "kept" } },
        ]),
    );
    let keys: Vec<Vec<String>> = split
        .iter()
        .map(|p| {
            let mut k: Vec<String> = p.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        })
        .collect();
    assert_eq!(keys, vec![
        vec!["native", "text", "type"],
        vec!["native", "text", "type"],
        vec!["executed", "id", "name", "native", "state", "time", "type"],
    ]);
    assert_eq!(rows[3].5, json!({ "type": "text", "text": "" }));
}

/// "merges tool result native data into the call's native data" with part rows.
#[test]
fn merges_tool_result_native_data_into_the_calls_part_row() {
    let (db, sid) = setup();
    let msg = "msg_tools";
    assistant_row(&db, &sid, msg, 0, None, json!([]));
    let call = |id: &str| {
        publish(&db, "session.tool.input.started", json!({ "sessionID": sid, "assistantMessageID": msg, "id": id, "name": id }), 0, ROWS);
        publish(
            &db,
            "session.tool.called",
            json!({ "sessionID": sid, "assistantMessageID": msg, "id": id, "input": {}, "executed": true, "native": { "call": id } }),
            0,
            ROWS,
        );
    };
    call("succeeded");
    publish(
        &db,
        "session.tool.success",
        json!({ "sessionID": sid, "assistantMessageID": msg, "id": "succeeded", "content": [{ "type": "text", "text": "done" }], "executed": true, "native": { "result": "succeeded" } }),
        0,
        ROWS,
    );
    call("failed");
    publish(
        &db,
        "session.tool.failed",
        json!({ "sessionID": sid, "assistantMessageID": msg, "id": "failed", "error": { "type": "tool.execution", "message": "failed" }, "executed": true }),
        0,
        ROWS,
    );
    assert_matches(&row_data(&db, msg), json!({ "content": [] }));
    assert_matches(
        &assistant(&db, &sid, msg),
        json!({ "content": [
            { "id": "succeeded", "native": { "call": "succeeded", "result": "succeeded" }, "state": { "status": "completed" } },
            { "id": "failed", "native": { "call": "failed" }, "state": { "status": "error" } },
        ] }),
    );
}

fn png_uri() -> String {
    format!("data:image/png;base64,{PNG}")
}

fn png_hash() -> String {
    use sha2::Digest;
    let bytes = base64::engine::general_purpose::STANDARD.decode(PNG).unwrap();
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// "stores media once and refs it from inbox, message, and tool rows when experimental.media_blob is on" (the tool
/// half, with part rows): the tool's `data:` file goes to `blob`, its part row holds the ref, reads hydrate it.
#[test]
fn a_tool_part_row_stores_its_media_as_a_blob() {
    let (db, sid) = setup();
    let msg = "msg_media_tool";
    assistant_row(&db, &sid, msg, 0, None, json!([]));
    publish(&db, "session.tool.input.started", json!({ "sessionID": sid, "assistantMessageID": msg, "id": "shot", "name": "shot" }), 0, MEDIA_ROWS);
    publish(&db, "session.tool.called", json!({ "sessionID": sid, "assistantMessageID": msg, "id": "shot", "input": {}, "executed": true }), 0, MEDIA_ROWS);
    publish(
        &db,
        "session.tool.success",
        json!({ "sessionID": sid, "assistantMessageID": msg, "id": "shot", "content": [{ "type": "file", "uri": png_uri(), "mime": "image/png" }], "executed": true }),
        0,
        MEDIA_ROWS,
    );
    let hash = png_hash();
    let rows = parts(&db, msg);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2.as_deref(), Some("shot"));
    assert_matches(
        &rows[0].5,
        json!({ "state": { "status": "completed", "content": [{ "type": "file", "uri": format!("media:{hash}"), "mime": "image/png" }] } }),
    );
    assert_matches(
        &assistant(&db, &sid, msg),
        json!({ "content": [{ "state": { "content": [{ "type": "file", "uri": png_uri(), "mime": "image/png" }] } }] }),
    );
    let blob: (String, String, i64) =
        db.with(|c| c.query_row("SELECT hash, mime, bytes FROM blob", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))).unwrap();
    assert_eq!(blob, (hash.clone(), "image/png".into(), 68));
    let refs: (String, String, i64) =
        db.with(|c| c.query_row("SELECT message_id, hash, seq FROM blob_ref", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))).unwrap();
    assert_eq!(refs, (msg.into(), hash, 0));
}

fn interleaved(db: &Database, sid: &str, msg: &str, flags: Flags) {
    step_started(db, sid, msg, 1, 0, flags);
    let p = |kind: &str, data: Value| {
        let mut data = data;
        data["sessionID"] = json!(sid);
        data["assistantMessageID"] = json!(msg);
        publish(db, kind, data, 0, flags);
    };
    p("session.reasoning.started", json!({ "ordinal": 0 }));
    p("session.reasoning.ended", json!({ "ordinal": 0, "text": "plan first" }));
    p("session.text.started", json!({ "ordinal": 1 }));
    p("session.text.ended", json!({ "ordinal": 1, "text": "running two tools" }));
    p("session.tool.input.started", json!({ "id": "call_first", "name": "read" }));
    p("session.tool.called", json!({ "id": "call_first", "input": { "file": "a.ts" }, "executed": true }));
    p("session.tool.input.started", json!({ "id": "call_second", "name": "read" }));
    p("session.tool.called", json!({ "id": "call_second", "input": { "file": "b.ts" }, "executed": true }));
    // The second tool completes before the first.
    p("session.tool.success", json!({ "id": "call_second", "content": [{ "type": "text", "text": "b contents" }], "executed": true }));
    p("session.tool.success", json!({ "id": "call_first", "content": [{ "type": "text", "text": "a contents" }], "executed": true }));
    p("session.text.started", json!({ "ordinal": 4 }));
    p("session.text.ended", json!({ "ordinal": 4, "text": "all done" }));
    p("session.step.ended", json!({ "finish": "stop", "cost": 0, "tokens": { "input": 10, "output": 5, "reasoning": 2, "cache": { "read": 0, "write": 0 } } }));
}

/// "stores interleaved assistant parts in session_part by ordinal across out-of-order tool completions and forks"
#[test]
fn stores_interleaved_parts_by_ordinal_across_out_of_order_tool_completions_and_forks() {
    let (db, sid) = setup();
    let msg = "msg_interleaved";
    interleaved(&db, &sid, msg, ROWS);
    assert_matches(&row_data(&db, msg), json!({ "content": [] }));
    let rows: Vec<(i64, String, Option<String>)> = parts(&db, msg).into_iter().map(|p| (p.0, p.1, p.2)).collect();
    assert_eq!(rows, vec![
        (0, "reasoning".into(), None),
        (1, "text".into(), None),
        (2, "tool".into(), Some("call_first".into())),
        (3, "tool".into(), Some("call_second".into())),
        (4, "text".into(), None),
    ]);

    let fork = "ses_projected_fork";
    publish(&db, "session.forked", json!({ "sessionID": fork, "parentID": sid, "boundary": { "type": "through", "messageID": msg } }), 0, ROWS);
    let forked = messages(&db, fork);
    assert_eq!(forked.len(), 1);
    assert_matches(
        &forked[0],
        json!({ "type": "assistant", "content": [
            { "type": "reasoning", "text": "plan first" },
            { "type": "text", "text": "running two tools" },
            { "type": "tool", "id": "call_first", "state": { "status": "completed" } },
            { "type": "tool", "id": "call_second", "state": { "status": "completed" } },
            { "type": "text", "text": "all done" },
        ] }),
    );
    // The flag only changes where the parts are stored.
    let (off, off_sid) = setup();
    interleaved(&off, &off_sid, msg, OFF);
    assert_eq!(count(&off, "SELECT count(*) FROM session_part"), 0);
    assert_eq!(messages(&off, &off_sid), messages(&db, &sid));
}

/// "appends inline content after existing session_part rows when splitting an inline assistant row" (upstream runs it
/// with the flag off; both paths store the same rows).
#[test]
fn appends_inline_content_after_existing_part_rows_when_splitting_an_inline_row() {
    for flags in [OFF, ROWS] {
        let (db, sid) = setup();
        let msg = "msg_conflict_replace";
        insert_row(
            &db,
            &sid,
            msg,
            "assistant",
            1,
            10,
            json!({ "agent": "build", "model": model(), "content": [
                { "type": "reasoning", "text": "fresh inline reasoning", "time": { "created": 10, "completed": 12 } },
                { "type": "text", "text": "fresh inline text" },
            ], "time": { "created": 10 } }),
        );
        for (ordinal, text) in ["stale part 0", "stale part 1", "stale trailing part 2"].iter().enumerate() {
            insert_part(&db, &sid, msg, ordinal as i64, "text", None, ordinal as i64 + 1, json!({ "type": "text", "text": text }));
        }
        publish(&db, "session.text.started", json!({ "sessionID": sid, "assistantMessageID": msg, "ordinal": 5 }), 0, flags);
        publish(&db, "session.text.ended", json!({ "sessionID": sid, "assistantMessageID": msg, "ordinal": 5, "text": "appended after split" }), 0, flags);
        assert_matches(&row_data(&db, msg), json!({ "content": [] }));
        let rows: Vec<(i64, Value)> = parts(&db, msg).into_iter().map(|p| (p.0, p.5)).collect();
        let expected = [
            json!({ "type": "text", "text": "stale part 0" }),
            json!({ "type": "text", "text": "stale part 1" }),
            json!({ "type": "text", "text": "stale trailing part 2" }),
            json!({ "type": "reasoning", "text": "fresh inline reasoning" }),
            json!({ "type": "text", "text": "fresh inline text" }),
            json!({ "type": "text", "text": "appended after split" }),
        ];
        assert_eq!(rows.len(), expected.len());
        for (i, ((ordinal, data), e)) in rows.iter().zip(expected).enumerate() {
            assert_eq!(*ordinal, i as i64);
            assert_matches(data, e);
        }
    }
}

/// "keeps projecting into session_part rows a newer release wrote for an in-progress message", with the flag on:
/// only the rows a part event touches change, at the event's time.
#[test]
fn part_events_touch_only_their_rows() {
    let (db, sid) = setup();
    let msg = "msg_rolled_back";
    assistant_row(&db, &sid, msg, 0, None, json!([]));
    insert_part(&db, &sid, msg, 0, "text", None, 1, json!({ "type": "text", "text": "before rollback" }));
    insert_part(
        &db,
        &sid,
        msg,
        1,
        "tool",
        Some("call_a"),
        1,
        json!({ "type": "tool", "id": "call_a", "name": "read", "executed": true, "state": { "status": "running", "input": { "path": "a.ts" }, "metadata": {} }, "time": { "created": 1, "ran": 1 } }),
    );
    publish(&db, "session.tool.success", json!({ "sessionID": sid, "assistantMessageID": msg, "id": "call_a", "content": [{ "type": "text", "text": "export {}" }], "executed": true }), 5, ROWS);
    publish(&db, "session.text.started", json!({ "sessionID": sid, "assistantMessageID": msg, "ordinal": 1 }), 5, ROWS);
    publish(&db, "session.text.ended", json!({ "sessionID": sid, "assistantMessageID": msg, "ordinal": 1, "text": "after rollback" }), 5, ROWS);
    publish(&db, "session.tool.input.started", json!({ "sessionID": sid, "assistantMessageID": msg, "id": "call_b", "name": "grep" }), 5, ROWS);
    assert_matches(
        &assistant(&db, &sid, msg),
        json!({ "content": [
            { "type": "text", "text": "before rollback" },
            { "type": "tool", "id": "call_a", "state": { "status": "completed", "content": [{ "type": "text", "text": "export {}" }] } },
            { "type": "text", "text": "after rollback" },
            { "type": "tool", "id": "call_b", "state": { "status": "streaming" } },
        ] }),
    );
    assert_matches(&row_data(&db, msg), json!({ "content": [] }));
    let rows: Vec<(i64, String, Option<String>, i64, i64)> = parts(&db, msg).into_iter().map(|p| (p.0, p.1, p.2, p.3, p.4)).collect();
    assert_eq!(rows, vec![
        (0, "text".into(), None, 1, 1),
        (1, "tool".into(), Some("call_a".into()), 1, 5),
        (2, "text".into(), None, 5, 5),
        (3, "tool".into(), Some("call_b".into()), 5, 5),
    ]);
    // A part event that changes nothing (a second success, the same streamed input) leaves its row alone.
    publish(&db, "session.tool.success", json!({ "sessionID": sid, "assistantMessageID": msg, "id": "call_a", "content": [{ "type": "text", "text": "again" }], "executed": true }), 9, ROWS);
    publish(&db, "session.tool.input.ended", json!({ "sessionID": sid, "assistantMessageID": msg, "id": "call_b", "text": "" }), 9, ROWS);
    // One for a part the message does not have writes nothing.
    publish(&db, "session.tool.called", json!({ "sessionID": sid, "assistantMessageID": msg, "id": "call_missing", "input": {}, "executed": true }), 9, ROWS);
    publish(&db, "session.reasoning.ended", json!({ "sessionID": sid, "assistantMessageID": msg, "ordinal": 0, "text": "none" }), 9, ROWS);
    let after: Vec<(i64, i64)> = parts(&db, msg).into_iter().map(|p| (p.0, p.4)).collect();
    assert_eq!(after, vec![(0, 1), (1, 5), (2, 5), (3, 5)]);
    publish(&db, "session.tool.input.ended", json!({ "sessionID": sid, "assistantMessageID": msg, "id": "call_b", "text": "{\"q\":1}" }), 9, ROWS);
    assert_eq!(parts(&db, msg)[3].4, 9);
}

/// "deletes a reverted message's session_part rows and the blobs only they referenced"
#[test]
fn a_revert_deletes_the_messages_part_rows_and_the_blobs_only_they_referenced() {
    let (db, sid) = setup();
    let msg = "msg_media_tool_reverted";
    step_started(&db, &sid, msg, 1, 0, MEDIA_ROWS);
    publish(&db, "session.tool.input.started", json!({ "sessionID": sid, "assistantMessageID": msg, "id": "shot", "name": "shot" }), 0, MEDIA_ROWS);
    publish(&db, "session.tool.called", json!({ "sessionID": sid, "assistantMessageID": msg, "id": "shot", "input": {}, "executed": true }), 0, MEDIA_ROWS);
    publish(
        &db,
        "session.tool.success",
        json!({ "sessionID": sid, "assistantMessageID": msg, "id": "shot", "content": [{ "type": "file", "uri": png_uri(), "mime": "image/png" }], "executed": true }),
        0,
        MEDIA_ROWS,
    );
    assert_eq!(count(&db, "SELECT count(*) FROM session_part"), 1);
    assert_eq!(count(&db, "SELECT count(*) FROM blob"), 1);
    publish(&db, "session.revert.committed", json!({ "sessionID": sid, "to": msg }), 0, MEDIA_ROWS);
    assert_eq!(count(&db, "SELECT count(*) FROM session_part"), 0);
    assert_eq!(count(&db, "SELECT count(*) FROM blob_ref"), 0);
    assert_eq!(count(&db, "SELECT count(*) FROM blob"), 0);
}

fn inline_shot() -> Value {
    json!({
        "type": "tool", "id": "call_inline_shot", "name": "screenshot", "executed": true,
        "state": { "status": "completed", "input": {}, "content": [{ "type": "file", "uri": png_uri(), "mime": "image/png" }] },
        "time": { "created": 2, "ran": 2, "completed": 2 },
    })
}

/// "moves a legacy inline media message's rewritten content to session_part with its media as a blob"
#[test]
fn a_content_rewrite_moves_a_legacy_inline_media_message_to_part_rows() {
    let (db, sid) = setup();
    let msg = "msg_legacy_media_rewritten";
    assistant_row(&db, &sid, msg, 0, Some(0), json!([inline_shot()]));
    publish(
        &db,
        "session.message.content.updated",
        json!({ "sessionID": sid, "messageID": msg, "content": [{ "type": "text", "text": "rewritten" }, inline_shot()] }),
        0,
        MEDIA_ROWS,
    );
    let hash = png_hash();
    assert_matches(&row_data(&db, msg), json!({ "content": [] }));
    let rows: Vec<Value> = parts(&db, msg).into_iter().map(|p| p.5).collect();
    assert_matches(
        &Value::Array(rows),
        json!([
            { "type": "text", "text": "rewritten" },
            { "type": "tool", "state": { "content": [{ "type": "file", "uri": format!("media:{hash}") }] } },
        ]),
    );
    assert_eq!(count(&db, "SELECT count(*) FROM blob"), 1);
    let refs: (String, String) = db.with(|c| c.query_row("SELECT message_id, hash FROM blob_ref", [], |r| Ok((r.get(0)?, r.get(1)?)))).unwrap();
    assert_eq!(refs, (msg.into(), hash));
    assert_matches(
        &assistant(&db, &sid, msg),
        json!({ "content": [
            { "type": "text", "text": "rewritten" },
            { "type": "tool", "id": "call_inline_shot", "state": { "content": [{ "type": "file", "uri": png_uri() }] } },
        ] }),
    );
    // A rewrite that drops the media releases it.
    publish(&db, "session.message.content.updated", json!({ "sessionID": sid, "messageID": msg, "content": [{ "type": "text", "text": "only text" }] }), 1, MEDIA_ROWS);
    assert_eq!(parts(&db, msg).iter().map(|p| (p.0, p.3, p.4)).collect::<Vec<_>>(), vec![(0, 1, 1)]);
    assert_eq!(count(&db, "SELECT count(*) FROM blob_ref"), 0);
    assert_eq!(count(&db, "SELECT count(*) FROM blob"), 0);
}

/// `projectTwoSteps`: two assistant steps, the first with reasoning, text and a tool call; event `index` projects
/// with part rows on when `rows_at(index)`.
fn project_two_steps(db: &Database, sid: &str, rows_at: impl Fn(usize) -> bool) {
    let first = format!("msg_{sid}_first");
    let second = format!("msg_{sid}_second");
    let events: Vec<(&str, Value)> = vec![
        ("session.step.started", json!({ "assistantMessageID": first, "agent": "build", "model": model(), "started": 1 })),
        ("session.reasoning.started", json!({ "assistantMessageID": first, "ordinal": 0 })),
        ("session.reasoning.ended", json!({ "assistantMessageID": first, "ordinal": 0, "text": "plan" })),
        ("session.text.started", json!({ "assistantMessageID": first, "ordinal": 1 })),
        ("session.text.ended", json!({ "assistantMessageID": first, "ordinal": 1, "text": "reading" })),
        ("session.tool.input.started", json!({ "assistantMessageID": first, "id": "call", "name": "read" })),
        ("session.tool.called", json!({ "assistantMessageID": first, "id": "call", "input": { "file": "a.ts" }, "executed": true })),
        ("session.tool.success", json!({ "assistantMessageID": first, "id": "call", "content": [{ "type": "text", "text": "a contents" }], "executed": true })),
        ("session.step.ended", { let mut u = usage(); u["assistantMessageID"] = json!(first); u["finish"] = json!("tool-calls"); u }),
        ("session.step.started", json!({ "assistantMessageID": second, "agent": "build", "model": model(), "started": 2 })),
        ("session.text.started", json!({ "assistantMessageID": second, "ordinal": 0 })),
        ("session.text.ended", json!({ "assistantMessageID": second, "ordinal": 0, "text": "done" })),
        ("session.step.ended", { let mut u = usage(); u["assistantMessageID"] = json!(second); u["finish"] = json!("stop"); u }),
    ];
    for (index, (kind, mut data)) in events.into_iter().enumerate() {
        data["sessionID"] = json!(sid);
        publish(db, kind, data, 0, if rows_at(index) { ROWS } else { OFF });
    }
}

/// "writes whole messages inline with experimental.part_rows off"
#[test]
fn writes_whole_messages_inline_with_part_rows_off() {
    let (db, sid) = setup();
    project_two_steps(&db, &sid, |_| false);
    assert_eq!(count(&db, "SELECT count(*) FROM session_part"), 0);
    let assistants: Vec<Value> = messages(&db, &sid).into_iter().filter(|m| m["type"] == "assistant").collect();
    assert_eq!(assistants.iter().map(|m| m["content"].as_array().unwrap().len()).collect::<Vec<_>>(), [3, 1]);
    for m in assistants {
        let mut data = m.clone();
        let o = data.as_object_mut().unwrap();
        let id = o.shift_remove("id").unwrap();
        o.shift_remove("type");
        assert_eq!(row_data(&db, id.as_str().unwrap()), data);
    }
}

/// "reads the same reply when experimental.part_rows is switched on, off and on again mid-session"
#[test]
fn reads_the_same_reply_when_part_rows_is_switched_on_off_and_on_again_mid_session() {
    let (db, sid) = setup();
    let (off, on) = ("ses_part_rows_off", "ses_part_rows_on");
    add_session(&db, off);
    add_session(&db, on);
    project_two_steps(&db, off, |_| false);
    project_two_steps(&db, on, |_| true);
    // On for the first step's start, off for its tool call, on for its end; then off for the second step's text start.
    project_two_steps(&db, &sid, |index| ![4, 5, 6, 7, 10].contains(&index));
    let mixed = without_ids(messages(&db, &sid));
    assert_eq!(mixed, without_ids(messages(&db, off)));
    assert_eq!(mixed, without_ids(messages(&db, on)));
    assert_eq!(
        mixed.iter().filter(|m| m["type"] == "assistant").map(|m| m["content"].as_array().unwrap().len()).collect::<Vec<_>>(),
        [3, 1]
    );
    let grouped: Vec<Vec<i64>> = [format!("msg_{sid}_first"), format!("msg_{sid}_second")]
        .iter()
        .map(|m| parts(&db, m).into_iter().map(|p| p.0).collect())
        .collect();
    assert_eq!(grouped, vec![vec![0, 1, 2], vec![0]]);
    let rows: Vec<(String, String)> = db
        .with(|c| {
            let mut s = c.prepare("SELECT type, data FROM session_message WHERE session_id = ? ORDER BY seq")?;
            let rows = s.query_map([&sid], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect()
        })
        .unwrap();
    for (kind, data) in rows {
        if kind == "assistant" {
            assert_eq!(serde_json::from_str::<Value>(&data).unwrap()["content"], json!([]));
        }
    }
    // The session written with the flag on throughout stores the same rows.
    for m in ["first", "second"] {
        let strip = |rows: Vec<(i64, String, Option<String>, i64, i64, Value)>| -> Vec<(i64, String, Option<String>, Value)> {
            rows.into_iter().map(|p| (p.0, p.1, p.2, p.5)).collect()
        };
        assert_eq!(strip(parts(&db, &format!("msg_{sid}_{m}"))), strip(parts(&db, &format!("msg_{on}_{m}"))));
    }
}

/// `session-store.test.ts` (with part rows): an untouched legacy inline row, a legacy row a part event split, and a
/// step projected straight into part rows read the same through every read path.
#[test]
fn reads_legacy_split_and_part_row_messages_alike() {
    let (db, sid) = setup();
    let (untouched, mutated, split) = ("msg_legacy_untouched", "msg_legacy_mutated", "msg_split");
    insert_row(
        &db,
        &sid,
        untouched,
        "assistant",
        -2,
        10,
        json!({ "agent": "build", "model": model(), "content": [{ "type": "text", "text": "legacy inline only" }], "time": { "created": 10, "completed": 11 } }),
    );
    insert_row(
        &db,
        &sid,
        mutated,
        "assistant",
        -1,
        20,
        json!({ "agent": "build", "model": model(), "content": [{ "type": "reasoning", "text": "legacy reasoning" }], "time": { "created": 20 } }),
    );
    publish(&db, "session.text.started", json!({ "sessionID": sid, "assistantMessageID": mutated, "ordinal": 1 }), 0, ROWS);
    publish(&db, "session.text.ended", json!({ "sessionID": sid, "assistantMessageID": mutated, "ordinal": 1, "text": "appended into session_part" }), 0, ROWS);
    step_started(&db, &sid, split, 30, 0, ROWS);
    let p = |kind: &str, mut data: Value| {
        data["sessionID"] = json!(sid);
        data["assistantMessageID"] = json!(split);
        publish(&db, kind, data, 0, ROWS);
    };
    p("session.reasoning.started", json!({ "ordinal": 0 }));
    p("session.reasoning.ended", json!({ "ordinal": 0, "text": "step reasoning" }));
    p("session.tool.input.started", json!({ "id": "call_1", "name": "bash" }));
    p("session.tool.called", json!({ "id": "call_1", "input": { "command": "ls" }, "executed": true }));
    p("session.tool.success", json!({ "id": "call_1", "content": [{ "type": "text", "text": "ok" }], "executed": true }));
    p("session.text.started", json!({ "ordinal": 2 }));
    p("session.text.ended", json!({ "ordinal": 2, "text": "step answer" }));

    assert_matches(&row_data(&db, mutated), json!({ "content": [] }));
    assert_eq!(parts(&db, mutated).len(), 2);
    let all = messages(&db, &sid);
    assert_matches(
        &Value::Array(all.clone()),
        json!([
            { "id": untouched, "content": [{ "type": "text", "text": "legacy inline only" }] },
            { "id": mutated, "content": [{ "type": "reasoning", "text": "legacy reasoning" }, { "type": "text", "text": "appended into session_part" }] },
            { "id": split, "content": [
                { "type": "reasoning", "text": "step reasoning" },
                { "type": "tool", "id": "call_1", "state": { "status": "completed" } },
                { "type": "text", "text": "step answer" },
            ] },
        ]),
    );
    // The carried legacy part keeps the row's creation time and is updated at the event's.
    assert_eq!(parts(&db, mutated).iter().map(|p| (p.0, p.3)).collect::<Vec<_>>(), vec![(0, 20), (1, 0)]);
}

/// Envelope updates with the flag on (`updateMessage` of `partRowWrites`) never write `content`: a legacy inline row
/// keeps its content as stored, a split row keeps `[]`; `time_created` follows only `session.step.started`; shells
/// and compactions are written whole.
#[test]
fn envelope_updates_leave_content_and_only_a_step_start_moves_the_creation_time() {
    let (db, sid) = setup();
    let msg = "msg_envelope";
    insert_row(
        &db,
        &sid,
        msg,
        "assistant",
        0,
        5,
        json!({ "agent": "build", "model": model(), "content": [{ "type": "text", "text": "inline", "state": { "kept": true } }], "time": { "created": 5 } }),
    );
    publish(
        &db,
        "session.retry.scheduled",
        json!({ "sessionID": sid, "assistantMessageID": msg, "attempt": 1, "at": 99, "error": { "type": "provider.unknown", "message": "boom" } }),
        7,
        ROWS,
    );
    let data = row_data(&db, msg);
    assert_eq!(data["content"], json!([{ "type": "text", "text": "inline", "state": { "kept": true } }]));
    assert_eq!(data["retry"]["attempt"], json!(1));
    let created = || count(&db, &format!("SELECT time_created FROM session_message WHERE id = '{msg}'"));
    publish(&db, "session.step.first-token", json!({ "sessionID": sid, "assistantMessageID": msg }), 8, ROWS);
    assert_eq!(created(), 5);
    // A step restarting the message: the creation time follows, the retry clears, the inline content stays.
    step_started(&db, &sid, msg, 40, 9, ROWS);
    assert_eq!(created(), 40);
    let data = row_data(&db, msg);
    assert!(data.get("retry").is_none());
    assert_eq!(data["time"], json!({ "created": 40 }));
    assert_eq!(data["content"].as_array().unwrap().len(), 1);
    assert!(parts(&db, msg).is_empty());
    // A part write splits it; envelope updates then keep `[]`.
    publish(&db, "session.text.started", json!({ "sessionID": sid, "assistantMessageID": msg, "ordinal": 1 }), 10, ROWS);
    publish(&db, "session.step.streamed", json!({ "sessionID": sid, "assistantMessageID": msg }), 11, ROWS);
    assert_eq!(row_data(&db, msg)["content"], json!([]));
    assert_eq!(row_data(&db, msg)["time"]["streamed"], json!(11));
    assert_matches(&assistant(&db, &sid, msg), json!({ "content": [{ "type": "text", "text": "inline", "native": { "kept": true } }, { "type": "text", "text": "" }] }));

    // Shells and compactions.
    publish(
        &db,
        "session.shell.started",
        json!({ "sessionID": sid, "shell": shell("running", None) }),
        13,
        ROWS,
    );
    publish(
        &db,
        "session.shell.ended",
        json!({ "sessionID": sid, "shell": shell("exited", Some(0)), "output": { "output": "a\n", "cursor": 2, "size": 2, "truncated": false } }),
        14,
        ROWS,
    );
    publish(&db, "session.compaction.started", json!({ "sessionID": sid, "reason": "manual" }), 15, ROWS);
    publish(&db, "session.compaction.ended", json!({ "sessionID": sid, "reason": "manual", "text": "summary" }), 16, ROWS);
    let all = messages(&db, &sid);
    assert_matches(&all[1], json!({ "type": "shell", "status": "exited", "exit": 0, "time": { "completed": 14 } }));
    assert_matches(&all[2], json!({ "type": "compaction", "status": "completed", "summary": "summary" }));
}

fn shell(status: &str, exit: Option<i64>) -> Value {
    let mut shell = json!({ "id": "sh_1", "status": status, "command": "ls", "cwd": "/project", "shell": "/bin/sh", "file": "/tmp/sh_1", "metadata": {}, "time": { "started": 13 } });
    if let Some(exit) = exit {
        shell["exit"] = json!(exit);
    }
    shell
}

/// The flag-on rows round-trip through forks and deletes: a fork copies settled messages' part rows (not an
/// in-progress one's), and deleting either session leaves the other's rows.
#[test]
fn forks_and_deletes_keep_each_sessions_part_rows() {
    let (db, sid) = setup();
    project_two_steps(&db, &sid, |_| true);
    let open = "msg_open";
    step_started(&db, &sid, open, 3, 0, ROWS);
    publish(&db, "session.text.started", json!({ "sessionID": sid, "assistantMessageID": open, "ordinal": 0 }), 0, ROWS);
    let fork = "ses_fork_rows";
    publish(&db, "session.forked", json!({ "sessionID": fork, "parentID": sid, "boundary": { "type": "through", "messageID": open } }), 0, ROWS);
    let parent = without_ids(messages(&db, &sid));
    let child = without_ids(messages(&db, fork));
    assert_eq!(child.len(), 2);
    assert_eq!(child[..], parent[..2]);
    assert_eq!(count(&db, &format!("SELECT count(*) FROM session_part WHERE session_id = '{fork}'")), 4);
    publish(&db, "session.deleted", json!({ "sessionID": sid }), 0, ROWS);
    assert_eq!(count(&db, &format!("SELECT count(*) FROM session_part WHERE session_id = '{sid}'")), 0);
    assert_eq!(without_ids(messages(&db, fork)), child);
    publish(&db, "session.deleted", json!({ "sessionID": fork }), 0, ROWS);
    assert_eq!(count(&db, "SELECT count(*) FROM session_part"), 0);
}

/// A step written with the flag on decodes back from its rows, whose `type` and `call_id` columns name each part.
#[test]
fn a_step_written_as_part_rows_decodes_back() {
    let (db, sid) = setup();
    project_two_steps(&db, &sid, |_| true);
    let first = format!("msg_{sid}_first");
    let decoded: Vec<Message> = db.session_messages(&sid).unwrap().into_iter().map(|(_, m)| m).collect();
    let Message::Assistant(a) = &decoded[0] else { panic!("assistant") };
    assert_eq!(a.id, first);
    assert_eq!(a.content.len(), 3);
    assert_eq!(parts(&db, &first).iter().map(|p| (p.1.as_str(), p.2.as_deref())).collect::<Vec<_>>(), [
        ("reasoning", None),
        ("text", None),
        ("tool", Some("call"))
    ]);
}
