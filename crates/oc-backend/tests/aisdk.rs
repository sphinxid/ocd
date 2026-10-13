//! AI SDK provider packages end to end: a configured provider whose package is the fixture AI SDK
//! provider (`oc-plugin/tests/fixtures/aisdk-provider`, LanguageModelV3, loaded from a `file://`
//! URL) runs through the AI SDK bridge in the Bun sidecar; the session runner streams text,
//! reasoning and tool calls from it, with usage, plugin `aisdk.*` and `session.http.*` hooks.
//!
//! Bun: `OCD_BUN_PATH`, else `OCD_TEST_BUN`, else `$OCD_SCRATCH/bun142/bun-linux-x64/bun`; the tests
//! are skipped (with a message) when none exists. No network.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::http::HeaderMap;
use axum::{Router, body::Body, response::Response, routing::post};
use common::*;
use parking_lot::Mutex;
use serde_json::{Value, json};

fn bun() -> Option<PathBuf> {
    static BUN: OnceLock<Option<PathBuf>> = OnceLock::new();
    BUN.get_or_init(|| {
        let path = std::env::var_os("OCD_BUN_PATH")
            .or_else(|| std::env::var_os("OCD_TEST_BUN"))
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("OCD_SCRATCH").map(|d| PathBuf::from(d).join("bun142/bun-linux-x64/bun")).filter(|p| p.exists()))?;
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

#[derive(Clone)]
enum Turn {
    Tool(&'static str, Value),
    Text { reasoning: Option<&'static str>, text: &'static str },
    Status(u16),
}

struct Mock {
    port: u16,
    requests: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
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
    let requests = Arc::new(Mutex::new(Vec::<(HeaderMap, Value)>::new()));
    let script = Arc::new(Mutex::new(turns));
    let recorded = requests.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, body: axum::Json<Value>| {
            let is_title = body.0.to_string().contains("You are a title generator");
            let turn = if is_title {
                None
            } else {
                recorded.lock().push((headers, body.0.clone()));
                let mut script = script.lock();
                Some(if script.is_empty() { Turn::Text { reasoning: None, text: "done" } } else { script.remove(0) })
            };
            async move {
                let usage = json!({"prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19,
                                   "prompt_tokens_details": {"cached_tokens": 2}, "completion_tokens_details": {"reasoning_tokens": 3}});
                match turn {
                    None => sse(&[
                        json!({"id":"t","choices":[{"index":0,"delta":{"role":"assistant","content":"Bridge test"}}]}),
                        json!({"id":"t","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":usage}),
                    ]),
                    Some(Turn::Tool(name, args)) => sse(&[
                        json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]}),
                        json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":usage}),
                    ]),
                    Some(Turn::Text { reasoning, text }) => {
                        let mut chunks = vec![];
                        if let Some(r) = reasoning {
                            chunks.push(json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":r}}]}));
                        }
                        chunks.push(json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","content":text}}]}));
                        chunks.push(json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":usage}));
                        sse(&chunks)
                    }
                    Some(Turn::Status(status)) => Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(Body::from(json!({"error": {"message": "bad request from mock", "type": "invalid_request_error"}}).to_string()))
                        .unwrap(),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Mock { port, requests }
}

struct Setup {
    h: Harness,
    mock: Mock,
}

impl Setup {
    async fn run(&self, session: &str, text: &str) {
        let r = self.h.post(&format!("/api/session/{session}/prompt"), json!({ "text": text })).await;
        assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::timeout(Duration::from_secs(60), self.h.app.coordinator().await_idle(session)).await.unwrap();
    }

    fn messages(&self, session: &str) -> Vec<Value> {
        self.h.app.db.session_messages(session).unwrap().into_iter().map(|(_, m)| serde_json::to_value(m).unwrap()).collect()
    }

    fn content(&self, session: &str) -> Vec<Value> {
        self.messages(session).iter().flat_map(|m| m["content"].as_array().cloned().unwrap_or_default()).collect()
    }
}

async fn setup(plugins: Value, model: &str, turns: Vec<Turn>) -> Setup {
    let mock = mock(turns).await;
    // SAFETY: tests only ever set this to the same value.
    unsafe { std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1") };
    let root = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    let cwd = root_path.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("hello.txt"), "hello from the file\n").unwrap();
    let provider = format!("aisdk:file://{}/index.js", fixture("aisdk-provider"));
    std::fs::write(
        cwd.join("opencode.json"),
        json!({
            "model": model,
            "plugins": plugins,
            "providers": {
                "fixture": {
                    "name": "Fixture",
                    "package": provider,
                    "settings": {"baseURL": format!("http://127.0.0.1:{}/v1", mock.port), "apiKey": "test", "marker": "configured"},
                    "models": {"m1": {"name": "Fixture One", "limit": {"context": 100000, "output": 4096}}}
                },
                "broken": {
                    "name": "Broken",
                    "package": "aisdk:file:///nonexistent/ocd-aisdk-missing.js",
                    "models": {"m1": {"name": "Broken One", "limit": {"context": 100000, "output": 4096}}}
                }
            }
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
    let r = h.get("/api/location").await;
    assert_eq!(r.status, 200);
    h.app.plugins.await_activation(&h.dir()).await;
    Setup { h, mock }
}

#[tokio::test]
async fn text_reasoning_tools_and_usage_through_the_bridge() {
    require_bun!();
    let s = setup(
        json!([]),
        "fixture/m1",
        vec![
            Turn::Tool("read", json!({ "path": "hello.txt" })),
            Turn::Text { reasoning: Some("The file greets."), text: "The file says hello." },
        ],
    )
    .await;
    // No plugins configured: the sidecar is not running before the first model request.
    assert!(!s.h.app.plugins.host().is_some_and(|h| h.is_running()));
    let session = s.h.session().await;
    s.run(&session, "read hello.txt").await;
    assert!(s.h.app.plugins.host().is_some_and(|h| h.is_running()));

    let content = s.content(&session);
    let tool = content.iter().find(|c| c["type"] == "tool").expect("tool call");
    assert_eq!(tool["name"], "read");
    assert_eq!(tool["state"]["status"], "completed", "{tool}");
    let reasoning = content.iter().find(|c| c["type"] == "reasoning").expect("reasoning");
    assert_eq!(reasoning["text"], "The file greets.");
    let text = content.iter().filter(|c| c["type"] == "text").last().expect("text");
    assert_eq!(text["text"], "The file says hello.");

    // Usage from the V3 finish part (12 in incl. 2 cached, 7 out incl. 3 reasoning).
    let messages = s.messages(&session);
    let last = messages.iter().filter(|m| m["type"] == "assistant").last().expect("assistant");
    assert_eq!(last["tokens"]["input"], 10, "{last}");
    assert_eq!(last["tokens"]["output"], 4, "{last}");
    assert_eq!(last["tokens"]["reasoning"], 3, "{last}");
    assert_eq!(last["tokens"]["cache"]["read"], 2, "{last}");
    assert_eq!(last["finish"], "stop", "{last}");

    // What the provider received: the V3 prompt (system, user, assistant tool call, tool result),
    // the tools, provider options under the provider key, the session headers.
    let requests = s.mock.requests.lock().clone();
    assert_eq!(requests.len(), 2);
    let (headers, first) = &requests[0];
    assert_eq!(first["model"], "m1");
    assert_eq!(first["messages"][0]["role"], "system");
    assert!(first["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "read"));
    assert_eq!(first["fixture"]["providerOptions"]["fixture"]["marker"], "configured");
    assert_eq!(first["fixture"]["sdk"], json!({ "name": "fixture", "marker": "configured", "fetch": "function" }));
    assert_eq!(headers.get("authorization").unwrap(), "Bearer test");
    assert_eq!(headers.get("x-opencode-session-id").map(|v| v.to_str().unwrap().to_string()), Some(session.clone()));
    let (_, second) = &requests[1];
    let messages = second["messages"].as_array().unwrap();
    let call = messages.iter().find(|m| m["role"] == "assistant").unwrap();
    assert_eq!(call["content"][0]["type"], "tool-call");
    assert_eq!(call["content"][0]["input"], json!({ "path": "hello.txt" }));
    let result = messages.iter().find(|m| m["role"] == "tool").unwrap();
    assert_eq!(result["content"][0]["type"], "tool-result");
    assert!(result["content"][0]["output"].to_string().contains("hello from the file"), "{result}");
    s.h.app.plugins.shutdown().await;
}

#[tokio::test]
async fn plugin_aisdk_and_http_hooks() {
    require_bun!();
    let s = setup(
        json!([fixture("aisdk-plugin"), fixture("http-plugin")]),
        "fixture/m1",
        vec![Turn::Text { reasoning: None, text: "ORIGINAL answer" }],
    )
    .await;
    let session = s.h.session().await;
    s.run(&session, "hi").await;
    let requests = s.mock.requests.lock().clone();
    let (headers, body) = &requests[0];
    // aisdk.sdk renamed the API model, aisdk.language added a header naming what the sdk hook saw.
    assert_eq!(body["model"], "m1-wrapped");
    let seen = headers.get("x-fixture-aisdk").unwrap().to_str().unwrap().to_string();
    assert_eq!(seen, format!("file://{}/index.js|fixture/m1|function|fixture", fixture("aisdk-provider")));
    // session.http.request rewrote the body and saw the SDK's request; session.http.response the answer.
    assert_eq!(body["user"], "fixture-user");
    let hook = headers.get("x-fixture-seen").unwrap().to_str().unwrap().to_string();
    assert!(hook.starts_with("primary|build|fixture/undefined|POST|/v1/chat/completions|"), "{hook}");
    let text = s.content(&session).into_iter().filter(|c| c["type"] == "text").last().expect("text");
    assert!(text["text"].as_str().unwrap().starts_with("HOOKED 200"), "{text}");
    s.h.app.plugins.shutdown().await;
}

#[tokio::test]
async fn provider_errors_and_initialization_failures() {
    require_bun!();
    let s = setup(json!([]), "fixture/m1", vec![Turn::Status(400)]).await;
    let session = s.h.session().await;
    s.run(&session, "hi").await;
    let messages = s.messages(&session);
    let failed = messages.iter().filter(|m| m["type"] == "assistant").last().expect("assistant");
    assert_eq!(failed["error"]["message"], "bad request from mock", "{failed}");

    // A package that cannot be loaded fails model initialization.
    let r = s.h.post(&format!("/api/session/{session}/model"), json!({ "model": { "providerID": "broken", "id": "m1" } })).await;
    assert!(r.status.is_success(), "{:?}", String::from_utf8_lossy(&r.body));
    s.run(&session, "again").await;
    let all = serde_json::to_string(&s.messages(&session)).unwrap();
    assert!(all.contains("Cannot initialize broken/m1"), "{all}");
    s.h.app.plugins.shutdown().await;
}
