//! End-to-end tests against the fake relay (127.0.0.1 only).

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::fake::{FakeRelay, Options};
use crate::{Client, ClientOptions, Event, Routes, Storage};

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
    crate::routes::routes([(ROUTE, target)])
}

/// A client that reads no proxy variables (tests never depend on the environment they run in), with short API retry
/// delays.
fn test_client(relay: &FakeRelay, storage: Storage) -> Client {
    client_with(relay, storage, &[])
}

fn client_with(relay: &FakeRelay, storage: Storage, env: &[(&str, &str)]) -> Client {
    let options = ClientOptions { proxy: None, env: Some(crate::proxy::env_of(env)) };
    let mut client = Client::with_options(relay.api.clone(), storage, options).unwrap();
    client.set_retry_scale(0.01);
    client
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
    let client = test_client(&relay, storage.clone());
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
    assert_eq!(create["headers"]["user-agent"], "opentunnel-sdk/0.6.1");
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
    let client = test_client(&relay, Storage::at(dir.path().join("opentunnel")));
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
    let client = test_client(&relay, Storage::at(dir.path().join("opentunnel")));
    let error = client.connect("default", routes(&target), None).await.err().unwrap();
    assert_eq!(error.to_string(), "Failed to connect tunnel: Bridge attach failed: bad_token");
    assert_eq!(error.code(), Some("bad_token"));
    assert!(error.is_fatal());

    // A certificate the server will not issue is fatal too.
    let relay = FakeRelay::start(Options { fail_issuance: Some("rate limited".into()), ..Options::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    let client = test_client(&relay, Storage::at(dir.path().join("opentunnel")));
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
    let client = test_client(&relay, Storage::at(dir.path().join("opentunnel")));
    let error = client.ensure("default").await.unwrap_err();
    assert_eq!(error.to_string(), "Failed to create tunnel: ServiceUnavailableError: Fake failure");
    assert!(!error.is_fatal());
    // `POST /api/tunnel` is sent again on a 503, six times.
    assert_eq!(relay.log().iter().filter(|e| e["http"] == "POST /api/tunnel").count(), 7);

    // Other attach errors reconnect with backoff.
    let relay = FakeRelay::start(Options { attach_error: Some("replaced".into()), ..Options::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    let client = test_client(&relay, Storage::at(dir.path().join("opentunnel")));
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
    let client = test_client(&relay, storage.clone());
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

// ---- proxies, retries, route options (0.6.1) -------------------------------------------------------------------

/// Request lines and headers a mock proxy received.
type ProxyLog = Arc<Mutex<Vec<(String, Vec<String>)>>>;

/// A local HTTP proxy that records each request it gets: `CONNECT` tunnels, and absolute-form requests forwarded one
/// at a time. It only connects to 127.0.0.1.
struct MockProxy {
    url: String,
    log: ProxyLog,
}

impl MockProxy {
    async fn start() -> MockProxy {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let log: ProxyLog = Arc::new(Mutex::new(Vec::new()));
        let record = log.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(proxy_connection(socket, record.clone()));
            }
        });
        MockProxy { url, log }
    }

    fn requests(&self) -> Vec<String> {
        self.log.lock().iter().map(|(line, _)| line.clone()).collect()
    }

    fn headers(&self, prefix: &str) -> Vec<Vec<String>> {
        self.log.lock().iter().filter(|(line, _)| line.starts_with(prefix)).map(|(_, h)| h.clone()).collect()
    }
}

async fn read_request_head(socket: &mut tokio::net::TcpStream, buffer: &mut Vec<u8>) -> Option<String> {
    loop {
        if let Some(i) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..i]).to_string();
            buffer.drain(..i + 4);
            return Some(head);
        }
        let mut chunk = [0u8; 4096];
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
}

