//! Event stream connection (`client/src/solid/connection.ts`): SSE `GET /api/event` with
//! connect timeout 2000 ms, reconnect delay 1000 ms, idle watchdog 45 s.

use super::Client;
use crate::app::EndpointHook;
use super::types::Event;
use futures::StreamExt;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(2000);
pub const RECONNECT_DELAY: Duration = Duration::from_millis(1000);
pub const IDLE_TIMEOUT: Duration = Duration::from_millis(45_000);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnStatus {
    Connecting,
    Connected,
    Reconnecting,
}

#[derive(Clone, Debug)]
pub enum SseMsg {
    Status { status: ConnStatus, attempt: u32, error: Option<String> },
    Event(Box<Event>),
}

/// Incremental SSE frame parser: `data: <json>\n\n`, comments start with `:`.
#[derive(Default)]
pub struct SseParser {
    buf: String,
}

impl SseParser {
    /// Feed bytes and return the `data` payloads of completed frames.
    pub fn feed(&mut self, chunk: &str) -> Vec<String> {
        self.buf.push_str(&chunk.replace("\r\n", "\n"));
        let mut out = Vec::new();
        while let Some(idx) = self.buf.find("\n\n") {
            let frame: String = self.buf.drain(..idx + 2).collect();
            let mut data: Vec<&str> = Vec::new();
            for line in frame.lines() {
                if line.starts_with(':') {
                    continue;
                }
                if let Some(rest) = line.strip_prefix("data:") {
                    data.push(rest.strip_prefix(' ').unwrap_or(rest));
                }
            }
            if !data.is_empty() {
                out.push(data.join("\n"));
            }
        }
        out
    }
}

enum Outcome {
    Aborted,
    Ended { error: String, connected_at: Option<Instant> },
}

async fn connect(client: &Client, tx: &UnboundedSender<SseMsg>) -> Outcome {
    let start = tokio::time::timeout(CONNECT_TIMEOUT, async {
        let resp = client.event_stream().await?;
        let mut stream = resp.bytes_stream();
        let mut parser = SseParser::default();
        // Wait for the first event.
        loop {
            match stream.next().await {
                None => anyhow::bail!("Event stream disconnected"),
                Some(Err(e)) => return Err(e.into()),
                Some(Ok(bytes)) => {
                    let frames = parser.feed(&String::from_utf8_lossy(&bytes));
                    if let Some(first) = frames.first() {
                        let ev: Event = serde_json::from_str(first)?;
                        if ev.kind != "server.connected" {
                            anyhow::bail!("Event stream did not start with server.connected");
                        }
                        let rest: Vec<Event> =
                            frames[1..].iter().filter_map(|f| serde_json::from_str(f).ok()).collect();
                        return Ok((stream, parser, ev, rest));
                    }
                }
            }
        }
    })
    .await;
    let (mut stream, mut parser, first, rest) = match start {
        Err(_) => return Outcome::Ended { error: "Timed out connecting to server".into(), connected_at: None },
        Ok(Err(e)) => return Outcome::Ended { error: super::error_message(&e), connected_at: None },
        Ok(Ok(v)) => v,
    };
    let connected_at = Instant::now();
    if tx.send(SseMsg::Event(Box::new(first))).is_err() {
        return Outcome::Aborted;
    }
    let _ = tx.send(SseMsg::Status { status: ConnStatus::Connected, attempt: 0, error: None });
    for ev in rest {
        let _ = tx.send(SseMsg::Event(Box::new(ev)));
    }
    loop {
        match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
            Err(_) => return Outcome::Ended { error: "Event stream stalled".into(), connected_at: Some(connected_at) },
            Ok(None) => {
                return Outcome::Ended { error: "Event stream disconnected".into(), connected_at: Some(connected_at) };
            }
            Ok(Some(Err(e))) => return Outcome::Ended { error: e.to_string(), connected_at: Some(connected_at) },
            Ok(Some(Ok(bytes))) => {
                for frame in parser.feed(&String::from_utf8_lossy(&bytes)) {
                    match serde_json::from_str::<Event>(&frame) {
                        Ok(ev) => {
                            if tx.send(SseMsg::Event(Box::new(ev))).is_err() {
                                return Outcome::Aborted;
                            }
                        }
                        Err(e) => tracing::debug!("invalid event frame: {e}"),
                    }
                }
            }
        }
    }
}

/// How [`run`] retries after the stream ends.
#[derive(Clone)]
pub struct Retry {
    /// Re-resolve the server after every disconnect (`options.reconnect`): a managed service
    /// that stopped is started again, possibly on a new port/password. The client (and every
    /// clone of it) moves to the endpoint it returns.
    pub reconnect: Option<EndpointHook>,
    /// Wait before the next attempt.
    pub delay: Duration,
    /// Skip the wait when the first retry re-resolved the server (`if (attempt === 1) continue`).
    pub immediate: bool,
}

