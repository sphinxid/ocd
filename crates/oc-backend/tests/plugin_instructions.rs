//! opencode 2.2's plugin API additions end to end: the real Bun sidecar runs oc-plugin's `v22-plugin` fixture
//! against the backend. Its `ctx.session.instructions.entry.*` and `ctx.experimental.generate.*` calls reach the
//! HTTP endpoints `experimental.session.instructions.entry.*` and `experimental.generate.*` (port of upstream's
//! `test/plugin/session-instructions.test.ts`).
//!
//! Bun: `OCD_BUN_PATH`, else `OCD_TEST_BUN`, else `$OCD_SCRATCH/bun144/bun-linux-x64/bun`; the tests are skipped
//! (with a message) when none exists.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::{Router, body::Body, response::Response, routing::post};
use common::*;
use parking_lot::Mutex;
use serde_json::{Value, json};

fn scratch_bun() -> Option<PathBuf> {
    std::env::var_os("OCD_SCRATCH").map(|dir| PathBuf::from(dir).join("bun144/bun-linux-x64/bun"))
}

fn bun() -> Option<PathBuf> {
    static BUN: OnceLock<Option<PathBuf>> = OnceLock::new();
    BUN.get_or_init(|| {
        let path = std::env::var_os("OCD_BUN_PATH")
            .or_else(|| std::env::var_os("OCD_TEST_BUN"))
            .map(PathBuf::from)
            .or_else(|| scratch_bun().filter(|p| p.exists()))?;
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

fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../oc-plugin/tests/fixtures").join(name).canonicalize().unwrap().display().to_string()
}

fn sse(chunks: &[Value]) -> Response {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    Response::builder().header("content-type", "text/event-stream").body(Body::from(body)).unwrap()
}

/// An OpenAI-compatible mock that answers every chat request with `text` and records the bodies.
async fn mock(text: &'static str) -> (u16, Arc<Mutex<Vec<Value>>>) {
    let bodies = Arc::new(Mutex::new(Vec::<Value>::new()));
    let recorded = bodies.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::Json<Value>| {
            let title = body.0.to_string().contains("You name conversations");
            if !title {
                recorded.lock().push(body.0.clone());
            }
            async move {
                let usage = json!({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15});
                let content = if title { "Title" } else { text };
                sse(&[
                    json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","content":content}}]}),
                    json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":usage}),
                ])
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (port, bodies)
}

async fn setup(port: u16) -> Harness {
    // SAFETY: tests only ever set this to the same value.
    unsafe { std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1") };
    let root = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    let cwd = root_path.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let config = json!({
        "model": "mock/m1",
        "plugins": [fixture("v22-plugin")],
        "providers": {"mock": {
            "name": "Mock",
            "package": "@opencode/ai/providers/openai-compatible",
            "settings": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "test"},
            "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}}}
        }}
    });
    std::fs::write(cwd.join("opencode.json"), config.to_string()).unwrap();
    // `experimental.generate.text` uses the base (global) configuration.
    std::fs::create_dir_all(root_path.join("config")).unwrap();
    let mut global_config = config.clone();
    global_config.as_object_mut().unwrap().remove("plugins");
    std::fs::write(root_path.join("config").join("opencode.json"), global_config.to_string()).unwrap();
    let db = oc_storage::Database::memory().unwrap();
    let app = oc_core::app::App::new(global(&root_path), db, "1.2.3");
    let services = oc_core::services::default_services(&app);
    let runner = oc_core::runner::Engine::new(app.clone(), &services);
    let opts = oc_server::ServerOptions { cwd: Some(cwd.display().to_string()), version: "1.2.3".into(), ready: true, ..Default::default() };
    let (router, _) = oc_backend::router(app.clone(), Some(runner), &opts).unwrap();
    let engine = oc_backend::Engine::new(app.clone());
    let h = Harness { app, engine, router, root, cwd };
    let r = h.get("/api/location").await;
    assert_eq!(r.status, 200);
    h.app.plugins.await_activation(&h.dir()).await;
    h
}

async fn rpc(h: &Harness, method: &str, input: Value) -> Value {
    let r = h.post(&format!("/api/rpc/fixture.v22/{method}"), json!({ "input": input })).await;
    r.json()
}

#[tokio::test]
async fn plugins_put_list_and_remove_session_instruction_entries() {
    require_bun!();
    let (port, bodies) = mock("done").await;
    let h = setup(port).await;
    let session = h.session().await;

    let put = rpc(&h, "instructions", json!({ "op": "put", "sessionID": session, "key": "team.guidelines", "value": "Prefer small, focused changes." })).await;
    assert_eq!(put, json!({ "output": { "done": true } }));
    let list = rpc(&h, "instructions", json!({ "op": "list", "sessionID": session })).await;
    assert_eq!(list, json!({ "output": { "entries": [{ "key": "team.guidelines", "value": "Prefer small, focused changes." }] } }));
    // The same entries the HTTP API lists.
    let r = h.get(&format!("/api/experimental/session/{session}/instructions/entries")).await;
    assert_eq!(r.json()["data"], json!([{ "key": "team.guidelines", "value": "Prefer small, focused changes." }]));

    // A missing session fails with `Session.NotFoundError` for every operation.
    let missing = "ses_missing0000000000000000";
    for input in [
        json!({ "op": "list", "sessionID": missing }),
        json!({ "op": "put", "sessionID": missing, "key": "deploy-target", "value": "production" }),
        json!({ "op": "remove", "sessionID": missing, "key": "deploy-target" }),
    ] {
        let out = rpc(&h, "instructions", input).await;
        assert_eq!(out["type"], "failed", "{out}");
        assert_eq!(out["data"]["name"], "Session.NotFoundError", "{out}");
    }
    // Invalid keys are rejected by the endpoint schema before anything is stored.
    for op in ["put", "remove"] {
        let out = rpc(&h, "instructions", json!({ "op": op, "sessionID": session, "key": "invalid/key", "value": "rejected" })).await;
        assert_eq!(out["data"]["name"], "SchemaError", "{out}");
    }
    let out = rpc(&h, "instructions", json!({ "op": "put", "sessionID": session, "key": "oversized", "value": "x".repeat(256 * 1024) })).await;
    assert_eq!(out["data"]["name"], "InstructionEntryValueTooLargeError", "{out}");

    // The next model request carries the entry in its initial instructions.
    let r = h.post(&format!("/api/session/{session}/prompt"), json!({ "text": "hello" })).await;
    assert_eq!(r.status, 200);
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::timeout(Duration::from_secs(60), h.app.coordinator().await_idle(&session)).await.unwrap();
    let first = bodies.lock()[0].to_string();
    assert!(first.contains("<context key=\\\"team.guidelines\\\">\\nPrefer small, focused changes.\\n</context>"), "{first}");

    let removed = rpc(&h, "instructions", json!({ "op": "remove", "sessionID": session, "key": "team.guidelines" })).await;
    assert_eq!(removed, json!({ "output": { "done": true } }));
    let list = rpc(&h, "instructions", json!({ "op": "list", "sessionID": session })).await;
    assert_eq!(list, json!({ "output": { "entries": [] } }));
    // No inbox item was admitted by any of this.
    let r = h.get(&format!("/api/session/{session}/inbox")).await;
    assert_eq!(r.json()["data"], json!([]), "{:?}", r.json());
    h.app.plugins.shutdown().await;
}

#[tokio::test]
async fn plugins_generate_text_through_the_experimental_api() {
    require_bun!();
    let (port, bodies) = mock("generated text").await;
    let h = setup(port).await;
    for kind in ["text", "legacy"] {
        let out = rpc(&h, "generate", json!({ "kind": kind, "prompt": "say something" })).await;
        assert_eq!(out, json!({ "output": { "text": "generated text" } }), "{kind}");
    }
    assert_eq!(bodies.lock().len(), 2);
    // Image and decision generation fail like upstream's `Generate` service for a chat model: an
    // `UnsupportedModelError` with the same message as the HTTP endpoints.
    let image = rpc(&h, "generate", json!({ "kind": "image" })).await;
    let http_image = h.post("/api/experimental/generate/image", json!({ "prompt": "a cat", "model": { "providerID": "mock", "id": "m1" } })).await.json();
    assert_eq!(http_image["_tag"], "UnsupportedModelError", "{http_image}");
    assert_eq!(image["message"], "mock/m1 is a chat model, not a image model", "{image}");
    assert_eq!(image["data"]["name"], "UnsupportedModelError", "{image}");
    assert_eq!(image["message"], http_image["message"]);
    let decisions = rpc(&h, "generate", json!({ "kind": "decisions" })).await;
    assert_eq!(decisions["message"], "mock/m1 is a chat model, not a decision model", "{decisions}");
    assert_eq!(decisions["data"]["name"], "UnsupportedModelError", "{decisions}");
    h.app.plugins.shutdown().await;
}

/// The `experimental.idle` session hook (`session/runner/llm.ts`): it runs once a session that ran the model would go
/// idle with nothing pending, with the background work of the session and its descendants; control-only drains such
/// as a manual compaction skip it, and a failing hook fails the execution.
#[tokio::test]
async fn plugins_idle_hook_runs_when_the_session_would_go_idle() {
    require_bun!();
    let (port, bodies) = mock("done").await;
    let h = setup(port).await;
    let key = |session: &str| format!("{}idle:{session}", oc_plugin::storage::namespace("fixture.v22"));
    let wait = |session: String| {
        let app = h.app.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            tokio::time::timeout(Duration::from_secs(60), app.coordinator().await_idle(&session)).await.unwrap();
        }
    };

    let session = h.session().await;
    let r = h.post(&format!("/api/session/{session}/prompt"), json!({ "text": "hello" })).await;
    assert_eq!(r.status, 200);
    wait(session.clone()).await;
    assert_eq!(bodies.lock().len(), 1);
    assert_eq!(h.app.db.kv_get(&key(&session)).unwrap(), Some(json!({ "sessionID": session, "background": [] })));

    // A manual compaction of the idle session never runs the model, so it skips the hook.
    h.app.db.kv_remove(&key(&session)).unwrap();
    let r = h.post(&format!("/api/session/{session}/compact"), json!({})).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    wait(session.clone()).await;
    assert_eq!(h.app.db.kv_get(&key(&session)).unwrap(), None);

    // A failing hook fails the execution.
    let r = h.post("/api/session", json!({ "id": "ses_fail" })).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let r = h.post("/api/session/ses_fail/prompt", json!({ "text": "hello" })).await;
    assert_eq!(r.status, 200);
    wait("ses_fail".into()).await;
    let info = h.get("/api/session/ses_fail").await.json();
    assert_eq!(info["data"]["outcome"], "failed", "{info}");
    h.app.plugins.shutdown().await;
}
