//! Integration tests against the real Bun sidecar and the committed bundle.
//!
//! Bun: `OCD_BUN_PATH` if set, otherwise `OCD_TEST_BUN`, otherwise `bun` 1.4.4 found on `PATH`
//! is not assumed — the tests are skipped (with a message) when no Bun is configured.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use oc_plugin::{
    ApiError, ApiRequest, HookError, HostEvent, HostOptions, InternalPlugins, LoadRequest, LocationRef, Operation, PluginApi,
    PluginHost, StorageEntry, StorageScan,
};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

/// bun 1.4.4 in the dev scratch folder (`$OCD_SCRATCH/bun144/bun-linux-x64/bun`), when `OCD_SCRATCH` is set.
fn scratch_bun() -> Option<PathBuf> {
    std::env::var_os("OCD_SCRATCH").map(|dir| PathBuf::from(dir).join("bun144/bun-linux-x64/bun"))
}

fn bun() -> Option<PathBuf> {
    static BUN: OnceLock<Option<PathBuf>> = OnceLock::new();
    BUN.get_or_init(|| {
        let path = std::env::var_os("OCD_BUN_PATH")
            .or_else(|| std::env::var_os("OCD_TEST_BUN"))
            .map(PathBuf::from)
            .or_else(|| scratch_bun().filter(|p| p.exists()))?;
        // SAFETY: set once, before any sidecar is spawned, to the same value in every test.
        unsafe { std::env::set_var("OCD_BUN_PATH", &path) };
        Some(path)
    })
    .clone()
}

macro_rules! require_bun {
    () => {
        if bun().is_none() {
            eprintln!("skipping: set OCD_BUN_PATH or OCD_TEST_BUN to a bun 1.4.4 binary");
            return;
        }
    };
}

fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name).to_string_lossy().into_owned()
}

/// A fake core: in-memory KV, a recorded call log, canned answers.
#[derive(Default)]
struct FakeApi {
    kv: Mutex<BTreeMap<String, Value>>,
    calls: Mutex<Vec<(Option<String>, String, Value)>>,
    invokes: Mutex<Vec<(String, Vec<Value>)>>,
    host: OnceLock<PluginHost>,
}

#[async_trait]
impl PluginApi for FakeApi {
    async fn call(&self, request: ApiRequest) -> Result<Value, ApiError> {
        self.calls.lock().push((request.plugin.clone(), request.method.clone(), request.input.clone()));
        match request.method.as_str() {
            "agent.list" => Ok(json!({ "location": { "directory": request.location.directory }, "data": [] })),
            "event.publish" => {
                let event = json!({
                    "id": "evt_test",
                    "created": 1,
                    "type": request.input["type"],
                    "location": { "directory": request.location.directory },
                    "data": request.input["data"],
                });
                if let Some(host) = self.host.get() {
                    host.publish_event(Some(&request.location), &event);
                }
                Ok(Value::Null)
            }
            "rpc.call" => Err(ApiError::named("rpc.unavailable", format!("RPC is unavailable: {}", request.input["rpcID"].as_str().unwrap_or("")))),
            m if m.ends_with(".reload") => Ok(Value::Null),
            other => Err(ApiError::unsupported(other)),
        }
    }

    async fn invoke(&self, _location: &LocationRef, handle: &str, args: Vec<Value>) -> Result<Value, ApiError> {
        self.invokes.lock().push((handle.to_string(), args));
        Ok(json!({ "content": "native read ok" }))
    }

    async fn storage_get(&self, key: &str) -> Result<Option<Value>, ApiError> {
        Ok(self.kv.lock().get(key).cloned())
    }

    async fn storage_set(&self, key: &str, value: Value) -> Result<(), ApiError> {
        self.kv.lock().insert(key.to_string(), value);
        Ok(())
    }

    async fn storage_remove(&self, key: &str) -> Result<(), ApiError> {
        self.kv.lock().remove(key);
        Ok(())
    }

    async fn storage_scan(&self, prefix: &str, after: Option<&str>, limit: Option<u64>) -> Result<StorageScan, ApiError> {
        let kv = self.kv.lock();
        let entries: Vec<StorageEntry> = kv
            .iter()
            .filter(|(k, _)| k.starts_with(prefix) && after.is_none_or(|a| k.as_str() > a))
            .take(limit.unwrap_or(u64::MAX) as usize)
            .map(|(k, v)| StorageEntry { key: k.clone(), value: v.clone() })
            .collect();
        Ok(StorageScan { entries, next: None })
    }
}

