//! The built-in GitHub Copilot provider plugin end to end against a local mock Copilot API (no
//! network, fake token): the live `/models` sync and the catalog it derives, the routing of each
//! model (upstream's in-tree AI SDK provider in the Bun sidecar for `/responses` and
//! `/chat/completions`, the native Anthropic package for `/v1/messages`), the Copilot request
//! headers, and the utility-model title.
//!
//! The request tests need Bun (`OCD_BUN_PATH`, `OCD_TEST_BUN` or `$OCD_SCRATCH/bun144/...`) and are
//! skipped without it.

mod common;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::extract::{Path as UrlPath, State};
use axum::http::HeaderMap;
use axum::{Router, body::Body, response::Response, routing::get, routing::post};
use common::*;
use parking_lot::Mutex;
use serde_json::{Value, json};

fn bun() -> Option<PathBuf> {
    static BUN: OnceLock<Option<PathBuf>> = OnceLock::new();
    BUN.get_or_init(|| {
        let path = std::env::var_os("OCD_BUN_PATH")
            .or_else(|| std::env::var_os("OCD_TEST_BUN"))
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("OCD_SCRATCH").map(|d| PathBuf::from(d).join("bun144/bun-linux-x64/bun")).filter(|p| p.exists()))?;
        // SAFETY: set once, before any sidecar is spawned, to the same value in every test.
        unsafe { std::env::set_var("OCD_BUN_PATH", &path) };
        Some(path)
    })
    .clone()
}

macro_rules! require_bun {
    () => {
        if bun().is_none() {
            eprintln!("skipping: set OCD_BUN_PATH or OCD_TEST_BUN to a bun 1.4.4 binary");
            return;
        }
    };
}

#[derive(Debug, Clone)]
struct Recorded {
    path: String,
    headers: HeaderMap,
    body: Value,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<String> {
        self.headers.get(name).map(|v| v.to_str().unwrap().to_string())
    }
}

#[derive(Clone, Default)]
struct Mock {
    port: u16,
    requests: Arc<Mutex<Vec<Recorded>>>,
    /// Main (non-title) requests answered so far: the first calls the `read` tool.
    turns: Arc<Mutex<usize>>,
}

fn remote(id: &str, endpoints: Option<&[&str]>, extra: Value) -> Value {
    let mut model = json!({
        "model_picker_enabled": true, "id": id, "name": format!("Remote {id}"), "version": id,
        "capabilities": {"family": "remote", "limits": {"max_context_window_tokens": 200000, "max_output_tokens": 64000, "max_prompt_tokens": 128000},
                         "supports": {"streaming": true, "tool_calls": true}},
    });
    if let Some(endpoints) = endpoints {
        model["supported_endpoints"] = json!(endpoints);
    }
    for (k, v) in extra.as_object().unwrap() {
        model[k] = v.clone();
    }
    model
}

fn models() -> Value {
    json!({"data": [
        remote("gpt-5.4", Some(&["/responses", "/chat/completions"]), json!({"version": "gpt-5.4-2026-03-05"})),
        remote("claude-sonnet-4.6", Some(&["/v1/messages", "/chat/completions"]), json!({})),
        remote("gemini-3.5-flash", Some(&["/chat/completions"]), json!({})),
        remote("gpt-4o-mini", Some(&["/chat/completions"]), json!({"model_picker_enabled": false})),
        remote("gpt-5-chat-latest", Some(&["/chat/completions"]), json!({})),
        remote("remote-only", None, json!({"version": "2025-10-01"})),
        remote("disabled", Some(&["/chat/completions"]), json!({"policy": {"state": "disabled"}})),
    ]})
}

fn events(events: &[(String, Value)]) -> Response {
    let mut body = String::new();
    for (name, data) in events {
        if name.is_empty() {
            body.push_str(&format!("data: {data}\n\n"));
        } else {
            body.push_str(&format!("event: {name}\ndata: {data}\n\n"));
        }
    }
    if events.iter().all(|(name, _)| name.is_empty()) {
        body.push_str("data: [DONE]\n\n");
    }
    Response::builder().header("content-type", "text/event-stream").body(Body::from(body)).unwrap()
}

