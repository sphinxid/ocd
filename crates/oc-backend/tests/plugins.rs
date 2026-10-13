//! Server plugins end to end: the real Bun sidecar runs oc-plugin's fixture plugins against the
//! backend (HTTP API) and the session runner (mock OpenAI-compatible provider).
//!
//! Bun: `OCD_BUN_PATH`, else `OCD_TEST_BUN`, else the session scratchpad's bun 1.4.2; the tests
//! are skipped (with a message) when none exists.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::{Router, body::Body, response::Response, routing::post};
use common::*;
use parking_lot::Mutex;
use serde_json::{Value, json};

const DEFAULT_TEST_BUN: &str = "$OCD_SCRATCH/bun142/bun-linux-x64/bun";

fn bun() -> Option<PathBuf> {
    static BUN: OnceLock<Option<PathBuf>> = OnceLock::new();
    BUN.get_or_init(|| {
        let path = std::env::var_os("OCD_BUN_PATH")
            .or_else(|| std::env::var_os("OCD_TEST_BUN"))
            .map(PathBuf::from)
            .or_else(|| Some(PathBuf::from(DEFAULT_TEST_BUN)).filter(|p| p.exists()))?;
        // SAFETY: set once, before any sidecar is spawned, to the same value in every test.
        unsafe { std::env::set_var("OCD_BUN_PATH", &path) };
        Some(path)
    })
    .clone()
}

macro_rules! require_bun {
    () => {
        if bun().is_none() {
            eprintln!("skipping: set OCD_BUN_PATH or OCD_TEST_BUN to a bun 1.4.2 binary");
            return;
        }
    };
}

fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../oc-plugin/tests/fixtures").join(name).canonicalize().unwrap().display().to_string()
}

