//! The `claude` process and its stdio protocol: a native port of the parts of
//! `@anthropic-ai/claude-agent-sdk` 0.3.301 that opencode uses (`ProcessTransport` and `Query`).
//!
//! - Spawn: the command line and environment `query()` builds (see [`args`] and [`final_env`]).
//! - NDJSON both ways. Our control requests (`initialize` first, then `interrupt`, `set_model`,
//!   `apply_flag_settings`, `get_context_usage`) are matched to `control_response`s by id.
//! - Claude's control requests: `hook_callback` (PreToolUse/PostToolUse → [`PendingTools`]) and
//!   `mcp_message` for the in-process `opencode` MCP server ([`ToolServer`]); anything else gets the
//!   SDK's error answer.
//! - Messages other than control traffic are delivered in order; `keep_alive`, `transcript_mirror`
//!   and `sdk_host_only` state changes are consumed.
//! - Close: end stdin, then SIGTERM after 2 s and SIGKILL 5 s later if the process is still alive (on Windows both
//!   terminate it, as Node does, and its exit reports the signal).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};

use super::convert::{self, ToolResult};
use super::js;

/// The SDK version opencode bundles (`CLAUDE_AGENT_SDK_VERSION`).
pub const SDK_VERSION: &str = "0.3.301";
const STDERR_TAIL: usize = 2048;
/// `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS` default: how long a finished run waits for `idle`.
const RUN_END_CEILING: Duration = Duration::from_millis(600_000);
const MCP_PROTOCOL: &str = "2025-11-25";
const MCP_SUPPORTED: [&str; 5] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05", "2024-10-07"];

/// `Math.random().toString(36).substring(2, 15)`.
fn request_id() -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let len = 10 + rand::random::<u8>() as usize % 3;
    (0..len).map(|_| DIGITS[rand::random::<u8>() as usize % 36] as char).collect()
}

