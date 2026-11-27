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

/// Builds `claude.exe` on Windows, which runs no script as a program: distlib's console-script launcher (the one pip
/// puts in `Scripts`) followed by the stand-in as a zip application. It runs the script with this Python and takes
/// the Python process down with it when it is terminated.
#[cfg(windows)]
const LAUNCHER: &str = r##"
import io, os, platform, sys, zipfile
from pip._vendor import distlib
name = "t64-arm.exe" if platform.machine().lower() in ("arm64", "aarch64") else "t64.exe"
with open(os.path.join(os.path.dirname(distlib.__file__), name), "rb") as f:
    launcher = f.read()
python = '"%s"' % sys.executable if " " in sys.executable else sys.executable
archive = io.BytesIO()
with zipfile.ZipFile(archive, "w") as z:
    z.write(sys.argv[1], "__main__.py")
with open(sys.argv[2], "wb") as f:
    f.write(launcher + b"#!" + python.encode() + b"\r\n" + archive.getvalue())
"##;

/// The Python 3 that runs the stand-in (`python3`; on Windows also `python`, with pip for the launcher).
fn python() -> Option<&'static str> {
    static PYTHON: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *PYTHON.get_or_init(|| {
        let check = if cfg!(windows) { "import sys, pip._vendor.distlib; assert sys.version_info[0] == 3" } else { "import sys; assert sys.version_info[0] == 3" };
        let candidates: &[&'static str] = if cfg!(windows) { &["python", "python3"] } else { &["python3"] };
        candidates.iter().copied().find(|python| std::process::Command::new(python).args(["-c", check]).output().is_ok_and(|o| o.status.success()))
    })
}

fn standin() -> Option<PathBuf> {
    if python().is_none() {
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
        // A link to the (executable) script, not a copy: a file just written can still be open in a child another test
        // thread is forking, and executing it then fails with ETXTBSY.
        #[cfg(unix)]
        let script = {
            let script = dir.path().join("claude");
            std::os::unix::fs::symlink(standin().unwrap(), &script).unwrap();
            script
        };
        #[cfg(windows)]
        let script = {
            let script = dir.path().join("claude.exe");
            let status = std::process::Command::new(python().unwrap())
                .args(["-c", LAUNCHER])
                .arg(standin().unwrap())
                .arg(&script)
                .status()
                .unwrap();
            assert!(status.success(), "building the stand-in's launcher failed");
            script
        };
        std::fs::write(dir.path().join("scenario.json"), scenario.to_string()).unwrap();
        Env { dir, script }
    }

    fn env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
        env.insert("HOME".into(), self.dir.path().join("home").to_string_lossy().into_owned());
        // Python finds `~` through USERPROFILE on Windows, and needs SystemRoot to start up.
        if cfg!(windows) {
            env.insert("USERPROFILE".into(), self.dir.path().join("home").to_string_lossy().into_owned());
            env.insert("SystemRoot".into(), std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()));
        }
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

/// The transcript operations: forks are made up (a fresh session ID), and every call is logged with its config folder.
#[derive(Default)]
struct Transcripts {
    forks: Vec<(String, String, Option<String>)>,
    removed: Vec<(String, Option<String>)>,
}

struct TestIo {
    exe: String,
    env: BTreeMap<String, String>,
    transcripts: Arc<Mutex<Transcripts>>,
}