struct Setup {
    host: PluginHost,
    api: Arc<FakeApi>,
    location: LocationRef,
    _dir: tempfile::TempDir,
}

fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let api = Arc::new(FakeApi::default());
    let options = HostOptions {
        bin: dir.path().join("bin"),
        cache: dir.path().join("cache"),
        locks: dir.path().join("locks"),
        cwd: dir.path().to_path_buf(),
        app: json!({ "name": "opencode", "version": "2.0.26", "channel": "local" }),
    };
    let host = PluginHost::new(options, api.clone());
    let _ = api.host.set(host.clone());
    let location = LocationRef::new(dir.path().to_string_lossy().into_owned());
    Setup { host, api, location, _dir: dir }
}

fn info(location: &LocationRef) -> Value {
    json!({
        "directory": location.directory,
        "project": { "id": "global", "directory": location.directory, "canonical": location.directory },
    })
}

fn add(target: String, options: Value) -> Operation {
    let options: Map<String, Value> = options.as_object().cloned().unwrap_or_default();
    Operation::Add { target, options, mtime: None }
}

fn request(s: &Setup, operations: Vec<Operation>) -> LoadRequest {
    LoadRequest {
        location: s.location.clone(),
        info: info(&s.location),
        operations,
        internal: InternalPlugins { pre: vec!["opencode.agent".into()], post: vec!["opencode.browser".into()] },
        blocked: None,
    }
}

fn kv_key(plugin: &str, key: &str) -> String {
    format!("{}{key}", oc_plugin::storage::namespace(plugin))
}

fn native_tools() -> Value {
    json!({
        "tools": [{
            "id": "read",
            "name": "read",
            "description": "Read a file",
            "input": { "type": "object", "properties": { "path": { "type": "string" } } },
            "options": { "codemode": false },
            "execute": { "$fn": "rs:tool.read" },
        }],
        "namespaces": [],
    })
}

fn context(call: &str) -> Value {
    json!({ "sessionID": "ses_test", "agent": "build", "messageID": "msg_test", "id": call })
}

