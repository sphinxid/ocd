//! End-to-end tests against the fake relay (127.0.0.1 only).

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::fake::{FakeRelay, Options};
use crate::{Client, Event, Routes, Storage};

const ROUTE: &str = "0123456789abcdef";

/// A local HTTP/1.1 target that answers `<method> <path> <body length>` and closes.
async fn target() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut data = Vec::new();
                let mut buffer = [0u8; 4096];
                let (head, body_len) = loop {
                    let n = socket.read(&mut buffer).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    data.extend_from_slice(&buffer[..n]);
                    if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&data[..i]).to_string();
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        while data.len() < i + 4 + len {
                            let n = socket.read(&mut buffer).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            data.extend_from_slice(&buffer[..n]);
                        }
                        break (head, data.len() - i - 4);
                    }
                };
                let line = head.lines().next().unwrap_or("").to_string();
                let size: usize = line
                    .split(' ')
                    .nth(1)
                    .and_then(|p| p.strip_prefix("/big/"))
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                let body = if size > 0 { "z".repeat(size) } else { format!("{line} {body_len}") };
                let response =
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    addr.to_string()
}

/// An HTTPS request through the relay's public listener, trusting only the fake CA.
async fn public_request(relay: &FakeRelay, sni: &str, path: &str, body: &[u8]) -> Result<Vec<u8>, String> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(relay.ca_pem().as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(relay.public).await.map_err(|e| e.to_string())?;
    let name = rustls::pki_types::ServerName::try_from(sni.to_string()).unwrap();
    let mut tls = connector.connect(name, tcp).await.map_err(|e| e.to_string())?;
    let request = format!("POST {path} HTTP/1.1\r\nHost: {sni}\r\nContent-Length: {}\r\n\r\n", body.len());
    tls.write_all(request.as_bytes()).await.map_err(|e| e.to_string())?;
    tls.write_all(body).await.map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    match tls.read_to_end(&mut out).await {
        Ok(_) => Ok(out),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !out.is_empty() => Ok(out),
        Err(e) => Err(e.to_string()),
    }
}

type Recorded = Arc<Mutex<Vec<Event>>>;

fn recorder() -> (Recorded, Arc<dyn Fn(Event) + Send + Sync>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    (events, Arc::new(move |e| sink.lock().push(e)))
}

fn routes(target: &str) -> Routes {
    [(ROUTE.to_string(), target.to_string())].into_iter().collect()
}

