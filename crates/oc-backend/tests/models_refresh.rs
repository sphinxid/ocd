//! The models.dev background refresh (upstream `ModelsDev.layer` and the models.dev plugin): a
//! refreshed catalog publishes `models-dev.refreshed` server-wide, every loaded location reloads its
//! integrations and providers, and the next catalog read is rebuilt from the new data. The catalog
//! comes from a local server (`OPENCODE_MODELS_URL`), never from models.dev.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use serde_json::{Value, json};

fn catalog() -> Value {
    json!({
        "acme": {
            "id": "acme", "name": "Acme", "env": ["ACME_API_KEY"], "npm": "@ai-sdk/openai-compatible",
            "api": "https://acme.invalid/v1",
            "models": {
                "acme-1": {
                    "id": "acme-1", "name": "Acme One", "release_date": "2026-01-01", "temperature": true,
                    "tool_call": true, "modalities": {"input": ["text"], "output": ["text"]},
                    "limit": {"context": 128000, "output": 8192}
                }
            }
        }
    })
}

#[tokio::test]
async fn refresh_publishes_globally_and_rebuilds_catalogs() {
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = hits.clone();
    let app = Router::new().route(
        "/api.json",
        get(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { axum::Json(catalog()) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // SAFETY: this test binary has a single test, set up before any thread reads the environment.
    unsafe {
        std::env::remove_var("OPENCODE_DISABLE_MODELS_FETCH");
        std::env::set_var("OPENCODE_MODELS_URL", &url);
    }

    let root = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    let cwd = root_path.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let g = |p: &str| {
        let d = root_path.join(p);
        std::fs::create_dir_all(&d).unwrap();
        d
    };
    let global = oc_util::Global {
        home: g("home"),
        data: g("data"),
        cache: g("cache"),
        config: g("config"),
        state: g("state"),
        tmp: g("tmp"),
        bin: g("bin"),
        log: g("log"),
        repos: g("repos"),
    };
    let app = oc_core::app::App::new(global, oc_storage::Database::memory().unwrap(), "1.2.3");
    let engine = oc_backend::Engine::new(app.clone());
    let directory = cwd.display().to_string();
    let mut events = app.bus.subscribe();
    // Started at server boot; the loop itself starts when the catalog is first needed.
    oc_backend::start_models_refresh(&engine);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    engine.locations.get(&directory).await.unwrap();
    engine.models().catalog(&[]).await.unwrap();
    let mut seen = vec![];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while seen.len() < 4 {
        let event = tokio::time::timeout_at(deadline, events.recv()).await.expect("refresh events").unwrap();
        // The location's boot notifications come first; collect from the refresh on.
        let started = !seen.is_empty() || event.kind == "models-dev.refreshed";
        if started && ["models-dev.refreshed", "integration.updated", "provider.updated", "model.updated"].contains(&event.kind.as_str()) {
            seen.push((event.kind.clone(), event.location.as_ref().map(|l| l.directory.clone())));
        }
    }
    assert_eq!(
        seen,
        vec![
            ("models-dev.refreshed".to_string(), None),
            ("integration.updated".to_string(), Some(directory.clone())),
            ("provider.updated".to_string(), Some(directory.clone())),
            ("model.updated".to_string(), Some(directory.clone())),
        ]
    );
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    let catalog = engine.models().catalog(&[]).await.unwrap();
    assert_eq!(catalog.provider("acme").map(|p| p.name.as_str()), Some("Acme"));
    // The refreshed body is cached, so the next process starts from it.
    let cached = app.db.kv_get(&format!("models-dev:catalog:{}", oc_util::hash::fast(&url))).unwrap().unwrap();
    assert_eq!(serde_json::from_str::<Value>(cached["body"].as_str().unwrap()).unwrap(), catalog_json());
}

fn catalog_json() -> Value {
    catalog()
}