#[tokio::test]
async fn promise_plugin_tool_command_and_hook() {
    require_bun!();
    let s = setup();
    let result = s.host.load(request(&s, vec![add(fixture("promise-plugin"), json!({ "suffix": "!" }))])).await;
    assert_eq!(result.resolution.enabled_ids(), ["opencode.agent", "fixture.promise", "opencode.browser"]);
    let inventory = result.inventory(|id| json!({ "id": id, "source": { "type": "builtin" } }));
    assert_eq!(inventory[1]["id"], "fixture.promise");
    assert_eq!(inventory[1]["state"], json!({ "status": "active" }));
    assert_eq!(inventory[1]["source"]["type"], "local");
    assert!(s.host.is_running());

    // tool.execute.before mutates the input and can rename the tool.
    assert!(s.host.has_hook(&s.location, "tool.execute.before", None));
    assert!(!s.host.has_hook(&s.location, "tool.execute.after", None));
    let event = json!({ "tool": "echo", "sessionID": "ses_test", "agent": "build", "messageID": "msg_test", "id": "call_1", "input": { "text": "hi" } });
    let out = s.host.trigger_hook(&s.location, "tool.execute.before", event).await.unwrap();
    assert_eq!(out["input"], json!({ "text": "[hi]" }));
    let renamed = json!({ "tool": "legacy_echo", "sessionID": "ses_test", "agent": "build", "messageID": "msg_test", "id": "call_1", "input": {} });
    assert_eq!(s.host.trigger_hook(&s.location, "tool.execute.before", renamed).await.unwrap()["tool"], "echo");
    // No registration: returned untouched without IPC.
    let untouched = json!({ "x": 1 });
    assert_eq!(s.host.trigger_hook(&s.location, "shell.create.before", untouched.clone()).await.unwrap(), untouched);

    // Tool transform: plugin tool added, native tool wrapped.
    assert!(s.host.transform_domains(&s.location).contains(&"tool".to_string()));
    let tools = s.host.transform(&s.location, "tool", native_tools()).await.unwrap();
    let echo = tools["tools"].as_array().unwrap().iter().find(|t| t["id"] == "echo").unwrap().clone();
    assert_eq!(echo["description"], "Echo text!");
    assert_eq!(echo["input"]["required"], json!(["text"]));
    let handle = oc_plugin::protocol::function_handle(&echo["execute"]).unwrap().to_string();
    assert!(oc_plugin::protocol::is_plugin_handle(&handle));
    let read = tools["tools"].as_array().unwrap().iter().find(|t| t["id"] == "read").unwrap().clone();
    assert_eq!(read["description"], "Read a file (wrapped)");

    let progress = Arc::new(Mutex::new(Vec::new()));
    let sink = progress.clone();
    let result = s
        .host
        .execute_tool(&s.location, &handle, json!({ "text": "[hi]" }), context("call_1"), Some(Arc::new(move |m| sink.lock().push(m))))
        .await
        .unwrap();
    assert_eq!(result["output"], json!({ "text": "[hi]" }));
    assert_eq!(result["content"], json!([{ "type": "text", "text": "echo: [hi]" }]));
    assert_eq!(result["metadata"], json!({ "session": "ses_test" }));
    assert_eq!(*progress.lock(), vec![json!({ "stage": "echoing" })]);

    let error = s.host.execute_tool(&s.location, &handle, json!({ "text": 5 }), context("call_2"), None).await.unwrap_err();
    assert!(error.tool_error);
    assert_eq!(
        error.message,
        "Invalid arguments for tool \"echo\":\n- text: Expected string\n\nArguments provided:\n{\n  \"text\": 5\n}\n\nUpdate the arguments and call the tool again."
    );

    // The wrapped native tool calls back into Rust through `invoke`.
    let read_handle = oc_plugin::protocol::function_handle(&read["execute"]).unwrap().to_string();
    let result = s.host.execute_tool(&s.location, &read_handle, json!({ "path": "/x" }), context("call_3"), None).await.unwrap();
    assert_eq!(result["content"], json!([{ "type": "text", "text": "native read ok" }]));
    let invokes = s.api.invokes.lock().clone();
    assert_eq!(invokes[0].0, "rs:tool.read");
    assert_eq!(invokes[0].1[0], json!({ "path": "/x" }));
    assert_eq!(invokes[0].1[1], context("call_3"));

    // Command transform + execution through its handle.
    let commands = s.host.transform(&s.location, "command", json!({ "commands": [{ "name": "init", "execute": { "$fn": "rs:command.init" } }] })).await.unwrap();
    let list = commands["commands"].as_array().unwrap();
    assert_eq!(list[0], json!({ "name": "init", "execute": { "$fn": "rs:command.init" } }));
    assert_eq!(list[1]["name"], "hello");
    assert_eq!(list[1]["description"], "Say hello");
    let hello = oc_plugin::protocol::function_handle(&list[1]["execute"]).unwrap().to_string();
    let invocation = json!({ "sessionID": "ses_test", "prompt": { "text": "" }, "delivery": "steer" });
    s.host.invoke(&s.location, &hello, vec![invocation], None).await.unwrap();
    assert!(s.api.calls.lock().iter().any(|(plugin, method, _)| plugin.as_deref() == Some("fixture.promise") && method == "agent.list"));
    assert_eq!(s.api.kv.lock().get(&kv_key("fixture.promise", "hello")), Some(&json!({ "sessionID": "ses_test", "agents": 0 })));

    // Closing the location runs the plugin's cleanup.
    s.host.close_location(&s.location).await;
    assert_eq!(s.api.kv.lock().get(&kv_key("fixture.promise", "cleanup")), Some(&json!(true)));
    s.host.shutdown().await;
}

