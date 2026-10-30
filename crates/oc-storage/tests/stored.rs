//! v2.2 storage (`core/test/session-projector.test.ts`, `session-store.test.ts`, `database` tests):
//! the 8 new migrations, `session_v2.time_busy`, assistant parts a newer release moved to
//! `session_part`, and media kept in `blob` behind `media:` refs (read, hydrate, release).

use base64::Engine;
use oc_schema::event::{Delivery, InboxItem, InterruptReason, SessionEvent};
use oc_schema::types::*;
use oc_schema::{Message, id};
use oc_storage::Database;
use oc_storage::projector::Committing;
use serde_json::{Value, json};

fn commit(db: &Database, e: SessionEvent, created: u64) -> (String, i64) {
    let eid = id::event();
    let seq = db.commit_session_event(Committing { id: &eid, created, metadata: None, event: &e }).unwrap();
    (eid, seq)
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
        model: Some(ModelRef { id: "m".into(), provider_id: "p".into(), variant: None }),
        metadata: None,
        permissions: None,
        version: "2.2.2".into(),
    }
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
    commit(&db, created(&sid), 0);
    db.with(|c| c.execute("UPDATE session_v2 SET time_updated = 0 WHERE id = ?", [&sid])).unwrap();
    (db, sid)
}

fn exec(db: &Database, sql: &str, p: impl rusqlite::Params) {
    db.with(|c| c.execute(sql, p)).unwrap();
}

fn insert_row(db: &Database, session: &str, id: &str, kind: &str, seq: i64, data: Value) {
    exec(
        db,
        "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?, ?, ?, ?, 0, 0, ?)",
        rusqlite::params![id, session, kind, seq, data.to_string()],
    );
    exec(
        db,
        "INSERT INTO event_sequence (aggregate_id, seq) VALUES (?1, ?2) ON CONFLICT(aggregate_id) DO UPDATE SET seq = max(seq, ?2)",
        rusqlite::params![session, seq],
    );
}

fn insert_part(db: &Database, session: &str, message: &str, ordinal: i64, call: Option<&str>, data: Value) {
    exec(
        db,
        "INSERT INTO session_part (message_id, session_id, ordinal, type, call_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?, 1, 1, ?)",
        rusqlite::params![message, session, ordinal, data["type"].as_str().unwrap(), call, data.to_string()],
    );
}

fn assistant_row(completed: Option<u64>) -> Value {
    let mut time = json!({"created": 0});
    if let Some(c) = completed {
        time["completed"] = json!(c);
    }
    json!({"time": time, "agent": "build", "model": {"id": "m", "providerID": "p", "variant": "default"}, "content": []})
}

fn row_data(db: &Database, id: &str) -> Value {
    let data: String = db.with(|c| c.query_row("SELECT data FROM session_message WHERE id = ?", [id], |r| r.get(0))).unwrap();
    serde_json::from_str(&data).unwrap()
}

fn message_json(db: &Database, session: &str, id: &str) -> Value {
    let (_, m) = db.session_messages(session).unwrap().into_iter().find(|(_, m)| m.id() == id).expect("message");
    serde_json::to_value(&m).unwrap()
}

fn tables(db: &Database) -> Vec<String> {
    db.with(|c| {
        let mut s = c.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?;
        let r = s.query_map([], |r| r.get(0))?.collect();
        r
    })
    .unwrap()
}

