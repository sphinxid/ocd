//! The Claude Agent provider against a stand-in `claude` (tests/fixtures/claude-agent/fake-claude.py, which speaks
//! Claude Code's SDK stdio protocol from a script): the login probe, turns through the session registry, the MCP
//! tool bridge, cancellation, failures and side calls. Skipped when `python3` is unavailable.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use oc_core::claude_agent::probe::{self, ProbeResult};
use oc_core::claude_agent::process::{Options, SystemPrompt};
use oc_core::claude_agent::sessions::{self, Context, Fork, Io, Registry, Reporter, Request, Storage};
use oc_core::claude_agent::stream::Converter;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn standin() -> Option<PathBuf> {
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        eprintln!("python3 not found: skipping");
        return None;
    }
    Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-agent/fake-claude.py"))
}

/// A scratch directory with the scenario and the stand-in's log.
struct Env {
    dir: tempfile::TempDir,
    script: PathBuf,
}

impl Env {
    fn new(scenario: Value) -> Env {
        let dir = tempfile::tempdir().unwrap();
        // A `.py` path would be run with Bun; the stand-in must look like a native executable.
        let script = dir.path().join("claude");
        std::fs::copy(standin().unwrap(), &script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(dir.path().join("scenario.json"), scenario.to_string()).unwrap();
        Env { dir, script }
    }

    fn env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
        env.insert("HOME".into(), self.dir.path().join("home").to_string_lossy().into_owned());
        env.insert("CLAUDE_STANDIN_LOG".into(), self.dir.path().join("log.jsonl").to_string_lossy().into_owned());
        env.insert("CLAUDE_STANDIN_SCENARIO".into(), self.dir.path().join("scenario.json").to_string_lossy().into_owned());
        env.insert("CLAUDE_CODE_SIMPLE".into(), "1".into());
        env.insert("NODE_OPTIONS".into(), "--inspect".into());
        env
    }

    fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("log.jsonl")).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn spawns(&self) -> Vec<Value> {
        self.log().into_iter().filter(|e| e["event"] == "spawn").collect()
    }

    /// Messages the server wrote to `claude`.
    fn received(&self) -> Vec<Value> {
        self.log().into_iter().filter(|e| e["event"] == "in").map(|e| e["message"].clone()).collect()
    }
}

struct TestIo {
    exe: String,
    env: BTreeMap<String, String>,
}

#[async_trait]
impl Io for TestIo {
    fn which(&self) -> Option<String> {
        Some(self.exe.clone())
    }
    fn env(&self) -> BTreeMap<String, String> {
        self.env.clone()
    }
    async fn session_exists(&self, _session_id: &str) -> Result<bool, String> {
        Ok(true)
    }
    async fn fork(&self, _session_id: &str, _message_id: &str) -> Result<Option<Fork>, String> {
        Ok(None)
    }
    async fn remove(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Default)]
struct MemoryStorage(Mutex<HashMap<String, Value>>);

#[async_trait]
impl Storage for MemoryStorage {
    async fn get(&self, key: &str) -> Option<Value> {
        self.0.lock().get(key).cloned()
    }
    async fn set(&self, key: &str, value: Value) {
        self.0.lock().insert(key.to_string(), value);
    }
}

#[derive(Default)]
struct Reports(Mutex<Vec<String>>);

impl Reporter for Reports {
    fn missing(&self) {
        self.0.lock().push("missing".into());
    }
    fn ready(&self, probe: ProbeResult) {
        self.0.lock().push(format!("ready {}", probe.version.unwrap_or_default()));
    }
}

fn context(env: &Env, storage: Arc<MemoryStorage>, reports: Arc<Reports>) -> Context {
    Context {
        io: Arc::new(TestIo { exe: env.script.to_string_lossy().into_owned(), env: env.env() }),
        storage,
        reporter: reports,
        directory: env.dir.path().to_string_lossy().into_owned(),
    }
}

fn user(text: &str) -> Value {
    json!({ "role": "user", "content": [{ "type": "text", "text": text }] })
}

fn read_tool() -> Value {
    json!({ "type": "function", "name": "read", "description": "Read a file", "inputSchema": { "type": "object", "properties": { "filePath": { "type": "string" } } } })
}

type Parts = Arc<Mutex<Vec<Value>>>;