/// `Te(value)`: the SDK's truthy environment check.
pub fn env_truthy(value: Option<&str>) -> bool {
    value.is_some_and(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

// ---------------------------------------------------------------------------------------------
// Pending tool calls (`tools/pending.ts`)
// ---------------------------------------------------------------------------------------------

struct Call {
    name: String,
    result: watch::Sender<Option<ToolResult>>,
}

#[derive(Default)]
struct PendingInner {
    calls: HashMap<String, Call>,
    replacements: HashMap<String, Vec<Value>>,
    next: Option<String>,
}

/// Claude's calls of OpenCode tools: announced by the assistant message and the PreToolUse hook,
/// executed by `tools/call` (which waits for OpenCode's result from the next step), and finished by
/// the PostToolUse hook (which can replace the result with content blocks).
#[derive(Default)]
pub struct PendingTools {
    inner: Mutex<PendingInner>,
}

impl PendingTools {
    pub fn new() -> Arc<PendingTools> {
        Arc::new(PendingTools::default())
    }

    fn expect_locked(inner: &mut PendingInner, id: &str, name: &str) -> Result<watch::Receiver<Option<ToolResult>>, String> {
        let call = inner.calls.entry(id.to_string()).or_insert_with(|| Call { name: name.to_string(), result: watch::channel(None).0 });
        if call.name != name {
            return Err(format!("Mismatched Claude tool call {id}"));
        }
        Ok(call.result.subscribe())
    }

    pub fn expect(&self, id: &str, name: &str) -> Result<(), String> {
        Self::expect_locked(&mut self.inner.lock(), id, name).map(|_| ())
    }

    pub fn before(&self, id: &str, name: &str) -> Result<(), String> {
        let mut inner = self.inner.lock();
        Self::expect_locked(&mut inner, id, name)?;
        inner.next = Some(id.to_string());
        Ok(())
    }

    pub async fn call(&self, name: &str) -> Result<Value, String> {
        let (id, mut result) = {
            let mut inner = self.inner.lock();
            let Some(id) = inner.next.take() else { return Err("Claude invoked an OpenCode tool without its PreToolUse ID".into()) };
            let result = Self::expect_locked(&mut inner, &id, name)?;
            (id, result)
        };
        let value = loop {
            if let Some(value) = result.borrow_and_update().clone() {
                break value;
            }
            if result.changed().await.is_err() {
                // The call was dropped without a result (cannot happen: clear() resolves first).
                break ToolResult::cancelled();
            }
        };
        let mut inner = self.inner.lock();
        if let Some(replacement) = value.replacement.clone() {
            inner.replacements.insert(id.clone(), replacement);
        }
        inner.calls.remove(&id);
        Ok(value.mcp)
    }

    pub fn resolve(&self, id: &str, result: ToolResult) -> Result<(), String> {
        let inner = self.inner.lock();
        let Some(call) = inner.calls.get(id) else { return Err(format!("No waiting Claude tool call {id}")) };
        if call.result.borrow().is_none() {
            call.result.send_replace(Some(result));
        }
        Ok(())
    }

    pub fn after(&self, id: &str) -> Option<Vec<Value>> {
        self.inner.lock().replacements.remove(id)
    }

    /// Call only after interrupt-and-drain; resolving these handlers earlier can start another model call.
    pub fn clear(&self) {
        let mut inner = self.inner.lock();
        for call in inner.calls.values() {
            if call.result.borrow().is_none() {
                call.result.send_replace(Some(ToolResult::cancelled()));
            }
        }
        inner.calls.clear();
        inner.replacements.clear();
        inner.next = None;
    }
}

// ---------------------------------------------------------------------------------------------
// The in-process `opencode` MCP server (`tools/server.ts` over @modelcontextprotocol/server 2.3.1)
// ---------------------------------------------------------------------------------------------

pub struct ToolServer {
    tools: Vec<Value>,
    definitions: Vec<Value>,
    pending: Arc<PendingTools>,
    /// Requests the client cancelled (`notifications/cancelled`): their responses are dropped.
    cancelled: Mutex<HashSet<String>>,
}

enum McpReply {
    Response(Value),
    /// The request was cancelled: nothing is sent.
    Silent,
}

impl ToolServer {
    pub fn new(tools: Vec<Value>, pending: Arc<PendingTools>) -> Arc<ToolServer> {
        let definitions = convert::definitions(&tools);
        Arc::new(ToolServer { tools, definitions, pending, cancelled: Mutex::new(HashSet::new()) })
    }

    /// The manifest the SDK captures before `initialize` (`initializeResult` and `toolsListResult`).
    pub fn manifest(&self) -> Value {
        json!({
            "initializeResult": self.initialize_result(MCP_PROTOCOL),
            "toolsListResult": { "tools": self.definitions },
        })
    }

    fn initialize_result(&self, version: &str) -> Value {
        json!({ "protocolVersion": version, "capabilities": { "tools": {} }, "serverInfo": { "name": "opencode", "version": "1.0.0" } })
    }

    fn error(id: &Value, code: i64, message: &str) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
    }

    async fn request(&self, message: &Value) -> McpReply {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let key = js::stringify(&id);
        let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result: Result<Value, String> = match method {
            "initialize" => {
                let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
                let version = if MCP_SUPPORTED.contains(&requested) { requested } else { MCP_PROTOCOL };
                Ok(self.initialize_result(version))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": self.definitions })),
            "tools/call" => {
                let name = format!("{}{}", convert::PREFIX, params.get("name").and_then(Value::as_str).unwrap_or_default());
                if convert::original(&name, &self.tools).is_none() {
                    Err(format!("Claude Code called an unregistered OpenCode tool: {name}"))
                } else {
                    self.pending.call(&name).await
                }
            }
            _ => return McpReply::Response(Self::error(&id, -32601, "Method not found")),
        };
        if self.cancelled.lock().remove(&key) {
            return McpReply::Silent;
        }
        McpReply::Response(match result {
            Ok(result) => json!({ "result": result, "jsonrpc": "2.0", "id": id }),
            Err(message) => Self::error(&id, -32603, &message),
        })
    }

    fn notification(&self, message: &Value) {
        if message.get("method").and_then(Value::as_str) == Some("notifications/cancelled")
            && let Some(id) = message.get("params").and_then(|p| p.get("requestId"))
        {
            self.cancelled.lock().insert(js::stringify(id));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------------------------

/// The `systemPrompt` option, as it reaches `initialize`.
#[derive(Debug, Clone, PartialEq)]
pub enum SystemPrompt {
    /// No option: `systemPrompt: [""]` (the login probe).
    Default,
    /// `{type: "preset", preset: "claude_code", snapshot: false, append}`.
    Preset { append: String },
    /// A replacement prompt: `systemPrompt: [text]`.
    Text(String),
}

/// Everything `query()` needs to start one `claude`.
pub struct Options {
    /// `pathToClaudeCodeExecutable`.
    pub executable: String,
    /// Runs script executables (`.js`, `.mjs`, `.ts`, `.tsx`, `.jsx`).
    pub bun: Option<PathBuf>,
    pub cwd: Option<String>,
    /// The `env` option (process env, connection env and isolation, before the SDK's additions).
    pub env: BTreeMap<String, String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub resume: Option<String>,
    pub session_id: Option<String>,
    pub include_partial_messages: bool,
    pub persist: bool,
    pub system: SystemPrompt,
    /// PreToolUse/PostToolUse hooks (every chat and side call has them; the probe does not).
    pub hooks: Option<Arc<PendingTools>>,
    /// The `opencode` SDK MCP server, when tools are offered.
    pub tools: Option<Arc<ToolServer>>,
}

/// The flags `ProcessTransport.initialize()` passes (`Be` forms), in order.
pub fn args(options: &Options) -> Vec<String> {
    let mut args: Vec<String> = ["--output-format", "stream-json", "--verbose", "--input-format", "stream-json"].map(String::from).to_vec();
    args.push("--thinking=adaptive".into());
    args.push("--thinking-display=summarized".into());
    if let Some(effort) = &options.effort {
        args.push(format!("--effort={effort}"));
    }
    if let Some(model) = &options.model {
        args.push(format!("--model={model}"));
    }
    if let Some(resume) = &options.resume {
        args.push(format!("--resume={resume}"));
    }
    args.push("--allowedTools=mcp__opencode__*".into());
    args.push("--tools=".into());
    args.push("--setting-sources=".into());
    args.push("--strict-mcp-config".into());
    args.push("--permission-mode=dontAsk".into());
    if options.include_partial_messages {
        args.push("--include-partial-messages".into());
    }
    if let Some(id) = &options.session_id {
        args.push(format!("--session-id={id}"));
    }
    if !options.persist {
        args.push("--no-session-persistence".into());
    }
    args.push("--settings".into());
    args.push("{\"disableAllHooks\":true}".into());
    args
}

/// The SDK's additions to the `env` option (`Sx` and `ProcessTransport.initialize`).
pub fn final_env(mut env: BTreeMap<String, String>) -> BTreeMap<String, String> {
    if env.get("CLAUDE_CODE_ENTRYPOINT").is_none_or(String::is_empty) {
        env.insert("CLAUDE_CODE_ENTRYPOINT".into(), "sdk-ts".into());
    }
    if env.get("CLAUDE_AGENT_SDK_VERSION").is_none_or(String::is_empty) {
        env.insert("CLAUDE_AGENT_SDK_VERSION".into(), SDK_VERSION.into());
    }
    if !env.keys().any(|k| k.to_uppercase() == "CLAUDE_CODE_SDK_READS_SESSION_STATE") {
        env.insert("CLAUDE_CODE_SDK_READS_SESSION_STATE".into(), "1".into());
    }
    env.remove("NODE_OPTIONS");
    if env_truthy(env.get("DEBUG_CLAUDE_AGENT_SDK").map(String::as_str)) {
        env.insert("DEBUG".into(), "1".into());
    } else {
        env.remove("DEBUG");
    }
    env
}

pub fn is_script(path: &str) -> bool {
    [".js", ".mjs", ".tsx", ".ts", ".jsx"].iter().any(|e| path.ends_with(e))
}

/// The `initialize` control request (`buildInitializeRequest`).
pub fn initialize_request(options: &Options) -> Value {
    let mut request = Map::new();
    request.insert("subtype".into(), json!("initialize"));
    if options.hooks.is_some() {
        request
            .insert("hooks".into(), json!({ "PreToolUse": [{ "hookCallbackIds": ["hook_0"] }], "PostToolUse": [{ "hookCallbackIds": ["hook_1"] }] }));
    }
    if let Some(server) = &options.tools {
        request.insert("sdkMcpServers".into(), json!(["opencode"]));
        if !env_truthy(std::env::var("CLAUDE_AGENT_SDK_DISABLE_MCP_MANIFESTS").ok().as_deref()) {
            request.insert("sdkMcpServerManifests".into(), json!({ "opencode": server.manifest() }));
        }
    }
    match &options.system {
        SystemPrompt::Default => {
            request.insert("systemPrompt".into(), json!([""]));
        }
        SystemPrompt::Text(text) => {
            request.insert("systemPrompt".into(), json!([text]));
        }
        SystemPrompt::Preset { append } => {
            request.insert("appendSystemPrompt".into(), json!(append));
            request.insert("systemPromptSnapshot".into(), json!(false));
        }
    }
    request.insert("skills".into(), json!([]));
    Value::Object(request)
}

// ---------------------------------------------------------------------------------------------
// Query
// ---------------------------------------------------------------------------------------------

enum Write {
    Line(String),
    End,
}

/// What the message reader delivers: SDK messages, then the end (with the failure, if any).
pub enum Delivered {
    Message(Value),
    End(Option<String>),
}

#[derive(Default)]
struct RunState {
    session_state: Option<String>,
    result_received: bool,
    ceiling: Option<tokio::task::AbortHandle>,
}

type Responder = oneshot::Sender<Result<Value, String>>;

pub struct Query {
    writer: mpsc::UnboundedSender<Write>,
    pending: Mutex<HashMap<String, Responder>>,
    /// Set once the reader ended: `Some(error)` rejects every later request.
    closed: Mutex<Option<String>>,
    /// The `initialize` response (`initializationResult`).
    init: watch::Receiver<Option<Result<Value, String>>>,
    messages: tokio::sync::Mutex<mpsc::UnboundedReceiver<Delivered>>,
    run: Mutex<RunState>,
    run_ended: watch::Sender<bool>,
    pid: Option<u32>,
    exited: watch::Receiver<bool>,
    cleaned: AtomicBool,
    bidirectional: bool,
    sent: AtomicBool,
}

/// What the process stage reports to the reader.
struct ProcessIo {
    stdout: Option<tokio::process::ChildStdout>,
    stderr_tail: Arc<Mutex<String>>,
    exit: watch::Receiver<Option<Exit>>,
    spawn_error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum Exit {
    Code(i32),
    Signal(i32),
}

fn signal_name(signal: i32) -> String {
    let name = match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        6 => "SIGABRT",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => return format!("SIG{signal}"),
    };
    name.to_string()
}

fn format_tail(tail: &str) -> String {
    let len = js::utf16_len(tail);
    let cut = if len > STDERR_TAIL { js::utf16_suffix(tail, len - STDERR_TAIL) } else { tail.to_string() };
    let cut = cut.trim();
    if cut.is_empty() { String::new() } else { format!(". stderr: {cut}") }
}

fn exit_error(exit: Exit, tail: &str) -> Option<String> {
    match exit {
        Exit::Code(0) => None,
        Exit::Code(code) => Some(format!("Claude Code process exited with code {code}{}", format_tail(tail))),
        Exit::Signal(signal) => Some(format!("Claude Code process terminated by signal {}{}", signal_name(signal), format_tail(tail))),
    }
}

fn spawn_error(path: &str, error: &std::io::Error) -> String {
    use std::io::ErrorKind;
    let native = !is_script(path);
    let launch = matches!(error.kind(), ErrorKind::NotFound | ErrorKind::PermissionDenied | ErrorKind::NotADirectory | ErrorKind::ReadOnlyFilesystem)
        || matches!(error.raw_os_error(), Some(1 | 13 | 20 | 40 | 36 | 30));
    if !launch {
        return format!("Failed to spawn Claude Code process: {error}");
    }
    if std::path::Path::new(path).exists() {
        return if native {
            format!(
                "Claude Code native binary at {path} exists but failed to launch. This usually means the binary does not match this system's libc — e.g. spawning a musl-linked binary on a glibc Linux host fails because the musl dynamic loader (/lib/ld-musl-*) is missing. Specify a matching binary with options.pathToClaudeCodeExecutable."
            )
        } else {
            format!("Claude Code executable at {path} exists but failed to launch.")
        };
    }
    if native {
        format!(
            "Claude Code native binary not found at {path}. Please ensure Claude Code is installed via native installer or specify a valid path with options.pathToClaudeCodeExecutable."
        )
    } else {
        format!("Claude Code executable not found at {path}. Is options.pathToClaudeCodeExecutable set?")
    }
}

#[cfg(unix)]
fn signal(pid: u32, signal: i32) {
    // SAFETY: plain kill(2) on a child we spawned.
    unsafe {
        libc::kill(pid as i32, signal);
    }
}

/// Windows has no signals: like Node's `child.kill(signal)` (libuv `uv_process_kill`), terminate the process and
/// remember the signal, which its exit then reports.
#[cfg(windows)]
fn signal(pid: u32, signal: i32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    // Recorded first: the exit can be seen as soon as the process is gone.
    SIGNALLED.lock().insert(pid, signal);
    // SAFETY: opens, terminates and closes a handle to a child we spawned and have not reaped.
    let terminated = unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            false
        } else {
            let ok = TerminateProcess(handle, 1) != 0;
            CloseHandle(handle);
            ok
        }
    };
    if !terminated {
        SIGNALLED.lock().remove(&pid);
    }
}

/// Children `signal` terminated on Windows, with the signal their exit reports.
#[cfg(windows)]
static SIGNALLED: once_cell::sync::Lazy<Mutex<HashMap<u32, i32>>> = once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

#[cfg(not(any(unix, windows)))]
fn signal(_pid: u32, _signal: i32) {}

impl Query {
    /// `query({prompt, options})`: spawns `claude` and writes `initialize`.
    pub fn start(options: Options) -> Arc<Query> {
        // Loading the SDK's `query()` sets this in opencode's own environment, which later children (shell tools,
        // MCP servers) inherit.
        static LEAK: std::sync::Once = std::sync::Once::new();
        LEAK.call_once(|| {
            // SAFETY: Rust's environment accessors are serialized; the value is constant.
            unsafe { std::env::set_var("CLAUDE_AGENT_SDK_VERSION", SDK_VERSION) }
        });
        let args = args(&options);
        let env = final_env(options.env.clone());
        let (command, argv) = if is_script(&options.executable) {
            let bun = options.bun.clone().map(|b| b.to_string_lossy().into_owned()).unwrap_or_else(|| "bun".into());
            (bun, std::iter::once(options.executable.clone()).chain(args).collect::<Vec<_>>())
        } else {
            (options.executable.clone(), args)
        };
        let mut cmd = tokio::process::Command::new(&command);
        cmd.args(&argv)
            .env_clear()
            .envs(&env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(false);
        if let Some(cwd) = &options.cwd {
            cmd.current_dir(cwd);
        }
        let (exit_tx, exit_rx) = watch::channel(None);
        let (exited_tx, exited_rx) = watch::channel(false);
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let (writer, mut writes) = mpsc::unbounded_channel::<Write>();
        let (io, pid) = match cmd.spawn() {
            Err(error) => {
                let _ = exit_tx.send(Some(Exit::Code(-1)));
                let _ = exited_tx.send(true);
                (
                    ProcessIo {
                        stdout: None,
                        stderr_tail: stderr_tail.clone(),
                        exit: exit_rx,
                        spawn_error: Some(spawn_error(&options.executable, &error)),
                    },
                    None,
                )
            }
            Ok(mut child) => {
                let pid = child.id();
                track(pid);
                let stdout = child.stdout.take();
                let mut stdin = child.stdin.take();
                if let Some(mut stderr) = child.stderr.take() {
                    let tail = stderr_tail.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 8192];
                        let mut pending: Vec<u8> = vec![];
                        loop {
                            match stderr.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    pending.extend_from_slice(&buf[..n]);
                                    let valid = match std::str::from_utf8(&pending) {
                                        Ok(_) => pending.len(),
                                        Err(e) => e.valid_up_to(),
                                    };
                                    let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
                                    pending.drain(..valid);
                                    let mut tail = tail.lock();
                                    tail.push_str(&text);
                                    if js::utf16_len(&tail) > 2 * STDERR_TAIL {
                                        let len = js::utf16_len(&tail);
                                        *tail = js::utf16_suffix(&tail, len - STDERR_TAIL);
                                    }
                                }
                            }
                        }
                    });
                }
                tokio::spawn(async move {
                    while let Some(write) = writes.recv().await {
                        match write {
                            Write::Line(line) => {
                                let Some(input) = stdin.as_mut() else { continue };
                                if input.write_all(line.as_bytes()).await.is_err() || input.flush().await.is_err() {
                                    stdin = None;
                                }
                            }
                            Write::End => {
                                if let Some(mut input) = stdin.take() {
                                    let _ = input.shutdown().await;
                                }
                            }
                        }
                    }
                });
                tokio::spawn(async move {
                    let status = child.wait().await;
                    untrack(pid);
                    let exit = match status {
                        #[cfg(unix)]
                        Ok(status) => {
                            use std::os::unix::process::ExitStatusExt;
                            match (status.code(), status.signal()) {
                                (Some(code), _) => Exit::Code(code),
                                (None, Some(signal)) => Exit::Signal(signal),
                                _ => Exit::Code(0),
                            }
                        }
                        #[cfg(windows)]
                        Ok(status) => match pid.and_then(|pid| SIGNALLED.lock().remove(&pid)) {
                            Some(signal) => Exit::Signal(signal),
                            None => Exit::Code(status.code().unwrap_or(0)),
                        },
                        #[cfg(not(any(unix, windows)))]
                        Ok(status) => Exit::Code(status.code().unwrap_or(0)),
                        Err(_) => Exit::Code(-1),
                    };
                    let _ = exit_tx.send(Some(exit));
                    let _ = exited_tx.send(true);
                });
                (ProcessIo { stdout, stderr_tail: stderr_tail.clone(), exit: exit_rx, spawn_error: None }, pid)
            }
        };
        let (init_tx, init_rx) = watch::channel(None);
        let (messages_tx, messages_rx) = mpsc::unbounded_channel();
        let query = Arc::new(Query {
            writer,
            pending: Mutex::new(HashMap::new()),
            closed: Mutex::new(None),
            init: init_rx,
            messages: tokio::sync::Mutex::new(messages_rx),
            run: Mutex::new(RunState::default()),
            run_ended: watch::channel(false).0,
            pid,
            exited: exited_rx,
            cleaned: AtomicBool::new(false),
            bidirectional: options.hooks.is_some() || options.tools.is_some(),
            sent: AtomicBool::new(false),
        });
        // `initialize` is always the first line written.
        let init = query.request_value(initialize_request(&options));
        tokio::spawn(async move {
            let _ = init_tx.send(Some(init.await));
        });
        let handlers = Arc::new(Handlers { pending: options.hooks.clone(), server: options.tools.clone(), inflight: Mutex::new(HashMap::new()) });
        let reader = query.clone();
        tokio::spawn(async move { reader.read(io, handlers, messages_tx).await });
        query
    }

    fn write(&self, value: &Value) {
        let _ = self.writer.send(Write::Line(format!("{}\n", js::stringify(value))));
    }

    /// Sends a control request; resolves with Claude's `response` payload (`{}` when it has none).
    fn request_value(&self, request: Value) -> impl std::future::Future<Output = Result<Value, String>> + Send + 'static {
        let id = request_id();
        let (tx, rx) = oneshot::channel();
        let closed = self.closed.lock().clone();
        if closed.is_none() {
            self.pending.lock().insert(id.clone(), tx);
            self.write(&json!({ "request_id": id, "type": "control_request", "request": request }));
        }
        async move {
            if let Some(error) = closed {
                return Err(error);
            }
            match rx.await {
                Ok(result) => result,
                Err(_) => Err("Query closed before response received".into()),
            }
        }
    }

    pub async fn request(&self, request: Value) -> Result<Value, String> {
        self.request_value(request).await
    }

    /// `initializationResult()`.
    pub async fn initialization(&self) -> Result<Value, String> {
        let mut init = self.init.clone();
        loop {
            if let Some(result) = init.borrow_and_update().clone() {
                return result;
            }
            if init.changed().await.is_err() {
                return Err("Query closed before response received".into());
            }
        }
    }

    /// Writes a prompt message (`streamInput`: `verbatimPrompts` adds `client_composed`).
    pub fn send_user(&self, message: Value) {
        let mut message = message;
        if let Some(o) = message.as_object_mut() {
            o.insert("client_composed".into(), json!(true));
        }
        {
            let mut run = self.run.lock();
            run.result_received = false;
            if let Some(ceiling) = run.ceiling.take() {
                ceiling.abort();
            }
        }
        self.run_ended.send_replace(false);
        self.sent.store(true, Ordering::SeqCst);
        self.write(&message);
    }

    /// Whether a prompt message was written (`streamInput` counted one).
    pub fn sent_input(&self) -> bool {
        self.sent.load(Ordering::SeqCst)
    }

    /// The next delivered message; `None` once the end was taken.
    pub async fn next(&self) -> Option<Delivered> {
        self.messages.lock().await.recv().await
    }

    /// `waitForRunEnd`: resolves once the last turn's result was followed by `idle` (or immediately
    /// for a query without hooks or tools).
    pub async fn wait_run_end(&self) {
        if !self.bidirectional {
            return;
        }
        let mut ended = self.run_ended.subscribe();
        loop {
            if *ended.borrow_and_update() || self.cleaned.load(Ordering::SeqCst) {
                return;
            }
            if ended.changed().await.is_err() {
                return;
            }
        }
    }

    /// `transport.endInput()`.
    pub fn end_input(&self) {
        let _ = self.writer.send(Write::End);
    }

    /// Whether the process has exited.
    pub fn exited(&self) -> bool {
        *self.exited.borrow()
    }

    /// Waits for the process to exit.
    pub async fn wait_exit(&self) {
        let mut exited = self.exited.clone();
        while !*exited.borrow_and_update() {
            if exited.changed().await.is_err() {
                return;
            }
        }
    }

    /// `Query.close()` / `transport.close()`: end stdin and, if the process is still alive after
    /// 2 s, SIGTERM it (SIGKILL 5 s later).
    pub fn close(&self) {
        self.cleanup(None);
        self.end_input();
        let Some(pid) = self.pid else { return };
        if self.exited() {
            return;
        }
        let exited = self.exited.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(2000)).await;
            if *exited.borrow() {
                return;
            }
            signal(pid, 15);
            tokio::time::sleep(Duration::from_millis(5000)).await;
            if !*exited.borrow() {
                signal(pid, 9);
            }
        });
    }

    fn cleanup(&self, error: Option<String>) {
        if self.cleaned.swap(true, Ordering::SeqCst) {
            return;
        }
        let reason = error.unwrap_or_else(|| "Query closed before response received".into());
        *self.closed.lock() = Some(reason.clone());
        for (_, responder) in self.pending.lock().drain() {
            let _ = responder.send(Err(reason.clone()));
        }
        self.run_ended.send_replace(true);
    }

    fn end_run(&self) {
        self.run_ended.send_replace(true);
        if let Some(ceiling) = self.run.lock().ceiling.take() {
            ceiling.abort();
        }
    }

    fn arm_ceiling(self: &Arc<Self>) {
        if *self.run_ended.borrow() || self.cleaned.load(Ordering::SeqCst) {
            return;
        }
        let me = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            tokio::time::sleep(RUN_END_CEILING).await;
            if let Some(me) = me.upgrade() {
                me.end_run();
            }
        });
        if let Some(previous) = self.run.lock().ceiling.replace(task.abort_handle()) {
            previous.abort();
        }
    }

    async fn read(self: Arc<Self>, io: ProcessIo, handlers: Arc<Handlers>, out: mpsc::UnboundedSender<Delivered>) {
        let mut last_error_text: Option<String> = None;
        let mut model_less_ended = false;
        let mut error_before_model_less: Option<String> = None;
        if let Some(stdout) = io.stdout {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
                let kind = message.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
                match kind.as_str() {
                    "control_response" => {
                        let response = message.get("response").cloned().unwrap_or(Value::Null);
                        let id = response.get("request_id").and_then(Value::as_str).unwrap_or_default().to_string();
                        if let Some(responder) = self.pending.lock().remove(&id) {
                            let result = if response.get("subtype").and_then(Value::as_str) == Some("success") {
                                Ok(response.get("response").cloned().unwrap_or(json!({})))
                            } else {
                                Err(response
                                    .get("error")
                                    .map(|e| e.as_str().map(String::from).unwrap_or_else(|| js::stringify(e)))
                                    .unwrap_or_default())
                            };
                            let _ = responder.send(result);
                        }
                        continue;
                    }
                    "control_request" => {
                        let me = self.clone();
                        let task_handlers = handlers.clone();
                        let id = message.get("request_id").and_then(Value::as_str).unwrap_or_default().to_string();
                        let task = tokio::spawn(async move {
                            let request = message.get("request").cloned().unwrap_or(Value::Null);
                            let answer = task_handlers.handle(&request).await;
                            task_handlers.inflight.lock().remove(&id);
                            if me.cleaned.load(Ordering::SeqCst) {
                                return;
                            }
                            match answer {
                                Answer::Success(response) => me.write(&json!({ "type": "control_response", "response": { "subtype": "success", "request_id": id, "response": response } })),
                                Answer::Error(error) => me.write(&json!({ "type": "control_response", "response": { "subtype": "error", "request_id": id, "error": error } })),
                                Answer::Silent => {}
                            }
                        });
                        let id = line_request_id(&line);
                        handlers.inflight.lock().insert(id, task.abort_handle());
                        continue;
                    }
                    "control_cancel_request" => {
                        // The handler keeps running; its answer is still written (as the SDK does).
                        continue;
                    }
                    "keep_alive" | "transcript_mirror" => continue,
                    _ => {}
                }
                let subtype = message.get("subtype").and_then(Value::as_str).unwrap_or_default();
                if kind == "result" {
                    let is_error = message.get("is_error").is_some_and(oc_ai::json::truthy);
                    let text = if is_error {
                        if subtype == "success" {
                            message.get("result").and_then(Value::as_str).map(String::from)
                        } else {
                            Some(
                                message
                                    .get("errors")
                                    .and_then(Value::as_array)
                                    .map(|e| {
                                        e.iter().filter_map(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("; ")
                                    })
                                    .unwrap_or_default(),
                            )
                        }
                    } else {
                        None
                    };
                    last_error_text = text.filter(|t| !t.is_empty());
                    model_less_ended = subtype == "success"
                        && !is_error
                        && message.get("num_turns").and_then(Value::as_f64) == Some(0.0)
                        && message.get("result").and_then(Value::as_str) == Some("");
                    if !model_less_ended {
                        error_before_model_less = last_error_text.clone();
                    }
                    let state = {
                        let mut run = self.run.lock();
                        run.result_received = true;
                        run.session_state.clone()
                    };
                    if state.as_deref().is_none_or(|s| s == "idle") || !self.bidirectional {
                        self.end_run();
                    } else {
                        self.arm_ceiling();
                    }
                } else if kind == "system" && subtype == "session_state_changed" {
                    let state = message.get("state").and_then(Value::as_str).map(String::from);
                    let received = {
                        let mut run = self.run.lock();
                        run.session_state = state.clone();
                        run.result_received
                    };
                    if state.as_deref() == Some("idle") {
                        if received {
                            self.end_run();
                        }
                    } else {
                        self.run_ended.send_replace(false);
                        if state.as_deref() != Some("requires_action") && received {
                            self.arm_ceiling();
                        } else if let Some(ceiling) = self.run.lock().ceiling.take() {
                            ceiling.abort();
                        }
                    }
                    if message.get("sdk_host_only") == Some(&Value::Bool(true)) {
                        continue;
                    }
                } else {
                    last_error_text = None;
                    if kind == "system" && subtype == "init" {
                        model_less_ended = false;
                    } else if kind != "system" || subtype == "compact_boundary" {
                        error_before_model_less = None;
                        model_less_ended = false;
                    }
                }
                let _ = out.send(Delivered::Message(message));
            }
        }
        // stdout ended: the process exits (or failed to start).
        let failure = if let Some(error) = io.spawn_error {
            Some(error)
        } else {
            let mut exit = io.exit.clone();
            let exit = loop {
                if let Some(exit) = *exit.borrow_and_update() {
                    break exit;
                }
                if exit.changed().await.is_err() {
                    break Exit::Code(0);
                }
            };
            if self.cleaned.load(Ordering::SeqCst) { None } else { exit_error(exit, &io.stderr_tail.lock()) }
        };
        let failure = failure.map(|error| {
            let text = last_error_text.clone().or_else(|| if model_less_ended { error_before_model_less.clone() } else { None });
            match text {
                Some(text) => format!("Claude Code returned an error result: {text}"),
                None => error,
            }
        });
        for (_, handle) in handlers.inflight.lock().drain() {
            handle.abort();
        }
        self.cleanup(failure.clone());
        let _ = out.send(Delivered::End(failure));
    }
}