#[tokio::test]
async fn effect_plugin_transforms_and_hooks() {
    require_bun!();
    let s = setup();
    let result = s.host.load(request(&s, vec![add(fixture("effect-plugin"), json!({}))])).await;
    assert_eq!(result.packages[0]["id"], "fixture.effect");
    assert_eq!(result.packages[0]["state"]["status"], "active");

    let agent = |id: &str| json!({ "id": id, "name": id, "request": { "settings": {}, "headers": {}, "body": {} }, "mode": "primary", "hidden": false, "permissions": [] });
    let agents = s.host.transform(&s.location, "agent", json!({ "agents": [agent("build"), agent("plan")] })).await.unwrap();
    let list = agents["agents"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["description"], "Build things (from effect plugin)");
    assert_eq!(list[1]["id"], "reviewer");
    assert_eq!(list[1]["mode"], "subagent");
    // Agent.Info.default permissions for a new agent.
    assert_eq!(list[1]["permissions"][1], json!({ "action": "read", "resource": "*.env", "effect": "ask" }));

    let skills = s.host.transform(&s.location, "skill", json!({ "skills": [] })).await.unwrap();
    assert_eq!(skills["skills"][0]["id"], "fixture-skill");

    // A Tool.Error from tool.execute.before rejects the call.
    let event = json!({ "tool": "forbidden", "sessionID": "ses_test", "agent": "build", "messageID": "msg_test", "id": "call_1", "input": {} });
    match s.host.trigger_hook(&s.location, "tool.execute.before", event).await {
        Err(HookError::Tool { message, metadata, .. }) => {
            assert_eq!(message, "forbidden by fixture");
            assert_eq!(metadata, Some(json!({ "reason": "test" })));
        }
        other => panic!("unexpected {other:?}"),
    }

    // Provider-scoped session hook.
    assert!(s.host.has_hook(&s.location, "session.context", Some("anthropic")));
    assert!(!s.host.has_hook(&s.location, "session.context", Some("openai")));
    let ctx = |provider: &str| json!({ "sessionID": "ses_test", "agent": "build", "model": { "providerID": provider, "modelID": "m" }, "system": [], "messages": [], "options": {}, "tools": {} });
    let out = s.host.trigger_hook(&s.location, "session.context", ctx("openai")).await.unwrap();
    assert_eq!(out["system"], json!([]));
    let out = s.host.trigger_hook(&s.location, "session.context", ctx("anthropic")).await.unwrap();
    assert_eq!(out["system"], json!([{ "type": "text", "text": "scoped to anthropic" }]));

    let retry = json!({ "sessionID": "ses_test", "agent": "build", "model": { "providerID": "openai", "modelID": "m" }, "error": { "type": "unknown", "message": "x" }, "attempt": 3, "decision": { "retry": true, "delay": 1000 } });
    let out = s.host.trigger_hook(&s.location, "session.retry", retry).await.unwrap();
    assert_eq!(out["decision"], json!({ "retry": false }));
    s.host.shutdown().await;
}

#[tokio::test]
async fn catalog_transforms_return_untouched_entries_by_reference_and_reuse_inputs() {
    require_bun!();
    let s = setup();
    let result = s.host.load(request(&s, vec![add(fixture("catalog-plugin"), json!({}))])).await;
    assert_eq!(result.packages[0]["state"]["status"], "active", "{:?}", result.packages);

    let model = |provider: &str, id: &str| json!({ "id": id, "providerID": provider, "name": id });
    let record = |id: &str, models: Value| json!({ "provider": { "id": id, "name": id }, "models": models });
    let providers = json!({ "records": [
        record("untouched", json!([model("untouched", "m1")])),
        record("fixture-provider", json!([])),
    ] });
    let out = s.host.transform_keyed(&s.location, "provider", 1, || providers.clone()).await.unwrap().unwrap();
    let records = out["records"].as_array().unwrap();
    assert_eq!(records.len(), 2, "{out}");
    // A record no transform reached is not decoded, re-encoded or sent back.
    assert_eq!(records[0], json!({ "$same": "untouched" }));
    assert_eq!(records[1]["provider"]["name"], "Fixture Provider");
    assert_eq!(records[1]["models"][0]["id"], "fixture-model");
    assert_eq!(records[1]["models"][0]["name"], "Fixture Model");

    // The sidecar kept the input: the same key needs no value, and gives the same result.
    let again = s.host.transform_keyed(&s.location, "provider", 1, || panic!("input sent again")).await.unwrap().unwrap();
    assert_eq!(again, out);
    // A new key sends the new input.
    let changed = json!({ "records": [record("fixture-provider", json!([])), record("other", json!([]))] });
    let out = s.host.transform_keyed(&s.location, "provider", 2, || changed.clone()).await.unwrap().unwrap();
    assert_eq!(out["records"][0]["provider"]["name"], "Fixture Provider");
    assert_eq!(out["records"][1], json!({ "$same": "other" }));

    let models = json!({
        "models": [
            ["opencode", [model("opencode", "big-pickle"), model("opencode", "other")]],
            ["untouched", [model("untouched", "m1")]],
        ],
        "providers": [record("opencode", json!([]))],
        "default": { "providerID": "opencode", "modelID": "big-pickle" },
    });
    let out = s.host.transform_keyed(&s.location, "model", 3, || models.clone()).await.unwrap().unwrap();
    assert_eq!(out["models"][0][0], "opencode");
    assert_eq!(out["models"][0][1][0]["name"], "Big Pickle (renamed)");
    assert_eq!(out["models"][0][1][1]["id"], "other");
    assert_eq!(out["models"][1], json!(["untouched", { "$same": true }]));
    assert_eq!(out["default"], json!({ "providerID": "opencode", "modelID": "big-pickle" }));
    s.host.shutdown().await;
}

