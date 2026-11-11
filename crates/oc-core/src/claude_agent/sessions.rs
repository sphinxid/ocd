//! The process-wide session registry (`claude/sessions.ts`): one entry per OpenCode session (plus
//! temporary entries for side calls), each with a lock, its live `claude` and its saved record. A
//! request prepares (sync decision, restart, branch, parked tool results) and runs one turn.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::convert::{self, ToolResult};
use super::js;
use super::probe::ProbeResult;
use super::process::{Options, PendingTools, SystemPrompt, ToolServer};
use super::session::{ClaudeSession, Step};
use super::sync::{self, Record};

pub const HANDOVER: Duration = Duration::from_millis(10_000);
pub const IDLE: Duration = Duration::from_millis(5 * 60_000);
const START: Duration = Duration::from_secs(30);

/// Claude Code features that duplicate opencode's. opencode's version wins.
pub const ISOLATION_ENV: [(&str, &str); 9] = [
    ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1"),
    ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
    ("DISABLE_AUTO_COMPACT", "1"),
    ("CLAUDE_CODE_ENABLE_AWAY_SUMMARY", "0"),
    ("ENABLE_TOOL_SEARCH", "false"),
    ("CLAUDE_CODE_DISABLE_ADVISOR_TOOL", "1"),
    ("CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK", "1"),
    ("CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK", "1"),
    // Raised so opencode's own truncation applies instead
    ("MAX_MCP_OUTPUT_TOKENS", "200000"),
];

/// Removed from the environment: `CLAUDE_CODE_SIMPLE` skips OAuth, `CLAUDE_CODE_EFFORT_LEVEL` would
/// override OpenCode's live effort, `MCP_TOOL_TIMEOUT` caps every OpenCode tool call.
pub const ISOLATION_REMOVED: [&str; 3] = ["CLAUDE_CODE_SIMPLE", "CLAUDE_CODE_EFFORT_LEVEL", "MCP_TOOL_TIMEOUT"];

/// `{...io.env(), ...connection, ...ISOLATION_ENV}`.
pub fn base_env(process: BTreeMap<String, String>, connection: &[(String, String)]) -> BTreeMap<String, String> {
    let mut env = process;
    for (k, v) in connection {
        env.insert(k.clone(), v.clone());
    }
    for (k, v) in ISOLATION_ENV {
        env.insert(k.into(), v.into());
    }
    for k in ISOLATION_REMOVED {
        env.remove(k);
    }
    env
}

/// A forked transcript (`ClaudeFork.Result`).
#[derive(Debug, Clone)]
pub struct Fork {
    pub session_id: String,
    pub messages: HashMap<String, String>,
}

/// Everything that reaches outside opencode (`ClaudeIO.Interface`).
#[async_trait]
pub trait Io: Send + Sync {
    fn which(&self) -> Option<String>;
    /// The environment before our isolation settings.
    fn env(&self) -> BTreeMap<String, String>;
    /// The Bun binary that runs script executables.
    async fn bun(&self) -> Option<PathBuf> {
        None
    }
    async fn session_exists(&self, session_id: &str) -> Result<bool, String>;
    async fn fork(&self, session_id: &str, message_id: &str) -> Result<Option<Fork>, String>;
    async fn remove(&self, session_id: &str) -> Result<(), String>;
}

/// The plugin KV (`ctx.storage`).
#[async_trait]
pub trait Storage: Send + Sync {
    async fn get(&self, key: &str) -> Option<Value>;
    async fn set(&self, key: &str, value: Value);
}

/// What a chat's own process found (`ClaudeStatus.report`).
pub trait Reporter: Send + Sync {
    fn missing(&self);
    fn ready(&self, probe: ProbeResult);
}

/// The location a request runs for.
#[derive(Clone)]
pub struct Context {
    pub io: Arc<dyn Io>,
    pub storage: Arc<dyn Storage>,
    pub reporter: Arc<dyn Reporter>,
    pub directory: String,
}

