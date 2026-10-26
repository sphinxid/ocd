//! End-to-end tests against a fake stdio MCP server. The server is this test binary re-executed
//! with `OC_MCP_FAKE_STDIO=1` (see `fake_stdio_server_entry`); libtest's own non-JSON output
//! lines are skipped by the client's framing.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use oc_mcp::{
    CallToolContent, ClientInfo, ElicitationHandler, LocalConfig, ManagerOptions, McpEvent, McpManager, ServerConfig,
    Status, TimeoutConfig,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn write(out: &mut impl Write, value: Value) {
    let mut line = serde_json::to_string(&value).unwrap();
    line.push('\n');
    out.write_all(line.as_bytes()).unwrap();
    out.flush().unwrap();
}

fn run_fake_server() {
    eprintln!("fake server starting");
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    // libtest printed "test <name> ... " without a newline; terminate that line.
    stdout.write_all(b"\n").unwrap();
    stdout.flush().unwrap();
    let mut lines = stdin.lock().lines();
    let mut changed = false;
    let mut next_server_id = 1000;
    while let Some(Ok(line)) = lines.next() {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(method) = msg.get("method").and_then(Value::as_str) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params["protocolVersion"],
                "capabilities": {"tools": {"listChanged": true}, "prompts": {}, "resources": {}},
                "serverInfo": {"name": "fake", "version": "1.0.0"},
                "instructions": "  Use the fake server.  \n",
                "echoClient": {"clientInfo": params["clientInfo"], "capabilities": params["capabilities"]}
            })),
            "tools/list" => {
                if changed {
                    Ok(json!({"tools": [{"name": "late tool", "inputSchema": {"type": "object"}}]}))
                } else if params.get("cursor").is_none() {
                    Ok(json!({"tools": [{"name": "echo", "description": "Echo", "inputSchema": {"type": "object"}}], "nextCursor": "p2"}))
                } else {
                    Ok(json!({"tools": [
                        {"name": "fail", "inputSchema": {"type": "object"}},
                        {"name": "image", "inputSchema": {"type": "object"}},
                        {"name": "structured", "inputSchema": {"type": "object"}, "outputSchema": {"type": "object", "properties": {"n": {"type": "number"}}, "required": ["n"]}},
                        {"name": "env", "inputSchema": {"type": "object"}},
                        {"name": "roots", "inputSchema": {"type": "object"}},
                        {"name": "notify", "inputSchema": {"type": "object"}},
                        {"name": "crash", "inputSchema": {"type": "object"}},
                        {"name": "rpc_error", "inputSchema": {"type": "object"}},
                        {"name": "slow", "inputSchema": {"type": "object"}}
                    ]}))
                }
            }
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                match name {
                    "echo" => Ok(json!({"content": [{"type": "text", "text": json!({"arguments": params["arguments"], "meta": params["_meta"]}).to_string()}]})),
                    "fail" => Ok(json!({"isError": true, "content": [{"type": "text", "text": "bad things"}]})),
                    "image" => Ok(json!({"content": [{"type": "image", "data": "AAAA", "mimeType": "image/png"}, {"type": "resource", "resource": {"uri": "x://y", "text": "embedded"}}]})),
                    "structured" => Ok(json!({"content": [], "structuredContent": {"n": params["arguments"]["n"]}})),
                    "env" => Ok(json!({"content": [{"type": "text", "text": format!("{}|{}", std::env::var("FAKE_VAR").unwrap_or_default(), std::env::current_dir().unwrap().display())}]})),
                    "roots" => {
                        let rid = next_server_id;
                        next_server_id += 1;
                        write(&mut stdout, json!({"jsonrpc": "2.0", "id": rid, "method": "roots/list"}));
                        let mut text = String::new();
                        while let Some(Ok(line)) = lines.next() {
                            let Ok(reply) = serde_json::from_str::<Value>(&line) else { continue };
                            if reply.get("id") == Some(&json!(rid)) {
                                text = reply["result"].to_string();
                                break;
                            }
                        }
                        Ok(json!({"content": [{"type": "text", "text": text}]}))
                    }
                    "notify" => {
                        changed = true;
                        write(&mut stdout, json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}));
                        Ok(json!({"content": [{"type": "text", "text": "ok"}]}))
                    }
                    "crash" => {
                        eprintln!("fatal: crashed on purpose");
                        std::process::exit(3);
                    }
                    "rpc_error" => Err((-32000, "kaboom".to_string())),
                    "slow" => {
                        std::thread::sleep(Duration::from_millis(1500));
                        Ok(json!({"content": []}))
                    }
                    _ => Err((-32602, format!("Unknown tool: {name}"))),
                }
            }
            "prompts/list" => Ok(json!({"prompts": [{"name": "greet", "description": "Greets", "arguments": [{"name": "who", "required": true}]}]})),
            "prompts/get" => Ok(json!({"messages": [{"role": "user", "content": {"type": "text", "text": format!("Hello {}", params["arguments"]["who"].as_str().unwrap_or(""))}}]})),
            "resources/list" => Ok(json!({"resources": [
                {"name": "b", "uri": "mem://b", "mimeType": "text/plain"},
                {"name": "a", "uri": "mem://a", "description": "first"}
            ]})),
            "resources/templates/list" => Err((-32601, "Method not found".to_string())),
            "resources/read" => Ok(json!({"contents": [
                {"uri": params["uri"], "text": "hello", "mimeType": "text/plain"},
                {"uri": params["uri"], "blob": "QUJD", "mimeType": "image/png"}
            ]})),
            "ping" => Ok(json!({})),
            _ => Err((-32601, "Method not found".to_string())),
        };
        match result {
            Ok(result) => write(&mut stdout, json!({"jsonrpc": "2.0", "id": id, "result": result})),
            Err((code, message)) => write(&mut stdout, json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})),
        }
    }
}

