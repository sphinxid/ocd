//! Session archiving (`core/test/session-archive.test.ts`, `server/test/session-update.test.ts`) and the `idle`
//! message filter (`server/test/message.test.ts`).

mod common;

use base64::Engine;
use common::*;
use serde_json::{Value, json};

/// Every event published so far on `events`.
fn drain(
    events: &mut tokio::sync::broadcast::Receiver<oc_core::bus::Event>,
) -> Vec<oc_core::bus::Event> {
    let mut out = vec![];
    while let Ok(e) = events.try_recv() {
        out.push(e);
    }
    out
}

fn ids(page: &Value) -> Vec<String> {
    let mut ids: Vec<String> = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn archives_and_unarchives_a_session_through_patch() {
    let h = harness();
    let id = h.session().await;
    async fn get(h: &Harness, id: &str) -> Value {
        h.get(&format!("/api/session/{id}")).await.json()["data"].clone()
    }
    async fn listed(h: &Harness, archived: bool) -> Vec<String> {
        ids(&h
            .get(&format!("/api/session?archived={archived}"))
            .await
            .json())
    }

    let r = h
        .req(
            "PATCH",
            &format!("/api/session/{id}"),
            Some(json!({"archived": true})),
        )
        .await;
    assert_eq!(r.status, 204, "{:?}", String::from_utf8_lossy(&r.body));
    assert!(get(&h, &id).await["time"]["archived"].is_number());
    assert!(listed(&h, true).await.contains(&id));
    assert!(!listed(&h, false).await.contains(&id));

    let r = h
        .req(
            "PATCH",
            &format!("/api/session/{id}"),
            Some(json!({"archived": false})),
        )
        .await;
    assert_eq!(r.status, 204);
    assert!(get(&h, &id).await["time"].get("archived").is_none());
    assert!(listed(&h, false).await.contains(&id));
}

#[tokio::test]
async fn archives_and_unarchives_through_one_durable_event_each() {
    let h = harness();
    let id = h.session().await;
    let mut events = h.app.bus.subscribe();
    let before = h.app.sessions.get(&id).unwrap().time.updated;
    assert!(h.app.sessions.get(&id).unwrap().time.archived.is_none());

    h.app.sessions.archive(&id).unwrap();
    h.app.sessions.archive(&id).unwrap();
    let archived = h.app.sessions.get(&id).unwrap();
    assert!(archived.time.archived.is_some());
    assert_eq!(archived.time.updated, archived.time.archived.unwrap());
    assert!(archived.time.updated >= before);

    h.app.sessions.unarchive(&id).unwrap();
    h.app.sessions.unarchive(&id).unwrap();
    assert!(h.app.sessions.get(&id).unwrap().time.archived.is_none());

    let published = drain(&mut events);
    let kinds: Vec<&str> = published.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, vec!["session.archived", "session.unarchived"]);
    for e in &published {
        assert_eq!(e.data, json!({"sessionID": id}));
        assert_eq!(e.durable.as_ref().unwrap().version, 1);
    }
    assert_eq!(
        published[1].durable.as_ref().unwrap().seq,
        published[0].durable.as_ref().unwrap().seq + 1
    );
}

#[tokio::test]
async fn filters_listed_sessions_by_archive_state() {
    let h = harness();
    let active = h.session().await;
    let archived = h.session().await;
    h.app.sessions.archive(&archived).unwrap();

    let mut both = vec![active.clone(), archived.clone()];
    both.sort();
    assert_eq!(ids(&h.get("/api/session").await.json()), both);
    assert_eq!(
        ids(&h.get("/api/session?archived=false").await.json()),
        vec![active.clone()]
    );
    assert_eq!(
        ids(&h.get("/api/session?archived=true").await.json()),
        vec![archived.clone()]
    );

    // The filter rides in the cursor as a string after parentID.
    let page = h
        .get("/api/session?archived=true&parentID=null&limit=1")
        .await
        .json();
    let next = page["cursor"]["next"].as_str().unwrap();
    let decoded = String::from_utf8(
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(next)
            .unwrap(),
    )
    .unwrap();
    assert!(
        decoded.starts_with(r#"{"parentID":"null","archived":"true","#),
        "{decoded}"
    );
    assert_eq!(
        ids(&h.get(&format!("/api/session?cursor={next}")).await.json()),
        Vec::<String>::new()
    );

    let r = h.get("/api/session?archived=yes").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["_tag"], "InvalidRequestError");

    // An archive time of 0 still counts as archived.
    h.app
        .bus
        .db()
        .with(|c| {
            c.execute(
                "UPDATE session_v2 SET time_archived = 0 WHERE id = ?",
                [&archived],
            )
        })
        .unwrap();
    let info = h.get(&format!("/api/session/{archived}")).await.json();
    assert_eq!(info["data"]["time"]["archived"], json!(0));
    assert_eq!(
        ids(&h.get("/api/session?archived=true").await.json()),
        vec![archived.clone()]
    );
}

#[tokio::test]
async fn rejects_archiving_an_unknown_session() {
    let h = harness();
    for archived in [true, false] {
        let r = h
            .req(
                "PATCH",
                "/api/session/ses_missing_archive",
                Some(json!({"archived": archived})),
            )
            .await;
        assert_eq!(r.status, 404);
        assert_eq!(r.json()["_tag"], "SessionNotFoundError");
    }
    assert!(matches!(
        h.app.sessions.archive("ses_missing_archive"),
        Err(oc_core::session::SessionError::NotFound(_))
    ));
    assert!(matches!(
        h.app.sessions.unarchive("ses_missing_archive"),
        Err(oc_core::session::SessionError::NotFound(_))
    ));
}

#[tokio::test]
async fn filters_idle_messages() {
    let h = harness();
    let session = h.session().await;
    h.app
        .bus
        .db()
        .with(|c| {
            c.execute(
                "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_idle', ?, 'idle', 1, 900, 900, ?)",
                rusqlite::params![session, r#"{"outcome":"succeeded","time":{"created":900}}"#],
            )
        })
        .unwrap();
    let r = h
        .get(&format!("/api/session/{session}/message?type=idle"))
        .await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    assert_eq!(
        r.json()["data"],
        json!([{"id": "msg_idle", "type": "idle", "outcome": "succeeded", "time": {"created": 900}}])
    );
    let r = h
        .get(&format!("/api/session/{session}/message?type=shell"))
        .await;
    assert_eq!(r.json()["data"], json!([]));
}