/// `ClaudeSessions.Request`.
pub struct Request {
    /// Only primary calls have one. Side calls run in a throwaway process.
    pub session_id: Option<String>,
    /// Native OpenCode fork lineage, or the session supplying a compaction/generate snapshot.
    pub source_id: Option<String>,
    pub model: String,
    pub effort: Option<String>,
    pub max_output_tokens: Option<f64>,
    pub connection: Vec<(String, String)>,
    pub system: SystemPrompt,
    pub prompt: Vec<Value>,
    pub tools: Vec<Value>,
    pub emit: Box<dyn FnMut(&Value) + Send + Sync>,
}

impl Request {
    /// The settings that need a new process to change.
    fn launch(&self) -> String {
        let system = match &self.system {
            SystemPrompt::Default => Value::Null,
            SystemPrompt::Text(text) => json!(text),
            SystemPrompt::Preset { append } => json!({ "type": "preset", "preset": "claude_code", "snapshot": false, "append": append }),
        };
        let connection: serde_json::Map<String, Value> = self.connection.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        js::stringify(&json!([self.max_output_tokens.map(oc_ai::json::num), connection, system, self.tools]))
    }
}

struct Live {
    session: Arc<ClaudeSession>,
    pending: Arc<PendingTools>,
    model: Mutex<String>,
    effort: Mutex<Option<String>>,
    launch: String,
    waiting: Mutex<Option<Vec<String>>>,
}

struct Active {
    cancel: CancellationToken,
    done: tokio::sync::watch::Receiver<bool>,
}

#[derive(Default)]
struct EntryData {
    process: Option<Arc<Live>>,
    record: Option<Record>,
    idle: Option<tokio::task::AbortHandle>,
    active: Option<Active>,
}

struct Entry {
    lock: tokio::sync::Mutex<()>,
    data: Mutex<EntryData>,
}

impl Entry {
    fn new() -> Arc<Entry> {
        Arc::new(Entry { lock: tokio::sync::Mutex::new(()), data: Mutex::new(EntryData::default()) })
    }

    fn process(&self) -> Option<Arc<Live>> {
        self.data.lock().process.clone()
    }
}

struct Continuation {
    end: usize,
    results: Vec<(String, ToolResult)>,
}

/// The turn's view of the reply, for the saved checkpoints.
#[derive(Default)]
struct TurnLog {
    reply: Vec<Value>,
    tool_messages: HashMap<String, String>,
    seen: HashSet<String>,
    message_id: Option<String>,
    waiting: Vec<String>,
}

#[derive(Default)]
pub struct Registry {
    entries: Mutex<HashMap<String, Arc<Entry>>>,
    temporary: Mutex<Vec<Arc<Entry>>>,
    owners: Mutex<usize>,
    handover: Mutex<Option<tokio::task::AbortHandle>>,
}

/// The cancellation of a run (not an error anyone sees).
const CANCELLED: &str = "Claude Agent request cancelled";

fn records_key(session_id: &str) -> String {
    format!("sessions/{session_id}")
}

impl Registry {
    pub fn new() -> Arc<Registry> {
        Arc::new(Registry::default())
    }

    /// A location's plugin started (`attach`): a pending handover is cancelled.
    pub fn attach(&self) {
        *self.owners.lock() += 1;
        if let Some(handover) = self.handover.lock().take() {
            handover.abort();
        }
    }

