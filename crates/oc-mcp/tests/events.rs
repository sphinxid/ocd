//! MCP Events (experimental, `mcp_events` flag), ported from upstream's `test/mcp-events.test.ts` (the client's
//! `events/list` and `events/stream`) and `describe("MCP events")` in `test/mcp.test.ts` (the registry's events and
//! subscriptions published as `experimental.mcp.event`), against the fake server over Streamable HTTP: the legacy
//! handshake for the client tests (where a cancelled stream sends `notifications/cancelled`), 2026-07-28 for the
//! registry's.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::modern::{self, Http, Options};
use indexmap::IndexMap;
use oc_mcp::events::{EventMessage, EventStreamError, Events, StreamInput};
use oc_mcp::manager::{SubscribeError, Subscription, SubscriptionStatus};
use oc_mcp::{ClientInfo, ManagerOptions, McpEvent, McpManager, ServerConfig, Status};
use parking_lot::Mutex;
use serde_json::{Value, json};

struct Location {
    manager: McpManager,
    published: Arc<Mutex<Vec<McpEvent>>>,
    changes: tokio::sync::watch::Receiver<usize>,
}

impl Location {
    /// Rechecks `snapshot` on every publication until `ready`.
    async fn until<T>(&mut self, mut snapshot: impl FnMut(&Location) -> T, ready: impl Fn(&T) -> bool) -> T {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let value = snapshot(self);
            if ready(&value) {
                return value;
            }
            let _ = tokio::time::timeout_at(deadline, self.changes.changed()).await.expect("timed out waiting for a publication");
        }
    }

    fn delivered(&self) -> Vec<Value> {
        self.published
            .lock()
            .iter()
            .filter_map(|e| match e {
                McpEvent::ExperimentalEvent { data } => Some(data.clone()),
                _ => None,
            })
            .collect()
    }

    fn only(&self) -> Option<Subscription> {
        self.manager.event_subscriptions().into_iter().next()
    }
}

async fn location(server: &Http, protocol: Option<&str>, mcp_events: bool) -> Location {
    let manager = McpManager::new(ManagerOptions {
        directory: std::env::temp_dir(),
        client_info: ClientInfo { name: "opencode".into(), version: "test".into() },
        credentials: None,
        elicitation: None,
        pool: None,
        mcp_events,
    });
    let published: Arc<Mutex<Vec<McpEvent>>> = Arc::default();
    let (tx, changes) = tokio::sync::watch::channel(0usize);
    let seen = published.clone();
    manager.on_event(Arc::new(move |event: &McpEvent| {
        seen.lock().push(event.clone());
        tx.send_modify(|n| *n += 1);
    }));
    let mut config = json!({"type": "remote", "url": server.url, "oauth": false});
    if let Some(protocol) = protocol {
        config["protocol"] = json!(protocol);
    }
    let config: ServerConfig = serde_json::from_value(config).unwrap();
    manager.set_servers(IndexMap::from([("resources".to_string(), config)])).await;
    manager.wait_ready("resources").await.unwrap();
    assert_eq!(manager.status("resources"), Some(Status::Connected));
    Location { manager, published, changes }
}

async fn events_of(server: &Http) -> (Location, Events) {
    let location = location(server, None, true).await;
    let events = location.manager.events("resources").await.unwrap().expect("MCP events");
    (location, events)
}

fn event(id: &str, cursor: Option<&str>) -> Value {
    let mut params = json!({"eventId": id, "name": "job.finished", "timestamp": "2026-11-10T00:00:00Z", "data": {"eventId": id}});
    if let Some(cursor) = cursor {
        params["cursor"] = json!(cursor);
    }
    params
}

