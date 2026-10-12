use std::time::Duration;

use oc_core::app::App;
use oc_core::runner::Engine;

#[tokio::test]
async fn config_change_reloads_location() {
    let root = std::env::temp_dir().join(format!("oc-watch-{}", std::process::id()));
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
    std::fs::write(project.join("opencode.json"), r#"{"shell": "bash"}"#).unwrap();

    let global = oc_util::Global::acquire().unwrap();
    let db = oc_storage::Database::open(&global.database("latest"), &global.data).unwrap();
    let app = App::new(global, db, "2.0.26");
    let engine = Engine::new(app.clone(), &oc_core::services::default_services(&app));
    let directory = project.display().to_string();
    let before = engine.location(&directory).await.unwrap();
    let mut rx = app.bus.subscribe();
    // Let the watcher settle before writing.
    tokio::time::sleep(Duration::from_millis(200)).await;

    std::fs::write(project.join("opencode.json"), r#"{"shell": "zsh"}"#).unwrap();
    let event = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = rx.recv().await.unwrap();
            if event.kind == "config.updated" {
                return event;
            }
        }
    })
    .await
    .expect("config.updated");
    assert_eq!(event.location.as_ref().unwrap().directory, directory);
    let after = engine.location(&directory).await.unwrap();
    assert_ne!(before.entries, after.entries);

    // Touching an unrelated file does not reload.
    std::fs::write(project.join("main.rs"), "fn main() {}").unwrap();
    let unrelated = tokio::time::timeout(Duration::from_millis(600), async {
        loop {
            if rx.recv().await.unwrap().kind == "config.updated" {
                return;
            }
        }
    })
    .await;
    assert!(unrelated.is_err());
    std::fs::remove_dir_all(&root).ok();
}
