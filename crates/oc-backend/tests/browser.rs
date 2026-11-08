//! The built-in browser plugin (`opencode.browser`, 2.2) end to end over the HTTP routes: a stand-in
//! desktop attaches with the `experimental.browser` RPC, follows the control events and answers the
//! commands the `browser.*` Code Mode tools send.

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use common::*;
use oc_tools::host::{PermissionError, PermissionRequest, ToolHost};
use oc_tools::tool::ToolConfig;
use oc_tools::{Registry, ToolContext};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CONTROL: &str = "rpc.experimental.browser.control";
const TAB: &str = "tab_00000000-0000-4000-8000-000000000001";

/// Calls an `experimental.browser` method at the harness's location.
async fn rpc(router: &axum::Router, dir: &str, method: &str, input: Value) -> (u16, Value) {
    use tower::ServiceExt;
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/rpc/experimental.browser/{method}?directory={}", enc(dir)))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&json!({ "input": input })).unwrap()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// Waits for a control event of `connection` with `kind`.
async fn control(rx: &mut tokio::sync::broadcast::Receiver<oc_core::bus::Event>, connection: &str, kind: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = rx.recv().await.unwrap();
            if event.kind == CONTROL && event.data["connectionID"] == connection && event.data["type"] == kind {
                return event.data.clone();
            }
        }
    })
    .await
    .expect("control event")
}

/// Starts `attach` for `connection` and waits for its `attached` event; aborting the task detaches.
async fn attach(h: &Harness, session: &str, connection: &str) -> tokio::task::JoinHandle<(u16, Value)> {
    let mut rx = h.app.bus.subscribe();
    let (router, dir) = (h.router.clone(), h.dir());
    let input = json!({ "sessionID": session, "connectionID": connection, "version": 4 });
    let task = tokio::spawn(async move { rpc(&router, &dir, "attach", input).await });
    let attached = control(&mut rx, connection, "attached").await;
    assert_eq!(attached, json!({ "type": "attached", "connectionID": connection, "version": 4 }));
    task
}

async fn permissions(h: &Harness, session: &str) -> Value {
    h.get(&format!("/api/session/{session}")).await.json()["data"]["permissions"].clone()
}

fn allows() -> Value {
    json!([
        { "action": "browser", "resource": "*", "effect": "allow" },
        { "action": "skill", "resource": "browser", "effect": "allow" },
    ])
}

