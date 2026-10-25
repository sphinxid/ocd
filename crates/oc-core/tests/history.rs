//! Session history (`core/session/history.ts`), ported from `session-compaction.test.ts` and
//! `session-native.test.ts`: a summary's kept text is rebuilt from `metadata.keepID` (or read from what a
//! pre-v2.1 row stored), and a native window only replays on the model that made it.

use oc_ai::types::{ContentPart, Role};
use oc_core::history::{self, Boundary};
use oc_core::lower::to_llm_messages;
use oc_core::recent_text::kept;
use oc_schema::event::SessionEvent;
use oc_schema::message::CompactionReason;
use oc_schema::types::*;
use oc_schema::{id, Message};
use oc_storage::projector::Committing;
use oc_storage::Database;
use serde_json::{json, Value};

fn commit(db: &Database, e: SessionEvent) -> String {
    let eid = id::event();
    db.commit_session_event(Committing { id: &eid, created: 1, metadata: None, event: &e }).unwrap();
    eid
}

fn model() -> ModelRef {
    ModelRef { id: "gpt".into(), provider_id: "openai".into(), variant: None }
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
            location: LocationRef { directory: "/project".into(), workspace_id: None },
            subpath: None,
            parent_id: None,
            slug: "native".into(),
            title: None,
            agent: None,
            model: None,
            metadata: None,
            permissions: None,
            version: "test".into(),
        },
    );
    (db, sid)
}

/// Appends a row at the next sequence, as projection would.
fn store(db: &Database, session: &str, kind: &str, data: Value) -> String {
    let seq = db.session_seq(session).unwrap() + 1;
    let mid = id::message();
    db.with(|c| {
        c.execute(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?, ?, ?, ?, 0, 0, ?)",
            rusqlite::params![mid, session, kind, seq, data.to_string()],
        )?;
        oc_storage::projector::reserve_seq(c, session, seq)
    })
    .unwrap();
    mid
}

fn user(db: &Database, session: &str, text: &str) -> String {
    store(db, session, "user", json!({ "time": { "created": 0 }, "text": text }))
}

fn compact(db: &Database, session: &str, native: Option<Value>, by: ModelRef, keep: Option<&str>) {
    let metadata = keep.map(|k| json!({ "keepID": k }).as_object().unwrap().clone());
    let eid = id::event();
    let event = SessionEvent::CompactionEnded {
        session_id: session.into(),
        reason: CompactionReason::Manual,
        model: Some(by),
        text: if native.is_some() { String::new() } else { "local summary".into() },
        native: native.map(|n| n.as_object().unwrap().clone()),
        cost: None,
        tokens: None,
    };
    db.commit_session_event(Committing { id: &eid, created: 1, metadata: metadata.as_ref(), event: &event }).unwrap();
}

fn window() -> Value {
    json!({ "messages": [{ "role": "assistant", "content": [{ "type": "compaction", "provider": "openai", "encrypted": "opaque" }] }] })
}

