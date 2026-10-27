//! A local stand-in for the OpenTunnel service, for tests: the HTTP API, a certificate authority that signs the
//! tunnel's certificate request, the bridge WebSocket, and a public TLS listener that routes by SNI like the relay.
//! Every request and bridge message is recorded, so a client's traffic can be compared with the SDK's.
//!
//! Nothing here talks to the real service: it only listens on 127.0.0.1.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use url::Url;

/// Behaviour switches, changeable while running.
#[derive(Debug, Clone)]
pub struct Options {
    /// `GET certificate` answers `issuing` this many times before `ready`.
    pub issuing_polls: u32,
    /// Issuance ends in `{type: "failed", reason}`.
    pub fail_issuance: Option<String>,
    /// `POST /api/tunnel` fails with this status and `_tag`.
    pub create_error: Option<(u16, String)>,
    pub heartbeat_ms: u64,
    pub idle_timeout_ms: u64,
    /// Attach is refused with this code.
    pub attach_error: Option<String>,
    /// The bridge upgrade answers without a subprotocol.
    pub omit_subprotocol: bool,
    /// The bridge upgrade is refused with this HTTP status.
    pub refuse_upgrade: Option<u16>,
    /// Days the issued certificates are valid.
    pub validity_days: i64,
    /// Hostnames are `<random>.<suffix>`.
    pub hostname_suffix: String,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            issuing_polls: 1,
            fail_issuance: None,
            create_error: None,
            heartbeat_ms: 15_000,
            idle_timeout_ms: 45_000,
            attach_error: None,
            omit_subprotocol: false,
            refuse_upgrade: None,
            validity_days: 90,
            hostname_suffix: "fake-opentunnel.test".into(),
        }
    }
}

#[derive(Debug, Clone)]
struct TunnelRecord {
    id: String,
    hostname: String,
    token: String,
    certificate_id: Option<String>,
    /// `issuing` answers left before the certificate is ready.
    polls: u32,
    state: Value,
}

enum ToBridge {
    Text(String),
    Binary(Vec<u8>),
    Close(u16, String),
    Drop,
}

struct Bridge {
    #[allow(dead_code)]
    tunnel: String,
    hostname: String,
    tx: mpsc::UnboundedSender<ToBridge>,
    conns: Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<ToPublic>>>>,
}

enum ToPublic {
    Data(Vec<u8>),
    End,
    Reset,
}

struct Ca {
    cert: rcgen::Certificate,
    key: rcgen::KeyPair,
}

struct Inner {
    options: Mutex<Options>,
    tunnels: Mutex<HashMap<String, TunnelRecord>>,
    bridges: Mutex<Vec<Arc<Bridge>>>,
    log: Mutex<Vec<Value>>,
    ca: Ca,
    ca_pem: String,
    next: AtomicU32,
    next_conn: AtomicU32,
}

impl Inner {
    fn record(&self, entry: Value) {
        self.log.lock().push(entry);
    }

    fn id(&self, prefix: &str) -> String {
        format!("{prefix}_{:04}", self.next.fetch_add(1, Ordering::SeqCst))
    }
}

/// A running fake service.
#[derive(Clone)]
pub struct FakeRelay {
    /// The API base URL to give a client.
    pub api: Url,
    /// Where public TLS connections are accepted (routed by SNI).
    pub public: SocketAddr,
    inner: Arc<Inner>,
}

fn random_label(len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..len).map(|_| ALPHABET[rand::random_range(0..ALPHABET.len())] as char).collect()
}

fn error_response(status: u16, tag: &str, message: &str, extra: Value) -> Response {
    let mut body = json!({ "_tag": tag, "message": message });
    if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            b.insert(k.clone(), v.clone());
        }
    }
    (StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST), axum::Json(body)).into_response()
}

fn headers_json(headers: &HeaderMap) -> Value {
    let mut out = serde_json::Map::new();
    for (name, value) in headers {
        out.insert(name.as_str().to_string(), Value::String(value.to_str().unwrap_or("").to_string()));
    }
    Value::Object(out)
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(String::from)
}

