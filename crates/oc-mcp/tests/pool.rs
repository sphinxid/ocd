//! MCP connection pooling (`pool: true`), ported from `describe("MCP pool")` in upstream's `test/mcp.test.ts`: one
//! connection per key shared by every Location of the process, a private connection for servers that negotiate the
//! legacy protocol or refuse the shared handshake, elicitations routed to the Location whose request caused them,
//! and the connection closed when its last Location lets go.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use common::modern::{self, Options};
use indexmap::IndexMap;
use oc_mcp::{ClientInfo, ElicitationHandler, ManagerOptions, McpEvent, McpManager, McpPool, ServerConfig, Status};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[test]
fn fake_modern_stdio_entry() {
    if std::env::var("OC_MCP_FAKE_MODERN").is_err() {
        return;
    }
    modern::run_stdio();
    std::process::exit(0);
}

/// Every log line of this test binary, to find the pool's "not shared" warnings.
fn logs() -> &'static Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buffer: Arc<Mutex<Vec<u8>>> = Arc::default();
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let writer = buffer.clone();
        let _ = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || Sink(writer.clone()))
            .try_init();
        buffer
    })
}

/// The pool's "not shared" warnings that name `server`.
fn ignored(server: &str) -> Vec<String> {
    let text = String::from_utf8_lossy(&logs().lock()).to_string();
    text.lines()
        .filter(|l| l.contains("mcp pool ignored") && l.contains(&format!("server={server}")))
        .map(|l| l.split("oc_mcp::pool: ").nth(1).unwrap_or(l).to_string())
        .collect()
}

fn remote(url: &str, extra: Value) -> ServerConfig {
    let mut config = json!({"type": "remote", "url": url, "oauth": false, "pool": true});
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        config[k] = v;
    }
    serde_json::from_value(config).unwrap()
}

fn local(env: &[(&str, &str)], cwd: Option<&Path>) -> ServerConfig {
    let exe = std::env::current_exe().unwrap().display().to_string();
    let mut environment = serde_json::Map::new();
    environment.insert("OC_MCP_FAKE_MODERN".into(), json!("1"));
    for (k, v) in env {
        environment.insert((*k).into(), json!(v));
    }
    let mut config = json!({
        "type": "local",
        "command": [exe, "fake_modern_stdio_entry", "--exact", "--nocapture", "--test-threads=1"],
        "environment": environment,
        "pool": true,
        "timeout": {"startup": 20000, "catalog": 10000, "execution": 10000}
    });
    if let Some(cwd) = cwd {
        config["cwd"] = json!(cwd.display().to_string());
    }
    serde_json::from_value(config).unwrap()
}

struct Location {
    manager: McpManager,
    events: Arc<Mutex<Vec<McpEvent>>>,
}

impl Location {
    fn status(&self, server: &str) -> Option<Status> {
        self.manager.status(server)
    }

    fn tool_names(&self) -> Vec<String> {
        self.manager.tools().into_iter().map(|t| t.name).collect()
    }
}

async fn open_with(
    name: &str,
    config: ServerConfig,
    pool: &McpPool,
    directory: &Path,
    elicitation: Option<Arc<dyn ElicitationHandler>>,
) -> Location {
    let _ = logs();
    let manager = McpManager::new(ManagerOptions {
        directory: directory.to_path_buf(),
        client_info: ClientInfo { name: "opencode".into(), version: "test".into() },
        credentials: None,
        elicitation,
        pool: Some(pool.clone()),
    });
    let events: Arc<Mutex<Vec<McpEvent>>> = Arc::default();
    let seen = events.clone();
    manager.on_event(Arc::new(move |event: &McpEvent| seen.lock().push(event.clone())));
    manager.set_servers(IndexMap::from([(name.to_string(), config)])).await;
    tokio::time::timeout(Duration::from_secs(30), manager.wait_ready(name)).await.expect("server settled").unwrap();
    Location { manager, events }
}

async fn open(name: &str, config: ServerConfig, pool: &McpPool, directory: &Path) -> Location {
    open_with(name, config, pool, directory, None).await
}

