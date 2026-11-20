//! Automations (`core/src/automation.ts`, `automation/sql.ts`, `schema/src/automation.ts`), opencode 2.3.1's
//! replacement for scheduled prompts, behind `experimental.automations` (old name `schedules`).
//!
//! An automation belongs to a project. On a timer (`at` once, `every` interval or `cron`, each in a zone) it either
//! sends a prompt (`{type: "prompt"}`) to an existing session or to a new child of its creating session with a
//! subagent on every run, or runs a Code Mode program with no model (`{type: "code"}`, see [`code`]) that keeps JSON
//! state between runs and can hand off one prompt. `expires` stops it for good. Rows live in the `automation` table;
//! [`Automations::run`] runs due rows. Every server process runs the loop, and a lease on KV `automation:leader`
//! (TTL 60 s, heartbeat 20 s while ticking, released when nothing is active) elects one leader per database, so an
//! automation runs once across processes sharing a data directory.
//!
//! An `event` trigger (with `experimental.mcp_events`) watches a connected MCP server's push event instead: the
//! leader opens one stream per active watch ([`event`]), runs the action on each event with the event as input, and
//! keeps its place in `event_position` (`{agent, cursor, id}`), so a restart, or another process taking the lease,
//! resumes from the last committed event. Watches a 2.3.1 process created in the same database run here too.

pub mod code;
pub mod event;
pub mod time;
pub mod tools;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use futures::future::BoxFuture;
use parking_lot::Mutex;
use rusqlite::OptionalExtension;
use rusqlite::types::Value as Sql;
use serde_json::{Map, Value, json};

use oc_schema::types::SessionInfo;
use oc_storage::Database;

use crate::permission::{AssertInput, PermissionError, Source};

pub use time::Zone;

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const MIN_PROMPT_INTERVAL: f64 = MINUTE as f64;
const MIN_CODE_INTERVAL: f64 = 15_000.0;
/// A run this far past its due time is marked late.
const LATE_AFTER: i64 = MINUTE;
/// The loop re-reads the table at least this often, so clock jumps and missed doorbells are caught.
pub const MAX_SLEEP: i64 = 30_000;
pub const LEADER_KEY: &str = "automation:leader";
pub const LEADER_TTL: i64 = 2 * MAX_SLEEP;
/// How long a due automation waits before its location's flag is read again, while automations are off there.
const DISABLED_RETRY: i64 = HOUR;
/// How long an automation whose prompt failed waits before trying again.
const FAILED_RETRY: i64 = 5 * MINUTE;
/// How long an automation whose code failed waits before trying again, up to MAX_CODE_FAILURES.
const CODE_FAILED_RETRY: i64 = MINUTE;
const MAX_CODE_FAILURES: i64 = 3;
const TICK_CONCURRENCY: usize = 4;

/// `Automation.DisabledError`'s message.
pub const DISABLED: &str = "Automations are disabled; set experimental.automations in config";

/// The `/automation` command template (`plugin/command/automation.txt`).
pub const COMMAND_TEMPLATE: &str = include_str!("../../../../assets/prompt/command/automation.txt");
pub const COMMAND: &str = "automation";
pub const COMMAND_DESCRIPTION: &str = "create an automation, or list, pause, resume and delete this project's automations";
pub const PLUGIN_ID: &str = "opencode.tool.automation";

/// `Automation.runSessionID`: the child session a `{subagent}` run starts, derived from the run so a retry adopts it.
pub fn run_session_id(id: &str, run: i64) -> String {
    format!("ses_{id}_{run}")
}

/// `Automation.isRunSession`: a `{subagent}` run's child, which finishes without reporting to or waking its parent.
pub fn is_run_session(session_id: &str) -> bool {
    session_id.starts_with("ses_aut_")
}

/// The failures of the automation service.
#[derive(Debug, Clone, PartialEq)]
pub enum AutomationError {
    /// `Automation.NotFoundError`
    NotFound(String),
    /// `Automation.InvalidError`
    Invalid(String),
    /// `Automation.DisabledError`
    Disabled,
    /// `Session.NotFoundError`
    SessionNotFound(String),
    /// `Permission.BlockedError` (its message)
    Blocked(String),
    /// `Permission.CorrectedError` (the feedback)
    Corrected(String),
    /// A declined permission: a defect upstream (it ends the tool call, or the HTTP request with a 500).
    Declined,
    /// `FileSystem.DirectoryNotFoundError`: a location whose folder is gone.
    DirectoryNotFound(String),
    /// Anything else a run can fail with (a failed admission, a storage error).
    Other(String),
}

impl AutomationError {
    pub fn message(&self) -> String {
        match self {
            AutomationError::NotFound(id) => format!("Automation not found: {id}"),
            AutomationError::Invalid(reason) => reason.clone(),
            AutomationError::Disabled => DISABLED.to_string(),
            AutomationError::SessionNotFound(id) => format!("Session not found: {id}"),
            AutomationError::Blocked(message) => message.clone(),
            AutomationError::Corrected(feedback) => feedback.clone(),
            AutomationError::Declined => "The user declined this tool call".to_string(),
            AutomationError::DirectoryNotFound(directory) => format!("Directory not found: {directory}"),
            AutomationError::Other(message) => message.clone(),
        }
    }

    pub(crate) fn from_permission(error: PermissionError) -> AutomationError {
        match error {
            blocked @ PermissionError::Blocked { .. } => AutomationError::Blocked(blocked.to_string()),
            PermissionError::Corrected { feedback } => AutomationError::Corrected(feedback),
            PermissionError::Declined => AutomationError::Declined,
        }
    }
}

impl std::fmt::Display for AutomationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// Why one opening of an event watch ended.
#[derive(Debug)]
enum WatchError {
    /// `Automation.EventDisconnected`: the stream broke or the server is not connected; it may reopen.
    Disconnected(String),
    Failed(AutomationError),
}

impl From<AutomationError> for WatchError {
    fn from(error: AutomationError) -> WatchError {
        WatchError::Failed(error)
    }
}

impl From<event::Failure> for WatchError {
    fn from(failure: event::Failure) -> WatchError {
        match failure {
            event::Failure::Disconnected(message) => WatchError::Disconnected(message),
            event::Failure::Error(message) => WatchError::Failed(AutomationError::Other(message)),
        }
    }
}

/// `Permission.invocation(context)`: the tool call that creates an automation. A tool call belongs to the model
/// request that issued it, even if the session has since switched agents.
#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    pub session_id: String,
    pub agent: String,
    pub source: Source,
}

impl Invocation {
    pub fn of(ctx: &oc_tools::ToolContext) -> Invocation {
        Invocation {
            session_id: ctx.session_id.clone(),
            agent: ctx.agent.clone(),
            source: Source { kind: "tool".into(), message_id: ctx.message_id.clone(), id: ctx.call_id.clone() },
        }
    }
}

/// `matchesAuthor(owner, agent, invocation)`
pub(crate) fn matches_author(owner: &SessionInfo, agent: Option<&str>, invocation: &Invocation) -> bool {
    invocation.session_id == owner.id && Some(invocation.agent.as_str()) == agent
}

/// `Automation.CreateInput`: the decoded request (`trigger` and `action` in their JSON forms).
#[derive(Debug, Clone, Default)]
pub struct CreateInput {
    pub session_id: Option<String>,
    pub directory: Option<String>,
    pub name: String,
    pub trigger: Value,
    pub action: Value,
    pub expires: Option<String>,
}

impl CreateInput {
    /// From a `CreateInput` / `Request` JSON object (already checked against its schema).
    pub fn from_value(input: &Value) -> CreateInput {
        let text = |k: &str| input.get(k).and_then(Value::as_str).map(String::from);
        CreateInput {
            session_id: text("sessionID"),
            directory: text("directory"),
            name: text("name").unwrap_or_default(),
            trigger: input.get("trigger").cloned().unwrap_or(Value::Null),
            action: input.get("action").cloned().unwrap_or(Value::Null),
            expires: text("expires"),
        }
    }
}

fn kind(value: &Value) -> &str {
    value.get("type").and_then(Value::as_str).unwrap_or_default()
}

/// Copies `keys` of `value` in this order.
fn pick(value: &Value, keys: &[&str]) -> Value {
    let mut out = Map::new();
    for key in keys {
        if let Some(v) = value.get(*key) {
            out.insert((*key).into(), v.clone());
        }
    }
    Value::Object(out)
}

/// `Automation.Trigger` in schema key order.
pub fn encode_trigger(trigger: &Value) -> Value {
    match kind(trigger) {
        "at" => pick(trigger, &["type", "at", "tz"]),
        "every" => pick(trigger, &["type", "every", "tz"]),
        "cron" => pick(trigger, &["type", "cron", "tz"]),
        "event" => pick(trigger, &["type", "server", "event", "args"]),
        _ => trigger.clone(),
    }
}

/// `Automation.Action` in schema key order.
pub fn encode_action(action: &Value) -> Value {
    match kind(action) {
        "prompt" => {
            let mut out = pick(action, &["type", "prompt"]);
            if let Some(to) = action.get("to") {
                out["to"] = pick(to, &["session", "subagent"]);
            }
            out
        }
        "code" => pick(action, &["type", "code", "args"]),
        _ => action.clone(),
    }
}

