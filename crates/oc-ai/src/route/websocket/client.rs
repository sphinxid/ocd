//! The WebSocket client of upstream's runtime: Bun 1.4.4's global `WebSocket`, constructed the way
//! `core/src/effect/websocket-constructor.ts` does it (handshake headers, optional `proxy`).
//!
//! - Connection: direct, or through an HTTP `CONNECT` tunnel ([`super::proxy`]): `CONNECT host:port HTTP/1.1`,
//!   `Host: host:port`, `Proxy-Connection: Keep-Alive` and `Proxy-Authorization` from the proxy URL's credentials;
//!   any 2xx answer opens the tunnel. An `https:` proxy gets TLS first (to the proxy's name, with the same trust);
//!   a `wss:` target then gets its own TLS inside the tunnel.
//! - Upgrade request (`build_request_body`): `GET <path> HTTP/1.1`, `Host` (port only when not the default),
//!   `Connection: Upgrade`, `Upgrade: websocket`, `Sec-WebSocket-Version: 13`,
//!   `Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits`, `Sec-WebSocket-Key`, then the caller's
//!   headers.
//! - Upgrade response (`validate_upgrade`): `101`, `Connection` listing `upgrade`, `Upgrade: websocket`, the
//!   expected `Sec-WebSocket-Accept`, `Sec-WebSocket-Version` 13 when present, no `Sec-WebSocket-Protocol`, and at
//!   most one `Sec-WebSocket-Extensions`, which must be a valid `permessage-deflate` answer (RFC 7692).
//! - permessage-deflate (`WebSocketDeflate.rs`): inbound messages with RSV1 are inflated (raw DEFLATE, the
//!   `00 00 ff ff` tail restored, the window reset after a final block or with `server_no_context_takeover`);
//!   outbound text of 860 bytes or more is compressed (level 6, the negotiated `client_max_window_bits`, at least 9,
//!   sync flush with the tail removed, reset with `client_no_context_takeover`).
//! - Frames: client frames are masked; a ping is answered with a pong; messages are limited to 128 MiB (after
//!   inflation). Protocol errors fail the socket like Bun's (`WebSocket connection to '<url>' failed: <reason>`).

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use base64::Engine;
use sha1::Digest;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

use super::proxy::{self, ProxyConfig};
use crate::route::Headers;

/// The extension offer of Bun's `WebSocket` (`WebSocketOptions::extensions`).
pub const EXTENSIONS_OFFER: &str = "permessage-deflate; client_max_window_bits";
/// Bun's `MIN_COMPRESS_SIZE`: shorter messages are sent uncompressed.
pub const MIN_COMPRESS_SIZE: usize = 860;
/// Bun's `WebSocketConnectedOptions.max_payload_length` for the global `WebSocket`.
pub const MAX_PAYLOAD: usize = 128 * 1024 * 1024;
/// Largest response head accepted (Bun: `--max-http-header-size` 16 KiB + 1 KiB).
const MAX_HEAD: usize = 16 * 1024 + 1024;
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub(crate) type BoxIo = Box<dyn Io>;

/// A failed connection attempt: the text after `WebSocket connection to '<url>' failed: `, or a constructor
/// exception (thrown before any connection).
#[derive(Debug)]
pub(crate) enum ConnectFailure {
    /// `new WebSocket(...)` threw (a rejected proxy URL).
    Thrown(String),
    /// The error event of a socket that never opened.
    Failed(String),
}

/// Negotiated permessage-deflate parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeflateParams {
    pub server_max_window_bits: u8,
    pub client_max_window_bits: u8,
    pub server_no_context_takeover: bool,
    pub client_no_context_takeover: bool,
}

impl Default for DeflateParams {
    fn default() -> Self {
        Self { server_max_window_bits: 15, client_max_window_bits: 15, server_no_context_takeover: false, client_no_context_takeover: false }
    }
}

/// An upgraded connection.
pub(crate) struct Upgraded {
    pub io: BoxIo,
    /// Bytes read after the upgrade response (the first frames).
    pub leftover: Vec<u8>,
    pub deflate: Option<DeflateParams>,
}

/// `parseDeflateResponse`: the negotiated parameters of an answer to [`EXTENSIONS_OFFER`], `None` for another
/// extension, an unknown or repeated parameter, or a missing or out-of-range window size.
pub fn parse_deflate_response(response: &str) -> Option<DeflateParams> {
    let mut parts = response.split(';').map(|p| p.trim_matches([' ', '\t']));
    if parts.next()? != "permessage-deflate" {
        return None;
    }
    let mut params = DeflateParams::default();
    let (mut server_bits, mut client_bits) = (None, None);
    for part in parts {
        let (key, bits) = match part.split_once('=') {
            Some((key, value)) => (key, Some(parse_window_bits(value)?)),
            None => (part, None),
        };
        match (key, bits) {
            ("server_no_context_takeover", None) if !params.server_no_context_takeover => params.server_no_context_takeover = true,
            ("client_no_context_takeover", None) if !params.client_no_context_takeover => params.client_no_context_takeover = true,
            ("server_max_window_bits", Some(_)) if server_bits.is_none() => server_bits = bits,
            ("client_max_window_bits", Some(_)) if client_bits.is_none() => client_bits = bits,
            _ => return None,
        }
    }
    params.server_max_window_bits = server_bits.unwrap_or(params.server_max_window_bits);
    params.client_max_window_bits = client_bits.unwrap_or(params.client_max_window_bits);
    Some(params)
}