/// `PRAGMA table_info` as sorted `(name, type, notnull, default, pk)` (column order differs between fresh and
/// migrated databases, so only the set is compared).
fn columns(db: &Database, table: &str) -> Vec<(String, String, i64, Option<String>, i64)> {
    let mut cols: Vec<_> = db
        .with(|c| {
            let mut s = c.prepare(&format!("PRAGMA table_info(`{table}`)"))?;
            let r = s.query_map([], |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect();
            r
        })
        .unwrap();
    cols.sort();
    cols
}

fn indexes(db: &Database) -> Vec<(String, String)> {
    db.with(|c| {
        let mut s = c.prepare(
            "SELECT name, sql FROM sqlite_master WHERE type = 'index' AND sql IS NOT NULL ORDER BY name",
        )?;
        let r = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect();
        r
    })
    .unwrap()
}

#[test]
fn a_database_migrated_from_2_1_4_matches_a_fresh_2_2_2_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("opencode.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(include_str!("fixtures/schema-2.1.4.sql")).unwrap();
        c.execute_batch("CREATE TABLE \"migration\" (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)").unwrap();
        for id in include_str!("fixtures/migration-ids-2.1.4.txt").lines() {
            c.execute("INSERT INTO migration (id, time_completed) VALUES (?, 0)", [id]).unwrap();
        }
        c.execute("INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) VALUES ('global', '/', 0, 0, '[]')", [])
            .unwrap();
        c.execute(
            "INSERT INTO session_v2 (id, project_id, slug, directory, version, time_created, time_updated) VALUES ('ses_old', 'global', 's', '/tmp', '2.1.4', 1, 2)",
            [],
        )
        .unwrap();
    }
    let migrated = Database::open(&path, dir.path()).unwrap();
    let fresh = Database::memory().unwrap();

    let ids = |db: &Database| -> Vec<String> {
        db.with(|c| {
            let mut s = c.prepare("SELECT id FROM migration ORDER BY id")?;
            let r = s.query_map([], |r| r.get(0))?.collect();
            r
        })
        .unwrap()
    };
    assert_eq!(ids(&migrated).len(), 58);
    assert_eq!(ids(&migrated), ids(&fresh));
    assert_eq!(ids(&fresh).last().unwrap(), "20261029165540_schedule_checks");

    let mut expected = tables(&fresh);
    expected.retain(|t| t != "migration");
    let mut got = tables(&migrated);
    got.retain(|t| t != "migration");
    assert_eq!(got, expected);
    assert!(!expected.contains(&"session_pending".to_string()));
    for t in ["session_v2", "schedule", "session_part", "blob", "blob_ref"] {
        assert!(expected.contains(&t.to_string()), "{t}");
        assert_eq!(columns(&migrated, t), columns(&fresh, t), "{t}");
    }
    for t in &expected {
        assert_eq!(columns(&migrated, t), columns(&fresh, t), "{t}");
    }
    assert_eq!(indexes(&migrated), indexes(&fresh));
    assert!(columns(&fresh, "session_v2").iter().all(|c| c.0 != "allowance"));
    // the session survives with no busy time
    let info = migrated.session_get("ses_old").unwrap().unwrap();
    assert_eq!(info.time.busy, None);
}