fn responses_stream(tool: Option<Value>, text: &str) -> Response {
    let mut out = vec![("response.created".into(), json!({"type": "response.created", "response": {"id": "resp_1", "created_at": 0, "model": "m"}}))];
    match tool {
        Some(args) => {
            let item = json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": ""});
            out.push(("response.output_item.added".into(), json!({"type": "response.output_item.added", "output_index": 0, "item": item})));
            let mut done = item.clone();
            done["arguments"] = json!(args.to_string());
            done["status"] = json!("completed");
            out.push(("response.output_item.done".into(), json!({"type": "response.output_item.done", "output_index": 0, "item": done})));
        }
        None => {
            out.push(("response.output_item.added".into(), json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": "msg_1"}})));
            out.push(("response.output_text.delta".into(), json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": text})));
            out.push(("response.output_item.done".into(), json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message", "id": "msg_1"}})));
        }
    }
    out.push(("response.completed".into(), json!({"type": "response.completed", "response": {"usage": {"input_tokens": 10, "output_tokens": 5}}})));
    events(&out)
}

fn chat_stream(text: &str) -> Response {
    events(&[
        (String::new(), json!({"id": "c", "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}}]})),
        (String::new(), json!({"id": "c", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 10, "completion_tokens": 5}})),
    ])
}

fn messages_stream(text: &str) -> Response {
    events(&[
        ("message_start".into(), json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": "m", "content": [], "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}})),
        ("content_block_start".into(), json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})),
        ("content_block_delta".into(), json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}})),
        ("content_block_stop".into(), json!({"type": "content_block_stop", "index": 0})),
        ("message_delta".into(), json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}})),
        ("message_stop".into(), json!({"type": "message_stop"})),
    ])
}

async fn answer(State(mock): State<Mock>, UrlPath(path): UrlPath<String>, headers: HeaderMap, body: axum::Json<Value>) -> Response {
    let path = format!("/{path}");
    let title = body.0.to_string().contains("You name conversations");
    mock.requests.lock().push(Recorded { path: path.clone(), headers, body: body.0.clone() });
    if body.0["stream"] == json!(false) {
        let completion = json!({"id": "c", "object": "chat.completion", "choices": [{"index": 0, "message": {"role": "assistant", "content": "  Utility title \n"}, "finish_reason": "stop"}]});
        return Response::builder().header("content-type", "application/json").body(Body::from(completion.to_string())).unwrap();
    }
    let first = !title && {
        let mut turns = mock.turns.lock();
        *turns += 1;
        *turns == 1
    };
    let text = if title { "Model title" } else { "done" };
    if path.ends_with("/responses") {
        return responses_stream(first.then(|| json!({"path": "hello.txt"})), text);
    }
    if path.ends_with("/messages") {
        return messages_stream(text);
    }
    chat_stream(text)
}

async fn mock() -> Mock {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock = Mock { port: listener.local_addr().unwrap().port(), ..Default::default() };
    let recorder = mock.clone();
    let app = Router::new()
        .route(
            "/models",
            get(move |headers: HeaderMap| {
                recorder.requests.lock().push(Recorded { path: "/models".into(), headers, body: Value::Null });
                async { axum::Json(models()) }
            }),
        )
        .route("/{*path}", post(answer))
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    mock
}

struct Setup {
    h: Harness,
    mock: Mock,
}

async fn setup(model: &str) -> Setup {
    let mock = mock().await;
    // SAFETY: tests only ever set this to the same value.
    unsafe { std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1") };
    let root = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    let cwd = root_path.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("hello.txt"), "hello from the file\n").unwrap();
    std::fs::write(cwd.join("opencode.json"), json!({ "model": model }).to_string()).unwrap();
    let db = oc_storage::Database::memory().unwrap();
    let app = oc_core::app::App::new(global(&root_path), db, "1.2.3");
    let services = oc_core::services::default_services(&app);
    let runner = oc_core::runner::Engine::new(app.clone(), &services);
    let opts = oc_server::ServerOptions { cwd: Some(cwd.display().to_string()), version: "1.2.3".into(), ready: true, ..Default::default() };
    let (router, _) = oc_backend::router(app.clone(), Some(runner), &opts).unwrap();
    let engine = oc_backend::Engine::new(app.clone());
    let h = Harness { app, engine, router, root, cwd };
    assert_eq!(h.get("/api/location").await.status, 200);
    Setup { h, mock }
}

