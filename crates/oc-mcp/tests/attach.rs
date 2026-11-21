//! Client-hosted MCP servers attached over a socket the client opened (`McpManager::attach`), ported from upstream's
//! `core/test/mcp-attach.test.ts`. Each fake client hosts an in-memory MCP server behind a `SocketTransport`: what
//! opencode sends over the "socket" reaches the fake, and its replies come back through `deliver`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use oc_mcp::transport::socket::{Socket, SocketTransport};
use oc_mcp::transport::{Transport, TransportEvent};
use oc_mcp::{
    Attachment, CallToolContent, ManagerError, ManagerOptions, McpEvent, McpManager, ServerConfig,
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// An in-memory client MCP server whose tools answer with `label:tool` and record the caller's session.
struct Fake {
    label: String,
    tools: Mutex<Vec<String>>,
    sessions: Mutex<Vec<Option<String>>>,
    /// Answers `tools/list` with an error while set.
    fail_list: AtomicBool,
    /// Rejects `initialize` with an error.
    reject: AtomicBool,
    initialized: AtomicBool,
    closed: AtomicBool,
    /// Close codes opencode closed the socket with.
    close_codes: Mutex<Vec<u16>>,
    transport: OnceLock<Weak<SocketTransport>>,
}

struct FakeSocket(Arc<Fake>);

impl Socket for FakeSocket {
    fn send(&self, text: String) {
        let fake = self.0.clone();
        tokio::spawn(async move { fake.receive(serde_json::from_str(&text).unwrap()) });
    }

    fn close(&self, code: u16) {
        self.0.close_codes.lock().push(code);
        self.0.closed.store(true, Ordering::SeqCst);
    }
}

impl Fake {
    fn new(label: &str, tools: &[&str]) -> Arc<Fake> {
        Arc::new(Fake {
            label: label.to_string(),
            tools: Mutex::new(tools.iter().map(|t| t.to_string()).collect()),
            sessions: Mutex::default(),
            fail_list: AtomicBool::new(false),
            reject: AtomicBool::new(false),
            initialized: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            close_codes: Mutex::default(),
            transport: OnceLock::new(),
        })
    }

    /// The transport opencode attaches, and its event stream.
    fn transport(self: &Arc<Self>) -> (Arc<dyn Transport>, mpsc::UnboundedReceiver<TransportEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let transport = SocketTransport::new(Arc::new(FakeSocket(self.clone())), tx);
        let _ = self.transport.set(Arc::downgrade(&transport));
        (transport, rx)
    }

    fn deliver(&self, message: Value) {
        if let Some(transport) = self.transport.get().and_then(Weak::upgrade) {
            transport.deliver(&message.to_string());
        }
    }

    fn connected(&self) -> bool {
        !self.closed.load(Ordering::SeqCst)
    }

    /// The client closes its socket.
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(transport) = self.transport.get().and_then(Weak::upgrade) {
            transport.ended();
        }
    }

    /// Registers a tool after connecting, announcing the change like the SDK's `McpServer.registerTool`.
    fn register(&self, tool: &str) {
        self.tools.lock().push(tool.to_string());
        if self.initialized.load(Ordering::SeqCst) {
            self.deliver(json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}));
        }
    }

    fn receive(&self, message: Value) {
        let method = message["method"].as_str().unwrap_or("");
        let id = message.get("id").cloned();
        let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        let response = match method {
            "initialize" if self.reject.load(Ordering::SeqCst) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": "not today"}})
            }
            "initialize" => reply(json!({
                "protocolVersion": message["params"]["protocolVersion"],
                "capabilities": {"tools": {"listChanged": true}},
                "serverInfo": {"name": self.label, "version": "1.0.0"},
            })),
            "notifications/initialized" => {
                self.initialized.store(true, Ordering::SeqCst);
                return;
            }
            "tools/list" if self.fail_list.load(Ordering::SeqCst) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": "list failed"}})
            }
            "tools/list" => {
                let tools: Vec<Value> = self
                    .tools
                    .lock()
                    .iter()
                    .map(|t| json!({"name": t, "description": format!("{} {t}", self.label), "inputSchema": {"type": "object"}}))
                    .collect();
                reply(json!({"tools": tools}))
            }
            "tools/call" => {
                let name = message["params"]["name"].as_str().unwrap_or("").to_string();
                if !self.tools.lock().contains(&name) {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": format!("Tool {name} not found")}})
                } else {
                    let session = message["params"]["_meta"]["ai.opencode/sessionID"].as_str().map(String::from);
                    self.sessions.lock().push(session);
                    reply(json!({"content": [{"type": "text", "text": format!("{}:{name}", self.label)}]}))
                }
            }
            _ if id.is_some() => reply(json!({})),
            _ => return,
        };
        self.deliver(response);
    }
}