#[test]
fn fake_stdio_server_entry() {
    if std::env::var("OC_MCP_FAKE_STDIO").is_err() {
        return;
    }
    run_fake_server();
    std::process::exit(0);
}

fn server_config(extra_env: &[(&str, &str)], cwd: Option<&str>) -> ServerConfig {
    let exe = std::env::current_exe().unwrap().display().to_string();
    let mut environment = IndexMap::new();
    environment.insert("OC_MCP_FAKE_STDIO".to_string(), "1".to_string());
    for (k, v) in extra_env {
        environment.insert(k.to_string(), v.to_string());
    }
    ServerConfig::Local(LocalConfig {
        command: vec![exe, "fake_stdio_server_entry".into(), "--exact".into(), "--nocapture".into(), "--test-threads=1".into()],
        cwd: cwd.map(String::from),
        environment: Some(environment),
        disabled: None,
        codemode: None,
        timeout: Some(TimeoutConfig { startup: Some(20_000), catalog: Some(10_000), execution: Some(1_000) }),
        protocol: None,
    })
}

fn manager(directory: PathBuf) -> McpManager {
    McpManager::new(ManagerOptions {
        directory,
        client_info: ClientInfo { name: "opencode".into(), version: "2.0.26".into() },
        credentials: None,
        elicitation: None,
    })
}

async fn connected(manager: &McpManager, name: &str) {
    manager.wait_ready(name).await.unwrap();
    assert_eq!(manager.status(name), Some(Status::Connected), "{:?}", manager.servers());
}