    /// A location's plugin stopped. When the last one goes, entries close after [`HANDOVER`] unless a
    /// new owner attaches (a reload hands live processes and parked tool handlers over).
    pub fn detach(self: &Arc<Self>) {
        let mut owners = self.owners.lock();
        *owners = owners.saturating_sub(1);
        if *owners != 0 || (self.entries.lock().is_empty() && self.temporary.lock().is_empty()) {
            return;
        }
        let me = self.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(HANDOVER).await;
            me.close_entries().await;
        });
        *self.handover.lock() = Some(task.abort_handle());
    }

    /// Closes every entry now (opencode is exiting).
    pub async fn close(&self) {
        if let Some(handover) = self.handover.lock().take() {
            handover.abort();
        }
        self.close_entries().await;
    }

    async fn close_entries(&self) {
        let entries: Vec<Arc<Entry>> = self.entries.lock().values().cloned().chain(self.temporary.lock().iter().cloned()).collect();
        for entry in entries {
            let active = entry.data.lock().active.as_ref().map(|a| (a.cancel.clone(), a.done.clone()));
            if let Some((cancel, mut done)) = active {
                cancel.cancel();
                while !*done.borrow_and_update() {
                    if done.changed().await.is_err() {
                        break;
                    }
                }
            }
            // The active turn's finalizer can install a new idle timer.
            if let Some(idle) = entry.data.lock().idle.take() {
                idle.abort();
            }
            let _guard = entry.lock.lock().await;
            stop(&entry).await;
        }
        self.entries.lock().clear();
    }

    fn entry_for(&self, request: &Request) -> Arc<Entry> {
        if let Some(id) = &request.session_id {
            return self.entries.lock().entry(id.clone()).or_insert_with(Entry::new).clone();
        }
        let entry = Entry::new();
        self.temporary.lock().push(entry.clone());
        entry
    }

    /// OpenCode stopped running the session, so a turn still waiting for tool results will not get them.
    pub fn idle(self: &Arc<Self>, session_id: &str) {
        let Some(entry) = self.entries.lock().get(session_id).cloned() else { return };
        let waiting = entry.process().is_some_and(|p| p.waiting.lock().is_some());
        if !waiting || entry.data.lock().active.is_some() {
            return;
        }
        arm_idle(&entry);
    }

    /// `ClaudeSessions.run`: one model call. `cancel` interrupts it (the turn is drained and saved).
    pub async fn run(self: &Arc<Self>, ctx: &Context, mut request: Request, cancel: CancellationToken) -> Result<(), String> {
        let entry = self.entry_for(&request);
        let _guard = tokio::select! {
            guard = entry.lock.lock() => guard,
            _ = cancel.cancelled() => return Err(CANCELLED.into()),
        };
        let (done_tx, done_rx) = tokio::sync::watch::channel(false);
        {
            let mut data = entry.data.lock();
            if let Some(idle) = data.idle.take() {
                idle.abort();
            }
            data.active = Some(Active { cancel: cancel.clone(), done: done_rx });
        }
        let result = async {
            let (start, continuation) = self.prepare(ctx, &entry, &request).await?;
            turn(ctx, &entry, &mut request, start, continuation, &cancel).await
        }
        .await;
        self.release(ctx, &entry, &request).await;
        let _ = done_tx.send(true);
        result
    }

    async fn release(&self, ctx: &Context, entry: &Arc<Entry>, request: &Request) {
        entry.data.lock().active = None;
        if request.session_id.is_none() {
            stop(entry).await;
            let session_id = entry.data.lock().record.as_ref().map(|r| r.claude_session_id.clone());
            if let Some(session_id) = session_id
                && let Err(error) = ctx.io.remove(&session_id).await
            {
                tracing::warn!(%error, "failed to remove a Claude side-call transcript");
            }
            self.temporary.lock().retain(|e| !Arc::ptr_eq(e, entry));
            return;
        }
        // A turn waiting for tool results is still busy. Subagents, permission prompts and questions can take longer
        // than IDLE_MS, so idle() starts the timer once OpenCode stops running the session.
        if entry.process().is_some_and(|p| p.waiting.lock().is_some()) {
            return;
        }
        arm_idle(entry);
    }

    async fn branch(&self, ctx: &Context, entry: &Arc<Entry>, request: &Request, source: Option<Record>) -> Result<usize, String> {
        let Some(source) = source else { return Ok(0) };
        let Some(boundary) = sync::boundary(&request.prompt, Some(&source)) else { return Ok(0) };
        // Validate catch-up input before creating a forked transcript on disk.
        sync::catch_up(&request.prompt, boundary.start)?;
        let Some(fork) = ctx.io.fork(&source.claude_session_id, &boundary.message_id).await? else { return Ok(0) };
        let history = boundary
            .history
            .iter()
            .map(|c| sync::Checkpoint {
                hash: c.hash.clone(),
                claude_message_id: c.claude_message_id.as_ref().and_then(|id| fork.messages.get(id).cloned()),
            })
            .collect();
        entry.data.lock().record = Some(Record { claude_session_id: fork.session_id, history: Some(history), interrupted: None });
        Ok(boundary.start)
    }

    async fn snapshot(&self, ctx: &Context, entry: &Arc<Entry>, request: &Request) -> Result<usize, String> {
        let Some(source_id) = request.source_id.clone() else { return Ok(0) };
        let source = self.entries.lock().get(&source_id).cloned();
        let copy = async {
            let live = source.as_ref().and_then(|s| s.data.lock().record.clone());
            let record = match live {
                Some(record) => Some(record),
                None => get_record(ctx, &source_id).await,
            };
            self.branch(ctx, entry, request, record).await
        };
        // Hold the source only while copying its settled prefix, never during side-call inference.
        match &source {
            Some(source) if !Arc::ptr_eq(source, entry) => {
                let _guard = source.lock.lock().await;
                copy.await
            }
            _ => copy.await,
        }
    }

    async fn prepare(&self, ctx: &Context, entry: &Arc<Entry>, request: &Request) -> Result<(usize, Option<Continuation>), String> {
        // Retire an exited or incompatible process first: once we continue its parked turn, open() must not replace it.
        // A launch change waits for a parked turn to end: stopping it makes Claude Code record the waiting tool calls as
        // refused by the user. A step without tools (the step limit) still stops it, since the old process offers them.
        if let Some(process) = entry.process() {
            let parked = process.waiting.lock().is_some() && process.session.running() && !request.tools.is_empty();
            if process.session.ended() || (process.launch != request.launch() && !parked) {
                stop(entry).await;
            }
        }
        let Some(session_id) = request.session_id.clone() else {
            let start = self.snapshot(ctx, entry, request).await?;
            return Ok((start, None));
        };
        if entry.data.lock().record.is_none() {
            let record = get_record(ctx, &session_id).await;
            entry.data.lock().record = record;
        }
        let record = entry.data.lock().record.clone();
        let decision = sync::decide(&request.prompt, record.as_ref());
        let start = match decision.kind {
            sync::DecisionKind::Fresh | sync::DecisionKind::Changed => {
                stop(entry).await;
                let previous = entry.data.lock().record.take();
                if decision.kind == sync::DecisionKind::Changed {
                    self.branch(ctx, entry, request, previous).await?
                } else if previous.is_none() {
                    self.snapshot(ctx, entry, request).await?
                } else {
                    decision.start
                }
            }
            _ => {
                if entry.process().is_some() || record.is_none() {
                    decision.start
                } else {
                    let claude = record.as_ref().map(|r| r.claude_session_id.clone()).unwrap_or_default();
                    if ctx.io.session_exists(&claude).await? {
                        decision.start
                    } else {
                        entry.data.lock().record = None;
                        0
                    }
                }
            }
        };
        let Some(live) = entry.process() else { return Ok((start, None)) };
        let waiting = live.waiting.lock().clone();
        let Some(waiting) = waiting.filter(|_| live.session.running()) else { return Ok((start, None)) };
        let collected = convert::collect(&request.prompt, start)?;
        let mut continuation = Continuation { end: collected.end, results: vec![] };
        for id in waiting {
            let Some(result) = collected.results.get(&id) else {
                stop(entry).await;
                entry.data.lock().record = None;
                return Ok((0, None));
            };
            continuation.results.push((id, result.clone()));
        }
        if collected.aborted {
            // A stopped OpenCode turn needs a fresh Claude turn; addUser would only steer the old one.
            // After a launch change it also needs a new process.
            if live.launch == request.launch() {
                interrupt(entry).await?;
            } else {
                stop(entry).await;
            }
            return Ok((start, None));
        }
        Ok((start, Some(continuation)))
    }
}

