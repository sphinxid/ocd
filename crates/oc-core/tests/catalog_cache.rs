//! A location's model catalog is built once and shared by every request until one of its inputs
//! changes (upstream keeps the `Provider`/`Model` states and rebuilds them on invalidation).
//! Rebuilding it per request made every `provider.list`/`model.list` call allocate a full copy of
//! the models.dev catalog, and concurrent requests multiplied that into gigabytes of resident memory.

use std::sync::Arc;

use oc_config::schema::{Entry, Info};
use serde_json::json;

fn entries(name: &str) -> Vec<Entry> {
    let info = Info::from_value(&json!({
        "providers": {"mock": {
            "name": name,
            "package": "@opencode/ai/providers/openai-compatible",
            "settings": {"baseURL": "http://127.0.0.1:9/v1"},
            "models": {"m1": {"name": "Mock One"}}
        }}
    }))
    .expect("config document");
    vec![Entry::document(None, info)]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_is_shared_until_an_input_changes() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: this test binary runs only this test.
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_TEST_HOME", home.path());
    }
    let db = oc_storage::Database::memory().unwrap();
    let models = Arc::new(oc_core::models::Models::new(db.clone(), oc_core::bus::Bus::new(db), "latest", "1.2.3"));
    let config = entries("Mock");

    let first = models.catalog_at(&config, Some("/loc")).await.unwrap();
    assert_eq!(first.provider("mock").map(|p| p.name.as_str()), Some("Mock"));
    let again = models.catalog_at(&config, Some("/loc")).await.unwrap();
    assert!(first.ptr_eq(&again), "unchanged inputs reuse the built catalog");

    // Concurrent first requests of another location share a single build.
    let builds: Vec<_> = (0..8)
        .map(|_| {
            let (models, config) = (models.clone(), config.clone());
            tokio::spawn(async move { models.catalog_at(&config, Some("/other")).await.unwrap() })
        })
        .collect();
    let mut catalogs = Vec::new();
    for build in builds {
        catalogs.push(build.await.unwrap());
    }
    assert!(catalogs.iter().all(|c| c.ptr_eq(&catalogs[0])), "concurrent requests wait for the build in flight");
    assert!(!catalogs[0].ptr_eq(&first), "locations keep their own catalog");

    // A configuration change rebuilds it.
    let renamed = models.catalog_at(&entries("Renamed"), Some("/loc")).await.unwrap();
    assert!(!renamed.ptr_eq(&first));
    assert_eq!(renamed.provider("mock").map(|p| p.name.as_str()), Some("Renamed"));

    // So does a new connection (provider availability depends on it).
    let renamed_config = entries("Renamed");
    models.integrations_for(Some("/loc")).connect_key("mock", "secret", None, None).await.unwrap();
    let connected = models.catalog_at(&renamed_config, Some("/loc")).await.unwrap();
    assert!(!connected.ptr_eq(&renamed), "a credential change rebuilds the catalog");
    let same = models.catalog_at(&renamed_config, Some("/loc")).await.unwrap();
    assert!(same.ptr_eq(&connected));

    // A closed location forgets its catalog.
    models.close_location("/loc");
    let reopened = models.catalog_at(&renamed_config, Some("/loc")).await.unwrap();
    assert!(!reopened.ptr_eq(&connected));
}
