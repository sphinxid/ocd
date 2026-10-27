//! The bridge WebSocket's client handshake, done by hand to send the same request as Bun's client. Like Bun's, it
//! fails unless the server picks the offered subprotocol, and every failure reads as an abnormal closure (1006).

use std::pin::Pin;
use std::task::{Context, Poll};

use base64::Engine;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use url::Url;

/// A plain or TLS connection.
pub enum Io {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for Io {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Io {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_flush(cx),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
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

/// Opens `url` (ws or wss) with the given subprotocol and extra headers.
pub async fn connect(url: &Url, protocol: &str, headers: &[(&str, &str)]) -> Result<WebSocketStream<Io>, Failed> {
    let host = url.host_str().ok_or(failed(1006))?.to_string();
    let tls = url.scheme() == "wss";
    let port = url.port_or_known_default().unwrap_or(if tls { 443 } else { 80 });
    let connect_host = host.trim_start_matches('[').trim_end_matches(']').to_string();
    let tcp = TcpStream::connect((connect_host.as_str(), port)).await.map_err(|_| failed(1006))?;
    let _ = tcp.set_nodelay(true);
    let mut io = if tls {
        let connector = tokio_rustls::TlsConnector::from(oc_util::http::websocket_tls());
        let name = rustls::pki_types::ServerName::try_from(connect_host.clone()).map_err(|_| failed(1006))?;
        Io::Tls(Box::new(connector.connect(name, tcp).await.map_err(|_| failed(1006))?))
    } else {
        Io::Plain(tcp)
    };
    let key_bytes: [u8; 16] = rand::random();
    let key = base64::engine::general_purpose::STANDARD.encode(key_bytes);
    let default_port = if tls { 443 } else { 80 };
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
    let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
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
    let leftover = buffer[end..].to_vec();
    let config = WebSocketConfig::default();
    Ok(WebSocketStream::from_partially_read(io, leftover, Role::Client, Some(config)).await)
}

#[cfg(test)]
mod tests {
    #[test]
    fn accept_key_matches_rfc6455() {
        assert_eq!(super::accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }
}