async fn get_record(ctx: &Context, session_id: &str) -> Option<Record> {
    Record::decode(ctx.storage.get(&records_key(session_id)).await.as_ref())
}

fn arm_idle(entry: &Arc<Entry>) {
    let weak = Arc::downgrade(entry);
    let task = tokio::spawn(async move {
        tokio::time::sleep(IDLE).await;
        let Some(entry) = weak.upgrade() else { return };
        let _guard = entry.lock.lock().await;
        stop(&entry).await;
    });
    if let Some(previous) = entry.data.lock().idle.replace(task.abort_handle()) {
        previous.abort();
    }
}

async fn stop(entry: &Arc<Entry>) {
    let Some(process) = entry.process() else { return };
    process.session.stop().await;
    process.pending.clear();
    let mut data = entry.data.lock();
    if data.process.as_ref().is_some_and(|p| Arc::ptr_eq(p, &process)) {
        data.process = None;
    }
}

async fn interrupt(entry: &Arc<Entry>) -> Result<(), String> {
    let Some(process) = entry.process() else { return Ok(()) };
    let result = process.session.interrupt().await;
    process.pending.clear();
    *process.waiting.lock() = None;
    result
}

/// `ClaudeQueryOptions.session(...)`.
fn session_options(
    ctx: &Context,
    executable: String,
    bun: Option<PathBuf>,
    request: &Request,
    pending: Arc<PendingTools>,
    resume: Option<String>,
) -> Options {
    let mut env = base_env(ctx.io.env(), &request.connection);
    if let Some(max) = request.max_output_tokens {
        env.insert("CLAUDE_CODE_MAX_OUTPUT_TOKENS".into(), js::number(max));
    }
    let tools = (!request.tools.is_empty()).then(|| ToolServer::new(request.tools.clone(), pending.clone()));
    let session_id = resume.is_none().then(|| uuid::Uuid::new_v4().to_string());
    Options {
        executable,
        bun,
        cwd: Some(ctx.directory.clone()),
        env,
        model: Some(request.model.clone()),
        effort: request.effort.clone(),
        resume,
        session_id,
        include_partial_messages: true,
        persist: request.session_id.is_some(),
        system: request.system.clone(),
        hooks: Some(pending),
        tools,
    }
}