impl Retry {
    /// `createClientConnection` (full TUI): 1000 ms, first re-resolved retry immediate.
    pub fn tui(reconnect: Option<EndpointHook>) -> Retry {
        Retry { reconnect, delay: RECONNECT_DELAY, immediate: true }
    }

    /// The mini transport's receive loop (`stream-v2.transport.ts`): re-resolve, then 250 ms.
    pub fn mini(reconnect: Option<EndpointHook>) -> Retry {
        Retry { reconnect, delay: MINI_RECONNECT_DELAY, immediate: false }
    }
}

pub const MINI_RECONNECT_DELAY: Duration = Duration::from_millis(250);

/// Run the reconnecting event stream until the receiver is dropped.
pub async fn run(client: Client, tx: UnboundedSender<SseMsg>, retry: Retry) {
    let mut attempt: u32 = 0;
    loop {
        let status = if attempt == 0 { ConnStatus::Connecting } else { ConnStatus::Reconnecting };
        if tx.send(SseMsg::Status { status, attempt, error: None }).is_err() {
            return;
        }
        match connect(&client, &tx).await {
            Outcome::Aborted => return,
            Outcome::Ended { error, connected_at } => {
                if connected_at.is_some_and(|t| t.elapsed() >= RECONNECT_DELAY) {
                    attempt = 0;
                }
                attempt += 1;
                tracing::info!(attempt, error = %error, "event stream disconnected");
                if tx
                    .send(SseMsg::Status { status: ConnStatus::Reconnecting, attempt, error: Some(error) })
                    .is_err()
                {
                    return;
                }
                if let Some(reconnect) = &retry.reconnect {
                    let next = reconnect().await;
                    if tx.is_closed() {
                        return;
                    }
                    match next {
                        Ok(endpoint) => {
                            if endpoint.url.trim_end_matches('/') != client.base() {
                                tracing::info!(url = %endpoint.url, "server endpoint changed");
                            }
                            client.set_endpoint(&endpoint);
                            if retry.immediate && attempt == 1 {
                                continue;
                            }
                        }
                        Err(e) => tracing::info!(attempt, error = %super::error_message(&e), "server resolution failed"),
                    }
                }
                tokio::time::sleep(retry.delay).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_frames() {
        let mut p = SseParser::default();
        assert!(p.feed("data: {\"a\":1}").is_empty());
        let out = p.feed("\n\n: heartbeat\n\ndata: {\"b\":2}\n\n");
        assert_eq!(out, vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()]);
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn connects_to_mock_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(req.starts_with("GET /api/event"));
            assert!(req.to_lowercase().contains("authorization: basic b3blbmnvzgu6c2vjcmv0"));
            let body = "data: {\"id\":\"evt_1\",\"type\":\"server.connected\",\"data\":{}}\n\n: heartbeat\n\ndata: {\"id\":\"evt_2\",\"created\":1,\"type\":\"session.renamed\",\"data\":{\"sessionID\":\"ses_1\",\"title\":\"T\"}}\n\n";
            let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n{body}");
            sock.write_all(resp.as_bytes()).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        });
        let client = crate::api::Client::new(&format!("http://{addr}"), Some("secret"), "/tmp");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run(client, tx, Retry::tui(None)));
        let mut kinds = Vec::new();
        while let Ok(Some(m)) = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv()).await {
            match m {
                SseMsg::Status { status, .. } => kinds.push(format!("{status:?}")),
                SseMsg::Event(e) => kinds.push(e.kind.clone()),
            }
            if kinds.iter().any(|k| k == "session.renamed") {
                break;
            }
        }
        task.abort();
        assert_eq!(kinds, vec!["Connecting", "server.connected", "Connected", "session.renamed"]);
    }

    /// A mock event server: every accepted stream sends `server.connected`; with `close` the
    /// stream then ends (a dropped service), otherwise it stays open. Reports each request's
    /// arrival time and lower-cased `authorization` header.
    async fn mock(close: bool) -> (String, tokio::sync::mpsc::UnboundedReceiver<(Instant, String)>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                    let auth = req.lines().find_map(|l| l.strip_prefix("authorization: ")).unwrap_or("").trim().to_string();
                    let _ = tx.send((Instant::now(), auth));
                    let body = "data: {\"id\":\"evt_1\",\"type\":\"server.connected\",\"data\":{}}\n\n";
                    let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n{body}");
                    let _ = sock.write_all(resp.as_bytes()).await;
                    if !close {
                        tokio::time::sleep(Duration::from_secs(10)).await;
                    }
                });
            }
        });
        (url, rx)
    }

    /// A URL nothing listens on (connection refused).
    async fn dead() -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        format!("http://{}", l.local_addr().unwrap())
    }

    fn auth(password: &str) -> String {
        use base64::Engine;
        format!("basic {}", base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"))).to_lowercase()
    }

    /// A reconnect hook handing out `endpoints` in order (the last one repeats), recording calls.
    fn hook(endpoints: Vec<super::super::Endpoint>) -> (EndpointHook, std::sync::Arc<std::sync::Mutex<Vec<Instant>>>) {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = calls.clone();
        let hook: EndpointHook = std::sync::Arc::new(move || {
            let mut c = seen.lock().unwrap();
            c.push(Instant::now());
            let e = endpoints[(c.len() - 1).min(endpoints.len() - 1)].clone();
            Box::pin(async move { Ok(e) })
        });
        (hook, calls)
    }

    /// `runStream`: after a disconnect the managed-service reconnect runs, the client moves to
    /// the endpoint it returns (new port and password), and the first retry does not wait.
    #[tokio::test]
    async fn reconnect_hook_moves_to_new_endpoint_immediately() {
        let (old, mut old_rx) = mock(true).await;
        let (new, mut new_rx) = mock(false).await;
        let (hook, calls) = hook(vec![super::super::Endpoint { url: new.clone(), password: Some("fresh".into()) }]);
        let client = crate::api::Client::new(&old, Some("stale"), "/tmp");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run(client.clone(), tx, Retry::tui(Some(hook))));
        let (_, old_auth) = old_rx.recv().await.unwrap();
        assert_eq!(old_auth, auth("stale"));
        let (at, new_auth) = tokio::time::timeout(Duration::from_secs(3), new_rx.recv()).await.unwrap().unwrap();
        assert_eq!(new_auth, auth("fresh"));
        let called = calls.lock().unwrap().clone();
        assert_eq!(called.len(), 1);
        assert!(at.duration_since(called[0]) < Duration::from_millis(500), "first retry should not wait");
        let mut statuses = Vec::new();
        while let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            if let SseMsg::Status { status, attempt, error } = m {
                statuses.push((status, attempt, error.is_some()));
            }
        }
        task.abort();
        assert_eq!(
            statuses,
            vec![
                (ConnStatus::Connecting, 0, false),
                (ConnStatus::Connected, 0, false),
                (ConnStatus::Reconnecting, 1, true),
                (ConnStatus::Reconnecting, 1, false),
                (ConnStatus::Connected, 0, false),
            ]
        );
        // Every clone of the client (terminal websocket, requests) follows.
        assert_eq!(client.base(), new);
        assert!(client.terminal_url("pty_1", "t", 0, "a").starts_with(&new.replacen("http://", "ws://", 1)));
    }

    /// Later retries back off by the reconnect delay even when the hook resolves.
    #[tokio::test]
    async fn later_retries_wait_for_the_delay() {
        let (old, _old_rx) = mock(true).await;
        let (new, mut new_rx) = mock(false).await;
        let gone = dead().await;
        let (hook, calls) = hook(vec![
            super::super::Endpoint { url: gone, password: None },
            super::super::Endpoint { url: new.clone(), password: None },
        ]);
        let client = crate::api::Client::new(&old, None, "/tmp");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run(client.clone(), tx, Retry::tui(Some(hook))));
        let (at, _) = tokio::time::timeout(Duration::from_secs(5), new_rx.recv()).await.unwrap().unwrap();
        task.abort();
        let called = calls.lock().unwrap().clone();
        assert_eq!(called.len(), 2);
        let waited = at.duration_since(called[1]);
        assert!(waited >= RECONNECT_DELAY - Duration::from_millis(50), "second retry waited {waited:?}");
        assert!(called[1].duration_since(called[0]) < Duration::from_millis(500), "first retry was immediate");
    }

    /// A failing hook keeps the old endpoint and retries after the delay.
    #[tokio::test]
    async fn failed_resolution_keeps_endpoint() {
        let (old, mut old_rx) = mock(true).await;
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        let hook: EndpointHook = std::sync::Arc::new(move || {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Err(anyhow::anyhow!("Timed out waiting for the background service to start")) })
        });
        let client = crate::api::Client::new(&old, None, "/tmp");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run(client.clone(), tx, Retry::tui(Some(hook))));
        let (first, _) = old_rx.recv().await.unwrap();
        let (second, _) = tokio::time::timeout(Duration::from_secs(3), old_rx.recv()).await.unwrap().unwrap();
        task.abort();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(second.duration_since(first) >= RECONNECT_DELAY - Duration::from_millis(50));
        assert_eq!(client.base(), old);
    }

    /// Mini's receive loop re-resolves and then always waits 250 ms.
    #[tokio::test]
    async fn mini_retry_waits_short_delay() {
        let (old, _old_rx) = mock(true).await;
        let (new, mut new_rx) = mock(false).await;
        let (hook, calls) = hook(vec![super::super::Endpoint { url: new.clone(), password: None }]);
        let client = crate::api::Client::new(&old, None, "/tmp");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run(client.clone(), tx, Retry::mini(Some(hook))));
        let (at, _) = tokio::time::timeout(Duration::from_secs(3), new_rx.recv()).await.unwrap().unwrap();
        task.abort();
        let called = calls.lock().unwrap().clone();
        let waited = at.duration_since(called[0]);
        assert!(waited >= MINI_RECONNECT_DELAY - Duration::from_millis(20) && waited < RECONNECT_DELAY, "waited {waited:?}");
        assert_eq!(client.base(), new);
    }
}
