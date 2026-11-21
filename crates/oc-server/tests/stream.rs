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

/// `pty-transport.test.ts` "PTY websocket closes with 1012 when its location reloads" (2.2.7), transport side.
#[tokio::test]
async fn pty_websocket_closes_with_1012_when_its_location_shuts_down() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_str().unwrap().to_string();
    let mock = Mock::new();
    let (_out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
    *mock.pty_output.lock() = Some(out_rx);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    *mock.pty_shutdown.lock() = Some(shutdown_rx);
    let server = start(mock.clone(), &d).await;
    let base = server.url().replacen("http://", "ws://", 1);
    let t = ticket(server.url(), "/api/pty/pty_1/connect-token").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("{base}/api/pty/pty_1/connect?ticket={t}")).await.unwrap();
    assert_eq!(ws.next().await.unwrap().unwrap(), Message::Text("hello".into()));
    assert!(matches!(ws.next().await.unwrap().unwrap(), Message::Binary(_)));
    shutdown_tx.send(()).unwrap();
    match ws.next().await.unwrap().unwrap() {
        Message::Close(Some(f)) => {
            assert_eq!(u16::from(f.code), 1012);
            assert_eq!(f.reason.as_str(), "location shut down");
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
    // 2.3.0's new public types are accepted.
    for good in ["session.step.first-token", "experimental.mcp.event&types=session.step.first-token"] {
        let res = client
            .get(format!("{}/api/event?types={good}", server.url()))
            .header("authorization", basic(PASSWORD))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200, "{good}");
    }
    server.shutdown().await;
}

// ---- experimental client-hosted MCP -------------------------------------------------------------------------------

async fn client_mcp_token(url: &str, query: &str, ticket_header: bool, origin: Option<&str>) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .post(format!("{url}/api/experimental/client-mcp/token?{query}"))
        .header("authorization", basic(PASSWORD));
    if ticket_header {
        req = req.header("x-opencode-ticket", "1");
    }
    if let Some(origin) = origin {
        req = req.header("origin", origin);
    }
    req.send().await.unwrap()
}

async fn client_mcp_ticket(url: &str, query: &str) -> String {
    let res = client_mcp_token(url, query, true, None).await;
    assert_eq!(res.status(), 200);
    res.json::<Value>().await.unwrap()["data"]["ticket"].as_str().unwrap().to_string()
}

fn refused(err: tokio_tungstenite::tungstenite::Error) -> u16 {
    match err {
        tokio_tungstenite::tungstenite::Error::Http(response) => response.status().as_u16(),
        other => panic!("{other:?}"),
    }
}