fn request(session: Option<&str>, prompt: Vec<Value>, tools: Vec<Value>) -> (Request, Parts) {
    let parts: Parts = Arc::default();
    let sink = parts.clone();
    let mut convert = Converter::new(tools.clone());
    let system = if session.is_some() {
        oc_core::claude_agent::bridge::system_prompt(Some("Be brief."), Some("Project rules"))
    } else {
        SystemPrompt::Text("Return only the requested output.".into())
    };
    let request = Request {
        session_id: session.map(String::from),
        source_id: None,
        compaction: false,
        model: "haiku".into(),
        effort: None,
        max_output_tokens: Some(4096.0),
        connection: vec![("ANTHROPIC_BASE_URL".into(), "http://127.0.0.1:9".into())],
        system,
        prompt,
        tools,
        emit: Box::new(move |message: &Value| sink.lock().extend(convert.convert(message))),
    };
    (request, parts)
}

fn kinds(parts: &Parts) -> Vec<String> {
    parts.lock().iter().map(|p| p["type"].as_str().unwrap_or_default().to_string()).collect()
}

fn text(parts: &Parts) -> String {
    parts.lock().iter().filter(|p| p["type"] == "text-delta").map(|p| p["delta"].as_str().unwrap_or_default()).collect()
}

#[tokio::test]
async fn probe_reports_version_account_and_context_windows() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(json!({ "contextWindows": { "opus": 1000000, "haiku": 200000 } }));
    let options = Options {
        executable: env.script.to_string_lossy().into_owned(),
        bun: None,
        cwd: None,
        env: sessions::base_env(env.env(), &[]),
        model: None,
        effort: None,
        resume: None,
        session_id: None,
        include_partial_messages: false,
        persist: false,
        system: SystemPrompt::Default,
        hooks: None,
        tools: None,
    };
    let result = probe::run(options).await.unwrap();
    assert_eq!(result.version.as_deref(), Some("2.1.302"));
    assert!(probe::is_logged_in(&result.account));
    let windows: Vec<Value> = result.models.iter().map(|m| m.get("contextWindow").cloned().unwrap_or(Value::Null)).collect();
    assert_eq!(windows, vec![Value::Null, json!(1000000), json!(200000)]);
    let spawn = &env.spawns()[0];
    assert_eq!(
        spawn["argv"],
        json!([
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json",
            "--thinking=adaptive",
            "--thinking-display=summarized",
            "--allowedTools=mcp__opencode__*",
            "--tools=",
            "--setting-sources=",
            "--strict-mcp-config",
            "--permission-mode=dontAsk",
            "--no-session-persistence",
            "--settings",
            "{\"disableAllHooks\":true}"
        ])
    );
    let received = env.received();
    assert_eq!(received[0]["request"], json!({ "subtype": "initialize", "systemPrompt": [""], "skills": [] }));
    let subtypes: Vec<&str> = received.iter().filter_map(|m| m["request"]["subtype"].as_str()).collect();
    assert_eq!(subtypes, ["initialize", "set_model", "get_context_usage", "set_model", "get_context_usage"]);
    // Isolation and the SDK's own variables.
    let child = spawn["env"].as_object().unwrap();
    for (k, v) in [
        ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1"),
        ("DISABLE_AUTO_COMPACT", "1"),
        ("ENABLE_TOOL_SEARCH", "false"),
        ("MAX_MCP_OUTPUT_TOKENS", "200000"),
        ("CLAUDE_CODE_ENTRYPOINT", "sdk-ts"),
        ("CLAUDE_AGENT_SDK_VERSION", "0.3.301"),
        ("CLAUDE_CODE_SDK_READS_SESSION_STATE", "1"),
    ] {
        assert_eq!(child.get(k), Some(&json!(v)), "{k}");
    }
    assert!(!child.contains_key("CLAUDE_CODE_SIMPLE"));
    assert!(!child.contains_key("NODE_OPTIONS"));
}

