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
    let seq = db.commit_session_event(Committing { id: &eid, created, metadata: None, event: &e, media: false }).unwrap();
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
fn a_database_migrated_from_2_1_4_matches_a_fresh_database() {
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
    assert_eq!(ids(&migrated).len(), 59);
    assert_eq!(ids(&migrated), ids(&fresh));
    assert_eq!(ids(&fresh).last().unwrap(), "20261030054400_job");

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

/// The schema dump `tests/fixtures/schema-2.2.5.txt` holds, taken from a 2.1.4 database that the real
/// opencode 2.2.5 binary migrated: migration ids, every table's columns and foreign keys, every index.
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

/// A 2.1.4 database migrated by ocd, and a fresh one, have exactly the schema the real 2.2.5 binary gives a
/// migrated 2.1.4 database.
#[test]
fn schema_matches_the_one_the_2_2_5_binary_migrates_to() {
    let expected = include_str!("fixtures/schema-2.2.5.txt");
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
    // 2.2.7: the child gets its own part rows and an empty envelope, like the parent
    let forked = msgs[0].1.id().to_string();
    assert_eq!(row_data(&db, &forked)["content"], json!([]));
    let parts: Vec<(String, String, i64, String, i64, i64)> = db
        .with(|c| {
            let mut s = c.prepare(
                "SELECT message_id, session_id, ordinal, type, time_created, time_updated FROM session_part ORDER BY session_id = ?, ordinal",
            )?;
            let rows = s.query_map([&child], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
            rows.collect()
        })
        .unwrap();
    assert_eq!(
        parts,
        [
            (mid.to_string(), sid.clone(), 0, "text".to_string(), 1, 1),
            (forked, child.clone(), 0, "text".to_string(), 1, 1),
        ]
    );
}

/// `session-projector.test.ts` "reads a split message an older build added inline content to as its parts then
/// that content" (2.2.7): reads put the parts first, and an update writes the whole content into part rows.
#[test]
fn reads_a_split_message_with_inline_content_as_its_parts_then_that_content() {
    let (db, sid) = setup();
    let mid = "msg_hybrid_read";
    let mut row = assistant_row(None);
    row["content"] = json!([
        {"type": "text", "text": "inline after"},
        {"type": "tool", "id": "call_inline", "name": "read", "executed": true,
         "state": {"status": "running", "input": {"path": "a.ts"}, "metadata": {}}, "time": {"created": 1, "ran": 1}}
    ]);
    insert_row(&db, &sid, mid, "assistant", 0, row);
    insert_part(&db, &sid, mid, 0, None, json!({"type": "text", "text": "split first"}));
    let content = message_json(&db, &sid, mid)["content"].clone();
    assert_eq!(content[0]["text"], "split first");
    assert_eq!(content[1]["text"], "inline after");
    assert_eq!(content[2]["id"], "call_inline");
    assert_eq!(content[2]["state"]["status"], "running");

    commit(
        &db,
        SessionEvent::ToolFailed {
            session_id: sid.clone(),
            assistant_message_id: mid.into(),
            id: "call_inline".into(),
            error: serde_json::from_value(json!({"type": "aborted", "message": "Tool execution interrupted: read"})).unwrap(),
            content: None,
            metadata: None,
            executed: true,
            native: None,
        },
        5,
    );
    let content = message_json(&db, &sid, mid)["content"].clone();
    assert_eq!(content[0]["text"], "split first");
    assert_eq!(content[1]["text"], "inline after");
    assert_eq!(content[2]["state"]["status"], "error");
    assert_eq!(row_data(&db, mid)["content"], json!([]));
    let n: i64 = db.with(|c| c.query_row("SELECT count(*) FROM session_part WHERE message_id = ?", [mid], |r| r.get(0))).unwrap();
    assert_eq!(n, 3);
}

/// "forks a split message an older build added inline media to" (2.2.7): the inline media moves to a blob the
/// forked message refs, and the fork reads like its parent.
#[test]
fn forks_a_split_message_with_inline_media() {
    let (db, sid) = setup();
    let mid = "msg_hybrid_fork";
    let shot = format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(PNG));
    let mut row = assistant_row(Some(1));
    row["content"] = json!([{"type": "tool", "id": "call_inline_shot", "name": "shot", "executed": true,
        "state": {"status": "completed", "input": {}, "content": [{"type": "file", "uri": shot, "mime": "image/png"}]},
        "time": {"created": 1, "ran": 1, "completed": 1}}]);
    insert_row(&db, &sid, mid, "assistant", 0, row);
    insert_part(&db, &sid, mid, 0, None, json!({"type": "text", "text": "split first"}));
    let parent = message_json(&db, &sid, mid);
    assert_eq!(parent["content"][0]["text"], "split first");
    assert_eq!(parent["content"][1]["id"], "call_inline_shot");

    let child = id::session();
    commit_media(
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
    let forked = msgs[0].1.id().to_string();
    let raw: String = db
        .with(|c| {
            c.query_row(
                "SELECT group_concat(data) FROM (SELECT data FROM session_message WHERE session_id = ?1 UNION ALL SELECT data FROM session_part WHERE session_id = ?1)",
                [&child],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert!(!raw.contains(&base64::engine::general_purpose::STANDARD.encode(PNG)));
    assert_eq!(refs(&db).into_iter().filter(|r| r.0 == child).map(|r| (r.1, r.2)).collect::<Vec<_>>(), [(forked.clone(), hash(PNG))]);
    let mut copy = serde_json::to_value(&msgs[0].1).unwrap();
    copy["id"] = json!(mid);
    assert_eq!(copy, parent);
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
    // The projection records the ref its payload names (`recordRefs` at admission).
    assert_eq!(count(&db, "blob_ref"), 1);
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

/// `20261030054400_job` (`job.test.ts` "migrates legacy kv job.background/* markers into the job table"): background
/// job markers move from `kv` into `job`, owned by the session or the subagent's parent, titled by the command,
/// description or path; a marker without an owner is dropped, and every `job.background/*` key is deleted.
#[test]
fn migrates_legacy_kv_job_markers_into_the_job_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("opencode.db");
    {
        let db = Database::open(&path, dir.path()).unwrap();
        db.with(|c| {
            c.execute_batch("DROP TABLE `job`; DELETE FROM migration WHERE id = '20261030054400_job';")?;
            let rows = [
                ("job.background/msg_b", json!({"id": "shell_legacy", "notificationID": "msg_b", "recovery": {"kind": "shell", "sessionID": "ses_migrated", "shellID": "shell_legacy", "command": "bun test"}, "status": "completed", "output": "all green"}), 20),
                ("job.background/msg_a", json!({"id": "ses_child", "notificationID": "msg_a", "recovery": {"kind": "subagent", "parentSessionID": "ses_parent", "childSessionID": "ses_child", "agent": "general", "description": "look around"}, "status": "running"}), 10),
                ("job.background/msg_c", json!({"id": "img_1", "notificationID": "msg_c", "recovery": {"kind": "image", "sessionID": "ses_img", "path": "/tmp/a.png", "model": "m"}, "status": "error", "error": "boom"}), 30),
                ("job.background/msg_d", json!({"id": "orphan", "notificationID": "msg_d", "recovery": {"kind": "shell", "shellID": "s", "command": "c"}, "status": "running"}), 40),
                ("other/key", json!({"keep": true}), 1),
            ];
            for (key, value, time) in rows {
                c.execute(
                    "INSERT INTO kv (key, value, time_created, time_updated) VALUES (?, ?, ?, ?)",
                    rusqlite::params![key, value.to_string(), time, time + 1],
                )?;
            }
            Ok(())
        })
        .unwrap();
    }
    let db = Database::open(&path, dir.path()).unwrap();
    type Row = (String, String, String, Option<String>, String, Option<String>, String, Option<String>, Option<String>, String, i64, i64);
    let rows: Vec<Row> = db
        .with(|c| {
            let mut s = c.prepare("SELECT * FROM job ORDER BY time_created, notification_id")?;
            let r = s
                .query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?))
                })?
                .collect();
            r
        })
        .unwrap();
    let summary: Vec<(&str, &str, &str, Option<&str>, &str, Option<&str>, &str, i64, i64)> = rows
        .iter()
        .map(|r| (r.0.as_str(), r.1.as_str(), r.2.as_str(), r.3.as_deref(), r.4.as_str(), r.5.as_deref(), r.6.as_str(), r.10, r.11))
        .collect();
    assert_eq!(
        summary,
        [
            ("msg_a", "ses_child", "ses_parent", Some("ses_child"), "subagent", Some("look around"), "running", 10, 11),
            ("msg_b", "shell_legacy", "ses_migrated", None, "shell", Some("bun test"), "completed", 20, 21),
            ("msg_c", "img_1", "ses_img", None, "image", Some("/tmp/a.png"), "error", 30, 31),
        ]
    );
    assert_eq!(rows[1].7.as_deref(), Some("all green"));
    assert_eq!(rows[2].8.as_deref(), Some("boom"));
    assert_eq!(
        serde_json::from_str::<Value>(&rows[1].9).unwrap(),
        json!({"kind": "shell", "sessionID": "ses_migrated", "shellID": "shell_legacy", "command": "bun test"})
    );
    let keys: Vec<String> = db
        .with(|c| {
            let mut s = c.prepare("SELECT key FROM kv ORDER BY key")?;
            let r = s.query_map([], |r| r.get(0))?.collect();
            r
        })
        .unwrap();
    assert_eq!(keys, ["other/key"]);
}

// ---- media blob writes (`experimental.media_blob`, `core/test/session-projector.test.ts`) ----------------------

fn commit_media(db: &Database, e: SessionEvent, created: u64) -> i64 {
    let eid = id::event();
    db.commit_session_event(Committing { id: &eid, created, metadata: None, event: &e, media: true }).unwrap()
}

fn inline_file(name: &str) -> Value {
    json!({"data": base64::engine::general_purpose::STANDARD.encode(PNG), "mime": "image/png", "source": {"type": "inline"}, "name": name})
}

fn inbox_payload(db: &Database, id: &str) -> Value {
    let raw: String = db.with(|c| c.query_row("SELECT payload FROM session_inbox WHERE id = ?", [id], |r| r.get(0))).unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn refs(db: &Database) -> Vec<(String, String, String, i64)> {
    db.with(|c| {
        let mut s = c.prepare("SELECT session_id, message_id, hash, seq FROM blob_ref ORDER BY seq, message_id")?;
        let r = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect();
        r
    })
    .unwrap()
}

/// "stores media once and refs it from inbox, message, and tool rows when experimental.media_blob is on"
#[test]
fn stores_media_once_and_refs_it_from_inbox_message_and_tool_rows() {
    let (db, sid) = setup();
    let h = hash(PNG);
    let uri = format!("media:{h}");
    let reference = |name: &str| json!({"mime": "image/png", "source": {"type": "inline"}, "name": name, "uri": uri});
    let inbox = "msg_media_user".to_string();
    commit_media(
        &db,
        SessionEvent::InboxEnqueued {
            session_id: sid.clone(),
            inbox_id: inbox.clone(),
            item: InboxItem {
                kind: "user".into(),
                payload: json!({"text": "look", "files": [inline_file("shot.png"), inline_file("copy.png")]}),
                delivery: Delivery::Steer,
            },
        },
        1,
    );
    assert_eq!(inbox_payload(&db, &inbox), json!({"text": "look", "files": [reference("shot.png"), reference("copy.png")]}));
    // Reads restore the bytes.
    let rows = db.session_inbox(&sid).unwrap();
    assert_eq!(rows[0].2["files"][1], inline_file("copy.png"));

    let delivered = commit_media(&db, SessionEvent::InboxDelivered { session_id: sid.clone(), inbox_id: inbox.clone() }, 2);
    assert_eq!(row_data(&db, &inbox)["files"], json!([reference("shot.png"), reference("copy.png")]));
    assert_eq!(message_json(&db, &sid, &inbox)["files"][0], inline_file("shot.png"));

    // A tool result's `data:` file.
    let assistant = "msg_media_tool";
    insert_row(&db, &sid, assistant, "assistant", 0, assistant_row(None));
    let b64 = base64::engine::general_purpose::STANDARD.encode(PNG);
    let events = [
        SessionEvent::ToolInputStarted { session_id: sid.clone(), assistant_message_id: assistant.into(), id: "shot".into(), name: "shot".into() },
        SessionEvent::ToolCalled {
            session_id: sid.clone(),
            assistant_message_id: assistant.into(),
            id: "shot".into(),
            input: Default::default(),
            executed: true,
            native: None,
        },
        SessionEvent::ToolSuccess {
            session_id: sid.clone(),
            assistant_message_id: assistant.into(),
            id: "shot".into(),
            content: vec![ToolContent::File { uri: format!("data:image/png;base64,{b64}"), mime: "image/png".into(), name: None }],
            metadata: None,
            executed: true,
            native: None,
        },
    ];
    for (i, e) in events.into_iter().enumerate() {
        commit_media(&db, e, 3 + i as u64);
    }
    assert_eq!(row_data(&db, assistant)["content"][0]["state"]["content"], json!([{"type": "file", "uri": uri, "mime": "image/png"}]));
    assert_eq!(
        message_json(&db, &sid, assistant)["content"][0]["state"]["content"][0]["uri"],
        json!(format!("data:image/png;base64,{b64}"))
    );

    let blobs: Vec<(String, String, i64)> = db
        .with(|c| {
            let mut s = c.prepare("SELECT hash, mime, bytes FROM blob")?;
            let r = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect();
            r
        })
        .unwrap();
    assert_eq!(blobs, [(h.clone(), "image/png".to_string(), PNG.len() as i64)]);
    assert_eq!(refs(&db), [(sid.clone(), assistant.to_string(), h.clone(), 0), (sid.clone(), inbox.clone(), h.clone(), delivered)]);
}

/// "keeps media inline unless experimental.media_blob is on"
#[test]
fn keeps_media_inline_unless_media_blob_is_on() {
    let (db, sid) = setup();
    commit(
        &db,
        SessionEvent::InboxEnqueued {
            session_id: sid.clone(),
            inbox_id: "msg_media_inline".into(),
            item: InboxItem { kind: "user".into(), payload: json!({"text": "look", "files": [inline_file("shot.png")]}), delivery: Delivery::Steer },
        },
        1,
    );
    commit(&db, SessionEvent::InboxDelivered { session_id: sid.clone(), inbox_id: "msg_media_inline".into() }, 2);
    assert_eq!(row_data(&db, "msg_media_inline")["files"], json!([inline_file("shot.png")]));
    assert_eq!(count(&db, "blob"), 0);
    assert_eq!(count(&db, "blob_ref"), 0);
}

/// "deletes a cancelled inbox row's blob refs and blobs"
#[test]
fn a_cancelled_inbox_row_releases_the_blobs_it_stored() {
    let (db, sid) = setup();
    commit_media(
        &db,
        SessionEvent::InboxEnqueued {
            session_id: sid.clone(),
            inbox_id: "msg_media_cancel".into(),
            item: InboxItem { kind: "user".into(), payload: json!({"text": "later", "files": [inline_file("shot.png")]}), delivery: Delivery::Queue },
        },
        1,
    );
    assert_eq!((count(&db, "blob_ref"), count(&db, "blob")), (1, 1));
    commit(&db, SessionEvent::InboxCancelled { session_id: sid.clone(), inbox_id: "msg_media_cancel".into() }, 2);
    assert_eq!((count(&db, "blob_ref"), count(&db, "blob")), (0, 0));
}

/// "forks a session's inline media into blobs when its location enables media_blob" (`ForkMedia`); without it the
/// copy keeps media inline.
#[test]
fn forks_store_inline_media_as_blobs_only_with_fork_media() {
    for media in [true, false] {
        let (db, sid) = setup();
        insert_row(&db, &sid, "msg_parent", "user", 1, json!({"time": {"created": 0}, "text": "look", "files": [inline_file("shot.png")]}));
        let child = id::session();
        let eid = id::event();
        let e = SessionEvent::Forked {
            session_id: child.clone(),
            parent_id: sid.clone(),
            boundary: ForkBoundary { kind: ForkBoundaryType::Through, message_id: "msg_parent".into() },
            instructions: None,
            instruction_entries: None,
        };
        db.commit_session_event(Committing { id: &eid, created: 3, metadata: None, event: &e, media }).unwrap();
        let copy: (String, i64, String) = db
            .with(|c| {
                c.query_row("SELECT id, seq, data FROM session_message WHERE session_id = ?", [&child], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
            })
            .unwrap();
        let data: Value = serde_json::from_str(&copy.2).unwrap();
        // The parent's row is never rewritten.
        assert_eq!(row_data(&db, "msg_parent")["files"], json!([inline_file("shot.png")]));
        if media {
            let h = hash(PNG);
            assert_eq!(data["files"], json!([{"mime": "image/png", "source": {"type": "inline"}, "name": "shot.png", "uri": format!("media:{h}")}]));
            assert_eq!(refs(&db), [(child.clone(), copy.0.clone(), h, 1)]);
        } else {
            assert_eq!(data["files"], json!([inline_file("shot.png")]));
            assert_eq!(count(&db, "blob"), 0);
        }
    }
}

/// `keepRefs` / `describe`: refs stay with their blob's size; refs whose blob is gone or unreferenced read as the
/// placeholder.
#[test]
fn reads_with_refs_keep_referenced_blobs_and_describe_them() {
    let (db, sid) = setup();
    let h = seed_media(&db, &sid);
    let orphan = insert_blob(&db, b"abc", "image/png");
    insert_row(
        &db,
        &sid,
        "msg_orphan",
        "user",
        5,
        json!({"time": {"created": 0}, "text": "o", "files": [{"uri": format!("media:{orphan}"), "mime": "image/png", "name": "gone.png", "source": {"type": "inline"}}]}),
    );
    let mut rows: Vec<(i64, String, String, String)> = db
        .with(|c| {
            let mut s = c.prepare("SELECT seq, id, type, data FROM session_message WHERE session_id = ? ORDER BY seq")?;
            let r = s.query_map([&sid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect();
            r
        })
        .unwrap();
    let meta = db.with(|c| oc_storage::stored::keep_refs(c, &mut rows)).unwrap();
    let user: Value = serde_json::from_str(&rows[0].3).unwrap();
    assert_eq!(user["files"][0]["uri"], json!(format!("media:{h}")));
    let placeholder = |text: &str| base64::engine::general_purpose::STANDARD.encode(text);
    assert_eq!(user["files"][1]["data"], json!(placeholder("[media unavailable: application/pdf]")));
    let tool: Value = serde_json::from_str(&rows[1].3).unwrap();
    assert_eq!(tool["content"][0]["state"]["content"][0]["uri"], json!(format!("media:{h}")));
    assert_eq!(tool["content"][0]["state"]["content"][1], json!({"type": "text", "text": "[media unavailable: anim.gif (image/gif)]"}));
    let orphaned: Value = serde_json::from_str(&rows[2].3).unwrap();
    assert_eq!(orphaned["files"][0]["data"], json!(placeholder("[media unavailable: gone.png (image/png)]")));
    assert_eq!(orphaned["files"][0]["mime"], "text/plain");

    let mut files = user["files"].as_array().unwrap().clone();
    oc_storage::stored::describe(&mut files, &meta);
    assert_eq!(
        serde_json::to_string(&files[0]).unwrap(),
        format!("{{\"mime\":\"image/png\",\"source\":{{\"type\":\"inline\"}},\"name\":\"shot.png\",\"uri\":\"media:{h}\",\"bytes\":{}}}", PNG.len())
    );
}

/// `split` only moves media a row carries inline (an import's), and the ref files it keeps name no new blob.
#[test]
fn split_moves_only_inline_media() {
    let d = "d".repeat(64);
    let mut user = json!({"text": "x", "files": [inline_file("a.png"), {"mime": "image/png", "source": {"type": "inline"}, "uri": format!("media:{d}")}]});
    let taken = oc_storage::stored::split(&mut user);
    assert_eq!(taken.hashes().into_iter().collect::<Vec<_>>(), [hash(PNG)]);
    assert_eq!(user["files"][0]["uri"], json!(format!("media:{}", hash(PNG))));
    let mut refs_only = json!({"text": "x", "files": [{"mime": "image/png", "source": {"type": "inline"}, "uri": format!("media:{d}")}]});
    assert!(oc_storage::stored::split(&mut refs_only).is_empty());
}

/// `database-migration.test.ts` "imports legacy credentials when bootstrapping a fresh database" (2.2.7, #62568).
#[test]
fn a_fresh_database_imports_legacy_credentials() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("auth.json"),
        json!({"openai": {"type": "oauth", "refresh": "refresh", "access": "access", "expires": 123}}).to_string(),
    )
    .unwrap();
    let db = Database::open(&dir.path().join("opencode.db"), dir.path()).unwrap();
    let rows: Vec<(String, String)> = db
        .with(|c| {
            let mut stmt = c.prepare("SELECT integration_id, label FROM credential")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect()
        })
        .unwrap();
    assert_eq!(rows, [("openai".to_string(), "OAuth".to_string())]);
    let n: i64 = db.with(|c| c.query_row("SELECT count(*) FROM migration", [], |r| r.get(0))).unwrap();
    assert_eq!(n, 59);
}

/// "bootstraps a fresh database when the legacy credential file is malformed": a file that is not a JSON object
/// is skipped with a warning, on a fresh database and when the import migration runs on an older one.
#[test]
fn a_malformed_legacy_credential_file_is_skipped() {
    for content in ["not json", "[]", "null", "\"text\""] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("auth.json"), content).unwrap();
        let db = Database::open(&dir.path().join("opencode.db"), dir.path()).unwrap();
        let n: i64 = db.with(|c| c.query_row("SELECT count(*) FROM credential", [], |r| r.get(0))).unwrap();
        assert_eq!(n, 0, "{content}");

        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(oc_storage::migrate::SCHEMA).unwrap();
        oc_storage::migrate::import_legacy_credentials(&c, &dir.path().join("auth.json")).unwrap();
    }
}
