//! A fake MCP server speaking the 2026-07-28 revision as `@modelcontextprotocol/server` 2.4.0 does (results carry
//! `resultType: "complete"`, `server/discover` answers the probe, `subscriptions/listen` is acknowledged), or the
//! legacy `initialize` handshake. It answers over stdio (this test binary re-executed, see `run_stdio`) or over
//! Streamable HTTP (`serve_http`). Optionally offers MCP Events.

#![allow(dead_code)]

use std::io::{BufRead, Write};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::mpsc;

pub const SUBSCRIPTION_ID: &str = "io.modelcontextprotocol/subscriptionId";
pub const SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// What the fake does, read from the environment over stdio.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Speaks only the legacy handshake: `server/discover` is an unknown method.
    pub legacy: bool,
    /// Legacy only: exits during `initialize` unless the client declares roots.
    pub require_roots: bool,
    /// Legacy only: refuses to list tools unless the client declared roots.
    pub require_roots_to_list: bool,
    /// Announces `tools.listChanged`.
    pub list_changed: bool,
    /// Offers MCP Events.
    pub events: bool,
    /// Appends the process id here when it starts.
    pub pid_file: Option<String>,
    /// Appends every request method here.
    pub wire_log: Option<String>,
}

impl Options {
    pub fn from_env() -> Options {
        let flag = |key: &str| std::env::var(key).is_ok_and(|v| v == "1");
        Options {
            legacy: flag("FAKE_LEGACY"),
            require_roots: flag("FAKE_REQUIRE_ROOTS"),
            require_roots_to_list: flag("FAKE_REQUIRE_ROOTS_TO_LIST"),
            list_changed: flag("FAKE_LIST_CHANGED"),
            events: flag("FAKE_EVENTS"),
            pid_file: std::env::var("FAKE_PID_FILE").ok(),
            wire_log: std::env::var("FAKE_WIRE_LOG").ok(),
        }
    }
}

/// A server's recorded state.
#[derive(Default)]
pub struct State {
    pub tool_lists: usize,
    pub tool_calls: Vec<Value>,
    pub extra_tools: Vec<Value>,
    pub methods: Vec<String>,
    /// The client's declared capabilities, from its handshake.
    pub capabilities: Option<Value>,
    /// `events/stream` params, in order.
    pub streams: Vec<Value>,
    pub cancelled: Vec<Value>,
}

/// What the server sends back for one message.
pub enum Reply {
    None,
    Message(Value),
    /// Keep the request open as a stream (the id); notifications flow on it until it is closed.
    Stream(Value),
    /// The process exits (stdio) or the connection drops (HTTP).
    Exit,
}

pub struct Fake {
    pub options: Options,
    pub state: Mutex<State>,
}

fn complete(mut result: Value) -> Value {
    result["resultType"] = json!("complete");
    result
}

impl Fake {
    pub fn new(options: Options) -> Arc<Fake> {
        Arc::new(Fake { options, state: Mutex::new(State::default()) })
    }

    fn modern(&self, message: &Value) -> bool {
        message.get("params").and_then(|p| p.get("_meta")).and_then(|m| m.get("io.modelcontextprotocol/protocolVersion")).is_some()
    }

    fn result(&self, id: &Value, result: Value, modern: bool) -> Value {
        let result = if modern {
            let mut result = complete(result);
            result["_meta"] = json!({ SERVER_INFO: {"name": "fake-modern", "version": "1.0.0"} });
            result
        } else {
            result
        };
        json!({"jsonrpc": "2.0", "id": id, "result": result})
    }