#[async_trait]
impl Io for TestIo {
    fn which(&self) -> Option<String> {
        Some(self.exe.clone())
    }
    fn env(&self) -> BTreeMap<String, String> {
        self.env.clone()
    }
    async fn session_exists(&self, session_id: &str, config_dir: Option<&str>) -> Result<bool, String> {
        // The stand-in writes its transcripts where Claude Code would.
        let dir = config_dir.map(PathBuf::from).unwrap_or_else(|| PathBuf::from(&self.env["HOME"]).join(".claude"));
        Ok(oc_core::claude_agent::transcripts::transcript_in(&dir, session_id))
    }
    async fn fork(&self, session_id: &str, message_id: &str, config_dir: Option<&str>) -> Result<Option<Fork>, String> {
        self.transcripts.lock().forks.push((session_id.into(), message_id.into(), config_dir.map(String::from)));
        Ok(Some(Fork { session_id: uuid::Uuid::new_v4().to_string(), messages: HashMap::new() }))
    }
    async fn remove(&self, session_id: &str, config_dir: Option<&str>) -> Result<(), String> {
        let mut transcripts = self.transcripts.lock();
        // A fork can only be deleted from the config folder it was written to.
        if transcripts.forks.iter().any(|(_, _, dir)| dir.as_deref() != config_dir) {
            return Err(format!("removed {session_id} from the wrong config folder"));
        }
        transcripts.removed.push((session_id.into(), config_dir.map(String::from)));
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
    async fn remove(&self, key: &str) {
        self.0.lock().remove(key);
    }
    async fn scan(&self, prefix: &str) -> Vec<String> {
        let mut keys: Vec<String> = self.0.lock().keys().filter(|k| k.starts_with(prefix)).cloned().collect();
        keys.sort();
        keys
    }
}

#[derive(Default)]
struct Reports(Mutex<Vec<String>>);

impl Reporter for Reports {
    fn missing(&self) {
        self.0.lock().push("missing".into());
    }
    fn ready(&self, probe: ProbeResult, config_dir: Option<String>) {
        let folder = config_dir.map(|d| format!(" {d}")).unwrap_or_default();
        self.0.lock().push(format!("ready {}{folder}", probe.version.unwrap_or_default()));
    }
}

fn context(env: &Env, storage: Arc<MemoryStorage>, reports: Arc<Reports>) -> Context {
    context_with(env, storage, reports, Arc::default())
}

fn context_with(env: &Env, storage: Arc<MemoryStorage>, reports: Arc<Reports>, transcripts: Arc<Mutex<Transcripts>>) -> Context {
    Context {
        io: Arc::new(TestIo { exe: env.script.to_string_lossy().into_owned(), env: env.env(), transcripts }),
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
        model: "haiku".into(),
        effort: None,
        max_output_tokens: Some(4096.0),
        connection: vec![("ANTHROPIC_BASE_URL".into(), "http://127.0.0.1:9".into())],
        system,
        prompt,
        tools,
        emit: Box::new(move |message: &Value| sink.lock().extend(convert.convert(message))),
        holding: None,
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
        web_search: false,
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
        // 2.3.0: earlier thinking is dropped when the prompt or tools change mid-session.
        ("CLAUDE_CODE_POLISHED_DEWDROP", "drop"),
        ("CLAUDE_CODE_ENTRYPOINT", "opencode"),
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
        web_search: false,
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
    assert_eq!(error["error"]["message"], json!("Credit balance is too low"));
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

    let missing = Context {
        io: Arc::new(TestIo { exe: env.dir.path().join("nope").to_string_lossy().into_owned(), env: env.env(), transcripts: Arc::default() }),
        ..ctx
    };
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
fn a_recorded_api_error_is_an_error_not_answer_text() {
    let messages = claude_messages("api-error");
    let assistant = messages.iter().find(|m| m["type"] == "assistant" && m.get("error").is_some()).unwrap();
    let result = messages.iter().find(|m| m["type"] == "result").unwrap();
    let mut convert = Converter::new(vec![]);
    assert!(convert.convert(assistant).is_empty());
    let parts = convert.convert(result);
    assert_eq!(parts.len(), 1);
    assert!(parts[0]["error"]["message"].as_str().unwrap().contains("claude-does-not-exist"));
}

#[test]
fn rejected_windows_give_their_reset_date_and_allowed_ones_do_not_fail() {
    let info = json!({ "type": "rate_limit_event", "uuid": "u", "session_id": "s", "rate_limit_info": { "status": "rejected", "rateLimitType": "seven_day_sonnet", "resetsAt": 1792800000 } });
    let text_result = claude_messages("text").into_iter().find(|m| m["type"] == "result").unwrap();
    let recorded = claude_messages("api-error");
    let assistant = recorded.iter().find(|m| m["type"] == "assistant" && m.get("error").is_some()).unwrap();
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

// ---------------------------------------------------------------------------------------------
// 2.4.1: config folders, account rotation, deleted sessions and the web search relay
// (`claude/{accounts,sessions,branches}.test.ts`, `plugin.test.ts`, `websearch.test.ts`)
// ---------------------------------------------------------------------------------------------

use oc_core::claude_agent::{accounts, config_dirs, websearch};

const FIRST: &str = "Remember the marker COPPER-OTTER. Reply only with that marker.";
const RESETS: &str = "2026-10-24T00:00:00.000Z";

fn followup(text: &str) -> Vec<Value> {
    vec![user(FIRST), json!({ "role": "assistant", "content": [{ "type": "text", "text": "COPPER-OTTER" }] }), user(text)]
}

/// A request with this connection (no gateway: every folder logs in on its own).
fn request_in(session: Option<&str>, prompt: Vec<Value>, connection: Vec<(String, String)>) -> (Request, Parts) {
    let (mut request, parts) = request(session, prompt, vec![]);
    request.connection = connection;
    (request, parts)
}

fn folder(env: &Env, name: &str) -> String {
    config_dirs(Some(&json!(env.dir.path().join(name).to_string_lossy()))).remove(0)
}

fn in_folder(dir: &str) -> Vec<(String, String)> {
    vec![("CLAUDE_CONFIG_DIR".into(), dir.into())]
}

/// The config folder of every `claude` started, in order.
fn folders(env: &Env) -> Vec<Option<String>> {
    env.spawns().iter().map(|s| s["env"]["CLAUDE_CONFIG_DIR"].as_str().map(String::from)).collect()
}

fn errors(parts: &Parts) -> Vec<String> {
    parts.lock().iter().filter(|p| p["type"] == "error").map(|p| p["error"]["message"].as_str().unwrap_or_default().to_string()).collect()
}

/// The provider's folder list, through the account rotation (what a model of the provider runs).
async fn rotating(
    registry: &Arc<Registry>,
    ctx: &Context,
    dirs: &[String],
    session: Option<&str>,
    prompt: Vec<Value>,
) -> (Result<(), String>, Parts) {
    let (request, parts) = request_in(session, prompt, in_folder(&dirs[0]));
    (accounts::run(registry, ctx, request, dirs, CancellationToken::new()).await, parts)
}

fn limit() -> Value {
    json!([{ "limit": {} }])
}

fn text_turn() -> Value {
    json!([{ "text": "COPPER-OTTER" }])
}

#[tokio::test]
async fn a_refused_chat_retries_unseen_on_the_next_account_and_stays_there() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(json!({ "turns": [limit(), text_turn(), text_turn()] }));
    let dirs = vec![folder(&env, "a"), folder(&env, "b"), folder(&env, "c")];
    let (storage, reports) = (Arc::new(MemoryStorage::default()), Arc::new(Reports::default()));
    let ctx = context(&env, storage.clone(), reports.clone());
    let registry = Registry::new();
    let (result, one) = rotating(&registry, &ctx, &dirs, Some("session-a"), vec![user(FIRST)]).await;
    result.unwrap();
    assert_eq!(text(&one), "COPPER-OTTER");
    assert!(errors(&one).is_empty());
    let (result, two) = rotating(&registry, &ctx, &dirs, Some("session-a"), followup("What was the marker? Reply only with it.")).await;
    result.unwrap();
    assert_eq!(text(&two), "COPPER-OTTER");
    assert_eq!(folders(&env), [Some(dirs[0].clone()), Some(dirs[1].clone())]);
    assert_eq!(storage.get("accounts/session-a").await, Some(json!(dirs[1])));
    assert_eq!(storage.get("last-account").await, Some(json!(dirs[1])));
    assert_eq!(reports.0.lock().last().cloned(), Some(format!("ready 2.1.302 {}", dirs[1])));
    // Nothing of the refused attempt reached OpenCode, so it did not cancel the turn: the first account's record
    // settled, and a later switch back catches up from the end of that turn.
    let refused = storage.get(&format!("sessions/session-a:{}", dirs[0])).await.unwrap();
    assert_eq!(refused["interrupted"], json!(false));
    registry.close().await;
}

#[tokio::test]
async fn a_new_chat_starts_on_the_account_that_last_worked() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(json!({ "turns": [limit(), text_turn(), text_turn()] }));
    let dirs = vec![folder(&env, "a"), folder(&env, "b"), folder(&env, "c")];
    let storage = Arc::new(MemoryStorage::default());
    let ctx = context(&env, storage.clone(), Arc::default());
    let old = Registry::new();
    rotating(&old, &ctx, &dirs, Some("session-a"), vec![user(FIRST)]).await.0.unwrap();
    assert_eq!(storage.get("last-account").await, Some(json!(dirs[1])));
    // After a restart, a new chat skips the refused account.
    old.close().await;
    let registry = Registry::new();
    let (result, parts) = rotating(&registry, &ctx, &dirs, Some("session-b"), vec![user(FIRST)]).await;
    result.unwrap();
    assert_eq!(text(&parts), "COPPER-OTTER");
    assert_eq!(folders(&env), [Some(dirs[0].clone()), Some(dirs[1].clone()), Some(dirs[1].clone())]);
    assert_eq!(storage.get("accounts/session-b").await, Some(json!(dirs[1])));
    registry.close().await;
}

#[tokio::test]
async fn a_side_call_starts_on_its_chats_account_and_can_rotate_without_moving_the_chat() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(json!({ "turns": [text_turn(), limit(), [{ "text": "Summary." }]] }));
    let dirs = vec![folder(&env, "a"), folder(&env, "b"), folder(&env, "c")];
    let storage = Arc::new(MemoryStorage::default());
    let transcripts: Arc<Mutex<Transcripts>> = Arc::default();
    let ctx = context_with(&env, storage.clone(), Arc::default(), transcripts.clone());
    let registry = Registry::new();
    rotating(&registry, &ctx, &dirs, Some("session-a"), vec![user(FIRST)]).await.0.unwrap();
    // A compaction: no session of its own, the chat as its source.
    let (mut request, summary) = request_in(None, followup("Summarize"), in_folder(&dirs[0]));
    request.source_id = Some("session-a".into());
    accounts::run(&registry, &ctx, request, &dirs, CancellationToken::new()).await.unwrap();
    assert!(errors(&summary).is_empty(), "{:?}", errors(&summary));
    assert_eq!(text(&summary), "Summary.");
    let forks: Vec<Option<String>> = transcripts.lock().forks.iter().map(|f| f.2.clone()).collect();
    assert_eq!(forks, [Some(dirs[0].clone())]);
    assert_eq!(folders(&env), [Some(dirs[0].clone()), Some(dirs[0].clone()), Some(dirs[1].clone())]);
    assert_eq!(storage.get("accounts/session-a").await, Some(json!(dirs[0])));
    assert_eq!(storage.get("last-account").await, Some(json!(dirs[1])));
    registry.close().await;
}

#[tokio::test]
async fn a_limit_after_output_is_shown_as_an_error_and_the_next_turn_moves() {
    if standin().is_none() {
        return;
    }
    let after_text = json!([
        { "text": "COPPER-OTTER" },
        { "assistant_error": "rate_limit", "text": "" },
        { "result": { "is_error": true, "result": "You've hit your limit", "api_error_status": 429 } },
    ]);
    let env = Env::new(json!({ "turns": [after_text, limit(), text_turn()] }));
    let dirs = vec![folder(&env, "a"), folder(&env, "b"), folder(&env, "c")];
    let storage = Arc::new(MemoryStorage::default());
    let ctx = context(&env, storage.clone(), Arc::default());
    let registry = Registry::new();
    let (result, one) = rotating(&registry, &ctx, &dirs, Some("session-a"), vec![user(FIRST)]).await;
    result.unwrap();
    assert_eq!(text(&one), "COPPER-OTTER");
    assert_eq!(one.lock().last().unwrap()["type"], json!("error"));
    assert!(storage.get("accounts/session-a").await.is_none());
    // The next turn tries the first account again, which refuses unseen, then moves on.
    let (result, two) = rotating(&registry, &ctx, &dirs, Some("session-a"), followup("What was the marker? Reply only with it.")).await;
    result.unwrap();
    assert!(errors(&two).is_empty());
    assert_eq!(folders(&env), [Some(dirs[0].clone()), Some(dirs[1].clone())]);
    assert_eq!(storage.get("accounts/session-a").await, Some(json!(dirs[1])));
    registry.close().await;
}

#[tokio::test]
async fn when_every_account_refuses_the_chat_shows_one_error_and_keeps_its_account() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(json!({ "turns": [text_turn(), limit(), limit(), text_turn()] }));
    let dirs = vec![folder(&env, "a"), folder(&env, "b")];
    let storage = Arc::new(MemoryStorage::default());
    let ctx = context(&env, storage.clone(), Arc::default());
    let registry = Registry::new();
    rotating(&registry, &ctx, &dirs, Some("session-a"), vec![user(FIRST)]).await.0.unwrap();
    // Like a model whose own limit is used up on every account.
    let (result, refused) = rotating(&registry, &ctx, &dirs, Some("session-a"), followup("Use the capped model")).await;
    result.unwrap();
    let errors = errors(&refused);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains(RESETS), "{}", errors[0]);
    assert_eq!(storage.get("accounts/session-a").await, Some(json!(dirs[0])));
    assert_eq!(storage.get("last-account").await, Some(json!(dirs[0])));
    // Like switching to a model that still has room: it starts on the chat's own account and works.
    let prompt = [followup("Use the capped model"), vec![user("What was the marker? Reply only with it.")]].concat();
    let (result, two) = rotating(&registry, &ctx, &dirs, Some("session-a"), prompt).await;
    result.unwrap();
    assert_eq!(text(&two), "COPPER-OTTER");
    assert_eq!(folders(&env), [Some(dirs[0].clone()), Some(dirs[1].clone()), Some(dirs[0].clone())]);
    registry.close().await;
}