async fn open(ctx: &Context, entry: &Arc<Entry>, request: &Request, cancel: &CancellationToken) -> Result<Arc<Live>, String> {
    let starting = entry.process().is_none();
    if starting {
        let Some(executable) = ctx.io.which() else {
            ctx.reporter.missing();
            return Err("Install Claude Code and run `claude auth login` first".into());
        };
        let pending = PendingTools::new();
        let resume = entry.data.lock().record.as_ref().map(|r| r.claude_session_id.clone());
        let bun = if super::process::is_script(&executable) { ctx.io.bun().await } else { None };
        let session = ClaudeSession::start(session_options(ctx, executable, bun, request, pending.clone(), resume));
        entry.data.lock().process = Some(Arc::new(Live {
            session,
            pending,
            model: Mutex::new(request.model.clone()),
            effort: Mutex::new(request.effort.clone()),
            launch: request.launch(),
            waiting: Mutex::new(None),
        }));
    }
    let live = entry.process().expect("the process was just recorded");
    // A cancel can only land while waiting for ready, so a started process is always recorded and stopped.
    let probe = tokio::select! {
        ready = tokio::time::timeout(START, live.session.ready()) => match ready {
            Ok(ready) => ready,
            Err(_) => Err("claude did not start within 30 seconds".into()),
        },
        _ = cancel.cancelled() => Err(CANCELLED.into()),
    };
    let probe = match probe {
        Ok(probe) => probe,
        Err(error) => {
            stop(entry).await;
            return Err(error);
        }
    };
    // A new process reports its version, login and models, which keeps the model list current without a separate check.
    if starting {
        ctx.reporter.ready(probe);
    }
    // Apply controls before sending input or releasing parked tool results.
    let model = live.model.lock().clone();
    if model != request.model {
        live.session.set_model(&request.model).await?;
        *live.model.lock() = request.model.clone();
    }
    let effort = live.effort.lock().clone();
    if effort != request.effort {
        live.session.set_effort(request.effort.as_deref()).await?;
        *live.effort.lock() = request.effort.clone();
    }
    Ok(live)
}