// ---- DER helpers for reading the certificate request ----------------------------------------------------------

/// (tag, whole element, contents, rest)
type Element<'a> = (u8, &'a [u8], &'a [u8], &'a [u8]);

fn der_element(data: &[u8]) -> Option<Element<'_>> {
    let tag = *data.first()?;
    let first = *data.get(1)? as usize;
    let (len, header) = if first < 0x80 {
        (first, 2)
    } else {
        let n = first & 0x7f;
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *data.get(2 + i)? as usize;
        }
        (len, 2 + n)
    };
    let end = header.checked_add(len)?;
    if data.len() < end {
        return None;
    }
    Some((tag, &data[..end], &data[header..end], &data[end..]))
}

fn der_children(mut data: &[u8]) -> Vec<(u8, &[u8], &[u8])> {
    let mut out = Vec::new();
    while let Some((tag, whole, contents, rest)) = der_element(data) {
        out.push((tag, whole, contents));
        data = rest;
    }
    out
}

/// What the fake checks in a request: the subject CN, SANs, the EC point and whether the signature verifies.
#[derive(Debug, Clone, PartialEq)]
pub struct CsrInfo {
    pub common_name: String,
    pub dns_names: Vec<String>,
    pub point: Vec<u8>,
    pub signature_valid: bool,
    pub der: Vec<u8>,
}