#[tokio::test]
async fn a_failure_that_is_not_a_limit_is_shown_without_moving_the_chat() {
    if standin().is_none() {
        return;
    }
    let failure = json!([
        { "assistant_error": "invalid_request", "text": "There's an issue with the selected model (claude-does-not-exist)." },
        { "result": { "is_error": true, "result": "There's an issue with the selected model (claude-does-not-exist)." } },
    ]);
    let env = Env::new(json!({ "turns": [limit(), failure] }));
    let dirs = vec![folder(&env, "a"), folder(&env, "b"), folder(&env, "c")];
    let storage = Arc::new(MemoryStorage::default());
    let ctx = context(&env, storage.clone(), Arc::default());
    let registry = Registry::new();
    let (result, parts) = rotating(&registry, &ctx, &dirs, Some("session-a"), vec![user(FIRST)]).await;
    result.unwrap();
    assert!(errors(&parts)[0].contains("claude-does-not-exist"));
    assert_eq!(folders(&env), [Some(dirs[0].clone()), Some(dirs[1].clone())]);
    assert!(storage.get("accounts/session-a").await.is_none());
    assert!(storage.get("last-account").await.is_none());
    registry.close().await;
}

#[tokio::test]
async fn a_logged_out_account_fails_with_the_command_to_log_in_to_its_folder() {
    if standin().is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let a = config_dirs(Some(&json!(dir.path().join("a").to_string_lossy()))).remove(0);
    let env = Env::new(json!({ "folders": { a.clone(): { "account": { "tokenSource": "none", "apiProvider": "firstParty" } } } }));
    let dirs = vec![a.clone(), folder(&env, "b")];
    let storage = Arc::new(MemoryStorage::default());
    let ctx = context(&env, storage.clone(), Arc::default());
    let registry = Registry::new();
    let (result, _) = rotating(&registry, &ctx, &dirs, Some("session-a"), vec![user(FIRST)]).await;
    let error = result.unwrap_err();
    assert!(error.contains(&format!("\"{a}\"")), "{error}");
    assert_eq!(folders(&env), [Some(a)]);
    assert!(storage.get("accounts/session-a").await.is_none());
    // No prompt was sent.
    assert!(!env.received().iter().any(|m| m["type"] == "user"));
    registry.close().await;
}