/// `client-mcp.test.ts` "accepts a single-use ticket instead of credentials" and the token route's checks (2.3.1).
#[tokio::test]
async fn client_mcp_tickets() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_str().unwrap().to_string();
    let mock = Mock::new();
    let server = start(mock.clone(), &d).await;
    let url = server.url().to_string();
    let base = url.replacen("http://", "ws://", 1);

    // Without the preflight-forcing header, or from a foreign origin, a ticket cannot be minted even with credentials.
    let res = client_mcp_token(&url, "name=web", false, None).await;
    assert_eq!(res.status(), 403);
    assert_eq!(res.json::<Value>().await.unwrap(), json!({"_tag": "ForbiddenError", "message": "Invalid client MCP connect token request"}));
    assert_eq!(client_mcp_token(&url, "name=web", true, Some("https://evil.example")).await.status(), 403);
    let res = client_mcp_token(&url, "name=bad%20name", true, None).await;
    assert_eq!(res.status(), 400);
    assert_eq!(
        res.json::<Value>().await.unwrap(),
        json!({"_tag": "InvalidRequestError", "message": "Expected a string matching the RegExp ^[a-zA-Z0-9_-]+$\n  at [\"name\"]", "kind": "Query"})
    );
    // The query is checked before the header.
    assert_eq!(client_mcp_token(&url, "", false, None).await.json::<Value>().await.unwrap()["message"], "Missing key\n  at [\"name\"]");
    let res = client_mcp_token(&url, "name=web", true, None).await;
    assert_eq!(res.status(), 200);
    let v = res.json::<Value>().await.unwrap();
    assert_eq!(v["location"]["directory"], d);
    assert_eq!(v["data"]["expires_in"], 60);
    assert_eq!(v["data"]["ticket"].as_str().unwrap().len(), 36);

    // Neither credentials nor a ticket: 401.
    assert_eq!(refused(tokio_tungstenite::connect_async(format!("{base}/api/experimental/client-mcp?name=web")).await.unwrap_err()), 401);
    // A ticket for another name, or another directory, is refused and stays valid for its own.
    let t = client_mcp_ticket(&url, "name=web").await;
    let err = tokio_tungstenite::connect_async(format!("{base}/api/experimental/client-mcp?name=other&ticket={t}")).await.unwrap_err();
    assert_eq!(refused(err), 403);
    let other = format!("{base}/api/experimental/client-mcp?name=web&ticket={t}&location[directory]={}", enc(&format!("{d}/other")));
    assert_eq!(refused(tokio_tungstenite::connect_async(other).await.unwrap_err()), 403);
    // A PTY ticket is no client MCP ticket.
    let pty = ticket(&url, "/api/pty/pty_1/connect-token").await;
    let err = tokio_tungstenite::connect_async(format!("{base}/api/experimental/client-mcp?name=web&ticket={pty}")).await.unwrap_err();
    assert_eq!(refused(err), 403);

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    *mock.mcp_client_output.lock() = Some(rx);
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("{base}/api/experimental/client-mcp?name=web&ticket={t}")).await.unwrap();
    assert!(mock.calls.lock().iter().any(|c| c.0 == "experimental.mcp.client" && c.2.contains("name: \"web\"")));
    // Used up.
    let err = tokio_tungstenite::connect_async(format!("{base}/api/experimental/client-mcp?name=web&ticket={t}")).await.unwrap_err();
    assert_eq!(refused(err), 403);

    // One JSON-RPC message per frame both ways; binary frames are decoded as UTF-8.
    tx.send(oc_server::McpClientOutbound::Text("{\"id\":0}".into())).unwrap();
    assert_eq!(ws.next().await.unwrap().unwrap(), Message::Text("{\"id\":0}".into()));
    ws.send(Message::Text("{\"a\":1}".into())).await.unwrap();
    ws.send(Message::Binary(b"{\"b\":2}".to_vec().into())).await.unwrap();
    for _ in 0..200 {
        if mock.mcp_client_seen.lock().len() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(*mock.mcp_client_seen.lock(), ["open", "deliver {\"a\":1}", "deliver {\"b\":2}"]);
    tx.send(oc_server::McpClientOutbound::Close { code: 1011, reason: "attach failed".into() }).unwrap();
    match ws.next().await.unwrap().unwrap() {
        Message::Close(Some(f)) => {
            assert_eq!(u16::from(f.code), 1011);
            assert_eq!(f.reason.as_str(), "attach failed");
        }
        other => panic!("{other:?}"),
    }
    for _ in 0..200 {
        if mock.mcp_client_seen.lock().last().map(String::as_str) == Some("ended") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(mock.mcp_client_seen.lock().last().map(String::as_str), Some("ended"));
    server.shutdown().await;
}

/// The client route's checks, in upstream's order: the query, the origin, the ticket, the location, the upgrade.
#[tokio::test]
async fn client_mcp_connect_checks() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_str().unwrap().to_string();
    let mock = Mock::new();
    let server = start(mock.clone(), &d).await;
    let url = server.url().to_string();
    let base = url.replacen("http://", "ws://", 1);
    let http = reqwest::Client::new();
    let get = |query: &str, origin: Option<&str>| {
        let mut req = http.get(format!("{url}/api/experimental/client-mcp?{query}")).header("authorization", basic(PASSWORD));
        if let Some(origin) = origin {
            req = req.header("origin", origin);
        }
        req.send()
    };
    let res = get("name=bad%20name", Some("https://evil.example")).await.unwrap();
    assert_eq!(res.status(), 400);
    assert_eq!(res.json::<Value>().await.unwrap()["_tag"], "InvalidRequestError");
    let res = get("name=web&name=x", None).await.unwrap();
    assert_eq!(res.json::<Value>().await.unwrap()["message"], "Expected string\n  at [\"name\"]");
    let res = get("name=web", Some("https://evil.example")).await.unwrap();
    assert_eq!(res.status(), 403);
    assert_eq!(res.text().await.unwrap(), "");
    // An unknown ticket is refused even with credentials.
    assert_eq!(get("name=web&ticket=bogus", None).await.unwrap().status(), 403);
    // The location is resolved before the upgrade; without an upgrade: 400.
    let res = get(&format!("name=web&location[directory]={}", enc("/nonexistent/x")), None).await.unwrap();
    assert_eq!(res.status(), 404);
    assert_eq!(res.json::<Value>().await.unwrap()["_tag"], "LocationNotFoundError");
    let res = get("name=web", None).await.unwrap();
    assert_eq!(res.status(), 400);
    assert_eq!(res.text().await.unwrap(), "");
    // An unknown ticket skips the credential check and is refused by the handler.
    let err = tokio_tungstenite::connect_async(format!("{base}/api/experimental/client-mcp?name=web&ticket=bogus")).await.unwrap_err();
    assert_eq!(refused(err), 403);
    let mut req = format!("{base}/api/experimental/client-mcp?name=web&location[directory]={}", enc(&format!("{d}/missing")))
        .into_client_request()
        .unwrap();
    req.headers_mut().insert("authorization", basic(PASSWORD).parse().unwrap());
    match tokio_tungstenite::connect_async(req).await.unwrap_err() {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(response.status(), 404);
            let body: Value = serde_json::from_slice(response.body().as_deref().unwrap()).unwrap();
            assert_eq!(body["_tag"], "LocationNotFoundError");
        }
        other => panic!("{other:?}"),
    }
    assert!(!mock.calls.lock().iter().any(|c| c.0 == "experimental.mcp.client"));
    server.shutdown().await;
}
