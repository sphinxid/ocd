//! v2.1 native data over HTTP (`server/test/session-import.test.ts`, `core/test/session-compaction.test.ts`):
//! imports and stored rows written before the rename read in the current shape.

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn upgrades_native_data_in_transcripts_exported_before_v2_1() {
    let h = harness();
    let template = h.post("/api/session", json!({})).await.json()["data"].clone();
    let id = oc_schema::id::session();
    let mut info = template.clone();
    info["id"] = json!(id);
    let r = h
        .post(
            "/api/experimental/session/import",
            json!({
                "info": info,
                "messages": [
                    {
                        "id": "msg_legacy",
                        "type": "assistant",
                        "agent": "build",
                        "model": { "providerID": "anthropic", "id": "claude" },
                        "providerState": { "finish": "kept" },
                        "content": [
                            { "type": "text", "text": "Answer", "state": { "signature": "text" } },
                            { "type": "reasoning", "text": "Thinking", "state": { "signature": "reasoning" } },
                            {
                                "type": "tool",
                                "id": "call",
                                "name": "web_search",
                                "executed": true,
                                "providerState": { "call": "kept" },
                                "providerResultState": { "result": "kept" },
                                "state": { "status": "completed", "input": {}, "content": [{ "type": "text", "text": "Found" }] },
                                "time": { "created": 1, "ran": 1, "completed": 2 },
                            },
                        ],
                        "time": { "created": 1, "completed": 2 },
                    },
                    {
                        "id": "msg_compaction",
                        "type": "compaction",
                        "status": "completed",
                        "reason": "manual",
                        "summary": "Summary",
                        "recent": "[User]: kept",
                        "providerState": { "compaction": "kept" },
                        "time": { "created": 3 },
                    },
                    { "id": "msg_moved", "type": "location-switched", "location": { "directory": "/moved" }, "time": { "created": 4 } },
                ],
            }),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let data = h.get(&format!("/api/session/{id}/message?order=asc")).await.json()["data"].clone();
    assert_eq!(data[0]["id"], "msg_legacy");
    assert_eq!(data[0]["native"], json!({ "finish": "kept" }));
    assert!(data[0].get("providerState").is_none());
    assert_eq!(data[0]["content"][0]["native"], json!({ "signature": "text" }));
    assert_eq!(data[0]["content"][1]["native"], json!({ "signature": "reasoning" }));
    assert_eq!(data[0]["content"][2]["native"], json!({ "call": "kept", "result": "kept" }));
    assert_eq!(data[1]["id"], "msg_compaction");
    assert_eq!(data[1]["native"], json!({ "compaction": "kept" }));
    assert_eq!(data[1]["metadata"], json!({ "recent": "[User]: kept" }));
    assert!(data[1].get("recent").is_none());
    assert_eq!(data[2]["location"]["directory"], "/moved");
}

#[tokio::test]
async fn rejects_an_invalid_imported_message_without_creating_the_session() {
    let h = harness();
    let mut info = h.post("/api/session", json!({})).await.json()["data"].clone();
    let id = oc_schema::id::session();
    info["id"] = json!(id);
    let r = h
        .post("/api/experimental/session/import", json!({ "info": info, "messages": [{ "id": "msg_invalid", "type": "assistant" }] }))
        .await;
    assert_eq!(r.status, 400, "{:?}", String::from_utf8_lossy(&r.body));
    let error = r.json();
    assert_eq!(error["_tag"], "InvalidRequestError");
    assert_eq!(error["kind"], "Payload");
    assert!(error["message"].as_str().unwrap().contains("[\"messages\"][0]"), "{error}");
    assert_eq!(h.get(&format!("/api/session/{id}")).await.status, 404);
}

#[tokio::test]
async fn stored_rows_of_either_shape_read_in_the_current_shape() {
    let h = harness();
    let id = h.session().await;
    let seq = h.app.db.session_seq(&id).unwrap();
    let insert = |mid: &str, kind: &str, seq: i64, data: serde_json::Value| {
        h.app
            .db
            .with(|c| {
                c.execute(
                    "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?, ?, ?, ?, 0, 0, ?)",
                    rusqlite::params![mid, id, kind, seq, data.to_string()],
                )?;
                oc_storage::projector::reserve_seq(c, &id, seq)
            })
            .unwrap();
    };
    insert("msg_kept", "user", seq + 1, json!({ "time": { "created": 0 }, "text": "Kept question" }));
    // Written by v2.1: the summary references the first message it kept.
    insert(
        "msg_summary",
        "compaction",
        seq + 2,
        json!({ "metadata": { "keepID": "msg_kept" }, "time": { "created": 0 }, "status": "completed", "reason": "auto", "summary": "s" }),
    );
    let context = h.get(&format!("/api/session/{id}/context")).await;
    assert_eq!(context.status, 200, "{:?}", String::from_utf8_lossy(&context.body));
    let context = context.json()["data"].clone();
    assert_eq!(context[0]["id"], "msg_summary");
    assert_eq!(context[0]["metadata"], json!({ "keepID": "msg_kept", "recent": "[User]: Kept question" }));

    // Written by 2.0.x: the kept text was stored in `recent`.
    insert(
        "msg_legacy",
        "compaction",
        seq + 3,
        json!({ "time": { "created": 0 }, "status": "completed", "reason": "auto", "summary": "s", "recent": "[User]: old" }),
    );
    let context = h.get(&format!("/api/session/{id}/context")).await.json()["data"].clone();
    assert_eq!(context[0]["id"], "msg_legacy");
    assert_eq!(context[0]["metadata"], json!({ "recent": "[User]: old" }));
    assert!(context[0].get("recent").is_none());
}