fn directories() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let (a, b) = (root.path().join("a"), root.path().join("fixture"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    (root, a, b)
}

async fn until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path).unwrap_or_default().lines().map(String::from).collect()
}

#[tokio::test]
async fn shares_one_connection_and_one_tool_list_between_locations() {
    let server = modern::serve_http(Options::default()).await;
    let pool = McpPool::new();
    let (_root, d0, d1) = directories();
    let a = open("resources", remote(&server.url, json!({})), &pool, &d0).await;
    let b = open("resources", remote(&server.url, json!({})), &pool, &d1).await;
    assert_eq!(a.status("resources"), Some(Status::Connected));
    assert_eq!(a.tool_names(), ["echo", "roots", "url-elicitation"]);
    assert_eq!(b.tool_names(), ["echo", "roots", "url-elicitation"]);
    assert_eq!(server.fake.state.lock().tool_lists, 1);
    a.manager.call_tool("resources", "echo", None, None).await.unwrap();
    b.manager.call_tool("resources", "echo", None, None).await.unwrap();
    assert_eq!(server.fake.state.lock().tool_calls.len(), 2);
    // A shared connection declares no roots.
    assert_eq!(server.fake.state.lock().capabilities.as_ref().and_then(|c| c.get("roots")), None);
    a.manager.shutdown().await;
    b.manager.shutdown().await;
}

#[tokio::test]
async fn keeps_connections_apart_when_the_configs_differ() {
    let server = modern::serve_http(Options::default()).await;
    let pool = McpPool::new();
    let (_root, d0, _) = directories();
    let a = open("resources", remote(&server.url, json!({})), &pool, &d0).await;
    let b = open("resources", remote(&server.url, json!({"headers": {"x-account": "b"}})), &pool, &d0).await;
    assert_eq!(a.status("resources"), Some(Status::Connected));
    assert_eq!(b.status("resources"), Some(Status::Connected));
    assert_eq!(server.fake.state.lock().tool_lists, 2);
}

#[tokio::test]
async fn keeps_a_server_that_negotiates_legacy_private_to_each_location() {
    let (root, d0, _) = directories();
    let (pids, wire) = (root.path().join("pids"), root.path().join("wire"));
    let config = local(
        &[("FAKE_LEGACY", "1"), ("FAKE_PID_FILE", &pids.display().to_string()), ("FAKE_WIRE_LOG", &wire.display().to_string())],
        Some(&d0),
    );
    let pool = McpPool::new();
    let a = open("legacy-private", config.clone(), &pool, &d0).await;
    let b = open("legacy-private", config, &pool, &d0).await;
    assert_eq!(a.status("legacy-private"), Some(Status::Connected));
    assert_eq!(b.status("legacy-private"), Some(Status::Connected));
    // One attempt through the pool, then each Location spawns its own, as it would unpooled.
    assert_eq!(lines(&pids).len(), 3);
    let methods = lines(&wire);
    let count = |method: &str| methods.iter().filter(|m| *m == method).count();
    // The private connections know the server is legacy, so they skip the probe.
    assert_eq!(count("server/discover"), 1, "{methods:?}");
    assert_eq!(count("initialize"), 3, "{methods:?}");
    assert_eq!(count("tools/list"), 2, "{methods:?}");
    // Once, when the verdict is recorded; the second Location skips straight to its own connection.
    let logged = ignored("legacy-private");
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert!(logged[0].starts_with("mcp pool ignored: server speaks an older protocol"), "{logged:?}");
    assert!(logged[0].contains("2025-11-25"), "{logged:?}");
    a.manager.shutdown().await;
    b.manager.shutdown().await;
}

#[tokio::test]
async fn connects_on_its_own_when_a_server_refuses_the_pooled_handshake() {
    let (root, d0, _) = directories();
    let pids = root.path().join("pids");
    // Exits during initialize unless the client declares roots, which shared connections never do.
    let config = local(&[("FAKE_LEGACY", "1"), ("FAKE_REQUIRE_ROOTS", "1"), ("FAKE_PID_FILE", &pids.display().to_string())], Some(&d0));
    let pool = McpPool::new();
    let a = open("refusing", config.clone(), &pool, &d0).await;
    assert_eq!(a.status("refusing"), Some(Status::Connected));
    let b = open("refusing", config, &pool, &d0).await;
    assert_eq!(b.status("refusing"), Some(Status::Connected));
    assert_eq!(lines(&pids).len(), 3);
    let logged = ignored("refusing");
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert!(logged[0].starts_with("mcp pool ignored: server refused the shared connection"), "{logged:?}");
    assert!(logged[0].contains("Connection closed"), "{logged:?}");
    a.manager.shutdown().await;
    b.manager.shutdown().await;
}