#[tokio::test]
async fn logged_out_and_old_versions() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(json!({ "init": { "account": { "tokenSource": "none", "apiProvider": "firstParty" }, "claude_code_version": "2.1.200" } }));
    let options = Options {
        executable: env.script.to_string_lossy().into_owned(),
        bun: None,
        cwd: None,
        env: sessions::base_env(env.env(), &[]),
        model: None,
        effort: None,
        resume: None,
        session_id: None,
        include_partial_messages: false,
        persist: false,
        system: SystemPrompt::Default,
        hooks: None,
        tools: None,
    };
    // An old Claude Code still lists its models (no context discovery before 2.1.257).
    let result = probe::run(options).await.unwrap();
    assert!(!probe::is_logged_in(&result.account));
    assert_eq!(env.received().len(), 1);
    // A chat refuses to start.
    let registry = Registry::new();
    let (request, _) = request(Some("ses_old"), vec![user("hi")], vec![]);
    let error = registry.run(&context(&env, Arc::default(), Arc::default()), request, CancellationToken::new()).await.unwrap_err();
    assert_eq!(error, "Claude Code 2.1.200 is too old; 2.1.248 or later is required. Run `claude update`.");
}

#[tokio::test]
async fn text_turns_continue_the_same_process() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(json!({ "turns": [[{ "thinking": "Let me answer." }, { "text": "COPPER-OTTER" }], [{ "text": "LILAC-HERON" }]] }));
    let storage = Arc::new(MemoryStorage::default());
    let reports = Arc::new(Reports::default());
    let ctx = context(&env, storage.clone(), reports.clone());
    let registry = Registry::new();
    let (first, parts) = request(Some("ses_a"), vec![user("Reply with a marker")], vec![]);
    registry.run(&ctx, first, CancellationToken::new()).await.unwrap();
    assert_eq!(text(&parts), "COPPER-OTTER");
    let kinds = kinds(&parts);
    assert!(kinds.contains(&"reasoning-delta".to_string()));
    assert_eq!(kinds.last().map(String::as_str), Some("finish"));
    let finish = parts.lock().last().cloned().unwrap();
    assert_eq!(finish["finishReason"], json!({ "unified": "stop", "raw": "end_turn" }));
    let record = storage.get("sessions/ses_a").await.unwrap();
    assert_eq!(record["interrupted"], json!(false));
    assert_eq!(record["history"].as_array().unwrap().len(), 2);

    let prompt =
        vec![user("Reply with a marker"), json!({ "role": "assistant", "content": [{ "type": "text", "text": "COPPER-OTTER" }] }), user("Another")];
    let (second, parts) = request(Some("ses_a"), prompt, vec![]);
    registry.run(&ctx, second, CancellationToken::new()).await.unwrap();
    assert_eq!(text(&parts), "LILAC-HERON");
    assert_eq!(env.spawns().len(), 1, "the second turn reuses the process");
    let users: Vec<Value> = env.received().into_iter().filter(|m| m["type"] == "user").collect();
    assert_eq!(users[1]["message"]["content"], json!([{ "type": "text", "text": "Another" }]));
    assert_eq!(users[1]["client_composed"], json!(true));
    assert_eq!(reports.0.lock().clone(), vec!["ready 2.1.302".to_string()]);
    let spawn = &env.spawns()[0];
    let argv: Vec<&str> = spawn["argv"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(argv.contains(&"--model=haiku"));
    assert!(argv.contains(&"--include-partial-messages"));
    assert!(argv.iter().any(|a| a.starts_with("--session-id=")));
    assert_eq!(spawn["env"]["CLAUDE_CODE_MAX_OUTPUT_TOKENS"], json!("4096"));
    assert_eq!(spawn["env"]["ANTHROPIC_BASE_URL"], json!("http://127.0.0.1:9"));
    let init = &env.received()[0]["request"];
    assert_eq!(
        init["appendSystemPrompt"],
        json!(
            "<runtime_info>You are running in OpenCode through the Claude Code harness. Mention this only when asked.</runtime_info>\n\n# Agent role (from OpenCode)\nBe brief.\n\nProject rules"
        )
    );
    assert_eq!(init["systemPromptSnapshot"], json!(false));
    registry.close().await;
}

