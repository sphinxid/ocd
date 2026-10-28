//! The Responses WebSocket client against local servers: CONNECT proxy tunnelling (credentials, `NO_PROXY`
//! bypass, refused tunnels, rejected proxy URLs), the upgrade request Bun 1.4.4 sends, and permessage-deflate in
//! both directions.
//!
//! The target is `127.0.0.2` (never exempt from proxying like `127.0.0.1`), which Linux routes to loopback.
#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use base64::Engine;
use oc_ai::route::Headers;
use oc_ai::route::websocket::{self, WsMessage};

const TRAILER: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

fn env(pairs: &[(&str, String)]) -> impl Fn(&str) -> Option<String> + Sync + use<> {
    let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    move |name: &str| map.get(name).cloned()
}

fn read_head(stream: &mut impl Read) -> Option<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        head.push(byte[0]);
    }
    Some(String::from_utf8_lossy(&head).into_owned())
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// A logging CONNECT proxy; `require` answers 407 unless `Proxy-Authorization` equals it.
fn proxy(require: Option<&'static str>) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let seen = seen.clone();
            std::thread::spawn(move || {
                let mut client = client;
                let Some(head) = read_head(&mut client) else { return };
                seen.lock().unwrap().push(head.clone());
                if require.is_some() && header(&head, "proxy-authorization") != require {
                    let _ = client.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n");
                    return;
                }
                let target = head.split_whitespace().nth(1).unwrap().to_string();
                let upstream = TcpStream::connect(&target).unwrap();
                client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").unwrap();
                let (mut a, mut b) = (client.try_clone().unwrap(), upstream.try_clone().unwrap());
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut a, &mut b);
                });
                let (mut b, mut a) = (upstream, client);
                let _ = std::io::copy(&mut b, &mut a);
            });
        }
    });
    (port, log)
}

/// A WebSocket server on 127.0.0.2: records each upgrade request, answers `extensions` (if any), and echoes every
/// text message back as `echo:<text>` (compressed when deflate was negotiated). Records whether each inbound
/// message had RSV1 set.
struct Server {
    port: u16,
    upgrades: Arc<Mutex<Vec<String>>>,
    compressed: Arc<Mutex<Vec<bool>>>,
}

fn server(extensions: Option<&'static str>) -> Server {
    let listener = TcpListener::bind("127.0.0.2:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (upgrades, compressed) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())));
    let (u, c) = (upgrades.clone(), compressed.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (u, c) = (u.clone(), c.clone());
            std::thread::spawn(move || serve(stream, extensions, u, c));
        }
    });
    Server { port, upgrades, compressed }
}

