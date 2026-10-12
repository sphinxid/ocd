//! Server-sent events: the global event feed (`/api/event`, spec 06 §8.1) and the session
//! log (`/api/experimental/session/{id}/log`, §8.2).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use crate::backend::{EventSubscription, SessionLogStream};

/// Per-subscriber queue capacity; a subscriber this far behind is dropped.
pub const SUBSCRIBER_CAPACITY: usize = 4_096;
/// Heartbeat interval.
pub const HEARTBEAT: Duration = Duration::from_secs(15);
/// Reserved SSE event name for mid-stream failures.
pub const FAILURE_EVENT: &str = "effect/http-api/stream/failure";

/// Event types forwarded on `/api/event` (`EventManifest.ServerDefinitions`), besides
/// `server.connected` and `rpc.*`.
pub const PUBLIC_EVENT_TYPES: &[&str] = &[
    "location.shutdown",
    "models-dev.refreshed",
    "credential.updated",
    "credential.switched",
    "integration.updated",
    "provider.updated",
    "model.updated",
    "agent.updated",
    "session.created",
    "session.agent.selected",
    "session.model.selected",
    "session.moved",
    "session.renamed",
    "session.metadata.updated",
    "session.permissions",
    "session.viewed",
    "session.usage.updated",
    "session.deleted",
    "session.forked",
    "session.inbox.delivered",
    "session.inbox.enqueued",
    "session.inbox.cancelled",
    "session.inbox.delivery.changed",
    "session.execution.started",
    "session.execution.succeeded",
    "session.execution.failed",
    "session.execution.interrupted",
    "session.instructions.updated",
    "session.synthetic",
    "session.skill.activated",
    "session.shell.started",
    "session.shell.ended",
    "session.step.started",
    "session.step.streamed",
    "session.step.ended",
    "session.step.failed",
    "session.text.started",
    "session.text.delta",
    "session.text.ended",
    "session.reasoning.started",
    "session.reasoning.delta",
    "session.reasoning.ended",
    "session.tool.input.started",
    "session.tool.input.delta",
    "session.tool.input.ended",
    "session.tool.called",
    "session.tool.progress",
    "session.tool.success",
    "session.tool.failed",
    "session.retry.scheduled",
    "session.compaction.started",
    "session.compaction.delta",
    "session.compaction.ended",
    "session.compaction.failed",
    "session.revert.staged",
    "session.revert.cleared",
    "session.revert.committed",
    "filesystem.changed",
    "reference.updated",
    "permission.asked",
    "permission.replied",
    "plugin.updated",
    "project.updated",
    "worktree.updated",
    "worktree.resolved",
    "command.updated",
    "config.updated",
    "skill.updated",
    "pty.created",
    "pty.updated",
    "pty.exited",
    "pty.deleted",
    "persistent-pty.added",
    "persistent-pty.removed",
    "shell.created",
    "shell.exited",
    "shell.deleted",
    "form.created",
    "form.replied",
    "form.cancelled",
    "websearch.updated",
    "session.status",
    "session.idle",
    "tui.prompt.append",
    "tui.command.execute",
    "tui.toast.show",
    "tui.session.select",
    "installation.updated",
    "installation.update-available",
    "vcs.branch.updated",
    "mcp.status.changed",
    "mcp.resources.changed",
];

/// `isOpenCodeEvent`.
pub fn is_public_event(event_type: &str) -> bool {
    event_type == "server.connected" || event_type.starts_with("rpc.") || PUBLIC_EVENT_TYPES.contains(&event_type)
}

/// `data: <JSON>\n\n`
pub fn frame(event: &Value) -> Option<Bytes> {
    let json = crate::support::to_js_string(event).ok()?;
    Some(Bytes::from(format!("data: {json}\n\n")))
}

fn sse_response(body: Body, extra_headers: bool) -> Response {
    let mut res = (StatusCode::OK, body).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    if extra_headers {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, no-transform"));
        h.insert("x-accel-buffering", HeaderValue::from_static("no"));
        h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    }
    res
}

#[derive(Debug)]
struct Overflow;

impl std::fmt::Display for Overflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("event subscriber overflow")
    }
}

impl std::error::Error for Overflow {}

/// Build the `/api/event` response: `server.connected` first, then public events, heartbeats
/// every 15 s. A subscriber that overflows its 4096-frame queue (or lags the broadcast
/// channel) is dropped and its response aborted.
pub fn event_response(subscription: EventSubscription) -> Response {
    event_response_with(subscription, SUBSCRIBER_CAPACITY, HEARTBEAT)
}

