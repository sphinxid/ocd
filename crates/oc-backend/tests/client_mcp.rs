//! Experimental client-hosted MCP end to end (2.3.1 `server/test/client-mcp.test.ts`): a fake client opens a WebSocket to
//! `/api/experimental/client-mcp?name=tui` and hosts an MCP server over it; the location's sessions see its tools
//! (through Code Mode, like any MCP server's) for as long as the socket stays open, calls carry the calling session, and
//! the socket closes with 1011 when the attach fails and with 1012 when the location reloads.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::{Router, body::Body, response::Response, routing::post};
use common::*;
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

enum Turn {
    Tool(&'static str, Value),
    Text(&'static str),
}

fn sse(chunks: &[Value]) -> Response {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    Response::builder().header("content-type", "text/event-stream").body(Body::from(body)).unwrap()
}

/// An OpenAI-compatible mock answering with `turns`, recording the request bodies.
async fn provider(turns: Vec<Turn>) -> (u16, Arc<Mutex<Vec<Value>>>) {
    let bodies = Arc::new(Mutex::new(Vec::<Value>::new()));
    let script = Arc::new(Mutex::new(turns));
    let recorded = bodies.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::Json<Value>| {
            let is_title = body.0.to_string().contains("You name conversations");
            let turn = if is_title {
                None
            } else {
                recorded.lock().push(body.0.clone());
                let mut script = script.lock();
                Some(if script.is_empty() { Turn::Text("done") } else { script.remove(0) })
            };
            async move {
                let usage = json!({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15});
                match turn {
                    None => sse(&[
                        json!({"id":"t","choices":[{"index":0,"delta":{"role":"assistant","content":"Title"}}]}),
                        json!({"id":"t","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":usage}),
                    ]),
                    Some(Turn::Tool(name, args)) => sse(&[
                        json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]}),
                        json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":usage}),
                    ]),
                    Some(Turn::Text(text)) => sse(&[
                        json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","content":text}}]}),
                        json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":usage}),
                    ]),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (port, bodies)
}

struct Setup {
    h: Harness,
    /// The server's base URL (`http://127.0.0.1:<port>`).
    base: String,
    bodies: Arc<Mutex<Vec<Value>>>,
    events: Mutex<tokio::sync::broadcast::Receiver<oc_core::bus::Event>>,
}

/// A backend with the session runner and a mock provider, served over TCP so clients can open WebSockets.
async fn setup(turns: Vec<Turn>) -> Setup {
    let (port, bodies) = provider(turns).await;
    // SAFETY: tests only ever set this to the same value.
    unsafe { std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1") };
    let root = tempfile::tempdir().unwrap();
    let root_path = oc_util::fs::canonicalize(root.path()).unwrap();
    let cwd = root_path.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let config = json!({
        "model": "mock/m1",
        "providers": {"mock": {
            "name": "Mock",
            "package": "@opencode/ai/providers/openai-compatible",
            "settings": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "test"},
            "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}}}
        }}
    });
    std::fs::write(cwd.join("opencode.json"), config.to_string()).unwrap();
    let db = oc_storage::Database::memory().unwrap();
    let app = oc_core::app::App::new(global(&root_path), db, "1.2.3");
    let services = oc_core::services::default_services(&app);
    let runner = oc_core::runner::Engine::new(app.clone(), &services);
    let opts = oc_server::ServerOptions { cwd: Some(cwd.display().to_string()), version: "1.2.3".into(), ready: true, ..Default::default() };
    let (router, _) = oc_backend::router(app.clone(), Some(runner), &opts).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let served = router.clone();
    tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });
    let engine = oc_backend::Engine::new(app.clone());
    let events = Mutex::new(app.bus.subscribe());
    let h = Harness { app, engine, router, root, cwd };
    Setup { h, base, bodies, events }
}

impl Setup {
    fn url(&self, query: &str) -> String {
        format!("{}/api/experimental/client-mcp?{query}", self.base.replacen("http://", "ws://", 1))
    }

    /// Waits for the next `mcp.tools.changed` of `server`.
    async fn tools_changed(&self, server: &str) {
        let found = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let next = self.events.lock().try_recv();
                match next {
                    Ok(e) if e.kind == "mcp.tools.changed" && e.data["server"] == server => return,
                    Ok(_) => {}
                    Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
        })
        .await;
        assert!(found.is_ok(), "no mcp.tools.changed for {server}");
    }

    /// Prompts the session and waits until it is idle.
    async fn run(&self, session: &str, text: &str) {
        let r = self.h.post(&format!("/api/session/{session}/prompt"), json!({ "text": text })).await;
        assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::timeout(Duration::from_secs(60), self.h.app.coordinator().await_idle(session)).await.unwrap();
    }
}