/// An `automation` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: String,
    pub project_id: String,
    /// The session that created it. No foreign key: deleting that session keeps the automation.
    pub session_id: Option<String>,
    /// Where it runs, recorded at creation.
    pub directory: String,
    pub name: String,
    pub trigger: Value,
    pub tz: String,
    pub action: Value,
    pub status: String,
    pub next_due_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub run_count: i64,
    pub failures: i64,
    pub last_run_at: Option<i64>,
    pub last_message_id: Option<String>,
    pub last_outcome: Option<String>,
    pub last_output: Option<String>,
    /// The loop leaves a due row alone until then, e.g. while its location has automations turned off.
    pub retry_at: Option<i64>,
    /// Code action state persisted across runs; `null` is stored as SQL NULL.
    pub state: Value,
    /// An event watch's authorizing agent and resume point.
    pub event_position: Option<Value>,
    pub time_created: i64,
    pub time_updated: i64,
}

const COLUMNS: &str = "id, project_id, session_id, directory, name, \"trigger\", tz, action, status, next_due_at, expires_at, \
     run_count, failures, last_run_at, last_message_id, last_outcome, last_output, retry_at, state, event_position, \
     time_created, time_updated";

fn json_column(text: Option<String>) -> Option<Value> {
    text.and_then(|t| serde_json::from_str(&t).ok())
}

impl Row {
    fn read(r: &rusqlite::Row) -> rusqlite::Result<Row> {
        Ok(Row {
            id: r.get(0)?,
            project_id: r.get(1)?,
            session_id: r.get(2)?,
            directory: r.get(3)?,
            name: r.get(4)?,
            trigger: json_column(r.get(5)?).unwrap_or(Value::Null),
            tz: r.get(6)?,
            action: json_column(r.get(7)?).unwrap_or(Value::Null),
            status: r.get(8)?,
            next_due_at: r.get(9)?,
            expires_at: r.get(10)?,
            run_count: r.get(11)?,
            failures: r.get(12)?,
            last_run_at: r.get(13)?,
            last_message_id: r.get(14)?,
            last_outcome: r.get(15)?,
            last_output: r.get(16)?,
            retry_at: r.get(17)?,
            state: json_column(r.get(18)?).unwrap_or(Value::Null),
            event_position: json_column(r.get(19)?).filter(|v| !v.is_null()),
            time_created: r.get(20)?,
            time_updated: r.get(21)?,
        })
    }

    pub fn trigger_type(&self) -> &str {
        kind(&self.trigger)
    }

    /// `toInfo(row)`: `Automation.Info` JSON in schema key order (times in epoch ms).
    pub fn info(&self) -> Value {
        let mut out = Map::new();
        out.insert("id".into(), json!(self.id));
        out.insert("name".into(), json!(self.name));
        out.insert("projectID".into(), json!(self.project_id));
        if let Some(s) = &self.session_id {
            out.insert("sessionID".into(), json!(s));
        }
        out.insert("directory".into(), json!(self.directory));
        out.insert("trigger".into(), encode_trigger(&self.trigger));
        out.insert("tz".into(), json!(self.tz));
        out.insert("action".into(), encode_action(&self.action));
        if let Some(v) = self.expires_at {
            out.insert("expires".into(), json!(v));
        }
        out.insert("status".into(), json!(self.status));
        if let Some(v) = self.next_due_at {
            out.insert("nextDueAt".into(), json!(v));
        }
        out.insert("runCount".into(), json!(self.run_count));
        if let Some(v) = self.last_run_at {
            out.insert("lastRunAt".into(), json!(v));
        }
        if let Some(v) = &self.last_outcome {
            out.insert("lastOutcome".into(), json!(v));
        }
        if let Some(v) = &self.last_output {
            out.insert("lastOutput".into(), json!(v));
        }
        if !self.state.is_null() {
            out.insert("state".into(), self.state.clone());
        }
        out.insert("time".into(), json!({ "created": self.time_created, "updated": self.time_updated }));
        Value::Object(out)
    }
}

fn duration(value: &str) -> Result<f64, AutomationError> {
    match time::duration_millis(value) {
        None => Err(AutomationError::Invalid(format!("Invalid duration: {value}"))),
        Some(ms) if ms > 0.0 => Ok(ms),
        Some(_) => Err(AutomationError::Invalid(format!("Duration must be positive: {value}"))),
    }
}

/// Stored durations were validated on create.
fn millis(value: &str) -> f64 {
    time::duration_millis(value).unwrap_or(f64::NAN)
}

/// `/^in\s+(.+)$/i`: the duration of a relative time.
fn relative(input: &str) -> Option<&str> {
    let head = input.get(..2)?;
    if !head.eq_ignore_ascii_case("in") {
        return None;
    }
    let rest = &input[2..];
    let trimmed = rest.trim_start_matches(|c: char| (c as u32) < 0x10000 && oc_codemode::number::is_js_whitespace(c as u16));
    (trimmed.len() < rest.len() && !trimmed.is_empty() && !trimmed.contains(['\n', '\r', '\u{2028}', '\u{2029}'])).then_some(trimmed)
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| (c as u32) < 0x10000 && oc_codemode::number::is_js_whitespace(c as u16))
}

/// `parseTime(raw, zone, now)`: `"in 10 minutes"`, an instant with an offset, or a wall-clock time in `zone`.
pub fn parse_time(raw: &str, zone: &Zone, now: i64) -> Option<i64> {
    let input = js_trim(raw);
    if let Some(delay) = relative(input) {
        return time::duration_millis(delay).filter(|d| *d > 0.0).map(|d| (now as f64 + d) as i64);
    }
    time::parse_at(input, zone)
}

/// `nextCron`: the first slot strictly after `now`, stepping past slots a fall-back repeats.
pub fn next_cron(cron: &time::Cron, now: i64) -> Option<i64> {
    let mut from = now;
    loop {
        let at = cron.next(from)?;
        if at > now {
            return Some(at);
        }
        from = at + HOUR;
    }
}

/// `following(row, now)`: the first slot strictly after `now`, or `None` for `at`. Missed slots are skipped, so
/// downtime runs once.
pub fn following(trigger: &Value, tz: &str, next_due_at: Option<i64>, now: i64) -> Option<i64> {
    match kind(trigger) {
        "every" => {
            // Keep the phase: skip every missed slot rather than running them in a burst.
            let every = millis(trigger.get("every").and_then(Value::as_str).unwrap_or_default());
            let due = next_due_at.unwrap_or(now) as f64;
            let next = due + (((now as f64 - due) / every).floor() + 1.0) * every;
            next.is_finite().then_some(next as i64)
        }
        "cron" => {
            let expr = trigger.get("cron").and_then(Value::as_str).unwrap_or_default();
            let cron = time::cron_parse_tz(expr, tz).ok()?;
            next_cron(&cron, now)
        }
        _ => None,
    }
}

fn finished(expires_at: Option<i64>, next: Option<i64>) -> bool {
    match next {
        None => true,
        Some(next) => expires_at.is_some_and(|e| next > e),
    }
}

fn expired(expires_at: Option<i64>, now: i64) -> bool {
    expires_at.is_some_and(|e| now > e)
}

/// `deliverySummary(sessionID, subagent?)`
fn delivery_summary(session_id: &str, subagent: bool) -> String {
    format!("{} {session_id}", if subagent { "Started subagent" } else { "Prompted" })
}

/// The location settings an automation follows (`settings(directory)`): config is per location, so an automation
/// follows the config of the directory it runs in.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub enabled: bool,
    /// `experimental.mcp_events`
    pub events: bool,
    pub timezone: Option<String>,
}

/// What the automation service needs from the server: admitting a prompt through the session API
/// (`sessions.prompt`, which also wakes the session).
pub trait Prompter: Send + Sync {
    fn prompt(&self, session_id: String, id: String, text: String, metadata: Map<String, Value>) -> BoxFuture<'static, Result<(), AutomationError>>;
    /// `execution.wake(sessionID)`
    fn wake(&self, session_id: &str);
}

/// The loop's clock (`Clock.currentTimeMillis`). Tests install a manual clock that follows tokio's paused time.
pub enum Clock {
    System,
    /// `base + (tokio now − start) + offset`: follows tokio's (paused) time.
    Manual {
        base: i64,
        start: tokio::time::Instant,
        offset: AtomicI64,
    },
    /// Stands still until set or advanced (`TestClock`).
    Fixed(AtomicI64),
}

impl Clock {
    pub fn manual(base: i64) -> Clock {
        Clock::Manual { base, start: tokio::time::Instant::now(), offset: AtomicI64::new(0) }
    }

    pub fn fixed(at: i64) -> Clock {
        Clock::Fixed(AtomicI64::new(at))
    }