#[tokio::test]
async fn a_model_with_one_folder_or_an_api_key_never_rotates() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(json!({ "turns": [limit(), limit()] }));
    let dirs = vec![folder(&env, "a"), folder(&env, "b"), folder(&env, "c")];
    let storage = Arc::new(MemoryStorage::default());
    let ctx = context(&env, storage.clone(), Arc::default());
    let registry = Registry::new();
    let work = folder(&env, "work");
    let (result, parts) = rotating(&registry, &ctx, std::slice::from_ref(&work), Some("session-a"), vec![user(FIRST)]).await;
    result.unwrap();
    assert!(errors(&parts)[0].contains("Resets at"));
    let (mut keyed, parts) = request_in(Some("session-key"), vec![user(FIRST)], in_folder(&dirs[0]));
    keyed.connection.push(("ANTHROPIC_API_KEY".into(), "sk-test".into()));
    accounts::run(&registry, &ctx, keyed, &dirs, CancellationToken::new()).await.unwrap();
    assert_eq!(errors(&parts).len(), 1);
    assert_eq!(folders(&env), [Some(work), Some(dirs[0].clone())]);
    assert!(storage.0.lock().keys().all(|k| !k.starts_with("accounts/") && k != "last-account"));
    registry.close().await;
}

#[tokio::test]
async fn a_new_fork_starts_on_its_parents_account() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(json!({ "turns": [limit(), text_turn(), [{ "text": "FORKED" }]] }));
    let dirs = vec![folder(&env, "a"), folder(&env, "b"), folder(&env, "c")];
    let storage = Arc::new(MemoryStorage::default());
    let transcripts: Arc<Mutex<Transcripts>> = Arc::default();
    let ctx = context_with(&env, storage.clone(), Arc::default(), transcripts.clone());
    let old = Registry::new();
    rotating(&old, &ctx, &dirs, Some("session-a"), vec![user(FIRST)]).await.0.unwrap();
    assert_eq!(storage.get("accounts/session-a").await, Some(json!(dirs[1])));
    // Move the newest account away, so only the parent's saved account can steer the fork there.
    storage.set("last-account", json!(dirs[2])).await;
    old.close().await;
    let registry = Registry::new();
    let (mut request, _) = request_in(Some("child-a"), followup("A forked question"), in_folder(&dirs[0]));
    request.source_id = Some("session-a".into());
    accounts::run(&registry, &ctx, request, &dirs, CancellationToken::new()).await.unwrap();
    let forks: Vec<Option<String>> = transcripts.lock().forks.iter().map(|f| f.2.clone()).collect();
    assert_eq!(forks, [Some(dirs[1].clone())]);
    assert_eq!(folders(&env).last().cloned().flatten(), Some(dirs[1].clone()));
    assert_eq!(storage.get("accounts/child-a").await, Some(json!(dirs[1])));
    registry.close().await;
}

