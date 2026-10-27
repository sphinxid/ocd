//! The experimental todo list (`tool-todo.test.ts`): the `opencode.tool.todo` plugin adds `todowrite` to a
//! location's native tools only while its config sets `experimental.todo: true`, and stays listed either way.

use std::sync::Arc;

use oc_core::{app::App, runner::Engine};

#[tokio::test]
async fn todowrite_is_absent_unless_experimental_todo_is_enabled() {
    let root = tempfile::tempdir().unwrap();
    for (k, v) in [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache")] {
        // SAFETY: this test binary has a single test.
        unsafe { std::env::set_var(k, root.path().join(v)) };
    }
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_DISABLE_FILEWATCHER", "1");
        std::env::set_var("OPENCODE_TEST_HOME", root.path().join("home"));
    }
    let global = oc_util::Global::acquire().unwrap();
    let db = oc_storage::Database::open(&global.database("latest"), &global.data).unwrap();
    let app = App::new(global, db, "2.1.4");
    let http = reqwest::Client::new();
    let services = oc_tools::tools::Services {
        formatter: Arc::new(oc_tools::formatter::Formatter::new()),
        ripgrep: Arc::new(oc_tools::ripgrep::Ripgrep::new(app.global.bin.clone(), http.clone())),
        shell: oc_tools::shell::service::ShellService::new(app.global.data.clone(), None),
        websearch: Arc::new(oc_tools::websearch::WebSearch::new(Arc::new(NoStore), http.clone())),
        http,
    };
    let engine = Engine::new(app.clone(), &services);

    for (name, experimental, expected) in [
        ("unset", None, false),
        ("off", Some(serde_json::json!({ "todo": false })), false),
        ("other", Some(serde_json::json!({ "subagent_depth": 2 })), false),
        ("on", Some(serde_json::json!({ "todo": true })), true),
    ] {
        let project = root.path().join(name);
        std::fs::create_dir_all(&project).unwrap();
        let mut config = serde_json::json!({});
        if let Some(experimental) = experimental {
            config["experimental"] = experimental;
        }
        std::fs::write(project.join("opencode.json"), config.to_string()).unwrap();
        let location = engine.location(&project.display().to_string()).await.unwrap();
        let mcp = engine.mcp(&location).await;
        let registry = engine.base_registry(&location, &mcp);
        let names: Vec<String> = registry.list().into_iter().map(|(id, _)| id).collect();
        assert_eq!(names.iter().any(|n| n == "todowrite"), expected, "{name}: {names:?}");
        if expected {
            // Registered between `subagent` and `webfetch`, on the native list.
            let at = names.iter().position(|n| n == "todowrite").unwrap();
            assert_eq!((names[at - 1].as_str(), names[at + 1].as_str()), ("subagent", "webfetch"));
            let snapshot = registry.snapshot(&[]);
            assert!(snapshot.direct_tool("todowrite").is_some() && snapshot.codemode_tool("todowrite").is_none());
        }
    }
    let ids = oc_core::plugins::internal_plugins();
    let at = ids.pre.iter().position(|id| id == "opencode.tool.todo").expect("opencode.tool.todo is a pre plugin");
    assert_eq!((ids.pre[at - 1].as_str(), ids.pre[at + 1].as_str()), ("opencode.tool.subagent", "opencode.tool.webfetch"));
}

struct NoStore;
#[async_trait::async_trait]
impl oc_tools::websearch::WebSearchStore for NoStore {
    async fn kv_get(&self, _: &str) -> Option<serde_json::Value> {
        None
    }
    async fn kv_set(&self, _: &str, _: serde_json::Value) {}
    async fn kv_remove(&self, _: &str) {}
    async fn api_key(&self, _: &str) -> Option<String> {
        None
    }
}