    pub fn now(&self) -> i64 {
        match self {
            Clock::System => oc_schema::id::now_ms() as i64,
            Clock::Manual { base, start, offset } => base + start.elapsed().as_millis() as i64 + offset.load(Ordering::SeqCst),
            Clock::Fixed(at) => at.load(Ordering::SeqCst),
        }
    }

    /// `TestClock.setTime`
    pub fn set(&self, ms: i64) {
        let delta = ms - self.now();
        self.advance(delta);
    }

    /// `TestClock.adjust` without sleeping.
    pub fn advance(&self, ms: i64) {
        match self {
            Clock::Manual { offset, .. } | Clock::Fixed(offset) => {
                offset.fetch_add(ms, Ordering::SeqCst);
            }
            Clock::System => {}
        }
    }
}

/// `Latch`: `open` releases every waiter and stays open until `close`.
#[derive(Default)]
struct Doorbell {
    open: AtomicBool,
    notify: tokio::sync::Notify,
}

impl Doorbell {
    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    fn close(&self) {
        self.open.store(false, Ordering::SeqCst);
    }
    /// `doorbell.await.pipe(Effect.timeoutOption(ms))`
    async fn wait(&self, ms: i64) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.open.load(Ordering::SeqCst) {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_millis(ms.max(0) as u64), notified).await;
    }
}

#[derive(Default)]
struct Lease {
    leading: bool,
    /// `leadership !== "unchecked"`: claim once on startup even with nothing active, since a finished automation
    /// may have left a prompt pending.
    checked: bool,
}

/// `ListInput`
#[derive(Debug, Clone, Default)]
pub struct ListInput {
    pub session_id: Option<String>,
    pub project_id: Option<String>,
}

/// The automation service (`Automation.Service`).
pub struct Automations {
    db: Database,
    app: Weak<crate::app::App>,
    engine: OnceLock<Weak<crate::runner::Engine>>,
    prompter: OnceLock<Arc<dyn Prompter>>,
    doorbell: Doorbell,
    owner: String,
    lease: Mutex<Lease>,
    clock: Clock,
    /// The running event watches, by automation (`watches`, a FiberMap).
    watches: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

type Values<'a> = Vec<(&'a str, Sql)>;

fn text(v: &str) -> Sql {
    Sql::Text(v.to_string())
}

fn opt(v: Option<i64>) -> Sql {
    v.map(Sql::Integer).unwrap_or(Sql::Null)
}

fn json_sql(v: &Value) -> Sql {
    if v.is_null() { Sql::Null } else { Sql::Text(v.to_string()) }
}

impl Automations {
    pub fn new(app: &Arc<crate::app::App>) -> Arc<Automations> {
        Self::with_clock(app, Clock::System)
    }

    pub fn with_clock(app: &Arc<crate::app::App>, clock: Clock) -> Arc<Automations> {
        Arc::new(Automations {
            db: app.db.clone(),
            app: Arc::downgrade(app),
            engine: OnceLock::new(),
            prompter: OnceLock::new(),
            doorbell: Doorbell::default(),
            owner: uuid::Uuid::new_v4().to_string(),
            lease: Mutex::new(Lease::default()),
            clock,
            watches: Mutex::new(HashMap::new()),
        })
    }

    pub fn set_engine(&self, engine: Weak<crate::runner::Engine>) {
        let _ = self.engine.set(engine);
    }

    pub fn set_prompter(&self, prompter: Arc<dyn Prompter>) {
        let _ = self.prompter.set(prompter);
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// This process's lease owner id.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub(crate) fn engine(&self) -> Option<Arc<crate::runner::Engine>> {
        self.engine.get().and_then(Weak::upgrade)
    }

    pub(crate) fn app(&self) -> Option<Arc<crate::app::App>> {
        self.app.upgrade()
    }

    fn runner(&self) -> Result<Arc<crate::runner::Engine>, AutomationError> {
        self.engine().ok_or_else(|| AutomationError::Other("Session runner is not available".into()))
    }

    /// The location services a session's work runs with in `directory` (`instances.provide` / `inDirectory`):
    /// plugins activated, the session's agent and the registered tools.
    pub(crate) async fn env(&self, session: &SessionInfo, directory: &str) -> Result<crate::runner::CheckEnv, AutomationError> {
        if !std::path::Path::new(directory).is_dir() {
            return Err(AutomationError::DirectoryNotFound(directory.to_string()));
        }
        let engine = self.runner()?;
        engine.check_env(session, directory).await.map_err(|e| {
            if std::path::Path::new(directory).is_dir() {
                AutomationError::Other(e.to_string())
            } else {
                AutomationError::DirectoryNotFound(directory.to_string())
            }
        })
    }

    pub(crate) fn session(&self, id: &str) -> Result<SessionInfo, AutomationError> {
        match self.db.session_get(id) {
            Ok(Some(s)) => Ok(s),
            _ => Err(AutomationError::SessionNotFound(id.to_string())),
        }
    }

    /// `existing(sessionID)`: the session, or `None` once it is deleted.
    fn existing(&self, id: &str) -> Option<SessionInfo> {
        self.db.session_get(id).ok().flatten()
    }

    /// `settings(directory)`: fails with `DirectoryNotFound` when the folder is gone.
    pub async fn settings(&self, directory: &str) -> Result<Settings, AutomationError> {
        if !std::path::Path::new(directory).is_dir() {
            return Err(AutomationError::DirectoryNotFound(directory.to_string()));
        }
        let Some(engine) = self.engine() else { return Ok(Settings::default()) };
        let location = engine.location(directory).await.map_err(|_| AutomationError::DirectoryNotFound(directory.to_string()))?;
        Ok(settings_of(&location.entries))
    }

    /// `enabled(directory)`: whether `experimental.automations` is on in the config of `directory`; `Err` when the
    /// folder is gone.
    pub async fn enabled(&self, directory: &str) -> Result<bool, AutomationError> {
        self.settings(directory).await.map(|s| s.enabled)
    }

    fn find(&self, id: &str) -> Result<Row, AutomationError> {
        let row = self
            .db
            .with(|c| c.query_row(&format!("SELECT {COLUMNS} FROM automation WHERE id = ?"), [id], Row::read).optional())
            .map_err(|e| AutomationError::Other(e.to_string()))?;
        row.ok_or_else(|| AutomationError::NotFound(id.to_string()))
    }

    /// The row, if any (tests and the HTTP layer).
    pub fn row(&self, id: &str) -> Option<Row> {
        self.find(id).ok()
    }

    /// `settle(row, values, {evenIfPaused})`: writes what came of running `row`, but only while its run_count is the
    /// one that was read: if another process recorded a run since, as after a leader handoff mid-run, this write
    /// would clobber its state or count the run twice. A pause that landed meanwhile wins too, except with
    /// `even_if_paused`, for a run that already happened.
    fn settle(&self, row: &Row, values: &[(&str, Sql)], even_if_paused: bool) {
        let assignments: Vec<String> = values.iter().map(|(k, _)| format!("\"{k}\" = ?")).collect();
        let status = if even_if_paused { "status != 'done'" } else { "status = 'active'" };
        let sql = format!("UPDATE automation SET {} WHERE id = ? AND run_count = ? AND {status}", assignments.join(", "));
        let mut params: Vec<Sql> = values.iter().map(|(_, v)| v.clone()).collect();
        params.push(Sql::Text(row.id.clone()));
        params.push(Sql::Integer(row.run_count));
        if let Err(e) = self.db.with(|c| c.execute(&sql, rusqlite::params_from_iter(params))) {
            tracing::error!(automation = %row.id, error = %e, "automation update failed");
        }
    }

    /// `pauseWith(row, now, reason)`
    fn pause_with(&self, row: &Row, now: i64, reason: &str) {
        self.settle(
            row,
            &[
                ("status", text("paused")),
                ("last_outcome", text("failed")),
                ("last_output", text(reason)),
                ("retry_at", Sql::Null),
                ("time_updated", Sql::Integer(now)),
            ],
            true,
        );
    }

    fn pending(&self, message_id: &str) -> bool {
        self.db.with(|c| c.query_row("SELECT id FROM session_inbox WHERE id = ?", [message_id], |_| Ok(())).optional()).ok().flatten().is_some()
    }

    fn wake(&self, session_id: &str) {
        if let Some(prompter) = self.prompter.get() {
            prompter.wake(session_id);
        }
    }