/// A server that is down, or that refuses this client, would refuse a private connection too.
#[tokio::test]
async fn keeps_pooling_a_server_that_was_down_or_refused_when_first_tried() {
    for (code, attempts, name) in [(503u16, 3usize, "down-503"), (401, 1, "down-401"), (403, 1, "down-403")] {
        let server = modern::serve_http(Options::default()).await;
        *server.refuse.lock() = Some(code);
        let pool = McpPool::new();
        let (_root, d0, d1) = directories();
        let a = open(name, remote(&server.url, json!({})), &pool, &d0).await;
        assert!(matches!(a.status(name), Some(Status::Failed { .. })), "{code}: {:?}", a.status(name));
        // The startup retries transient failures, and nothing tries a private connection afterwards.
        assert_eq!(*server.refused.lock(), attempts, "{code}");
        assert_eq!(ignored(name), Vec::<String>::new());
        *server.refuse.lock() = None;
        a.manager.connect(name).await.unwrap();
        assert_eq!(a.status(name), Some(Status::Connected));
        let b = open(name, remote(&server.url, json!({})), &pool, &d1).await;
        assert_eq!(b.status(name), Some(Status::Connected));
        assert_eq!(server.fake.state.lock().tool_lists, 1, "{code}");
    }
}

#[tokio::test]
async fn decides_a_legacy_server_is_private_right_after_the_handshake_before_listing_its_tools() {
    let (root, d0, d1) = directories();
    let wire = root.path().join("wire");
    // Connects without roots but needs one to list its tools, which shared connections never declare.
    let config = local(&[("FAKE_LEGACY", "1"), ("FAKE_REQUIRE_ROOTS_TO_LIST", "1"), ("FAKE_WIRE_LOG", &wire.display().to_string())], Some(&d0));
    let pool = McpPool::new();
    let a = open("listing", config.clone(), &pool, &d0).await;
    let b = open("listing", config, &pool, &d1).await;
    assert_eq!(a.status("listing"), Some(Status::Connected));
    assert_eq!(b.status("listing"), Some(Status::Connected));
    let methods = lines(&wire);
    let count = |method: &str| methods.iter().filter(|m| *m == method).count();
    // The shared attempt probes, falls back to initialize, then closes without listing.
    assert_eq!(count("server/discover"), 1, "{methods:?}");
    assert_eq!(count("initialize"), 3, "{methods:?}");
    assert_eq!(count("tools/list"), 2, "{methods:?}");
    assert_eq!(ignored("listing").len(), 1);
}

#[tokio::test]
async fn fails_a_pooled_server_that_cant_be_spawned_after_one_attempt() {
    let (_root, d0, _) = directories();
    let config: ServerConfig =
        serde_json::from_value(json!({"type": "local", "command": ["opencode-missing-mcp-server"], "pool": true})).unwrap();
    let pool = McpPool::new();
    let a = open("missing", config, &pool, &d0).await;
    assert!(matches!(a.status("missing"), Some(Status::Failed { .. })), "{:?}", a.status("missing"));
    // On Windows the spawn succeeds through the shell and the missing command shows up only as an exit during the
    // handshake, which can't be told apart from a server refusing it.
    if !cfg!(windows) {
        assert!(ignored("missing").iter().all(|l| !l.contains("refused")), "{:?}", ignored("missing"));
    }
    assert_eq!(pool.open_slots(), 0);
}