#[tokio::test]
async fn stdio_lifecycle_tools_and_calls() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let manager = manager(dir.path().to_path_buf());
    let mut events = manager.subscribe();
    let mut servers = IndexMap::new();
    servers.insert("my.fake".to_string(), server_config(&[("FAKE_VAR", "hello")], Some("sub")));
    servers.insert(
        "off".to_string(),
        ServerConfig::Local(LocalConfig {
            command: vec!["true".into()],
            cwd: None,
            environment: None,
            disabled: Some(true),
            codemode: None,
            timeout: None,
            protocol: None,
        }),
    );
    manager.set_servers(servers).await;
    connected(&manager, "my.fake").await;
    assert_eq!(manager.status("off"), Some(Status::Disabled));

    let infos = manager.servers();
    assert_eq!(infos.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["my.fake", "off"]);
    assert!(infos[0].integration_id.is_none());

    // Pagination joined both pages; names sorted; prefixed ids sanitized.
    let tools = manager.tools();
    let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["crash", "echo", "env", "fail", "image", "notify", "roots", "rpc_error", "slow", "structured"]);
    assert_eq!(tools[1].prefixed_name(), "my_fake_echo");
    assert_eq!(tools[1].description.as_deref(), Some("Echo"));

    // Instructions are trimmed.
    assert_eq!(manager.instructions(), vec![("my.fake".to_string(), "Use the fake server.".to_string())]);

    // Events: status pending → connected, tools/resources changed.
    let mut seen = Vec::new();
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
        seen.push(event);
    }
    assert!(seen.contains(&McpEvent::ToolsChanged { server: "my.fake".into() }));
    assert!(seen.contains(&McpEvent::StatusChanged { server: "my.fake".into() }));
    assert!(seen.contains(&McpEvent::StatusChanged { server: "off".into() }));

    // Echo carries arguments and the session id meta + progress token.
    let result = manager.call_tool("my.fake", "echo", Some(json!({"x": 1})), Some("ses_1")).await.unwrap();
    let CallToolContent::Text { text } = &result.content[0] else { panic!() };
    let echoed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(echoed["arguments"], json!({"x": 1}));
    assert_eq!(echoed["meta"]["ai.opencode/sessionID"], "ses_1");
    assert!(echoed["meta"]["progressToken"].is_number());
    // No args → {}
    let result = manager.call_tool("my.fake", "echo", None, None).await.unwrap();
    let CallToolContent::Text { text } = &result.content[0] else { panic!() };
    assert_eq!(serde_json::from_str::<Value>(text).unwrap()["arguments"], json!({}));

    // isError and content mapping.
    let result = manager.call_tool("my.fake", "fail", None, None).await.unwrap();
    assert!(result.is_error);
    assert_eq!(oc_mcp::naming::tool_output(&result, None).unwrap_err(), "bad things");
    let result = manager.call_tool("my.fake", "image", None, None).await.unwrap();
    assert_eq!(
        result.content,
        vec![
            CallToolContent::Media { data: "AAAA".into(), mime_type: "image/png".into(), uri: None },
            CallToolContent::Text { text: "embedded".into() }
        ]
    );

    // Structured content validated against the output schema.
    let result = manager.call_tool("my.fake", "structured", Some(json!({"n": 5})), None).await.unwrap();
    assert_eq!(result.structured, Some(json!({"n": 5})));
    let error = manager.call_tool("my.fake", "structured", Some(json!({"n": "x"})), None).await.unwrap_err();
    assert!(
        error.to_string().starts_with(
            "MCP tool \"structured\" on server \"my.fake\" failed: Structured content does not match the tool's output schema:"
        ),
        "{error}"
    );

    // Environment and resolved cwd.
    let result = manager.call_tool("my.fake", "env", None, None).await.unwrap();
    let CallToolContent::Text { text } = &result.content[0] else { panic!() };
    let expected_cwd = std::fs::canonicalize(dir.path().join("sub")).unwrap();
    let (var, cwd) = text.split_once('|').unwrap();
    assert_eq!(var, "hello");
    assert_eq!(std::fs::canonicalize(cwd).unwrap(), expected_cwd);

    // Server → client roots/list request.
    let result = manager.call_tool("my.fake", "roots", None, None).await.unwrap();
    let CallToolContent::Text { text } = &result.content[0] else { panic!() };
    let roots: Value = serde_json::from_str(text).unwrap();
    assert_eq!(roots["roots"][0]["uri"], oc_mcp::connection::file_url(dir.path()));

    // JSON-RPC error and execution timeout.
    let error = manager.call_tool("my.fake", "rpc_error", None, None).await.unwrap_err();
    assert_eq!(error.to_string(), "MCP tool \"rpc_error\" on server \"my.fake\" failed: kaboom");
    let error = manager.call_tool("my.fake", "slow", None, None).await.unwrap_err();
    assert_eq!(error.to_string(), "MCP tool \"slow\" on server \"my.fake\" failed: Request timed out");

    // Unknown server.
    let error = manager.call_tool("nope", "x", None, None).await.unwrap_err();
    assert_eq!(error.to_string(), "MCP server not found: nope");
    // Disabled server.
    let error = manager.call_tool("off", "x", None, None).await.unwrap_err();
    assert_eq!(error.to_string(), "MCP server \"off\" is disabled.");

    // Prompts (refreshed in the background after connect).
    tokio::time::sleep(Duration::from_millis(300)).await;
    let prompts = manager.prompts();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].arguments[0].name, "who");
    let args = oc_mcp::naming::prompt_arguments(&prompts[0].arguments, "\"big world\"");
    let prompt = manager.prompt("my.fake", "greet", Some(args)).await.unwrap().unwrap();
    assert_eq!(oc_mcp::naming::prompt_result_text(&prompt), "Hello big world");

    // Resources: template listing failure is swallowed, catalog sorted.
    let catalog = manager.resource_catalog().await;
    assert_eq!(catalog.resources.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
    assert!(catalog.templates.is_empty());
    assert_eq!(
        serde_json::to_value(&catalog.resources[0]).unwrap(),
        json!({"server": "my.fake", "name": "a", "uri": "mem://a", "description": "first"})
    );
    let content = manager.read_resource("my.fake", "mem://a").await.unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&content).unwrap(),
        json!({"server": "my.fake", "uri": "mem://a", "contents": [
            {"type": "text", "uri": "mem://a", "text": "hello", "mimeType": "text/plain"},
            {"type": "blob", "uri": "mem://a", "blob": "QUJD", "mimeType": "image/png"}
        ]})
    );
    assert_eq!(oc_mcp::naming::resource_files(&content).len(), 1);

    // list_changed → tools re-listed.
    let mut events = manager.subscribe();
    manager.call_tool("my.fake", "notify", None, None).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(event @ McpEvent::ToolsChanged { .. }) = events.recv().await {
                return event;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(event.server(), "my.fake");
    assert_eq!(manager.tools().iter().map(|t| t.prefixed_name()).collect::<Vec<_>>(), vec!["my_fake_late_tool"]);

    // Disconnect / connect.
    manager.disconnect("my.fake").await.unwrap();
    assert_eq!(manager.status("my.fake"), Some(Status::Disabled));
    assert!(manager.tools().is_empty());
    manager.connect("my.fake").await.unwrap();
    assert_eq!(manager.status("my.fake"), Some(Status::Connected));

    // Unexpected exit → failed with exit code and stderr tail.
    let error = manager.call_tool("my.fake", "crash", None, None).await.unwrap_err();
    assert!(error.to_string().contains("failed: Connection closed"), "{error}");
    let status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status @ Status::Failed { .. }) = manager.status("my.fake") {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let Status::Failed { error } = status else { unreachable!() };
    assert!(error.starts_with("Connection closed: MCP server process exited with code 3: "), "{error}");
    assert!(error.contains("fatal: crashed on purpose"), "{error}");
    let error = manager.call_tool("my.fake", "echo", None, None).await.unwrap_err();
    assert!(error.to_string().starts_with("MCP server \"my.fake\" is not connected: Connection closed: MCP server process exited"));

    // Remove.
    manager.remove("my.fake").await.unwrap();
    assert!(manager.status("my.fake").is_none());
    manager.shutdown().await;
}