fn manager() -> (McpManager, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let manager = McpManager::new(ManagerOptions { directory: dir.path().to_path_buf(), ..Default::default() });
    (manager, dir)
}

async fn attach(manager: &McpManager, name: &str, fake: &Arc<Fake>) -> Result<Attachment, oc_mcp::ConnectError> {
    let (transport, events) = fake.transport();
    manager.attach(name, transport, events).await
}

/// Records the servers of `mcp.tools.changed` events.
fn tools_changed(manager: &McpManager) -> Arc<Mutex<Vec<String>>> {
    let servers = Arc::new(Mutex::new(Vec::new()));
    let mut events = manager.subscribe();
    let out = servers.clone();
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            if let McpEvent::ToolsChanged { server } = event {
                out.lock().push(server);
            }
        }
    });
    servers
}

async fn wait_until(mut check: impl FnMut() -> bool) {
    // Generous for loaded CI machines; a passing check returns at once.
    for _ in 0..4000 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition not met");
}

fn names(manager: &McpManager) -> Vec<String> {
    manager.tools().into_iter().map(|t| t.name).collect()
}

/// Waits until the session tool registry's view of the tools (debounced) has, or no longer has, `server_tool`.
async fn registered(manager: &McpManager, id: &str, present: bool) {
    wait_until(|| manager.registered_tools().iter().any(|t| format!("{}_{}", t.server, t.name) == id) == present).await;
}

async fn call(manager: &McpManager, server: &str, tool: &str, session: Option<&str>) -> Result<String, ManagerError> {
    let result = manager.call_tool(server, tool, None, session).await?;
    Ok(result
        .content
        .iter()
        .map(|c| match c {
            CallToolContent::Text { text } => text.clone(),
            _ => String::new(),
        })
        .collect())
}

#[tokio::test]
async fn attaches_a_client_and_routes_its_tools_with_the_session() {
    let (mcp, _dir) = manager();
    let changes = tools_changed(&mcp);
    let client = Fake::new("tui", &["open_terminal"]);
    let _attachment = attach(&mcp, "tui", &client).await.unwrap();
    wait_until(|| changes.lock().contains(&"tui".to_string())).await;
    let tools: Vec<_> = mcp.tools().into_iter().map(|t| (t.server, t.name, t.codemode)).collect();
    assert_eq!(tools, [("tui".to_string(), "open_terminal".to_string(), None)]);
    registered(&mcp, "tui_open_terminal", true).await;
    assert_eq!(call(&mcp, "tui", "open_terminal", Some("ses_attached")).await.unwrap(), "tui:open_terminal");
    assert_eq!(*client.sessions.lock(), [Some("ses_attached".to_string())]);
}

#[tokio::test]
async fn removes_an_attached_entry_when_its_attachment_closes() {
    let (mcp, _dir) = manager();
    let changes = tools_changed(&mcp);
    let client = Fake::new("tui", &["open_terminal"]);
    let attachment = attach(&mcp, "tui", &client).await.unwrap();
    registered(&mcp, "tui_open_terminal", true).await;
    let before = changes.lock().len();
    attachment.close().await;
    assert!(mcp.tools().is_empty());
    // Closing the attachment closes the socket normally.
    assert_eq!(*client.close_codes.lock(), [1000]);
    wait_until(|| changes.lock().len() > before).await;
    registered(&mcp, "tui_open_terminal", false).await;
    assert!(matches!(call(&mcp, "tui", "open_terminal", None).await, Err(ManagerError::NotFound(_))));
}

#[tokio::test]
async fn dropping_an_attachment_detaches_it() {
    let (mcp, _dir) = manager();
    let client = Fake::new("tui", &["open_terminal"]);
    let attachment = attach(&mcp, "tui", &client).await.unwrap();
    drop(attachment);
    wait_until(|| mcp.tools().is_empty()).await;
    wait_until(|| !client.connected()).await;
}

#[tokio::test]
async fn fails_an_attach_whose_initial_tool_list_fails_and_closes_its_transport() {
    let (mcp, _dir) = manager();
    let first = Fake::new("first", &["open_terminal"]);
    let second = Fake::new("second", &["broken"]);
    second.fail_list.store(true, Ordering::SeqCst);
    let _first = attach(&mcp, "tui", &first).await.unwrap();
    let error = attach(&mcp, "tui", &second).await.err().unwrap();
    assert!(error.to_string().contains("list failed"), "{error}");
    assert!(!second.connected());
    assert_eq!(names(&mcp), ["open_terminal"]);
}