#[tokio::test]
async fn storage_events_and_rpc() {
    require_bun!();
    let s = setup();
    s.host.load(request(&s, vec![add(fixture("storage-events"), json!({}))])).await;
    {
        let kv = s.api.kv.lock();
        assert_eq!(kv.get(&kv_key("fixture.storage", "boots")), Some(&json!({ "count": 1 })));
        assert_eq!(kv.get(&kv_key("fixture.storage", "scanned")), Some(&json!(["prefix/a", "prefix/b"])));
        assert!(kv.keys().all(|k| k.starts_with("plugin:")));
    }
    assert!(s.host.wants_events());
    assert_eq!(s.host.rpc_ids(&s.location), ["fixture.echo"]);

    let output = s.host.rpc_call(&s.location, "fixture.echo", "ping", json!({ "text": "hey" })).await.unwrap();
    assert_eq!(output, json!({ "text": "pong: hey" }));
    // The RPC event went through event.publish and came back to the subscriber.
    assert!(s.api.calls.lock().iter().any(|(_, m, i)| m == "event.publish" && i["type"] == "rpc.fixture.echo.pong"));
    let refused = s.host.rpc_call(&s.location, "fixture.echo", "ping", json!({ "text": "no" })).await.unwrap_err();
    assert_eq!((refused.kind.as_str(), refused.message.as_str()), ("refused", "Refusing to ping"));
    assert_eq!(refused.data, Some(json!({ "why": "said no" })));
    let invalid = s.host.rpc_call(&s.location, "fixture.echo", "ping", json!({ "text": 3 })).await.unwrap_err();
    assert_eq!(invalid.kind, "rpc.invalid_input");
    let missing = s.host.rpc_call(&s.location, "fixture.echo", "nope", json!({})).await.unwrap_err();
    assert_eq!((missing.kind.as_str(), missing.message.as_str()), ("rpc.method_not_found", "Unknown RPC method: fixture.echo.nope"));
    let unknown = s.host.rpc_call(&s.location, "other", "ping", json!({})).await.unwrap_err();
    assert_eq!((unknown.kind.as_str(), unknown.message.as_str()), ("rpc.unavailable", "RPC is unavailable: other"));

    s.host.publish_event(None, &json!({ "id": "evt_x", "created": 2, "type": "plugin.updated", "data": {} }));
    // Not a server event: filtered by the sidecar.
    s.host.publish_event(None, &json!({ "id": "evt_y", "created": 3, "type": "not.a.server.event", "data": {} }));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (pong, updated) = {
            let kv = s.api.kv.lock();
            (kv.get(&kv_key("fixture.storage", "event:rpc.fixture.echo.pong")).cloned(), kv.get(&kv_key("fixture.storage", "event:plugin.updated")).cloned())
        };
        if pong.is_some() && updated.is_some() {
            assert_eq!(pong, Some(json!({ "text": "hey" })));
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "events not delivered");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!s.api.kv.lock().contains_key(&kv_key("fixture.storage", "event:not.a.server.event")));

    // A second boot sees the stored counter.
    s.host.close_location(&s.location).await;
    s.host.load(request(&s, vec![add(fixture("storage-events"), json!({}))])).await;
    assert_eq!(s.api.kv.lock().get(&kv_key("fixture.storage", "boots")), Some(&json!({ "count": 2 })));
    s.host.shutdown().await;
}

