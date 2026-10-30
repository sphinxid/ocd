//! The bridge WebSocket's client handshake, done by hand to send the same request as Bun's client. Like Bun's, it
//! fails unless the server picks the offered subprotocol, and every failure reads as an abnormal closure (1006).
//! Through a proxy it first opens a tunnel with `CONNECT`, as Bun's WebSocket does with its `proxy` option.

use std::pin::Pin;
use std::task::{Context, Poll};

use base64::Engine;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use url::Url;

/// A byte stream: TCP, TLS, or either through a proxy's tunnel.
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// The connection a bridge WebSocket runs on.
pub struct Io(Box<dyn Stream>);

impl AsyncRead for Io {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for Io {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut *self.get_mut().0).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().0).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().0).poll_shutdown(cx)
    }
}

/// Why the handshake failed, as the close event Bun reports for it.
#[derive(Debug, Clone, PartialEq)]
pub struct Failed {
    pub code: u16,
    pub reason: String,
}

fn failed(code: u16) -> Failed {
    Failed { code, reason: String::new() }
}

/// `Sec-WebSocket-Accept` for a key.
pub fn accept_key(key: &str) -> String {
    let digest = ring::digest::digest(
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
    );
    base64::engine::general_purpose::STANDARD.encode(digest.as_ref())
}

fn unbracket(host: &str) -> &str {
    host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host)
}

async fn tls(io: Io, host: &str) -> Result<Io, Failed> {
    let connector = tokio_rustls::TlsConnector::from(oc_util::http::websocket_tls());
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| failed(1006))?;
    Ok(Io(Box::new(connector.connect(name, io).await.map_err(|_| failed(1006))?)))
}

/// Reads an HTTP response head (through the blank line) and whatever arrived after it.
async fn read_head(io: &mut Io) -> Result<(String, Vec<u8>), Failed> {
    let mut buffer = Vec::new();
    let end = loop {
        let mut chunk = [0u8; 4096];
        let n = io.read(&mut chunk).await.map_err(|_| failed(1006))?;
        if n == 0 {
            return Err(failed(1006));
        }
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(i) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buffer.len() > 64 * 1024 {
            return Err(failed(1006));
        }
    };
    Ok((String::from_utf8_lossy(&buffer[..end]).into_owned(), buffer[end..].to_vec()))
}

/// Percent-decodes a URL's userinfo part.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `Proxy-Authorization` for a proxy URL's credentials, as Bun sends it: `Basic` with the decoded `user:password`
/// whenever either is set.
pub fn proxy_authorization(proxy: &Url) -> Option<String> {
    let password = proxy.password().unwrap_or("");
    if proxy.username().is_empty() && password.is_empty() {
        return None;
    }
    let credentials = format!("{}:{}", percent_decode(proxy.username()), percent_decode(password));
    Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(credentials)))
}

/// Opens a tunnel to `host:port` through an HTTP or HTTPS proxy, as Bun's WebSocket does: `CONNECT host:port` with
/// `Host`, `Proxy-Connection: Keep-Alive` and the proxy's credentials. Anything but a 2xx answer fails.
async fn through_proxy(proxy: &Url, host: &str, port: u16) -> Result<Io, Failed> {
    let proxy_host = unbracket(proxy.host_str().ok_or(failed(1006))?).to_string();
    let proxy_tls = proxy.scheme() == "https";
    let proxy_port = proxy.port_or_known_default().unwrap_or(if proxy_tls { 443 } else { 80 });
    let tcp = TcpStream::connect((proxy_host.as_str(), proxy_port)).await.map_err(|_| failed(1006))?;
    let _ = tcp.set_nodelay(true);
    let mut io = Io(Box::new(tcp));
    if proxy_tls {
        io = tls(io, &proxy_host).await?;
    }
    let authority = format!("{host}:{port}");
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: Keep-Alive\r\n");
    if let Some(authorization) = proxy_authorization(proxy) {
        request.push_str(&format!("Proxy-Authorization: {authorization}\r\n"));
    }
    request.push_str("\r\n");
    io.write_all(request.as_bytes()).await.map_err(|_| failed(1006))?;
    io.flush().await.map_err(|_| failed(1006))?;
    let (head, rest) = read_head(&mut io).await?;
    let code = head.split("\r\n").next().unwrap_or("").split(' ').nth(1).unwrap_or("");
    if code.len() != 3 || !code.starts_with('2') || !rest.is_empty() {
        return Err(failed(1006));
    }
    Ok(io)
}

/// Opens `url` (ws or wss) with the given subprotocol and extra headers, through `proxy` when given.
pub async fn connect(
    url: &Url,
    protocol: &str,
    headers: &[(&str, &str)],
    proxy: Option<&Url>,
) -> Result<WebSocketStream<Io>, Failed> {
    let host = url.host_str().ok_or(failed(1006))?.to_string();
    let secure = url.scheme() == "wss";
    let port = url.port_or_known_default().unwrap_or(if secure { 443 } else { 80 });
    let connect_host = unbracket(&host).to_string();
    let mut io = match proxy {
        Some(proxy) => through_proxy(proxy, &host, port).await?,
        None => {
            let tcp = TcpStream::connect((connect_host.as_str(), port)).await.map_err(|_| failed(1006))?;
            let _ = tcp.set_nodelay(true);
            Io(Box::new(tcp))
        }
    };
    if secure {
        io = tls(io, &connect_host).await?;
    }
    let key_bytes: [u8; 16] = rand::random();
    let key = base64::engine::general_purpose::STANDARD.encode(key_bytes);
    let default_port = if secure { 443 } else { 80 };
    let host_header = if port == default_port { host.clone() } else { format!("{host}:{port}") };
    let mut path = url.path().to_string();
    if let Some(q) = url.query() {
        path.push('?');
        path.push_str(q);
    }
    let mut request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Protocol: {protocol}\r\n"
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    io.write_all(request.as_bytes()).await.map_err(|_| failed(1006))?;
    io.flush().await.map_err(|_| failed(1006))?;

    let (head, leftover) = read_head(&mut io).await?;
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or("");
    let code = status.split(' ').nth(1).unwrap_or("");
    if code != "101" {
        return Err(failed(1006));
    }
    let mut accept = None;
    let mut chosen = None;
    let mut upgrade = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "sec-websocket-accept" => accept = Some(value.to_string()),
            "sec-websocket-protocol" => chosen = Some(value.to_string()),
            "upgrade" => upgrade = value.eq_ignore_ascii_case("websocket"),
            _ => {}
        }
    }
    if !upgrade || accept.as_deref() != Some(accept_key(&key).as_str()) || chosen.as_deref() != Some(protocol) {
        return Err(failed(1006));
    }
    let config = WebSocketConfig::default();
    Ok(WebSocketStream::from_partially_read(io, leftover, Role::Client, Some(config)).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_key_matches_rfc6455() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn proxy_credentials_are_decoded() {
        let auth = |u: &str| proxy_authorization(&Url::parse(u).unwrap());
        assert_eq!(auth("http://p:1"), None);
        // What Bun 1.4.4 sent for the same URLs.
        assert_eq!(auth("http://u:p%40x@p:1").as_deref(), Some("Basic dTpwQHg="));
        assert_eq!(auth("http://user@p:1").as_deref(), Some("Basic dXNlcjo="));
        assert_eq!(auth("http://:pw@p:1").as_deref(), Some("Basic OnB3"));
    }
}
