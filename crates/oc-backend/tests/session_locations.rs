//! Envelope locations of session events. Upstream's `bus.publish` takes the location from the ambient
//! Location service, and since 2.2 these session routes run outside one (no `sessionLocationMiddleware`,
//! protocol/src/groups/session.ts): `session.update`, `session.fork`, `session.switchAgent`,
//! `session.switchModel`, `session.revert.commit` and `session.background`. Their events carry no
//! `location`, while routes that still run in the session's location keep it. Checked against the 2.2.2
//! binary (session_crud, session_switch, session_revert, session_fork).

mod common;

use common::*;
use serde_json::json;

fn drain(events: &mut tokio::sync::broadcast::Receiver<oc_core::bus::Event>) -> Vec<oc_core::bus::Event> {
    let mut out = vec![];
    while let Ok(e) = events.try_recv() {
        out.push(e);
    }
    out
}

fn find<'a>(events: &'a [oc_core::bus::Event], kind: &str) -> Vec<&'a oc_core::bus::Event> {
    let found: Vec<_> = events.iter().filter(|e| e.kind == kind).collect();
    assert!(!found.is_empty(), "no {kind} in {:?}", events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>());
    found
}

#[tokio::test]
async fn location_free_session_routes_publish_without_location() {
    let h = harness();
    let id = h.session().await;
    let r = h.post(&format!("/api/session/{id}/prompt"), json!({"text": "hello", "resume": false})).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    h.promote(&id).await;
    let user = h.get(&format!("/api/session/{id}/message")).await.json()["data"][0]["id"].as_str().unwrap().to_string();

    let mut events = h.app.bus.subscribe();
    let uri = format!("/api/session/{id}");
    let patch = |body: serde_json::Value| h.req("PATCH", &uri, Some(body));
    assert_eq!(patch(json!({"title": "Renamed"})).await.status, 204);
    assert_eq!(patch(json!({"metadata": {"k": "v"}})).await.status, 204);
    assert_eq!(patch(json!({"permissions": [{"action": "shell", "resource": "*", "effect": "deny"}]})).await.status, 204);
    assert_eq!(patch(json!({"archived": true})).await.status, 204);
    assert_eq!(patch(json!({"archived": false})).await.status, 204);
    assert_eq!(h.post(&format!("/api/session/{id}/agent"), json!({"agent": "plan"})).await.status, 204);
    assert_eq!(h.post(&format!("/api/session/{id}/model"), json!({"model": {"id": "m", "providerID": "p"}})).await.status, 204);
    assert_eq!(h.post(&format!("/api/session/{id}/fork"), json!({})).await.status, 200);
    let r = h.post(&format!("/api/session/{id}/revert/stage"), json!({"messageID": user})).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    assert_eq!(h.post(&format!("/api/session/{id}/revert/commit"), json!({})).await.status, 204);

    let published = drain(&mut events);
    for kind in [
        "session.renamed",
        "session.metadata.updated",
        "session.permissions",
        "session.archived",
        "session.unarchived",
        "session.agent.selected",
        "session.model.selected",
        "session.forked",
        "session.revert.committed",
    ] {
        for e in find(&published, kind) {
            assert!(e.location.is_none(), "{kind} carries {:?}", e.location);
            assert!(e.durable.is_some(), "{kind} is durable");
        }
    }
    // Staging still runs in the session's location.
    let staged = find(&published, "session.revert.staged");
    assert_eq!(staged[0].location.as_ref().map(|l| l.directory.as_str()), Some(h.dir().as_str()));
}

#[tokio::test]
async fn located_routes_keep_the_session_location() {
    let h = harness();
    let id = h.session().await;
    let mut events = h.app.bus.subscribe();
    // `session.prompt` still runs in the session's location.
    let r = h.post(&format!("/api/session/{id}/prompt"), json!({"text": "hello", "resume": false})).await;
    assert_eq!(r.status, 200);
    h.promote(&id).await;
    // A staged revert committed by the next prompt commits inside the prompt's location.
    let user = h.get(&format!("/api/session/{id}/message")).await.json()["data"][0]["id"].as_str().unwrap().to_string();
    let r = h.post(&format!("/api/session/{id}/revert/stage"), json!({"messageID": user})).await;
    assert_eq!(r.status, 200);
    let r = h.post(&format!("/api/session/{id}/prompt"), json!({"text": "again", "resume": false})).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let published = drain(&mut events);
    let dir = h.dir();
    for kind in ["session.inbox.enqueued", "session.revert.committed"] {
        for e in find(&published, kind) {
            assert_eq!(e.location.as_ref().map(|l| l.directory.as_str()), Some(dir.as_str()), "{kind}");
        }
    }
}

#[tokio::test]
async fn plugin_routing_follows_the_session_owner() {
    let h = harness();
    let id = h.session().await;
    let r = h.post(&format!("/api/session/{id}/prompt"), json!({"text": "hello", "resume": false})).await;
    assert_eq!(r.status, 200);
    h.promote(&id).await;
    let mut events = h.app.bus.subscribe();
    assert_eq!(h.req("PATCH", &format!("/api/session/{id}"), Some(json!({"title": "Renamed"}))).await.status, 204);
    assert_eq!(h.post(&format!("/api/session/{id}/fork"), json!({})).await.status, 200);
    let published = drain(&mut events);
    // Unlocated session events reach the owner's location (the parent's for a fork), like `Bus.subscribe`.
    for kind in ["session.renamed", "session.forked"] {
        let e = find(&published, kind)[0];
        assert!(e.location.is_none());
        assert_eq!(h.app.bus.route(e).map(|l| l.directory), Some(h.dir()), "{kind}");
    }
    // Located and global events route as published.
    let global = h.app.bus.publish("server.test", json!({}), None);
    assert_eq!(h.app.bus.route(&global), None);
}