pub fn parse_csr(pem: &str) -> Option<CsrInfo> {
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let der = base64::engine::general_purpose::STANDARD.decode(body.trim()).ok()?;
    let (_, _, outer, _) = der_element(&der)?;
    let parts = der_children(outer);
    let (_, info_whole, info) = *parts.first()?;
    let (_, _, signature_bits) = *parts.get(2)?;
    let fields = der_children(info);
    let subject = fields.get(1)?.2;
    let mut common_name = String::new();
    for (_, _, rdn) in der_children(subject) {
        for (_, _, attr) in der_children(rdn) {
            let kv = der_children(attr);
            if kv.len() == 2 && kv[0].2 == [0x55, 0x04, 0x03] {
                common_name = String::from_utf8_lossy(kv[1].2).into_owned();
            }
        }
    }
    let spki = der_children(fields.get(2)?.2);
    let point = spki.get(1)?.2.get(1..)?.to_vec();
    let mut dns_names = Vec::new();
    if let Some((0xa0, _, attributes)) = fields.get(3).copied() {
        for (_, _, attribute) in der_children(attributes) {
            let kv = der_children(attribute);
            for (_, _, extensions) in kv.get(1).map(|v| der_children(v.2)).unwrap_or_default() {
                for (_, _, extension) in der_children(extensions) {
                    let e = der_children(extension);
                    if e.len() >= 2 && e[0].2 == [0x55, 0x1d, 0x11] {
                        let names = der_children(e[e.len() - 1].2);
                        for (_, _, seq) in names {
                            for (tag, _, name) in der_children(seq) {
                                if tag == 0x82 {
                                    dns_names.push(String::from_utf8_lossy(name).into_owned());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let signature = signature_bits.get(1..)?;
    let key = ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, &point);
    let signature_valid = key.verify(info_whole, signature).is_ok();
    Some(CsrInfo { common_name, dns_names, point, signature_valid, der })
}

struct EcPoint(Vec<u8>);

impl rcgen::PublicKeyData for EcPoint {
    fn der_bytes(&self) -> &[u8] {
        &self.0
    }
    fn algorithm(&self) -> &rcgen::SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

fn ymd(date: chrono::DateTime<chrono::Utc>) -> (i32, u8, u8) {
    use chrono::Datelike;
    (date.year(), date.month() as u8, date.day() as u8)
}

impl FakeRelay {
    /// Starts the fake on 127.0.0.1 (two ephemeral ports).
    pub async fn start(options: Options) -> FakeRelay {
        let key = rcgen::KeyPair::generate().expect("ca key");
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("ca params");
        params.distinguished_name.push(rcgen::DnType::CommonName, "Fake OpenTunnel Test CA");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let cert = params.self_signed(&key).expect("ca cert");
        let ca_pem = cert.pem();
        let inner = Arc::new(Inner {
            options: Mutex::new(options),
            tunnels: Mutex::new(HashMap::new()),
            bridges: Mutex::new(Vec::new()),
            log: Mutex::new(Vec::new()),
            ca: Ca { cert, key },
            ca_pem,
            next: AtomicU32::new(1),
            next_conn: AtomicU32::new(1),
        });
        let app = Router::new()
            .route("/api/tunnel", post(create))
            .route("/api/tunnel/{id}", get(get_tunnel).delete(remove))
            .route("/api/tunnel/{id}/certificate", post(bind).get(certificate))
            .route("/api/tunnel/{id}/connect", get(connect))
            .route("/__fake/log", get(admin_log))
            .route("/__fake/{action}", post(admin))
            .with_state(inner.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind api");
        let api_addr = listener.local_addr().expect("api addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let public = TcpListener::bind("127.0.0.1:0").await.expect("bind public");
        let public_addr = public.local_addr().expect("public addr");
        let public_inner = inner.clone();
        tokio::spawn(async move {
            while let Ok((socket, peer)) = public.accept().await {
                tokio::spawn(public_connection(public_inner.clone(), socket, peer));
            }
        });
        FakeRelay { api: Url::parse(&format!("http://{api_addr}")).expect("url"), public: public_addr, inner }
    }

    /// The PEM of the CA that signs tunnel certificates (trust it to reach a tunnel's public address).
    pub fn ca_pem(&self) -> &str {
        &self.inner.ca_pem
    }

    /// Requests and bridge messages so far.
    pub fn log(&self) -> Vec<Value> {
        self.inner.log.lock().clone()
    }

    pub fn clear_log(&self) {
        self.inner.log.lock().clear();
    }

    pub fn options(&self, f: impl FnOnce(&mut Options)) {
        f(&mut self.inner.options.lock())
    }

    /// `(id, hostname, token)` of every tunnel created.
    pub fn tunnels(&self) -> Vec<(String, String, String)> {
        let mut out: Vec<_> =
            self.inner.tunnels.lock().values().map(|t| (t.id.clone(), t.hostname.clone(), t.token.clone())).collect();
        out.sort();
        out
    }

    /// Attached bridges.
    pub fn bridges(&self) -> usize {
        self.inner.bridges.lock().len()
    }

    fn send_all(&self, make: impl Fn() -> ToBridge) {
        for bridge in self.inner.bridges.lock().iter() {
            let _ = bridge.tx.send(make());
        }
    }

    pub fn drain(&self, reason: &str) {
        let text = json!({"type": "drain", "reason": reason}).to_string();
        self.send_all(|| ToBridge::Text(text.clone()));
    }

    pub fn ping(&self, time_sent: Value) {
        let text = json!({"type": "ping", "time_sent": time_sent}).to_string();
        self.send_all(|| ToBridge::Text(text.clone()));
    }

    /// Sends raw text to every bridge.
    pub fn send_text(&self, text: &str) {
        self.send_all(|| ToBridge::Text(text.to_string()));
    }

    pub fn close_bridges(&self, code: u16, reason: &str) {
        self.send_all(|| ToBridge::Close(code, reason.to_string()));
    }

    /// Drops every bridge's TCP connection without a close frame.
    pub fn drop_bridges(&self) {
        self.send_all(|| ToBridge::Drop);
    }

    /// Issues every tunnel a new certificate (a server-side renewal).
    pub fn renew(&self) {
        let mut tunnels = self.inner.tunnels.lock();
        for tunnel in tunnels.values_mut() {
            if let Some(point) = tunnel.state.get("_point").and_then(Value::as_str).map(String::from) {
                let point = hex_decode(&point);
                tunnel.state = issue(&self.inner, &tunnel.hostname, &point);
            }
        }
    }
}

fn hex_decode(text: &str) -> Vec<u8> {
    (0..text.len() / 2).filter_map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()).collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn issue(inner: &Inner, hostname: &str, point: &[u8]) -> Value {
    let days = inner.options.lock().validity_days;
    let mut params =
        rcgen::CertificateParams::new(vec![hostname.to_string(), format!("*.{hostname}")]).expect("cert params");
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(rcgen::DnType::CommonName, hostname);
    let now = chrono::Utc::now();
    let (y, m, d) = ymd(now - chrono::Duration::days(1));
    params.not_before = rcgen::date_time_ymd(y, m, d);
    let expiry = now + chrono::Duration::days(days);
    let (y, m, d) = ymd(expiry + chrono::Duration::days(1));
    params.not_after = rcgen::date_time_ymd(y, m, d);
    let serial: [u8; 8] = rand::random();
    params.serial_number = Some(rcgen::SerialNumber::from_slice(&serial));
    let cert = params.signed_by(&EcPoint(point.to_vec()), &inner.ca.cert, &inner.ca.key).expect("sign");
    json!({
        "type": "ready",
        "certificate": cert.pem(),
        "chain": inner.ca_pem,
        "expiry": crate::storage::iso(&expiry),
        "_point": hex_encode(point),
    })
}

/// The public view of a certificate state (without the fake's private fields).
fn public_state(state: &Value) -> Value {
    let mut state = state.clone();
    if let Some(o) = state.as_object_mut() {
        o.remove("_point");
    }
    state
}

async fn create(State(inner): State<Arc<Inner>>, headers: HeaderMap, body: Bytes) -> Response {
    inner.record(
        json!({"http": "POST /api/tunnel", "headers": headers_json(&headers), "body": String::from_utf8_lossy(&body)}),
    );
    if let Some((status, tag)) = inner.options.lock().create_error.clone() {
        return error_response(status, &tag, "Fake failure", json!({}));
    }
    let suffix = inner.options.lock().hostname_suffix.clone();
    let tunnel = TunnelRecord {
        id: inner.id("tun"),
        hostname: format!("{}.{suffix}", random_label(12)),
        token: format!("tok_{}", random_label(32)),
        certificate_id: None,
        polls: 0,
        state: Value::Null,
    };
    let response = json!({
        "tunnel": {"id": tunnel.id, "hostname": tunnel.hostname, "state": "offline"},
        "token": tunnel.token,
    });
    inner.tunnels.lock().insert(tunnel.id.clone(), tunnel);
    (StatusCode::CREATED, axum::Json(response)).into_response()
}

#[allow(clippy::result_large_err)]
fn authorize(inner: &Inner, id: &str, headers: &HeaderMap) -> Result<TunnelRecord, Response> {
    let tunnels = inner.tunnels.lock();
    let Some(tunnel) = tunnels.get(id) else {
        return Err(error_response(404, "TunnelNotFoundError", "Tunnel not found", json!({"tunnelID": id})));
    };
    if bearer(headers).as_deref() != Some(tunnel.token.as_str()) {
        return Err(error_response(401, "UnauthorizedError", "Invalid token", json!({})));
    }
    Ok(tunnel.clone())
}

async fn get_tunnel(State(inner): State<Arc<Inner>>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    inner.record(json!({"http": "GET /api/tunnel/:id", "id": id, "headers": headers_json(&headers)}));
    match authorize(&inner, &id, &headers) {
        Ok(t) => axum::Json(json!({"id": t.id, "hostname": t.hostname, "state": "offline"})).into_response(),
        Err(r) => r,
    }
}

async fn remove(State(inner): State<Arc<Inner>>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    inner.record(json!({"http": "DELETE /api/tunnel/:id", "id": id, "headers": headers_json(&headers)}));
    match authorize(&inner, &id, &headers) {
        Ok(_) => {
            inner.tunnels.lock().remove(&id);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(r) => r,
    }
}

async fn bind(State(inner): State<Arc<Inner>>, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let payload: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let csr = payload.get("csr").and_then(Value::as_str).unwrap_or("").to_string();
    let parsed = parse_csr(&csr);
    inner.record(json!({
        "http": "POST /api/tunnel/:id/certificate",
        "id": id,
        "headers": headers_json(&headers),
        "body_keys": payload.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()),
        "csr": parsed.as_ref().map(|c| json!({
            "cn": c.common_name,
            "dns": c.dns_names,
            "point_len": c.point.len(),
            "signature_valid": c.signature_valid,
            "pem_lines": csr.lines().map(str::len).collect::<Vec<_>>(),
            "der_len": c.der.len(),
        })),
    }));
    let tunnel = match authorize(&inner, &id, &headers) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let Some(parsed) = parsed.filter(|c| c.signature_valid) else {
        return error_response(400, "InvalidRequestError", "Invalid certificate request", json!({}));
    };
    if parsed.common_name != tunnel.hostname {
        return error_response(
            400,
            "InvalidHostnameError",
            "Hostname mismatch",
            json!({"provided": parsed.common_name, "expected": tunnel.hostname}),
        );
    }
    let mut tunnels = inner.tunnels.lock();
    let record = tunnels.get_mut(&id).expect("tunnel");
    if record.state.get("type").and_then(Value::as_str) == Some("issuing") {
        return error_response(
            409,
            "CertificateInProgressError",
            "Certificate issuance in progress",
            json!({"tunnelID": id}),
        );
    }
    let options = inner.options.lock().clone();
    let certificate_id = inner.id("cert");
    record.certificate_id = Some(certificate_id.clone());
    record.polls = options.issuing_polls;
    record.state = json!({"type": "issuing", "_point": hex_encode(&parsed.point)});
    (StatusCode::ACCEPTED, axum::Json(json!({"id": certificate_id, "state": {"type": "issuing"}}))).into_response()
}

async fn certificate(State(inner): State<Arc<Inner>>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    inner.record(json!({"http": "GET /api/tunnel/:id/certificate", "id": id, "headers": headers_json(&headers)}));
    if let Err(r) = authorize(&inner, &id, &headers) {
        return r;
    }
    let (fail, polls) = {
        let options = inner.options.lock();
        (options.fail_issuance.clone(), options.issuing_polls)
    };
    let mut tunnels = inner.tunnels.lock();
    let record = tunnels.get_mut(&id).expect("tunnel");
    let Some(certificate_id) = record.certificate_id.clone() else {
        return error_response(404, "CertificateNotFoundError", "No certificate", json!({"tunnelID": id}));
    };
    if record.state.get("type").and_then(Value::as_str) == Some("issuing") {
        if record.polls > 0 && polls > 0 {
            record.polls -= 1;
        } else if let Some(reason) = fail {
            record.state = json!({"type": "failed", "reason": reason});
        } else {
            let point = hex_decode(record.state.get("_point").and_then(Value::as_str).unwrap_or(""));
            record.state = issue(&inner, &record.hostname, &point);
        }
    }
    axum::Json(json!({"id": certificate_id, "state": public_state(&record.state)})).into_response()
}

async fn connect(
    State(inner): State<Arc<Inner>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    inner.record(json!({"http": "GET /api/tunnel/:id/connect", "id": id, "headers": headers_json(&headers)}));
    let options = inner.options.lock().clone();
    if let Some(status) = options.refuse_upgrade {
        return error_response(status, "TunnelNotFoundError", "Refused", json!({"tunnelID": id}));
    }
    let ws = if options.omit_subprotocol { ws } else { ws.protocols(["opentunnel"]) };
    ws.on_upgrade(move |socket| bridge(inner, id, socket))
}

fn describe(message: &Message) -> Value {
    match message {
        Message::Text(t) => serde_json::from_str::<Value>(t.as_str()).unwrap_or_else(|_| json!({"raw": t.as_str()})),
        Message::Binary(b) => {
            let conn = if b.len() >= 4 { Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]])) } else { None };
            json!({"binary": b.len(), "conn": conn})
        }
        Message::Close(c) => {
            json!({"close": c.as_ref().map(|c| json!({"code": c.code, "reason": c.reason.as_str()}))})
        }
        Message::Ping(_) => json!({"ws_ping": true}),
        Message::Pong(_) => json!({"ws_pong": true}),
    }
}

async fn bridge(inner: Arc<Inner>, id: String, mut socket: WebSocket) {
    let (tx, mut rx) = mpsc::unbounded_channel::<ToBridge>();
    let conns: Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<ToPublic>>>> = Arc::default();
    let mut registered: Option<Arc<Bridge>> = None;
    loop {
        tokio::select! {
            command = rx.recv() => {
                let Some(command) = command else { break };
                let message = match command {
                    ToBridge::Text(t) => Message::Text(t.into()),
                    ToBridge::Binary(b) => Message::Binary(b.into()),
                    ToBridge::Close(code, reason) => {
                        inner.record(json!({"to_client": {"close": {"code": code, "reason": reason}}}));
                        let _ = socket.send(Message::Close(Some(CloseFrame { code, reason: reason.into() }))).await;
                        break;
                    }
                    ToBridge::Drop => {
                        inner.record(json!({"to_client": "drop"}));
                        break;
                    }
                };
                if !matches!(message, Message::Binary(_)) {
                    inner.record(json!({"to_client": describe(&message)}));
                }
                if socket.send(message).await.is_err() {
                    break;
                }
            }
            message = socket.recv() => {
                let Some(Ok(message)) = message else {
                    inner.record(json!({"from_client": "eof"}));
                    break;
                };
                let entry = describe(&message);
                let value: Value = match &message {
                    Message::Text(t) => serde_json::from_str(t.as_str()).unwrap_or(Value::Null),
                    _ => Value::Null,
                };
                if !matches!(message, Message::Binary(_)) {
                    inner.record(json!({"from_client": entry}));
                }
                match message {
                    Message::Close(_) => break,
                    Message::Binary(b) if b.len() >= 4 => {
                        let conn = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                        if let Some(public) = conns.lock().get(&conn) {
                            let _ = public.send(ToPublic::Data(b[4..].to_vec()));
                        }
                    }
                    Message::Text(_) => match value.get("type").and_then(Value::as_str) {
                        Some("attach") if registered.is_none() => {
                            let options = inner.options.lock().clone();
                            let tunnel = inner.tunnels.lock().get(&id).cloned();
                            let token = value.get("token").and_then(Value::as_str);
                            let reply = match (&tunnel, &options.attach_error) {
                                (_, Some(code)) => json!({"type": "attach_error", "code": code}),
                                (None, _) => json!({"type": "attach_error", "code": "tunnel_not_bound"}),
                                (Some(t), _) if Some(t.token.as_str()) != token => json!({"type": "attach_error", "code": "bad_token"}),
                                (Some(t), _) if t.state.get("type").and_then(Value::as_str) != Some("ready") => {
                                    json!({"type": "attach_error", "code": "cert_not_ready"})
                                }
                                (Some(_), _) => json!({
                                    "type": "attached",
                                    "session": inner.id("ses"),
                                    "routes": value.get("routes").cloned().unwrap_or(json!([])),
                                    "heartbeat_ms": options.heartbeat_ms,
                                    "idle_timeout_ms": options.idle_timeout_ms,
                                }),
                            };
                            inner.record(json!({"to_client": reply}));
                            if socket.send(Message::Text(reply.to_string().into())).await.is_err() {
                                break;
                            }
                            if reply["type"] == "attached" {
                                let tunnel = tunnel.expect("tunnel");
                                let entry = Arc::new(Bridge { tunnel: tunnel.id.clone(), hostname: tunnel.hostname.clone(), tx: tx.clone(), conns: conns.clone() });
                                inner.bridges.lock().push(entry.clone());
                                registered = Some(entry);
                            }
                        }
                        Some("ping") => {
                            let reply = json!({"type": "pong", "time_sent": value["time_sent"]});
                            inner.record(json!({"to_client": reply}));
                            let _ = socket.send(Message::Text(reply.to_string().into())).await;
                        }
                        Some("end") => {
                            let conn = value["conn"].as_u64().unwrap_or(0) as u32;
                            if let Some(public) = conns.lock().get(&conn) {
                                let _ = public.send(ToPublic::End);
                            }
                        }
                        Some("reset") => {
                            let conn = value["conn"].as_u64().unwrap_or(0) as u32;
                            if let Some(public) = conns.lock().remove(&conn) {
                                let _ = public.send(ToPublic::Reset);
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
    }
    if let Some(entry) = registered {
        inner.bridges.lock().retain(|b| !Arc::ptr_eq(b, &entry));
    }
    for (_, public) in conns.lock().drain() {
        let _ = public.send(ToPublic::Reset);
    }
}

/// The SNI in a TLS ClientHello record, or `None` if there is none (or more bytes are needed).
pub fn client_hello_sni(data: &[u8]) -> Option<String> {
    if data.len() < 5 || data[0] != 22 {
        return None;
    }
    let record_len = u16::from_be_bytes([data[3], data[4]]) as usize;
    let record = data.get(5..5 + record_len)?;
    if record.first() != Some(&1) {
        return None;
    }
    let mut i = 4 + 2 + 32;
    let session = *record.get(i)? as usize;
    i += 1 + session;
    let suites = u16::from_be_bytes([*record.get(i)?, *record.get(i + 1)?]) as usize;
    i += 2 + suites;
    let compression = *record.get(i)? as usize;
    i += 1 + compression;
    let ext_len = u16::from_be_bytes([*record.get(i)?, *record.get(i + 1)?]) as usize;
    i += 2;
    let end = i + ext_len;
    while i + 4 <= end {
        let kind = u16::from_be_bytes([record[i], record[i + 1]]);
        let len = u16::from_be_bytes([record[i + 2], record[i + 3]]) as usize;
        let body = record.get(i + 4..i + 4 + len)?;
        if kind == 0 && body.len() >= 5 {
            let name_len = u16::from_be_bytes([body[3], body[4]]) as usize;
            return Some(String::from_utf8_lossy(body.get(5..5 + name_len)?).into_owned());
        }
        i += 4 + len;
    }
    None
}

async fn public_connection(inner: Arc<Inner>, mut socket: tokio::net::TcpStream, peer: SocketAddr) {
    let mut first = Vec::new();
    let sni = loop {
        let mut chunk = [0u8; 4096];
        let n = match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        first.extend_from_slice(&chunk[..n]);
        if first.len() >= 5 {
            let need = 5 + u16::from_be_bytes([first[3], first[4]]) as usize;
            if first.len() >= need {
                break client_hello_sni(&first);
            }
        }
        if first.len() > 64 * 1024 {
            return;
        }
    };
    let Some(sni) = sni else { return };
    let bridge = {
        let bridges = inner.bridges.lock();
        bridges
            .iter()
            .rev()
            .find(|b| {
                let sni = sni.to_lowercase();
                sni == b.hostname || sni.ends_with(&format!(".{}", b.hostname))
            })
            .cloned()
    };
    let Some(bridge) = bridge else {
        inner.record(json!({"public": "no bridge", "sni": sni}));
        return;
    };
    let conn = inner.next_conn.fetch_add(1, Ordering::SeqCst);
    let (tx, mut rx) = mpsc::unbounded_channel::<ToPublic>();
    bridge.conns.lock().insert(conn, tx);
    let open = json!({"type": "open", "conn": conn, "peer": peer.to_string(), "sni": sni, "alpn": ""});
    let _ = bridge.tx.send(ToBridge::Text(open.to_string()));
    let mut frame = conn.to_be_bytes().to_vec();
    frame.extend_from_slice(&first);
    let _ = bridge.tx.send(ToBridge::Binary(frame));
    let (mut read, mut write) = socket.into_split();
    let to_bridge = bridge.tx.clone();
    let reader = tokio::spawn(async move {
        let mut buffer = vec![0u8; 16 * 1024];
        loop {
            match read.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut frame = conn.to_be_bytes().to_vec();
                    frame.extend_from_slice(&buffer[..n]);
                    if to_bridge.send(ToBridge::Binary(frame)).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = to_bridge.send(ToBridge::Text(json!({"type": "end", "conn": conn}).to_string()));
    });
    while let Some(message) = rx.recv().await {
        match message {
            ToPublic::Data(bytes) => {
                if write.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            ToPublic::End => {
                let _ = write.shutdown().await;
            }
            ToPublic::Reset => {
                reader.abort();
                return;
            }
        }
    }
    let _ = reader.await;
}

async fn admin_log(State(inner): State<Arc<Inner>>) -> Response {
    axum::Json(Value::Array(inner.log.lock().clone())).into_response()
}

async fn admin(State(inner): State<Arc<Inner>>, Path(action): Path<String>, body: Bytes) -> Response {
    let relay = FakeRelay {
        api: Url::parse("http://127.0.0.1/").unwrap(),
        public: ([127, 0, 0, 1], 0).into(),
        inner: inner.clone(),
    };
    let payload: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    match action.as_str() {
        "drain" => relay.drain(payload.get("reason").and_then(Value::as_str).unwrap_or("maintenance")),
        "ping" => relay.ping(payload.get("time_sent").cloned().unwrap_or(json!(1))),
        "text" => relay.send_text(payload.get("text").and_then(Value::as_str).unwrap_or("")),
        "close" => relay.close_bridges(
            payload.get("code").and_then(Value::as_u64).unwrap_or(1000) as u16,
            payload.get("reason").and_then(Value::as_str).unwrap_or(""),
        ),
        "drop" => relay.drop_bridges(),
        "renew" => relay.renew(),
        "clear" => relay.clear_log(),
        "options" => relay.options(|o| {
            if let Some(v) = payload.get("issuing_polls").and_then(Value::as_u64) {
                o.issuing_polls = v as u32;
            }
            if let Some(v) = payload.get("fail_issuance") {
                o.fail_issuance = v.as_str().map(String::from);
            }
            if let Some(v) = payload.get("attach_error") {
                o.attach_error = v.as_str().map(String::from);
            }
            if let Some(v) = payload.get("omit_subprotocol").and_then(Value::as_bool) {
                o.omit_subprotocol = v;
            }
            if let Some(v) = payload.get("refuse_upgrade") {
                o.refuse_upgrade = v.as_u64().map(|s| s as u16);
            }
            if let Some(v) = payload.get("heartbeat_ms").and_then(Value::as_u64) {
                o.heartbeat_ms = v;
            }
            if let Some(v) = payload.get("idle_timeout_ms").and_then(Value::as_u64) {
                o.idle_timeout_ms = v;
            }
            if let Some(v) = payload.get("validity_days").and_then(Value::as_i64) {
                o.validity_days = v;
            }
            if let Some(v) = payload.get("create_error") {
                o.create_error =
                    v.as_array().and_then(|a| Some((a.first()?.as_u64()? as u16, a.get(1)?.as_str()?.to_string())));
            }
        }),
        "state" => {
            let tunnels: Vec<Value> = inner
                .tunnels
                .lock()
                .values()
                .map(|t| json!({"id": t.id, "hostname": t.hostname, "state": t.state.get("type")}))
                .collect();
            return axum::Json(json!({"tunnels": tunnels, "bridges": inner.bridges.lock().len()})).into_response();
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    }
    StatusCode::NO_CONTENT.into_response()
}
