//! Regression guard: with the plugin runtime attached (the built-in GitHub Copilot plugin enabled)
//! and no Copilot connection, a location's catalog is built once and reused by later requests.
//! The Copilot plugin's initial `/models` load finds nothing to load, so it must neither rebuild the
//! catalog nor invalidate the cached one.

use std::sync::Arc;

use oc_config::schema::{Entry, Info};
use serde_json::json;

fn entries() -> Vec<Entry> {
    let info = Info::from_value(&json!({
        "model": "mock/m1",
        "providers": {"mock": {
            "name": "Mock",
            "package": "@opencode/ai/providers/openai-compatible",
            "settings": {"baseURL": "http://127.0.0.1:9/v1", "apiKey": "test"},
            "models": {"m1": {"name": "Mock One"}}
        }}
    }))
    .expect("config document");
    vec![Entry::document(None, info)]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_is_built_once_per_location_without_copilot() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: this test binary runs only this test.
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_TEST_HOME", home.path());
        for name in ["GITHUB_TOKEN", "OPENCODE_API_KEY"] {
            std::env::remove_var(name);
        }
    }
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
    let models = Arc::new(oc_core::models::Models::new(db.clone(), bus.clone(), "latest", "1.2.3"));
    let plugins = oc_core::plugins::Plugins::new(global, db, bus, "1.2.3");
    assert!(plugins.copilot_enabled("/loc"), "the built-in Copilot plugin runs by default");
    models.set_plugins(plugins);
    let config = entries();

    let first = models.catalog_at(&config, Some("/loc")).await.unwrap();
    assert!(first.model("mock", "m1").is_some());
    // The Copilot models route through the AI SDK provider when nothing was loaded.
    let copilot = first.record("github-copilot").expect("models.dev lists GitHub Copilot");
    assert!(!copilot.models.is_empty());
    assert!(copilot.models.values().all(|m| m.package.as_deref() == Some("aisdk:@ai-sdk/github-copilot")));
    assert_eq!(models.catalog_builds(Some("/loc")), 1, "the Copilot initial load changes nothing, so no rebuild");
    for _ in 0..5 {
        let again = models.catalog_at(&config, Some("/loc")).await.unwrap();
        assert!(again.ptr_eq(&first), "repeated requests reuse the cached catalog");
    }
    // Concurrent requests share it as well.
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let (models, config) = (models.clone(), config.clone());
            tokio::spawn(async move { models.catalog_at(&config, Some("/loc")).await.unwrap() })
        })
        .collect();
    for task in tasks {
        assert!(task.await.unwrap().ptr_eq(&first));
    }
    assert_eq!(models.catalog_builds(Some("/loc")), 1);
}