fn line_request_id(line: &str) -> String {
    serde_json::from_str::<Value>(line).ok().and_then(|v| v.get("request_id").and_then(Value::as_str).map(String::from)).unwrap_or_default()
}

enum Answer {
    Success(Value),
    Error(String),
    Silent,
}

/// Claude's control requests (`processControlRequest`).
struct Handlers {
    pending: Option<Arc<PendingTools>>,
    server: Option<Arc<ToolServer>>,
    inflight: Mutex<HashMap<String, tokio::task::AbortHandle>>,
}

const REMOTE_SUBTYPES: [&str; 4] = ["remote_tool_call", "remote_plumbing_call", "remote_tools_probe", "remote_tools_reannounce"];

impl Handlers {
    async fn handle(&self, request: &Value) -> Answer {
        let subtype = request.get("subtype").and_then(Value::as_str).unwrap_or_default();
        if REMOTE_SUBTYPES.contains(&subtype) {
            return Answer::Silent;
        }
        match subtype {
            "can_use_tool" => Answer::Error("canUseTool callback is not provided.".into()),
            "hook_callback" => {
                let id = request.get("callback_id").and_then(Value::as_str).unwrap_or_default();
                let input = request.get("input").cloned().unwrap_or(Value::Null);
                let Some(pending) = self.pending.as_ref().filter(|_| id == "hook_0" || id == "hook_1") else {
                    return Answer::Error(format!("No hook callback found for ID: {id}"));
                };
                let event = input.get("hook_event_name").and_then(Value::as_str).unwrap_or_default();
                let tool_use_id = input.get("tool_use_id").and_then(Value::as_str).unwrap_or_default();
                if id == "hook_0" {
                    if event == "PreToolUse" {
                        let name = input.get("tool_name").and_then(Value::as_str).unwrap_or_default();
                        if let Err(error) = pending.before(tool_use_id, name) {
                            return Answer::Error(error);
                        }
                    }
                    return Answer::Success(json!({}));
                }
                if event != "PostToolUse" {
                    return Answer::Success(json!({}));
                }
                match pending.after(tool_use_id) {
                    Some(replacement) => {
                        Answer::Success(json!({ "hookSpecificOutput": { "hookEventName": "PostToolUse", "updatedMCPToolOutput": replacement } }))
                    }
                    None => Answer::Success(json!({})),
                }
            }
            "mcp_message" => {
                let name = request.get("server_name").and_then(Value::as_str).unwrap_or_default();
                let Some(server) = self.server.as_ref().filter(|_| name == "opencode") else {
                    return Answer::Error(format!("SDK MCP server not found: {name}"));
                };
                let message = request.get("message").cloned().unwrap_or(Value::Null);
                let is_request = message.get("method").is_some() && message.get("id").is_some_and(|id| !id.is_null());
                if !is_request {
                    server.notification(&message);
                    return Answer::Success(json!({ "mcp_response": { "jsonrpc": "2.0", "result": {}, "id": 0 } }));
                }
                match server.request(&message).await {
                    McpReply::Response(response) => Answer::Success(json!({ "mcp_response": response })),
                    McpReply::Silent => Answer::Silent,
                }
            }
            "elicitation" => Answer::Success(json!({ "action": "decline" })),
            "request_user_dialog" => Answer::Silent,
            "oauth_token_refresh" => Answer::Error("getOAuthToken callback is not provided.".into()),
            "host_auth_token_refresh" => Answer::Error("getHostAuthToken callback is not provided.".into()),
            "remote_control_work_secret" => Answer::Error("refreshWorkSecret callback is not provided.".into()),
            other => Answer::Error(format!("Unsupported control request subtype: {other}")),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Children killed on exit (the SDK's `Z_` tracker: SIGTERM to every live child when opencode exits)
// ---------------------------------------------------------------------------------------------

static CHILDREN: once_cell::sync::Lazy<Mutex<HashSet<u32>>> = once_cell::sync::Lazy::new(|| Mutex::new(HashSet::new()));

#[cfg(unix)]
extern "C" fn at_exit() {
    terminate_all();
}

fn track(pid: Option<u32>) {
    #[cfg(unix)]
    {
        static REGISTERED: std::sync::Once = std::sync::Once::new();
        REGISTERED.call_once(|| {
            // SAFETY: registers a plain function that only signals child processes.
            unsafe {
                libc::atexit(at_exit);
            }
        });
    }
    if let Some(pid) = pid {
        CHILDREN.lock().insert(pid);
    }
}

fn untrack(pid: Option<u32>) {
    if let Some(pid) = pid {
        CHILDREN.lock().remove(&pid);
    }
}

/// SIGTERM every `claude` still running (call when the server exits).
pub fn terminate_all() {
    for pid in CHILDREN.lock().drain() {
        signal(pid, 15);
    }
}