#[tokio::test]
async fn attachments_grant_the_browser_and_end_on_replacement_detach_and_deletion() {
    let h = harness();
    let session = h.session().await;
    let agents = h.get(&format!("/api/agent?directory={}", enc(&h.dir()))).await.json();
    let build = agents["data"].as_array().unwrap().iter().find(|a| a["id"] == "build").unwrap().clone();
    let rules = build["permissions"].as_array().unwrap();
    assert_eq!(
        rules[rules.len() - 2..],
        [
            json!({ "action": "browser", "resource": "*", "effect": "deny" }),
            json!({ "action": "skill", "resource": "browser", "effect": "deny" })
        ]
    );
    assert_eq!(permissions(&h, &session).await, Value::Null);

    let first = attach(&h, &session, "c1").await;
    assert_eq!(permissions(&h, &session).await, allows());
    let state = json!({ "tabs": [], "focusedTabID": null });
    let ok = rpc(&h.router, &h.dir(), "state", json!({ "sessionID": session, "connectionID": "c1", "state": state })).await;
    assert_eq!(ok, (200, json!({})));
    let missing = rpc(&h.router, &h.dir(), "command", json!({ "sessionID": session, "connectionID": "c1", "requestID": "r" })).await;
    assert_eq!(
        missing,
        (400, json!({ "_tag": "RpcError", "type": "unavailable", "message": "Browser request is no longer available.", "data": {} }))
    );
    let invalid = rpc(&h.router, &h.dir(), "attach", json!({ "sessionID": session, "connectionID": "x", "version": 3 })).await;
    assert_eq!(invalid, (400, json!({ "_tag": "RpcError", "type": "rpc.invalid_input", "message": "Expected 4\n  at [\"version\"]" })));
    let unknown = rpc(&h.router, &h.dir(), "attach", json!({ "sessionID": "ses_missing", "connectionID": "x", "version": 4 })).await;
    assert_eq!(unknown.1["message"], "Session not found.");

    // A second desktop replaces the first, which must not reclaim the session.
    let second = attach(&h, &session, "c2").await;
    assert_eq!(first.await.unwrap(), (200, json!({ "output": "replaced" })));
    assert_eq!(permissions(&h, &session).await, allows());
    let stale = rpc(&h.router, &h.dir(), "state", json!({ "sessionID": session, "connectionID": "c1", "state": state })).await;
    assert_eq!(stale.1["message"], "Browser is unavailable.");

    // The desktop going away removes the allows.
    second.abort();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while permissions(&h, &session).await != json!([]) {
        assert!(tokio::time::Instant::now() < deadline, "allows were not removed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!h.app.plugins.browsers.attached(&h.dir(), &session));

    // Deleting the session ends its attachment.
    let third = attach(&h, &session, "c3").await;
    assert_eq!(h.req("DELETE", &format!("/api/session/{session}"), None).await.status, 204);
    let ended = tokio::time::timeout(Duration::from_secs(5), third).await.expect("attach ended").unwrap();
    assert_eq!(ended, (200, json!({ "output": "closed" })));
}

struct Host;

#[async_trait]
impl ToolHost for Host {
    async fn ask_permission(&self, _request: PermissionRequest) -> Result<(), PermissionError> {
        Ok(())
    }
}

fn context(h: &Harness, session: &str) -> ToolContext {
    ToolContext {
        session_id: session.into(),
        message_id: "msg_test".into(),
        call_id: "call_execute".into(),
        agent: "build".into(),
        directory: h.cwd.clone(),
        worktree: h.cwd.clone(),
        project_id: "prj".into(),
        workspace_id: None,
        global: Arc::new(global(h.root.path())),
        cancel: tokio_util::sync::CancellationToken::new(),
        progress: Arc::new(|_| {}),
        config: Arc::new(ToolConfig::default()),
        host: Arc::new(Host),
    }
}

fn tab(url: &str) -> Value {
    json!({ "id": TAB, "url": url, "title": "Example", "loading": false, "canGoBack": false, "canGoForward": false, "generation": 2 })
}

/// A desktop that answers every command of `connection` until the test ends, recording the commands.
fn desktop(h: &Harness, session: &str, connection: &str) -> Arc<parking_lot::Mutex<Vec<Value>>> {
    let commands = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let recorded = commands.clone();
    let mut rx = h.app.bus.subscribe();
    let (router, dir, session, connection) = (h.router.clone(), h.dir(), session.to_string(), connection.to_string());
    tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            if event.kind != CONTROL || event.data["connectionID"] != connection.as_str() || event.data["type"] != "command" {
                continue;
            }
            let request = event.data["requestID"].as_str().unwrap().to_string();
            let attachment = json!({ "sessionID": session, "connectionID": connection, "requestID": request });
            let (_, command) = rpc(&router, &dir, "command", attachment.clone()).await;
            let command = command["output"].clone();
            recorded.lock().push(command.clone());
            let action = &command["action"];
            let success = |value: Value, files: Value| json!({ "type": "success", "result": { "value": value, "files": files } });
            let outcome = match action["type"].as_str().unwrap() {
                "tabs.open" => {
                    let state = json!({ "tabs": [tab(action["url"].as_str().unwrap_or("about:blank"))], "focusedTabID": TAB });
                    let input = json!({ "sessionID": session, "connectionID": connection, "state": state });
                    assert_eq!(rpc(&router, &dir, "state", input).await.0, 200);
                    success(tab(action["url"].as_str().unwrap_or("about:blank")), json!([]))
                }
                "snapshot" => {
                    success(json!({ "extra": true, "content": "- button [ref=e1]", "truncated": false, "tab": tab("x") }), json!([]))
                }
                "screenshot" => success(
                    json!({ "tab": tab("x") }),
                    json!([{ "id": "file_00000000-0000-4000-9000-000000000001", "name": "../shot.png", "mime": "image/png", "data": "iVBORw0KGgo=" }]),
                ),
                "files.upload" => success(tab("x"), json!([])),
                "console" => json!({ "type": "failure", "code": "unsupported", "message": "No console here." }),
                "network.list" => success(json!({ "tab": tab("x") }), json!([])),
                // Never answered: the request times out.
                "wait" => continue,
                other => panic!("unexpected {other}"),
            };
            let input = json!({ "sessionID": session, "connectionID": connection, "requestID": request, "outcome": outcome });
            assert_eq!(rpc(&router, &dir, "result", input).await, (200, json!({})));
        }
    });
    commands
}