async fn proxy_connection(mut socket: tokio::net::TcpStream, log: ProxyLog) {
    let mut buffer = Vec::new();
    while let Some(head) = read_request_head(&mut socket, &mut buffer).await {
        let mut lines = head.split("\r\n").map(String::from);
        let line = lines.next().unwrap_or_default();
        let headers: Vec<String> = lines.collect();
        log.lock().push((line.clone(), headers.clone()));
        let mut parts = line.split(' ');
        let (method, target) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
        if method == "CONNECT" {
            let Some(port) = target.strip_prefix("127.0.0.1:") else { return };
            let Ok(mut upstream) =
                tokio::net::TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap_or(0))).await
            else {
                let _ = socket.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                return;
            };
            let _ = socket.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await;
            let _ = upstream.write_all(&buffer).await;
            let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
            return;
        }
        let Ok(url) = url::Url::parse(&target) else { return };
        if url.host_str() != Some("127.0.0.1") {
            return;
        }
        let length = headers
            .iter()
            .find_map(|h| h.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
            .unwrap_or(0usize);
        while buffer.len() < length {
            let mut chunk = [0u8; 4096];
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            }
        }
        let body: Vec<u8> = buffer.drain(..length).collect();
        let mut forwarded = format!("{method} {} HTTP/1.1\r\n", &url[url::Position::BeforePath..]);
        for h in &headers {
            let name = h.split(':').next().unwrap_or("").to_ascii_lowercase();
            if !name.starts_with("proxy-") && name != "connection" {
                forwarded.push_str(&format!("{h}\r\n"));
            }
        }
        forwarded.push_str("Connection: close\r\n\r\n");
        let Ok(mut upstream) = tokio::net::TcpStream::connect(("127.0.0.1", url.port().unwrap_or(80))).await else {
            return;
        };
        let _ = upstream.write_all(forwarded.as_bytes()).await;
        let _ = upstream.write_all(&body).await;
        let mut response = Vec::new();
        let _ = upstream.read_to_end(&mut response).await;
        if socket.write_all(&response).await.is_err() {
            return;
        }
    }
}

fn api_host(relay: &FakeRelay) -> String {
    format!("127.0.0.1:{}", relay.api.port().unwrap())
}

#[tokio::test]
async fn api_calls_the_certificate_check_and_the_bridge_go_through_the_proxy() {
    let relay = FakeRelay::start(Options::default()).await;
    let proxy = MockProxy::start().await;
    let dir = tempfile::tempdir().unwrap();
    let client = client_with(&relay, Storage::at(dir.path().join("opentunnel")), &[("HTTP_PROXY", &proxy.url)]);
    let target = target().await;
    let (events, sink) = recorder();
    let connection = client.connect("default", routes(&target), Some(sink)).await.unwrap();
    let hostname = connection.tunnel.hostname.clone();
    // The tunnel works through the proxied bridge.
    let response = public_request(&relay, &format!("{ROUTE}.{hostname}"), "/via", b"").await.unwrap();
    assert!(String::from_utf8_lossy(&response).ends_with("POST /via HTTP/1.1 0"));
    wait_for(&events, |e| matches!(e, Event::ConnectionClosed { .. })).await;
    connection.close().await;

    let api = format!("http://{}", api_host(&relay));
    let requests = proxy.requests();
    let id = &relay.tunnels()[0].0;
    for expected in [
        format!("POST {api}/api/tunnel HTTP/1.1"),
        format!("POST {api}/api/tunnel/{id}/certificate HTTP/1.1"),
        format!("GET {api}/api/tunnel/{id}/certificate HTTP/1.1"),
        format!("CONNECT {} HTTP/1.1", api_host(&relay)),
    ] {
        assert!(requests.contains(&expected), "{expected} not in {requests:?}");
    }
    // API calls carry the SDK's user agent, the certificate check Bun's, and every one tells the proxy to keep the
    // connection open as Bun does.
    let gets = proxy.headers(&format!("GET {api}/api/tunnel/{id}/certificate"));
    let agents: Vec<String> = gets
        .iter()
        .filter_map(|h| h.iter().find(|l| l.to_ascii_lowercase().starts_with("user-agent:")).cloned())
        .map(|l| l.split_once(':').unwrap().1.trim().to_string())
        .collect();
    assert!(agents.contains(&"opentunnel-sdk/0.6.1".to_string()), "{agents:?}");
    assert!(agents.contains(&"Bun/1.4.4".to_string()), "{agents:?}");
    assert!(gets.iter().all(|h| h.iter().any(|l| l.eq_ignore_ascii_case("proxy-connection: Keep-Alive"))));
    // The bridge's CONNECT is exactly Bun's.
    assert_eq!(
        proxy.headers("CONNECT")[0],
        vec![format!("Host: {}", api_host(&relay)), "Proxy-Connection: Keep-Alive".to_string()]
    );
    // The relay saw nothing directly: everything came through the proxy.
    assert_eq!(relay.log().iter().filter(|e| e["http"] == "GET /api/tunnel/:id/connect").count(), 1);
}