#[tokio::test]
async fn fails_an_attach_whose_client_rejects_initialize() {
    let (mcp, _dir) = manager();
    let client = Fake::new("tui", &["open_terminal"]);
    client.reject.store(true, Ordering::SeqCst);
    let error = attach(&mcp, "tui", &client).await.err().unwrap();
    assert!(error.to_string().contains("not today"), "{error}");
    assert!(!client.connected());
    assert!(mcp.tools().is_empty());
}

#[tokio::test]
async fn routes_an_unknown_tool_on_an_attached_only_name_to_the_client() {
    let (mcp, _dir) = manager();
    let client = Fake::new("tui", &["open_terminal"]);
    let _attachment = attach(&mcp, "tui", &client).await.unwrap();
    match call(&mcp, "tui", "missing", None).await {
        Err(ManagerError::ToolCall { message, .. }) => {
            assert!(message.starts_with("MCP tool \"missing\" on client \"tui\" failed: "), "{message}")
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn removes_an_attached_entry_when_the_client_closes_the_connection() {
    let (mcp, _dir) = manager();
    let changes = tools_changed(&mcp);
    let client = Fake::new("tui", &["open_terminal"]);
    let _attachment = attach(&mcp, "tui", &client).await.unwrap();
    registered(&mcp, "tui_open_terminal", true).await;
    let before = changes.lock().len();
    client.close();
    wait_until(|| changes.lock().len() > before).await;
    assert!(mcp.tools().is_empty());
    registered(&mcp, "tui_open_terminal", false).await;
}

#[tokio::test]
async fn stacks_a_second_attach_under_the_same_name_and_keeps_the_first_connected() {
    let (mcp, _dir) = manager();
    let first = Fake::new("first", &["old_tool", "shared"]);
    let second = Fake::new("second", &["shared"]);
    let _first = attach(&mcp, "tui", &first).await.unwrap();
    registered(&mcp, "tui_old_tool", true).await;
    let _second = attach(&mcp, "tui", &second).await.unwrap();
    assert_eq!(names(&mcp), ["shared"]);
    registered(&mcp, "tui_old_tool", false).await;
    assert_eq!(call(&mcp, "tui", "shared", Some("ses_client_mcp")).await.unwrap(), "second:shared");
    assert!(first.sessions.lock().is_empty());
    assert!(first.connected());
}

#[tokio::test]
async fn falls_back_to_the_previous_connection_when_the_top_one_closes() {
    let (mcp, _dir) = manager();
    let changes = tools_changed(&mcp);
    let first = Fake::new("first", &["shared"]);
    let second = Fake::new("second", &["shared"]);
    let _first = attach(&mcp, "tui", &first).await.unwrap();
    let _second = attach(&mcp, "tui", &second).await.unwrap();
    // Changes on an idle connection stay hidden until it takes over.
    first.register("added");
    registered(&mcp, "tui_shared", true).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(names(&mcp), ["shared"]);
    let before = changes.lock().len();
    second.close();
    wait_until(|| changes.lock().len() > before).await;
    assert_eq!(names(&mcp), ["added", "shared"]);
    registered(&mcp, "tui_added", true).await;
    assert_eq!(call(&mcp, "tui", "shared", None).await.unwrap(), "first:shared");
    assert_eq!(call(&mcp, "tui", "added", None).await.unwrap(), "first:added");
}

#[tokio::test]
async fn falls_back_to_a_connection_with_the_same_tools_without_publishing_a_change() {
    let (mcp, _dir) = manager();
    let changes = tools_changed(&mcp);
    // Same label, so both list identical tools; the recorded sessions tell them apart.
    let first = Fake::new("same", &["shared"]);
    let second = Fake::new("same", &["shared"]);
    let marker = Fake::new("marker", &["done"]);
    let _first = attach(&mcp, "tui", &first).await.unwrap();
    let second_attachment = attach(&mcp, "tui", &second).await.unwrap();
    wait_until(|| changes.lock().len() == 2).await;
    registered(&mcp, "tui_shared", true).await;
    second_attachment.close().await;
    call(&mcp, "tui", "shared", Some("ses_fallback")).await.unwrap();
    assert_eq!(*first.sessions.lock(), [Some("ses_fallback".to_string())]);
    assert!(second.sessions.lock().is_empty());
    // Events arrive in order, so once the marker's attach shows up any fallback event would have too.
    let _marker = attach(&mcp, "marker", &marker).await.unwrap();
    wait_until(|| changes.lock().contains(&"marker".to_string())).await;
    assert_eq!(*changes.lock(), ["tui", "tui", "marker"]);
}

#[tokio::test]
async fn keeps_the_stored_tools_when_the_fallback_relist_fails() {
    let (mcp, _dir) = manager();
    let changes = tools_changed(&mcp);
    let first = Fake::new("first", &["kept"]);
    let second = Fake::new("second", &["newer"]);
    let _first = attach(&mcp, "tui", &first).await.unwrap();
    let second_attachment = attach(&mcp, "tui", &second).await.unwrap();
    first.register("hidden");
    first.fail_list.store(true, Ordering::SeqCst);
    wait_until(|| changes.lock().len() == 2).await;
    second_attachment.close().await;
    assert_eq!(names(&mcp), ["kept"]);
    wait_until(|| changes.lock().len() == 3).await;
    registered(&mcp, "tui_kept", true).await;
    assert_eq!(call(&mcp, "tui", "kept", None).await.unwrap(), "first:kept");
}

#[tokio::test]
async fn refreshes_the_top_connections_tools_when_it_reports_a_change() {
    let (mcp, _dir) = manager();
    let changes = tools_changed(&mcp);
    let first = Fake::new("first", &["below"]);
    let second = Fake::new("second", &["top"]);
    let _first = attach(&mcp, "tui", &first).await.unwrap();
    let _second = attach(&mcp, "tui", &second).await.unwrap();
    wait_until(|| changes.lock().len() == 2).await;
    second.register("added");
    wait_until(|| changes.lock().len() == 3).await;
    assert_eq!(names(&mcp), ["added", "top"]);
    registered(&mcp, "tui_added", true).await;
    assert_eq!(call(&mcp, "tui", "added", None).await.unwrap(), "second:added");
}

#[tokio::test]
async fn closing_a_connection_below_the_top_changes_nothing_visible() {
    let (mcp, _dir) = manager();
    let changes = tools_changed(&mcp);
    let first = Fake::new("first", &["old_tool"]);
    let second = Fake::new("second", &["shared"]);
    let first_attachment = attach(&mcp, "tui", &first).await.unwrap();
    let _second = attach(&mcp, "tui", &second).await.unwrap();
    wait_until(|| changes.lock().len() == 2).await;
    registered(&mcp, "tui_shared", true).await;
    first_attachment.close().await;
    wait_until(|| !first.connected()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(changes.lock().len(), 2);
    assert_eq!(names(&mcp), ["shared"]);
    assert_eq!(call(&mcp, "tui", "shared", None).await.unwrap(), "second:shared");
}

#[tokio::test]
async fn removes_the_entry_once_every_stacked_connection_closes() {
    let (mcp, _dir) = manager();
    let first = Fake::new("first", &["shared"]);
    let second = Fake::new("second", &["shared"]);
    let _first = attach(&mcp, "tui", &first).await.unwrap();
    let _second = attach(&mcp, "tui", &second).await.unwrap();
    registered(&mcp, "tui_shared", true).await;
    second.close();
    first.close();
    registered(&mcp, "tui_shared", false).await;
    assert!(mcp.tools().is_empty());
    assert!(matches!(call(&mcp, "tui", "shared", None).await, Err(ManagerError::NotFound(_))));
}

#[tokio::test]
async fn hides_attached_entries_from_server_status_and_keeps_them_across_config_changes() {
    let (mcp, _dir) = manager();
    let client = Fake::new("tui", &["open_terminal"]);
    let _attachment = attach(&mcp, "tui", &client).await.unwrap();
    assert!(mcp.servers().is_empty());
    // A configured server sharing the client's name neither shows the client nor captures its tools.
    let disabled = || {
        serde_json::from_value::<ServerConfig>(json!({"type": "local", "command": ["unused"], "disabled": true})).unwrap()
    };
    mcp.add("tui", disabled()).await;
    mcp.add("other", disabled()).await;
    let servers: Vec<_> = mcp.servers().into_iter().map(|s| s.name).collect();
    assert_eq!(servers, ["other", "tui"]);
    mcp.remove("tui").await.unwrap();
    let servers: Vec<_> = mcp.servers().into_iter().map(|s| s.name).collect();
    assert_eq!(servers, ["other"]);
    let tools: Vec<_> = mcp.tools().into_iter().map(|t| (t.server, t.name)).collect();
    assert_eq!(tools, [("tui".to_string(), "open_terminal".to_string())]);
    assert_eq!(call(&mcp, "tui", "open_terminal", None).await.unwrap(), "tui:open_terminal");
}

#[tokio::test]
async fn shutdown_closes_attached_connections_and_publishes_nothing() {
    let (mcp, _dir) = manager();
    let client = Fake::new("tui", &["open_terminal"]);
    let attachment = attach(&mcp, "tui", &client).await.unwrap();
    let changes = tools_changed(&mcp);
    mcp.shutdown().await;
    assert!(mcp.is_closing());
    assert!(!client.connected());
    assert!(mcp.tools().is_empty());
    attachment.close().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(changes.lock().is_empty());
    // A closing registry attaches nothing.
    let late = Fake::new("late", &["x"]);
    assert!(attach(&mcp, "late", &late).await.is_err());
    assert!(!late.connected());
}