/// What a fake client saw: the requests it received, the sessions of its tool calls and how its socket closed.
#[derive(Default)]
struct Seen {
    methods: Vec<String>,
    sessions: Vec<Option<String>>,
    initialize: Option<Value>,
    close: Option<(u16, String)>,
}

/// A client hosting an MCP server with `open_terminal` over a WebSocket to `url`; `reject` answers `initialize` with an
/// error. Dropping the sender closes the socket.
async fn client(url: &str, label: &'static str, reject: bool) -> (Arc<Mutex<Seen>>, tokio::sync::oneshot::Sender<()>) {
    let (ws, response) = tokio_tungstenite::connect_async(url).await.unwrap();
    assert_eq!(response.status(), 101);
    let (mut sink, mut stream) = ws.split();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (close_tx, mut close_rx) = tokio::sync::oneshot::channel::<()>();
    let out = seen.clone();
    tokio::spawn(async move {
        loop {
            let message = tokio::select! {
                _ = &mut close_rx => { let _ = sink.send(Message::Close(None)).await; let _ = sink.close().await; return }
                message = stream.next() => message,
            };
            let text = match message {
                Some(Ok(Message::Text(text))) => text.to_string(),
                Some(Ok(Message::Close(frame))) => {
                    out.lock().close = frame.map(|f| (u16::from(f.code), f.reason.to_string()));
                    return;
                }
                Some(Ok(_)) => continue,
                _ => return,
            };
            let message: Value = serde_json::from_str(&text).unwrap();
            let method = message["method"].as_str().unwrap_or("").to_string();
            out.lock().methods.push(method.clone());
            let id = message.get("id").cloned();
            let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
            let response = match method.as_str() {
                "initialize" if reject => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": "not today"}}),
                "initialize" => {
                    out.lock().initialize = Some(message["params"].clone());
                    reply(json!({
                        "protocolVersion": message["params"]["protocolVersion"],
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": label, "version": "1.0.0"},
                    }))
                }
                "tools/list" => reply(json!({"tools": [{"name": "open_terminal", "description": "Open a terminal", "inputSchema": {"type": "object"}}]})),
                "tools/call" => {
                    out.lock().sessions.push(message["params"]["_meta"]["ai.opencode/sessionID"].as_str().map(String::from));
                    reply(json!({"content": [{"type": "text", "text": format!("opened by {label}")}]}))
                }
                _ if id.is_some() => reply(json!({})),
                _ => continue,
            };
            // Frames should be text, but binary frames are decoded as UTF-8 too.
            let frame = if label == "binary" { Message::Binary(response.to_string().into_bytes().into()) } else { Message::Text(response.to_string().into()) };
            if sink.send(frame).await.is_err() {
                return;
            }
        }
    });
    (seen, close_tx)
}

async fn wait_until(mut check: impl FnMut() -> bool) {
    for _ in 0..2000 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not met");
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"].as_array().into_iter().flatten().filter_map(|t| t["function"]["name"].as_str().map(String::from)).collect()
}

/// What a request offers: its tool definitions and system messages (not the conversation, which may quote a call).
fn offered(body: &Value) -> String {
    let system: Vec<&Value> = body["messages"].as_array().into_iter().flatten().filter(|m| m["role"] == "system").collect();
    format!("{} {}", body["tools"], serde_json::to_string(&system).unwrap())
}

const CALL: &str = "return await tools.tui.open_terminal({})";

#[tokio::test]
async fn attaches_a_client_mcp_server_for_the_lifetime_of_its_socket() {
    let s = setup(vec![
        Turn::Tool("execute", json!({ "code": CALL })),
        Turn::Text("opened"),
        Turn::Text("gone"),
    ])
    .await;
    let session = s.h.session().await;
    let dir = enc(&s.h.dir());
    let (seen, close) = client(&s.url(&format!("location[directory]={dir}&name=tui")), "binary", false).await;
    s.tools_changed("tui").await;
    // The handshake: legacy `initialize` with opencode's client info, roots and form/url elicitation.
    let initialize = seen.lock().initialize.clone().unwrap();
    assert_eq!(initialize["protocolVersion"], "2025-11-25");
    assert_eq!(initialize["clientInfo"], json!({"name": "opencode", "version": "1.2.3"}));
    assert_eq!(initialize["capabilities"], json!({"elicitation": {"form": {}, "url": {}}, "roots": {}}));
    // Attached clients are never listed.
    assert_eq!(s.h.get("/api/mcp").await.json()["data"], json!([]));

    s.run(&session, "open a terminal").await;
    assert_eq!(seen.lock().sessions, [Some(session.clone())]);
    assert_eq!(seen.lock().methods, ["initialize", "notifications/initialized", "tools/list", "tools/call"]);
    let bodies = s.bodies.lock().clone();
    // Through Code Mode by default, like any MCP server's tools.
    assert!(tool_names(&bodies[0]).contains(&"execute".to_string()));
    assert!(!tool_names(&bodies[0]).iter().any(|t| t.contains("open_terminal")));
    assert!(offered(&bodies[0]).contains("tui.open_terminal"), "Code Mode documents tui.open_terminal");
    assert!(bodies[1].to_string().contains("opened by binary"), "{}", bodies[1]);

    // Once the socket closes the tools are gone.
    close.send(()).unwrap();
    s.tools_changed("tui").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    s.run(&session, "again").await;
    let bodies = s.bodies.lock().clone();
    assert!(tool_names(bodies.last().unwrap()).contains(&"execute".to_string()));
    // The session's catalog says the namespace went away.
    let last = bodies.last().unwrap()["messages"].to_string();
    assert!(last.contains("The following tool namespaces are no longer available and must not be used: `tui`."), "{last}");
}