/// Python's `repr` of a str (`b1/dump.py` wrote the fixture with it).
fn py_str(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

fn py_opt(s: &Option<String>) -> String {
    s.as_deref().map(py_str).unwrap_or_else(|| "None".into())
}

/// The schema dump `tests/fixtures/schema-2.2.2.txt` holds, taken from a 2.1.4 database that the real
/// opencode 2.2.2 binary migrated: migration ids, every table's columns and foreign keys, every index.
fn schema_dump(db: &Database) -> String {
    db.with(|c| {
        let mut out = Vec::new();
        let ids: Vec<String> = c.prepare("SELECT id FROM migration ORDER BY id")?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
        out.push(format!("migrations {}: last={}", ids.len(), ids.last().cloned().unwrap_or_default()));
        out.extend(ids.iter().map(|i| format!("  mig {i}")));
        let tables: Vec<String> = c
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for t in tables {
            out.push(format!("table {t}"));
            let mut cols: Vec<(String, String, i64, Option<String>, i64)> = c
                .prepare(&format!("PRAGMA table_info(`{t}`)"))?
                .query_map([], |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
                .collect::<Result<_, _>>()?;
            cols.sort();
            for (name, ty, notnull, dflt, pk) in cols {
                out.push(format!("  col ({}, {}, {notnull}, {}, {pk})", py_str(&name), py_str(&ty), py_opt(&dflt)));
            }
            let mut fks: Vec<Vec<String>> = c
                .prepare(&format!("PRAGMA foreign_key_list(`{t}`)"))?
                .query_map([], |r| (2..8).map(|i| r.get::<_, String>(i)).collect::<Result<Vec<_>, _>>())?
                .collect::<Result<_, _>>()?;
            fks.sort();
            for f in fks {
                out.push(format!("  fk ({})", f.iter().map(|v| py_str(v)).collect::<Vec<_>>().join(", ")));
            }
        }
        let indexes: Vec<(String, String)> = c
            .prepare("SELECT name, sql FROM sqlite_master WHERE type='index' AND sql IS NOT NULL ORDER BY name")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        out.extend(indexes.into_iter().map(|(n, s)| format!("index {n}: {s}")));
        Ok(out.join("\n") + "\n")
    })
    .unwrap()
}

/// A 2.1.4 database migrated by ocd, and a fresh one, have exactly the schema the real 2.2.2 binary gives a
/// migrated 2.1.4 database.
#[test]
fn schema_matches_the_one_the_2_2_2_binary_migrates_to() {
    let expected = include_str!("fixtures/schema-2.2.2.txt");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("opencode.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(include_str!("fixtures/schema-2.1.4.sql")).unwrap();
        c.execute_batch("CREATE TABLE \"migration\" (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)").unwrap();
        for id in include_str!("fixtures/migration-ids-2.1.4.txt").lines() {
            c.execute("INSERT INTO migration (id, time_completed) VALUES (?, 0)", [id]).unwrap();
        }
    }
    let migrated = Database::open(&path, dir.path()).unwrap();
    assert_eq!(schema_dump(&migrated), expected);
    assert_eq!(schema_dump(&Database::memory().unwrap()), expected);
}

#[test]
fn records_each_executions_start_including_a_resume_after_shutdown() {
    let (db, sid) = setup();
    let times = || -> (Option<i64>, Option<i64>, i64) {
        db.with(|c| c.query_row("SELECT time_busy, time_idle, time_updated FROM session_v2 WHERE id = ?", [&sid], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        }))
        .unwrap()
    };
    commit(&db, SessionEvent::ExecutionStarted { session_id: sid.clone() }, 10);
    assert_eq!(times(), (Some(10), None, 0));
    commit(&db, SessionEvent::ExecutionInterrupted { session_id: sid.clone(), reason: InterruptReason::Shutdown }, 20);
    commit(&db, SessionEvent::ExecutionStarted { session_id: sid.clone() }, 30);
    assert_eq!(times(), (Some(30), None, 0));
    commit(&db, SessionEvent::ExecutionSucceeded { session_id: sid.clone() }, 40);
    assert_eq!(times(), (Some(30), Some(40), 0));
    commit(&db, SessionEvent::ExecutionStarted { session_id: sid.clone() }, 50);
    assert_eq!(times(), (Some(50), Some(40), 0));

    let info = db.session_get(&sid).unwrap().unwrap();
    assert_eq!(info.time.busy, Some(50));
    let v = serde_json::to_value(&info.time).unwrap();
    assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["created", "updated", "busy", "idle"]);
}