    fn error(id: &Value, code: i64, message: &str) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
    }

    fn tools(&self) -> Vec<Value> {
        let mut tools = vec![
            json!({"name": "echo", "inputSchema": {"type": "object", "properties": {}}}),
            json!({"name": "url-elicitation", "inputSchema": {"type": "object", "properties": {}}}),
            json!({"name": "roots", "inputSchema": {"type": "object", "properties": {}}}),
        ];
        tools.extend(self.state.lock().extra_tools.clone());
        tools
    }

    fn capabilities(&self) -> Value {
        let mut capabilities = json!({"tools": {}});
        if self.options.list_changed {
            capabilities["tools"] = json!({"listChanged": true});
        }
        if self.options.events {
            capabilities["extensions"] = json!({"io.modelcontextprotocol/events": {}});
        }
        capabilities
    }

    fn log(&self, method: &str) {
        self.state.lock().methods.push(method.to_string());
        if let Some(path) = &self.options.wire_log {
            let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
            writeln!(file, "{method}").unwrap();
        }
    }

    /// Handles one client message.
    pub fn handle(&self, message: &Value) -> Reply {
        let method = message.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        if !method.is_empty() {
            self.log(&method);
        }
        if method == "notifications/cancelled" {
            self.state.lock().cancelled.push(params.get("requestId").cloned().unwrap_or(Value::Null));
            return Reply::None;
        }
        let Some(id) = message.get("id").cloned() else { return Reply::None };
        if method.is_empty() {
            return Reply::None;
        }
        let modern = self.modern(message);
        if modern && self.options.legacy {
            return Reply::Message(Self::error(&id, -32601, "Method not found"));
        }
        let declared_roots = |state: &State| state.capabilities.as_ref().is_some_and(|c| c.get("roots").is_some());
        match method.as_str() {
            "server/discover" => {
                let capabilities = params.get("_meta").and_then(|m| m.get("io.modelcontextprotocol/clientCapabilities")).cloned();
                self.state.lock().capabilities = capabilities;
                Reply::Message(self.result(
                    &id,
                    json!({"supportedVersions": ["2026-07-28"], "capabilities": self.capabilities(), "ttlMs": 0, "cacheScope": "private"}),
                    true,
                ))
            }
            "initialize" => {
                let capabilities = params.get("capabilities").cloned();
                let roots = capabilities.as_ref().is_some_and(|c| c.get("roots").is_some());
                self.state.lock().capabilities = capabilities;
                if self.options.require_roots && !roots {
                    return Reply::Exit;
                }
                Reply::Message(self.result(
                    &id,
                    json!({
                        "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-11-25")),
                        "capabilities": self.capabilities(),
                        "serverInfo": {"name": "fake-legacy", "version": "1.0.0"}
                    }),
                    false,
                ))
            }
            "subscriptions/listen" => Reply::Stream(id),
            "tools/list" => {
                if self.options.require_roots_to_list && !declared_roots(&self.state.lock()) {
                    return Reply::Message(Self::error(&id, -32603, "roots required"));
                }
                self.state.lock().tool_lists += 1;
                Reply::Message(self.result(&id, json!({"tools": self.tools()}), modern))
            }
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                self.state.lock().tool_calls.push(params.clone());
                let responses = params.get("inputResponses").cloned();
                match name.as_str() {
                    "url-elicitation" => match responses {
                        None => Reply::Message(json!({"jsonrpc": "2.0", "id": id, "result": {
                            "resultType": "input_required",
                            "inputRequests": {"confirm": {"method": "elicitation/create", "params": {
                                "mode": "url", "message": "Confirm in the browser", "url": "https://example.com/confirm"
                            }}},
                            "requestState": "pending"
                        }})),
                        Some(responses) => {
                            let action = responses["confirm"]["action"].clone();
                            Reply::Message(self.result(&id, json!({"content": [], "structuredContent": {"action": action}}), true))
                        }
                    },
                    "roots" => match responses {
                        None => Reply::Message(json!({"jsonrpc": "2.0", "id": id, "result": {
                            "resultType": "input_required",
                            "inputRequests": {"roots": {"method": "roots/list"}},
                            "requestState": "roots"
                        }})),
                        Some(responses) => Reply::Message(self.result(
                            &id,
                            json!({"content": [], "structuredContent": {"kind": "roots", "roots": responses["roots"]["roots"]}}),
                            true,
                        )),
                    },
                    _ => Reply::Message(self.result(&id, json!({"content": [{"type": "text", "text": "ok"}]}), modern)),
                }
            }
            "events/list" => Reply::Message(self.result(
                &id,
                json!({"events": [{"name": "job.finished", "description": "A job ended", "delivery": ["push"]}]}),
                modern,
            )),
            "events/stream" => {
                self.state.lock().streams.push(params);
                Reply::Stream(id)
            }
            "ping" => Reply::Message(self.result(&id, json!({}), modern)),
            _ => Reply::Message(Self::error(&id, -32601, "Method not found")),
        }
    }
}

/// The acknowledgement a `subscriptions/listen` stream starts with.
pub fn acknowledged(id: &Value) -> Value {
    json!({"jsonrpc": "2.0", "method": "notifications/subscriptions/acknowledged", "params": {"notifications": {}, "_meta": {SUBSCRIPTION_ID: id}}})
}

/// Serves the fake over this process's stdin/stdout until stdin closes.
pub fn run_stdio() {
    let options = Options::from_env();
    if let Some(path) = &options.pid_file {
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        writeln!(file, "{}", std::process::id()).unwrap();
    }
    let fake = Fake::new(options);
    let mut stdout = std::io::stdout();
    // libtest printed "test <name> ... " without a newline; terminate that line.
    stdout.write_all(b"\n").unwrap();
    stdout.flush().unwrap();
    let write = |stdout: &mut std::io::Stdout, value: &Value| {
        let mut line = serde_json::to_string(value).unwrap();
        line.push('\n');
        stdout.write_all(line.as_bytes()).unwrap();
        stdout.flush().unwrap();
    };
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        match fake.handle(&message) {
            Reply::None => {}
            Reply::Message(value) => write(&mut stdout, &value),
            Reply::Stream(id) => {
                if message["method"] == "subscriptions/listen" {
                    write(&mut stdout, &acknowledged(&id));
                }
            }
            Reply::Exit => std::process::exit(1),
        }
    }
}