#[tokio::test]
async fn a_second_client_with_the_same_name_stacks_and_the_first_takes_over_when_it_closes() {
    let s = setup(vec![
        Turn::Tool("execute", json!({ "code": CALL })),
        Turn::Text("newer"),
        Turn::Tool("execute", json!({ "code": CALL })),
        Turn::Text("older"),
    ])
    .await;
    let session = s.h.session().await;
    let (first, _first_close) = client(&s.url("name=tui"), "first", false).await;
    s.tools_changed("tui").await;
    let (second, second_close) = client(&s.url("name=tui"), "second", false).await;
    s.tools_changed("tui").await;
    s.run(&session, "open").await;
    assert_eq!(second.lock().sessions, [Some(session.clone())]);
    assert!(first.lock().sessions.is_empty());
    assert!(first.lock().close.is_none());
    second_close.send(()).unwrap();
    wait_until(|| second.lock().methods.len() >= 4).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    s.run(&session, "open again").await;
    assert_eq!(first.lock().sessions, [Some(session.clone())]);
}

#[tokio::test]
async fn closes_with_1011_when_the_client_rejects_initialize() {
    let s = setup(vec![]).await;
    let (seen, _close) = client(&s.url("name=tui"), "tui", true).await;
    wait_until(|| seen.lock().close.is_some()).await;
    assert_eq!(seen.lock().close, Some((1011, "attach failed".to_string())));
}

#[tokio::test]
async fn closes_with_1012_on_reload_and_attaches_again_to_the_fresh_location() {
    let s = setup(vec![]).await;
    let (seen, _close) = client(&s.url("name=tui"), "tui", false).await;
    s.tools_changed("tui").await;
    let r = s.h.req("POST", "/api/location/reload", None).await;
    assert_eq!(r.status, 204);
    wait_until(|| seen.lock().close.is_some()).await;
    assert_eq!(seen.lock().close, Some((1012, "location shut down".to_string())));
    // A reconnecting client attaches to the replacement location.
    let (again, _again_close) = client(&s.url("name=tui"), "tui", false).await;
    s.tools_changed("tui").await;
    assert!(again.lock().close.is_none());
}

#[tokio::test]
async fn refuses_bad_requests_before_upgrading() {
    let s = setup(vec![]).await;
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let get = |query: &str| http.get(format!("{}/api/experimental/client-mcp?{query}", s.base)).send();
    // Without an upgrade: 400, empty.
    let r = get("name=tui").await.unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(r.text().await.unwrap(), "");
    let r = get("name=bad%20name").await.unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(
        r.json::<Value>().await.unwrap(),
        json!({"_tag": "InvalidRequestError", "message": "Expected a string matching the RegExp ^[a-zA-Z0-9_-]+$\n  at [\"name\"]", "kind": "Query"})
    );
    let r = get("").await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["message"], "Missing key\n  at [\"name\"]");
    let r = get("name=a&name=b").await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["message"], "Expected string\n  at [\"name\"]");
    // A missing location answers 404 once the request upgrades.
    let missing = s.h.cwd.join("missing");
    let url = s.url(&format!("location[directory]={}&name=tui", enc(&missing.display().to_string())));
    match tokio_tungstenite::connect_async(url).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), 404);
            let body: Value = serde_json::from_slice(response.body().as_deref().unwrap()).unwrap();
            assert_eq!(body["_tag"], "LocationNotFoundError");
        }
        other => panic!("{other:?}"),
    }
    // A bad name never opens the socket.
    match tokio_tungstenite::connect_async(s.url("name=bad.name")).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => assert_eq!(response.status(), 400),
        other => panic!("{other:?}"),
    }
}
