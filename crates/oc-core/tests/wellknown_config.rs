//! Well-known organization config (`config.ts` `loadWellknownEntry`): a registered source's inline and remote
//! configs load as documents tagged with its origin, resolved with the latest stored key credential; without a
//! usable credential (or when resolution fails) the last resolved configs stay.

use std::sync::Arc;

use axum::{Json, Router, extract::Query, routing::get};
use oc_ai::credentials::kv::{KvStore, MemoryKv};
use oc_ai::credentials::store::{CreateCredential, MemoryCredentialStore};
use oc_ai::credentials::wellknown::{SOURCES_KEY, WellKnown};
use oc_ai::credentials::{CredentialStore, CredentialValue};
use oc_config::schema::Entry;
use oc_core::wellknown_config::ConfigWellKnown;
use serde_json::{Value, json};

#[tokio::test]
async fn wellknown_sources_contribute_tagged_documents() {
    let base = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let remote = format!("{base}/config?token={{env:CORP_TOKEN}}");
        let app = Router::new()
            .route(
                "/.well-known/opencode",
                get(move || {
                    let remote = remote.clone();
                    async move {
                        Json(json!({
                            "auth": {"command": ["corp", "login"], "env": "CORP_TOKEN"},
                            "config": {"username": "{env:CORP_TOKEN}"},
                            "remote_config": {"url": remote},
                        }))
                    }
                }),
            )
            .route(
                "/config",
                get(|Query(query): Query<std::collections::HashMap<String, String>>| async move {
                    if query.get("token").map(String::as_str) != Some("tok-1") {
                        return (axum::http::StatusCode::UNAUTHORIZED, Json(json!({})));
                    }
                    (axum::http::StatusCode::OK, Json(json!({"config": {"model": "corp/fast"}})))
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    };
    let kv = Arc::new(MemoryKv::new());
    kv.set(SOURCES_KEY, json!([base])).await.unwrap();
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let service = Arc::new(WellKnown::new(http, kv.clone(), oc_ai::credentials::map_env::<[(&str, &str); 0], &str, &str>([])));
    let store = MemoryCredentialStore::shared();
    let config = Arc::new(ConfigWellKnown { service: service.clone(), store: store.clone() });

    let dir = tempfile::tempdir().unwrap();
    let mut global = oc_util::Global::resolve();
    global.config = dir.path().join("config");
    global.home = dir.path().join("home");
    let loader = oc_config::load::Loader::new(&global, dir.path().join("project"), oc_config::discovery::Options::default())
        .with_wellknown(config.clone());

    // No credential yet: nothing resolved, nothing cached.
    let loaded = loader.load_wellknown().await.unwrap();
    assert_eq!(loaded, vec![(base.clone(), vec![])]);

    // The latest stored key resolves the inline config (with `{env:}` from the credential) and the remote one.
    store.create(CreateCredential::new(base.clone(), CredentialValue::Key { key: "tok-0".into(), metadata: None })).await.unwrap();
    store.create(CreateCredential::new(base.clone(), CredentialValue::Key { key: "tok-1".into(), metadata: None })).await.unwrap();
    let loaded = loader.load_wellknown().await.unwrap();
    let documents = &loaded[0].1;
    assert_eq!(documents.len(), 2);
    let infos: Vec<Value> = documents
        .iter()
        .map(|d| match d {
            Entry::Document { info, wellknown, integration, path } => {
                assert_eq!(wellknown.as_deref(), Some(base.as_str()));
                assert!(integration.is_none() && path.is_none());
                serde_json::to_value(info).unwrap()
            }
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(infos[0]["username"], "tok-1");
    assert_eq!(infos[1]["model"], json!({"providerID": "corp", "model": "fast"}));

    // The documents load between the integration documents and the local files.
    let (_, entries) = loader.entries().await.unwrap();
    assert!(entries.iter().any(|e| matches!(e, Entry::Document { wellknown: Some(origin), .. } if *origin == base)));

    // A failed resolution (here: the remote config refuses the key) keeps the last resolved configs.
    let latest = store.list(&base).await.unwrap().pop().unwrap();
    store.remove(&latest.id).await.unwrap();
    let loaded = loader.load_wellknown().await.unwrap();
    assert_eq!(loaded[0].1.len(), 2);
}