#[tokio::test]
async fn the_proxy_option_and_its_credentials() {
    let relay = FakeRelay::start(Options::default()).await;
    let proxy = MockProxy::start().await;
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::at(dir.path().join("opentunnel"));
    let with_credentials = proxy.url.replace("http://", "http://u:p%40x@");
    let options = ClientOptions { proxy: Some(with_credentials), env: Some(crate::proxy::env_of(&[])) };
    let client = Client::with_options(relay.api.clone(), storage.clone(), options).unwrap();
    let target = target().await;
    let connection = client.connect("default", routes(&target), None).await.unwrap();
    connection.close().await;
    let connect = proxy.headers("CONNECT");
    assert_eq!(connect.len(), 1);
    assert!(connect[0].contains(&"Proxy-Authorization: Basic dTpwQHg=".to_string()), "{connect:?}");
    assert!(proxy.requests().iter().any(|r| r.starts_with("POST http://127.0.0.1")));

    // Bun's NO_PROXY exempts hosts even from the explicit proxy.
    let before = proxy.requests().len();
    let options =
        ClientOptions { proxy: Some(proxy.url.clone()), env: Some(crate::proxy::env_of(&[("NO_PROXY", "127.0.0.1")])) };
    let client = Client::with_options(relay.api.clone(), storage, options).unwrap();
    let connection = client.connect("default", routes(&target), None).await.unwrap();
    connection.close().await;
    assert_eq!(proxy.requests().len(), before);

    // An unusable proxy option fails when the client is made.
    for (proxy, message) in [
        (
            "socks5://127.0.0.1:1080",
            "The proxy option uses an unsupported proxy scheme 'socks5': OpenTunnel connects through http:// and https:// proxies",
        ),
        ("http://[bad", "The proxy option is not a valid proxy URL"),
    ] {
        let options = ClientOptions { proxy: Some(proxy.into()), env: None };
        let error = Client::with_options(relay.api.clone(), Storage::at(dir.path().join("x")), options).err().unwrap();
        assert_eq!(error.to_string(), message);
        assert!(error.is_fatal());
    }
}