#[tokio::test]
async fn stdio_spawn_failure_and_reload() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path().to_path_buf());
    let mut servers = IndexMap::new();
    servers.insert(
        "missing".to_string(),
        ServerConfig::Local(LocalConfig {
            command: vec!["/definitely/not/here-oc-mcp".into()],
            cwd: None,
            environment: None,
            disabled: None,
            codemode: None,
            timeout: None,
            protocol: None,
        }),
    );
    manager.set_servers(servers.clone()).await;
    manager.wait_ready("missing").await.unwrap();
    match manager.status("missing").unwrap() {
        Status::Failed { error } => assert!(error.contains("/definitely/not/here-oc-mcp"), "{error}"),
        other => panic!("{other:?}"),
    }
    // Reload with the same config leaves the entry alone; a new server is added.
    servers.insert("fake".to_string(), server_config(&[], None));
    manager.set_servers(servers.clone()).await;
    connected(&manager, "fake").await;
    // Removing from config removes the server.
    servers.shift_remove("fake");
    manager.set_servers(servers).await;
    assert!(manager.status("fake").is_none());
    // Runtime add.
    manager.add("added", server_config(&[], None)).await;
    connected(&manager, "added").await;
    manager.shutdown().await;
}

struct AcceptAll;

#[async_trait::async_trait]
impl ElicitationHandler for AcceptAll {
    async fn create(&self, _server: &str, _params: Value, _cancel: CancellationToken) -> Result<Value, String> {
        Ok(json!({"action": "accept", "content": {}}))
    }
    async fn complete(&self, _server: &str, _id: &str) {}
}

#[tokio::test]
async fn stdio_handshake_declares_capabilities() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let exe = std::env::current_exe().unwrap().display().to_string();
    let mut env = IndexMap::new();
    env.insert("OC_MCP_FAKE_STDIO".to_string(), "1".to_string());
    let transport: Arc<dyn oc_mcp::transport::Transport> = oc_mcp::transport::stdio::StdioTransport::new(
        oc_mcp::transport::stdio::StdioOptions {
            server: "raw".into(),
            command: exe,
            args: vec!["fake_stdio_server_entry".into(), "--exact".into(), "--nocapture".into()],
            cwd: dir.path().to_path_buf(),
            environment: env,
        },
        tx,
    );
    let client = oc_mcp::protocol::Client::new(
        transport.clone(),
        rx,
        oc_mcp::protocol::ClientConfig {
            server: "raw".into(),
            client_info: ClientInfo::default(),
            negotiation: None,
            elicitation: Some(Arc::new(AcceptAll)),
            root_uri: "file:///tmp".into(),
        },
    );
    client.connect(Duration::from_secs(20)).await.unwrap();
    assert_eq!(client.negotiated_protocol_version().as_deref(), Some("2025-11-25"));
    assert!(!client.is_modern());
    assert_eq!(client.server_info().unwrap()["name"], "fake");
    assert_eq!(
        client.capabilities(),
        &json!({"elicitation": {"form": {"applyDefaults": true}, "url": {}}, "roots": {}})
    );
    // Starting twice is rejected.
    assert_eq!(transport.start().await.unwrap_err().to_string(), "Stdio transport already started");
    client.close().await;
    assert_eq!(
        transport.send(json!({}), Default::default()).await.unwrap_err().to_string(),
        "Not connected"
    );
}