#[tokio::test]
async fn tool_calls_park_the_turn_until_the_next_step() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(
        json!({ "turns": [[{ "tools": [{ "name": "read", "input": { "filePath": "a.txt" } }] }, { "text": "The marker is LEMON-IBIS." }]] }),
    );
    let ctx = context(&env, Arc::default(), Arc::default());
    let registry = Registry::new();
    let (first, parts) = request(Some("ses_t"), vec![user("Read a.txt")], vec![read_tool()]);
    registry.run(&ctx, first, CancellationToken::new()).await.unwrap();
    let call = parts.lock().iter().find(|p| p["type"] == "tool-call").cloned().unwrap();
    assert_eq!(call["toolName"], json!("read"));
    assert_eq!(call["input"], json!("{\"filePath\":\"a.txt\"}"));
    assert_eq!(parts.lock().last().unwrap()["finishReason"]["unified"], json!("tool-calls"));
    let id = call["toolCallId"].as_str().unwrap().to_string();
    let init = &env.received()[0]["request"];
    assert_eq!(init["sdkMcpServers"], json!(["opencode"]));
    assert_eq!(init["sdkMcpServerManifests"]["opencode"]["toolsListResult"]["tools"][0]["name"], json!("read"));

    let prompt = vec![
        user("Read a.txt"),
        json!({ "role": "assistant", "content": [{ "type": "tool-call", "toolCallId": id, "toolName": "read", "input": { "filePath": "a.txt" } }] }),
        json!({ "role": "tool", "content": [{ "type": "tool-result", "toolCallId": id, "toolName": "read", "output": { "type": "text", "value": "LEMON-IBIS" } }] }),
    ];
    let (second, parts) = request(Some("ses_t"), prompt, vec![read_tool()]);
    tokio::time::timeout(Duration::from_secs(20), registry.run(&ctx, second, CancellationToken::new())).await.unwrap().unwrap();
    assert_eq!(text(&parts), "The marker is LEMON-IBIS.");
    let responses: Vec<Value> = env.received().into_iter().filter(|m| m["type"] == "control_response").collect();
    let mcp = responses.iter().find_map(|r| r["response"]["response"].get("mcp_response").cloned()).unwrap();
    assert_eq!(mcp["result"], json!({ "content": [{ "type": "text", "text": "LEMON-IBIS" }], "isError": false }));
    // A continuation adds no user message.
    assert_eq!(env.received().iter().filter(|m| m["type"] == "user").count(), 1);
    registry.close().await;
}

#[tokio::test]
async fn cancelling_interrupts_and_drains() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(json!({ "turns": [[{ "text": "A very long list" }, { "hold": true }], [{ "text": "Stopped." }]] }));
    let storage = Arc::new(MemoryStorage::default());
    let ctx = context(&env, storage.clone(), Arc::default());
    let registry = Registry::new();
    let (first, parts) = request(Some("ses_c"), vec![user("Write a long list")], vec![]);
    let cancel = CancellationToken::new();
    let run = {
        let (registry, ctx, cancel) = (registry.clone(), ctx.clone(), cancel.clone());
        tokio::spawn(async move { registry.run(&ctx, first, cancel).await })
    };
    for _ in 0..200 {
        if text(&parts).contains("list") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    cancel.cancel();
    let error = tokio::time::timeout(Duration::from_secs(20), run).await.unwrap().unwrap().unwrap_err();
    assert!(sessions::is_cancelled(&error));
    assert!(env.received().iter().any(|m| m["request"]["subtype"] == "interrupt"));
    let record = storage.get("sessions/ses_c").await.unwrap();
    assert_eq!(record["interrupted"], json!(true));
    let (second, parts) = request(Some("ses_c"), vec![user("Write a long list"), user("Stop")], vec![]);
    registry.run(&ctx, second, CancellationToken::new()).await.unwrap();
    assert_eq!(text(&parts), "Stopped.");
    registry.close().await;
}

#[tokio::test]
async fn failures_become_stream_errors() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(json!({ "turns": [[
        { "assistant_error": "billing_error", "text": "Credit balance is too low" },
        { "result": { "is_error": true, "result": "Credit balance is too low" } }
    ]] }));
    let ctx = context(&env, Arc::default(), Arc::default());
    let registry = Registry::new();
    let (first, parts) = request(Some("ses_e"), vec![user("hi")], vec![]);
    registry.run(&ctx, first, CancellationToken::new()).await.unwrap();
    let error = parts.lock().iter().find(|p| p["type"] == "error").cloned().unwrap();
    assert_eq!(error["error"]["message"], json!("Credit balance is too low Check your Claude plan and extra-usage settings."));
    assert!(!kinds(&parts).contains(&"finish".to_string()));
    registry.close().await;
}