async fn run(h: &Harness, session: &str, code: &str) -> Value {
    let mut registry = Registry::new();
    oc_core::browser::tools::register(&mut registry, &h.app.plugins.browsers, &h.dir());
    let snapshot = registry.snapshot(&[]);
    let result = oc_core::codemode::execute(&snapshot, json!({ "code": code }), context(h, session)).await.unwrap();
    let output = result.output.unwrap();
    serde_json::from_str(output["output"].as_str().unwrap()).unwrap_or(output["output"].clone())
}

#[tokio::test]
async fn codemode_tools_round_trip_through_the_desktop() {
    let h = harness();
    std::fs::write(h.cwd.join("upload.txt"), "hello").unwrap();
    let session = h.session().await;
    let commands = desktop(&h, &session, "c1");
    let _attached = attach(&h, &session, "c1").await;
    let out = run(
        &h,
        &session,
        "const errors = []\n\
         const run = async (f) => { try { return await f() } catch (e) { errors.push(String(e.message)) } }\n\
         const opened = await tools.browser.tabs.open({ url: 'localhost:3000' })\n\
         const snap = await tools.browser.snapshot({ tabID: opened.id })\n\
         const shot = await tools.browser.screenshot({ tabID: opened.id })\n\
         await tools.browser.files.upload({ tabID: opened.id, ref: 'e1', paths: ['upload.txt'] })\n\
         await run(() => tools.browser.console({ tabID: opened.id }))\n\
         await run(() => tools.browser.network.list({ tabID: opened.id }))\n\
         await run(() => tools.browser.snapshot({ tabID: 'tab_00000000-0000-4000-8000-000000000009' }))\n\
         await run(() => tools.browser.navigate({ tabID: opened.id, url: '/etc/hosts' }))\n\
         return { opened, snap, shot, errors }",
    )
    .await;
    assert_eq!(out["opened"], tab("http://localhost:3000/"));
    // Undeclared keys are dropped and the declared ones keep the schema's order.
    assert_eq!(
        serde_json::to_string(&out["snap"]).unwrap(),
        format!(r#"{{"tab":{},"content":"- button [ref=e1]","truncated":false}}"#, tab("x"))
    );
    let file = &out["shot"]["files"][0];
    assert_eq!((file["name"].as_str(), file["mime"].as_str(), file["bytes"].as_u64()), (Some("../shot.png"), Some("image/png"), Some(8)));
    let path = std::path::PathBuf::from(file["path"].as_str().unwrap());
    assert_eq!(path.file_name().unwrap(), ".._shot.png");
    assert!(path.parent().unwrap().parent().unwrap().file_name().unwrap().to_string_lossy().starts_with("opencode-browser-"));
    assert_eq!(std::fs::read(&path).unwrap(), base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgo=").unwrap());
    std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    assert_eq!(
        out["errors"],
        json!([
            "[browser.unsupported] No console here.",
            // An invalid result reads as its schema decode error, the failure's cause (#60109).
            "Missing key\n  at [\"requests\"]",
            "[browser.tab_unavailable] This tab is closed or does not belong to the connected session. Call browser.tabs.list({}) and use an exact returned tabID. If no tabs exist, use browser.tabs.open({}). Never substitute a request ID, file ID, or element ref for tabID.",
            // Programs see the failure text a direct call gives the model, the unwrapped cause (2.2.5, #60109).
            "Unsupported browser URL",
        ])
    );
    let commands = commands.lock().clone();
    assert_eq!(commands[0], json!({ "action": { "type": "tabs.open", "url": "http://localhost:3000/" }, "files": [] }));
    assert_eq!(commands[1], json!({ "action": { "type": "snapshot", "tabID": TAB }, "generation": 2, "files": [] }));
    let upload = &commands[3];
    assert_eq!(upload["action"], json!({ "type": "files.upload", "tabID": TAB, "ref": "e1", "paths": ["upload.txt"] }));
    assert_eq!(upload["files"][0]["data"], "aGVsbG8=");
    assert_eq!(upload["files"][0]["mime"], "text/plain");
}

#[tokio::test]
async fn requests_time_out_with_a_cancel_and_fail_when_the_desktop_leaves() {
    let h = harness();
    h.app.plugins.browsers.set_timeout(Duration::from_millis(300));
    let session = h.session().await;
    let _commands = desktop(&h, &session, "c1");
    let attached = attach(&h, &session, "c1").await;
    let mut rx = h.app.bus.subscribe();
    let out = run(
        &h,
        &session,
        "const tab = await tools.browser.tabs.open({})\n\
         try { await tools.browser.wait({ tabID: tab.id, condition: 'load' }) } catch (e) { return e.message }",
    )
    .await;
    assert_eq!(
        out,
        "[browser.timeout] browser.wait did not finish within 60 seconds; its outcome is unknown. Check the desktop connection, call browser.tabs.list({}), and inspect the tab or browser.files.list({tabID}) for completed work. Do not blindly repeat a mutating action or start another recording."
    );
    control(&mut rx, "c1", "command").await;
    let waiting = control(&mut rx, "c1", "command").await;
    let cancel = control(&mut rx, "c1", "cancel").await;
    assert_eq!(cancel["requestID"], waiting["requestID"]);

    // A desktop that leaves mid-request: the call fails as disconnected.
    h.app.plugins.browsers.set_timeout(Duration::from_secs(30));
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        attached.abort();
    });
    let code =
        format!("try {{ return await tools.browser.wait({{ tabID: '{TAB}', condition: 'load' }}) }} catch (e) {{ return e.message }}");
    assert_eq!(
        run(&h, &session, &code).await,
        "[browser.disconnected] Browser connection closed; the action may already have run. Reconnect this session in the desktop app, call browser.tabs.list({}), and inspect the target tab with browser.snapshot({tabID}). Do not repeat clicks, submissions, uploads, or evaluations until their outcome is known."
    );
    let code = "try { return await tools.browser.tabs.list({}) } catch (e) { return e.message }";
    assert_eq!(
        run(&h, &session, code).await,
        "[browser.disconnected] No desktop browser is connected to this session. Open this session in the desktop app and wait for it to connect. Then call browser.tabs.list({}). Repeating browser actions while disconnected will not help."
    );
}