fn texts(messages: &[Message]) -> Vec<String> {
    to_llm_messages(messages, &model(), None)
        .into_iter()
        .map(|m| {
            m.content
                .iter()
                .map(|p| match p {
                    ContentPart::Text(t) => t.text.clone(),
                    ContentPart::Compaction(c) => format!("<{}>", c.encrypted.clone().unwrap_or_default()),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .collect()
}

#[test]
fn summaries_written_before_kept_message_references_keep_their_stored_recent_text() {
    let (db, sid) = setup();
    user(&db, &sid, "Kept question");
    store(
        &db,
        &sid,
        "compaction",
        json!({
            "status": "completed",
            "reason": "auto",
            "summary": "## Objective\n- legacy",
            "recent": "[User]: Kept question as flattened then",
            "time": { "created": 0 },
        }),
    );
    let context = history::load(&db, &sid, Boundary::Latest).unwrap();
    assert_eq!(context.len(), 1);
    assert_eq!(kept(&context[0]), "[User]: Kept question as flattened then");
    let encoded = serde_json::to_value(&context[0]).unwrap();
    assert!(encoded.get("recent").is_none());
    assert_eq!(encoded["metadata"], json!({ "recent": "[User]: Kept question as flattened then" }));
}

#[test]
fn rebuilds_kept_text_from_keep_id_and_forks_rebuild_it_too() {
    let (db, sid) = setup();
    user(&db, &sid, "Earlier question");
    let keep = user(&db, &sid, "Kept question");
    compact(&db, &sid, None, model(), Some(&keep));
    let parent = history::load(&db, &sid, Boundary::Latest).unwrap();
    assert_eq!(parent.len(), 1);
    assert_eq!(kept(&parent[0]), "[User]: Kept question");
    // Only in memory: the stored row keeps the reference.
    let stored: String = db
        .with(|c| c.query_row("SELECT data FROM session_message WHERE type = 'compaction'", [], |r| r.get(0)))
        .unwrap();
    let stored: Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(stored["metadata"], json!({ "keepID": keep }));
    assert!(texts(&parent)[0].contains("<recent-context>\n[User]: Kept question\n</recent-context>"));

    let child = id::session();
    commit(
        &db,
        SessionEvent::Forked {
            session_id: child.clone(),
            parent_id: sid.clone(),
            boundary: ForkBoundary { kind: ForkBoundaryType::Through, message_id: parent[0].id().into() },
            instructions: None,
            instruction_entries: None,
        },
    );
    let forked = history::load(&db, &child, Boundary::Latest).unwrap();
    assert_eq!(kept(&forked[0]), "[User]: Kept question");
    let keep_of = |m: &Message| match m {
        Message::Compaction(c) => c.metadata.as_ref().and_then(|m| m.get("keepID")).cloned(),
        _ => None,
    };
    assert_ne!(keep_of(&forked[0]), keep_of(&parent[0]));
}

#[test]
fn ignores_non_message_keep_id_strings_when_loading_history() {
    let (db, sid) = setup();
    user(&db, &sid, "Earlier question");
    store(
        &db,
        &sid,
        "compaction",
        json!({ "metadata": { "keepID": "not-a-msg-id" }, "status": "completed", "reason": "manual", "summary": "## Objective\n- plugin summary", "time": { "created": 0 } }),
    );
    let context = history::load(&db, &sid, Boundary::Latest).unwrap();
    assert_eq!(kept(&context[0]), "");
    let Message::Compaction(c) = &context[0] else { panic!("compaction") };
    assert_eq!(c.metadata.as_ref().unwrap().get("keepID"), Some(&json!("not-a-msg-id")));
}

#[test]
fn a_native_window_replays_only_on_the_model_that_made_it() {
    let (db, sid) = setup();
    user(&db, &sid, "original request");
    compact(&db, &sid, Some(window()), model(), None);
    user(&db, &sid, "continue");

    let replayed = history::load(&db, &sid, Boundary::Model(&model())).unwrap();
    assert!(history::is_native_compaction(&replayed[0]));
    assert_eq!(texts(&replayed), vec!["<opaque>", "continue"]);
    let other = ModelRef { id: "other".into(), ..model() };
    let other_provider = ModelRef { provider_id: "other".into(), ..model() };
    for boundary in [Boundary::Local, Boundary::Model(&other), Boundary::Model(&other_provider)] {
        let expanded = history::load(&db, &sid, boundary).unwrap();
        assert!(!expanded.iter().any(history::is_native_compaction));
        assert_eq!(texts(&expanded), vec!["original request", "continue"]);
    }
    // Model-neutral readers start at any compaction.
    let latest = history::load(&db, &sid, Boundary::Latest).unwrap();
    assert_eq!(latest.len(), 2);
}

#[test]
fn falls_back_to_an_earlier_compatible_native_or_local_checkpoint() {
    let (db, sid) = setup();
    user(&db, &sid, "before local");
    compact(&db, &sid, None, model(), None);
    user(&db, &sid, "after local");
    compact(&db, &sid, Some(window()), model(), None);
    user(&db, &sid, "after native");
    compact(&db, &sid, Some(window()), ModelRef { id: "other".into(), ..model() }, None);

    let replayed = history::load(&db, &sid, Boundary::Model(&model())).unwrap();
    assert_eq!(replayed.iter().map(Message::kind).collect::<Vec<_>>(), vec!["compaction", "user"]);
    assert!(history::is_native_compaction(&replayed[0]));
    let local = history::load(&db, &sid, Boundary::Local).unwrap();
    assert_eq!(local.iter().map(Message::kind).collect::<Vec<_>>(), vec!["compaction", "user", "user"]);
    assert!(!history::is_native_compaction(&local[0]));
}

#[test]
fn skips_a_malformed_native_window_and_replays_the_history_before_it() {
    let (db, sid) = setup();
    user(&db, &sid, "before");
    compact(&db, &sid, Some(json!({ "messages": [{ "role": "invalid", "content": [] }] })), model(), None);
    user(&db, &sid, "after");
    let history = history::load(&db, &sid, Boundary::Model(&model())).unwrap();
    assert_eq!(texts(&history), vec!["before", "after"]);
    let context = history::load(&db, &sid, Boundary::Latest).unwrap();
    assert_eq!(context.iter().map(Message::kind).collect::<Vec<_>>(), vec!["compaction", "user"]);
}

#[test]
fn loads_and_replays_a_pre_v2_1_provider_context_checkpoint_with_pre_2_0_15_media() {
    let (db, sid) = setup();
    user(&db, &sid, "original request");
    store(
        &db,
        &sid,
        "compaction",
        json!({
            "status": "completed",
            "reason": "manual",
            "model": { "providerID": "openai", "id": "gpt" },
            "summary": "",
            "recent": "",
            "providerContext": {
                "version": 1,
                "provenance": { "providerID": "openai" },
                "messages": [{ "role": "user", "content": [{ "type": "media", "mediaType": "image/png", "data": "AAAA" }] }],
            },
            "time": { "created": 0 },
        }),
    );
    user(&db, &sid, "continue");
    let history = history::load(&db, &sid, Boundary::Model(&model())).unwrap();
    assert!(history.iter().any(history::is_native_compaction));
    let lowered = to_llm_messages(&history, &model(), None);
    assert_eq!(lowered[0].role, Role::User);
    assert!(matches!(&lowered[0].content[0], ContentPart::Media(_)));
    let local = history::load(&db, &sid, Boundary::Local).unwrap();
    assert!(!local.iter().any(history::is_native_compaction));
    assert_eq!(texts(&local), vec!["original request", "continue"]);
}

#[test]
fn system_messages_before_a_native_checkpoint_are_superseded() {
    let (db, sid) = setup();
    user(&db, &sid, "original request");
    store(&db, &sid, "system", json!({ "time": { "created": 0 }, "text": "changed instructions" }));
    compact(&db, &sid, Some(window()), model(), None);
    store(&db, &sid, "system", json!({ "time": { "created": 0 }, "text": "newest instructions" }));
    user(&db, &sid, "continue");
    let replayed = history::load(&db, &sid, Boundary::Model(&model())).unwrap();
    assert_eq!(replayed.iter().map(Message::kind).collect::<Vec<_>>(), vec!["compaction", "system", "user"]);
    let expanded = history::load(&db, &sid, Boundary::Local).unwrap();
    assert_eq!(expanded.iter().map(Message::kind).collect::<Vec<_>>(), vec!["user", "system", "user"]);
}
