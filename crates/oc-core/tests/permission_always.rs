//! Regression: replying "always" from another thread than the one that evaluated the request must
//! save the rule for the session's project, and later requests must be auto-approved
//! (core/src/permission.ts `reply` + `savedRules`). Found by the parity suite (permission_always).

use std::time::Duration;

use oc_core::app::App;
use oc_core::permission::{AssertInput, Reply};
use oc_core::runner::Engine;
use oc_core::session::CreateInput;
use oc_schema::types::{Effect, LocationRef};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn always_reply_saves_rule_for_session_project() {
    let root = std::env::temp_dir().join(format!("oc-perm-always-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    for (k, v) in [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache")] {
        unsafe { std::env::set_var(k, root.join(v)) };
    }
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_TEST_HOME", root.join("home"));
    }
    let global = oc_util::Global::acquire().unwrap();
    let db = oc_storage::Database::open(&global.database("latest"), &global.data).unwrap();
    let app = App::new(global, db, "2.0.26");
    let engine = Engine::new(app.clone(), &oc_core::services::default_services(&app));
    let directory = project.display().to_string();
    engine.location(&directory).await.unwrap();
    let session = app
        .sessions
        .create(CreateInput {
            location: Some(LocationRef { directory: directory.clone(), workspace_id: None }),
            ..Default::default()
        })
        .await
        .unwrap();

    let input = |id: &str| AssertInput {
        id: Some(id.to_string()),
        session_id: session.id.clone(),
        action: "read".into(),
        resources: vec![".env".into()],
        save: Some(vec!["*".into()]),
        metadata: None,
        source: None,
        agent: Some("build".into()),
    };
    let perms = app.permissions.clone();
    let first = input("per_000000000001aaaaaaaaaaaaaa");
    let waiter = tokio::spawn(async move { perms.assert(first).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Reply from a plain OS thread: nothing may depend on thread-local state from the evaluation.
    let perms = app.permissions.clone();
    std::thread::spawn(move || futures::executor::block_on(perms.reply("per_000000000001aaaaaaaaaaaaaa", Reply::Always, None)).unwrap())
        .join()
        .unwrap();
    waiter.await.unwrap().unwrap();

    let (_, effect) = app.permissions.ask(input("per_000000000002aaaaaaaaaaaaaa")).await;
    assert_eq!(effect, Effect::Allow);
    let _ = std::fs::remove_dir_all(&root);
}