#[tokio::test]
async fn tunnels_relay_loopback_bytes_until_the_attachment_ends() {
    let h = harness();
    let session = h.session().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 1024];
                let n = socket.read(&mut buffer).await.unwrap_or(0);
                let _ = socket.write_all(&[b"echo:".as_slice(), &buffer[..n]].concat()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    let attached = attach(&h, &session, "c1").await;
    let base = json!({ "sessionID": session, "connectionID": "c1" });
    let with = |extra: Value| {
        let mut input = base.clone();
        input.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        input
    };
    let (status, opened) = rpc(&h.router, &h.dir(), "tunnel.open", with(json!({ "target": { "host": "127.0.0.1", "port": port } }))).await;
    assert_eq!(status, 200);
    let tunnel = opened["output"].as_str().unwrap().to_string();
    let data = base64::engine::general_purpose::STANDARD.encode(b"ping");
    assert_eq!(rpc(&h.router, &h.dir(), "tunnel.write", with(json!({ "tunnelID": tunnel, "data": data }))).await, (200, json!({})));
    let mut received = Vec::new();
    loop {
        let (status, read) = rpc(&h.router, &h.dir(), "tunnel.read", with(json!({ "tunnelID": tunnel }))).await;
        assert_eq!(status, 200);
        if read["output"]["eof"] == true {
            break;
        }
        received.extend(base64::engine::general_purpose::STANDARD.decode(read["output"]["data"].as_str().unwrap()).unwrap());
    }
    assert_eq!(received, b"echo:ping");
    let refused = {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let (_, body) = rpc(&h.router, &h.dir(), "tunnel.open", with(json!({ "target": { "host": "127.0.0.1", "port": port } }))).await;
        (body["message"].clone(), port)
    };
    assert_eq!(refused.0, format!("connect ECONNREFUSED 127.0.0.1:{}", refused.1));
    let (_, opened) = rpc(&h.router, &h.dir(), "tunnel.open", with(json!({ "target": { "host": "127.0.0.1", "port": port } }))).await;
    let open = opened["output"].as_str().unwrap().to_string();
    attached.abort();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (status, body) = rpc(&h.router, &h.dir(), "tunnel.read", with(json!({ "tunnelID": open }))).await;
    assert_eq!(status, 400);
    assert_eq!(body["message"], "Browser attachment is unavailable; its network connections were closed.");
}