#[tokio::test]
async fn invalid_plugins_report_upstream_errors() {
    require_bun!();
    let s = setup();
    let empty = tempfile::tempdir().unwrap();
    let empty_target = empty.path().to_string_lossy().into_owned();
    let result = s.host.load(request(&s, vec![add(fixture("invalid"), json!({})), add(empty_target.clone(), json!({}))])).await;
    assert_eq!(result.resolution.enabled_ids(), ["opencode.agent", "opencode.browser"]);
    let failures = &result.resolution.failures;
    assert_eq!(failures.len(), 2);
    assert_eq!(failures[0]["state"]["error"], "Plugin must export a default definition with an id and an effect or setup function.");
    assert!(failures[0]["state"]["ref"].as_str().unwrap().starts_with("err_"));
    assert_eq!(failures[0]["source"], json!({ "type": "local", "path": fixture("invalid") }));
    assert_eq!(failures[0]["features"], json!({ "server": true }));
    assert_eq!(failures[1]["state"]["error"], format!("Plugin entrypoint not found: {empty_target}"));
    s.host.shutdown().await;
}

#[tokio::test]
async fn failing_transform_disables_the_plugin() {
    require_bun!();
    let s = setup();
    let mut events = s.host.subscribe();
    s.host.load(request(&s, vec![add(fixture("bad-transform"), json!({}))])).await;
    let before = s.host.domain_revision(&s.location, "command");
    let commands = s.host.transform(&s.location, "command", json!({ "commands": [] })).await.unwrap();
    assert_eq!(commands["commands"][0]["name"], "bad-first");
    let skills = s.host.transform(&s.location, "skill", json!({ "skills": [{ "id": "keep", "name": "keep", "description": "d", "path": "/k/SKILL.md", "content": "c" }] })).await.unwrap();
    assert_eq!(skills["skills"][0]["id"], "keep");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut changed = false;
    while !changed {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(HostEvent::DomainsChanged { domains, .. })) => changed = domains.contains(&"command".to_string()),
            Ok(Ok(_)) => {}
            other => panic!("no DomainsChanged: {other:?}"),
        }
    }
    assert!(s.host.domain_revision(&s.location, "command") > before);
    assert!(!s.host.transform_domains(&s.location).contains(&"command".to_string()));
    let packages = s.host.packages(&s.location);
    assert_eq!(packages[0]["state"]["status"], "failed");
    assert_eq!(packages[0]["state"]["error"], "Plugin disabled after skill.transform failed. Check server logs for details.");
    s.host.shutdown().await;
}

#[tokio::test]
async fn upstream_internal_plugin_runs_unchanged() {
    require_bun!();
    let s = setup();
    let result = s.host.load(request(&s, vec![add(fixture("upstream-mcp-codemode-defaults"), json!({}))])).await;
    assert_eq!(result.packages[0]["id"], "opencode.mcp.codemode.defaults");
    let value = json!({
        "servers": [
            ["executor", { "type": "local", "command": ["executor", "mcp"] }],
            ["posthog", { "type": "remote", "url": "https://mcp.posthog.com/mcp" }],
            ["other", { "type": "remote", "url": "https://example.com/mcp", "codemode": true }],
        ],
        "removed": [],
    });
    let out = s.host.transform(&s.location, "mcp", value).await.unwrap();
    let servers = out["servers"].as_array().unwrap();
    assert_eq!(servers[0][1]["codemode"], false);
    assert_eq!(servers[1][1]["headers"], json!({ "x-posthog-mcp-mode": "tools" }));
    assert_eq!(servers[2][1]["codemode"], true);
    s.host.shutdown().await;
}

#[tokio::test]
async fn no_sidecar_without_package_plugins() {
    require_bun!();
    let s = setup();
    let result = s.host.load(request(&s, vec![Operation::Remove { target: "opencode.browser".into() }, add("opencode.agent".into(), json!({}))])).await;
    assert_eq!(result.resolution.enabled_ids(), ["opencode.agent"]);
    assert!(!s.host.is_running());
}

#[tokio::test]
async fn restarts_after_a_crash() {
    require_bun!();
    let s = setup();
    let mut events = s.host.subscribe();
    s.host.load(request(&s, vec![add(fixture("promise-plugin"), json!({}))])).await;
    let pid = s.host.sidecar_pid().expect("running");
    // We started this process; kill exactly it.
    std::process::Command::new("kill").arg("-9").arg(pid.to_string()).status().unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut crashed = false;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(HostEvent::Crashed { .. })) => crashed = true,
            Ok(Ok(HostEvent::Restarted)) => break,
            Ok(Ok(_)) => {}
            other => panic!("no restart: {other:?}"),
        }
    }
    assert!(crashed);
    assert_ne!(s.host.sidecar_pid(), Some(pid));
    assert!(s.host.has_hook(&s.location, "tool.execute.before", None));
    let event = json!({ "tool": "echo", "sessionID": "ses_test", "agent": "build", "messageID": "msg_test", "id": "c", "input": { "text": "x" } });
    assert_eq!(s.host.trigger_hook(&s.location, "tool.execute.before", event).await.unwrap()["input"]["text"], "[x]");
    s.host.shutdown().await;
}

