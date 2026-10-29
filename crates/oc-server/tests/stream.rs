//! SSE framing and PTY WebSocket protocols over a real listener.

mod common;

use std::time::Duration;

use common::*;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

async fn start(mock: std::sync::Arc<Mock>, dir: &str) -> oc_server::RunningServer {
    let mut opts = options(Some(PASSWORD), dir);
    opts.port = Some(0);
    oc_server::serve(mock, opts).await.unwrap()
}

/// Read SSE frames (split on blank lines) from a streaming reqwest response.
struct Frames {
    res: reqwest::Response,
    buf: String,
}

impl Frames {
    async fn next(&mut self) -> Option<String> {
        loop {
            if let Some(i) = self.buf.find("\n\n") {
                let frame = self.buf[..i].to_string();
                self.buf.drain(..i + 2);
                return Some(frame);
            }
            let chunk = tokio::time::timeout(Duration::from_secs(5), self.res.chunk()).await.ok()?.ok()??;
            self.buf.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
}

#[tokio::test]
async fn event_feed_framing() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::new();
    let server = start(mock.clone(), dir.path().to_str().unwrap()).await;
    let res = reqwest::Client::new()
        .get(format!("{}/api/event", server.url()))
        .header("authorization", basic(PASSWORD))
        .header("accept-encoding", "br")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let h = res.headers();
    assert_eq!(h["content-type"], "text/event-stream");
    assert_eq!(h["cache-control"], "no-cache, no-transform");
    assert_eq!(h["x-accel-buffering"], "no");
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert!(h.get("content-encoding").is_none());
    let mut frames = Frames { res, buf: String::new() };

    let first = frames.next().await.unwrap();
    let v: Value = serde_json::from_str(first.strip_prefix("data: ").unwrap()).unwrap();
    assert_eq!(v["type"], "server.connected");
    assert_eq!(v["data"], json!({}));
    assert!(v["id"].as_str().unwrap().starts_with("evt_"));
    assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["id", "type", "data"]);