#[tokio::test]
async fn each_config_folder_keeps_its_own_claude_session_and_switching_back_catches_up() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(json!({ "turns": [text_turn(), text_turn(), text_turn()] }));
    let storage = Arc::new(MemoryStorage::default());
    let ctx = context(&env, storage.clone(), Arc::default());
    let registry = Registry::new();
    let work = folder(&env, "claude-work");
    let (one, _) = request_in(Some("session-a"), vec![user(FIRST)], vec![]);
    registry.run(&ctx, one, CancellationToken::new()).await.unwrap();
    let flag = |run: usize, name: &str| {
        env.spawns()[run]["argv"].as_array().unwrap().iter().filter_map(Value::as_str).find_map(|a| a.strip_prefix(name).map(String::from))
    };
    let id = flag(0, "--session-id=").unwrap();
    let mut prompt = followup("What was the marker? Reply only with it.");
    let (two, _) = request_in(Some("session-a"), prompt.clone(), in_folder(&work));
    registry.run(&ctx, two, CancellationToken::new()).await.unwrap();
    prompt.push(json!({ "role": "assistant", "content": [{ "type": "text", "text": "COPPER-OTTER" }] }));
    prompt.push(user("And once more?"));
    let (three, _) = request_in(Some("session-a"), prompt, vec![]);
    registry.run(&ctx, three, CancellationToken::new()).await.unwrap();
    let users: Vec<String> = env.received().into_iter().filter(|m| m["type"] == "user").map(|m| m["message"]["content"].to_string()).collect();
    // The other folder cannot resume this login's transcript, so it starts its own from the whole history.
    assert_eq!(folders(&env), [None, Some(work.clone()), None]);
    assert!(flag(1, "--resume=").is_none());
    assert!(users[1].contains("Remember the marker COPPER-OTTER"));
    let other = storage.get(&format!("sessions/session-a:{work}")).await.unwrap();
    assert_ne!(other["claudeSessionID"], json!(id));
    assert_eq!(flag(2, "--resume=").as_deref(), Some(id.as_str()));
    assert!(users[2].contains("What was the marker?"));
    assert!(!users[2].contains("Remember the marker COPPER-OTTER"));
    assert_eq!(storage.get("sessions/session-a").await.unwrap()["claudeSessionID"], json!(id));
    registry.close().await;
}

