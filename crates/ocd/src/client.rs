//! A small HTTP client of the opencode v2 API, mirroring the generated TypeScript client
//! (`packages/client/src/promise/generated/client.ts`): operations are looked up by operationId in
//! the OpenAPI document, declared error statuses become `Declared` errors carrying the server body,
//! everything else is a `ClientError` with reason `Transport`/`UnexpectedStatus`/... .

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use base64::Engine;
use futures::StreamExt;
use serde_json::Value;

/// Where a server lives and how to authenticate (`Service.Endpoint`).
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub url: String,
    /// Basic auth password (username is always `opencode`).
    pub password: Option<String>,
}

impl Endpoint {
    /// `Service.headers(endpoint)`
    pub fn authorization(&self) -> Option<String> {
        self.password.as_ref().map(|p| {
            format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("opencode:{p}")))
        })
    }
}

#[derive(Debug, Clone)]
pub enum ClientError {
    /// A declared error status: the decoded body (`_tag`, `message`, ...).
    Declared { status: u16, body: Value },
    /// `ClientError(reason, detail)`
    Client { reason: &'static str, detail: Option<String>, status: Option<u16> },
}

impl ClientError {
    pub fn transport(detail: impl Into<String>) -> Self {
        ClientError::Client { reason: "Transport", detail: Some(detail.into()), status: None }
    }
    pub fn tag(&self) -> Option<&str> {
        match self {
            ClientError::Declared { body, .. } => body.get("_tag").and_then(Value::as_str),
            _ => None,
        }
    }
    pub fn status(&self) -> Option<u16> {
        match self {
            ClientError::Declared { status, .. } => Some(*status),
            ClientError::Client { status, .. } => *status,
        }
    }
    pub fn is_transport(&self) -> bool {
        matches!(self, ClientError::Client { reason: "Transport", .. })
    }
    pub fn body(&self) -> Option<&Value> {
        match self {
            ClientError::Declared { body, .. } => Some(body),
            _ => None,
        }
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Declared { body, .. } => {
                let msg = body
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| body.pointer("/data/message").and_then(Value::as_str))
                    .unwrap_or("");
                f.write_str(msg)
            }
            ClientError::Client { reason, detail, .. } => match detail {
                Some(d) if !d.is_empty() => write!(f, "{reason}: {d}"),
                _ => f.write_str(reason),
            },
        }
    }
}

impl std::error::Error for ClientError {}

#[derive(Debug, Clone)]
pub struct OpInfo {
    pub method: String,
    pub path: String,
    pub success: u16,
    pub declared: Vec<u16>,
}

/// operationId → (method, path, statuses), from the checked-in OpenAPI document.
pub static OPS: LazyLock<HashMap<String, OpInfo>> = LazyLock::new(|| {
    let doc: Value = serde_json::from_str(oc_server::OPENAPI_JSON).unwrap_or(Value::Null);
    let mut out = HashMap::new();
    if let Some(paths) = doc.get("paths").and_then(Value::as_object) {
        for (path, methods) in paths {
            let Some(methods) = methods.as_object() else { continue };
            for (method, op) in methods {
                let Some(id) = op.get("operationId").and_then(Value::as_str) else { continue };
                let statuses: Vec<u16> =
                    op.get("responses").and_then(Value::as_object).map(|r| r.keys().filter_map(|k| k.parse().ok()).collect()).unwrap_or_default();
                let success = statuses.iter().copied().find(|s| (200..300).contains(s)).unwrap_or(200);
                let declared = statuses.into_iter().filter(|s| *s != success).collect();
                out.insert(id.to_string(), OpInfo { method: method.to_uppercase(), path: path.clone(), success, declared });
            }
        }
    }
    out
});

/// `encodeURIComponent`
pub fn encode_component(s: &str) -> String {
    const SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'!')
        .remove(b'~')
        .remove(b'*')
        .remove(b'\'')
        .remove(b'(')
        .remove(b')');
    percent_encoding::utf8_percent_encode(s, SET).to_string()
}

/// `appendQuery(params, key, value)`.
pub fn append_query(out: &mut Vec<(String, String)>, key: &str, value: &Value) {
    match value {
        Value::Null => out.push((key.into(), "null".into())),
        Value::Array(items) => {
            for i in items {
                append_query(out, key, i)
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                append_query(out, &format!("{key}[{k}]"), v)
            }
        }
        Value::String(s) => out.push((key.into(), s.clone())),
        Value::Bool(b) => out.push((key.into(), b.to_string())),
        Value::Number(n) => out.push((key.into(), n.to_string())),
    }
}

#[derive(Clone)]
pub struct Client {
    pub endpoint: Endpoint,
    http: reqwest::Client,
    extra_headers: Vec<(String, String)>,
}