#[tokio::test]
async fn connects_each_location_on_its_own_without_pool() {
    let server = modern::serve_http(Options::default()).await;
    let pool = McpPool::new();
    let (_root, d0, _) = directories();
    let config: ServerConfig =
        serde_json::from_value(json!({"type": "remote", "url": server.url, "oauth": false, "protocol": "2026-07-28"})).unwrap();
    let a = open("resources", config.clone(), &pool, &d0).await;
    let b = open("resources", config, &pool, &d0).await;
    assert_eq!(a.status("resources"), Some(Status::Connected));
    assert_eq!(b.status("resources"), Some(Status::Connected));
    assert_eq!(server.fake.state.lock().tool_lists, 2);
    assert_eq!(pool.open_slots(), 0);
}

#[tokio::test]
async fn closes_the_shared_connection_when_its_last_location_lets_go() {
    let server = modern::serve_http(Options::default()).await;
    let pool = McpPool::new();
    let (_root, d0, d1) = directories();
    let config = remote(&server.url, json!({}));
    let a = open("resources", config.clone(), &pool, &d0).await;
    let b = open("resources", config.clone(), &pool, &d1).await;
    a.manager.shutdown().await;
    b.manager.call_tool("resources", "echo", None, None).await.unwrap();
    let c = open("resources", config.clone(), &pool, &d0).await;
    assert_eq!(server.fake.state.lock().tool_lists, 1);

    b.manager.shutdown().await;
    c.manager.disconnect("resources").await.unwrap();
    assert_eq!(pool.open_slots(), 0);
    let d = open("resources", config, &pool, &d0).await;
    assert_eq!(d.status("resources"), Some(Status::Connected));
    assert_eq!(server.fake.state.lock().tool_lists, 2);
}

/// `registers a server's prompt commands again after reconnecting`: a shared reconnect joins the open connection
/// instead of listing again, and announces every list.
#[tokio::test]
async fn a_shared_reconnect_joins_the_open_connection_and_announces_its_lists() {
    for shared in [true, false] {
        let server = modern::serve_http(Options::default()).await;
        let config = if shared {
            remote(&server.url, json!({}))
        } else {
            serde_json::from_value(json!({"type": "remote", "url": server.url, "oauth": false, "protocol": "2026-07-28"})).unwrap()
        };
        let pool = McpPool::new();
        let (_root, d0, d1) = directories();
        let _a = open("resources", config.clone(), &pool, &d0).await;
        let b = open("resources", config, &pool, &d1).await;
        b.manager.disconnect("resources").await.unwrap();
        assert!(b.manager.tools().is_empty());
        b.events.lock().clear();
        b.manager.connect("resources").await.unwrap();
        assert_eq!(b.status("resources"), Some(Status::Connected));
        assert_eq!(b.tool_names(), ["echo", "roots", "url-elicitation"]);
        let kinds: Vec<&str> = b.events.lock().iter().map(|e| e.event_type()).collect();
        for kind in ["mcp.tools.changed", "mcp.resources.changed", "mcp.prompts.changed", "mcp.status.changed"] {
            assert!(kinds.contains(&kind), "{shared}: {kinds:?}");
        }
        assert_eq!(server.fake.state.lock().tool_lists, if shared { 1 } else { 3 });
    }
}

/// Records the server name of every elicitation and answers as told.
struct Recorder {
    asked: Mutex<Vec<String>>,
    answer: Option<Value>,
    cancelled: Arc<Mutex<bool>>,
}

impl Recorder {
    fn new(answer: Option<Value>) -> Arc<Recorder> {
        Arc::new(Recorder { asked: Mutex::default(), answer, cancelled: Arc::default() })
    }
}

#[async_trait]
impl ElicitationHandler for Recorder {
    async fn create(&self, server: &str, _params: Value, cancel: CancellationToken) -> Result<Value, String> {
        self.asked.lock().push(server.to_string());
        if let Some(answer) = &self.answer {
            tokio::time::sleep(Duration::from_millis(50)).await;
            return Ok(answer.clone());
        }
        // Waits for an answer that never comes; dropping or cancelling the ask cancels its form.
        struct Dropped(Arc<Mutex<bool>>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                *self.0.lock() = true;
            }
        }
        let _dropped = Dropped(self.cancelled.clone());
        cancel.cancelled().await;
        Ok(json!({"action": "cancel"}))
    }

    async fn complete(&self, _server: &str, _elicitation_id: &str) {}
}