async fn until(what: &str, check: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn input(name: &str, arguments: Option<Value>, cursor: Option<&str>) -> StreamInput {
    StreamInput { name: name.into(), arguments, cursor: cursor.map(|c| Some(c.to_string())), max_age_ms: None }
}

// ---- the client (mcp-events.test.ts) ---------------------------------------------------------------------------

#[tokio::test]
async fn declares_the_extension_and_lists_every_page_of_event_types() {
    let server = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let (_location, events) = events_of(&server).await;
    let capabilities = server.fake.state.lock().capabilities.clone().unwrap();
    assert_eq!(capabilities["extensions"], json!({"io.modelcontextprotocol/events": {}}));
    assert_eq!(
        events.list().await.unwrap(),
        vec![
            json!({"name": "job.finished", "description": "A job ended", "inputSchema": {}}),
            json!({"name": "job.failed", "delivery": ["push"]}),
        ]
    );
    assert_eq!(server.fake.state.lock().lists, vec![json!({}), json!({"cursor": "page-2"})]);
}

#[tokio::test]
async fn delivers_every_message_of_a_stream_in_order_and_ends_when_the_server_closes_it() {
    let server = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let (_location, events) = events_of(&server).await;
    let mut stream = events.stream(input("job.finished", Some(json!({"job": "j1"})), Some("c0")));
    let open = server.next_stream().await;
    assert_eq!(open.params, json!({"name": "job.finished", "arguments": {"job": "j1"}, "cursor": "c0"}));
    open.notify("active", json!({"cursor": "c0", "truncated": true}));
    open.notify("event", event("e1", Some("c1")));
    open.notify("heartbeat", json!({"cursor": "c2"}));
    open.notify("error", json!({"error": {"code": -32603, "message": "UpstreamError"}}));
    open.notify("event", event("e2", None));
    open.close();
    let mut collected = Vec::new();
    while let Some(message) = stream.next().await.unwrap() {
        collected.push(message.to_json());
    }
    assert_eq!(
        collected,
        vec![
            json!({"type": "active", "cursor": "c0", "truncated": true}),
            json!({"type": "event", "eventId": "e1", "name": "job.finished", "timestamp": "2026-11-10T00:00:00Z", "data": {"eventId": "e1"}, "cursor": "c1"}),
            json!({"type": "heartbeat", "cursor": "c2"}),
            json!({"type": "error", "error": {"code": -32603, "message": "UpstreamError"}}),
            json!({"type": "event", "eventId": "e2", "name": "job.finished", "timestamp": "2026-11-10T00:00:00Z", "data": {"eventId": "e2"}, "cursor": null}),
        ]
    );
}

#[tokio::test]
async fn fails_a_stream_the_server_terminates_and_cancels_its_request() {
    let server = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let (_location, events) = events_of(&server).await;
    let mut stream = events.stream(input("job.finished", None, None));
    let open = server.next_stream().await;
    open.notify("terminated", json!({"error": {"code": -32024, "message": "Forbidden", "data": {"reason": "revoked"}}}));
    assert_eq!(
        stream.next().await,
        Err(EventStreamError::Terminated {
            server: "resources".into(),
            code: -32024,
            message: "Forbidden".into(),
            data: Some(json!({"reason": "revoked"})),
        })
    );
    until("the cancellation", || server.fake.state.lock().cancelled.contains(&open.id)).await;
}

#[tokio::test]
async fn routes_concurrent_streams_by_subscription_id() {
    let server = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let (_location, events) = events_of(&server).await;
    let mut a = events.stream(input("job.finished", Some(json!({"job": "a"})), None));
    let first = server.next_stream().await;
    let mut b = events.stream(input("job.finished", Some(json!({"job": "b"})), None));
    let second = server.next_stream().await;
    let (stream_a, stream_b) = if first.params["arguments"]["job"] == "a" { (first, second) } else { (second, first) };
    stream_b.notify("event", event("for-b", None));
    stream_a.notify("event", event("for-a", None));
    assert!(matches!(a.next().await, Ok(Some(EventMessage::Event { event_id, .. })) if event_id == "for-a"));
    assert!(matches!(b.next().await, Ok(Some(EventMessage::Event { event_id, .. })) if event_id == "for-b"));
}

#[tokio::test]
async fn cancels_the_request_when_the_consumer_stops_reading() {
    let server = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let (_location, events) = events_of(&server).await;
    let mut stream = events.stream(input("job.finished", None, None));
    let open = server.next_stream().await;
    open.notify("active", json!({"cursor": null, "truncated": false}));
    assert_eq!(stream.next().await, Ok(Some(EventMessage::Active { cursor: None, truncated: false })));
    drop(stream);
    until("the cancellation", || server.fake.state.lock().cancelled.contains(&open.id)).await;
}

#[tokio::test]
async fn offers_no_events_when_the_server_does_not_or_when_the_connection_does_not_declare_them() {
    let plain = modern::serve_http(Options::default()).await;
    let location = location(&plain, None, true).await;
    assert!(location.manager.events("resources").await.unwrap().is_none());
    let offering = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let undeclared = self::location(&offering, None, false).await;
    assert!(undeclared.manager.events("resources").await.unwrap().is_none());
    let capabilities = offering.fake.state.lock().capabilities.clone().unwrap();
    assert!(capabilities.get("extensions").is_none(), "{capabilities}");
}

// ---- the registry (mcp.test.ts "MCP events") -------------------------------------------------------------------

#[tokio::test]
async fn streams_a_servers_events_over_streamable_http_naming_the_event_in_mcp_name() {
    let server = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let location = location(&server, Some("2026-07-28"), true).await;
    let events = location.manager.events("resources").await.unwrap().expect("MCP events");
    let mut stream = events.stream(input("job.finished", Some(json!({"job": "j1"})), None));
    let open = server.next_stream().await;
    assert_eq!(open.name.as_deref(), Some("job.finished"));
    assert_eq!(open.params["name"], "job.finished");
    assert_eq!(open.params["arguments"], json!({"job": "j1"}));
    open.notify("active", json!({"cursor": "c1", "truncated": false}));
    open.close();
    assert_eq!(stream.next().await, Ok(Some(EventMessage::Active { cursor: Some("c1".into()), truncated: false })));
    assert_eq!(stream.next().await, Ok(None));
}

#[tokio::test]
async fn publishes_a_subscriptions_events_on_the_bus_and_follows_its_status() {
    let server = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let mut location = location(&server, Some("2026-07-28"), true).await;
    let subscription = location
        .manager
        .subscribe_events("resources", "job.finished", Some(json!({"job": "j1", "filter": {"a": 1, "b": 2}})), None)
        .await
        .unwrap();
    assert_eq!(subscription.id, "mcpsub_2c691983d6df57af6bb5e2e8");
    assert_eq!(subscription.server, "resources");
    assert_eq!(subscription.status, SubscriptionStatus::Pending);
    assert_eq!(subscription.cursor, None);
    let open = server.next_stream().await;
    assert_eq!(open.params["cursor"], Value::Null);
    open.notify("active", json!({"cursor": "c1", "truncated": false}));
    location.until(|l| l.only().map(|s| s.status), |s| *s == Some(SubscriptionStatus::Active)).await;
    open.notify(
        "event",
        json!({"eventId": "e1", "name": "job.finished", "timestamp": "2026-11-11T00:00:00Z", "data": [null, {"job": "j1"}, false], "cursor": "c2"}),
    );
    let delivered = location.until(|l| l.delivered(), |d| !d.is_empty()).await;
    assert_eq!(
        delivered,
        vec![json!({
            "id": subscription.id, "server": "resources", "name": "job.finished", "eventId": "e1",
            "timestamp": "2026-11-11T00:00:00Z", "data": [null, {"job": "j1"}, false], "cursor": "c2",
        })]
    );
    assert!(location.published.lock().contains(&McpEvent::ExperimentalSubscriptionChanged { id: subscription.id.clone() }));
    assert_eq!(McpEvent::ExperimentalEvent { data: delivered[0].clone() }.event_type(), "experimental.mcp.event");

    // The same arguments in another key order are the same subscription.
    let again = location
        .manager
        .subscribe_events("resources", "job.finished", Some(json!({"filter": {"b": 2, "a": 1}, "job": "j1"})), None)
        .await
        .unwrap();
    assert_eq!(again.id, subscription.id);
    assert_eq!(server.fake.state.lock().streams.len(), 1);

    location.manager.unsubscribe_events(&subscription.id);
    assert!(location.manager.event_subscriptions().is_empty());
    open.aborted().await;
}

#[tokio::test]
async fn backs_off_failed_opens_resets_after_active_and_resumes_with_a_replay_gap_once() {
    let server = modern::serve_http(Options { events: true, failed_opens: 2, ..Default::default() }).await;
    let mut location = location(&server, Some("2026-07-28"), true).await;
    let started = tokio::time::Instant::now();
    location.manager.subscribe_events("resources", "job.finished", None, None).await.unwrap();
    // Failed opens stay pending and back off 1 s, then 2 s.
    let first = server.next_stream().await;
    let opened = started.elapsed();
    assert_eq!(server.fake.state.lock().streams.len(), 3);
    assert!(opened >= Duration::from_millis(2900), "{opened:?}");
    first.notify("active", json!({"cursor": "c1", "truncated": false}));
    location.until(|l| l.only().map(|s| s.status), |s| *s == Some(SubscriptionStatus::Active)).await;
    first.notify("heartbeat", json!({"cursor": "c5"}));
    first.close();
    let pending = location.until(|l| l.only(), |s| s.as_ref().is_some_and(|s| s.status == SubscriptionStatus::Pending)).await.unwrap();
    assert_eq!(pending.cursor.as_deref(), Some("c5"));
    let closed = tokio::time::Instant::now();
    // A confirmed open reset the delay to a second.
    let reopened = server.next_stream().await;
    let waited = closed.elapsed();
    assert!(waited >= Duration::from_millis(900) && waited < Duration::from_millis(1900), "{waited:?}");
    assert_eq!(server.fake.state.lock().streams.len(), 4);
    assert_eq!(reopened.params["cursor"], "c5");
    reopened.notify("active", json!({"cursor": "c8", "truncated": true}));
    reopened.notify("event", event("e9", Some("c9")));
    reopened.notify("event", event("e10", Some("c10")));
    let delivered = location.until(|l| l.delivered(), |d| d.len() >= 2).await;
    assert_eq!(delivered[0]["eventId"], "e9");
    assert_eq!(delivered[0]["truncated"], true);
    assert_eq!(delivered[1]["eventId"], "e10");
    assert!(delivered[1].get("truncated").is_none());
    reopened.close();
    let pending = location.until(|l| l.only(), |s| s.as_ref().is_some_and(|s| s.status == SubscriptionStatus::Pending)).await.unwrap();
    assert_eq!(pending.cursor.as_deref(), Some("c10"));
}

#[tokio::test]
async fn ends_a_subscription_the_server_terminates_or_rejects_without_reopening_it() {
    let server = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let mut location = location(&server, Some("2026-07-28"), true).await;
    let finished = location.manager.subscribe_events("resources", "job.finished", None, None).await.unwrap();
    let open = server.next_stream().await;
    open.notify("terminated", json!({"error": {"code": -32024, "message": "Revoked"}}));
    let missing = location.manager.subscribe_events("resources", "missing", None, None).await.unwrap();
    let status = |id: String| move |l: &Location| l.manager.event_subscriptions().into_iter().find(|s| s.id == id).map(|s| s.status);
    let ended = |s: &Option<SubscriptionStatus>| matches!(s, Some(SubscriptionStatus::Ended { .. }));
    assert_eq!(
        location.until(status(finished.id.clone()), ended).await,
        Some(SubscriptionStatus::Ended { reason: "terminated".into(), error: Some(json!({"code": -32024, "message": "Revoked"})) })
    );
    assert_eq!(
        location.until(status(missing.id.clone()), ended).await,
        Some(SubscriptionStatus::Ended { reason: "rejected".into(), error: Some(json!({"code": -32023, "message": "Unknown event type"})) })
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let names: Vec<Value> = server.fake.state.lock().streams.iter().map(|p| p["name"].clone()).collect();
    assert_eq!(names, [json!("job.finished"), json!("missing")]);
}

#[tokio::test]
async fn reopens_without_a_replay_gap_when_cursorless_and_ends_when_disabled() {
    let server = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let mut location = location(&server, Some("2026-07-28"), true).await;
    location.manager.subscribe_events("resources", "job.finished", None, None).await.unwrap();
    let first = server.next_stream().await;
    first.notify("active", json!({"cursor": null}));
    location.until(|l| l.only().map(|s| s.status), |s| *s == Some(SubscriptionStatus::Active)).await;

    location.manager.connect("resources").await.unwrap();
    let reopened = server.next_stream().await;
    first.aborted().await;
    assert_eq!(reopened.params["cursor"], Value::Null);
    // The null cursor means replay is unsupported; reconnecting is not evidence of a gap.
    reopened.notify("active", json!({"cursor": null, "truncated": false}));
    reopened.notify("event", event("e2", Some("c2")));
    let delivered = location.until(|l| l.delivered(), |d| !d.is_empty()).await;
    assert_eq!(delivered[0]["eventId"], "e2");
    assert!(delivered[0].get("truncated").is_none());

    location.manager.disconnect("resources").await.unwrap();
    let ended = location
        .until(|l| l.only(), |s| s.as_ref().is_some_and(|s| matches!(s.status, SubscriptionStatus::Ended { .. })))
        .await
        .unwrap();
    assert_eq!(ended.status, SubscriptionStatus::Ended { reason: "unavailable".into(), error: None });
    assert_eq!(ended.cursor.as_deref(), Some("c2"));
}

#[tokio::test]
async fn refuses_to_subscribe_with_the_flag_off_and_ends_at_a_server_without_events() {
    let offering = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let off = location(&offering, Some("2026-07-28"), false).await;
    assert_eq!(
        off.manager.subscribe_events("resources", "job.finished", None, None).await,
        Err(SubscribeError::EventsUnavailable { server: "resources".into(), message: "MCP events need the mcp_events flag.".into() })
    );
    assert_eq!(
        off.manager.subscribe_events("nope", "job.finished", None, None).await.unwrap_err().to_string(),
        "MCP events need the mcp_events flag."
    );

    let plain = modern::serve_http(Options::default()).await;
    let mut location = location(&plain, Some("2026-07-28"), true).await;
    location.manager.subscribe_events("resources", "job.finished", None, None).await.unwrap();
    let ended = location
        .until(|l| l.only().map(|s| s.status), |s| matches!(s, Some(SubscriptionStatus::Ended { .. })))
        .await;
    assert_eq!(ended, Some(SubscriptionStatus::Ended { reason: "unavailable".into(), error: None }));
}

#[tokio::test]
async fn offers_no_events_with_the_flag_off_or_from_a_server_without_them() {
    let offering = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let plain = modern::serve_http(Options::default()).await;
    let off = location(&offering, Some("2026-07-28"), false).await;
    assert!(off.manager.events("resources").await.unwrap().is_none());
    let none = location(&plain, Some("2026-07-28"), true).await;
    assert!(none.manager.events("resources").await.unwrap().is_none());
}