#[tokio::test]
async fn a_logged_out_model_folder_fails_with_the_command_to_log_in_to_it() {
    if standin().is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work = config_dirs(Some(&json!(dir.path().join("claude-work").to_string_lossy()))).remove(0);
    let env = Env::new(json!({ "folders": { work.clone(): { "account": { "tokenSource": "none", "apiProvider": "firstParty" } } } }));
    let reports = Arc::new(Reports::default());
    let ctx = context(&env, Arc::default(), reports.clone());
    let registry = Registry::new();
    let (request, _) = request_in(Some("session-a"), vec![user(FIRST)], in_folder(&work));
    let error = registry.run(&ctx, request, CancellationToken::new()).await.unwrap_err();
    assert_eq!(error, oc_core::claude_agent::login_message(Some(&work)));
    assert!(error.contains(&format!("\"{work}\"")));
    // The report names the folder, so the provider's status can tell it is another account.
    assert_eq!(reports.0.lock().clone(), [format!("ready 2.1.302 {work}")]);
    registry.close().await;
}

#[tokio::test]
async fn a_branch_is_forked_and_deleted_in_the_chats_own_config_folder() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(json!({ "turns": [text_turn(), [{ "text": "Summary." }]] }));
    let transcripts: Arc<Mutex<Transcripts>> = Arc::default();
    let ctx = context_with(&env, Arc::default(), Arc::default(), transcripts.clone());
    let registry = Registry::new();
    let work = folder(&env, "claude-work");
    let (one, _) = request_in(Some("session-a"), vec![user(FIRST)], in_folder(&work));
    registry.run(&ctx, one, CancellationToken::new()).await.unwrap();
    let (mut summary, _) = request_in(None, followup("Summarize"), in_folder(&work));
    summary.source_id = Some("session-a".into());
    registry.run(&ctx, summary, CancellationToken::new()).await.unwrap();
    let transcripts = transcripts.lock();
    assert_eq!(transcripts.forks.len(), 1);
    assert_eq!(transcripts.forks[0].2.as_deref(), Some(work.as_str()));
    // The double refuses a removal from another folder.
    assert_eq!(transcripts.removed.len(), 1);
    assert_eq!(transcripts.removed[0].1.as_deref(), Some(work.as_str()));
    drop(transcripts);
    registry.close().await;
}