// A newer release wrote this in-progress message's parts as session_part rows, then the user rolled back.
#[test]
fn keeps_projecting_into_session_part_rows_a_newer_release_wrote_for_an_in_progress_message() {
    let (db, sid) = setup();
    let mid = "msg_rolled_back";
    insert_row(&db, &sid, mid, "assistant", 0, assistant_row(None));
    insert_part(&db, &sid, mid, 0, None, json!({"type": "text", "text": "before rollback"}));
    insert_part(
        &db,
        &sid,
        mid,
        1,
        Some("call_a"),
        json!({"type": "tool", "id": "call_a", "name": "read", "executed": true,
               "state": {"status": "running", "input": {"path": "a.ts"}, "metadata": {}}, "time": {"created": 1, "ran": 1}}),
    );

    commit(
        &db,
        SessionEvent::ToolSuccess {
            session_id: sid.clone(),
            assistant_message_id: mid.into(),
            id: "call_a".into(),
            content: vec![ToolContent::Text { text: "export {}".into() }],
            metadata: None,
            executed: true,
            native: None,
        },
        5,
    );
    commit(&db, SessionEvent::TextStarted { session_id: sid.clone(), assistant_message_id: mid.into(), ordinal: 1 }, 5);
    commit(
        &db,
        SessionEvent::TextEnded {
            session_id: sid.clone(),
            assistant_message_id: mid.into(),
            ordinal: 1,
            text: "after rollback".into(),
            native: None,
        },
        5,
    );
    commit(
        &db,
        SessionEvent::ToolInputStarted { session_id: sid.clone(), assistant_message_id: mid.into(), id: "call_b".into(), name: "grep".into() },
        5,
    );

    let m = message_json(&db, &sid, mid);
    let content = m["content"].as_array().unwrap();
    assert_eq!(content.len(), 4);
    assert_eq!(content[0], json!({"type": "text", "text": "before rollback"}));
    assert_eq!(content[1]["id"], "call_a");
    assert_eq!(content[1]["state"]["status"], "completed");
    assert_eq!(content[1]["state"]["content"], json!([{"type": "text", "text": "export {}"}]));
    assert_eq!(content[2], json!({"type": "text", "text": "after rollback"}));
    assert_eq!(content[3]["id"], "call_b");
    assert_eq!(content[3]["state"]["status"], "streaming");

    assert_eq!(row_data(&db, mid)["content"], json!([]));
    let parts: Vec<(i64, String, Option<String>, i64)> = db
        .with(|c| {
            let mut s = c.prepare("SELECT ordinal, type, call_id, time_updated FROM session_part WHERE message_id = ? ORDER BY ordinal")?;
            let r = s.query_map([mid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect();
            r
        })
        .unwrap();
    assert_eq!(
        parts,
        vec![
            (0, "text".into(), None, 1),
            (1, "tool".into(), Some("call_a".into()), 5),
            (2, "text".into(), None, 5),
            (3, "tool".into(), Some("call_b".into()), 5),
        ]
    );
}

#[test]
fn hydrates_assistant_parts_from_session_part_when_present_and_falls_back_to_inline_content() {
    let (db, sid) = setup();
    insert_row(&db, &sid, "msg_inline", "assistant", 0, {
        let mut r = assistant_row(Some(1));
        r["content"] = json!([{"type": "text", "text": "inline"}]);
        r
    });
    insert_row(&db, &sid, "msg_split", "assistant", 1, assistant_row(Some(2)));
    insert_part(&db, &sid, "msg_split", 1, None, json!({"type": "text", "text": "second"}));
    insert_part(&db, &sid, "msg_split", 0, None, json!({"type": "text", "text": "first"}));
    assert_eq!(message_json(&db, &sid, "msg_inline")["content"], json!([{"type": "text", "text": "inline"}]));
    assert_eq!(
        message_json(&db, &sid, "msg_split")["content"],
        json!([{"type": "text", "text": "first"}, {"type": "text", "text": "second"}])
    );
}

#[test]
fn forks_a_settled_message_whose_parts_are_in_session_part_with_its_content() {
    let (db, sid) = setup();
    let mid = "msg_split_settled";
    insert_row(&db, &sid, mid, "assistant", 0, assistant_row(Some(0)));
    insert_part(&db, &sid, mid, 0, None, json!({"type": "text", "text": "split answer"}));
    let child = id::session();
    commit(
        &db,
        SessionEvent::Forked {
            session_id: child.clone(),
            parent_id: sid.clone(),
            boundary: ForkBoundary { kind: ForkBoundaryType::Through, message_id: mid.into() },
            instructions: None,
            instruction_entries: None,
        },
        10,
    );
    let msgs = db.session_messages(&child).unwrap();
    assert_eq!(msgs.len(), 1);
    let v = serde_json::to_value(&msgs[0].1).unwrap();
    assert_eq!(v["type"], "assistant");
    assert_eq!(v["content"], json!([{"type": "text", "text": "split answer"}]));
    // the parent keeps its part rows, the child has none (its content is inline)
    let n: i64 = db.with(|c| c.query_row("SELECT count(*) FROM session_part", [], |r| r.get(0))).unwrap();
    assert_eq!(n, 1);
}

const PNG: &[u8] = b"\x89PNG fake bytes";

fn hash(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

fn insert_blob(db: &Database, bytes: &[u8], mime: &str) -> String {
    let h = hash(bytes);
    exec(
        db,
        "INSERT INTO blob (hash, mime, bytes, width, height, data, time_created) VALUES (?, ?, ?, NULL, NULL, ?, 0)",
        rusqlite::params![h, mime, bytes.len() as i64, bytes],
    );
    h
}

fn insert_ref(db: &Database, session: &str, message: &str, hash: &str, seq: i64) {
    exec(db, "INSERT INTO blob_ref (session_id, message_id, hash, seq) VALUES (?, ?, ?, ?)", rusqlite::params![session, message, hash, seq]);
}

fn count(db: &Database, table: &str) -> i64 {
    db.with(|c| c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))).unwrap()
}

/// Rows written by 2.2.2 with `experimental.media_blob` on: a user file and a tool screenshot (in a part row)
/// held as `media:` refs, plus a ref whose blob is gone.
fn seed_media(db: &Database, sid: &str) -> String {
    let h = insert_blob(db, PNG, "image/png");
    let gone = "f".repeat(64);
    insert_row(db, sid, "msg_user", "user", 0, json!({
        "time": {"created": 0}, "text": "look",
        "files": [
            {"mime": "image/png", "source": {"type": "inline"}, "name": "shot.png", "uri": format!("media:{h}")},
            {"mime": "application/pdf", "source": {"type": "inline"}, "uri": format!("media:{gone}")}
        ]
    }));
    insert_ref(db, sid, "msg_user", &h, 0);
    insert_row(db, sid, "msg_tool", "assistant", 1, assistant_row(Some(1)));
    insert_part(db, sid, "msg_tool", 0, Some("call_s"), json!({
        "type": "tool", "id": "call_s", "name": "screenshot", "executed": true,
        "state": {"status": "completed", "input": {}, "metadata": {}, "content": [
            {"type": "file", "uri": format!("media:{h}"), "mime": "image/png"},
            {"type": "file", "uri": format!("media:{gone}"), "mime": "image/gif", "name": "anim.gif"}
        ]},
        "time": {"created": 1, "ran": 1, "completed": 1}
    }));
    insert_ref(db, sid, "msg_tool", &h, 1);
    h
}

#[test]
fn reads_hydrate_media_refs_and_name_missing_blobs() {
    let (db, sid) = setup();
    seed_media(&db, &sid);
    let b64 = base64::engine::general_purpose::STANDARD.encode(PNG);

    let user = message_json(&db, &sid, "msg_user");
    assert_eq!(user["files"][0]["data"], json!(b64));
    assert_eq!(user["files"][0]["mime"], "image/png");
    assert!(user["files"][0].get("uri").is_none());
    let missing = base64::engine::general_purpose::STANDARD.decode(user["files"][1]["data"].as_str().unwrap()).unwrap();
    assert_eq!(String::from_utf8(missing).unwrap(), "[media unavailable: application/pdf]");
    assert_eq!(user["files"][1]["mime"], "text/plain");

    let tool = message_json(&db, &sid, "msg_tool");
    let items = &tool["content"][0]["state"]["content"];
    assert_eq!(items[0], json!({"type": "file", "uri": format!("data:image/png;base64,{b64}"), "mime": "image/png"}));
    assert_eq!(items[1], json!({"type": "text", "text": "[media unavailable: anim.gif (image/gif)]"}));
}

#[test]
fn inbox_payloads_hydrate_media_refs() {
    let (db, sid) = setup();
    let h = insert_blob(&db, PNG, "image/png");
    let inbox = id::message();
    commit(
        &db,
        SessionEvent::InboxEnqueued {
            session_id: sid.clone(),
            inbox_id: inbox.clone(),
            item: InboxItem {
                kind: "user".into(),
                payload: json!({"text": "hi", "files": [{"mime": "image/png", "source": {"type": "inline"}, "uri": format!("media:{h}")}]}),
                delivery: Delivery::Queue,
            },
        },
        5,
    );
    insert_ref(&db, &sid, &inbox, &h, 1);
    let rows = db.session_inbox(&sid).unwrap();
    assert_eq!(rows[0].2["files"][0]["data"], json!(base64::engine::general_purpose::STANDARD.encode(PNG)));

    // cancelling the inbox item releases its blob
    commit(&db, SessionEvent::InboxCancelled { session_id: sid.clone(), inbox_id: inbox.clone() }, 6);
    assert_eq!(count(&db, "blob_ref"), 0);
    assert_eq!(count(&db, "blob"), 0);
}

#[test]
fn media_blobs_are_served_only_while_referenced_and_released_with_their_owners() {
    let (db, sid) = setup();
    let h = seed_media(&db, &sid);
    let read = |h: &str| db.with(|c| oc_storage::stored::read_blob(c, h)).unwrap();
    assert_eq!(read(&h), Some(("image/png".to_string(), PNG.to_vec())));
    assert_eq!(read(&"0".repeat(64)), None);

    // a revert that removes only the tool message keeps the blob the user message still names
    commit(&db, SessionEvent::RevertCommitted { session_id: sid.clone(), to: "msg_tool".into() }, 7);
    assert_eq!(count(&db, "blob_ref"), 1);
    assert_eq!(read(&h).map(|b| b.0), Some("image/png".into()));

    // an unreferenced blob is not served
    let orphan = insert_blob(&db, b"orphan", "image/png");
    assert_eq!(read(&orphan), None);

    // deleting the session releases its refs and every blob nothing names anymore
    commit(&db, SessionEvent::Deleted { session_id: sid.clone() }, 8);
    assert_eq!(count(&db, "blob_ref"), 0);
    let left: Vec<String> = db
        .with(|c| {
            let mut s = c.prepare("SELECT hash FROM blob")?;
            let r = s.query_map([], |r| r.get(0))?.collect();
            r
        })
        .unwrap();
    assert_eq!(left, vec![orphan], "unreferenced blobs await their sweep; released ones are deleted");
}

#[test]
fn forks_record_refs_for_copied_media() {
    let (db, sid) = setup();
    let h = seed_media(&db, &sid);
    let child = id::session();
    commit(
        &db,
        SessionEvent::Forked {
            session_id: child.clone(),
            parent_id: sid.clone(),
            boundary: ForkBoundary { kind: ForkBoundaryType::Through, message_id: "msg_tool".into() },
            instructions: None,
            instruction_entries: None,
        },
        10,
    );
    let refs: Vec<(String, String)> = db
        .with(|c| {
            let mut s = c.prepare("SELECT message_id, hash FROM blob_ref WHERE session_id = ? ORDER BY seq")?;
            let r = s.query_map([&child], |r| Ok((r.get(0)?, r.get(1)?)))?.collect();
            r
        })
        .unwrap();
    assert_eq!(refs.len(), 2);
    assert!(refs.iter().all(|(_, hash)| *hash == h));
    // the child reads the same with the parent gone
    commit(&db, SessionEvent::Deleted { session_id: sid.clone() }, 11);
    let msgs = db.session_messages(&child).unwrap();
    let user = msgs.iter().find(|(_, m)| matches!(m, Message::User { .. })).unwrap();
    let v = serde_json::to_value(&user.1).unwrap();
    assert_eq!(v["files"][0]["data"], json!(base64::engine::general_purpose::STANDARD.encode(PNG)));
}