async fn turn(
    ctx: &Context,
    entry: &Arc<Entry>,
    request: &mut Request,
    start: usize,
    continuation: Option<Continuation>,
    cancel: &CancellationToken,
) -> Result<(), String> {
    // Validate before opening a process: a rejected rebuild must not pair an empty process with the old record.
    let has_record = entry.data.lock().record.is_some();
    let content = if request.session_id.is_some() || has_record {
        sync::catch_up(&request.prompt, continuation.as_ref().map(|c| c.end).unwrap_or(start))?
    } else {
        convert::content(&request.prompt)?
    };
    let live = open(ctx, entry, request, cancel).await?;
    let session = live.session.clone();
    let adds_user = continuation.is_none() || !content.is_empty();
    let user_id = adds_user.then(|| uuid::Uuid::new_v4().to_string());
    let log = Mutex::new(TurnLog::default());
    let (mut completed, mut interrupted, mut started, mut parked) = (false, false, false, false);

    let step: Result<(), String> = async {
        match &continuation {
            None => session.begin(content.clone(), user_id.clone().unwrap_or_default())?,
            Some(continuation) => {
                if let Some(user) = &user_id {
                    session.add_user(content.clone(), user.clone());
                }
                for (id, result) in &continuation.results {
                    live.pending.resolve(id, result.clone())?;
                }
            }
        }
        started = true;
        let tools = request.tools.clone();
        let pending = live.pending.clone();
        let emit_out = &mut request.emit;
        let mut emit = |message: &Value| -> Result<(), String> {
            let kind = message.get("type").and_then(Value::as_str).unwrap_or_default();
            if kind == "system" && message.get("subtype").and_then(Value::as_str) == Some("init") && !tools.is_empty() {
                let connected = message.get("mcp_servers").and_then(Value::as_array).is_some_and(|s| {
                    s.iter().any(|s| {
                        s.get("name").and_then(Value::as_str) == Some("opencode") && s.get("status").and_then(Value::as_str) == Some("connected")
                    })
                });
                if !connected {
                    return Err("The OpenCode tool server did not connect to Claude Code".into());
                }
            }
            let top = message.get("parent_tool_use_id").is_none_or(Value::is_null);
            let uuid = message.get("uuid").and_then(Value::as_str).unwrap_or_default().to_string();
            if kind == "assistant" && top && !message.get("error").is_some_and(oc_ai::json::truthy) && !log.lock().seen.contains(&uuid) {
                let mut log = log.lock();
                log.seen.insert(uuid.clone());
                log.message_id = Some(uuid.clone());
                for part in message.get("message").and_then(|m| m.get("content")).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => log.reply.push(json!({ "type": "text", "text": part.get("text").cloned().unwrap_or(Value::Null) })),
                        Some("tool_use") => {
                            let full = part.get("name").and_then(Value::as_str).unwrap_or_default();
                            let original = convert::original(full, &tools);
                            if original.is_none() && full.starts_with(convert::PREFIX) {
                                // A tool removed after this process started is still on its tool server; answer here so
                                // the turn goes on.
                                let id = part.get("id").and_then(Value::as_str).unwrap_or_default();
                                pending.expect(id, full)?;
                                let removed = json!({ "isError": true, "content": [{ "type": "text", "text": "This tool is no longer available." }] });
                                pending.resolve(id, convert::ToolResult { mcp: removed, replacement: None })?;
                            }
                            let Some(name) = original else { continue };
                            let id = part.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                            pending.expect(&id, full)?;
                            log.waiting.push(id.clone());
                            let mut call = serde_json::Map::new();
                            call.insert("type".into(), json!("tool-call"));
                            call.insert("toolCallId".into(), json!(id));
                            call.insert("toolName".into(), json!(name));
                            if let Some(input) = part.get("input") {
                                call.insert("input".into(), input.clone());
                            }
                            log.reply.push(Value::Object(call));
                        }
                        _ => {}
                    }
                }
            }
            if kind == "user"
                && !uuid.is_empty()
                && let Some(content) = message.get("message").and_then(|m| m.get("content")).and_then(Value::as_array)
            {
                let mut log = log.lock();
                for part in content {
                    if part.get("type").and_then(Value::as_str) == Some("tool_result") {
                        let id = part.get("tool_use_id").and_then(Value::as_str).unwrap_or_default().to_string();
                        log.tool_messages.insert(id, uuid.clone());
                    }
                }
            }
            emit_out(message);
            Ok(())
        };
        let outcome = tokio::select! {
            outcome = session.read_step(&mut emit, &tools) => outcome?,
            _ = cancel.cancelled() => {
                interrupted = true;
                return Err(CANCELLED.to_string());
            }
        };
        // A failed result becomes an error part, which ends OpenCode's stream and cancels this run before it
        // settles: upstream's turn ends interrupted, so the next prompt continues after the submitted input.
        if let Step::Result(result) = &outcome
            && result_failed(result)
        {
            interrupted = true;
            return Ok(());
        }
        parked = matches!(outcome, Step::Tools);
        completed = true;
        *live.waiting.lock() = if parked { Some(log.lock().waiting.clone()) } else { None };
        Ok(())
    }
    .await;

    // A tool boundary ends only the OpenCode step; the Claude turn waits for the results. Anything else interrupts and drains it.
    if !parked && let Err(error) = interrupt(entry).await {
        tracing::warn!(%error, "failed to interrupt the Claude turn");
    }
    // Save the record.
    if let Some(session_id) = &request.session_id
        && session.persisted()
        && started
    {
        let previous = entry.data.lock().record.clone();
        let history = (completed || interrupted).then(|| {
            let log = log.lock();
            sync::build_checkpoints(sync::BuildInput {
                prompt: &request.prompt,
                previous: previous.as_ref(),
                user_id: user_id.as_deref(),
                reply: if completed { &log.reply } else { &[] },
                message_id: if completed { log.message_id.as_deref() } else { None },
                tool_messages: &log.tool_messages,
            })
        });
        let record = Record { claude_session_id: session.id.clone(), history, interrupted: Some(interrupted && !completed) };
        entry.data.lock().record = Some(record.clone());
        ctx.storage.set(&records_key(session_id), serde_json::to_value(&record).unwrap_or(Value::Null)).await;
    }
    step
}

/// Whether a result is a failure (`ClaudeAgentError`: not `success`, `is_error`, or an HTTP error status).
fn result_failed(result: &Value) -> bool {
    let success = result.get("subtype").and_then(Value::as_str) == Some("success");
    let status = if success { result.get("api_error_status").and_then(Value::as_f64) } else { None };
    !success || result.get("is_error").is_some_and(oc_ai::json::truthy) || status.is_some_and(|s| s >= 400.0)
}

/// Whether `error` is a run's cancellation rather than a failure.
pub fn is_cancelled(error: &str) -> bool {
    error == CANCELLED
}