#[tokio::test]
async fn side_calls_run_in_a_throwaway_process() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(json!({ "turns": [[{ "text": "A title" }]] }));
    let ctx = context(&env, Arc::default(), Arc::default());
    let registry = Registry::new();
    let (call, parts) = request(None, vec![user("Name this")], vec![]);
    registry.run(&ctx, call, CancellationToken::new()).await.unwrap();
    assert_eq!(text(&parts), "A title");
    let argv = env.spawns()[0]["argv"].clone();
    assert!(argv.as_array().unwrap().contains(&json!("--no-session-persistence")));
    assert_eq!(env.received()[0]["request"]["systemPrompt"], json!(["Return only the requested output."]));
    // The process is stopped: interrupt, then stdin closes and it exits.
    for _ in 0..100 {
        if env.log().iter().any(|e| e["event"] == "exit") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(env.log().iter().any(|e| e["event"] == "exit"));
    assert_eq!(env.received().last().unwrap()["request"]["subtype"], json!("interrupt"));
}

#[tokio::test]
async fn startup_failures_and_disconnected_tools() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(json!({ "exit": { "code": 3, "stderr": "boom: config unreadable\n" } }));
    let ctx = context(&env, Arc::default(), Arc::default());
    let registry = Registry::new();
    let (call, _) = request(Some("ses_x"), vec![user("hi")], vec![]);
    let error = registry.run(&ctx, call, CancellationToken::new()).await.unwrap_err();
    assert_eq!(error, "Claude Code process exited with code 3. stderr: boom: config unreadable");

    let env = Env::new(json!({ "turns": [[{ "init_servers": [{ "name": "opencode", "status": "failed", "source": "sdk" }] }, { "text": "x" }]] }));
    let ctx = context(&env, Arc::default(), Arc::default());
    let (call, _) = request(Some("ses_y"), vec![user("hi")], vec![read_tool()]);
    let error = registry.run(&ctx, call, CancellationToken::new()).await.unwrap_err();
    assert_eq!(error, "The OpenCode tool server did not connect to Claude Code");

    let missing = Context { io: Arc::new(TestIo { exe: env.dir.path().join("nope").to_string_lossy().into_owned(), env: env.env() }), ..ctx };
    let (call, _) = request(Some("ses_z"), vec![user("hi")], vec![]);
    let error = registry.run(&missing, call, CancellationToken::new()).await.unwrap_err();
    assert!(error.starts_with("Claude Code native binary not found at "), "{error}");
    registry.close().await;
}

// ---------------------------------------------------------------------------------------------
// Recorded streams (upstream's MIT fixtures, `language-model/{convert,error}.test.ts`)
// ---------------------------------------------------------------------------------------------

fn recording(name: &str) -> Vec<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/claude-agent/recordings/{name}.jsonl"));
    std::fs::read_to_string(path).unwrap().lines().skip(1).map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn claude_messages(name: &str) -> Vec<Value> {
    recording(name)
        .into_iter()
        .filter(|l| l["from"] == "claude" && l["message"]["type"] != "control_response")
        .map(|l| l["message"].clone())
        .collect()
}

fn tools(names: &[&str]) -> Vec<Value> {
    names.iter().map(|n| json!({ "type": "function", "name": n, "inputSchema": { "type": "object" } })).collect()
}

/// The recorded first reply calls lookup twice.
fn first_tool_reply(offered: Vec<Value>) -> Vec<Value> {
    let mut convert = Converter::new(offered);
    let mut parts = vec![];
    for message in claude_messages("tools") {
        parts.extend(convert.convert(&message));
        if message["type"] == "stream_event" && message["event"]["type"] == "message_stop" {
            break;
        }
    }
    parts
}

#[test]
fn calls_to_tools_we_did_not_offer_are_left_to_claude_code() {
    let offered = first_tool_reply(tools(&["lookup", "confirm"]));
    assert_eq!(offered.iter().filter(|p| p["type"] == "tool-call").count(), 2);
    assert_eq!(offered.last().unwrap()["finishReason"]["unified"], json!("tool-calls"));
    let unknown = first_tool_reply(tools(&["confirm"]));
    assert!(!unknown.iter().any(|p| p["type"] == "tool-call" || p["type"] == "finish"));
}