#[tokio::test]
async fn no_proxy_exempts_hosts_with_each_rule_set() {
    let relay = FakeRelay::start(Options::default()).await;
    let proxy = MockProxy::start().await;
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::at(dir.path().join("opentunnel"));
    let target = target().await;

    // Exempt for both: nothing goes through the proxy.
    let client = client_with(&relay, storage.clone(), &[("HTTP_PROXY", &proxy.url), ("NO_PROXY", "127.0.0.1")]);
    client.connect("default", routes(&target), None).await.unwrap().close().await;
    assert_eq!(proxy.requests(), Vec::<String>::new());

    // `no_proxy` with another port exempts nothing.
    let client = client_with(&relay, storage.clone(), &[("HTTP_PROXY", &proxy.url), ("no_proxy", "127.0.0.1:1")]);
    client.connect("default", routes(&target), None).await.unwrap().close().await;
    let requests = proxy.requests();
    assert!(requests.iter().any(|r| r.starts_with("GET http://")), "{requests:?}");
    assert!(requests.iter().any(|r| r.starts_with("CONNECT ")), "{requests:?}");

    // A CIDR range with a port: the SDK's rules (the bridge) exempt the host, Bun's (`fetch`) do not.
    proxy.log.lock().clear();
    let port = relay.api.port().unwrap();
    let no_proxy = format!("127.0.0.0/8:{port}");
    let client = client_with(&relay, storage, &[("HTTP_PROXY", &proxy.url), ("NO_PROXY", &no_proxy)]);
    client.connect("default", routes(&target), None).await.unwrap().close().await;
    let requests = proxy.requests();
    assert!(requests.iter().any(|r| r.starts_with("GET http://")), "{requests:?}");
    assert!(!requests.iter().any(|r| r.starts_with("CONNECT ")), "{requests:?}");
}