/// One scripted model turn.
#[derive(Clone)]
enum Turn {
    Tool(&'static str, Value),
    Text(&'static str),
}

struct Mock {
    port: u16,
    bodies: Arc<Mutex<Vec<Value>>>,
}

fn sse(chunks: &[Value]) -> Response {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    Response::builder().header("content-type", "text/event-stream").body(Body::from(body)).unwrap()
}

async fn mock(turns: Vec<Turn>) -> Mock {
    let bodies = Arc::new(Mutex::new(Vec::<Value>::new()));
    let script = Arc::new(Mutex::new(turns));
    let recorded = bodies.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::Json<Value>| {
            let is_title = body.0.to_string().contains("You are a title generator");
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
                        json!({"id":"t","choices":[{"index":0,"delta":{"role":"assistant","content":"Plugin test"}}]}),
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
    Mock { port, bodies }
}

struct Setup {
    h: Harness,
    mock: Mock,
}

impl Setup {
    /// The tool names offered in the `n`th model request.
    fn tools(&self, n: usize) -> Vec<Value> {
        self.mock.bodies.lock().get(n).and_then(|b| b["tools"].as_array().cloned()).unwrap_or_default()
    }

    /// Prompts the session and waits until it is idle.
    async fn run(&self, session: &str, text: &str) {
        let r = self.h.post(&format!("/api/session/{session}/prompt"), json!({ "text": text })).await;
        assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::timeout(Duration::from_secs(60), self.h.app.coordinator().await_idle(session)).await.unwrap();
    }

    fn messages(&self, session: &str) -> Vec<Value> {
        self.h.app.db.session_messages(session).unwrap().into_iter().map(|(_, m)| serde_json::to_value(m).unwrap()).collect()
    }

    async fn shutdown(&self) {
        self.h.app.plugins.shutdown().await;
    }
}

/// A backend with the session runner, a mock provider and `plugins` configured at the location.
async fn setup(plugins: Value, turns: Vec<Turn>) -> Setup {
    let mock = mock(turns).await;
    // SAFETY: tests only ever set this to the same value.
    unsafe { std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1") };
    let root = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    let cwd = root_path.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        cwd.join("opencode.json"),
        json!({
            "model": "mock/m1",
            "plugins": plugins,
            "providers": {"mock": {
                "name": "Mock",
                "package": "@opencode/ai/providers/openai-compatible",
                "settings": {"baseURL": format!("http://127.0.0.1:{}/v1", mock.port), "apiKey": "test"},
                "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}}}
            }}
        })
        .to_string(),
    )
    .unwrap();
    let db = oc_storage::Database::memory().unwrap();
    let app = oc_core::app::App::new(global(&root_path), db, "1.2.3");
    let services = oc_core::services::default_services(&app);
    let runner = oc_core::runner::Engine::new(app.clone(), &services);
    let opts = oc_server::ServerOptions { cwd: Some(cwd.display().to_string()), version: "1.2.3".into(), ready: true, ..Default::default() };
    let (router, _) = oc_backend::router(app.clone(), Some(runner), &opts).unwrap();
    let engine = oc_backend::Engine::new(app.clone());
    let h = Harness { app, engine, router, root, cwd };
    // Boot the location and wait for its plugins.
    let r = h.get("/api/location").await;
    assert_eq!(r.status, 200);
    h.app.plugins.await_activation(&h.dir()).await;
    Setup { h, mock }
}

fn kv_key(plugin: &str, key: &str) -> String {
    format!("{}{key}", oc_plugin::storage::namespace(plugin))
}

#[tokio::test]
async fn plugin_tool_offered_and_runs_with_before_hook() {
    require_bun!();
    let s = setup(
        json!([{ "package": fixture("promise-plugin"), "options": { "suffix": "!" } }]),
        vec![Turn::Tool("echo", json!({ "text": "hi" })), Turn::Text("echoed")],
    )
    .await;
    assert!(s.h.app.plugins.host().is_some_and(|h| h.is_running()));

    // plugin.list: built-ins (pre), the local plugin, built-ins (post).
    let list = s.h.get("/api/plugin").await.json();
    let ids: Vec<&str> = list["data"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
    let at = ids.iter().position(|i| *i == "fixture.promise").expect("plugin listed");
    assert_eq!(ids[at - 1], "opencode.warming");
    assert_eq!(ids[at + 1], "opencode.config.instruction");
    assert_eq!(list["data"][at]["source"]["type"], "local");
    assert!(list["data"][at]["source"]["path"].as_str().unwrap().starts_with(&fixture("promise-plugin")));
    assert_eq!(list["data"][at]["state"], json!({ "status": "active" }));

    // The command from the plugin appears.
    let commands = s.h.get("/api/command").await.json();
    assert!(commands["data"].as_array().unwrap().iter().any(|c| c["name"] == "hello" && c["description"] == "Say hello"), "{commands}");

    let session = s.h.session().await;
    s.run(&session, "echo please").await;

    // The plugin tool was offered to the model (direct: `codemode: false`), the native `read`
    // carries the plugin's description edit.
    let tools = s.tools(0);
    let echo = tools.iter().find(|t| t["function"]["name"] == "echo").expect("echo offered");
    assert_eq!(echo["function"]["description"], "Echo text!");
    assert_eq!(echo["function"]["parameters"]["required"], json!(["text"]));
    let read = tools.iter().find(|t| t["function"]["name"] == "read").expect("read offered");
    assert!(read["function"]["description"].as_str().unwrap().ends_with(" (wrapped)"));

    // It ran in the sidecar, with the input rewritten by `tool.execute.before`.
    let messages = s.messages(&session);
    let tool = messages.iter().flat_map(|m| m["content"].as_array().cloned().unwrap_or_default()).find(|c| c["type"] == "tool").expect("tool call");
    assert_eq!(tool["name"], "echo");
    assert_eq!(tool["state"]["status"], "completed", "{tool}");
    assert_eq!(tool["state"]["content"], json!([{ "type": "text", "text": "echo: [hi]" }]));
    assert_eq!(tool["state"]["metadata"]["session"], json!(session));
    let second = s.mock.bodies.lock()[1].to_string();
    assert!(second.contains("echo: [hi]"), "{second}");

    // A plugin command runs in the sidecar and can use the API and storage.
    let r = s.h.post(&format!("/api/session/{session}/command"), json!({ "name": "hello", "text": "" })).await;
    assert_eq!(r.status, 204, "{:?}", String::from_utf8_lossy(&r.body));
    let stored = s.h.app.db.kv_get(&kv_key("fixture.promise", "hello")).unwrap().expect("stored by command");
    assert_eq!(stored["sessionID"], json!(session));
    assert!(stored["agents"].as_u64().unwrap() > 0);

    // Closing the location runs the plugin's cleanup (storage persists in the kv table).
    let r = s.h.req("DELETE", "/api/debug/location", None).await;
    assert_eq!(r.status, 204);
    assert_eq!(s.h.app.db.kv_get(&kv_key("fixture.promise", "cleanup")).unwrap(), Some(json!(true)));
    s.shutdown().await;
}

#[tokio::test]
async fn effect_plugin_agents_skills_and_rejecting_hook() {
    require_bun!();
    let s = setup(json!([fixture("effect-plugin")]), vec![Turn::Tool("forbidden", json!({})), Turn::Text("ok")]).await;

    let agents = s.h.get("/api/agent").await.json();
    let list = agents["data"].as_array().unwrap();
    let ids: Vec<&str> = list.iter().map(|a| a["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"reviewer"), "{ids:?}");
    assert!(!ids.contains(&"plan"), "{ids:?}");
    let build = list.iter().find(|a| a["id"] == "build").unwrap();
    assert_eq!(build["description"], "Build things (from effect plugin)");
    let reviewer = list.iter().find(|a| a["id"] == "reviewer").unwrap();
    assert_eq!(reviewer["mode"], "subagent");
    // The browser plugin (`post`) still applies to the plugin's agent.
    assert_eq!(reviewer["permissions"].as_array().unwrap().last().unwrap(), &json!({ "action": "browser", "resource": "*", "effect": "deny" }));

    let skills = s.h.get("/api/skill").await.json();
    assert!(skills["data"].as_array().unwrap().iter().any(|k| k["id"] == "fixture-skill"), "{skills}");

    // `tool.execute.before` failing with a Tool.Error rejects the call before it runs.
    let session = s.h.session().await;
    s.run(&session, "try the forbidden tool").await;
    let messages = s.messages(&session);
    let tool = messages.iter().flat_map(|m| m["content"].as_array().cloned().unwrap_or_default()).find(|c| c["type"] == "tool").expect("tool call");
    assert_eq!(tool["name"], "forbidden");
    assert_eq!(tool["state"]["status"], "error", "{tool}");
    assert_eq!(tool["state"]["error"]["message"], "forbidden by fixture");
    assert_eq!(tool["state"]["metadata"], json!({ "reason": "test" }));
    s.shutdown().await;
}

#[tokio::test]
async fn permission_evaluate_and_session_hooks() {
    require_bun!();
    let s = setup(
        json!([fixture("permission-plugin")]),
        vec![
            Turn::Tool("read", json!({ "path": "allowed.env" })),
            Turn::Tool("read", json!({ "path": "blocked.env" })),
            Turn::Tool("glob", json!({ "pattern": "*.env" })),
            Turn::Text("done"),
        ],
    )
    .await;
    std::fs::write(s.h.cwd.join("allowed.env"), "SECRET=1\n").unwrap();
    std::fs::write(s.h.cwd.join("blocked.env"), "SECRET=2\n").unwrap();
    let session = s.h.session().await;
    let r = s.h.post(&format!("/api/session/{session}/prompt"), json!({ "text": "read the env files" })).await;
    assert_eq!(r.status, 200);
    let admitted = r.json();
    // `session.prompt` rewrote the text before admission.
    assert_eq!(admitted["data"]["payload"]["text"], "read the env files [prompt hook]", "{admitted}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::timeout(Duration::from_secs(60), s.h.app.coordinator().await_idle(&session)).await.unwrap();

    let messages = s.messages(&session);
    let tools: Vec<Value> = messages.iter().flat_map(|m| m["content"].as_array().cloned().unwrap_or_default()).filter(|c| c["type"] == "tool").collect();
    assert_eq!(tools.len(), 3, "{messages:#?}");
    // `.env` reads ask by default; the hook allowed one (no permission request) and denied the other.
    assert_eq!(tools[0]["state"]["status"], "completed", "{}", tools[0]);
    assert!(tools[0]["state"]["content"].to_string().contains("SECRET=1"));
    assert_eq!(tools[1]["state"]["status"], "error", "{}", tools[1]);
    assert_eq!(tools[1]["state"]["error"]["message"], "blocked by fixture");
    // `tool.execute.after` replaced the glob result.
    assert_eq!(tools[2]["state"]["content"], json!([{ "type": "text", "text": "glob result replaced" }]));
    // `session.context` added a system part to every primary request.
    let first = s.mock.bodies.lock()[0].to_string();
    assert!(first.contains("context hook was here"), "{first}");
    s.shutdown().await;
}

#[tokio::test]
async fn storage_events_and_rpc() {
    require_bun!();
    let s = setup(json!([fixture("storage-events")]), vec![]).await;
    assert_eq!(s.h.app.db.kv_get(&kv_key("fixture.storage", "boots")).unwrap(), Some(json!({ "count": 1 })));
    assert_eq!(s.h.app.db.kv_get(&kv_key("fixture.storage", "scanned")).unwrap(), Some(json!(["prefix/a", "prefix/b"])));

    // rpc.call through the HTTP endpoint.
    let r = s.h.post("/api/rpc/fixture.echo/ping", json!({ "input": { "text": "hey" } })).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json(), json!({ "output": { "text": "pong: hey" } }));
    let r = s.h.post("/api/rpc/fixture.echo/ping", json!({ "input": { "text": "no" } })).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json(), json!({ "_tag": "RpcError", "type": "refused", "message": "Refusing to ping", "data": { "why": "said no" } }));
    let r = s.h.post("/api/rpc/fixture.echo/nope", json!({})).await;
    assert_eq!(r.json(), json!({ "_tag": "RpcError", "type": "rpc.method_not_found", "message": "Unknown RPC method: fixture.echo.nope" }));
    let r = s.h.post("/api/rpc/missing/ping", json!({})).await;
    assert_eq!(r.json(), json!({ "_tag": "RpcError", "type": "rpc.unavailable", "message": "RPC is unavailable: missing" }));

    // `ctx.event.subscribe()` receives bus events: the RPC's own event and a session event.
    let session = s.h.session().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let pong = s.h.app.db.kv_get(&kv_key("fixture.storage", "event:rpc.fixture.echo.pong")).unwrap();
        let created = s.h.app.db.kv_get(&kv_key("fixture.storage", "event:session.created")).unwrap();
        if let (Some(pong), Some(created)) = (pong, created) {
            assert_eq!(pong, json!({ "text": "hey" }));
            assert_eq!(created["sessionID"], json!(session));
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "events not delivered");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Storage persists across activations: a reload boots the plugin again.
    let r = s.h.req("DELETE", "/api/debug/location", None).await;
    assert_eq!(r.status, 204);
    s.h.get("/api/location").await;
    s.h.app.plugins.await_activation(&s.h.dir()).await;
    assert_eq!(s.h.app.db.kv_get(&kv_key("fixture.storage", "boots")).unwrap(), Some(json!({ "count": 2 })));
    s.shutdown().await;
}

#[tokio::test]
async fn no_sidecar_without_plugins() {
    let s = setup(json!(["-opencode.browser"]), vec![]).await;
    assert!(!s.h.app.plugins.host().is_some_and(|h| h.is_running()));
    let list = s.h.get("/api/plugin").await.json();
    let ids: Vec<&str> = list["data"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), 87);
    assert!(!ids.contains(&"opencode.browser"));
    // The browser deny is gone with its plugin.
    let agents = s.h.get("/api/agent").await.json();
    let build = agents["data"].as_array().unwrap().iter().find(|a| a["id"] == "build").unwrap().clone();
    assert!(!build["permissions"].to_string().contains("browser"));
    s.shutdown().await;
}

#[tokio::test]
async fn catalog_integration_websearch_and_codemode_tools() {
    require_bun!();
    let s = setup(
        json!([fixture("catalog-plugin")]),
        vec![Turn::Tool("execute", json!({ "code": "return await tools.shout({ text: 'hi' })" })), Turn::Text("shouted")],
    )
    .await;
    let d = s.h.dir();
    // provider/model transforms run between the internal provider plugins and the configured ones.
    let providers = s.h.get("/api/provider").await.json();
    assert!(providers["data"].as_array().unwrap().iter().any(|p| p["id"] == "fixture-provider" && p["name"] == "Fixture Provider"), "{providers}");
    let models = s.h.get("/api/model").await.json();
    let list = models["data"].as_array().unwrap();
    assert!(list.iter().any(|m| m["providerID"] == "opencode" && m["id"] == "big-pickle" && m["name"] == "Big Pickle (renamed)"), "{models}");
    assert!(list.iter().any(|m| m["providerID"] == "fixture-provider" && m["id"] == "fixture-model"), "{models}");
    // integration transform.
    let integration = s.h.get("/api/integration/fixture-integration").await;
    assert_eq!(integration.status, 200, "{:?}", String::from_utf8_lossy(&integration.body));
    let integration = integration.json();
    assert_eq!(integration["data"]["name"], "Fixture Integration");
    assert_eq!(integration["data"]["methods"][0]["type"], "key");
    // websearch transform: the plugin provider is listed and queried through its handle.
    let search = s.h.get("/api/websearch/provider").await.json();
    assert!(search["data"].to_string().contains("fixture-search"), "{search}");
    let r = s.h.post("/api/websearch", json!({ "query": "rust", "providerID": "fixture-search" })).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["data"]["results"][0]["url"], "https://example.com/rust");
    let _ = d;

    // A Code Mode tool from a plugin, called inside `execute`: `tool.execute.before` applies to the
    // nested call too.
    let session = s.h.session().await;
    s.run(&session, "shout").await;
    let messages = s.messages(&session);
    let tool = messages.iter().flat_map(|m| m["content"].as_array().cloned().unwrap_or_default()).find(|c| c["type"] == "tool").expect("tool call");
    assert_eq!(tool["name"], "execute");
    assert_eq!(tool["state"]["status"], "completed", "{tool}");
    assert!(tool["state"]["content"].to_string().contains("HI!"), "{tool}");
    s.shutdown().await;
}

#[tokio::test]
async fn local_plugin_hot_reload() {
    require_bun!();
    if oc_config::discovery::filewatcher_disabled() {
        eprintln!("skipping: file watching is disabled");
        return;
    }
    let source = |name: &str| {
        format!(
            "import {{ Plugin }} from \"@opencode/plugin\"\nexport default Plugin.define({{ id: \"fixture.reload\", setup: async (ctx) => {{ await ctx.command.transform((e) => {{ e.add({{ name: \"{name}\", execute: async () => {{}} }}) }}) }} }})\n"
        )
    };
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().canonicalize().unwrap().join("reload");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(plugin.join("index.ts"), source("before")).unwrap();
    let s = setup(json!([plugin.display().to_string()]), vec![]).await;
    let names = |v: Value| v["data"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert!(names(s.h.get("/api/command").await.json()).contains(&"before".to_string()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    std::fs::write(plugin.join("index.ts"), source("after")).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let list = names(s.h.get("/api/command").await.json());
        if list.contains(&"after".to_string()) && !list.contains(&"before".to_string()) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "plugin was not reloaded: {list:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    s.shutdown().await;
}