fn parse_window_bits(value: &str) -> Option<u8> {
    let digits = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value).as_bytes();
    let bits = match *digits {
        [d @ b'0'..=b'9'] => d - b'0',
        [b'1', d @ b'0'..=b'9'] => 10 + (d - b'0'),
        _ => return None,
    };
    (8..=15).contains(&bits).then_some(bits)
}

/// `Sec-WebSocket-Accept` for a key.
pub fn accept_key(key: &str) -> String {
    let mut sha = sha1::Sha1::new();
    sha.update(key.as_bytes());
    sha.update(GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(sha.finalize())
}

/// `HostFormatter`: the host with `:port` unless it is the scheme's default.
fn host_header(host: &str, port: u16, secure: bool) -> String {
    if (secure && port == 443) || (!secure && port == 80) { host.to_string() } else { format!("{host}:{port}") }
}

/// WebKit's `HTTPHeaderNames.in` (Bun 1.4.4): names `Headers` stores as known headers, in their default case.
const KNOWN_HEADERS: &[&str] = &[
    "Accept",
    "Accept-Charset",
    "Accept-Language",
    "Accept-Encoding",
    "Accept-Ranges",
    "Access-Control-Allow-Credentials",
    "Access-Control-Allow-Headers",
    "Access-Control-Allow-Methods",
    "Access-Control-Allow-Origin",
    "Access-Control-Expose-Headers",
    "Access-Control-Max-Age",
    "Access-Control-Request-Headers",
    "Access-Control-Request-Method",
    "Age",
    "Authorization",
    "Cache-Control",
    "Connection",
    "Content-Disposition",
    "Content-Encoding",
    "Content-Language",
    "Content-Length",
    "Content-Location",
    "Content-Security-Policy",
    "Content-Security-Policy-Report-Only",
    "Content-Type",
    "Content-Range",
    "Cookie",
    "Cookie2",
    "Cross-Origin-Embedder-Policy",
    "Cross-Origin-Embedder-Policy-Report-Only",
    "Cross-Origin-Opener-Policy",
    "Cross-Origin-Opener-Policy-Report-Only",
    "Cross-Origin-Resource-Policy",
    "Date",
    "DNT",
    "Default-Style",
    "ETag",
    "Expect",
    "Expires",
    "Host",
    "If-Match",
    "If-Modified-Since",
    "If-None-Match",
    "If-Range",
    "If-Unmodified-Since",
    "Keep-Alive",
    "Last-Event-ID",
    "Last-Modified",
    "Link",
    "Location",
    "Origin",
    "Ping-From",
    "Ping-To",
    "Purpose",
    "Pragma",
    "Proxy-Authorization",
    "Proxy-Connection",
    "Range",
    "Referer",
    "Referrer-Policy",
    "Refresh",
    "Report-To",
    "Sec-Fetch-Dest",
    "Sec-Fetch-Mode",
    "Sec-WebSocket-Accept",
    "Sec-WebSocket-Extensions",
    "Sec-WebSocket-Key",
    "Sec-WebSocket-Protocol",
    "Sec-WebSocket-Version",
    "Server-Timing",
    "Service-Worker",
    "Service-Worker-Allowed",
    "Service-Worker-Navigation-Preload",
    "Set-Cookie",
    "Set-Cookie2",
    "SourceMap",
    "TE",
    "Timing-Allow-Origin",
    "Trailer",
    "Transfer-Encoding",
    "Upgrade",
    "Upgrade-Insecure-Requests",
    "User-Agent",
    "Vary",
    "Via",
    "X-Content-Type-Options",
    "X-DNS-Prefetch-Control",
    "X-Frame-Options",
    "X-SourceMap",
    "X-XSS-Protection",
    "X-Temp-Tablet",
    "Icy-MetaInt",
    "Icy-Metadata",
];

/// The handshake headers as Bun iterates a `Headers` built from them (`FetchHeaders::Iterator` with
/// `lowerCaseKeys = false`): known names in their default case, others as given; values without surrounding
/// whitespace; sorted by name (code point order, so the capitalised known names come first).
pub fn fetch_headers(headers: &Headers) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| {
            let name = KNOWN_HEADERS.iter().find(|k| k.eq_ignore_ascii_case(name)).map_or_else(|| name.clone(), |k| k.to_string());
            (name, value.trim_matches([' ', '\t', '\r', '\n']).to_string())
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The upgrade request (`build_request_body`). Caller headers named like a handshake header are skipped, and a
/// caller `host` replaces the computed one.
pub fn upgrade_request(path: &str, host: &str, port: u16, secure: bool, key: &str, headers: &Headers, authorization: Option<&str>) -> Vec<u8> {
    let mut out = format!("GET {path} HTTP/1.1\r\n");
    let custom_host = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("host")).map(|(_, v)| v.clone());
    out.push_str(&format!("Host: {}\r\n", custom_host.unwrap_or_else(|| host_header(host, port, secure))));
    out.push_str("Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\n");
    out.push_str(&format!("Sec-WebSocket-Extensions: {EXTENSIONS_OFFER}\r\n"));
    out.push_str(&format!("Sec-WebSocket-Key: {key}\r\n"));
    if let Some(auth) = authorization
        && !headers.keys().any(|k| k.eq_ignore_ascii_case("authorization"))
    {
        out.push_str(&format!("Authorization: {auth}\r\n"));
    }
    const SKIPPED: [&str; 7] =
        ["host", "connection", "upgrade", "sec-websocket-version", "sec-websocket-extensions", "sec-websocket-key", "sec-websocket-protocol"];
    let mut bytes = out.into_bytes();
    for (name, value) in fetch_headers(headers) {
        if SKIPPED.iter().any(|s| name.eq_ignore_ascii_case(s)) {
            continue;
        }
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(b": ");
        bytes.extend_from_slice(value.as_bytes());
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(b"\r\n");
    bytes
}

/// `write_connect_request`.
pub fn connect_request(host: &str, port: u16, authorization: Option<&str>) -> Vec<u8> {
    let mut out = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\nProxy-Connection: Keep-Alive\r\n");
    if let Some(auth) = authorization {
        out.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    out.push_str("\r\n");
    out.into_bytes()
}

/// A parsed response head.
#[derive(Debug, Clone)]
pub(crate) struct Head {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
}

/// Parses one response head from `buf`: `Ok(Some((head, consumed)))`, `Ok(None)` when incomplete, `Err` for a
/// malformed one.
fn parse_head(buf: &[u8]) -> Result<Option<(Head, usize)>, ()> {
    let prefix = &buf[..buf.len().min(7)];
    if !b"HTTP/1.".starts_with(prefix) {
        return Err(());
    }
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { return Ok(None) };
    let text = std::str::from_utf8(&buf[..end]).map_err(|_| ())?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().ok_or(())?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().ok_or(())?;
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(());
    }
    let status = parts.next().filter(|s| s.len() == 3).and_then(|s| s.parse::<u16>().ok()).ok_or(())?;
    let reason = parts.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(())?;
        if name.is_empty() || name.contains([' ', '\t']) {
            return Err(());
        }
        headers.push((name.to_string(), value.trim_matches([' ', '\t']).to_string()));
    }
    Ok(Some((Head { status, reason, headers }, end + 4)))
}

/// Reads a final response head (skipping 1xx interim answers except 101). Returns the head and the bytes after it.
async fn read_head(io: &mut BoxIo, buf: &mut Vec<u8>) -> Result<Head, String> {
    let mut chunk = [0u8; 4096];
    loop {
        match parse_head(buf) {
            Err(()) => return Err("Invalid response".into()),
            Ok(Some((head, used))) => {
                buf.drain(..used);
                if head.status == 100 || (102..=199).contains(&head.status) {
                    continue;
                }
                return Ok(head);
            }
            Ok(None) if buf.len() > MAX_HEAD => return Err("Response headers too large".into()),
            Ok(None) => {}
        }
        let n = io.read(&mut chunk).await.map_err(|_| "Connection ended".to_string())?;
        if n == 0 {
            return Err("Connection ended".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// A stream that yields `prefix` before reading from `inner` (bytes read past a `CONNECT` answer).
struct Prefixed {
    prefix: Vec<u8>,
    inner: BoxIo,
}

impl AsyncRead for Prefixed {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if !self.prefix.is_empty() {
            let n = self.prefix.len().min(buf.remaining());
            buf.put_slice(&self.prefix[..n]);
            self.prefix.drain(..n);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Prefixed {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

fn strip_brackets(host: &str) -> &str {
    host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host)
}

async fn tls(io: BoxIo, host: &str) -> Result<BoxIo, String> {
    let name = rustls::pki_types::ServerName::try_from(strip_brackets(host).to_string()).map_err(|e| format!("TLS handshake failed: {e}"))?;
    let connector = tokio_rustls::TlsConnector::from(oc_util::http::websocket_tls());
    let stream = connector.connect(name, io).await.map_err(|e| format!("TLS handshake failed: {e}"))?;
    Ok(Box::new(stream))
}

async fn tcp(host: &str, port: u16) -> Result<BoxIo, String> {
    let stream = tokio::net::TcpStream::connect((strip_brackets(host), port)).await.map_err(|e| {
        let code = match e.kind() {
            io::ErrorKind::ConnectionRefused => Some("ECONNREFUSED"),
            io::ErrorKind::TimedOut => Some("ETIMEDOUT"),
            _ => None,
        };
        match code {
            Some(code) => format!("Failed to connect ({code})"),
            None => format!("Failed to connect ({e})"),
        }
    })?;
    let _ = stream.set_nodelay(true);
    Ok(Box::new(stream))
}

/// Opens the tunnel through `proxy` to `host:port`.
async fn tunnel(proxy: &ProxyConfig, host: &str, port: u16) -> Result<BoxIo, String> {
    let mut io = tcp(&proxy.host, proxy.port).await?;
    if proxy.https {
        io = tls(io, &proxy.host).await?;
    }
    io.write_all(&connect_request(host, port, proxy.authorization.as_deref())).await.map_err(|_| "Failed to write".to_string())?;
    io.flush().await.map_err(|_| "Failed to write".to_string())?;
    let mut buf = Vec::new();
    let head = read_head(&mut io, &mut buf).await?;
    if !(200..300).contains(&head.status) {
        let reason = if head.status == 407 { "Proxy authentication required" } else { "Proxy connection failed" };
        let status = if head.reason.is_empty() { head.status.to_string() } else { format!("{} {}", head.status, head.reason) };
        return Err(format!("{reason}: {status}"));
    }
    Ok(if buf.is_empty() { io } else { Box::new(Prefixed { prefix: buf, inner: io }) })
}

/// Opens and upgrades a socket to `url` (a `ws:` / `wss:` URL), using the proxy the environment selects.
pub(crate) async fn connect(url: &url::Url, headers: &Headers, env: proxy::Env<'_>) -> Result<Upgraded, ConnectFailure> {
    let proxy = proxy::resolve(url, env).map_err(ConnectFailure::Thrown)?;
    let secure = url.scheme() == "wss";
    let host = url.host_str().unwrap_or_default().to_string();
    let port = url.port_or_known_default().unwrap_or(if secure { 443 } else { 80 });
    let mut io = match &proxy {
        Some(proxy) => tunnel(proxy, &host, port).await,
        None => tcp(&host, port).await,
    }
    .map_err(ConnectFailure::Failed)?;
    if secure {
        io = tls(io, &host).await.map_err(ConnectFailure::Failed)?;
    }
    let key = base64::engine::general_purpose::STANDARD.encode(rand::random::<[u8; 16]>());
    let path = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    let authorization = proxy::basic_authorization(url);
    let request = upgrade_request(&path, &host, port, secure, &key, headers, authorization.as_deref());
    let write = async {
        io.write_all(&request).await?;
        io.flush().await
    };
    write.await.map_err(|_| ConnectFailure::Failed("Failed to write".into()))?;
    let mut buf = Vec::new();
    let head = read_head(&mut io, &mut buf).await.map_err(ConnectFailure::Failed)?;
    let deflate = validate(&head, &key).map_err(ConnectFailure::Failed)?;
    Ok(Upgraded { io, leftover: buf, deflate })
}

/// `validate_upgrade` (and the 101 check before it).
fn validate(head: &Head, key: &str) -> Result<Option<DeflateParams>, String> {
    if head.status != 101 {
        return Err(format!("Expected 101 status code: {}", head.status));
    }
    let (mut upgrade, mut accept) = (None::<&str>, None::<&str>);
    let (mut connection_seen, mut connection_upgrade) = (false, false);
    let mut deflate = None;
    for (name, value) in &head.headers {
        let name = name.to_ascii_lowercase();
        match name.as_str() {
            "connection" => {
                connection_seen = true;
                connection_upgrade |= value.split(',').any(|t| t.trim_matches([' ', '\t']).eq_ignore_ascii_case("upgrade"));
            }
            "upgrade" if upgrade.is_none() => upgrade = Some(value),
            "sec-websocket-version" if value != "13" => return Err("Invalid websocket version".into()),
            "sec-websocket-accept" if accept.is_none() => accept = Some(value),
            // No subprotocol is requested, so any answer is a mismatch.
            "sec-websocket-protocol" => return Err("Mismatch client protocol".into()),
            "sec-websocket-extensions" => {
                if deflate.is_some() {
                    return Err("Unsupported websocket extension".into());
                }
                deflate = Some(parse_deflate_response(value).ok_or("Unsupported websocket extension")?);
            }
            _ => {}
        }
    }
    if upgrade.is_none_or(str::is_empty) {
        return Err("Missing upgrade header".into());
    }
    if !connection_seen {
        return Err("Missing connection header".into());
    }
    if accept.is_none_or(str::is_empty) {
        return Err("Missing websocket accept header".into());
    }
    if !connection_upgrade {
        return Err("Invalid connection header".into());
    }
    if !upgrade.unwrap_or_default().eq_ignore_ascii_case("websocket") {
        return Err("Invalid upgrade header".into());
    }
    if accept != Some(accept_key(key).as_str()) {
        return Err("Mismatch websocket accept header".into());
    }
    Ok(deflate)
}

// ------------------------------------------------------------------------------------------------ deflate

const TRAILER: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

/// The outbound half of permessage-deflate (`PerMessageDeflate.compress`, client side).
pub struct Compressor {
    stream: flate2::Compress,
    reset: bool,
}

impl Compressor {
    pub fn new(params: DeflateParams) -> Self {
        // zlib refuses an 8-bit window for raw deflate; a peer that negotiated 8 bits can decode 9-bit output.
        let bits = params.client_max_window_bits.clamp(9, 15);
        Self { stream: flate2::Compress::new_with_window_bits(flate2::Compression::new(6), false, bits), reset: params.client_no_context_takeover }
    }

    /// One message, compressed with a sync flush and the `00 00 ff ff` tail removed.
    pub fn compress(&mut self, input: &[u8]) -> Result<Vec<u8>, String> {
        let mut out: Vec<u8> = Vec::with_capacity(input.len() / 2 + 64);
        let mut consumed = 0;
        loop {
            if out.capacity() - out.len() < 4096 {
                out.reserve(4096);
            }
            let before = self.stream.total_in();
            let status = self.stream.compress_vec(&input[consumed..], &mut out, flate2::FlushCompress::Sync).map_err(|e| e.to_string())?;
            consumed += (self.stream.total_in() - before) as usize;
            // Done once every byte went in and the flush did not fill the output.
            if consumed == input.len() && (out.len() < out.capacity() || status == flate2::Status::BufError) {
                break;
            }
        }
        if out.ends_with(&TRAILER) {
            out.truncate(out.len() - 4);
        }
        if self.reset {
            self.stream.reset();
        }
        Ok(out)
    }
}

/// The inbound half of permessage-deflate (`PerMessageDeflate.decompress`, client side).
pub struct Decompressor {
    stream: flate2::Decompress,
    reset: bool,
}

/// A message that could not be inflated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InflateError {
    Invalid,
    TooLarge,
}

impl Decompressor {
    pub fn new(params: DeflateParams) -> Self {
        // A larger window decodes any smaller one (raw inflate takes 9 to 15 here).
        let bits = params.server_max_window_bits.clamp(9, 15);
        Self { stream: flate2::Decompress::new_with_window_bits(false, bits), reset: params.server_no_context_takeover }
    }

    /// One message (the `00 00 ff ff` tail restored), at most `max` bytes once inflated.
    pub fn decompress(&mut self, input: &[u8], max: usize) -> Result<Vec<u8>, InflateError> {
        let mut data = Vec::with_capacity(input.len() + 4);
        data.extend_from_slice(input);
        data.extend_from_slice(&TRAILER);
        let mut out: Vec<u8> = Vec::with_capacity((input.len() * 4).clamp(4096, 128 * 1024));
        let mut consumed = 0;
        let mut ended = false;
        loop {
            if out.capacity() - out.len() < 4096 {
                out.reserve(out.capacity().max(4096));
            }
            let before = self.stream.total_in();
            let status = self.stream.decompress_vec(&data[consumed..], &mut out, flate2::FlushDecompress::Sync).map_err(|_| InflateError::Invalid)?;
            consumed += (self.stream.total_in() - before) as usize;
            if out.len() > max {
                return Err(InflateError::TooLarge);
            }
            match status {
                flate2::Status::StreamEnd => {
                    ended = true;
                    break;
                }
                // No progress possible: finished when every byte went in, stuck otherwise.
                flate2::Status::BufError if consumed == data.len() => break,
                flate2::Status::BufError => return Err(InflateError::Invalid),
                flate2::Status::Ok => {}
            }
            if consumed == data.len() && out.len() < out.capacity() {
                break;
            }
        }
        // RFC 7692 section 7.2.3: a final block ends this DEFLATE stream; the next message starts a new one.
        if ended || self.reset {
            self.stream.reset(false);
        }
        Ok(out)
    }
}

// ------------------------------------------------------------------------------------------------- frames

pub const OP_CONTINUATION: u8 = 0x0;
pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xA;

/// A masked client frame.
pub fn encode_frame(opcode: u8, rsv1: bool, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | if rsv1 { 0x40 } else { 0 } | opcode);
    let len = payload.len();
    if len < 126 {
        out.push(0x80 | len as u8);
    } else if len < 65536 {
        out.push(0x80 | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    let mask = rand::random::<[u8; 4]>();
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

/// What the reader hands back.
#[derive(Debug, PartialEq)]
pub enum Inbound {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    /// A close frame: code (1005 without one) and reason.
    Close(u16, String),
}

/// A protocol error (Bun's `ErrorCode` text) and the close code sent to the peer, if any.
#[derive(Debug, PartialEq)]
pub struct ProtocolError {
    pub message: &'static str,
    pub close_code: Option<u16>,
}

const fn protocol(message: &'static str, close_code: Option<u16>) -> ProtocolError {
    ProtocolError { message, close_code }
}

/// Incremental frame decoding over a byte buffer.
pub struct Reader {
    pub buf: Vec<u8>,
    deflate: Option<Decompressor>,
    message: Option<(u8, bool, Vec<u8>)>,
}

impl Reader {
    pub fn new(leftover: Vec<u8>, deflate: Option<DeflateParams>) -> Self {
        Self { buf: leftover, deflate: deflate.map(Decompressor::new), message: None }
    }

    /// The next complete message or control frame in the buffer (`Ok(None)`: more bytes needed).
    pub fn next_message(&mut self) -> Result<Option<Inbound>, ProtocolError> {
        loop {
            let Some((fin, rsv1, rsv23, opcode, payload, used)) = self.frame()? else { return Ok(None) };
            self.buf.drain(..used);
            if rsv23 {
                return Err(protocol("Protocol error - RSV2 and RSV3 must be clear", Some(1002)));
            }
            let data = matches!(opcode, OP_TEXT | OP_BINARY);
            if rsv1 && (!data || self.deflate.is_none()) {
                return Err(protocol("Protocol error - RSV1 must be clear", Some(1002)));
            }
            match opcode {
                OP_CLOSE | OP_PING | OP_PONG => {
                    if !fin {
                        return Err(protocol("Protocol error - control frame is fragmented", Some(1002)));
                    }
                    match opcode {
                        OP_PING => return Ok(Some(Inbound::Ping(payload))),
                        OP_PONG => continue,
                        _ => {
                            if payload.len() == 1 {
                                return Err(protocol("Protocol error - invalid close code", Some(1002)));
                            }
                            if payload.is_empty() {
                                return Ok(Some(Inbound::Close(1005, String::new())));
                            }
                            let code = u16::from_be_bytes([payload[0], payload[1]]);
                            if !valid_close_code(code) {
                                return Err(protocol("Protocol error - invalid close code", Some(1002)));
                            }
                            let Ok(reason) = String::from_utf8(payload[2..].to_vec()) else {
                                return Err(protocol("Server sent invalid UTF8", Some(1007)));
                            };
                            return Ok(Some(Inbound::Close(code, reason)));
                        }
                    }
                }
                OP_TEXT | OP_BINARY => {
                    if self.message.is_some() {
                        return Err(protocol("Protocol error - unexpected opcode", Some(1002)));
                    }
                    self.message = Some((opcode, rsv1, payload));
                }
                OP_CONTINUATION => {
                    let Some((_, _, buffered)) = self.message.as_mut() else {
                        return Err(protocol("Protocol error - unexpected opcode", Some(1002)));
                    };
                    buffered.extend_from_slice(&payload);
                }
                _ => return Err(protocol("Protocol error - unsupported control frame", Some(1002))),
            }
            if self.message.as_ref().is_some_and(|(_, _, b)| b.len() > MAX_PAYLOAD) {
                return Err(protocol("Message too big", Some(1009)));
            }
            if !fin {
                continue;
            }
            let (opcode, compressed, payload) = self.message.take().unwrap_or_default();
            let payload = if compressed {
                let deflate = self.deflate.as_mut().ok_or(protocol("Protocol error - RSV1 must be clear", Some(1002)))?;
                match deflate.decompress(&payload, MAX_PAYLOAD) {
                    Ok(out) => out,
                    Err(InflateError::TooLarge) => return Err(protocol("Message too big", Some(1009))),
                    Err(InflateError::Invalid) => return Err(protocol("Invalid compressed data", Some(1007))),
                }
            } else {
                payload
            };
            return Ok(Some(if opcode == OP_TEXT {
                Inbound::Text(String::from_utf8(payload).map_err(|_| protocol("Server sent invalid UTF8", Some(1007)))?)
            } else {
                Inbound::Binary(payload)
            }));
        }
    }

    #[allow(clippy::type_complexity)]
    fn frame(&self) -> Result<Option<(bool, bool, bool, u8, Vec<u8>, usize)>, ProtocolError> {
        let b = &self.buf;
        if b.len() < 2 {
            return Ok(None);
        }
        let (fin, rsv1, rsv23, opcode) = (b[0] & 0x80 != 0, b[0] & 0x40 != 0, b[0] & 0x30 != 0, b[0] & 0x0F);
        if b[1] & 0x80 != 0 {
            return Err(protocol("Protocol error - unexpected mask from server", Some(1002)));
        }
        let (len, header) = match b[1] & 0x7F {
            126 => {
                if b.len() < 4 {
                    return Ok(None);
                }
                (u16::from_be_bytes([b[2], b[3]]) as u64, 4)
            }
            127 => {
                if b.len() < 10 {
                    return Ok(None);
                }
                let len = u64::from_be_bytes(b[2..10].try_into().unwrap_or_default());
                if len > (1 << 53) - 1 {
                    return Err(protocol("Payload length over 2^53 - 1", Some(1009)));
                }
                (len, 10)
            }
            n => (n as u64, 2),
        };
        if opcode >= 8 && len > 125 {
            return Err(protocol("Protocol error - invalid control frame", Some(1002)));
        }
        let buffered = self.message.as_ref().map_or(0, |(_, _, p)| p.len()) as u64;
        if buffered + len > MAX_PAYLOAD as u64 {
            return Err(protocol("Message too big", Some(1009)));
        }
        let total = header + len as usize;
        if b.len() < total {
            return Ok(None);
        }
        Ok(Some((fin, rsv1, rsv23, opcode, b[header..total].to_vec(), total)))
    }
}

/// Close codes a peer may send (RFC 6455 §7.4, as npm `ws` checks them).
fn valid_close_code(code: u16) -> bool {
    (1000..=1014).contains(&code) && !(1004..=1006).contains(&code) || (3000..=4999).contains(&code)
}

/// The sending half: frames (compressed when negotiated and large enough) written in order.
pub(crate) struct Writer {
    pub(crate) io: tokio::io::WriteHalf<BoxIo>,
    pub(crate) deflate: Option<Compressor>,
}

impl Writer {
    pub async fn text(&mut self, text: &str) -> io::Result<()> {
        let frame = match &mut self.deflate {
            Some(deflate) if text.len() >= MIN_COMPRESS_SIZE => match deflate.compress(text.as_bytes()) {
                Ok(compressed) => encode_frame(OP_TEXT, true, &compressed),
                Err(_) => encode_frame(OP_TEXT, false, text.as_bytes()),
            },
            _ => encode_frame(OP_TEXT, false, text.as_bytes()),
        };
        self.io.write_all(&frame).await?;
        self.io.flush().await
    }

    pub async fn control(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        self.io.write_all(&encode_frame(opcode, false, payload)).await?;
        self.io.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deflate_answers() {
        assert_eq!(parse_deflate_response("permessage-deflate"), Some(DeflateParams::default()));
        assert_eq!(
            parse_deflate_response("permessage-deflate; server_no_context_takeover; client_max_window_bits=10"),
            Some(DeflateParams { server_no_context_takeover: true, client_max_window_bits: 10, ..Default::default() })
        );
        assert_eq!(
            parse_deflate_response("permessage-deflate;server_max_window_bits=\"9\""),
            Some(DeflateParams { server_max_window_bits: 9, ..Default::default() })
        );
        assert_eq!(parse_deflate_response("permessage-deflate; client_max_window_bits"), None);
        assert_eq!(parse_deflate_response("permessage-deflate; server_max_window_bits=16"), None);
        assert_eq!(parse_deflate_response("permessage-deflate; foo"), None);
        assert_eq!(parse_deflate_response("permessage-deflate; client_no_context_takeover; client_no_context_takeover"), None);
        assert_eq!(parse_deflate_response("x-webkit-deflate-frame"), None);
    }

    #[test]
    fn request_layout() {
        let mut headers = Headers::new();
        headers.insert("x-session-id".into(), "s".into());
        headers.insert("openai-beta".into(), " responses_websockets=2026-02-06 ".into());
        headers.insert("user-agent".into(), "ua".into());
        headers.insert("authorization".into(), "Bearer k".into());
        headers.insert("sec-websocket-key".into(), "ignored".into());
        let request = String::from_utf8(upgrade_request("/v1/responses", "api.test", 443, true, "a2V5", &headers, None)).unwrap();
        // Known names are capitalised and sort first (what Bun 1.4.4 sends for upstream's headers).
        assert_eq!(
            request,
            "GET /v1/responses HTTP/1.1\r\nHost: api.test\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\nSec-WebSocket-Key: a2V5\r\nAuthorization: Bearer k\r\n\
             User-Agent: ua\r\nopenai-beta: responses_websockets=2026-02-06\r\nx-session-id: s\r\n\r\n"
        );
        let request = String::from_utf8(upgrade_request("/", "127.0.0.2", 8080, false, "a2V5", &Headers::new(), Some("Basic eDp5"))).unwrap();
        assert!(request.contains("Host: 127.0.0.2:8080\r\n") && request.contains("Authorization: Basic eDp5\r\n"), "{request}");
        assert_eq!(
            String::from_utf8(connect_request("api.test", 443, Some("Basic eDp5"))).unwrap(),
            "CONNECT api.test:443 HTTP/1.1\r\nHost: api.test:443\r\nProxy-Connection: Keep-Alive\r\nProxy-Authorization: Basic eDp5\r\n\r\n"
        );
    }

    #[test]
    fn upgrade_validation() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        assert_eq!(accept_key(key), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        let head = |status: u16, extra: &[(&str, &str)]| Head {
            status,
            reason: String::new(),
            headers: [("Upgrade", "websocket"), ("Connection", "Upgrade"), ("Sec-WebSocket-Accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")]
                .iter()
                .chain(extra)
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        assert_eq!(validate(&head(101, &[]), key), Ok(None));
        assert_eq!(validate(&head(101, &[("sec-websocket-extensions", "permessage-deflate")]), key), Ok(Some(DeflateParams::default())));
        assert_eq!(validate(&head(403, &[]), key), Err("Expected 101 status code: 403".into()));
        assert_eq!(validate(&head(101, &[("Sec-WebSocket-Extensions", "foo")]), key), Err("Unsupported websocket extension".into()));
        assert_eq!(
            validate(&head(101, &[("Sec-WebSocket-Extensions", "permessage-deflate"), ("Sec-WebSocket-Extensions", "permessage-deflate")]), key),
            Err("Unsupported websocket extension".into())
        );
        assert_eq!(validate(&head(101, &[("Sec-WebSocket-Protocol", "x")]), key), Err("Mismatch client protocol".into()));
        assert_eq!(validate(&head(101, &[]), "b3RoZXIga2V5IHZhbHVlIQ=="), Err("Mismatch websocket accept header".into()));
    }

    fn server_frame(opcode: u8, rsv1: bool, fin: bool, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![if fin { 0x80 } else { 0 } | if rsv1 { 0x40 } else { 0 } | opcode];
        if payload.len() < 126 {
            out.push(payload.len() as u8);
        } else {
            out.push(126);
            out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        out.extend_from_slice(payload);
        out
    }

    /// The server side of permessage-deflate (what a server sends).
    fn server_compress(stream: &mut flate2::Compress, text: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(text.len() + 64);
        stream.compress_vec(text.as_bytes(), &mut out, flate2::FlushCompress::Sync).unwrap();
        assert!(out.ends_with(&TRAILER));
        out.truncate(out.len() - 4);
        out
    }

    #[test]
    fn inflates_compressed_messages_with_context_takeover() {
        let mut server = flate2::Compress::new(flate2::Compression::default(), false);
        let text = "{\"type\":\"response.output_text.delta\",\"delta\":\"hello hello hello\"}";
        let mut bytes = server_frame(OP_TEXT, true, true, &server_compress(&mut server, text));
        // The second message reuses the window (context takeover) and is split over two frames.
        let second = server_compress(&mut server, text);
        bytes.extend(server_frame(OP_TEXT, true, false, &second[..3]));
        bytes.extend(server_frame(OP_PING, false, true, b"p"));
        bytes.extend(server_frame(OP_CONTINUATION, false, true, &second[3..]));
        bytes.extend(server_frame(OP_TEXT, false, true, b"plain"));
        let mut reader = Reader::new(bytes, Some(DeflateParams::default()));
        assert_eq!(reader.next_message(), Ok(Some(Inbound::Text(text.into()))));
        assert_eq!(reader.next_message(), Ok(Some(Inbound::Ping(b"p".to_vec()))));
        assert_eq!(reader.next_message(), Ok(Some(Inbound::Text(text.into()))));
        assert_eq!(reader.next_message(), Ok(Some(Inbound::Text("plain".into()))));
        assert_eq!(reader.next_message(), Ok(None));
    }

    #[test]
    fn rejects_rsv1_without_negotiation_and_bad_data() {
        let mut reader = Reader::new(server_frame(OP_TEXT, true, true, b"xx"), None);
        assert_eq!(reader.next_message().unwrap_err().message, "Protocol error - RSV1 must be clear");
        let mut reader = Reader::new(server_frame(OP_TEXT, true, true, &[0xff, 0xff, 0xff]), Some(DeflateParams::default()));
        assert_eq!(reader.next_message().unwrap_err(), protocol("Invalid compressed data", Some(1007)));
        let mut reader = Reader::new(server_frame(OP_CLOSE, false, true, &[0x03, 0xe8, b'o', b'k']), None);
        assert_eq!(reader.next_message(), Ok(Some(Inbound::Close(1000, "ok".into()))));
        let mut reader = Reader::new(server_frame(OP_CLOSE, false, true, &[]), None);
        assert_eq!(reader.next_message(), Ok(Some(Inbound::Close(1005, String::new()))));
    }

    #[test]
    fn compresses_large_messages_round_trip() {
        let params = DeflateParams { client_max_window_bits: 10, client_no_context_takeover: true, ..Default::default() };
        let mut deflate = Compressor::new(params);
        let text = "x".repeat(5000) + &"response.create ".repeat(100);
        let compressed = deflate.compress(text.as_bytes()).unwrap();
        assert!(compressed.len() < text.len() / 10 && !compressed.ends_with(&TRAILER));
        // A server inflating with the negotiated window (no context takeover: a fresh inflater per message).
        for _ in 0..2 {
            let mut data = compressed.clone();
            data.extend_from_slice(&TRAILER);
            let mut inflate = flate2::Decompress::new_with_window_bits(false, 10);
            let mut out = Vec::with_capacity(text.len() + 16);
            inflate.decompress_vec(&data, &mut out, flate2::FlushDecompress::Sync).unwrap();
            assert_eq!(out, text.as_bytes());
        }
        assert_eq!(deflate.compress(text.as_bytes()).unwrap(), compressed);
    }
}
