//! `list_mcp_events` (plugin `opencode.tools.mcp-events`), ported from upstream 2.4.1's `test/tool-mcp-events.test.ts`
//! over the real registry, a Location's MCP manager and a fake 2026-07-28 server offering MCP Events over Streamable
//! HTTP.

#[path = "../../oc-mcp/tests/common/modern.rs"]
#[allow(dead_code)]
mod modern;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use modern::{Http, Options};
use oc_core::app::App;
use oc_core::runner::Engine;
use oc_schema::types::PermissionRule;
use oc_tools::{Tool, ToolContext, ToolResult};
use serde_json::{Value, json};

struct H {
    app: Arc<App>,
    engine: Arc<Engine>,
    dir: PathBuf,
    server: Http,
    _tmp: tempfile::TempDir,
}

fn global(root: &Path) -> oc_util::Global {
    let g = |p: &str| {
        let d = root.join(p);
        std::fs::create_dir_all(&d).unwrap();
        d
    };
    oc_util::Global {
        home: g("home"),
        data: g("data"),
        cache: g("cache"),
        config: g("config"),
        state: g("state"),
        tmp: g("tmp"),
        bin: g("bin"),
        log: g("log"),
        repos: g("repos"),
    }
}

struct NoStore;
#[async_trait]
impl oc_tools::websearch::WebSearchStore for NoStore {
    async fn kv_get(&self, _: &str) -> Option<Value> {
        None
    }
    async fn kv_set(&self, _: &str, _: Value) {}
    async fn kv_remove(&self, _: &str) {}
    async fn api_key(&self, _: &str) -> Option<String> {
        None
    }
}

async fn harness() -> H {
    unsafe { std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1") };
    let server = modern::serve_http(Options {
        events: true,
        event_pages: Some(vec![vec![
            json!({"name": "job.finished", "description": "A job ended", "delivery": ["push"], "inputSchema": {}, "payloadSchema": {"type": "object"}}),
            json!({"name": "job.polled", "delivery": ["poll"], "inputSchema": {}, "payloadSchema": {}}),
        ]]),
        ..Default::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let root = oc_util::fs::canonicalize(tmp.path()).unwrap();
    let dir = root.join("project");
    std::fs::create_dir_all(&dir).unwrap();
    let config = json!({
        "mcp": {"servers": {
            "jobs": {"type": "remote", "url": server.url, "oauth": false},
            "offline": {"type": "remote", "url": server.url, "oauth": false, "protocol": "legacy"},
        }},
    });
    std::fs::write(dir.join("opencode.json"), config.to_string()).unwrap();
    let app = App::new(global(&root.join("g")), oc_storage::Database::memory().unwrap(), "2.3.1");
    let services = oc_tools::tools::Services {
        formatter: Arc::new(oc_tools::formatter::Formatter::new()),
        ripgrep: Arc::new(oc_tools::ripgrep::Ripgrep::new(app.global.bin.clone(), reqwest::Client::new())),
        shell: oc_tools::shell::service::ShellService::new(app.global.data.clone(), None),
        websearch: Arc::new(oc_tools::websearch::WebSearch::new(Arc::new(NoStore), reqwest::Client::new())),
        http: reqwest::Client::new(),
    };
    let engine = Engine::new(app.clone(), &services);
    H { app, engine, dir, server, _tmp: tmp }
}

fn rule(action: &str, resource: &str, effect: &str) -> PermissionRule {
    serde_json::from_value(json!({"action": action, "resource": resource, "effect": effect})).unwrap()
}

impl H {
    async fn session(&self, rules: Vec<PermissionRule>) -> String {
        let session = self
            .app
            .sessions
            .create(oc_core::session::CreateInput {
                location: Some(oc_schema::types::LocationRef { directory: self.dir.display().to_string(), workspace_id: None }),
                permissions: Some(rules),
                ..Default::default()
            })
            .await
            .unwrap();
        session.id
    }

    /// The location's registry, as a session's step lists it.
    async fn ids(&self) -> Vec<String> {
        let location = self.engine.location(&self.dir.display().to_string()).await.unwrap();
        let mcp = self.engine.mcp(&location).await;
        mcp.wait_ready("jobs").await.unwrap();
        let registry = self.engine.base_registry(&location, &mcp).await;
        registry.list().into_iter().map(|(id, _)| id).collect()
    }

    async fn tool(&self) -> Option<Arc<dyn Tool>> {
        let location = self.engine.location(&self.dir.display().to_string()).await.unwrap();
        let mcp = self.engine.mcp(&location).await;
        mcp.wait_ready("jobs").await.unwrap();
        mcp.wait_ready("offline").await.unwrap();
        let registry = self.engine.base_registry(&location, &mcp).await;
        registry.list().into_iter().find(|(id, _)| id == "opencode_list_mcp_events").map(|(_, tool)| tool)
    }

    async fn list(&self, session: &str, input: Value) -> Result<ToolResult, oc_tools::ToolError> {
        let tool = self.tool().await.expect("the list tool");
        let ctx = ToolContext {
            codemode: false,
            session_id: session.into(),
            message_id: "msg_tool".into(),
            call_id: "call_list".into(),
            agent: "build".into(),
            directory: self.dir.clone(),
            worktree: self.dir.clone(),
            project_id: String::new(),
            workspace_id: None,
            global: self.app.global.clone(),
            cancel: tokio_util::sync::CancellationToken::new(),
            stop: Default::default(),
            progress: Arc::new(|_| {}),
            config: Arc::new(oc_tools::tool::ToolConfig::default()),
            host: Arc::new(oc_core::host::CoreHost { app: self.app.clone(), skills: Arc::new(Vec::new()), mcp: None, location: None, engine: None, blocking_parent: None }),
        };
        tool.execute(input, ctx).await
    }
}

#[tokio::test]
async fn lists_one_servers_push_events_after_asking_permission() {
    let h = harness().await;
    let ids = h.ids().await;
    assert!(ids.contains(&"opencode_list_mcp_events".to_string()), "{ids:?}");
    assert!(!ids.contains(&"opencode_watch_mcp_event".to_string()), "{ids:?}");
    let session = h.session(vec![rule("*", "*", "allow")]).await;
    let result = h.list(&session, json!({"server": "jobs"})).await.unwrap();
    assert_eq!(
        serde_json::to_string(&result.output.unwrap()).unwrap(),
        r#"{"events":[{"server":"jobs","event":"job.finished","description":"A job ended","args":{},"data":{"type":"object"}}]}"#
    );
    // The permission is asked per server, and denying it refuses the listing.
    let denied = h.session(vec![rule("*", "*", "allow"), rule("opencode_list_mcp_events", "jobs", "deny")]).await;
    let error = h.list(&denied, json!({"server": "jobs"})).await.unwrap_err();
    assert!(error.message.contains("opencode_list_mcp_events"), "{}", error.message);
    let other = h.session(vec![rule("*", "*", "allow"), rule("opencode_list_mcp_events", "other", "deny")]).await;
    assert!(h.list(&other, json!({"server": "jobs"})).await.is_ok());
}

#[tokio::test]
async fn fails_for_a_server_that_sends_no_events() {
    let h = harness().await;
    let session = h.session(vec![rule("*", "*", "allow")]).await;
    // Connected over the legacy handshake, where MCP Events aren't defined.
    let error = h.list(&session, json!({"server": "offline"})).await.unwrap_err();
    assert_eq!(error.message, r#"MCP server "offline" is not connected or sends no events"#);
}