// plugin.test.ts "deleting a session stops its process and removes what was saved for it" (2.4.1)
#[tokio::test]
async fn deleting_a_session_stops_its_process_and_removes_its_records() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(json!({ "turns": [text_turn()] }));
    let storage = Arc::new(MemoryStorage::default());
    let ctx = context(&env, storage.clone(), Arc::default());
    let registry = Registry::new();
    let (one, _) = request_in(Some("session-a"), vec![user(FIRST)], vec![]);
    registry.run(&ctx, one, CancellationToken::new()).await.unwrap();
    storage.set("sessions/session-a:/claude-work", json!({ "claudeSessionID": "other-folder" })).await;
    storage.set("sessions/session-ab", json!({ "claudeSessionID": "another-chat" })).await;
    storage.set("models", json!({ "models": [] })).await;
    registry.remove(storage.as_ref(), "session-a").await;
    for _ in 0..100 {
        if env.log().iter().any(|e| e["event"] == "exit") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(env.log().iter().any(|e| e["event"] == "exit"));
    let mut keys: Vec<String> = storage.0.lock().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["models", "sessions/session-ab"]);
    // A new turn starts over: nothing resumes the deleted session.
    registry.close().await;
}

#[tokio::test]
async fn the_default_folder_runs_without_naming_it() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(json!({ "turns": [text_turn()] }));
    let ctx = context(&env, Arc::default(), Arc::default());
    let registry = Registry::new();
    let default = oc_core::claude_agent::config_dirs(Some(&json!("~/.claude"))).remove(0);
    // A side call: it saves no transcript, so the stand-in writes nothing to the real default folder either way.
    let (one, parts) = request_in(None, vec![user(FIRST)], in_folder(&default));
    registry.run(&ctx, one, CancellationToken::new()).await.unwrap();
    assert_eq!(text(&parts), "COPPER-OTTER");
    assert_eq!(folders(&env), [None]);
    registry.close().await;
}