/// The sidecar dying while a hook runs skips the hook (like a sidecar that is not running) instead of failing
/// the caller, e.g. a session step's `session.context` hook.
#[tokio::test]
async fn hook_is_skipped_when_the_sidecar_dies_during_the_call() {
    require_bun!();
    let s = setup();
    s.host.load(request(&s, vec![add(fixture("promise-plugin"), json!({}))])).await;
    let event = json!({ "tool": "crash_host", "sessionID": "ses_test", "agent": "build", "messageID": "msg_test", "id": "c", "input": { "text": "x" } });
    let out = s.host.trigger_hook(&s.location, "tool.execute.before", event.clone()).await;
    assert_eq!(out.unwrap(), event);
    s.host.shutdown().await;
}

/// 2.2 plugin API: typed event subscriptions, the `experimental.idle` session hook, and the session instruction
/// and `experimental.generate` API calls with their endpoint identifiers.
#[tokio::test]
async fn v22_event_filter_idle_hook_and_api_calls() {
    require_bun!();
    let s = setup();
    let result = s.host.load(request(&s, vec![add(fixture("v22-plugin"), json!({}))])).await;
    assert_eq!(result.resolution.enabled_ids(), ["opencode.agent", "fixture.v22", "opencode.browser"]);
    // A local plugin's source is the configured target (its directory), and its features are its own parts.
    let inventory = result.inventory(|id| json!({ "id": id }));
    assert_eq!(inventory[1]["source"], json!({ "type": "local", "path": fixture("v22-plugin") }));
    assert_eq!(inventory[1]["features"], json!({ "server": true }));

    // `subscribe({types})`: known types only; unknown types are dropped, and none left never emits.
    assert!(s.host.wants_events());
    s.host.publish_event(None, &json!({ "id": "evt_1", "created": 1, "type": "plugin.updated", "data": {} }));
    s.host.publish_event(None, &json!({ "id": "evt_2", "created": 2, "type": "command.updated", "data": {} }));
    let key = |k: &str| kv_key("fixture.v22", k);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let done = {
            let kv = s.api.kv.lock();
            kv.contains_key(&key("all:command.updated")) && kv.contains_key(&key("some:command.updated"))
        };
        if done {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "events not delivered");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    {
        let kv = s.api.kv.lock();
        assert!(kv.contains_key(&key("all:plugin.updated")));
        assert!(!kv.contains_key(&key("some:plugin.updated")));
        assert!(!kv.keys().any(|k| k.starts_with(&key("none:"))));
    }

    // `session.experimental.idle`: the event reaches the hook as sent; a throwing hook fails the trigger.
    assert!(s.host.has_hook(&s.location, "session.experimental.idle", None));
    let background = json!([
        { "kind": "shell", "sessionID": "ses_idle", "shellID": "sh_1", "command": "sleep 10" },
        { "kind": "subagent", "sessionID": "ses_idle", "childSessionID": "ses_child", "agent": "general", "description": "look" },
    ]);
    let event = json!({ "sessionID": "ses_idle", "background": background });
    let out = s.host.trigger_hook(&s.location, "session.experimental.idle", event.clone()).await.unwrap();
    assert_eq!(out, event);
    assert_eq!(s.api.kv.lock().get(&key("idle:ses_idle")), Some(&event));
    let failed = s.host.trigger_hook(&s.location, "session.experimental.idle", json!({ "sessionID": "ses_fail", "background": [] })).await;
    assert!(matches!(&failed, Err(HookError::Failed(message)) if message.contains("idle hook failed")), "{failed:?}");

    // API calls use upstream's endpoint identifiers (the backend maps them to the experimental operations).
    let call = |method: &'static str, input: Value| {
        let host = s.host.clone();
        let location = s.location.clone();
        async move { host.rpc_call(&location, "fixture.v22", method, input).await }
    };
    let put = call("instructions", json!({ "op": "put", "sessionID": "ses_x", "key": "deploy-target", "value": "production" })).await;
    assert_eq!(put.unwrap_err().kind, "failed");
    let _ = call("instructions", json!({ "op": "list", "sessionID": "ses_x" })).await;
    let _ = call("instructions", json!({ "op": "remove", "sessionID": "ses_x", "key": "deploy-target" })).await;
    for kind in ["text", "legacy", "image", "decisions"] {
        let _ = call("generate", json!({ "kind": kind, "prompt": "hi" })).await;
    }
    let calls: Vec<(String, Value)> =
        s.api.calls.lock().iter().filter(|(p, _, _)| p.as_deref() == Some("fixture.v22")).map(|(_, m, i)| (m.clone(), i.clone())).collect();
    let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(
        methods,
        [
            "session.instructions.entry.put",
            "session.instructions.entry.list",
            "session.instructions.entry.remove",
            "generate.text",
            "generate.text",
            "generate.image",
            "generate.decisions",
        ]
    );
    assert_eq!(calls[0].1, json!({ "sessionID": "ses_x", "key": "deploy-target", "value": "production" }));
    assert_eq!(calls[1].1, json!({ "sessionID": "ses_x" }));
    assert_eq!(calls[3].1, json!({ "prompt": "hi", "model": { "providerID": "mock", "id": "m1" } }));
    s.host.shutdown().await;
}