fn serve(mut stream: TcpStream, extensions: Option<&str>, upgrades: Arc<Mutex<Vec<String>>>, compressed: Arc<Mutex<Vec<bool>>>) {
    let Some(head) = read_head(&mut stream) else { return };
    upgrades.lock().unwrap().push(head.clone());
    let key = header(&head, "sec-websocket-key").unwrap().to_string();
    let accept = oc_ai::route::websocket::client::accept_key(&key);
    let mut response = format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n");
    if let Some(ext) = extensions {
        response.push_str(&format!("Sec-WebSocket-Extensions: {ext}\r\n"));
    }
    response.push_str("\r\n");
    stream.write_all(response.as_bytes()).unwrap();
    let deflate = extensions.is_some();
    let mut inflate = flate2::Decompress::new(false);
    let mut deflater = flate2::Compress::new(flate2::Compression::default(), false);
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    loop {
        let mut head = [0u8; 2];
        if reader.read_exact(&mut head).is_err() {
            return;
        }
        let (rsv1, opcode) = (head[0] & 0x40 != 0, head[0] & 0x0F);
        let mut len = (head[1] & 0x7F) as usize;
        if len == 126 {
            let mut b = [0u8; 2];
            reader.read_exact(&mut b).unwrap();
            len = u16::from_be_bytes(b) as usize;
        } else if len == 127 {
            let mut b = [0u8; 8];
            reader.read_exact(&mut b).unwrap();
            len = u64::from_be_bytes(b) as usize;
        }
        let mut mask = [0u8; 4];
        reader.read_exact(&mut mask).unwrap();
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload).unwrap();
        payload.iter_mut().enumerate().for_each(|(i, b)| *b ^= mask[i % 4]);
        if opcode == 8 {
            return;
        }
        compressed.lock().unwrap().push(rsv1);
        let text = if rsv1 {
            payload.extend_from_slice(&TRAILER);
            let mut out = Vec::with_capacity(1 << 20);
            inflate.decompress_vec(&payload, &mut out, flate2::FlushDecompress::Sync).unwrap();
            String::from_utf8(out).unwrap()
        } else {
            String::from_utf8(payload).unwrap()
        };
        let reply = format!("echo:{text}");
        let (rsv, body) = if deflate {
            let mut out = Vec::with_capacity(reply.len() + 64);
            deflater.compress_vec(reply.as_bytes(), &mut out, flate2::FlushCompress::Sync).unwrap();
            out.truncate(out.len() - 4);
            (0x40, out)
        } else {
            (0, reply.into_bytes())
        };
        let mut frame = vec![0x80 | rsv | 0x1];
        if body.len() < 126 {
            frame.push(body.len() as u8);
        } else {
            frame.push(126);
            frame.extend_from_slice(&(body.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(&body);
        stream.write_all(&frame).unwrap();
    }
}

fn headers() -> Headers {
    [("authorization", "Bearer k"), ("openai-beta", "responses_websockets=2026-02-06")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

async fn round_trip(connection: &websocket::WebSocketConnection, text: &str) -> String {
    let mut messages = connection.take_messages().unwrap();
    connection.send_text(text.to_string()).await.unwrap();
    match tokio::time::timeout(std::time::Duration::from_secs(10), messages.recv()).await.unwrap() {
        Some(Ok(WsMessage::Text(reply))) => reply,
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn tunnels_through_the_proxy_with_credentials() {
    let server = server(None);
    let (proxy_port, log) = proxy(Some("Basic dXNAZXI6cDpzcw=="));
    let url = format!("ws://127.0.0.2:{}/v1/responses", server.port);
    let env = env(&[("HTTP_PROXY", format!("http://us%40er:p%3Ass@127.0.0.1:{proxy_port}"))]);
    let connection = websocket::open_with_env(&url, &headers(), &env).await.unwrap();
    assert_eq!(round_trip(&connection, "hi").await, "echo:hi");
    let log = log.lock().unwrap().clone();
    let port = server.port;
    assert_eq!(
        log,
        [format!(
            "CONNECT 127.0.0.2:{port} HTTP/1.1\r\nHost: 127.0.0.2:{port}\r\nProxy-Connection: Keep-Alive\r\nProxy-Authorization: Basic dXNAZXI6cDpzcw==\r\n\r\n"
        )]
    );
    // The upgrade request inside the tunnel, as Bun 1.4.4 writes it.
    let upgrade = server.upgrades.lock().unwrap()[0].clone();
    let key = header(&upgrade, "sec-websocket-key").unwrap().to_string();
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(&key).unwrap().len(), 16);
    assert_eq!(
        upgrade,
        format!(
            "GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.2:{port}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\nSec-WebSocket-Key: {key}\r\n\
             Authorization: Bearer k\r\nopenai-beta: responses_websockets=2026-02-06\r\n\r\n"
        )
    );
    connection.close_and_wait().await;
}

#[tokio::test]
async fn proxy_selection_and_no_proxy_bypass() {
    let server = server(None);
    let (proxy_port, log) = proxy(None);
    let url = format!("ws://127.0.0.2:{}/v1/responses", server.port);
    let proxy_url = format!("http://127.0.0.1:{proxy_port}");
    let cases: Vec<(Vec<(&str, String)>, bool)> = vec![
        (vec![("WS_PROXY", proxy_url.clone())], true),
        (vec![("all_proxy", proxy_url.clone())], true),
        // `HTTPS_PROXY` is for `wss:` sockets only.
        (vec![("HTTPS_PROXY", proxy_url.clone())], false),
        // An empty `WS_PROXY` stops the search.
        (vec![("WS_PROXY", String::new()), ("HTTP_PROXY", proxy_url.clone())], false),
        // `NO_PROXY`: upstream's exact and `.suffix` rules, then Bun's CIDR rule on `no_proxy`.
        (vec![("HTTP_PROXY", proxy_url.clone()), ("NO_PROXY", "127.0.0.2".into())], false),
        (vec![("HTTP_PROXY", proxy_url.clone()), ("NO_PROXY", ".0.0.2".into())], false),
        (vec![("HTTP_PROXY", proxy_url.clone()), ("no_proxy", "127.0.0.0/8".into())], false),
        (vec![("HTTP_PROXY", proxy_url.clone()), ("NO_PROXY", "127.0.0.2:1".into())], true),
    ];
    for (pairs, proxied) in cases {
        let before = log.lock().unwrap().len();
        let connection = websocket::open_with_env(&url, &headers(), &env(&pairs)).await.unwrap();
        assert_eq!(round_trip(&connection, "x").await, "echo:x");
        connection.close_and_wait().await;
        assert_eq!(log.lock().unwrap().len() - before, usize::from(proxied), "{pairs:?}");
    }
}

#[tokio::test]
async fn refused_tunnels_and_rejected_proxy_urls_fail_the_connect() {
    let server = server(None);
    let (proxy_port, _) = proxy(Some("Basic eDp5"));
    let url = format!("ws://127.0.0.2:{}/v1/responses", server.port);
    let error = websocket::open_with_env(&url, &headers(), &env(&[("WS_PROXY", format!("http://127.0.0.1:{proxy_port}"))])).await.err().unwrap();
    assert_eq!(
        error.message,
        format!("Failed to open WebSocket: WebSocket connection to '{url}' failed: Proxy authentication required: 407 Proxy Authentication Required")
    );
    let error = websocket::open_with_env(&url, &headers(), &env(&[("WS_PROXY", "socks5://127.0.0.1:1".into())])).await.err().unwrap();
    assert_eq!(error.message, "Unsupported proxy protocol \"socks5\" (expected \"http\" or \"https\")");
    let error = websocket::open_with_env(&url, &headers(), &env(&[("WS_PROXY", "127.0.0.1:1".into())])).await.err().unwrap();
    assert_eq!(error.message, "Invalid proxy URL: 127.0.0.1:1");
}

#[tokio::test]
async fn permessage_deflate_round_trip() {
    let server = server(Some("permessage-deflate"));
    let url = format!("ws://127.0.0.2:{}/v1/responses", server.port);
    let connection = websocket::open_with_env(&url, &headers(), &env(&[])).await.unwrap();
    assert!(connection.deflate().is_some());
    let mut messages = connection.take_messages().unwrap();
    let large = format!("{{\"type\":\"response.create\",\"input\":\"{}\"}}", "lorem ipsum ".repeat(200));
    for text in [large.as_str(), "short", large.as_str()] {
        connection.send_text(text.to_string()).await.unwrap();
        match tokio::time::timeout(std::time::Duration::from_secs(10), messages.recv()).await.unwrap() {
            Some(Ok(WsMessage::Text(reply))) => assert_eq!(reply, format!("echo:{text}")),
            other => panic!("unexpected {other:?}"),
        }
    }
    // Messages of 860 bytes or more go out compressed (context takeover across them), shorter ones plain.
    assert_eq!(*server.compressed.lock().unwrap(), [true, false, true]);
    connection.close_and_wait().await;

    // Without the server's consent nothing is compressed.
    let plain = server_without_deflate().await;
    assert_eq!(plain, [false]);
}

async fn server_without_deflate() -> Vec<bool> {
    let server = server(None);
    let url = format!("ws://127.0.0.2:{}/", server.port);
    let connection = websocket::open_with_env(&url, &Headers::new(), &env(&[])).await.unwrap();
    assert!(connection.deflate().is_none());
    assert_eq!(round_trip(&connection, &"z".repeat(2000)).await, format!("echo:{}", "z".repeat(2000)));
    connection.close_and_wait().await;
    server.compressed.lock().unwrap().clone()
}

#[tokio::test]
async fn invalid_extension_answer_fails_the_upgrade() {
    let server = server(Some("permessage-deflate; client_max_window_bits"));
    let url = format!("ws://127.0.0.2:{}/v1/responses", server.port);
    let error = websocket::open_with_env(&url, &headers(), &env(&[])).await.err().unwrap();
    assert_eq!(error.message, format!("Failed to open WebSocket: WebSocket connection to '{url}' failed: Unsupported websocket extension"));
}