/// One request.
#[derive(Default)]
pub struct Req<'a> {
    pub path: Vec<(&'a str, String)>,
    pub query: Vec<(&'a str, Value)>,
    pub body: Option<Value>,
    pub headers: Vec<(String, String)>,
    pub timeout: Option<Duration>,
}

impl<'a> Req<'a> {
    pub fn new() -> Self {
        Req::default()
    }
    pub fn path(mut self, k: &'a str, v: impl Into<String>) -> Self {
        self.path.push((k, v.into()));
        self
    }
    pub fn query(mut self, k: &'a str, v: Value) -> Self {
        if !v.is_null() || k == "parentID" {
            self.query.push((k, v));
        }
        self
    }
    /// `location: {directory}`
    pub fn location(self, directory: &str) -> Self {
        self.query("location", serde_json::json!({ "directory": directory }))
    }
    pub fn body(mut self, b: Value) -> Self {
        self.body = Some(b);
        self
    }
    pub fn header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }
}

impl Client {
    pub fn new(endpoint: &Endpoint) -> Client {
        let http = reqwest::Client::builder().no_proxy().build().unwrap_or_default();
        Client { endpoint: endpoint.clone(), http, extra_headers: vec![] }
    }

    fn url(&self, path: &str, query: &[(String, String)]) -> Result<url::Url, ClientError> {
        let mut base = url::Url::parse(&self.endpoint.url).map_err(|e| ClientError::transport(e.to_string()))?;
        if !base.path().ends_with('/') {
            let p = format!("{}/", base.path());
            base.set_path(&p);
        }
        let mut url = base.join(path.trim_start_matches('/')).map_err(|e| ClientError::transport(e.to_string()))?;
        if !query.is_empty() {
            let mut q = url.query_pairs_mut();
            for (k, v) in query {
                q.append_pair(k, v);
            }
        }
        Ok(url)
    }

    fn prepare(&self, op: &OpInfo, req: &Req) -> Result<reqwest::RequestBuilder, ClientError> {
        let mut path = op.path.clone();
        for (k, v) in &req.path {
            path = path.replace(&format!("{{{k}}}"), &encode_component(v));
        }
        let mut query = vec![];
        for (k, v) in &req.query {
            append_query(&mut query, k, v);
        }
        let url = self.url(&path, &query)?;
        let method = reqwest::Method::from_bytes(op.method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut b = self.http.request(method, url);
        if let Some(a) = self.endpoint.authorization() {
            b = b.header("authorization", a);
        }
        for (k, v) in self.extra_headers.iter().chain(req.headers.iter()) {
            b = b.header(k.as_str(), v.as_str());
        }
        if let Some(body) = &req.body {
            b = b.header("content-type", "application/json").body(serde_json::to_vec(body).unwrap_or_default());
        }
        if let Some(t) = req.timeout {
            b = b.timeout(t);
        }
        Ok(b)
    }

    async fn error(op: &OpInfo, response: reqwest::Response) -> ClientError {
        let status = response.status().as_u16();
        if op.declared.contains(&status) {
            return match json(response).await {
                Ok(body) => ClientError::Declared { status, body },
                Err(e) => e,
            };
        }
        ClientError::Client { reason: "UnexpectedStatus", detail: Some(status.to_string()), status: Some(status) }
    }

    /// Call `operation` and decode the JSON body (`Value::Null` for 204 operations).
    pub async fn call(&self, operation: &str, req: Req<'_>) -> Result<Value, ClientError> {
        let op = OPS.get(operation).cloned().ok_or_else(|| ClientError::transport(format!("unknown operation {operation}")))?;
        let response = self.prepare(&op, &req)?.send().await.map_err(|e| ClientError::transport(error_text(&e)))?;
        if response.status().as_u16() != op.success {
            return Err(Self::error(&op, response).await);
        }
        if op.success == 204 {
            return Ok(Value::Null);
        }
        json(response).await
    }

    /// `event.subscribe()`
    pub async fn subscribe(&self) -> Result<EventStream, ClientError> {
        let op = OPS.get("event.subscribe").cloned().unwrap_or(OpInfo {
            method: "GET".into(),
            path: "/api/event".into(),
            success: 200,
            declared: vec![400, 401],
        });
        let response = self.prepare(&op, &Req::new())?.send().await.map_err(|e| ClientError::transport(error_text(&e)))?;
        if response.status().as_u16() != op.success {
            return Err(Self::error(&op, response).await);
        }
        if content_type(&response) != "text/event-stream" {
            return Err(ClientError::Client {
                reason: "UnsupportedContentType",
                detail: response.headers().get("content-type").and_then(|v| v.to_str().ok()).map(String::from),
                status: None,
            });
        }
        Ok(EventStream { inner: Box::pin(response.bytes_stream()), buffer: String::new(), pending: Default::default(), done: false })
    }

    /// A raw request (used by `opencode api`).
    pub async fn raw(&self, method: &str, path_and_query: &str, headers: &[(String, String)], body: Option<String>) -> Result<reqwest::Response, ClientError> {
        let base = url::Url::parse(&self.endpoint.url).map_err(|e| ClientError::transport(e.to_string()))?;
        let url = base.join(path_and_query).map_err(|e| ClientError::transport(e.to_string()))?;
        let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| ClientError::transport(e.to_string()))?;
        let mut b = self.http.request(method, url);
        for (k, v) in headers {
            b = b.header(k.as_str(), v.as_str());
        }
        if let Some(body) = body {
            b = b.body(body);
        }
        b.send().await.map_err(|e| ClientError::transport(error_text(&e)))
    }
}

