//! `State.changes` (#56267, `state.test.ts` "publishes the initial projection and skips unchanged reloads",
//! `config/reload.test.ts`): a location announces every catalog domain when it boots, and a configuration
//! reload then announces provider and model plus only the domains whose projection changed. The expected
//! sequences were recorded from the opencode 2.2.2 binary.

mod common;

use std::time::Duration;

use common::*;
use serde_json::json;
use tokio::sync::broadcast::Receiver;

type Rx = Receiver<oc_core::bus::Event>;

fn drain(rx: &mut Rx) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if e.kind.ends_with(".updated") && e.kind != "project.updated" {
            out.push(e.kind.clone());
        }
    }
    out
}

async fn wait_for(rx: &mut Rx, kind: &str) -> Vec<String> {
    let mut seen = Vec::new();
    let found = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let Ok(e) = rx.recv().await else { continue };
            if e.kind.ends_with(".updated") && e.kind != "project.updated" {
                seen.push(e.kind.clone());
            }
            if e.kind == kind {
                return;
            }
        }
    })
    .await;
    assert!(found.is_ok(), "no {kind}; saw {seen:?}");
    seen
}

#[tokio::test]
async fn reloads_announce_only_changed_domains() {
    unsafe { std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1") };
    let root = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    let project = root_path.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let config = project.join("opencode.json");
    std::fs::write(&config, r#"{"$schema": "https://opencode.ai/config.json"}"#).unwrap();
    let db = oc_storage::Database::memory().unwrap();
    let app = oc_core::app::App::new(global(&root_path), db, "1.2.3");
    let runner = oc_core::runner::Engine::new(app.clone(), &oc_core::services::default_services(&app));
    let opts = oc_server::ServerOptions { cwd: Some(project.display().to_string()), version: "1.2.3".into(), ready: true, ..Default::default() };
    let (router, _) = oc_backend::router(app.clone(), Some(runner), &opts).unwrap();
    let get = |router: axum::Router| async move {
        use tower::ServiceExt;
        let res = router
            .oneshot(axum::http::Request::get("/api/agent").body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    };
    let mut rx = app.bus.subscribe();

    get(router.clone()).await;
    let boot = wait_for(&mut rx, "plugin.updated").await;
    assert_eq!(
        boot,
        [
            "integration.updated",
            "provider.updated",
            "model.updated",
            "agent.updated",
            "command.updated",
            "skill.updated",
            "websearch.updated",
            "reference.updated",
            "plugin.updated"
        ]
    );
    // Let the baseline projections settle and the config watcher start.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    drain(&mut rx);

    let reload = |doc: serde_json::Value| {
        let mut doc = doc;
        doc["$schema"] = json!("https://opencode.ai/config.json");
        std::fs::write(&config, doc.to_string()).unwrap();
    };

    reload(json!({ "agents": { "extra": { "prompt": "x", "mode": "primary" } } }));
    wait_for(&mut rx, "config.updated").await;
    get(router.clone()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(drain(&mut rx), ["provider.updated", "model.updated", "provider.updated", "model.updated", "agent.updated"]);

    // A change no domain projects (a username) announces provider and model only.
    reload(json!({ "agents": { "extra": { "prompt": "x", "mode": "primary" } }, "username": "z" }));
    wait_for(&mut rx, "config.updated").await;
    get(router.clone()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(drain(&mut rx), ["provider.updated", "model.updated", "provider.updated", "model.updated"]);

    // Commands project `{name, description}`: a template-only edit announces nothing for them.
    reload(json!({ "agents": { "extra": { "prompt": "x", "mode": "primary" } }, "username": "z",
                   "commands": { "hello": { "template": "say hi", "description": "greet" } } }));
    wait_for(&mut rx, "config.updated").await;
    get(router.clone()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(drain(&mut rx), ["provider.updated", "model.updated", "provider.updated", "model.updated", "command.updated"]);
    reload(json!({ "agents": { "extra": { "prompt": "x", "mode": "primary" } }, "username": "z",
                   "commands": { "hello": { "template": "say hello", "description": "greet" } } }));
    wait_for(&mut rx, "config.updated").await;
    get(router.clone()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(drain(&mut rx), ["provider.updated", "model.updated", "provider.updated", "model.updated"]);
}