impl Setup {
    /// A fake Copilot OAuth connection whose API endpoint is the mock.
    async fn connect(&self) {
        let r = self
            .h
            .post(
                "/api/credential",
                json!({"integrationID": "github-copilot", "value": {"type": "oauth", "methodID": "device", "refresh": "gho_refresh",
                       "access": "gho_access", "expires": 0, "metadata": {"apiEndpoint": format!("http://127.0.0.1:{}", self.mock.port)}}}),
            )
            .await;
        assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    }

    async fn models(&self) -> Vec<Value> {
        let r = self.h.get("/api/model").await;
        assert_eq!(r.status, 200);
        r.json()["data"].as_array().unwrap().iter().filter(|m| m["providerID"] == "github-copilot").cloned().collect()
    }

    /// The Copilot catalog once the `/models` sync after connecting has landed.
    async fn synced(&self) -> Vec<Value> {
        for _ in 0..100 {
            let models = self.models().await;
            if models.iter().any(|m| m["id"] == "remote-only") {
                return models;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the Copilot model list never synced: {:?}", self.models().await);
    }

    async fn run(&self, session: &str, text: &str) {
        let r = self.h.post(&format!("/api/session/{session}/prompt"), json!({ "text": text })).await;
        assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::timeout(Duration::from_secs(60), self.h.app.coordinator().await_idle(session)).await.unwrap();
    }

    fn requests(&self, path: &str) -> Vec<Recorded> {
        self.mock.requests.lock().iter().filter(|r| r.path == path).cloned().collect()
    }

    async fn title(&self, session: &str) -> Option<String> {
        for _ in 0..100 {
            let r = self.h.get(&format!("/api/session/{session}")).await;
            let title = r.json()["data"]["title"].as_str().map(String::from);
            if title.as_deref().is_some_and(|t| !t.starts_with("New session")) {
                return title;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    }
}

fn find<'a>(models: &'a [Value], id: &str) -> &'a Value {
    models.iter().find(|m| m["id"] == id).unwrap_or_else(|| panic!("{id} missing"))
}

#[tokio::test]
async fn the_live_model_list_drives_the_catalog() {
    let s = setup("github-copilot/gpt-5.4").await;
    s.connect().await;
    let models = s.synced().await;
    let base = format!("http://127.0.0.1:{}", s.mock.port);

    let gpt = find(&models, "gpt-5.4");
    assert_eq!(gpt["package"], "aisdk:@ai-sdk/github-copilot");
    assert_eq!(gpt["settings"], json!({"baseURL": base, "endpoint": "responses"}));
    let claude = find(&models, "claude-sonnet-4.6");
    assert_eq!(claude["package"], "@opencode/ai/providers/anthropic");
    assert_eq!(claude["settings"], json!({"baseURL": format!("{base}/v1"), "endpoint": "messages"}));
    assert_eq!(find(&models, "gemini-3.5-flash")["settings"]["endpoint"], "chat");
    let added = find(&models, "remote-only");
    assert_eq!(added["name"], "Remote remote-only");
    assert_eq!(added["time"]["released"], 1759276800000u64);
    assert!(added["settings"].get("endpoint").is_none());
    // Only models the account has; the policy-disabled one, the picker-hidden utility model and the
    // chat alias are not offered.
    for absent in ["disabled", "gpt-4o-mini", "gpt-5-chat-latest", "claude-opus-4.7"] {
        assert!(models.iter().all(|m| m["id"] != absent), "{absent}");
    }

    let sync = s.requests("/models");
    assert_eq!(sync.len(), 1);
    assert_eq!(sync[0].header("authorization").as_deref(), Some("Bearer gho_refresh"));
    assert_eq!(sync[0].header("x-github-api-version").as_deref(), Some("2026-08-01"));
    assert_eq!(sync[0].header("user-agent").as_deref(), Some(format!("opencode/{}/1.2.3/cli", oc_util::version::CHANNEL).as_str()));
}

#[tokio::test]
async fn responses_route_headers_and_utility_title() {
    require_bun!();
    let s = setup("github-copilot/gpt-5.4").await;
    s.connect().await;
    s.synced().await;
    let session = s.h.session().await;
    s.run(&session, "read hello.txt").await;

    let main = s.requests("/responses");
    assert_eq!(main.len(), 2, "{main:?}");
    for (request, initiator) in main.iter().zip(["user", "agent"]) {
        assert_eq!(request.body["model"], "gpt-5.4");
        assert_eq!(request.header("authorization").as_deref(), Some("Bearer gho_access"));
        assert_eq!(request.header("openai-intent").as_deref(), Some("conversation-edits"));
        assert_eq!(request.header("x-github-api-version").as_deref(), Some("2026-08-01"));
        assert_eq!(request.header("x-interaction-type").as_deref(), Some("conversation-agent"));
        assert_eq!(request.header("x-interaction-id").as_deref(), Some(session.as_str()));
        assert_eq!(request.header("x-initiator").as_deref(), Some(initiator));
        assert_eq!(request.header("user-agent").as_deref(), Some(format!("opencode/{}/1.2.3/cli", oc_util::version::CHANNEL).as_str()));
    }
    // The tool result went back on the Responses wire.
    let input = main[1].body["input"].as_array().unwrap();
    assert!(input.iter().any(|i| i["type"] == "function_call_output"), "{input:?}");

    // The title came from the free utility model, not from a billed title request.
    assert_eq!(s.title(&session).await.as_deref(), Some("Utility title"));
    let titles: Vec<Recorded> = s.requests("/chat/completions");
    assert_eq!(titles.len(), 1, "{titles:?}");
    assert_eq!(titles[0].body["model"], "gpt-4o-mini");
    assert_eq!(titles[0].body["stream"], false);
    assert_eq!(titles[0].header("authorization").as_deref(), Some("Bearer gho_refresh"));
    assert_eq!(titles[0].header("x-interaction-type").as_deref(), Some("agent-session-name-generation"));
    assert_eq!(titles[0].header("x-initiator").as_deref(), Some("agent"));
    s.h.app.plugins.shutdown().await;
}

#[tokio::test]
async fn chat_and_native_messages_routes() {
    require_bun!();
    let s = setup("github-copilot/gemini-3.5-flash").await;
    s.connect().await;
    s.synced().await;
    let session = s.h.session().await;
    s.run(&session, "hi").await;
    let chat: Vec<Recorded> = s.requests("/chat/completions").into_iter().filter(|r| r.body["stream"] == true).collect();
    assert!(!chat.is_empty());
    let request = chat.iter().find(|r| r.body["model"] == "gemini-3.5-flash").expect("gemini request");
    assert_eq!(request.header("authorization").as_deref(), Some("Bearer gho_access"));
    assert_eq!(request.header("x-initiator").as_deref(), Some("user"));

    let r = s.h.post(&format!("/api/session/{session}/model"), json!({"model": {"providerID": "github-copilot", "id": "claude-sonnet-4.6"}})).await;
    assert!(r.status.is_success());
    s.run(&session, "again").await;
    let messages = s.requests("/v1/messages");
    assert_eq!(messages.len(), 1, "{messages:?}");
    // The native Anthropic package with the OAuth token as bearer; the Copilot interaction headers.
    assert_eq!(messages[0].header("authorization").as_deref(), Some("Bearer gho_access"));
    assert_eq!(messages[0].header("x-api-key"), None);
    assert_eq!(messages[0].header("x-interaction-type").as_deref(), Some("conversation-agent"));
    assert_eq!(messages[0].header("x-interaction-id").as_deref(), Some(session.as_str()));
    s.h.app.plugins.shutdown().await;
}
