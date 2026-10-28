//! A scripted HTTP/1.1 server on 127.0.0.1 that records every request (cloud credential tests).

#![allow(dead_code)]

use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone, Debug)]
pub struct Req {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn form(&self) -> Vec<(String, String)> {
        url::form_urlencoded::parse(self.body.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
    }
    pub fn field(&self, name: &str) -> Option<String> {
        self.form().into_iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }
    pub fn param(&self, name: &str) -> Option<String> {
        url::form_urlencoded::parse(self.query.as_bytes()).find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
    }
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Resp {
    pub fn text(status: u16, body: impl Into<String>) -> Resp {
        Resp { status, headers: vec![("content-type".into(), "text/plain".into())], body: body.into() }
    }
    pub fn json(status: u16, value: Value) -> Resp {
        Resp { status, headers: vec![("content-type".into(), "application/json".into())], body: value.to_string() }
    }
    pub fn xml(status: u16, body: impl Into<String>) -> Resp {
        Resp { status, headers: vec![("content-type".into(), "text/xml".into())], body: body.into() }
    }
    pub fn with(mut self, name: &str, value: &str) -> Resp {
        self.headers.push((name.into(), value.into()));
        self
    }
}

pub struct Mock {
    pub base: String,
    pub port: u16,
    requests: Arc<Mutex<Vec<Req>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Mock {
    pub fn requests(&self) -> Vec<Req> {
        self.requests.lock().clone()
    }
    pub fn requests_to(&self, path: &str) -> Vec<Req> {
        self.requests.lock().iter().filter(|r| r.path == path).cloned().collect()
    }
}

/// `route(request, nth request to this path)`.
pub async fn mock(route: impl Fn(&Req, usize) -> Resp + Send + Sync + 'static) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let counts = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
    let route = Arc::new(route);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let route = route.clone();
            let recorded = recorded.clone();
            let counts = counts.clone();
            tokio::spawn(async move {
                let Some(request) = read_request(&mut socket).await else { return };
                let n = {
                    let mut counts = counts.lock();
                    let entry = counts.entry(request.path.clone()).or_insert(0);
                    *entry += 1;
                    *entry - 1
                };
                recorded.lock().push(request.clone());
                let response = route(&request, n);
                let mut out = format!("HTTP/1.1 {} X\r\ncontent-length: {}\r\nconnection: close\r\n", response.status, response.body.len());
                for (k, v) in &response.headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                out.push_str(&response.body);
                let _ = socket.write_all(out.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    Mock { base: format!("http://127.0.0.1:{port}"), port, requests, task }
}

pub async fn read_request<S: tokio::io::AsyncRead + Unpin>(socket: &mut S) -> Option<Req> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    let length = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse::<usize>().ok()).unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    Some(Req { method, path, query, headers, body: String::from_utf8_lossy(&body).to_string() })
}

/// A port nothing listens on.
pub fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}