    /// `advance(row, ended, done, event?)`: a successful run's cursor or next timer. A slow timer skips slots it missed
    /// while running.
    fn advance(row: &Row, ended: i64, done: bool, event: Option<&event::Message>) -> Values<'static> {
        if let (Some(event), Some(Value::Object(position))) = (event, &row.event_position) {
            let mut position = position.clone();
            position.insert("cursor".into(), json!(event.cursor));
            position.insert("id".into(), json!(event.event_id));
            let mut values = Vec::new();
            if done {
                values.push(("status", text("done")));
            }
            values.push(("event_position", Sql::Text(Value::Object(position).to_string())));
            return values;
        }
        let next = following(&row.trigger, &row.tz, row.next_due_at, ended);
        if done || finished(row.expires_at, next) {
            vec![("status", text("done")), ("next_due_at", Sql::Null)]
        } else {
            vec![("next_due_at", opt(next))]
        }
    }

    /// `subagent(owner, id, invocation?)`: the agent a run's child starts with, if `owner` could start it with the
    /// subagent tool: not hidden, not primary-only, within the depth limit and already allowed in the owner's
    /// current location. The tools authorized when the automation was created do not grant permission to start a
    /// child.
    async fn subagent(&self, owner: &SessionInfo, id: &str, invocation: Option<&Invocation>) -> Result<crate::agent::Agent, AutomationError> {
        let env = self.env(owner, &owner.location.directory).await?;
        if env.location.project.id != owner.project_id {
            return Err(AutomationError::Invalid("The creating session's location is no longer in its project".into()));
        }
        // `SubagentDepth.reached`: the configured limit if another generation of children would exceed it.
        let limit = oc_config::fold::subagent_depth(&env.location.entries);
        let mut current = owner.clone();
        let mut reached = true;
        for _ in 0..limit {
            let Some(parent) = current.parent_id.clone() else {
                reached = false;
                break;
            };
            current = self.session(&parent).map_err(|e| AutomationError::Other(e.message()))?;
        }
        if reached {
            return Err(AutomationError::Invalid(format!("Subagent depth limit reached ({limit})")));
        }
        let Some(agent) = env.agents.select(Some(id)).cloned() else {
            return Err(AutomationError::Invalid(format!("Unknown subagent: {id}")));
        };
        if agent.hidden {
            return Err(AutomationError::Invalid(format!("Subagent {id} is hidden")));
        }
        if agent.mode == crate::agent::Mode::Primary {
            return Err(AutomationError::Invalid(format!("Agent {id} is primary-only")));
        }
        let Some(creator) = env.agents.select(owner.agent.as_deref()).cloned() else {
            return Err(AutomationError::Invalid(format!(
                "Creating session's agent not found: {}",
                owner.agent.as_deref().unwrap_or("(default)")
            )));
        };
        if let Some(invocation) = invocation
            && !matches_author(owner, Some(&creator.id), invocation)
        {
            return Err(AutomationError::Invalid("Create the automation with the session's current agent".into()));
        }
        let app = self.app().ok_or(AutomationError::Declined)?;
        let input = AssertInput {
            id: None,
            session_id: invocation.map(|i| i.session_id.clone()).unwrap_or_else(|| owner.id.clone()),
            action: "subagent".into(),
            resources: vec![agent.id.clone()],
            save: None,
            metadata: None,
            source: invocation.map(|i| i.source.clone()),
            agent: Some(invocation.map(|i| i.agent.clone()).unwrap_or_else(|| creator.id.clone())),
        };
        // Only consulted for an ask. Never borrow an ambient grant, even if a hook turned a config allow into an ask.
        let deny = vec![oc_schema::types::PermissionRule {
            action: "subagent".into(),
            resource: "*".into(),
            effect: oc_schema::types::Effect::Deny,
        }];
        match crate::permission::unattended(deny, app.permissions.assert(input)).await {
            Ok(()) => Ok(agent),
            Err(blocked @ PermissionError::Blocked { .. }) => Err(AutomationError::Invalid(format!("Cannot start subagent {id}: {blocked}"))),
            Err(other) => Err(AutomationError::from_permission(other)),
        }
    }

    /// `promptTarget(row, to)`: the session a prompt goes to; a `{subagent}` target starts this run's child.
    async fn prompt_target(&self, row: &Row, to: &Value) -> Result<String, AutomationError> {
        if let Some(session) = to.get("session").and_then(Value::as_str) {
            let Some(target) = self.existing(session) else {
                return Err(AutomationError::Invalid(format!("Target session {session} no longer exists")));
            };
            if target.project_id != row.project_id {
                return Err(AutomationError::Invalid(format!("Target session {session} moved to another project")));
            }
            return Ok(target.id);
        }
        let subagent = to.get("subagent").and_then(Value::as_str).unwrap_or_default();
        // The child hangs off the creating session, so a run needs that session.
        let Some(owner) = row.session_id.as_deref().and_then(|id| self.existing(id)) else {
            return Err(AutomationError::Invalid("The session that created this automation no longer exists".into()));
        };
        if owner.project_id != row.project_id {
            return Err(AutomationError::Invalid("The session that created this automation moved to another project".into()));
        }
        let agent = self.subagent(&owner, subagent, None).await?;
        let app = self.app().ok_or_else(|| AutomationError::Other("The server is shutting down".into()))?;
        // The id is derived from the run too, so a retry adopts the child it already started instead of leaking one.
        // Like the subagent tool's children it inherits the parent's location and permissions, but it reports to no
        // one.
        let child = app
            .sessions
            .create(crate::session::CreateInput {
                id: Some(run_session_id(&row.id, row.run_count + 1)),
                parent_id: Some(owner.id.clone()),
                agent: Some(agent.id.clone()),
                model: agent.model.clone().or_else(|| owner.model.clone()),
                title: Some(row.name.clone()),
                ..Default::default()
            })
            .await
            .map_err(|e| AutomationError::Other(e.to_string()))?;
        // Session.create adopts an existing ID, even if its caller requested a different parent or agent.
        if child.parent_id.as_deref() != Some(owner.id.as_str())
            || child.agent.as_deref() != Some(agent.id.as_str())
            || child.project_id != row.project_id
        {
            return Err(AutomationError::Invalid(format!("Run child {} has a different parent, agent or project", child.id)));
        }
        if child.location != owner.location {
            return Err(AutomationError::Invalid(format!("Run child {} is no longer in the creator's location", child.id)));
        }
        if child.permissions != owner.permissions {
            return Err(AutomationError::Invalid(format!("Run child {} no longer matches the creator's permissions", child.id)));
        }
        Ok(child.id)
    }

    /// `codeFailed(row, now, outcome)`
    fn code_failed(&self, row: &Row, now: i64, error: &str, output: Option<&str>, invalid: bool) {
        let ended = self.clock.now();
        let strikes = row.failures + 1;
        // Preserve the diagnostic even when logs alone would fill the budget.
        let reason = match output {
            None => error.to_string(),
            Some(output) => format!("{error}\n{output}"),
        };
        if row.trigger_type() == "event" {
            return self.pause_with(row, now, &code::bounded_output(&reason));
        }
        if !invalid && strikes < MAX_CODE_FAILURES {
            return self.settle(
                row,
                &[
                    ("failures", Sql::Integer(strikes)),
                    ("last_outcome", text("failed")),
                    ("last_output", Sql::Text(code::bounded_output(&reason))),
                    ("retry_at", Sql::Integer(ended + CODE_FAILED_RETRY)),
                    ("time_updated", Sql::Integer(ended)),
                ],
                false,
            );
        }
        let prefix = if invalid { "Paused: ".to_string() } else { format!("Paused after {strikes} failed runs: ") };
        self.settle(
            row,
            &[
                ("status", text("paused")),
                ("failures", Sql::Integer(strikes)),
                ("last_run_at", Sql::Integer(now)),
                ("last_outcome", text("failed")),
                ("last_output", Sql::Text(code::bounded_output(&format!("{prefix}{reason}")))),
                ("retry_at", Sql::Null),
                ("time_updated", Sql::Integer(ended)),
            ],
            true,
        );
    }

    /// `admit(sessionID, text, values, done)`: admits the prompt through the session API, then commits the run.
    #[allow(clippy::too_many_arguments)]
    async fn admit(
        &self,
        row: &Row,
        now: i64,
        late: bool,
        session_id: &str,
        prompt: &str,
        mut values: Values<'static>,
        done: bool,
        event: Option<&event::Message>,
    ) -> Result<(), AutomationError> {
        let run = row.run_count + 1;
        let code = kind(&row.action) == "code";
        // A code action's unsuffixed ID identifies its tool calls, not its returned prompt.
        let id = format!("msg_{}_{run}{}", row.id, if code { "_send" } else { "" });
        let mut automation = Map::new();
        automation.insert("id".into(), json!(row.id));
        automation.insert("name".into(), json!(row.name));
        automation.insert("run".into(), json!(run));
        automation.insert("dueAt".into(), json!(row.next_due_at.unwrap_or(now)));
        automation.insert("late".into(), json!(late));
        let mut metadata = Map::new();
        metadata.insert("automation".into(), Value::Object(automation));
        let prompter = self.prompter.get().cloned().ok_or_else(|| AutomationError::Other("Session runner is not available".into()))?;
        prompter.prompt(session_id.to_string(), id.clone(), prompt.to_string(), metadata).await?;
        values.push(("last_message_id", Sql::Text(id)));
        self.complete(row, now, late, values, done, event);
        Ok(())
    }

    /// `complete(values, done)`: commits the action result and advances the timer together, only after any
    /// handoff is durable.
    fn complete(&self, row: &Row, now: i64, late: bool, mut values: Values<'static>, done: bool, event: Option<&event::Message>) {
        let ended = self.clock.now();
        let code = kind(&row.action) == "code";
        values.push(("run_count", Sql::Integer(row.run_count + 1)));
        values.push(("last_run_at", Sql::Integer(now)));
        values.push(("last_outcome", text(if late { "late" } else { "fired" })));
        values.push(("retry_at", Sql::Null));
        values.extend(Self::advance(row, ended, done, event));
        values.push(("time_updated", Sql::Integer(if code { ended } else { now })));
        self.settle(row, &values, true);
    }

    /// `runAction(row, now, event?)`: an invalid target or policy pauses the automation with the reason.
    async fn run_action(&self, row: &Row, now: i64, event: Option<&event::Message>) -> Result<(), AutomationError> {
        match self.run_action_inner(row, now, event).await {
            Err(AutomationError::Invalid(reason)) => {
                self.pause_with(row, now, &reason);
                Ok(())
            }
            other => other,
        }
    }

    async fn run_action_inner(&self, row: &Row, now: i64, event: Option<&event::Message>) -> Result<(), AutomationError> {
        let action = &row.action;
        let due = row.next_due_at.unwrap_or(now);
        let late = now - due >= LATE_AFTER;

        if kind(action) == "prompt" {
            let prompt = action.get("prompt").and_then(Value::as_str).unwrap_or_default();
            let prompt = match event {
                Some(message) => event::prompt(prompt, message),
                None => prompt.to_string(),
            };
            if event.is_some() && prompt.len() > 32_768 {
                return Err(AutomationError::Invalid("Event prompt exceeds 32 KiB".into()));
            }
            let to = action.get("to").cloned().unwrap_or(Value::Null);
            let target = self.prompt_target(row, &to).await?;
            // Only an unchanging timer prompt can be coalesced. Re-wake it in case the first wake was lost.
            if event.is_none()
                && to.get("session").is_some()
                && let Some(last) = &row.last_message_id
                && self.pending(last)
            {
                self.wake(&target);
                let mut values = vec![("last_outcome", text("skipped")), ("retry_at", Sql::Null)];
                values.extend(Self::advance(row, self.clock.now(), false, None));
                values.push(("time_updated", Sql::Integer(now)));
                self.settle(row, &values, false);
                return Ok(());
            }
            let summary = delivery_summary(&target, to.get("subagent").is_some());
            return self.admit(row, now, late, &target, &prompt, vec![("last_output", Sql::Text(summary))], false, event).await;
        }

        // The tools were authorized for the creating session's agent and permissions, so a run needs that session.
        let Some(owner) = row.session_id.as_deref().and_then(|id| self.existing(id)) else {
            self.pause_with(row, now, "The session that authorized this automation's tools no longer exists");
            return Ok(());
        };
        if owner.project_id != row.project_id {
            self.pause_with(row, now, "The session that authorized this automation's tools moved to another project");
            return Ok(());
        }
        let input = code::RunInput {
            message_id: format!("msg_{}_{}", row.id, row.run_count + 1),
            state: row.state.clone(),
            now,
            runs: row.run_count,
            event: event.map(event::input),
        };
        let outcome = code::run(self, &owner, action, input, &row.directory).await?;
        let (state, done, send, output) = match outcome {
            code::Outcome::Failed { error, output, invalid } => {
                self.code_failed(row, now, &error, output.as_deref(), invalid);
                return Ok(());
            }
            code::Outcome::Ok { state, done, send, output } => (state, done, send, output),
        };
        let mut values: Values<'static> = Vec::new();
        if let Some(state) = &state {
            values.push(("state", json_sql(state)));
        }
        values.push(("failures", Sql::Integer(0)));
        let Some(send) = send else {
            values.push(("last_output", output.clone().map(Sql::Text).unwrap_or(Sql::Null)));
            self.complete(row, now, late, values, done, event);
            return Ok(());
        };
        let to = match &send.subagent {
            Some(agent) => json!({ "subagent": agent }),
            None => json!({ "session": owner.id }),
        };
        let sent = async {
            let target = self.prompt_target(row, &to).await?;
            let summary = delivery_summary(&target, send.subagent.is_some());
            let mut values = values.clone();
            values.push(("last_output", Sql::Text(code::handoff_output(&summary, output.as_deref()))));
            self.admit(row, now, late, &target, &send.prompt, values, done, event).await
        }
        .await;
        if let Err(error) = sent {
            let invalid = matches!(error, AutomationError::Invalid(_) | AutomationError::DirectoryNotFound(_));
            self.code_failed(row, now, &error.message(), output.as_deref(), invalid);
        }
        Ok(())
    }

    /// `runDue(row, now)`: runs one due automation, after the checks both kinds of action share: leadership,
    /// expiry and the flag.
    async fn run_due(&self, row: &Row, now: i64) -> Result<(), AutomationError> {
        let Some(due) = row.next_due_at else { return Ok(()) };
        // Claimed for each run, not once per tick: the lease may have passed to another process since the tick began.
        if !self.claim_leader(now) {
            return Ok(());
        }
        // A slot that only comes up late, after the automation expired (downtime, a retry), never runs.
        if expired(row.expires_at, now) && now - due >= LATE_AFTER {
            self.settle(
                row,
                &[
                    ("status", text("done")),
                    ("next_due_at", Sql::Null),
                    ("last_outcome", text("expired")),
                    ("retry_at", Sql::Null),
                    ("time_updated", Sql::Integer(now)),
                ],
                false,
            );
            return Ok(());
        }
        if !self.settings(&row.directory).await?.enabled {
            // Reading the flag boots the location, so check again in an hour rather than every tick. The row keeps
            // its due time, so once the flag is back on it catches up like any missed slot.
            self.settle(
                row,
                &[
                    ("last_outcome", text("disabled")),
                    ("retry_at", Sql::Integer(now + DISABLED_RETRY)),
                    ("time_updated", Sql::Integer(now)),
                ],
                false,
            );
            return Ok(());
        }
        self.run_action(row, now, None).await
    }

    const ELIGIBLE: &'static str = "status = 'active' AND next_due_at <= ?1 AND (retry_at IS NULL OR retry_at <= ?1)";

    fn due_ids(&self, from: i64) -> Vec<String> {
        self.db
            .with(|c| {
                let mut stmt = c.prepare(&format!("SELECT id FROM automation WHERE {} ORDER BY next_due_at ASC", Self::ELIGIBLE))?;
                stmt.query_map([from], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()
            })
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "automation tick failed");
                Vec::new()
            })
    }

    /// `tickAt(from)`: runs every automation due at `from`, at most once each.
    pub async fn tick_at(&self, from: i64) {
        use futures::StreamExt;
        let due = self.due_ids(from);
        let concurrency = if due.len() > 1 { TICK_CONCURRENCY } else { 1 };
        futures::stream::iter(due)
            .for_each_concurrent(concurrency, |id| async move {
                // Selection isn't admission: a timer queued behind slow actions can be paused, removed or rescheduled.
                let row = self
                    .db
                    .with(|c| {
                        c.query_row(&format!("SELECT {COLUMNS} FROM automation WHERE id = ?2 AND {}", Self::ELIGIBLE), rusqlite::params![from, id], Row::read)
                            .optional()
                    })
                    .ok()
                    .flatten();
                let Some(row) = row else { return };
                let now = self.clock.now();
                match self.run_due(&row, now).await {
                    Ok(()) => {}
                    // The folder (e.g. a removed worktree) is gone; pause rather than retrying every 5 minutes.
                    Err(AutomationError::DirectoryNotFound(directory)) => {
                        self.pause_with(&row, now, &format!("Directory no longer exists: {directory}"));
                    }
                    Err(error) => {
                        // Back off rather than retrying a failing run on every tick.
                        tracing::warn!(automation = %row.id, cause = %error, "automation failed to run");
                        self.settle(
                            &row,
                            &[
                                ("last_outcome", text("failed")),
                                ("last_output", Sql::Text(error.message())),
                                ("retry_at", Sql::Integer(now + FAILED_RETRY)),
                                ("time_updated", Sql::Integer(now)),
                            ],
                            false,
                        );
                    }
                }
            })
            .await;
    }

    /// `tick`: runs every automation that is due now.
    pub async fn tick(&self) {
        self.tick_at(self.clock.now()).await
    }

    /// `nextWake(from)`: rows due by `from` that could not run are rechecked after at most MAX_SLEEP.
    fn next_wake(&self, from: i64) -> i64 {
        let at: Option<i64> = self
            .db
            .with(|c| c.query_row("SELECT min(next_due_at) FROM automation WHERE status = 'active' AND next_due_at > ?", [from], |r| r.get(0)))
            .ok()
            .flatten();
        match at {
            None => MAX_SLEEP,
            Some(at) => (at - self.clock.now()).clamp(0, MAX_SLEEP),
        }
    }

    /// `rewake`: a crash between admission and a session claiming the work can lose the wake. Each automation
    /// records only its last admission, not earlier pending children.
    fn rewake(&self) {
        let sessions: Vec<String> = self
            .db
            .with(|c| {
                let mut stmt = c.prepare(
                    "SELECT DISTINCT session_inbox.session_id FROM automation INNER JOIN session_inbox ON session_inbox.id = automation.last_message_id",
                )?;
                stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "automation rewake failed");
                Vec::new()
            });
        for session in sessions {
            self.wake(&session);
        }
    }

    /// `hasActiveRows(timersOnly)`: whether any automation is active (only timers with `timers_only`).
    fn has_active_rows(&self, timers_only: bool) -> bool {
        let sql = if timers_only {
            "SELECT id FROM automation WHERE status = 'active' AND next_due_at IS NOT NULL LIMIT 1"
        } else {
            "SELECT id FROM automation WHERE status = 'active' LIMIT 1"
        };
        self.db.with(|c| c.query_row(sql, [], |_| Ok(())).optional()).ok().flatten().is_some()
    }

    /// Stops every running event watch (`FiberMap.clear(watches)`).
    fn clear_watches(&self) {
        for (_, watch) in self.watches.lock().drain() {
            watch.abort();
        }
    }

    /// Stops one event watch (`FiberMap.remove(watches, id)`).
    fn remove_watch(&self, id: &str) {
        if let Some(watch) = self.watches.lock().remove(id) {
            watch.abort();
        }
    }

    /// `claimLeader(now)`: takes or renews the lease on KV `automation:leader` when it is ours or has expired.
    pub fn claim_leader(&self, now: i64) -> bool {
        let value = json!({ "owner": self.owner, "until": now + LEADER_TTL }).to_string();
        self.db
            .with(|c| {
                c.query_row(
                    "INSERT INTO kv (key, value, time_created, time_updated) VALUES (?1, ?2, ?3, ?3) \
                     ON CONFLICT (key) DO UPDATE SET value = ?2, time_updated = ?3 \
                     WHERE json_extract(kv.value, '$.owner') = ?4 OR json_extract(kv.value, '$.until') <= ?3 \
                     RETURNING key",
                    rusqlite::params![LEADER_KEY, value, now, self.owner],
                    |_| Ok(()),
                )
                .optional()
            })
            .ok()
            .flatten()
            .is_some()
    }

    /// `releaseLeader`
    pub fn release_leader(&self) {
        self.clear_watches();
        {
            let mut lease = self.lease.lock();
            lease.leading = false;
            lease.checked = true;
        }
        let _ = self
            .db
            .with(|c| c.execute("DELETE FROM kv WHERE key = ? AND json_extract(value, '$.owner') = ?", rusqlite::params![LEADER_KEY, self.owner]));
    }

    /// One iteration of the loop.
    async fn iteration(self: &Arc<Self>) {
        self.doorbell.close();
        let active = self.has_active_rows(false);
        let (leading, checked) = {
            let lease = self.lease.lock();
            (lease.leading, lease.checked)
        };
        if !active && checked {
            if leading {
                self.release_leader();
            }
            self.doorbell.wait(MAX_SLEEP).await;
            return;
        }
        let now = self.clock.now();
        if !self.claim_leader(now) {
            self.clear_watches();
            {
                let mut lease = self.lease.lock();
                lease.leading = false;
                lease.checked = true;
            }
            self.doorbell.wait(MAX_SLEEP).await;
            return;
        }
        if !leading {
            {
                let mut lease = self.lease.lock();
                lease.leading = true;
                lease.checked = true;
            }
            self.rewake();
        }
        if !active {
            self.release_leader();
            self.doorbell.wait(MAX_SLEEP).await;
            return;
        }
        // No timer tick or next-slot query for event-only databases; timer-only databases need no extra probe.
        if self.reconcile_watches() > 0 && !self.has_active_rows(true) {
            self.doorbell.wait(MAX_SLEEP).await;
            return;
        }
        let me = self.clone();
        let heartbeat = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis((LEADER_TTL / 3) as u64)).await;
                me.claim_leader(me.clock.now());
            }
        });
        self.tick_at(now).await;
        heartbeat.abort();
        if !self.has_active_rows(false) {
            self.release_leader();
            self.doorbell.wait(MAX_SLEEP).await;
            return;
        }
        let wait = self.next_wake(now);
        if wait > 0 {
            self.doorbell.wait(wait).await;
        }
    }

    /// `run`: runs automations whenever one falls due, until the task is dropped; the lease is released on the way
    /// out.
    pub async fn run(self: Arc<Self>) {
        struct Release(Arc<Automations>);
        impl Drop for Release {
            fn drop(&mut self) {
                self.0.release_leader();
            }
        }
        let _release = Release(self.clone());
        loop {
            self.iteration().await;
        }
    }

    fn resolve_zone(tz: Option<&str>) -> Result<Zone, AutomationError> {
        match tz {
            None => Ok(time::zone_make_local()),
            Some(tz) => time::zone_make_named(tz).ok_or_else(|| AutomationError::Invalid(format!("Unknown time zone: {tz}"))),
        }
    }

    /// `firstDue(trigger, action, zone, now)`: the trigger as stored, with a relative `at` fixed to an instant, and
    /// its first due time.
    fn first_due(trigger: &Value, action: &Value, zone: &Zone, now: i64) -> Result<(Value, Option<i64>), AutomationError> {
        let s = |k: &str| trigger.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let trigger = encode_trigger(trigger);
        match kind(&trigger) {
            "event" => Ok((trigger, None)),
            "at" => {
                let input = s("at");
                let Some(at) = parse_time(&input, zone, now) else {
                    return Err(AutomationError::Invalid(format!("Invalid time: {input}")));
                };
                if at <= now {
                    return Err(AutomationError::Invalid(format!("Time is in the past: {input}")));
                }
                let stored = if relative(js_trim(&input)).is_some() {
                    let mut stored = json!({ "type": "at", "at": time::format_iso(at) });
                    if let Some(tz) = trigger.get("tz") {
                        stored["tz"] = tz.clone();
                    }
                    stored
                } else {
                    trigger
                };
                Ok((stored, Some(at)))
            }
            "every" => {
                let every = duration(&s("every"))?;
                let code = kind(action) == "code";
                if every < if code { MIN_CODE_INTERVAL } else { MIN_PROMPT_INTERVAL } {
                    return Err(AutomationError::Invalid(
                        if code { "every must be at least 15 seconds" } else { "every must be at least 1 minute" }.into(),
                    ));
                }
                Ok((trigger, Some(now + every as i64)))
            }
            _ => {
                let expr = s("cron");
                let cron = time::cron_parse(&expr, Some(zone)).map_err(AutomationError::Invalid)?;
                if cron.seconds_len() != 1 {
                    return Err(AutomationError::Invalid("cron must run at most once a minute".into()));
                }
                let next = next_cron(&cron, now).ok_or_else(|| AutomationError::Invalid(format!("cron never runs: {expr}")))?;
                Ok((trigger, Some(next)))
            }
        }
    }

    /// `resolveAction`: resolves a prompt's target and checks a code action, both in the automation's directory.
    async fn resolve_action(
        &self,
        input: &CreateInput,
        owner: Option<&SessionInfo>,
        trigger: &Value,
        directory: &str,
        project_id: &str,
        invocation: Option<&Invocation>,
    ) -> Result<Value, AutomationError> {
        let action = &input.action;
        if kind(action) == "prompt" {
            let to = match (action.get("to").filter(|t| !t.is_null()), owner) {
                (Some(to), _) => pick(to, &["session", "subagent"]),
                (None, Some(owner)) => json!({ "session": owner.id }),
                (None, None) => return Err(AutomationError::Invalid("A prompt action needs `to` when there is no session".into())),
            };
            // Another project's session is out of reach, as it is for the automation tools.
            if let Some(session) = to.get("session").and_then(Value::as_str)
                && self.session(session)?.project_id != project_id
            {
                return Err(AutomationError::Invalid(format!("Session {session} is in another project")));
            }
            if let Some(subagent) = to.get("subagent").and_then(Value::as_str) {
                // The child of each run hangs off the creating session.
                let Some(owner) = owner else {
                    return Err(AutomationError::Invalid("A prompt to a subagent needs a session to be its parent".into()));
                };
                self.subagent(owner, subagent, invocation).await?;
            }
            return Ok(json!({ "type": "prompt", "prompt": action.get("prompt").cloned().unwrap_or(json!("")), "to": to }));
        }
        // Tool permissions are asked in, and checked against, the creating session.
        let Some(owner) = owner else { return Err(AutomationError::Invalid("A code action needs a session".into())) };
        let mut code = json!({ "type": "code", "code": action.get("code").cloned().unwrap_or(json!("")) });
        if let Some(args) = action.get("args") {
            code["args"] = args.clone();
        }
        code::authorize(self, owner, &input.name, trigger, &code, invocation, directory).await?;
        Ok(code)
    }

    /// `create(input, invocation?)`: validates and authorizes, then stores the automation. Returns `Automation.Info`.
    pub async fn create(&self, input: CreateInput, invocation: Option<&Invocation>) -> Result<Value, AutomationError> {
        let owner = match &input.session_id {
            Some(id) => Some(self.session(id)?),
            None => None,
        };
        let directory = match (&owner, &input.directory) {
            (Some(owner), _) => owner.location.directory.clone(),
            (None, Some(directory)) => directory.clone(),
            (None, None) => self.app().map(|a| a.global.home.display().to_string()).unwrap_or_default(),
        };
        if !is_absolute(&directory) {
            return Err(AutomationError::Invalid(format!("Directory must be an absolute path: {directory}")));
        }
        let missing = |e: AutomationError| match e {
            AutomationError::DirectoryNotFound(_) => AutomationError::Invalid(format!("Directory does not exist: {directory}")),
            other => other,
        };
        let config = self.settings(&directory).await.map_err(missing)?;
        if !config.enabled {
            return Err(AutomationError::Disabled);
        }
        if kind(&input.trigger) == "event" {
            if !config.events {
                return Err(AutomationError::Invalid("Event automations require experimental.mcp_events".into()));
            }
            let (Some(owner), Some(invocation)) = (&owner, invocation) else {
                return Err(AutomationError::Invalid("Create an event automation from a tool call in a local session".into()));
            };
            if owner.location.workspace_id.is_some() {
                return Err(AutomationError::Invalid("Create an event automation from a tool call in a local session".into()));
            }
            self.authorize_watch(owner, invocation, &input, &directory).await.map_err(missing)?;
        }
        let tz = if kind(&input.trigger) == "event" { None } else { input.trigger.get("tz").and_then(Value::as_str).map(String::from) };
        let zone = Self::resolve_zone(tz.or(config.timezone.clone()).as_deref())?;
        let now = self.clock.now();
        let (trigger, due) = Self::first_due(&input.trigger, &input.action, &zone, now)?;
        let expires_at = match &input.expires {
            None => None,
            Some(expires) => Some(parse_time(expires, &zone, now).ok_or_else(|| AutomationError::Invalid(format!("Invalid time: {expires}")))?),
        };
        if expires_at.is_some_and(|e| due.unwrap_or(now) > e) {
            return Err(AutomationError::Invalid("The automation expires before it first runs".into()));
        }
        let project_id = match &owner {
            Some(owner) => owner.project_id.clone(),
            None => {
                let app = self.app().ok_or_else(|| AutomationError::Other("The server is shutting down".into()))?;
                crate::project::resolve_and_persist(&app.bus, std::path::Path::new(&directory)).await.map_err(|e| AutomationError::Other(e.to_string()))?.id
            }
        };
        let action = self.resolve_action(&input, owner.as_ref(), &trigger, &directory, &project_id, invocation).await.map_err(missing)?;

        let id = oc_schema::id::ascending("aut");
        let event_position = (kind(&trigger) == "event").then(|| invocation.map(|i| json!({ "agent": i.agent }))).flatten();
        let values: Vec<(&str, Sql)> = vec![
            ("id", Sql::Text(id.clone())),
            ("project_id", Sql::Text(project_id)),
            ("session_id", owner.as_ref().map(|o| Sql::Text(o.id.clone())).unwrap_or(Sql::Null)),
            ("directory", Sql::Text(directory.clone())),
            ("name", Sql::Text(input.name.clone())),
            ("trigger", Sql::Text(trigger.to_string())),
            ("event_position", event_position.map(|p| Sql::Text(p.to_string())).unwrap_or(Sql::Null)),
            ("tz", Sql::Text(time::zone_to_string(&zone))),
            ("action", Sql::Text(action.to_string())),
            ("next_due_at", opt(due)),
            ("expires_at", opt(expires_at)),
            ("time_created", Sql::Integer(now)),
            ("time_updated", Sql::Integer(now)),
        ];
        let columns: Vec<String> = values.iter().map(|(k, _)| format!("\"{k}\"")).collect();
        let marks: Vec<&str> = values.iter().map(|_| "?").collect();
        let sql = format!("INSERT INTO automation ({}) VALUES ({}) RETURNING {COLUMNS}", columns.join(", "), marks.join(", "));
        let params: Vec<Sql> = values.into_iter().map(|(_, v)| v).collect();
        let row = self
            .db
            .with(|c| c.query_row(&sql, rusqlite::params_from_iter(params), Row::read))
            .map_err(|e| AutomationError::Other(e.to_string()))?;
        self.doorbell.open();
        Ok(row.info())
    }

    /// An event watch's creation checks (`create` with an `event` trigger): the session's current agent, the
    /// `opencode_watch_mcp_event` permission for the server, then the server's push event type and the trigger's
    /// arguments ([`event::resolve`] from now, its stream dropped right away).
    async fn authorize_watch(&self, owner: &SessionInfo, invocation: &Invocation, input: &CreateInput, directory: &str) -> Result<(), AutomationError> {
        let env = self.env(owner, directory).await?;
        if !matches_author(owner, env.agent.as_ref().map(|a| a.id.as_str()), invocation) {
            return Err(AutomationError::Invalid("Create the watch with the session's current agent".into()));
        }
        let app = self.app().ok_or(AutomationError::Declined)?;
        let server = input.trigger.get("server").and_then(Value::as_str).unwrap_or_default().to_string();
        let mut metadata = Map::new();
        metadata.insert("automation".into(), json!(input.name));
        metadata.insert("trigger".into(), encode_trigger(&input.trigger));
        app.permissions
            .assert(AssertInput {
                id: None,
                session_id: invocation.session_id.clone(),
                action: "opencode_watch_mcp_event".into(),
                resources: vec![server.clone()],
                save: None,
                metadata: Some(metadata),
                source: Some(invocation.source.clone()),
                agent: Some(invocation.agent.clone()),
            })
            .await
            .map_err(AutomationError::from_permission)?;
        // Everything `resolve` fails with, the server not being configured included, is an invalid trigger.
        let events = self.mcp_events(directory, &server).await.map_err(|e| match e {
            AutomationError::DirectoryNotFound(_) => e,
            other => AutomationError::Invalid(other.message()),
        })?;
        event::resolve(events, &encode_trigger(&input.trigger), None).await.map(drop).map_err(|e| AutomationError::Invalid(e.message().to_string()))
    }

    /// The MCP Events of `server` in the Location of `directory` (`mcp.events(server)` there).
    async fn mcp_events(&self, directory: &str, server: &str) -> Result<Option<oc_mcp::events::Events>, AutomationError> {
        if !std::path::Path::new(directory).is_dir() {
            return Err(AutomationError::DirectoryNotFound(directory.to_string()));
        }
        let engine = self.runner()?;
        let location = engine.location(directory).await.map_err(|_| AutomationError::DirectoryNotFound(directory.to_string()))?;
        let mcp = engine.mcp(&location).await;
        mcp.events(server).await.map_err(|e| AutomationError::Other(e.to_string()))
    }

    /// `checkWatch(row, server)`: re-checks the original placement and the resource-specific grant before opening a
    /// stream or running an action.
    async fn check_watch(&self, row: &Row, server: &str) -> Result<(), AutomationError> {
        let config = self.settings(&row.directory).await?;
        if !config.enabled || !config.events {
            return Err(AutomationError::Invalid("Event automations are disabled".into()));
        }
        let owner = row.session_id.as_deref().and_then(|id| self.existing(id));
        let Some(owner) = owner.filter(|o| {
            o.project_id == row.project_id && o.location.directory == row.directory && o.location.workspace_id.is_none()
        }) else {
            return Err(AutomationError::Invalid("The session that authorized this watch moved or was deleted".into()));
        };
        let env = self.env(&owner, &row.directory).await?;
        let authorized = row.event_position.as_ref().and_then(|p| p.get("agent")).and_then(Value::as_str);
        let Some(agent) = env.agent.as_ref().filter(|a| Some(a.id.as_str()) == authorized) else {
            return Err(AutomationError::Invalid("The session agent that authorized this watch changed".into()));
        };
        let app = self.app().ok_or(AutomationError::Declined)?;
        let input = AssertInput {
            id: None,
            session_id: owner.id.clone(),
            action: crate::mcp_events::ACTION.into(),
            resources: vec![server.to_string()],
            save: None,
            metadata: None,
            source: None,
            agent: Some(agent.id.clone()),
        };
        let allow = vec![oc_schema::types::PermissionRule {
            action: crate::mcp_events::ACTION.into(),
            resource: server.to_string(),
            effect: oc_schema::types::Effect::Allow,
        }];
        crate::permission::unattended(allow, app.permissions.assert(input)).await.map_err(AutomationError::from_permission)
    }

    /// `expireWatch(row, now)`
    fn expire_watch(&self, row: &Row, now: i64) {
        self.settle(row, &[("status", text("done")), ("last_outcome", text("expired")), ("time_updated", Sql::Integer(now))], false);
    }

    /// `event_position` with `cursor` (null allowed) over `position` and the current row's.
    fn position_with(position: &Value, current: Option<&Value>, cursor: Option<&str>) -> Value {
        let mut out = position.as_object().cloned().unwrap_or_default();
        if let Some(Value::Object(current)) = current {
            for (k, v) in current {
                out.insert(k.clone(), v.clone());
            }
        }
        out.insert("cursor".into(), json!(cursor));
        Value::Object(out)
    }

    /// Opens the watch's stream once and processes it until it fails or should stop (`open` in `watch`): `Ok` ends
    /// the watch quietly, an `Err` from `Failure::Disconnected` may reopen.
    async fn watch_once(self: &Arc<Self>, id: &str, delivery: &tokio::sync::Mutex<()>, attempts: &mut u32) -> Result<(), WatchError> {
        let row = self.find(id)?;
        if row.status != "active" || row.trigger_type() != "event" {
            return Ok(());
        }
        let trigger = row.trigger.clone();
        let server = trigger.get("server").and_then(Value::as_str).unwrap_or_default().to_string();
        let now = self.clock.now();
        if row.expires_at.is_some_and(|e| now >= e) {
            self.expire_watch(&row, now);
            return Ok(());
        }
        self.check_watch(&row, &server).await?;
        let Some(position) = row.event_position.clone() else {
            return Err(AutomationError::Invalid("No authorized event watch".into()).into());
        };
        let cursor = match position.get("cursor") {
            Some(Value::Null) => return Err(AutomationError::Invalid("No replay cursor; recreate the watch to start from now".into()).into()),
            Some(Value::String(cursor)) => Some(cursor.clone()),
            _ => None,
        };
        let events = self.mcp_events(&row.directory, &server).await?;
        let mut source = event::resolve(events, &trigger, cursor.as_deref()).await?;
        // Even a crash before the first heartbeat must not silently turn a restart into a fresh watch.
        if cursor.is_none() {
            self.settle(&row, &[("event_position", Sql::Text(Self::position_with(&position, None, None).to_string()))], false);
        }
        loop {
            let message = source.next().await?;
            let _delivering = delivery.lock().await;
            let current = self.find(id)?;
            let at = self.clock.now();
            if current.status != "active" || !self.claim_leader(at) {
                return Ok(());
            }
            if current.expires_at.is_some_and(|e| at >= e) {
                self.expire_watch(&current, at);
                return Ok(());
            }
            self.check_watch(&current, &server).await?;
            let oc_mcp::events::EventMessage::Event { event_id, name, timestamp, data, cursor } = message else {
                let position = Self::position_with(&position, current.event_position.as_ref(), message.cursor());
                self.settle(&current, &[("event_position", Sql::Text(position.to_string()))], false);
                *attempts = 0;
                continue;
            };
            *attempts = 0;
            if current.event_position.as_ref().and_then(|p| p.get("id")).and_then(Value::as_str) == Some(event_id.as_str()) {
                continue;
            }
            let message = event::Message { event_id, name, timestamp, data, cursor };
            self.run_action(&current, at, Some(&message)).await?;
            if self.find(id)?.status != "active" {
                return Ok(());
            }
        }
    }

    /// `watch(id, expires)`: follows one event automation until it is paused, done or removed, or this process loses
    /// the lease. A broken stream reopens from the last committed cursor up to three times in a row (1, 2 and 4 s
    /// later); any other failure pauses the automation with the reason. Expiry stops admission, but cannot cut a
    /// delivery between its inbox write and cursor commit.
    async fn watch(self: Arc<Self>, id: String, expires: Option<i64>) {
        let delivery = tokio::sync::Mutex::new(());
        let open = async {
            let mut attempts = 0u32;
            loop {
                match self.watch_once(&id, &delivery, &mut attempts).await {
                    Ok(()) => return Ok(()),
                    Err(WatchError::Disconnected(message)) => {
                        let cursor = self.find(&id)?.event_position.and_then(|p| p.get("cursor").cloned()).filter(|c| !c.is_null());
                        if cursor.is_none() {
                            return Err(AutomationError::Invalid("No replay cursor; recreate the watch to start from now".into()));
                        }
                        if attempts >= 3 {
                            return Err(AutomationError::Other(message));
                        }
                        tokio::time::sleep(Duration::from_secs(1 << attempts)).await;
                        attempts += 1;
                    }
                    Err(WatchError::Failed(error)) => return Err(error),
                }
            }
        };
        let result = match expires {
            None => open.await,
            Some(expires) => {
                let wait = Duration::from_millis((expires - self.clock.now()).max(0) as u64);
                tokio::select! {
                    result = open => result,
                    _ = async {
                        tokio::time::sleep(wait).await;
                        let _delivering = delivery.lock().await;
                        if let Ok(row) = self.find(&id) {
                            self.expire_watch(&row, self.clock.now());
                        }
                    } => Ok(()),
                }
            }
        };
        let Err(error) = result else { return };
        if matches!(error, AutomationError::NotFound(_)) {
            return;
        }
        if let Ok(row) = self.find(&id)
            && row.status == "active"
        {
            let reason: String = error.message().chars().take(1024).collect();
            self.pause_with(&row, self.clock.now(), &reason);
        }
    }

    /// `reconcileWatches`: the lease/doorbell loop also sees changes from other processes; it never polls a producer
    /// for events. Returns how many watches are active.
    fn reconcile_watches(self: &Arc<Self>) -> usize {
        let active: Vec<(String, Option<i64>)> = self
            .db
            .with(|c| {
                let mut stmt = c.prepare("SELECT id, expires_at FROM automation WHERE status = 'active' AND next_due_at IS NULL")?;
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap_or_default();
        let mut watches = self.watches.lock();
        watches.retain(|id, watch| {
            let keep = active.iter().any(|(a, _)| a == id);
            if !keep {
                watch.abort();
            }
            keep
        });
        for (id, expires) in &active {
            if watches.get(id).is_none_or(|w| w.is_finished()) {
                watches.insert(id.clone(), tokio::spawn(self.clone().watch(id.clone(), *expires)));
            }
        }
        active.len()
    }

    /// `list({sessionID, projectID})`: oldest first.
    pub fn list(&self, input: &ListInput) -> Vec<Value> {
        self.rows(input).iter().map(Row::info).collect()
    }

    pub fn rows(&self, input: &ListInput) -> Vec<Row> {
        self.db
            .with(|c| {
                let mut filters = Vec::new();
                let mut params = Vec::new();
                if let Some(s) = &input.session_id {
                    filters.push("session_id = ?");
                    params.push(s.clone());
                }
                if let Some(p) = &input.project_id {
                    filters.push("project_id = ?");
                    params.push(p.clone());
                }
                let filter = if filters.is_empty() { String::new() } else { format!("WHERE {}", filters.join(" AND ")) };
                let mut stmt = c.prepare(&format!("SELECT {COLUMNS} FROM automation {filter} ORDER BY time_created ASC, id ASC"))?;
                stmt.query_map(rusqlite::params_from_iter(params), Row::read)?.collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap_or_default()
    }

    pub fn get(&self, id: &str) -> Result<Value, AutomationError> {
        Ok(self.find(id)?.info())
    }

    pub fn pause(&self, id: &str) -> Result<Value, AutomationError> {
        let row = self.find(id)?;
        if row.status != "active" {
            return Ok(row.info());
        }
        let now = self.clock.now();
        let _ = self.db.with(|c| {
            c.execute("UPDATE automation SET status = 'paused', time_updated = ? WHERE id = ? AND status = 'active'", rusqlite::params![now, id])
        });
        if row.trigger_type() == "event" {
            self.remove_watch(id);
        }
        self.get(id)
    }

    pub fn resume(&self, id: &str) -> Result<Value, AutomationError> {
        let row = self.find(id)?;
        if row.status != "paused" {
            return Ok(row.info());
        }
        let now = self.clock.now();
        // An `at` that fell due while paused runs once, late, unless it has expired; a repeating automation resumes
        // at its next slot.
        let next = match row.next_due_at {
            None => None,
            Some(due) if due > now => Some(due),
            Some(due) => Some(following(&row.trigger, &row.tz, row.next_due_at, now).unwrap_or(due)),
        };
        let done = (row.trigger_type() != "event" && (next.is_none() || finished(row.expires_at, next))) || expired(row.expires_at, now);
        let _ = self.db.with(|c| {
            c.execute(
                "UPDATE automation SET status = ?, next_due_at = ?, failures = 0, retry_at = NULL, time_updated = ? WHERE id = ? AND status = 'paused'",
                rusqlite::params![if done { "done" } else { "active" }, if done { None } else { next }, now, id],
            )
        });
        self.doorbell.open();
        self.get(id)
    }

    pub fn remove(&self, id: &str) -> Result<(), AutomationError> {
        self.find(id)?;
        let _ = self.db.with(|c| c.execute("DELETE FROM automation WHERE id = ?", [id]));
        self.remove_watch(id);
        Ok(())
    }
}

/// Node's `path.isAbsolute` for the platform.
pub fn is_absolute(input: &str) -> bool {
    std::path::Path::new(input).is_absolute() || (cfg!(windows) && (input.starts_with('/') || input.starts_with('\\')))
}

/// The location settings from its config entries: the `automations` and `mcp_events` flags and `timezone`.
pub fn settings_of(entries: &[oc_config::Entry]) -> Settings {
    Settings {
        enabled: oc_config::flag::enabled(entries, "automations"),
        events: oc_config::flag::enabled(entries, "mcp_events"),
        timezone: oc_config::fold::latest(entries, |i| i.timezone.as_ref()).cloned(),
    }
}