async fn wait_for(events: &Recorded, f: impl Fn(&Event) -> bool) {
    for _ in 0..200 {
        if events.lock().iter().any(&f) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("event not seen: {:?}", events.lock());
}

/// Waits until the relay has seen a client message matching `f`.
async fn wait_log(relay: &FakeRelay, f: impl Fn(&Value) -> bool) {
    for _ in 0..200 {
        if from_client(&relay.log()).iter().any(&f) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("message not seen: {:?}", from_client(&relay.log()));
}

fn from_client(log: &[Value]) -> Vec<Value> {
    log.iter().filter_map(|e| e.get("from_client").cloned()).collect()
}

#[tokio::test]
async fn creates_a_tunnel_and_forwards_tls_connections() {
    let relay = FakeRelay::start(Options::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::at(dir.path().join("opentunnel"));
    let client = Client::new(relay.api.clone(), storage.clone());
    let target = target().await;
    let (events, sink) = recorder();
    let connection = client.connect("default", routes(&target), Some(sink)).await.unwrap();
    let hostname = connection.tunnel.hostname.clone();
    assert!(hostname.ends_with(".fake-opentunnel.test"));
    assert_eq!(storage.load("default").unwrap().unwrap().hostname, hostname);
    assert!(!dir.path().join("opentunnel/default/pending.json").exists());

    let response = public_request(&relay, &format!("{ROUTE}.{hostname}"), "/hello", b"abc").await.unwrap();
    let text = String::from_utf8_lossy(&response);
    assert!(text.starts_with("HTTP/1.1 200 OK"), "{text}");
    assert!(text.ends_with("POST /hello HTTP/1.1 3"), "{text}");

    // Large bodies in both directions cross many 32 KiB frames.
    let response =
        public_request(&relay, &format!("{ROUTE}.{hostname}"), "/big/3000000", &vec![b'y'; 2_000_000]).await.unwrap();
    assert!(response.len() > 3_000_000);

    // Unknown routes are reset before TLS starts.
    assert!(public_request(&relay, &format!("other.{hostname}"), "/", b"").await.is_err());
    assert!(public_request(&relay, &hostname, "/", b"").await.is_err());
    wait_for(&events, |e| matches!(e, Event::ConnectionClosed { error: None, .. })).await;

    let log = relay.log();
    let sent = from_client(&log);
    assert_eq!(
        sent[0],
        json!({"type": "attach", "token": storage.load("default").unwrap().unwrap().token, "transport": "ws", "routes": [ROUTE], "client": {"version": "0.1.0", "max_conns": 256}})
    );
    assert!(sent.iter().any(|m| m["type"] == "reset" && m["code"] == "unknown_route"));
    let csr = log.iter().find_map(|e| e.get("csr")).unwrap();
    assert_eq!(csr["cn"], json!(hostname));
    assert_eq!(csr["dns"], json!([hostname, format!("*.{hostname}")]));
    assert_eq!(csr["signature_valid"], json!(true));
    let create = log.iter().find(|e| e["http"] == "POST /api/tunnel").unwrap();
    assert_eq!(create["headers"]["user-agent"], "opentunnel-sdk/0.4.1");
    assert_eq!(create["body"], "{}");
    let upgrade = log.iter().find(|e| e["http"] == "GET /api/tunnel/:id/connect").unwrap();
    assert_eq!(upgrade["headers"]["sec-websocket-protocol"], "opentunnel");

    connection.close().await;
    wait_for(&events, |e| matches!(e, Event::Stopped { error: None })).await;
    assert_eq!(connection.closed().await, Ok(()));
    wait_log(&relay, |m| m == &json!({"close": {"code": 1000, "reason": ""}})).await;

    // A second connect reuses the stored tunnel.
    let again = client.connect("default", routes(&target), None).await.unwrap();
    assert_eq!(again.tunnel.hostname, hostname);
    assert_eq!(relay.tunnels().len(), 1);
    again.close().await;
}

#[tokio::test]
async fn reconnects_after_drain_close_and_drop_and_answers_pings() {
    let relay = FakeRelay::start(Options::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let client = Client::new(relay.api.clone(), Storage::at(dir.path().join("opentunnel")));
    let target = target().await;
    let (events, sink) = recorder();
    let connection = client.connect("default", routes(&target), Some(sink)).await.unwrap();
    let sni = format!("{ROUTE}.{}", connection.tunnel.hostname);

    relay.ping(json!(1e3));
    wait_log(&relay, |m| m == &json!({"type": "pong", "time_sent": 1000})).await;

    relay.drain("maintenance");
    wait_for(&events, |e| e == &Event::Disconnected { reason: "server draining: maintenance".into() }).await;
    wait_for(&events, |e| matches!(e, Event::Connected { .. })).await;
    for _ in 0..100 {
        if relay.bridges() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    relay.close_bridges(4000, "bye now");
    wait_for(&events, |e| e == &Event::Disconnected { reason: "bridge closed: 4000 bye now".into() }).await;
    // After an attached session the backoff restarts at attempt 0.
    assert!(events.lock().iter().any(|e| matches!(e, Event::Reconnecting { attempt: 1, .. })));
    for _ in 0..100 {
        if relay.bridges() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    relay.drop_bridges();
    wait_for(&events, |e| e == &Event::Disconnected { reason: "bridge closed: 1006".into() }).await;
    for _ in 0..100 {
        if relay.bridges() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let response = public_request(&relay, &sni, "/after", b"").await.unwrap();
    assert!(String::from_utf8_lossy(&response).ends_with("POST /after HTTP/1.1 0"));
    connection.close().await;
}

#[tokio::test]
async fn fatal_and_retried_failures() {
    let target = target().await;

    // A rejected token stops the tunnel for good.
    let relay = FakeRelay::start(Options { attach_error: Some("bad_token".into()), ..Options::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    let client = Client::new(relay.api.clone(), Storage::at(dir.path().join("opentunnel")));
    let error = client.connect("default", routes(&target), None).await.err().unwrap();
    assert_eq!(error.to_string(), "Failed to connect tunnel");
    assert!(error.is_fatal());

    // A certificate the server will not issue is fatal too.
    let relay = FakeRelay::start(Options { fail_issuance: Some("rate limited".into()), ..Options::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    let client = Client::new(relay.api.clone(), Storage::at(dir.path().join("opentunnel")));
    let error = client.ensure("default").await.unwrap_err();
    assert_eq!(error.to_string(), "Certificate issuance failed: rate limited");
    assert!(error.is_fatal());
    // The pending identity stays for a later resume.
    assert!(dir.path().join("opentunnel/default/pending.json").exists());

    // API failures are retried by the caller.
    let relay =
        FakeRelay::start(Options { create_error: Some((503, "ServiceUnavailableError".into())), ..Options::default() })
            .await;
    let dir = tempfile::tempdir().unwrap();
    let client = Client::new(relay.api.clone(), Storage::at(dir.path().join("opentunnel")));
    let error = client.ensure("default").await.unwrap_err();
    assert_eq!(error.to_string(), "Failed to create tunnel");
    assert!(!error.is_fatal());

    // Other attach errors reconnect with backoff.
    let relay = FakeRelay::start(Options { attach_error: Some("replaced".into()), ..Options::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    let client = Client::new(relay.api.clone(), Storage::at(dir.path().join("opentunnel")));
    let (events, sink) = recorder();
    let pending = tokio::spawn(async move { client.connect("default", routes(&target), Some(sink)).await.map(|_| ()) });
    wait_for(&events, |e| e == &Event::Disconnected { reason: "attach failed: replaced".into() }).await;
    wait_for(&events, |e| matches!(e, Event::Reconnecting { attempt: 2, .. })).await;
    pending.abort();
}

#[tokio::test]
async fn resumes_an_interrupted_issuance_and_picks_up_renewals() {
    let relay = FakeRelay::start(Options { issuing_polls: 1000, ..Options::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::at(dir.path().join("opentunnel"));
    let client = Client::new(relay.api.clone(), storage.clone());
    // Interrupted while the certificate is being issued.
    assert!(tokio::time::timeout(Duration::from_millis(500), client.ensure("default")).await.is_err());
    assert!(storage.load_pending("default").unwrap().is_some());
    assert!(storage.load("default").unwrap().is_none());
    relay.options(|o| o.issuing_polls = 0);
    relay.clear_log();
    // Resuming binds again (409 while issuing, ignored) and polls until ready.
    let identity = client.ensure("default").await.unwrap();
    assert_eq!(relay.tunnels().len(), 1);
    let log = relay.log();
    assert!(log.iter().any(|e| e["http"] == "POST /api/tunnel/:id/certificate"));
    assert!(!log.iter().any(|e| e["http"] == "POST /api/tunnel"));
    assert!(storage.load_pending("default").unwrap().is_none());

    // A certificate the server renewed while offline is picked up and saved.
    relay.renew();
    let renewed = client.ensure("default").await.unwrap();
    assert_ne!(renewed.certificate, identity.certificate);
    assert_eq!(storage.load("default").unwrap().unwrap().certificate, renewed.certificate);
}