pub(crate) fn event_response_with(subscription: EventSubscription, capacity: usize, heartbeat: Duration) -> Response {
    let (tx, rx) = mpsc::channel::<Bytes>(capacity);
    let overflow = Arc::new(AtomicBool::new(false));
    let connected = serde_json::json!({ "id": crate::ids::event_id(), "type": "server.connected", "data": {} });
    if let Some(f) = frame(&connected) {
        let _ = tx.try_send(f);
    }
    let flag = overflow.clone();
    tokio::spawn(async move {
        let offer = |v: Value| -> Result<(), ()> {
            let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
            if !is_public_event(ty) {
                return Ok(());
            }
            let Some(f) = frame(&v) else { return Ok(()) };
            match tx.try_send(f) {
                Ok(()) => Ok(()),
                Err(mpsc::error::TrySendError::Full(_)) => {
                    flag.store(true, Ordering::SeqCst);
                    Err(())
                }
                Err(mpsc::error::TrySendError::Closed(_)) => Err(()),
            }
        };
        match subscription {
            EventSubscription::Broadcast(mut source) => loop {
                tokio::select! {
                    _ = tx.closed() => break,
                    item = source.recv() => match item {
                        Ok(v) => if offer(v).is_err() { break },
                        Err(broadcast::error::RecvError::Lagged(_)) => { flag.store(true, Ordering::SeqCst); break }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            },
            EventSubscription::Stream(mut source) => loop {
                tokio::select! {
                    _ = tx.closed() => break,
                    item = source.next() => match item {
                        Some(v) => if offer(v).is_err() { break },
                        None => break,
                    }
                }
            },
        }
    });
    let start = tokio::time::Instant::now() + heartbeat;
    let interval = tokio::time::interval_at(start, heartbeat);
    let stream = futures::stream::unfold((rx, interval, overflow, false), |(mut rx, mut interval, overflow, done)| async move {
        if done {
            return None;
        }
        tokio::select! {
            item = rx.recv() => match item {
                Some(bytes) => Some((Ok::<Bytes, Overflow>(bytes), (rx, interval, overflow, false))),
                None => {
                    if overflow.load(Ordering::SeqCst) {
                        Some((Err(Overflow), (rx, interval, overflow, true)))
                    } else {
                        None
                    }
                }
            },
            _ = interval.tick() => Some((Ok(Bytes::from_static(b": heartbeat\n\n")), (rx, interval, overflow, false))),
        }
    });
    sse_response(Body::from_stream(stream), true)
}

/// Encode one session-log item / the failure event.
pub(crate) fn log_frame(item: &Result<Value, crate::backend::StreamFailure>) -> Bytes {
    match item {
        Ok(v) => frame(v).unwrap_or_default(),
        Err(f) => {
            let data = crate::support::to_js_string(&f.0).unwrap_or_else(|_| "[]".into());
            Bytes::from(format!("event: {FAILURE_EVENT}\ndata: {data}\n\n"))
        }
    }
}

/// Build the session-log SSE response; a failure item ends the stream after the reserved
/// failure event.
pub fn session_log_response(stream: SessionLogStream) -> Response {
    let stream = stream
        .scan(false, |failed, item| {
            if *failed {
                return futures::future::ready(None);
            }
            *failed = item.is_err();
            futures::future::ready(Some(Ok::<Bytes, std::io::Error>(log_frame(&item))))
        })
        .boxed();
    sse_response(Body::from_stream(stream), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_filter() {
        assert!(is_public_event("session.text.delta"));
        assert!(is_public_event("rpc.plugin.thing"));
        assert!(is_public_event("server.connected"));
        assert!(!is_public_event("lsp.updated"));
        assert!(!is_public_event("mcp.tools.changed"));
        assert!(!is_public_event("session.compacted"));
        assert!(!is_public_event("session.usage.recorded"));
    }

    #[test]
    fn frames() {
        let f = frame(&serde_json::json!({"id":"evt_1","type":"x","data":{}})).unwrap();
        assert_eq!(&f[..], b"data: {\"id\":\"evt_1\",\"type\":\"x\",\"data\":{}}\n\n");
        let fail = log_frame(&Err(crate::backend::StreamFailure::die("boom")));
        assert_eq!(
            &fail[..],
            b"event: effect/http-api/stream/failure\ndata: [{\"_tag\":\"Die\",\"defect\":\"boom\"}]\n\n"
        );
    }

    async fn collect(res: Response, max: usize) -> (Vec<String>, bool) {
        let mut stream = res.into_body().into_data_stream();
        let mut frames = Vec::new();
        let mut errored = false;
        while frames.len() < max {
            match tokio::time::timeout(Duration::from_secs(2), stream.next()).await {
                Ok(Some(Ok(b))) => frames.push(String::from_utf8_lossy(&b).into_owned()),
                Ok(Some(Err(_))) => {
                    errored = true;
                    break;
                }
                _ => break,
            }
        }
        (frames, errored)
    }

    #[tokio::test]
    async fn overflowing_subscriber_is_dropped() {
        let (tx, rx) = broadcast::channel(64);
        let res = event_response_with(EventSubscription::Broadcast(rx), 3, Duration::from_secs(60));
        for i in 0..10 {
            tx.send(serde_json::json!({"id": i, "type": "session.idle", "data": {}})).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (frames, errored) = collect(res, 100).await;
        assert!(frames[0].contains("server.connected"));
        assert!(frames.len() <= 3, "{frames:?}");
        assert!(errored, "stream must fail on overflow");
    }

    #[tokio::test]
    async fn lagging_broadcast_subscriber_is_dropped() {
        let (tx, rx) = broadcast::channel(2);
        for i in 0..5 {
            tx.send(serde_json::json!({"id": i, "type": "session.idle", "data": {}})).unwrap();
        }
        let res = event_response_with(EventSubscription::Broadcast(rx), 100, Duration::from_secs(60));
        let (_, errored) = collect(res, 100).await;
        assert!(errored);
    }

    #[tokio::test]
    async fn heartbeat_comments() {
        let (_tx, rx) = broadcast::channel::<Value>(4);
        let res = event_response_with(EventSubscription::Broadcast(rx), 10, Duration::from_millis(30));
        let (frames, _) = collect(res, 3).await;
        assert!(frames[0].starts_with("data: {\"id\":\"evt_"));
        assert_eq!(frames[1], ": heartbeat\n\n");
        assert_eq!(frames[2], ": heartbeat\n\n");
    }

    #[tokio::test]
    async fn stream_ends_when_source_closes() {
        let res = event_response_with(EventSubscription::Stream(futures::stream::empty().boxed()), 10, Duration::from_secs(60));
        let (frames, errored) = collect(res, 10).await;
        assert_eq!(frames.len(), 1);
        assert!(!errored);
    }
}