// websearch.test.ts "searches in a claude with only WebSearch" (2.4.1), against the recorded exchange
#[tokio::test]
async fn the_web_search_relay_runs_claude_with_only_websearch() {
    if standin().is_none() {
        return;
    }
    let lines = recording("websearch");
    let output = lines.iter().find_map(|l| l["message"].get("tool_use_result").cloned()).unwrap();
    let env = Env::new(json!({ "turns": [[{ "websearch": output }, { "text": "Node.js moves to one release a year." }]] }));
    let connection = vec![("ANTHROPIC_BASE_URL".to_string(), "https://gateway.example".to_string())];
    let options = websearch::options(env.script.to_string_lossy().into_owned(), None, env.env(), &connection);
    let found = websearch::search(options, "Node.js release schedule").await.unwrap();
    assert!(found[0].title.as_deref().unwrap().starts_with("Summary of all results below, written by Claude"));
    assert!(found[0].content.as_deref().unwrap().contains("Node.js"));
    assert!(found.len() > 1);
    let spawn = &env.spawns()[0];
    let recorded_spawn = lines.iter().find(|l| l["from"] == "spawn").unwrap();
    assert_eq!(spawn["argv"], recorded_spawn["args"]);
    assert_eq!(spawn["env"]["ANTHROPIC_BASE_URL"], json!("https://gateway.example"));
    // Compared as real paths: the temp folder can be a link (macOS) or a short name (Windows).
    let real = |p: PathBuf| p.canonicalize().unwrap_or(p);
    assert_eq!(spawn["cwd"].as_str().map(PathBuf::from).map(real), Some(real(std::env::temp_dir())));
    // The initialize request and the prompt, as the SDK writes them.
    let sent: Vec<Value> = lines.iter().filter(|l| l["from"] == "sdk").map(|l| l["message"].clone()).collect();
    let received = env.received();
    assert_eq!(received[0]["request"], sent[0]["request"]);
    assert_eq!(received[1], sent[1]);
    // The process is closed as soon as the results arrive.
    for _ in 0..200 {
        if env.log().iter().any(|e| e["event"] == "exit") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(env.log().iter().any(|e| e["event"] == "exit"));
}

#[tokio::test]
async fn the_web_search_relay_reports_refusals_and_replies_without_a_search() {
    if standin().is_none() {
        return;
    }
    let env = Env::new(json!({ "turns": [limit(), [{ "text": "I know this one." }]] }));
    let search = |env: &Env| websearch::search(websearch::options(env.script.to_string_lossy().into_owned(), None, env.env(), &[]), "q");
    let error = search(&env).await.unwrap_err();
    assert!(error.contains("Resets at"), "{error}");
    let error = search(&env).await.unwrap_err();
    assert_eq!(error, "Claude finished without searching the web");
}

#[test]
fn the_recorded_web_search_gives_claudes_summary_and_links() {
    let found = claude_messages("websearch").iter().find_map(websearch::results).unwrap();
    assert!(found[0].title.as_deref().unwrap().starts_with("Summary of all results below, written by Claude (top source: Evolving"));
    assert!(found[0].content.as_deref().unwrap().contains("Node.js"));
    assert_eq!(found.len(), 13);
    assert!(found[1..].iter().all(|r| r.content.is_none()));
}