/// An open stream on the HTTP fake: what the test sends down it.
pub struct OpenStream {
    pub id: Value,
    pub params: Value,
    pub sender: mpsc::UnboundedSender<Option<Value>>,
}

impl OpenStream {
    /// Sends a `notifications/events/<kind>` naming this stream.
    pub fn notify(&self, kind: &str, mut params: Value) {
        params["_meta"] = json!({SUBSCRIPTION_ID: self.id});
        let _ = self.sender.send(Some(json!({"jsonrpc": "2.0", "method": format!("notifications/events/{kind}"), "params": params})));
    }

    /// Ends the stream with its result.
    pub fn close(&self) {
        let _ = self.sender.send(Some(
            json!({"jsonrpc": "2.0", "id": self.id, "result": {"resultType": "complete", "_meta": {SUBSCRIPTION_ID: self.id}}}),
        ));
        let _ = self.sender.send(None);
    }
}

/// The HTTP fake's handle.
pub struct Http {
    pub url: String,
    pub fake: Arc<Fake>,
    /// Answers every POST with this status instead, while set.
    pub refuse: Arc<Mutex<Option<u16>>>,
    pub refused: Arc<Mutex<usize>>,
    /// `subscriptions/listen` streams, which a list change notifies.
    listeners: Arc<Mutex<Vec<mpsc::UnboundedSender<Option<Value>>>>>,
    /// Each `events/stream` opened.
    pub opened: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<OpenStream>>>,
    opener: mpsc::UnboundedSender<OpenStream>,
}

impl Http {
    pub fn send_tool_list_changed(&self) {
        for listener in self.listeners.lock().iter() {
            let _ = listener.send(Some(json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})));
        }
    }

    pub async fn next_stream(&self) -> OpenStream {
        tokio::time::timeout(std::time::Duration::from_secs(20), self.opened.lock().await.recv())
            .await
            .expect("an events/stream opened")
            .expect("stream")
    }
}

fn sse(rx: mpsc::UnboundedReceiver<Option<Value>>, first: Vec<Value>) -> axum::response::Response {
    use axum::body::Body;
    let frame = |v: Value| Ok::<String, std::io::Error>(format!("event: message\ndata: {v}\n\n"));
    let first = futures::stream::iter(first.into_iter().map(frame));
    let rest = futures::stream::unfold(rx, move |mut rx| async move { rx.recv().await.flatten().map(|v| (frame(v), rx)) });
    axum::response::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(futures::StreamExt::chain(first, rest)))
        .unwrap()
}

/// Serves the fake over Streamable HTTP on 127.0.0.1 (modern requests carry no session).
pub async fn serve_http(options: Options) -> Http {
    use axum::Router;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::post;

    let fake = Fake::new(options);
    let (opener, opened) = mpsc::unbounded_channel();
    #[derive(Clone)]
    struct Shared {
        fake: Arc<Fake>,
        refuse: Arc<Mutex<Option<u16>>>,
        refused: Arc<Mutex<usize>>,
        listeners: Arc<Mutex<Vec<mpsc::UnboundedSender<Option<Value>>>>>,
        opener: mpsc::UnboundedSender<OpenStream>,
    }
    let shared = Shared {
        fake: fake.clone(),
        refuse: Arc::default(),
        refused: Arc::default(),
        listeners: Arc::default(),
        opener: opener.clone(),
    };
    async fn handle(State(shared): State<Shared>, _headers: HeaderMap, body: String) -> axum::response::Response {
        use axum::body::Body;
        if let Some(status) = *shared.refuse.lock() {
            *shared.refused.lock() += 1;
            return axum::response::Response::builder().status(status).body(Body::empty()).unwrap();
        }
        let message: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        match shared.fake.handle(&message) {
            Reply::None | Reply::Exit => axum::response::Response::builder().status(202).body(Body::empty()).unwrap(),
            Reply::Message(value) => axum::response::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(Body::from(value.to_string()))
                .unwrap(),
            Reply::Stream(id) => {
                let (tx, rx) = mpsc::unbounded_channel();
                if message["method"] == "subscriptions/listen" {
                    shared.listeners.lock().push(tx);
                    return sse(rx, vec![acknowledged(&id)]);
                }
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let _ = shared.opener.send(OpenStream { id, params, sender: tx });
                sse(rx, vec![])
            }
        }
    }
    let app = Router::new().route("/mcp", post(handle).get(|| async { axum::http::StatusCode::METHOD_NOT_ALLOWED })).with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Http {
        url: format!("http://127.0.0.1:{port}/mcp"),
        fake,
        refuse: shared.refuse,
        refused: shared.refused,
        listeners: shared.listeners,
        opened: Arc::new(tokio::sync::Mutex::new(opened)),
        opener,
    }
}