#[tokio::test]
async fn routes_an_elicitation_to_the_location_whose_call_caused_it() {
    let server = modern::serve_http(Options::default()).await;
    let pool = McpPool::new();
    let (_root, d0, d1) = directories();
    let elsewhere = Recorder::new(Some(json!({"action": "decline"})));
    let here = Recorder::new(Some(json!({"action": "accept"})));
    let _a = open_with("resources", remote(&server.url, json!({})), &pool, &d0, Some(elsewhere.clone())).await;
    let b = open_with("resources", remote(&server.url, json!({})), &pool, &d1, Some(here.clone())).await;
    let result = b.manager.call_tool("resources", "url-elicitation", None, None).await.unwrap();
    assert_eq!(result.structured, Some(json!({"action": "accept"})));
    assert_eq!(*here.asked.lock(), ["resources"]);
    assert!(elsewhere.asked.lock().is_empty());
}

#[tokio::test]
async fn asks_every_holder_about_an_elicitation_no_request_claims_and_takes_the_first_answer() {
    use oc_mcp::link::{LinkEvents, LinkInput, Spawn};
    use oc_mcp::pool::{PoolInput, Selection};
    struct Quiet;
    #[async_trait]
    impl LinkEvents for Quiet {
        async fn connecting(&self) {}
        async fn connected(&self) {}
        async fn failed(&self, _: Status, _: bool) {}
        async fn settled(&self) {}
        async fn closed(&self, _: String) {}
        async fn changed(&self, _: oc_mcp::link::Kind) {}
    }
    let server = modern::serve_http(Options::default()).await;
    let pool = McpPool::new();
    let (_root, d0, d1) = directories();
    let config: ServerConfig =
        serde_json::from_value(json!({"type": "remote", "url": server.url, "oauth": false, "protocol": "2026-07-28"})).unwrap();
    let key = oc_mcp::pool::key(&oc_mcp::pool::KeyInput { config: &config, cwd: None, credential: None, client_info: None, mcp_events: false });
    // Each holder knows the server by its own name; the connection carries the first one's.
    let holder = |directory: &Path, name: &str, handler: Arc<Recorder>| {
        let key = key.clone();
        pool.connection(PoolInput {
            link: LinkInput {
                name: name.to_string(),
                config: config.clone(),
                directory: directory.to_path_buf(),
                shared: false,
                client_info: ClientInfo::default(),
                auth_provider: oc_mcp::link::no_auth(),
                elicitation: Some(handler),
                lock: Default::default(),
                callbacks: Arc::new(Spawn),
                fork: Arc::new(Spawn),
                events: Arc::new(Quiet),
            },
            select: Arc::new(move || {
                let key = key.clone();
                Box::pin(async move { Selection { key, auth_provider: oc_mcp::link::no_auth() } })
            }),
        })
    };
    let first = Recorder::new(Some(json!({"action": "accept"})));
    let second = Recorder::new(None);
    let a = holder(&d0, "first", first.clone());
    a.start().await;
    let b = holder(&d1, "second", second.clone());
    b.start().await;
    let client = a.client().expect("a shared connection");
    assert!(Arc::ptr_eq(&client, &b.client().unwrap()));
    // Called on the shared client directly, so no Location claims the request.
    let result = client.call_tool("url-elicitation", None, None).await.unwrap();
    assert_eq!(result.structured, Some(json!({"action": "accept"})));
    until("the other form to be cancelled", || *second.cancelled.lock()).await;
    assert_eq!(*first.asked.lock(), ["first"]);
    assert_eq!(*second.asked.lock(), ["second"]);
}

#[tokio::test]
async fn fans_tool_list_changes_out_to_every_location() {
    let server = modern::serve_http(Options { list_changed: true, ..Default::default() }).await;
    let pool = McpPool::new();
    let (_root, d0, d1) = directories();
    let a = open("resources", remote(&server.url, json!({})), &pool, &d0).await;
    let b = open("resources", remote(&server.url, json!({})), &pool, &d1).await;
    a.events.lock().clear();
    b.events.lock().clear();
    server.fake.state.lock().extra_tools = vec![json!({"name": "added", "inputSchema": {"type": "object", "properties": {}}})];
    // The listen stream opens right after the handshake.
    until("the listen stream", || server.fake.state.lock().methods.iter().any(|m| m == "subscriptions/listen")).await;
    server.send_tool_list_changed();
    for location in [&a, &b] {
        until("the added tool", || location.tool_names().contains(&"added".to_string())).await;
        until("tools.changed", || location.events.lock().iter().any(|e| e.event_type() == "mcp.tools.changed")).await;
    }
    assert_eq!(server.fake.state.lock().tool_lists, 2);
}

