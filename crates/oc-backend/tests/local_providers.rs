//! The local model discovery plugins in a server: a location's boot starts them, a discovered inventory shows in the
//! provider, model and integration lists, and each change announces `integration.updated` only when the integration
//! list changed (LM Studio loses its models.dev integration, vLLM has none) before `provider.updated` and
//! `model.updated`. (An inventory found before the boot announced itself is part of the boot's notifications.) The
//! mocks listen on ephemeral ports; the default origins are never asked.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{StatusCode, Uri};
use axum::response::IntoResponse;
use common::*;
use oc_ai::catalog::local::Kind;
use parking_lot::Mutex;
use serde_json::{json, Value};

async fn mock(answer: impl Fn(&str) -> Option<Value> + Send + Sync + 'static) -> (String, Arc<Mutex<Vec<String>>>) {
    let answer = Arc::new(answer);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    let app = axum::Router::new().fallback(move |uri: Uri| {
        let (answer, recorded) = (answer.clone(), recorded.clone());
        async move {
            recorded.lock().push(uri.path().to_string());
            // Slower than the location's boot, so the inventories arrive after it announced itself.
            tokio::time::sleep(Duration::from_millis(600)).await;
            match answer(uri.path()) {
                Some(body) => axum::Json(body).into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (origin, seen)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discovered_providers_are_listed_and_announced() {
    let h = harness();
    let (lmstudio, _) = mock(|path| {
        (path == "/api/v1/models").then(|| {
            json!({"models": [{"type": "llm", "key": "qwen/coder", "display_name": "Qwen Coder", "architecture": "qwen3",
                "loaded_instances": [], "max_context_length": 65536, "capabilities": {"vision": false, "trained_for_tool_use": true}}]})
        })
    })
    .await;
    let (vllm, vllm_seen) = mock(|path| match path {
        "/health" => Some(json!({})),
        "/v1/models" => Some(json!({"object": "list", "data": [{"id": "served", "owned_by": "vllm", "max_model_len": 8192}]})),
        _ => None,
    })
    .await;
    let local = &h.app.plugins.local;
    local.set_default_origin(Kind::LmStudio, Some(lmstudio.clone()));
    local.set_default_origin(Kind::Ollama, None);
    local.set_default_origin(Kind::Vllm, Some(vllm.clone()));
    local.enable();

    let mut rx = h.app.bus.subscribe();
    assert_eq!(h.get("/api/provider").await.status, 200);
    // Both inventories arrive (each one reload).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut models = Value::Null;
    while tokio::time::Instant::now() < deadline {
        models = h.get("/api/model").await.json()["data"].clone();
        let ids: Vec<&str> = models.as_array().unwrap().iter().filter_map(|m| m["id"].as_str()).collect();
        if ids.contains(&"qwen/coder") && ids.contains(&"served") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    let mut kinds = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if e.kind.ends_with(".updated") && e.location.as_ref().is_some_and(|l| l.directory == h.dir()) {
            kinds.push(e.kind.clone());
        }
    }
    // The boot burst ends with `plugin.updated`; after it, only the two reloads.
    let boot = kinds.iter().position(|k| k == "plugin.updated").expect("boot burst");
    let after: Vec<&str> = kinds[boot + 1..].iter().map(String::as_str).collect();
    let mut counted = after.clone();
    counted.sort();
    assert_eq!(counted, ["integration.updated", "model.updated", "model.updated", "provider.updated", "provider.updated"], "{after:?}");

    let model = |id: &str| models.as_array().unwrap().iter().find(|m| m["id"] == json!(id)).cloned().unwrap();
    let qwen = model("qwen/coder");
    assert_eq!((qwen["providerID"].clone(), qwen["name"].clone(), qwen["family"].clone()), (json!("lmstudio"), json!("Qwen Coder"), json!("qwen3")));
    assert_eq!(qwen["limit"], json!({"context": 65536, "output": 32000}));
    assert_eq!(model("served")["providerID"], json!("vllm"));

    let providers = h.get("/api/provider").await.json()["data"].clone();
    let provider = |id: &str| providers.as_array().unwrap().iter().find(|p| p["id"] == json!(id)).cloned().unwrap();
    assert_eq!(provider("lmstudio")["name"], "LM Studio");
    assert_eq!(provider("lmstudio")["settings"], json!({"baseURL": format!("{lmstudio}/v1"), "provider": "lmstudio", "apiKey": ""}));
    assert!(provider("lmstudio").get("integrationID").is_none());
    assert_eq!(provider("vllm")["settings"]["baseURL"], json!(format!("{vllm}/v1")));
    let integrations = h.get("/api/integration").await.json()["data"].clone();
    assert!(!integrations.as_array().unwrap().iter().any(|i| i["id"] == json!("lmstudio")), "the discovered provider needs no integration");
    assert_eq!(vllm_seen.lock().first().map(String::as_str), Some("/health"));
}