#[test]
fn a_recorded_failed_turn_ends_in_an_error_after_its_text() {
    let mut convert = Converter::new(vec![]);
    let mut parts = vec![];
    for message in claude_messages("text-cancel") {
        parts.extend(convert.convert(&message));
        if message["type"] == "result" {
            break;
        }
    }
    assert!(parts.iter().any(|p| p["type"] == "text-delta"));
    assert_eq!(parts.last().unwrap()["type"], json!("error"));
    assert!(!parts.iter().any(|p| p["type"] == "finish"));
}

#[test]
fn a_recorded_billing_failure_is_an_error_not_answer_text() {
    let messages = claude_messages("billing-error");
    let assistant = messages.iter().find(|m| m["type"] == "assistant" && m.get("error").is_some()).unwrap();
    let result = messages.iter().find(|m| m["type"] == "result").unwrap();
    let mut convert = Converter::new(vec![]);
    assert!(convert.convert(assistant).is_empty());
    let parts = convert.convert(result);
    assert_eq!(parts.len(), 1);
    assert!(parts[0]["error"]["message"].as_str().unwrap().contains("extra usage"));
}

#[test]
fn rejected_windows_give_their_reset_date_and_allowed_ones_do_not_fail() {
    let info = json!({ "type": "rate_limit_event", "uuid": "u", "session_id": "s", "rate_limit_info": { "status": "rejected", "rateLimitType": "seven_day_sonnet", "resetsAt": 1792800000 } });
    let text_result = claude_messages("text").into_iter().find(|m| m["type"] == "result").unwrap();
    let billing = claude_messages("billing-error");
    let assistant = billing.iter().find(|m| m["type"] == "assistant" && m.get("error").is_some()).unwrap();
    let mut convert = Converter::new(vec![]);
    assert!(convert.convert(&info).is_empty());
    let mut limited = assistant.clone();
    limited["error"] = json!("rate_limit");
    assert!(convert.convert(&limited).is_empty());
    let mut failed = text_result.clone();
    failed["subtype"] = json!("success");
    failed["is_error"] = json!(true);
    failed["result"] = json!("Weekly limit reached.");
    let parts = convert.convert(&failed);
    assert!(parts[0]["error"]["message"].as_str().unwrap().contains("2026-10-24T00:00:00.000Z"));
    for status in ["allowed", "allowed_warning", "rejected"] {
        let mut ok = Converter::new(vec![]);
        let mut event = info.clone();
        event["rate_limit_info"]["status"] = json!(status);
        event["rate_limit_info"]["overageStatus"] = json!("allowed");
        ok.convert(&event);
        assert_eq!(ok.convert(&text_result).last().unwrap()["type"], json!("finish"));
    }
    let mut recovered = Converter::new(vec![]);
    recovered.convert(&info);
    recovered.convert(&json!({ "type": "rate_limit_event", "uuid": "u", "session_id": "s", "rate_limit_info": { "status": "allowed", "overageStatus": "rejected" } }));
    let mut later = failed.clone();
    later["api_error_status"] = json!(429);
    let parts = recovered.convert(&later);
    assert!(!parts[0]["error"]["message"].as_str().unwrap().contains("Resets at"));
}

#[test]
fn empty_successes_after_api_retries_report_the_terminal_status() {
    let text_result = claude_messages("text").into_iter().find(|m| m["type"] == "result").unwrap();
    for status in [500, 529] {
        let mut result = text_result.clone();
        result["subtype"] = json!("success");
        result["is_error"] = json!(false);
        result["result"] = json!("");
        result["api_error_status"] = json!(status);
        let parts = Converter::new(vec![]).convert(&result);
        let message = parts[0]["error"]["message"].as_str().unwrap().to_string();
        assert!(message.contains(&status.to_string()) && message.contains("retries"), "{message}");
        assert!(!parts.iter().any(|p| p["type"] == "finish"));
    }
    let mut empty = text_result.clone();
    empty["result"] = json!("");
    empty["is_error"] = json!(false);
    empty["api_error_status"] = Value::Null;
    assert_eq!(Converter::new(vec![]).convert(&empty).last().unwrap()["type"], json!("finish"));
}
