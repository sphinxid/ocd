//! Event stream connection (`client/src/solid/connection.ts`): SSE `GET /api/event` with
//! connect timeout 2000 ms, reconnect delay 1000 ms, idle watchdog 45 s.

use super::Client;
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

/// Run the reconnecting event stream until the receiver is dropped.
pub async fn run(client: Client, tx: UnboundedSender<SseMsg>) {
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
                if tx
                    .send(SseMsg::Status { status: ConnStatus::Reconnecting, attempt, error: Some(error) })
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(RECONNECT_DELAY).await;
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
        let task = tokio::spawn(run(client, tx));
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
}