    // wait for the subscription to exist
    for _ in 0..50 {
        if mock.events.receiver_count() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    mock.events.send(json!({"id":"evt_x","created":1,"type":"lsp.updated","data":{}})).unwrap();
    mock.events
        .send(json!({"id":"evt_2","created":1,"type":"session.text.delta","location":{"directory":"/d"},"data":{"delta":"hi"}}))
        .unwrap();
    mock.events.send(json!({"id":"evt_3","created":2,"type":"rpc.plugin.ping","location":{"directory":"/d"},"data":{}})).unwrap();
    let f = frames.next().await.unwrap();
    assert_eq!(
        f,
        r#"data: {"id":"evt_2","created":1,"type":"session.text.delta","location":{"directory":"/d"},"data":{"delta":"hi"}}"#
    );
    let f = frames.next().await.unwrap();
    assert!(f.contains("rpc.plugin.ping"));
    server.shutdown().await;
}

#[tokio::test]
async fn session_log_sse_and_failure_event() {
    let dir = tempfile::tempdir().unwrap();
    let server = start(Mock::new(), dir.path().to_str().unwrap()).await;
    let client = reqwest::Client::new();
    let res = client
        .get(format!("{}/api/experimental/session/ses_1/log?after=0&follow=true", server.url()))
        .header("authorization", basic(PASSWORD))
        .send()
        .await
        .unwrap();
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    let body = res.text().await.unwrap();
    assert_eq!(
        body,
        concat!(
            "data: {\"type\":\"session.renamed\",\"durable\":{\"aggregateID\":\"ses_1\",\"seq\":1,\"version\":1},\"data\":{}}\n\n",
            "data: {\"type\":\"log.synced\",\"aggregateID\":\"ses_1\",\"seq\":1}\n\n",
            "event: effect/http-api/stream/failure\ndata: [{\"_tag\":\"Die\",\"defect\":\"boom\"}]\n\n",
        )
    );
    // existence is checked before streaming
    let res = client
        .get(format!("{}/api/experimental/session/ses_missing/log", server.url()))
        .header("authorization", basic(PASSWORD))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    assert_eq!(res.json::<Value>().await.unwrap()["_tag"], "SessionNotFoundError");
    let res = client
        .get(format!("{}/api/experimental/session/ses_1/log?follow=1", server.url()))
        .header("authorization", basic(PASSWORD))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    server.shutdown().await;
}

async fn ticket(url: &str, path: &str) -> String {
    let res = reqwest::Client::new()
        .post(format!("{url}{path}"))
        .header("authorization", basic(PASSWORD))
        .header("x-opencode-ticket", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    res.json::<Value>().await.unwrap()["data"]["ticket"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn pty_websocket_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_str().unwrap().to_string();
    let mock = Mock::new();
    let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
    *mock.pty_output.lock() = Some(out_rx);
    let server = start(mock.clone(), &d).await;
    let base = server.url().replacen("http://", "ws://", 1);

    // ticket is bound to {ptyID, directory}: a ticket for another directory is rejected (403)
    let t = ticket(server.url(), "/api/pty/pty_1/connect-token").await;
    let bad = format!("{base}/api/pty/pty_1/connect?ticket={t}&location[directory]=/other");
    let err = tokio_tungstenite::connect_async(bad).await.unwrap_err();
    assert!(err.to_string().contains("403"), "{err}");
    // without ticket or credentials → 401
    let err = tokio_tungstenite::connect_async(format!("{base}/api/pty/pty_1/connect")).await.unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");

    let t = ticket(server.url(), "/api/pty/pty_1/connect-token").await;
    let url = format!("{base}/api/pty/pty_1/connect?ticket={t}&cursor=0");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    assert_eq!(ws.next().await.unwrap().unwrap(), Message::Text("hello".into()));
    match ws.next().await.unwrap().unwrap() {
        Message::Binary(b) => assert_eq!(&b[..], b"\x00{\"cursor\":5}"),
        other => panic!("{other:?}"),
    }
    out_tx.send("live".to_string()).unwrap();
    assert_eq!(ws.next().await.unwrap().unwrap(), Message::Text("live".into()));
    ws.send(Message::Text("ls\n".into())).await.unwrap();
    ws.send(Message::Binary(vec![0xff, 0xfe].into())).await.unwrap(); // invalid UTF-8: dropped
    ws.send(Message::Binary(b"pwd\n".to_vec().into())).await.unwrap();
    for _ in 0..50 {
        if mock.pty_input.lock().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(*mock.pty_input.lock(), vec!["ls\n".to_string(), "pwd\n".to_string()]);
    assert!(mock.calls.lock().iter().any(|c| c.2.contains("cursor: Some(0)")));
    drop(out_tx);
    match ws.next().await.unwrap().unwrap() {
        Message::Close(Some(f)) => assert_eq!(u16::from(f.code), 1000),
        other => panic!("{other:?}"),
    }

    // second attach: the mock has no output channel left → "session exited"
    let mut req = format!("{base}/api/pty/pty_1/connect").into_client_request().unwrap();
    req.headers_mut().insert("authorization", basic(PASSWORD).parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    match ws.next().await.unwrap().unwrap() {
        Message::Close(Some(f)) => {
            assert_eq!(u16::from(f.code), 4404);
            assert_eq!(f.reason.as_str(), "session exited");
        }
        other => panic!("{other:?}"),
    }
    server.shutdown().await;
}

#[tokio::test]
async fn persistent_pty_websocket_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::new();
    let server = start(mock.clone(), dir.path().to_str().unwrap()).await;
    let base = server.url().replacen("http://", "ws://", 1);
    let t = ticket(server.url(), "/api/experimental/persistent-pty/pty_p/connect-token").await;
    let url = format!("{base}/api/experimental/persistent-pty/pty_p/connect?ticket={t}&cursor=2&input_protocol=1&attachment_id=att1&role=observer");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    let text = |m: Message| -> Value {
        match m {
            Message::Text(t) => serde_json::from_str(t.as_str()).unwrap(),
            other => panic!("{other:?}"),
        }
    };
    let attached = text(ws.next().await.unwrap().unwrap());
    assert_eq!(attached["type"], "attached");
    assert_eq!(attached["attachmentID"], "att1");
    assert_eq!(attached["inputProtocol"], 1);
    assert_eq!(attached["role"], "observer");
    assert_eq!(attached["replay"], json!({"requestedOffset":2,"availableOffset":0,"endOffset":4,"truncated":false}));
    assert_eq!(attached["info"]["id"], "pty_p");
    assert_eq!(ws.next().await.unwrap().unwrap(), Message::Binary(b"past".to_vec().into()));
    assert_eq!(text(ws.next().await.unwrap().unwrap()), json!({"type":"replay_complete","endOffset":4}));
    assert_eq!(ws.next().await.unwrap().unwrap(), Message::Binary(b"live".to_vec().into()));
    assert_eq!(
        text(ws.next().await.unwrap().unwrap()),
        json!({"type":"resized","cols":100,"rows":30,"generation":2,"checkpoint":"Y3A="})
    );
    assert_eq!(text(ws.next().await.unwrap().unwrap()), json!({"type":"title_changed","title":"x"}));

    ws.send(Message::Binary(vec![0, 0, 120, 0, 40].into())).await.unwrap();
    ws.send(Message::Binary(vec![1, 0, 120, 0, 40, b'h', b'i'].into())).await.unwrap();
    ws.send(Message::Binary(vec![1, 0, 0, 0, 40, b'x'].into())).await.unwrap(); // ignored
    for _ in 0..50 {
        if mock.persistent_input.lock().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(*mock.persistent_input.lock(), vec![(0, 120, 40, vec![]), (1, 120, 40, b"hi".to_vec())]);
    ws.close(None).await.unwrap();

    // bad cursor → 400 before upgrade; unknown terminal → close 4404 "terminal unavailable"
    let mut req = format!("{base}/api/experimental/persistent-pty/pty_p/connect?cursor=-1").into_client_request().unwrap();
    req.headers_mut().insert("authorization", basic(PASSWORD).parse().unwrap());
    let err = tokio_tungstenite::connect_async(req).await.unwrap_err();
    assert!(err.to_string().contains("400"), "{err}");
    let mut req = format!("{base}/api/experimental/persistent-pty/pty_zz/connect").into_client_request().unwrap();
    req.headers_mut().insert("authorization", basic(PASSWORD).parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    match ws.next().await.unwrap().unwrap() {
        Message::Close(Some(f)) => {
            assert_eq!(u16::from(f.code), 4404);
            assert_eq!(f.reason.as_str(), "terminal unavailable");
        }
        other => panic!("{other:?}"),
    }
    server.shutdown().await;
}

#[tokio::test]
async fn serve_binds_and_reports_info() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(Some(PASSWORD), dir.path().to_str().unwrap());
    opts.port = Some(0);
    opts.ready = false;
    let server = oc_server::serve(Mock::new(), opts).await.unwrap();
    assert!(server.url().starts_with("http://127.0.0.1:"));
    let client = reqwest::Client::new();
    let res = client.get(format!("{}/api/info", server.url())).header("authorization", basic(PASSWORD)).send().await.unwrap();
    assert_eq!(res.status(), 503);
    let v: Value = res.json().await.unwrap();
    assert_eq!(v["urls"], json!([server.url()]));
    assert_eq!(v["pid"], std::process::id());
    server.set_ready();
    let res = client.get(format!("{}/api/info", server.url())).header("authorization", basic(PASSWORD)).send().await.unwrap();
    assert_eq!(res.status(), 200);
    server.shutdown().await;
}

#[tokio::test]
async fn default_port_counts_up_when_busy() {
    // Occupy whatever port the scan would pick first, then check the next server skips it.
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(None, dir.path().to_str().unwrap());
    opts.port = None;
    let a = oc_server::serve(Mock::new(), opts.clone()).await.unwrap();
    let b = oc_server::serve(Mock::new(), opts).await.unwrap();
    assert!(a.local_addr().port() >= oc_server::server::DEFAULT_PORT);
    assert!(b.local_addr().port() > a.local_addr().port());
    a.shutdown().await;
    b.shutdown().await;
}

/// `event-feed.test.ts`: a subscriber with `types` only receives those types, after `server.connected`.
#[tokio::test]
async fn event_feed_filters_by_types() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::new();
    let server = start(mock.clone(), dir.path().to_str().unwrap()).await;
    let res = reqwest::Client::new()
        .get(format!("{}/api/event?types=session.idle&types=rpc.plugin.ping", server.url()))
        .header("authorization", basic(PASSWORD))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let mut frames = Frames { res, buf: String::new() };
    assert!(frames.next().await.unwrap().contains("server.connected"));
    for _ in 0..50 {
        if mock.events.receiver_count() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    mock.events.send(json!({"id":"evt_1","created":1,"type":"session.text.delta","data":{}})).unwrap();
    mock.events.send(json!({"id":"evt_2","created":1,"type":"session.idle","data":{}})).unwrap();
    mock.events.send(json!({"id":"evt_3","created":1,"type":"rpc.other","location":{"directory":"/d"},"data":{}})).unwrap();
    mock.events.send(json!({"id":"evt_4","created":1,"type":"rpc.plugin.ping","location":{"directory":"/d"},"data":{}})).unwrap();
    assert!(frames.next().await.unwrap().contains("\"evt_2\""));
    assert!(frames.next().await.unwrap().contains("\"evt_4\""));

    // unknown types (server.connected included) are a query error
    let client = reqwest::Client::new();
    for bad in ["nope", "server.connected", "session.idle&types=bogus", ""] {
        let res = client
            .get(format!("{}/api/event?types={bad}", server.url()))
            .header("authorization", basic(PASSWORD))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400, "{bad}");
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["_tag"], "InvalidRequestError");
        assert_eq!(v["kind"], "Query");
    }
    server.shutdown().await;
}