#[tokio::test]
async fn unusable_proxies_stop_the_tunnel() {
    let relay = FakeRelay::start(Options::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::at(dir.path().join("opentunnel"));
    let target = target().await;

    // Without a tunnel yet, creating it fails as a network error, which is retried by the caller.
    let socks = client_with(&relay, storage.clone(), &[("HTTP_PROXY", "socks5://127.0.0.1:1")]);
    let error = socks.connect("default", routes(&target), None).await.err().unwrap();
    assert_eq!(error.to_string(), format!("Failed to create tunnel: Transport error (POST {}api/tunnel)", relay.api));
    assert!(!error.is_fatal());
    assert!(relay.log().is_empty());
    test_client(&relay, storage.clone()).connect("default", routes(&target), None).await.unwrap().close().await;

    // With a tunnel whose certificate has not expired, an unreachable API is skipped and the bridge refuses the
    // proxy for good.
    let cases = [
        (
            ("HTTP_PROXY", "socks5://127.0.0.1:1"),
            "HTTP_PROXY uses an unsupported proxy scheme 'socks5': OpenTunnel connects through http:// and https:// proxies",
        ),
        (("http_proxy", "http://[bad"), "http_proxy is not a valid proxy URL"),
        (
            ("ALL_PROXY", "socks://127.0.0.1:1"),
            "ALL_PROXY uses an unsupported proxy scheme 'socks': OpenTunnel connects through http:// and https:// proxies",
        ),
    ];
    for ((name, value), message) in cases {
        let proxied = client_with(&relay, storage.clone(), &[(name, value)]);
        let (events, sink) = recorder();
        let error = proxied.connect("default", routes(&target), Some(sink)).await.err().unwrap();
        assert_eq!(error.to_string(), format!("Failed to connect tunnel: {message}"));
        assert!(error.is_fatal(), "{error}");
        assert_eq!(error.code(), None);
        let events = events.lock().clone();
        assert_eq!(events, vec![Event::Connecting { attempt: 0 }, Event::Stopped { error: Some(message.into()) }]);
    }
    // `HTTPS_PROXY` is only for https and wss URLs.
    let https_only = client_with(&relay, storage, &[("HTTPS_PROXY", "socks5://127.0.0.1:1")]);
    https_only.connect("default", routes(&target), None).await.unwrap().close().await;
}

#[tokio::test]
async fn api_failures_are_retried_and_described() {
    let relay = FakeRelay::start(Options::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::at(dir.path().join("opentunnel"));
    let client = test_client(&relay, storage.clone());
    let progress = Arc::new(Mutex::new(Vec::new()));
    let seen = progress.clone();
    let on = move |p: crate::Progress| seen.lock().push(p);
    client.ensure_with_progress("default", Some(&on)).await.unwrap();
    progress.lock().clear();
    client.ensure_with_progress("default", Some(&on)).await.unwrap();
    assert_eq!(*progress.lock(), vec![crate::Progress::CheckingCertificate, crate::Progress::Ready]);

    // Reads are retried on 5xx; once they still fail, a certificate that has not expired keeps serving.
    relay.clear_log();
    relay.options(|o| o.certificate_error = Some((500, "Whatever".into())));
    client.ensure("default").await.unwrap();
    assert_eq!(relay.log().iter().filter(|e| e["http"] == "GET /api/tunnel/:id/certificate").count(), 7);

    // An error the API answers with is neither retried nor skipped.
    relay.clear_log();
    relay.options(|o| o.certificate_error = Some((404, "TunnelNotFoundError".into())));
    let error = client.ensure("default").await.unwrap_err();
    assert_eq!(error.to_string(), "Failed to read certificate: TunnelNotFoundError: Fake failure");
    assert_eq!(relay.log().iter().filter(|e| e["http"] == "GET /api/tunnel/:id/certificate").count(), 1);
    // A status the endpoint does not declare is described by its status.
    relay.options(|o| o.certificate_error = Some((409, "Nope".into())));
    assert_eq!(client.ensure("default").await.unwrap_err().to_string(), "Failed to read certificate: HTTP 409");

    // Invalid routes are reported as the SDK does, with the message twice.
    let error = client.connect("default", crate::routes::routes([("Bad_Name", "127.0.0.1:1")]), None).await;
    assert_eq!(error.err().unwrap().to_string(), "Invalid route name 'Bad_Name': Invalid route name 'Bad_Name'");
}

#[tokio::test]
async fn route_conflicts_and_proxy_protocol_routes() {
    let relay = FakeRelay::start(Options::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let client = test_client(&relay, Storage::at(dir.path().join("opentunnel")));
    let target = target().await;

    // A PROXY protocol header reaches the target before the request.
    let mut with_header = Routes::new();
    with_header.insert(ROUTE.into(), crate::Route::with_proxy_protocol(target.clone(), "v1"));
    let connection = client.connect("default", with_header, None).await.unwrap();
    let hostname = connection.tunnel.hostname.clone();
    assert_eq!(connection.urls()[ROUTE], format!("https://{ROUTE}.{hostname}"));
    let response = public_request(&relay, &format!("{ROUTE}.{hostname}"), "/pp", b"").await.unwrap();
    let text = String::from_utf8_lossy(&response).to_string();
    assert!(text.contains("PROXY TCP4 127.0.0.1 0.0.0.0 ") && text.ends_with(" 443 0"), "{text}");
    connection.close().await;

    // `route_conflict` with `Fail` stops the tunnel after telling which routes were claimed.
    relay.options(|o| o.attach_error = Some("route_conflict".into()));
    let (events, sink) = recorder();
    let options = crate::ConnectOptions {
        routes: routes(&target),
        on_route_conflict: crate::OnRouteConflict::Fail,
        on_event: Some(sink),
    };
    let error = client.connect_with("default", options).await.err().unwrap();
    assert_eq!(
        error.to_string(),
        "Failed to connect tunnel: Bridge attach failed: route_conflict (another client holds a route)"
    );
    assert_eq!(error.code(), Some("route_conflict"));
    assert!(error.is_fatal());
    assert!(events.lock().contains(&Event::RouteConflict { routes: vec![ROUTE.into()] }));
    assert_eq!(
        serde_json::to_value(Event::RouteConflict { routes: vec![ROUTE.into()] }).unwrap(),
        json!({"type": "route-conflict", "routes": [ROUTE]})
    );

    // By default it retries.
    let (events, sink) = recorder();
    let pending = tokio::spawn({
        let client = client.clone();
        let target = target.clone();
        async move { client.connect("default", routes(&target), Some(sink)).await.map(|_| ()) }
    });
    wait_for(&events, |e| e == &Event::Disconnected { reason: "attach failed: route_conflict".into() }).await;
    wait_for(&events, |e| matches!(e, Event::Reconnecting { .. })).await;
    pending.abort();
}