/// 2.2 loader: a package with only a GUI part (and a TUI part) loads under its directory name with nothing to run;
/// a TUI-only package has no entrypoint for the server.
#[tokio::test]
async fn gui_only_and_tui_only_packages() {
    require_bun!();
    let s = setup();
    let result = s.host.load(request(&s, vec![add(fixture("gui-only"), json!({ "flag": true })), add(fixture("tui-only"), json!({}))])).await;
    assert_eq!(result.resolution.enabled_ids(), ["opencode.agent", "gui-only", "opencode.browser"]);
    let inventory = result.inventory(|id| json!({ "id": id }));
    assert_eq!(
        inventory[1],
        json!({
            "id": "gui-only",
            "source": { "type": "local", "path": fixture("gui-only") },
            "state": { "status": "active" },
            "features": { "tui": true, "gui": true },
        })
    );
    let failures = &result.resolution.failures;
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["state"]["error"], format!("Plugin entrypoint not found: {}", fixture("tui-only")));
    assert_eq!(failures[0]["features"], json!({ "server": true }));
    s.host.shutdown().await;
}

/// Installs a real published v2 plugin from npm with `bun add` (upstream cache layout) and runs it.
/// Needs network: `cargo test -p oc-plugin --test sidecar -- --ignored`.
#[tokio::test]
#[ignore = "needs network access to the npm registry"]
async fn real_npm_plugin_opencode_models_discovery() {
    require_bun!();
    let s = setup();
    let spec = "opencode-models-discovery@1.10.0";
    let result = s.host.load(request(&s, vec![add(spec.into(), json!({}))])).await;
    assert!(result.resolution.failures.is_empty(), "{:?}", result.resolution.failures);
    let info = &result.packages[0];
    assert_eq!(info["id"], "opencode.models-discovery");
    assert_eq!(info["state"]["status"], "active", "{info}");
    assert_eq!(info["source"], json!({ "type": "package", "target": spec, "version": "1.10.0" }));
    assert_eq!(info["features"], json!({ "server": true, "tui": true }));

    // Upstream cache layout: <cache>/npm/<name>@<spec>/<generation>/node_modules/<name>.
    let (installed, exists) = s.host.npm().resolve(spec);
    assert!(exists);
    assert!(installed.directory.contains("/npm/opencode-models-discovery@1.10.0/"), "{}", installed.directory);
    assert!(installed.directory.ends_with("/node_modules/opencode-models-discovery"));

    // The plugin asked for providers and registered its transforms.
    let calls = s.api.calls.lock().clone();
    assert!(calls.iter().any(|(p, m, _)| p.as_deref() == Some("opencode.models-discovery") && m == "provider.list"), "{calls:?}");
    let domains = s.host.transform_domains(&s.location);
    assert!(domains.contains(&"provider".to_string()), "{domains:?}");
    let providers = s.host.transform(&s.location, "provider", json!({ "records": [] })).await.unwrap();
    assert!(providers["records"].is_array());
    s.host.shutdown().await;
}
