//! Minimal loopback HTTP/1.1 server for OAuth redirects (replaces `node:http` `createServer`).
//!
//! One request per connection (`Connection: close`). The handler may answer later (after the token
//! exchange) by awaiting inside its future; when the future is dropped the connection is closed without a
//! response (`response.destroy()`). The server stops when its [`CancellationToken`] (the attempt scope) is
//! cancelled.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// A parsed request.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// Path without the query string.
    pub path: String,
    /// Raw query string (without `?`).
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    /// `URLSearchParams.get(name)`: first value, decoded.
    pub fn param(&self, name: &str) -> Option<String> {
        url::form_urlencoded::parse(self.query.as_bytes()).find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
    }

    /// `URLSearchParams.has(name)`.
    pub fn has_param(&self, name: &str) -> bool {
        url::form_urlencoded::parse(self.query.as_bytes()).any(|(k, _)| k == name)
    }
}

/// A response to write.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub content_type: Option<&'static str>,
    pub body: String,
}

impl Response {
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self { status, content_type: None, body: body.into() }
    }
    pub fn html(status: u16, body: impl Into<String>) -> Self {
        Self { status, content_type: Some("text/html"), body: body.into() }
    }
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self { status, content_type: Some("application/json"), body: body.into() }
    }
    pub fn not_found(body: &str) -> Self {
        Self::new(404, body)
    }
}

pub type HandlerFuture = Pin<Box<dyn Future<Output = Option<Response>> + Send>>;
/// Request handler; `None` closes the connection without a response.
pub type Handler = Arc<dyn Fn(Request) -> HandlerFuture + Send + Sync>;

/// Wraps a synchronous handler.
pub fn sync_handler<F>(handler: F) -> Handler
where
    F: Fn(Request) -> Response + Send + Sync + 'static,
{
    Arc::new(move |request| {
        let response = handler(request);
        Box::pin(async move { Some(response) })
    })
}

/// A bound, not yet serving, listener.
pub struct Bound {
    listeners: Vec<TcpListener>,
    pub port: u16,
}

/// Binds `host:port` (`port == 0` → ephemeral). `"localhost"` binds 127.0.0.1 and, best effort, `[::1]` on
/// the same port (Node resolves `localhost` either way; browsers may try both).
pub async fn bind(host: &str, port: u16) -> io::Result<Bound> {
    let primary = if host == "localhost" { "127.0.0.1" } else { host };
    let addr: SocketAddr = format!("{}:{port}", bracket(primary))
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))?;
    let listener = TcpListener::bind(addr).await?;
    let port = listener.local_addr()?.port();
    let mut listeners = vec![listener];
    if host == "localhost"
        && let Ok(v6) = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port))).await
    {
        listeners.push(v6);
    }
    Ok(Bound { listeners, port })
}

fn bracket(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() }
}

impl Bound {
    /// Serves until `scope` is cancelled.
    pub fn serve(self, handler: Handler, scope: CancellationToken) {
        for listener in self.listeners {
            let handler = handler.clone();
            let scope = scope.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = scope.cancelled() => break,
                        accepted = listener.accept() => {
                            let Ok((stream, _)) = accepted else { continue };
                            // Accepted connections finish on their own (like `server.close()`): a deferred
                            // response is still written after the scope closes, or dropped with its sender.
                            tokio::spawn(handle(stream, handler.clone()));
                        }
                    }
                }
            });
        }
    }
}

async fn handle(mut stream: TcpStream, handler: Handler) {
    let read = tokio::time::timeout(std::time::Duration::from_secs(30), read_request(&mut stream)).await;
    let Ok(Some(request)) = read else { return };
    if let Some(response) = handler(request).await {
        let _ = write_response(&mut stream, &response).await;
    }
    let _ = stream.shutdown().await;
}

async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = find(&buffer, b"\r\n\r\n") {
            break pos;
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let length = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0)
        .min(1024 * 1024);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    // Absolute-form targets are reduced to their path.
    let target = match target.find("://") {
        Some(i) if !target.starts_with('/') => {
            target[i + 3..].find('/').map(|j| target[i + 3 + j..].to_string()).unwrap_or_else(|| "/".into())
        }
        _ => target,
    };
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.split('#').next().unwrap_or("").to_string()),
        None => (target.split('#').next().unwrap_or("/").to_string(), String::new()),
    };
    Some(Request { method, path, query, headers, body })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "",
    }
}

async fn write_response(stream: &mut TcpStream, response: &Response) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\n", response.status, reason(response.status));
    if let Some(content_type) = response.content_type {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", response.body.len()));
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(response.body.as_bytes()).await?;
    stream.flush().await
}

/// One-shot slot completed by the first callback (`Deferred` semantics: later completions are ignored).
pub struct Once<T> {
    sender: parking_lot::Mutex<Option<tokio::sync::oneshot::Sender<T>>>,
}

impl<T> Once<T> {
    pub fn new() -> (Arc<Self>, tokio::sync::oneshot::Receiver<T>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (Arc::new(Self { sender: parking_lot::Mutex::new(Some(tx)) }), rx)
    }

    /// Returns false when already completed.
    pub fn complete(&self, value: T) -> bool {
        match self.sender.lock().take() {
            Some(sender) => {
                let _ = sender.send(value);
                true
            }
            None => false,
        }
    }
}
