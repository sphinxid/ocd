//! The LM Studio discovery plugin with the integration's env variable set (`provider-lmstudio.test.ts`, #63266). In
//! its own test binary: the variable is process-wide.

use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use oc_ai::catalog::local::Kind;
use oc_core::models::Models;
use oc_core::plugins::Plugins;
use parking_lot::Mutex;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lmstudio_keeps_sending_the_integration_env_key_after_models_load() {
    // SAFETY: the only test of this binary.
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var(oc_core::local_providers::ORIGIN_OVERRIDE, "off");
        std::env::set_var("LMSTUDIO_API_KEY", "env-key");
    }
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let recorded = seen.clone();
    let app = axum::Router::new().fallback(move |headers: HeaderMap| {
        let recorded = recorded.clone();
        async move {
            let authorization = headers.get("authorization").and_then(|v| v.to_str().ok()).map(String::from);
            recorded.lock().push(authorization.clone());
            if authorization.as_deref() != Some("Bearer env-key") {
                return (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": {"code": "invalid_api_key"}}))).into_response();
            }
            axum::Json(json!({"models": [{"type": "llm", "key": "secured-model", "display_name": "Secured", "loaded_instances": [], "max_context_length": 8192}]}))
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let home = tempfile::tempdir().unwrap();
    let root = home.path();
    let global = Arc::new(oc_util::Global {
        home: root.to_path_buf(),
        data: root.join("data"),
        cache: root.join("cache"),
        config: root.join("config"),
        state: root.join("state"),
        tmp: root.join("tmp"),
        bin: root.join("cache/bin"),
        log: root.join("data/log"),
        repos: root.join("data/repos"),
    });
    let db = oc_storage::Database::memory().unwrap();
    let bus = oc_core::bus::Bus::new(db.clone());
    let models = Arc::new(Models::new(db.clone(), bus.clone(), "latest", "1.2.3"));
    let plugins = Plugins::new(global, db, bus, "1.2.3");
    models.set_plugins(plugins.clone());
    for kind in Kind::ALL {
        plugins.local.set_default_origin(kind, None);
    }
    plugins.local.set_default_origin(Kind::LmStudio, Some(origin));
    plugins.local.set_interval(Duration::from_millis(5));
    plugins.local.enable();
    // Discovery starts before the location's first catalog, so the env variable comes from models.dev.
    plugins.local.observe("/loc", &[]);
    plugins.local.start("/loc");

    // Generous for loaded debug runs (the other discovery tests wait up to 15 s).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let catalog = models.catalog_at(&[], Some("/loc")).await.unwrap();
        if catalog.model("lmstudio", "secured-model").is_some() && seen.lock().len() >= 3 {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "not discovered: {:?}", seen.lock());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(seen.lock().iter().all(|a| a.as_deref() == Some("Bearer env-key")), "{:?}", seen.lock());
    let integrations = models.integrations_for(Some("/loc"));
    assert!(integrations.get("lmstudio").await.unwrap().is_some(), "the integration stays");
    let catalog = models.catalog_at(&[], Some("/loc")).await.unwrap();
    assert_eq!(catalog.provider("lmstudio").unwrap().integration_id.as_deref(), Some("lmstudio"));
    assert!(catalog.is_available("lmstudio"));
    server.abort();
}
