//! MCP Events (experimental, the SEP-3415 beta on 2026-07-28), ported from upstream 2.4.1's `test/mcp-events.test.ts`
//! (the client's `events/list`, `check` and `events/stream`) and `describe("MCP events")` in `test/mcp.test.ts` (the
//! registry's events and subscriptions published as `experimental.mcp.event`), against the fake server speaking
//! 2026-07-28 over Streamable HTTP. There a stream the client cancels is aborted rather than named in
//! `notifications/cancelled` (upstream's fixture is an in-memory transport).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::modern::{self, Http, Options};
use indexmap::IndexMap;
use oc_mcp::events::{EventMessage, EventStreamError, Events, StreamInput};
use oc_mcp::manager::{EventPosition, EventUpdate, FollowError, SubscribeError, Subscription, SubscriptionStatus};
use oc_mcp::{ClientInfo, Connection, ManagerOptions, McpEvent, McpManager, ServerConfig, Status};
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

async fn location(server: &Http, protocol: Option<&str>) -> Location {
    let manager = McpManager::new(ManagerOptions {
        directory: std::env::temp_dir(),
        client_info: ClientInfo { name: "opencode".into(), version: "test".into() },
        credentials: None,
        elicitation: None,
        pool: None,
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

/// A connection with an unset protocol, which negotiates 2026-07-28 with the fake.
async fn events_of(server: &Http) -> (Location, Events) {
    let location = location(server, None).await;
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

async fn collect(stream: &mut oc_mcp::events::EventStream) -> Result<Vec<Value>, EventStreamError> {
    let mut collected = Vec::new();
    while let Some(message) = stream.next().await? {
        collected.push(message.to_json());
    }
    Ok(collected)
}

// ---- the client (mcp-events.test.ts) ---------------------------------------------------------------------------

#[tokio::test]
async fn declares_the_extension_and_lists_every_page_of_event_types() {
    let server = modern::serve_http(Options { events: true, events_list_changed: true, ..Default::default() }).await;
    let (_location, events) = events_of(&server).await;
    let capabilities = server.fake.state.lock().capabilities.clone().unwrap();
    assert_eq!(capabilities["extensions"], json!({"io.modelcontextprotocol/events": {}}));
    assert_eq!(
        events.list().await.unwrap(),
        vec![
            json!({
                "name": "job.finished", "description": "A job ended", "delivery": ["push", "poll"],
                "inputSchema": {"type": "object", "properties": {"job": {"type": "string"}}}, "payloadSchema": {"type": "object"},
            }),
            json!({"name": "job.failed", "delivery": ["push", "future"], "inputSchema": {}, "payloadSchema": {}}),
        ]
    );
    assert_eq!(server.fake.state.lock().lists[..2], [json!({}), json!({"cursor": "page-2"})]);
    // It listens for event list changes and relists once the listen opens, again on a change, and again after a
    // listen the server ended is reopened. Each relist reads both pages.
    until("the listen and its relist", || {
        let state = server.fake.state.lock();
        state.listens.len() == 1 && state.lists.len() == 4
    })
    .await;
    assert_eq!(server.fake.state.lock().listens[0], json!({"notifications": {"eventsListChanged": true}}));
    server.send_events_list_changed();
    until("the relist on a change", || server.fake.state.lock().lists.len() == 6).await;
    server.close_listens();
    until("the reopened listen and its relist", || {
        let state = server.fake.state.lock();
        state.listens.len() == 2 && state.lists.len() == 8
    })
    .await;
}

#[tokio::test]
async fn delivers_every_message_of_a_stream_in_order_and_ends_when_the_server_closes_it() {
    let server = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let (_location, events) = events_of(&server).await;
    let mut stream = events.stream(input("job.finished", Some(json!({"job": "j1"})), Some("c0")));
    let open = server.next_stream().await;
    // Resuming from a cursor replays at most an hour.
    assert_eq!(open.params, json!({"name": "job.finished", "arguments": {"job": "j1"}, "cursor": "c0", "maxAgeMs": 3_600_000}));
    open.notify("active", json!({"cursor": "c0", "truncated": true, "heartbeatMs": 30_000}));
    open.notify("event", event("e1", Some("c1")));
    open.notify("heartbeat", json!({"cursor": "c2"}));
    open.notify("error", json!({"error": {"code": -32603, "message": "UpstreamError"}}));
    open.notify("event", event("e2", None));
    open.close();
    assert_eq!(
        collect(&mut stream).await.unwrap(),
        vec![
            json!({"type": "active", "cursor": "c0", "truncated": true}),
            json!({"type": "event", "eventId": "e1", "name": "job.finished", "timestamp": "2026-11-10T00:00:00Z", "data": {"eventId": "e1"}, "cursor": "c1"}),
            json!({"type": "heartbeat", "cursor": "c2"}),
            json!({"type": "error", "error": {"code": -32603, "message": "UpstreamError"}}),
            json!({"type": "event", "eventId": "e2", "name": "job.finished", "timestamp": "2026-11-10T00:00:00Z", "data": {"eventId": "e2"}, "cursor": null}),
        ]
    );
}

/// The beta wire's checks: an `active` needs its heartbeat grant, an event an ISO 8601 timestamp and object data, and
/// events whose name or data don't match the listed type are dropped.
#[tokio::test]
async fn drops_malformed_notifications_and_events_that_do_not_match_their_type() {
    let pages = vec![vec![json!({
        "name": "job.finished", "delivery": ["push"], "inputSchema": {},
        "payloadSchema": {"type": "object", "properties": {"eventId": {"type": "string"}}, "required": ["eventId"]},
    })]];
    let server = modern::serve_http(Options { events: true, event_pages: Some(pages), ..Default::default() }).await;
    let (_location, events) = events_of(&server).await;
    let mut stream = events.stream(input("job.finished", None, None));
    let open = server.next_stream().await;
    open.notify("active", json!({"cursor": "a0"}));
    open.notify("active", json!({"cursor": "a1", "heartbeatMs": 0}));
    open.notify("active", json!({"cursor": "a2", "heartbeatMs": 1.5}));
    open.notify("active", json!({"cursor": "a3", "heartbeatMs": 30_000}));
    let mut bad_time = event("late", None);
    bad_time["timestamp"] = json!("2026-11-10 00:00:00");
    open.notify("event", bad_time);
    let mut list_data = event("list", None);
    list_data["data"] = json!([1]);
    open.notify("event", list_data);
    let mut other = event("other", None);
    other["name"] = json!("job.failed");
    open.notify("event", other);
    let mut mismatched = event("mismatched", None);
    mismatched["data"] = json!({"eventId": 7});
    open.notify("event", mismatched);
    open.notify("error", json!({"error": {"code": 1.5, "message": "not an integer"}}));
    open.notify("error", json!({"error": {"code": -32603, "message": "list data", "data": [1]}}));
    let mut offset = event("offset", Some("c9"));
    offset["timestamp"] = json!("2026-11-10T09:30:00.125+05:30");
    open.notify("event", offset);
    open.close();
    let collected = collect(&mut stream).await.unwrap();
    assert_eq!(
        collected,
        vec![
            json!({"type": "active", "cursor": "a3", "truncated": false}),
            json!({"type": "event", "eventId": "offset", "name": "job.finished", "timestamp": "2026-11-10T09:30:00.125+05:30", "data": {"eventId": "offset"}, "cursor": "c9"}),
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
    open.aborted().await;
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
    open.notify("active", json!({"cursor": null, "truncated": false, "heartbeatMs": 30_000}));
    assert_eq!(stream.next().await, Ok(Some(EventMessage::Active { cursor: None, truncated: false })));
    drop(stream);
    open.aborted().await;
}

/// `fails a stream that misses three granted heartbeats`, in real time with a short grant: any message restarts the
/// deadline, and three intervals of silence fail the stream and cancel its request.
#[tokio::test]
async fn fails_a_stream_that_misses_three_granted_heartbeats() {
    let server = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let (_location, events) = events_of(&server).await;
    let mut stream = events.stream(input("job.finished", None, None));
    let open = server.next_stream().await;
    // A 400 ms grant allows 1.2 s of silence from the last message.
    open.notify("active", json!({"cursor": null, "heartbeatMs": 400}));
    assert!(matches!(stream.next().await, Ok(Some(EventMessage::Active { .. }))));
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        open.notify("heartbeat", json!({}));
        assert_eq!(stream.next().await, Ok(Some(EventMessage::Heartbeat { cursor: None })));
    }
    let quiet = tokio::time::Instant::now();
    assert_eq!(stream.next().await, Err(EventStreamError::Broken("MCP server resources stopped sending event stream heartbeats".into())));
    assert!(quiet.elapsed() >= Duration::from_millis(1100), "{:?}", quiet.elapsed());
    open.aborted().await;
}

#[tokio::test]
async fn checks_the_type_and_arguments_before_opening_a_stream() {
    let server = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let (_location, events) = events_of(&server).await;
    let error = |code: i64, message: &str| EventStreamError::Rejected { server: "resources".into(), code, message: message.into() };
    let rejected = |code: i64, message: &str| Err(error(code, message));
    assert_eq!(events.check("job.finished", Some(&json!({"job": "j1"}))).await, Ok(()));
    assert_eq!(events.check("job.finished", None).await, Ok(()));
    assert_eq!(
        events.check("job.finished", Some(&json!({"job": 1}))).await,
        rejected(-32602, "Arguments for job.finished do not match its inputSchema")
    );
    assert_eq!(events.check("missing", None).await, rejected(-32023, "No push event type missing on MCP server resources"));
    // Malformed types are left out.
    assert_eq!(events.check("job.broken", None).await, rejected(-32023, "No push event type job.broken on MCP server resources"));
    // An unknown name relists every time; a known one doesn't.
    assert_eq!(server.fake.state.lock().lists.len(), 6);
    let mut stream = events.stream(input("job.finished", Some(json!({"job": 2})), None));
    assert_eq!(stream.next().await, Err(error(-32602, "Arguments for job.finished do not match its inputSchema")));
    assert!(server.fake.state.lock().streams.is_empty());
}

#[tokio::test]
async fn offers_no_events_when_the_server_does_not_on_the_legacy_handshake_or_when_undeclared() {
    let plain = modern::serve_http(Options::default()).await;
    let location = location(&plain, None).await;
    assert!(location.manager.events("resources").await.unwrap().is_none());
    let offering = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let legacy = self::location(&offering, Some("legacy")).await;
    assert!(legacy.manager.events("resources").await.unwrap().is_none());
    // Undeclared: a connection that does not declare the extension.
    let undeclared = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let config: ServerConfig = serde_json::from_value(json!({"type": "remote", "url": undeclared.url, "oauth": false})).unwrap();
    let connection =
        Arc::new(Connection::connect("resources", &config, &std::env::temp_dir(), None, None, &ClientInfo::default()).await.unwrap());
    assert!(connection.modern());
    assert!(Events::of(&connection).is_none());
    let capabilities = undeclared.fake.state.lock().capabilities.clone().unwrap();
    assert!(capabilities.get("extensions").is_none(), "{capabilities}");
    connection.close().await;
}

// ---- the registry (mcp.test.ts "MCP events") -------------------------------------------------------------------

#[tokio::test]
async fn streams_a_servers_events_over_streamable_http_with_mcp_name_and_mcp_param_headers() {
    let server = modern::serve_http(Options { events: true, job_header: true, ..Default::default() }).await;
    let location = location(&server, Some("2026-07-28")).await;
    let events = location.manager.events("resources").await.unwrap().expect("MCP events");
    let mut stream = events.stream(input("job.finished", Some(json!({"job": "j1"})), None));
    let open = server.next_stream().await;
    open.notify("active", json!({"cursor": "c1", "truncated": false, "heartbeatMs": 30_000}));
    open.close();
    assert_eq!(collect(&mut stream).await.unwrap(), vec![json!({"type": "active", "cursor": "c1", "truncated": false})]);
    // The first listing has no annotation, so the stream relists after the server rejects it for the missing header.
    assert_eq!(open.name.as_deref(), Some("job.finished"));
    assert_eq!(open.job.as_deref(), Some("j1"));
    let state = server.fake.state.lock();
    assert_eq!(state.streams.len(), 2);
    for params in &state.streams {
        assert_eq!(params["name"], "job.finished");
        assert_eq!(params["arguments"], json!({"job": "j1"}));
    }
    assert_eq!(state.lists.len(), 4);
}

#[tokio::test]
async fn publishes_a_subscriptions_events_on_the_bus_and_follows_its_status() {
    let server = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let mut location = location(&server, Some("2026-07-28")).await;
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
    open.notify("active", json!({"cursor": "c1", "truncated": false, "heartbeatMs": 30_000}));
    location.until(|l| l.only().map(|s| s.status), |s| *s == Some(SubscriptionStatus::Active)).await;
    open.notify(
        "event",
        json!({"eventId": "e1", "name": "job.finished", "timestamp": "2026-11-11T00:00:00Z", "data": {"job": "j1", "ok": false, "note": null}, "cursor": "c2"}),
    );
    let delivered = location.until(|l| l.delivered(), |d| !d.is_empty()).await;
    assert_eq!(
        delivered,
        vec![json!({
            "id": subscription.id, "server": "resources", "name": "job.finished", "eventId": "e1",
            "timestamp": "2026-11-11T00:00:00Z", "data": {"job": "j1", "ok": false, "note": null}, "cursor": "c2",
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

/// Each reopen waits its backoff delay after the stream ended, jittered to between 0.8 and 1.2 times it.
#[tokio::test]
async fn backs_off_failed_opens_resets_after_active_and_resumes_with_a_replay_gap_once() {
    let server = modern::serve_http(Options { events: true, failed_opens: 2, ..Default::default() }).await;
    let mut location = location(&server, Some("2026-07-28")).await;
    let started = tokio::time::Instant::now();
    location.manager.subscribe_events("resources", "job.finished", None, None).await.unwrap();
    // Failed opens stay pending and back off about 1 s, then about 2 s.
    let first = server.next_stream().await;
    let opened = started.elapsed();
    assert_eq!(server.fake.state.lock().streams.len(), 3);
    assert!(opened >= Duration::from_millis(2400), "{opened:?}");
    first.notify("active", json!({"cursor": "c1", "truncated": false, "heartbeatMs": 30_000}));
    location.until(|l| l.only().map(|s| s.status), |s| *s == Some(SubscriptionStatus::Active)).await;
    first.notify("heartbeat", json!({"cursor": "c5"}));
    first.close();
    let pending = location.until(|l| l.only(), |s| s.as_ref().is_some_and(|s| s.status == SubscriptionStatus::Pending)).await.unwrap();
    assert_eq!(pending.cursor.as_deref(), Some("c5"));
    let closed = tokio::time::Instant::now();
    // A confirmed open reset the delay to about a second.
    let reopened = server.next_stream().await;
    let waited = closed.elapsed();
    assert!(waited >= Duration::from_millis(750) && waited < Duration::from_millis(1900), "{waited:?}");
    assert_eq!(server.fake.state.lock().streams.len(), 4);
    assert_eq!(reopened.params["cursor"], "c5");
    assert_eq!(reopened.params["maxAgeMs"], 3_600_000);
    reopened.notify("active", json!({"cursor": "c8", "truncated": true, "heartbeatMs": 30_000}));
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
    let mut location = location(&server, Some("2026-07-28")).await;
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
        Some(SubscriptionStatus::Ended {
            reason: "rejected".into(),
            error: Some(json!({"code": -32023, "message": "No push event type missing on MCP server resources"})),
        })
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    // "missing" isn't listed, so the client refuses it without asking the server.
    let names: Vec<Value> = server.fake.state.lock().streams.iter().map(|p| p["name"].clone()).collect();
    assert_eq!(names, [json!("job.finished")]);
}

#[tokio::test]
async fn reopens_without_a_replay_gap_when_cursorless_and_ends_when_disabled() {
    let server = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let mut location = location(&server, Some("2026-07-28")).await;
    location.manager.subscribe_events("resources", "job.finished", None, None).await.unwrap();
    let first = server.next_stream().await;
    first.notify("active", json!({"cursor": null, "heartbeatMs": 30_000}));
    location.until(|l| l.only().map(|s| s.status), |s| *s == Some(SubscriptionStatus::Active)).await;

    location.manager.connect("resources").await.unwrap();
    let reopened = server.next_stream().await;
    first.aborted().await;
    assert_eq!(reopened.params["cursor"], Value::Null);
    // The null cursor means replay is unsupported; reconnecting is not evidence of a gap.
    reopened.notify("active", json!({"cursor": null, "truncated": false, "heartbeatMs": 30_000}));
    reopened.notify("event", event("e2", Some("c2")));
    let delivered = location.until(|l| l.delivered(), |d| !d.is_empty()).await;
    assert_eq!(delivered[0]["data"], json!({"eventId": "e2"}));
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
async fn ends_a_subscription_at_a_server_without_events() {
    let plain = modern::serve_http(Options::default()).await;
    let mut location = location(&plain, Some("2026-07-28")).await;
    location.manager.subscribe_events("resources", "job.finished", None, None).await.unwrap();
    let ended = location
        .until(|l| l.only().map(|s| s.status), |s| matches!(s, Some(SubscriptionStatus::Ended { .. })))
        .await;
    assert_eq!(ended, Some(SubscriptionStatus::Ended { reason: "unavailable".into(), error: None }));
    assert_eq!(
        location.manager.subscribe_events("nope", "job.finished", None, None).await,
        Err(SubscribeError::NotFound("nope".into()))
    );
}

#[tokio::test]
async fn offers_no_events_from_a_server_without_them() {
    let plain = modern::serve_http(Options::default()).await;
    let none = location(&plain, Some("2026-07-28")).await;
    assert!(none.manager.events("resources").await.unwrap().is_none());
}

/// `Mcp.follow`: positions with the event ids it delivered, replayed events skipped, the cursor capped.
#[tokio::test]
async fn follow_yields_positions_skips_replayed_events_and_refuses_an_oversized_cursor() {
    let server = modern::serve_http(Options { events: true, ..Default::default() }).await;
    let location = location(&server, Some("2026-07-28")).await;
    let position = EventPosition { cursor: Some("c0".into()), seen: vec!["e1".into()] };
    let mut following = location.manager.follow("resources", "job.finished", None, Some(position)).unwrap();
    let open = server.next_stream().await;
    assert_eq!(open.params, json!({"name": "job.finished", "cursor": "c0", "maxAgeMs": 3_600_000}));
    open.notify("active", json!({"cursor": "c0", "heartbeatMs": 30_000}));
    async fn next(following: &mut oc_mcp::manager::Following) -> Result<EventUpdate, FollowError> {
        tokio::time::timeout(Duration::from_secs(20), following.next()).await.expect("an update").expect("following")
    }
    let at = |cursor: &str, seen: &[&str]| EventPosition { cursor: Some(cursor.into()), seen: seen.iter().map(|s| s.to_string()).collect() };
    assert_eq!(next(&mut following).await, Ok(EventUpdate::Position(at("c0", &["e1"]))));
    // A replayed event moves the cursor without being delivered again; a heartbeat that doesn't move it says nothing.
    open.notify("event", event("e1", Some("c1")));
    assert_eq!(next(&mut following).await, Ok(EventUpdate::Position(at("c1", &["e1"]))));
    open.notify("heartbeat", json!({"cursor": "c1"}));
    open.notify("event", event("e2", Some("c2")));
    let Ok(EventUpdate::Event { event, position, truncated }) = next(&mut following).await else { panic!("an event") };
    assert_eq!((event.event_id.as_str(), event.cursor.as_deref(), truncated), ("e2", Some("c2"), false));
    assert_eq!(position, at("c2", &["e1", "e2"]));
    open.notify("heartbeat", json!({"cursor": "x".repeat(4097)}));
    assert_eq!(
        next(&mut following).await,
        Err(FollowError::Unavailable { server: "resources".into(), message: "The MCP server sent an oversized event cursor.".into() })
    );
    open.aborted().await;
    assert!(matches!(location.manager.follow("nope", "job.finished", None, None), Err(SubscribeError::NotFound(_))));
}