#[tokio::test]
async fn answers_no_roots_on_a_shared_connection() {
    let server = modern::serve_http(Options::default()).await;
    let pool = McpPool::new();
    let (_root, d0, _) = directories();
    let own_config: ServerConfig =
        serde_json::from_value(json!({"type": "remote", "url": server.url, "oauth": false, "protocol": "2026-07-28"})).unwrap();
    let own = open("resources", own_config, &pool, &d0).await;
    let roots = own.manager.call_tool("resources", "roots", None, None).await.unwrap();
    assert_eq!(roots.structured, Some(json!({"kind": "roots", "roots": [{"uri": oc_mcp::connection::file_url(&d0)}]})));
    let shared = open("resources", remote(&server.url, json!({})), &pool, &d0).await;
    let error = shared.manager.call_tool("resources", "roots", None, None).await.unwrap_err();
    assert!(error.to_string().contains("roots/list"), "{error}");
}

#[tokio::test]
async fn shares_a_local_server_across_directories_only_when_its_cwd_is_fixed() {
    let (root, d0, d1) = directories();
    let fixed = root.path().join("fixed");
    std::fs::create_dir_all(&fixed).unwrap();
    let pids = |name: &str| root.path().join(name);
    let server = |file: &str, cwd: Option<&Path>| local(&[("FAKE_PID_FILE", &pids(file).display().to_string())], cwd);
    let pool = McpPool::new();
    let mut held = Vec::new();
    for directory in [&d0, &d1] {
        held.push(open("fixed-cwd", server("home", Some(&fixed)), &pool, directory).await);
    }
    assert_eq!(lines(&pids("home")).len(), 1);
    assert_eq!(ignored("fixed-cwd"), Vec::<String>::new());

    for directory in [&d0, &d1] {
        held.push(open("project-cwd", server("project", None), &pool, directory).await);
    }
    assert_eq!(lines(&pids("project")).len(), 2);
    let logged = ignored("project-cwd");
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert!(logged[0].starts_with("mcp pool ignored: local server has no fixed cwd, so each directory runs its own"), "{logged:?}");
    for location in held {
        location.manager.shutdown().await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn shares_one_local_process_fails_every_holder_when_it_dies_and_closes_it_after_the_last() {
    let (root, d0, _) = directories();
    let pids = root.path().join("pids");
    let config = local(&[("FAKE_PID_FILE", &pids.display().to_string())], Some(&d0));
    let alive = |pid: i32| unsafe { libc::kill(pid, 0) == 0 };
    let pool = McpPool::new();
    let a = open("process", config.clone(), &pool, &d0).await;
    let b = open("process", config, &pool, &d0).await;
    let started = || lines(&pids).iter().map(|p| p.parse::<i32>().unwrap()).collect::<Vec<_>>();
    assert_eq!(started().len(), 1);
    let first = started()[0];

    unsafe { libc::kill(first, libc::SIGKILL) };
    for location in [&a, &b] {
        until("failed", || matches!(location.status("process"), Some(Status::Failed { .. }))).await;
    }
    a.manager.connect("process").await.unwrap();
    assert_eq!(a.status("process"), Some(Status::Connected));
    assert!(matches!(b.status("process"), Some(Status::Failed { .. })));
    b.manager.connect("process").await.unwrap();
    assert_eq!(b.status("process"), Some(Status::Connected));
    assert_eq!(started().len(), 2);
    let second = started()[1];

    a.manager.shutdown().await;
    assert!(alive(second));
    b.manager.call_tool("process", "echo", None, None).await.unwrap();
    b.manager.shutdown().await;
    until("the process to exit", || !alive(second)).await;
}