pub fn error_text(e: &reqwest::Error) -> String {
    let mut s = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(inner) = source {
        s = format!("{s}: {inner}");
        source = inner.source();
    }
    s
}

fn content_type(r: &reqwest::Response) -> String {
    r.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_default()
}

async fn json(response: reqwest::Response) -> Result<Value, ClientError> {
    let ct = content_type(&response);
    let raw = response.headers().get("content-type").and_then(|v| v.to_str().ok()).map(String::from);
    if ct != "application/json" && !raw.as_deref().is_some_and(|r| r.contains("+json")) {
        return Err(ClientError::Client { reason: "UnsupportedContentType", detail: raw, status: None });
    }
    let text = response.text().await.map_err(|e| ClientError::transport(error_text(&e)))?;
    if text.is_empty() {
        return Err(ClientError::Client { reason: "MalformedResponse", detail: None, status: None });
    }
    serde_json::from_str(&text).map_err(|e| ClientError::Client { reason: "MalformedResponse", detail: Some(e.to_string()), status: None })
}

const MAX_SSE_EVENT: usize = 16 * 1024 * 1024;

type ByteStream = std::pin::Pin<Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>;

/// SSE frames of `GET /api/event`, decoded like the generated client.
pub struct EventStream {
    inner: ByteStream,
    buffer: String,
    pending: std::collections::VecDeque<Value>,
    done: bool,
}

impl EventStream {
    /// Next event; `None` when the stream ended.
    pub async fn next(&mut self) -> Option<Result<Value, ClientError>> {
        loop {
            if let Some(v) = self.pending.pop_front() {
                return Some(Ok(v));
            }
            if self.done {
                return None;
            }
            let chunk = self.inner.next().await;
            let finished = match chunk {
                None => true,
                Some(Err(e)) => {
                    self.done = true;
                    return Some(Err(ClientError::transport(error_text(&e))));
                }
                Some(Ok(bytes)) => {
                    self.buffer.push_str(&String::from_utf8_lossy(&bytes));
                    false
                }
            };
            if self.buffer.len() > MAX_SSE_EVENT {
                self.done = true;
                return Some(Err(ClientError::Client { reason: "SseEventTooLarge", detail: None, status: None }));
            }
            let trailing_cr = !finished && self.buffer.ends_with('\r');
            if trailing_cr {
                self.buffer.pop();
            }
            self.buffer = self.buffer.replace("\r\n", "\n").replace('\r', "\n");
            if trailing_cr {
                self.buffer.push('\r');
            }
            if finished && !self.buffer.is_empty() {
                self.buffer.push_str("\n\n");
            }
            while let Some(boundary) = self.buffer.find("\n\n") {
                let block: String = self.buffer[..boundary].to_string();
                self.buffer = self.buffer[boundary + 2..].to_string();
                let data: Vec<&str> =
                    block.split('\n').filter_map(|l| l.strip_prefix("data:").map(|d| d.trim_start())).collect();
                let data = data.join("\n");
                if !data.is_empty() {
                    match serde_json::from_str(&data) {
                        Ok(v) => self.pending.push_back(v),
                        Err(e) => {
                            self.done = true;
                            return Some(Err(ClientError::Client { reason: "MalformedResponse", detail: Some(e.to_string()), status: None }));
                        }
                    }
                }
            }
            if finished {
                self.done = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ops_loaded() {
        let op = OPS.get("session.list").unwrap();
        assert_eq!(op.method, "GET");
        assert_eq!(op.path, "/api/session");
        assert_eq!(OPS.get("session.remove").unwrap().success, 204);
    }

    #[test]
    fn query_encoding() {
        let mut q = vec![];
        append_query(&mut q, "location", &serde_json::json!({"directory": "/a b"}));
        append_query(&mut q, "parentID", &Value::Null);
        assert_eq!(q, vec![("location[directory]".to_string(), "/a b".to_string()), ("parentID".into(), "null".into())]);
        assert_eq!(encode_component("ses/a b"), "ses%2Fa%20b");
    }
}
