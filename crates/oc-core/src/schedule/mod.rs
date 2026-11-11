//! Scheduled prompts (`core/src/schedule.ts`, `schedule/sql.ts`, `schema/src/schedule.ts`), behind
//! `experimental.schedules`.
//!
//! A schedule wakes its session with a prompt once after a delay (`in`), at a time (`at`), every interval (`every`)
//! or on a cron, optionally gated by a shell or Code Mode check (`if` / `until`, see [`check`]). Rows live in the
//! `schedule` table; [`Schedules::run`] fires due rows. Every server process runs the loop, and a lease on KV
//! `schedule:leader` (TTL 60 s, heartbeat 20 s while ticking, released when no schedule is active) elects one leader
//! per database, so a schedule fires once across processes sharing a data directory.

pub mod check;
pub mod time;
pub mod tools;

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

use crate::permission::PermissionError;

pub use time::Zone;

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const MIN_INTERVAL: f64 = MINUTE as f64;
const MIN_CHECK_INTERVAL: f64 = 15_000.0;
const DEFAULT_UNTIL_AFTER: i64 = 24 * HOUR;
/// A firing this far past its due time is marked late.
const LATE_AFTER: i64 = 60_000;
/// The loop re-reads the table at least this often, so clock jumps and missed doorbells are caught.
pub const MAX_SLEEP: i64 = 30_000;
pub const LEADER_KEY: &str = "schedule:leader";
pub const LEADER_TTL: i64 = 2 * MAX_SLEEP;
/// How long a due schedule waits before its location's flag is read again, while schedules are off there.
const DISABLED_RETRY: i64 = HOUR;
/// How long a schedule whose prompt failed waits before trying again.
const FAILED_RETRY: i64 = 5 * MINUTE;
/// How long a schedule whose check failed waits before trying again, up to MAX_CHECK_FAILURES.
const CHECK_FAILED_RETRY: i64 = MINUTE;
const MAX_CHECK_FAILURES: i64 = 3;
const TICK_CONCURRENCY: usize = 4;

/// `Schedule.DisabledError`'s message.
pub const DISABLED: &str = "Scheduled tasks are disabled; set experimental.schedules in config";

/// The `/schedule` command template (`plugin/command/schedule.txt`).
pub const COMMAND_TEMPLATE: &str = include_str!("../../../../assets/prompt/command/schedule.txt");
pub const COMMAND: &str = "schedule";
pub const COMMAND_DESCRIPTION: &str = "schedule a prompt for later, or list, pause and delete this session's schedules";
pub const PLUGIN_ID: &str = "opencode.tool.schedule";

/// The failures of the schedule service.
#[derive(Debug, Clone, PartialEq)]
pub enum ScheduleError {
    /// `Schedule.NotFoundError`
    NotFound(String),
    /// `Schedule.InvalidError`
    Invalid(String),
    /// `Schedule.DisabledError`
    Disabled,
    /// `Session.NotFoundError`
    SessionNotFound(String),
    /// `Permission.BlockedError` (its message)
    Blocked(String),
    /// `Permission.CorrectedError` (the feedback)
    Corrected(String),
    /// A declined permission: a defect upstream (it ends the tool call, or the HTTP request with a 500).
    Declined,
}

impl ScheduleError {
    pub fn message(&self) -> String {
        match self {
            ScheduleError::NotFound(id) => format!("Schedule not found: {id}"),
            ScheduleError::Invalid(reason) => reason.clone(),
            ScheduleError::Disabled => DISABLED.to_string(),
            ScheduleError::SessionNotFound(id) => format!("Session not found: {id}"),
            ScheduleError::Blocked(message) => message.clone(),
            ScheduleError::Corrected(feedback) => feedback.clone(),
            ScheduleError::Declined => "The user declined this tool call".to_string(),
        }
    }

    pub(crate) fn from_permission(error: PermissionError) -> ScheduleError {
        match error {
            blocked @ PermissionError::Blocked { .. } => ScheduleError::Blocked(blocked.to_string()),
            PermissionError::Corrected { feedback } => ScheduleError::Corrected(feedback),
            PermissionError::Declined => ScheduleError::Declined,
        }
    }
}

impl std::fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// Why a firing step failed (`tick`'s cause handling).
#[derive(Debug, Clone)]
pub enum StepError {
    /// `FileSystem.DirectoryNotFoundError`: the session's folder is gone; the schedule pauses.
    DirectoryNotFound(String),
    Other(String),
}

/// `Schedule.CreateInput`: the decoded request, with `when` and the conditions in their tagged forms.
#[derive(Debug, Clone, Default)]
pub struct CreateInput {
    pub session_id: String,
    pub name: Option<String>,
    /// `{type, in|at|every|cron, tz?}`
    pub when: Option<Value>,
    /// `{type:"shell", run, is?, matches?, changes?}` or `{type:"code", code}`
    pub r#if: Option<Value>,
    pub until: Option<Value>,
    /// `{after?, at?, times?}`
    pub ends: Option<Value>,
    pub prompt: String,
}

impl CreateInput {
    /// `Schedule.toCreateInput(sessionID, request)` over a `Schedule.Request` JSON object (already decoded).
    pub fn from_request(session_id: &str, request: &Value) -> CreateInput {
        let get = |k: &str| request.get(k).filter(|v| !v.is_null()).cloned();
        CreateInput {
            session_id: session_id.to_string(),
            name: request.get("name").and_then(Value::as_str).map(String::from),
            when: get("when").map(|w| to_when(&w)),
            r#if: get("if").map(|c| to_condition(&c)),
            until: get("until").map(|c| to_condition(&c)),
            ends: get("ends"),
            prompt: request.get("prompt").and_then(Value::as_str).unwrap_or_default().to_string(),
        }
    }
}

fn to_when(when: &Value) -> Value {
    let mut out = Map::new();
    for kind in ["in", "at", "every", "cron"] {
        if let Some(v) = when.get(kind).filter(|v| !v.is_null()) {
            out.insert("type".into(), json!(kind));
            out.insert(kind.into(), v.clone());
            break;
        }
    }
    if out.is_empty() {
        out.insert("type".into(), json!("cron"));
    }
    if let Some(tz) = when.get("tz").filter(|v| !v.is_null()) {
        out.insert("tz".into(), tz.clone());
    }
    Value::Object(out)
}

fn to_condition(condition: &Value) -> Value {
    if let Some(code) = condition.get("code").filter(|v| !v.is_null()) {
        return json!({ "type": "code", "code": code });
    }
    let mut out = Map::new();
    out.insert("type".into(), json!("shell"));
    for key in ["run", "is", "matches", "changes"] {
        if let Some(v) = condition.get(key).filter(|v| !v.is_null()) {
            out.insert(key.into(), v.clone());
        }
    }
    Value::Object(out)
}

/// `Schedule.When` in schema key order.
pub fn encode_when(when: &Value) -> Value {
    let kind = when.get("type").and_then(Value::as_str).unwrap_or_default();
    let mut out = Map::new();
    out.insert("type".into(), json!(kind));
    if let Some(v) = when.get(kind) {
        out.insert(kind.into(), v.clone());
    }
    if let Some(tz) = when.get("tz").filter(|v| !v.is_null()) {
        out.insert("tz".into(), tz.clone());
    }
    Value::Object(out)
}

/// `Schedule.Condition` in schema key order.
pub fn encode_condition(condition: &Value) -> Value {
    let kind = condition.get("type").and_then(Value::as_str).unwrap_or_default();
    let keys: &[&str] = if kind == "code" { &["type", "code"] } else { &["type", "run", "is", "matches", "changes"] };
    let mut out = Map::new();
    for key in keys {
        if let Some(v) = condition.get(*key).filter(|v| !v.is_null()) {
            out.insert((*key).into(), v.clone());
        }
    }
    Value::Object(out)
}

/// A `schedule` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: String,
    pub session_id: Option<String>,
    pub name: Option<String>,
    pub action: Value,
    /// `{mode: "if"|"until", condition}`
    pub check: Option<Value>,
    pub when: Value,
    pub tz: String,
    pub status: String,
    pub next_due_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub remaining_runs: Option<i64>,
    pub run_count: i64,
    pub checks: i64,
    pub failures: i64,
    pub last_fired_at: Option<i64>,
    pub last_message_id: Option<String>,
    pub last_outcome: Option<String>,
    pub retry_at: Option<i64>,
    /// `null` is stored as SQL NULL.
    pub state: Value,
    pub time_created: i64,
    pub time_updated: i64,
}

const COLUMNS: &str = "id, session_id, name, action, \"check\", \"when\", tz, status, next_due_at, ends_at, remaining_runs, \
     run_count, checks, failures, last_fired_at, last_message_id, last_outcome, retry_at, state, time_created, time_updated";

fn json_column(text: Option<String>) -> Option<Value> {
    text.and_then(|t| serde_json::from_str(&t).ok())
}

impl Row {
    fn read(r: &rusqlite::Row) -> rusqlite::Result<Row> {
        Ok(Row {
            id: r.get(0)?,
            session_id: r.get(1)?,
            name: r.get(2)?,
            action: json_column(r.get(3)?).unwrap_or(Value::Null),
            check: json_column(r.get(4)?).filter(|v| !v.is_null()),
            when: json_column(r.get(5)?).unwrap_or(Value::Null),
            tz: r.get(6)?,
            status: r.get(7)?,
            next_due_at: r.get(8)?,
            ends_at: r.get(9)?,
            remaining_runs: r.get(10)?,
            run_count: r.get(11)?,
            checks: r.get(12)?,
            failures: r.get(13)?,
            last_fired_at: r.get(14)?,
            last_message_id: r.get(15)?,
            last_outcome: r.get(16)?,
            retry_at: r.get(17)?,
            state: json_column(r.get(18)?).unwrap_or(Value::Null),
            time_created: r.get(19)?,
            time_updated: r.get(20)?,
        })
    }

    pub fn prompt(&self) -> &str {
        self.action.get("prompt").and_then(Value::as_str).unwrap_or_default()
    }

    fn mode(&self) -> Option<&str> {
        self.check.as_ref().and_then(|c| c.get("mode")).and_then(Value::as_str)
    }

    fn condition(&self) -> Option<&Value> {
        self.check.as_ref().and_then(|c| c.get("condition"))
    }

    /// `toInfo(row, lastOutput)`: `Schedule.Info` JSON in schema key order (times in epoch ms).
    pub fn info(&self, last_output: Option<&str>) -> Value {
        let mut out = Map::new();
        out.insert("id".into(), json!(self.id));
        if let Some(s) = &self.session_id {
            out.insert("sessionID".into(), json!(s));
        }
        if let Some(n) = &self.name {
            out.insert("name".into(), json!(n));
        }
        out.insert("when".into(), encode_when(&self.when));
        out.insert("tz".into(), json!(self.tz));
        if self.mode() == Some("if")
            && let Some(c) = self.condition()
        {
            out.insert("if".into(), encode_condition(c));
        }
        if self.mode() == Some("until")
            && let Some(c) = self.condition()
        {
            out.insert("until".into(), encode_condition(c));
        }
        out.insert("action".into(), json!({ "type": "prompt", "prompt": self.prompt() }));
        out.insert("status".into(), json!(self.status));
        let mut opt = |key: &str, v: Option<i64>| {
            if let Some(v) = v {
                out.insert(key.into(), json!(v));
            }
        };
        opt("nextDueAt", self.next_due_at);
        opt("endsAt", self.ends_at);
        opt("remainingRuns", self.remaining_runs);
        out.insert("runCount".into(), json!(self.run_count));
        if let Some(v) = self.last_fired_at {
            out.insert("lastFiredAt".into(), json!(v));
        }
        if let Some(o) = &self.last_outcome {
            out.insert("lastOutcome".into(), json!(o));
        }
        if let Some(o) = last_output {
            out.insert("lastOutput".into(), json!(o));
        }
        out.insert("time".into(), json!({ "created": self.time_created, "updated": self.time_updated }));
        Value::Object(out)
    }
}

fn duration(value: &str) -> Result<f64, ScheduleError> {
    match time::duration_millis(value) {
        None => Err(ScheduleError::Invalid(format!("Invalid duration: {value}"))),
        Some(ms) if ms > 0.0 => Ok(ms),
        Some(_) => Err(ScheduleError::Invalid(format!("Duration must be positive: {value}"))),
    }
}

/// Stored durations were validated on create.
fn millis(value: &str) -> f64 {
    time::duration_millis(value).unwrap_or(f64::NAN)
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

/// `following(row, now)`: the first slot strictly after `now`, or `None` for one-shots.
pub fn following(when: &Value, tz: &str, next_due_at: Option<i64>, now: i64) -> Option<i64> {
    match when.get("type").and_then(Value::as_str)? {
        "every" => {
            let every = millis(when.get("every").and_then(Value::as_str).unwrap_or_default());
            let due = next_due_at.unwrap_or(now) as f64;
            let next = due + (((now as f64 - due) / every).floor() + 1.0) * every;
            next.is_finite().then_some(next as i64)
        }
        "cron" => {
            let expr = when.get("cron").and_then(Value::as_str).unwrap_or_default();
            let cron = time::cron_parse_tz(expr, tz).ok()?;
            next_cron(&cron, now)
        }
        _ => None,
    }
}

fn finished(ends_at: Option<i64>, next: Option<i64>, remaining: Option<i64>) -> bool {
    match next {
        None => true,
        Some(next) => remaining.is_some_and(|r| r <= 0) || ends_at.is_some_and(|e| next > e),
    }
}

fn prompt_text(prompt: &str, message: Option<&str>) -> String {
    match message {
        Some(m) if !m.trim().is_empty() => format!("{prompt}\n\n{m}"),
        _ => prompt.to_string(),
    }
}

/// `resolveStoredCheck`
fn resolve_stored_check(input: &CreateInput) -> Result<Option<Value>, ScheduleError> {
    if input.r#if.is_some() && input.until.is_some() {
        return Err(ScheduleError::Invalid("Choose either if or until, not both".into()));
    }
    let check = match (&input.r#if, &input.until) {
        (Some(c), _) => json!({ "mode": "if", "condition": c }),
        (_, Some(c)) => json!({ "mode": "until", "condition": c }),
        _ => return Ok(None),
    };
    check::validate(&check["condition"]).map_err(ScheduleError::Invalid)?;
    Ok(Some(check))
}

/// The location settings a schedule follows (`settings(session)`).
#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub enabled: bool,
    pub timezone: Option<String>,
}

/// What the schedule service needs from the server: admitting a prompt through the session API
/// (`sessions.prompt`, which also wakes the session).
pub trait Prompter: Send + Sync {
    fn prompt(&self, session_id: String, id: String, text: String, metadata: Map<String, Value>) -> BoxFuture<'static, Result<(), StepError>>;
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
    boot_checked: bool,
}

/// The schedule service (`Schedule.Service`).
pub struct Schedules {
    db: Database,
    app: Weak<crate::app::App>,
    engine: OnceLock<Weak<crate::runner::Engine>>,
    prompter: OnceLock<Arc<dyn Prompter>>,
    doorbell: Doorbell,
    owner: String,
    lease: Mutex<Lease>,
    clock: Clock,
}

impl Schedules {
    pub fn new(app: &Arc<crate::app::App>) -> Arc<Schedules> {
        Self::with_clock(app, Clock::System)
    }

    pub fn with_clock(app: &Arc<crate::app::App>, clock: Clock) -> Arc<Schedules> {
        Arc::new(Schedules {
            db: app.db.clone(),
            app: Arc::downgrade(app),
            engine: OnceLock::new(),
            prompter: OnceLock::new(),
            doorbell: Doorbell::default(),
            owner: uuid::Uuid::new_v4().to_string(),
            lease: Mutex::new(Lease::default()),
            clock,
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

    fn app(&self) -> Option<Arc<crate::app::App>> {
        self.app.upgrade()
    }

    fn session(&self, id: &str) -> Result<SessionInfo, ScheduleError> {
        match self.db.session_get(id) {
            Ok(Some(s)) => Ok(s),
            _ => Err(ScheduleError::SessionNotFound(id.to_string())),
        }
    }

    /// `settings(session)`: config is per location, so a schedule follows the config where its session lives.
    /// `Err` carries the directory when the session's folder is gone.
    pub async fn settings(&self, session: &SessionInfo) -> Result<Settings, String> {
        let directory = session.location.directory.clone();
        if !std::path::Path::new(&directory).is_dir() {
            return Err(directory);
        }
        let Some(engine) = self.engine() else { return Ok(Settings::default()) };
        let location = engine.location(&directory).await.map_err(|_| directory.clone())?;
        Ok(settings_of(&location.entries))
    }

    /// `enabled(sessionID)`: whether `experimental.schedules` is on where the session lives.
    pub async fn enabled(&self, session_id: &str) -> Result<bool, ScheduleError> {
        let session = self.session(session_id)?;
        Ok(self.settings(&session).await.map(|s| s.enabled).unwrap_or(false))
    }

    fn find(&self, id: &str) -> Result<Row, ScheduleError> {
        let row = self
            .db
            .with(|c| c.query_row(&format!("SELECT {COLUMNS} FROM schedule WHERE id = ?"), [id], Row::read).optional())
            .map_err(|e| ScheduleError::Invalid(e.to_string()))?;
        row.ok_or_else(|| ScheduleError::NotFound(id.to_string()))
    }

    /// The row, if any (tests and the HTTP layer).
    pub fn row(&self, id: &str) -> Option<Row> {
        self.find(id).ok()
    }

    /// `update(row, values)`: only while the row is still active.
    fn update(&self, id: &str, values: &[(&str, Sql)]) {
        self.set(id, values, true);
    }

    fn set(&self, id: &str, values: &[(&str, Sql)], only_active: bool) {
        let assignments: Vec<String> = values.iter().map(|(k, _)| format!("\"{k}\" = ?")).collect();
        let mut sql = format!("UPDATE schedule SET {} WHERE id = ?", assignments.join(", "));
        if only_active {
            sql.push_str(" AND status = 'active'");
        }
        let mut params: Vec<Sql> = values.iter().map(|(_, v)| v.clone()).collect();
        params.push(Sql::Text(id.to_string()));
        if let Err(e) = self.db.with(|c| c.execute(&sql, rusqlite::params_from_iter(params))) {
            tracing::error!(schedule = id, error = %e, "schedule update failed");
        }
    }

    fn pending(&self, message_id: &str) -> bool {
        self.db.with(|c| c.query_row("SELECT id FROM session_inbox WHERE id = ?", [message_id], |_| Ok(())).optional()).ok().flatten().is_some()
    }

    fn wake(&self, session_id: &str) {
        if let Some(prompter) = self.prompter.get() {
            prompter.wake(session_id);
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn admit_wake(
        &self,
        row: &Row,
        session_id: &str,
        run: i64,
        due: i64,
        late: bool,
        text: String,
        check_meta: Option<Value>,
    ) -> Result<String, StepError> {
        // The id is derived from `run_count`, so retrying after a crash reconciles with the earlier admission.
        let id = format!("msg_{}_{run}", row.id);
        let mut schedule = Map::new();
        schedule.insert("id".into(), json!(row.id));
        if let Some(n) = &row.name {
            schedule.insert("name".into(), json!(n));
        }
        schedule.insert("run".into(), json!(run));
        schedule.insert("dueAt".into(), json!(due));
        schedule.insert("late".into(), json!(late));
        if let Some(c) = check_meta {
            schedule.insert("check".into(), c);
        }
        let mut metadata = Map::new();
        metadata.insert("schedule".into(), Value::Object(schedule));
        let prompter = self.prompter.get().cloned().ok_or_else(|| StepError::Other("Session runner is not available".into()))?;
        prompter.prompt(session_id.to_string(), id.clone(), text, metadata).await?;
        Ok(id)
    }

    async fn fire(&self, row: &Row, session: &SessionInfo, due: i64, now: i64) -> Result<(), StepError> {
        let late = now - due >= LATE_AFTER;
        let next = following(&row.when, &row.tz, row.next_due_at, now);
        let text = |v: &str| Sql::Text(v.to_string());
        let int = Sql::Integer;
        let opt = |v: Option<i64>| v.map(Sql::Integer).unwrap_or(Sql::Null);
        let state = |v: &Value| if v.is_null() { Sql::Null } else { Sql::Text(v.to_string()) };

        if let Some(check) = &row.check {
            let mode = row.mode().unwrap_or("if").to_string();
            let condition = check.get("condition").cloned().unwrap_or(Value::Null);
            let message_id = format!("msg_{}_check_{}", row.id, row.checks + 1);
            let outcome = check::evaluate(self, session, &condition, &row.state, &message_id, now, row.checks).await?;
            match outcome {
                check::Outcome::Failed { error, invalid } => {
                    let strikes = row.failures + 1;
                    if !invalid && strikes < MAX_CHECK_FAILURES {
                        self.update(
                            &row.id,
                            &[
                                ("failures", int(strikes)),
                                ("last_outcome", text("failed")),
                                ("retry_at", int(now + CHECK_FAILED_RETRY)),
                                ("time_updated", int(now)),
                            ],
                        );
                        return Ok(());
                    }
                    let run = row.run_count + 1;
                    let note = if invalid {
                        format!("[Schedule paused after invalid check: {error}]")
                    } else {
                        format!("[Schedule paused after {strikes} failed checks: {error}]")
                    };
                    let id = self
                        .admit_wake(
                            row,
                            &session.id,
                            run,
                            due,
                            late,
                            format!("{}\n\n{note}", row.prompt()),
                            Some(json!({ "error": error, "failures": strikes })),
                        )
                        .await?;
                    self.set(
                        &row.id,
                        &[
                            ("status", text("paused")),
                            ("failures", int(strikes)),
                            ("run_count", int(run)),
                            ("last_message_id", text(&id)),
                            ("last_fired_at", int(now)),
                            ("last_outcome", text("failed")),
                            ("retry_at", Sql::Null),
                            ("time_updated", int(now)),
                        ],
                        false,
                    );
                    return Ok(());
                }
                check::Outcome::Ok { wake: false, state: new_state, .. } => {
                    let checks = row.checks + 1;
                    let ended = next.is_none() || matches!((row.ends_at, next), (Some(e), Some(n)) if n > e);
                    if mode == "until" && ended {
                        let run = row.run_count + 1;
                        let note = format!("[Schedule timed out without matching: {}]", check::describe(&condition));
                        let id = self
                            .admit_wake(row, &session.id, run, due, late, format!("{}\n\n{note}", row.prompt()), Some(json!({ "expired": true })))
                            .await?;
                        self.set(
                            &row.id,
                            &[
                                ("status", text("done")),
                                ("checks", int(checks)),
                                ("failures", int(0)),
                                ("state", state(&new_state)),
                                ("run_count", int(run)),
                                ("last_message_id", text(&id)),
                                ("last_fired_at", int(now)),
                                ("last_outcome", text("expired")),
                                ("next_due_at", Sql::Null),
                                ("remaining_runs", int(0)),
                                ("retry_at", Sql::Null),
                                ("time_updated", int(now)),
                            ],
                            false,
                        );
                        return Ok(());
                    }
                    let mut values = vec![
                        ("checks", int(checks)),
                        ("failures", int(0)),
                        ("state", state(&new_state)),
                        ("last_outcome", text("checked")),
                        ("retry_at", Sql::Null),
                    ];
                    if ended {
                        values.push(("status", text("done")));
                        values.push(("next_due_at", Sql::Null));
                    } else {
                        values.push(("next_due_at", opt(next)));
                    }
                    values.push(("time_updated", int(now)));
                    self.update(&row.id, &values);
                    return Ok(());
                }
                check::Outcome::Ok { wake: true, state: new_state, message } => {
                    let run = row.run_count + 1;
                    let meta = match &message {
                        Some(m) => json!({ "message": m }),
                        None => json!({}),
                    };
                    let id = self.admit_wake(row, &session.id, run, due, late, prompt_text(row.prompt(), message.as_deref()), Some(meta)).await?;
                    let remaining = if mode == "until" { Some(0) } else { row.remaining_runs.map(|r| r - 1) };
                    let done = mode == "until" || finished(row.ends_at, next, remaining);
                    // Record the run even if a pause landed during the prompt, or the next firing would reuse this
                    // message id.
                    let mut values = vec![
                        ("run_count", int(run)),
                        ("checks", int(row.checks + 1)),
                        ("failures", int(0)),
                        ("state", state(&new_state)),
                        ("last_message_id", text(&id)),
                        ("last_fired_at", int(now)),
                        ("last_outcome", text(if late { "late" } else { "fired" })),
                        ("retry_at", Sql::Null),
                        ("remaining_runs", opt(remaining)),
                    ];
                    if done {
                        values.push(("status", text("done")));
                    }
                    values.push(("next_due_at", if done { Sql::Null } else { opt(next) }));
                    values.push(("time_updated", int(now)));
                    self.set(&row.id, &values, false);
                    return Ok(());
                }
            }
        }

        let run = row.run_count + 1;
        let id = self.admit_wake(row, &session.id, run, due, late, row.prompt().to_string(), None).await?;
        let remaining = row.remaining_runs.map(|r| r - 1);
        let done = finished(row.ends_at, next, remaining);
        let mut values = vec![
            ("run_count", int(run)),
            ("last_message_id", text(&id)),
            ("last_fired_at", int(now)),
            ("last_outcome", text(if late { "late" } else { "fired" })),
            ("retry_at", Sql::Null),
            ("remaining_runs", opt(remaining)),
        ];
        if done {
            values.push(("status", text("done")));
        }
        values.push(("next_due_at", if done { Sql::Null } else { opt(next) }));
        values.push(("time_updated", int(now)));
        self.set(&row.id, &values, false);
        Ok(())
    }

    async fn step(&self, row: &Row, now: i64) -> Result<(), StepError> {
        let (Some(due), Some(session_id)) = (row.next_due_at, row.session_id.as_deref()) else { return Ok(()) };
        if !self.claim_leader(self.clock.now()) {
            return Ok(());
        }
        let session = self.session(session_id).map_err(|e| StepError::Other(e.message()))?;
        let settings = self.settings(&session).await.map_err(StepError::DirectoryNotFound)?;
        if !settings.enabled {
            // Reading the flag boots the session's location, so check again in an hour rather than every tick. The
            // row keeps its due time, so once the flag is back on it catches up like any missed slot.
            self.update(
                &row.id,
                &[
                    ("last_outcome", Sql::Text("disabled".into())),
                    ("retry_at", Sql::Integer(now + DISABLED_RETRY)),
                    ("time_updated", Sql::Integer(now)),
                ],
            );
            return Ok(());
        }
        if let Some(last) = &row.last_message_id
            && self.pending(last)
        {
            // The wake that followed admission may have been lost; waking again is harmless.
            self.wake(&session.id);
            let next = following(&row.when, &row.tz, row.next_due_at, now);
            let done = finished(row.ends_at, next, row.remaining_runs);
            self.update(
                &row.id,
                &[
                    ("last_outcome", Sql::Text("skipped".into())),
                    ("retry_at", Sql::Null),
                    ("status", Sql::Text(if done { "done" } else { "active" }.into())),
                    ("next_due_at", if done { Sql::Null } else { next.map(Sql::Integer).unwrap_or(Sql::Null) }),
                    ("time_updated", Sql::Integer(now)),
                ],
            );
            return Ok(());
        }
        self.fire(row, &session, due, now).await
    }

    fn due(&self, now: i64) -> Vec<Row> {
        self.db
            .with(|c| {
                let mut stmt = c.prepare(&format!(
                    "SELECT {COLUMNS} FROM schedule WHERE status = 'active' AND next_due_at <= ?1 \
                     AND (retry_at IS NULL OR retry_at <= ?1) ORDER BY next_due_at ASC"
                ))?;
                stmt.query_map([now], Row::read)?.collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "schedule tick failed");
                Vec::new()
            })
    }

    /// `tickAt(now)`: fires every schedule due at `now`, at most once each.
    pub async fn tick_at(&self, now: i64) {
        use futures::StreamExt;
        let due = self.due(now);
        let concurrency = if due.len() > 1 { TICK_CONCURRENCY } else { 1 };
        futures::stream::iter(due)
            .for_each_concurrent(concurrency, |row| async move {
                match self.step(&row, now).await {
                    Ok(()) => {}
                    Err(StepError::DirectoryNotFound(_)) => {
                        // The session's folder (e.g. a removed worktree) is gone; pause rather than retrying every 5
                        // minutes.
                        self.update(
                            &row.id,
                            &[
                                ("status", Sql::Text("paused".into())),
                                ("last_outcome", Sql::Text("failed".into())),
                                ("retry_at", Sql::Null),
                                ("time_updated", Sql::Integer(now)),
                            ],
                        );
                    }
                    Err(StepError::Other(cause)) => {
                        // Back off rather than retrying a failing prompt on every tick.
                        tracing::warn!(schedule = %row.id, cause = %cause, "schedule failed to fire");
                        self.update(
                            &row.id,
                            &[
                                ("last_outcome", Sql::Text("failed".into())),
                                ("retry_at", Sql::Integer(now + FAILED_RETRY)),
                                ("time_updated", Sql::Integer(now)),
                            ],
                        );
                    }
                }
            })
            .await;
    }

    /// `tick`: fires every schedule that is due now.
    pub async fn tick(&self) {
        self.tick_at(self.clock.now()).await
    }

    /// `nextWake(from)`: rows due by `from` that could not fire are rechecked after at most MAX_SLEEP.
    fn next_wake(&self, from: i64) -> i64 {
        let at: Option<i64> = self
            .db
            .with(|c| c.query_row("SELECT min(next_due_at) FROM schedule WHERE status = 'active' AND next_due_at > ?", [from], |r| r.get(0)))
            .ok()
            .flatten();
        match at {
            None => MAX_SLEEP,
            Some(at) => (at - self.clock.now()).clamp(0, MAX_SLEEP),
        }
    }

    /// `rewake`: a crash between admitting a prompt and its session claiming the work leaves the prompt unclaimed and
    /// the session asleep, so wake every session whose last scheduled prompt is still pending.
    fn rewake(&self) {
        let sessions: Vec<Option<String>> = self
            .db
            .with(|c| {
                let mut stmt = c.prepare(
                    "SELECT DISTINCT schedule.session_id FROM schedule INNER JOIN session_inbox ON session_inbox.id = schedule.last_message_id",
                )?;
                stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "schedule rewake failed");
                Vec::new()
            });
        for session in sessions.into_iter().flatten() {
            self.wake(&session);
        }
    }

    fn has_active_rows(&self) -> bool {
        self.db.with(|c| c.query_row("SELECT id FROM schedule WHERE status = 'active' LIMIT 1", [], |_| Ok(())).optional()).ok().flatten().is_some()
    }

    /// `claimLeader(now)`: takes or renews the lease on KV `schedule:leader` when it is ours or has expired.
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
        self.lease.lock().leading = false;
        let _ = self
            .db
            .with(|c| c.execute("DELETE FROM kv WHERE key = ? AND json_extract(value, '$.owner') = ?", rusqlite::params![LEADER_KEY, self.owner]));
    }

    /// One iteration of the loop.
    async fn iteration(self: &Arc<Self>) {
        self.doorbell.close();
        let active = self.has_active_rows();
        let (leading, boot_checked) = {
            let lease = self.lease.lock();
            (lease.leading, lease.boot_checked)
        };
        if !active && boot_checked {
            if leading {
                self.release_leader();
            }
            self.doorbell.wait(MAX_SLEEP).await;
            return;
        }
        let now = self.clock.now();
        if !self.claim_leader(now) {
            {
                let mut lease = self.lease.lock();
                lease.leading = false;
                lease.boot_checked = true;
            }
            self.doorbell.wait(MAX_SLEEP).await;
            return;
        }
        if !leading {
            {
                let mut lease = self.lease.lock();
                lease.leading = true;
                lease.boot_checked = true;
            }
            self.rewake();
        }
        if !active {
            self.release_leader();
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
        if !self.has_active_rows() {
            self.release_leader();
            self.doorbell.wait(MAX_SLEEP).await;
            return;
        }
        let wait = self.next_wake(now);
        if wait > 0 {
            self.doorbell.wait(wait).await;
        }
    }

    /// `run`: ticks whenever a schedule falls due, until the task is dropped; the lease is released on the way out.
    pub async fn run(self: Arc<Self>) {
        struct Release(Arc<Schedules>);
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

    fn resolve_zone(tz: Option<&str>) -> Result<Zone, ScheduleError> {
        match tz {
            None => Ok(time::zone_make_local()),
            Some(tz) => time::zone_make_named(tz).ok_or_else(|| ScheduleError::Invalid(format!("Unknown time zone: {tz}"))),
        }
    }

    fn first_due(when: &Value, has_check: bool, zone: &Zone, now: i64) -> Result<i64, ScheduleError> {
        let s = |k: &str| when.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        match when.get("type").and_then(Value::as_str).unwrap_or_default() {
            "in" => Ok(now + duration(&s("in"))? as i64),
            "at" => {
                let input = s("at");
                let Some(at) = time::parse_at(&input, zone) else {
                    return Err(ScheduleError::Invalid(format!("Invalid time: {input}")));
                };
                if at <= now {
                    return Err(ScheduleError::Invalid(format!("Time is in the past: {input}")));
                }
                Ok(at)
            }
            "every" => {
                let every = duration(&s("every"))?;
                let min = if has_check { MIN_CHECK_INTERVAL } else { MIN_INTERVAL };
                if every < min {
                    return Err(ScheduleError::Invalid(
                        if has_check { "every must be at least 15 seconds" } else { "every must be at least 1 minute" }.into(),
                    ));
                }
                Ok(now + every as i64)
            }
            _ => {
                let expr = s("cron");
                let cron = time::cron_parse(&expr, Some(zone)).map_err(ScheduleError::Invalid)?;
                if cron.seconds_len() != 1 {
                    return Err(ScheduleError::Invalid("cron must fire at most once a minute".into()));
                }
                next_cron(&cron, now).ok_or_else(|| ScheduleError::Invalid(format!("cron never fires: {expr}")))
            }
        }
    }

    fn ends_at(ends: Option<&Value>, is_until: bool, zone: &Zone, now: i64) -> Result<Option<i64>, ScheduleError> {
        let after = match ends.and_then(|e| e.get("after")).and_then(Value::as_str) {
            Some(a) => Some(now + duration(a)? as i64),
            None => None,
        };
        let Some(at) = ends.and_then(|e| e.get("at")).and_then(Value::as_str) else {
            if after.is_some() {
                return Ok(after);
            }
            return Ok(is_until.then_some(now + DEFAULT_UNTIL_AFTER));
        };
        let Some(parsed) = time::parse_at(at, zone) else {
            return Err(ScheduleError::Invalid(format!("Invalid time: {at}")));
        };
        Ok(Some(parsed.min(after.unwrap_or(parsed))))
    }

    /// `create(input)`: validates, authorizes and seeds a check, then stores the schedule. Returns `Schedule.Info`.
    pub async fn create(&self, input: CreateInput) -> Result<Value, ScheduleError> {
        let session = self.session(&input.session_id)?;
        let missing = |dir: String| ScheduleError::Invalid(format!("Session directory does not exist: {dir}"));
        let config = self.settings(&session).await.map_err(missing)?;
        if !config.enabled {
            return Err(ScheduleError::Disabled);
        }
        let check = resolve_stored_check(&input)?;
        let mode = check.as_ref().and_then(|c| c.get("mode")).and_then(Value::as_str).map(String::from);
        let when = match (&input.when, mode.as_deref()) {
            (Some(w), _) => w.clone(),
            (None, Some("until")) => json!({ "type": "every", "every": "30 seconds" }),
            _ => return Err(ScheduleError::Invalid("when is required unless until is set".into())),
        };
        let tz = when.get("tz").and_then(Value::as_str).map(String::from).or(config.timezone.clone());
        let zone = Self::resolve_zone(tz.as_deref())?;
        let now = self.clock.now();
        let due = Self::first_due(&when, check.is_some(), &zone, now)?;
        if input.ends.as_ref().and_then(|e| e.get("times")).and_then(Value::as_f64).is_some_and(|t| t < 1.0) {
            return Err(ScheduleError::Invalid("ends.times must be at least 1".into()));
        }
        let ends = Self::ends_at(input.ends.as_ref(), mode.as_deref() == Some("until"), &zone, now)?;
        if ends.is_some_and(|e| due > e) {
            return Err(ScheduleError::Invalid("The schedule ends before it first fires".into()));
        }

        let id = oc_schema::id::ascending("sch");
        let mut seeded_state = Value::Null;
        let mut seeded_checks = 0;
        let mut matched: Option<Option<String>> = None;
        if let Some(check) = &check {
            let condition = &check["condition"];
            let mode = mode.as_deref().unwrap_or("if");
            check::authorize(self, &session, input.name.as_deref(), &when, mode, condition).await?;
            let kind = condition.get("type").and_then(Value::as_str);
            let should_seed =
                mode == "until" || kind == Some("code") || (kind == Some("shell") && condition.get("changes") == Some(&Value::Bool(true)));
            if should_seed {
                let init =
                    check::evaluate(self, &session, condition, &Value::Null, &format!("msg_{id}_check_0"), now, 0).await.map_err(|e| match e {
                        StepError::DirectoryNotFound(dir) => missing(dir),
                        StepError::Other(message) => ScheduleError::Invalid(message),
                    })?;
                match init {
                    check::Outcome::Failed { error, invalid } => {
                        // A code check that throws or fails at creation is broken (e.g. wrong MCP response field).
                        if invalid || kind == Some("code") {
                            return Err(ScheduleError::Invalid(error));
                        }
                    }
                    check::Outcome::Ok { wake: true, state, message } if mode == "until" => {
                        seeded_state = state;
                        matched = Some(message);
                    }
                    check::Outcome::Ok { state, .. } => {
                        seeded_state = state;
                        seeded_checks = 1;
                    }
                }
            }
        }

        let tz = time::zone_to_string(&zone);
        let check_text = check.as_ref().map(|c| c.to_string());
        let state_text = (!seeded_state.is_null()).then(|| seeded_state.to_string());
        let action = json!({ "type": "prompt", "prompt": input.prompt }).to_string();
        let when_text = when.to_string();
        let ends_sql = ends.map(Sql::Integer).unwrap_or(Sql::Null);
        let insert = |values: Vec<(&str, Sql)>| -> Result<Row, ScheduleError> {
            let columns: Vec<String> = values.iter().map(|(k, _)| format!("\"{k}\"")).collect();
            let marks: Vec<&str> = values.iter().map(|_| "?").collect();
            let sql = format!("INSERT INTO schedule ({}) VALUES ({}) RETURNING {COLUMNS}", columns.join(", "), marks.join(", "));
            let params: Vec<Sql> = values.into_iter().map(|(_, v)| v).collect();
            self.db.with(|c| c.query_row(&sql, rusqlite::params_from_iter(params), Row::read)).map_err(|e| ScheduleError::Invalid(e.to_string()))
        };
        let opt_text = |v: &Option<String>| v.clone().map(Sql::Text).unwrap_or(Sql::Null);
        if let Some(message) = matched {
            let row = insert(vec![
                ("id", Sql::Text(id.clone())),
                ("session_id", Sql::Text(session.id.clone())),
                ("name", opt_text(&input.name)),
                ("check", opt_text(&check_text)),
                ("state", opt_text(&state_text)),
                ("checks", Sql::Integer(1)),
                ("action", Sql::Text(action)),
                ("when", Sql::Text(when_text)),
                ("tz", Sql::Text(tz)),
                ("status", Sql::Text("done".into())),
                ("next_due_at", Sql::Null),
                ("ends_at", ends_sql),
                ("remaining_runs", Sql::Integer(0)),
                ("run_count", Sql::Integer(1)),
                ("last_fired_at", Sql::Integer(now)),
                ("last_outcome", Sql::Text("fired".into())),
                ("time_created", Sql::Integer(now)),
                ("time_updated", Sql::Integer(now)),
            ])?;
            return Ok(row.info(message.as_deref()));
        }
        let remaining =
            if mode.as_deref() == Some("until") { Some(1) } else { input.ends.as_ref().and_then(|e| e.get("times")).and_then(Value::as_i64) };
        let row = insert(vec![
            ("id", Sql::Text(id.clone())),
            ("session_id", Sql::Text(session.id.clone())),
            ("name", opt_text(&input.name)),
            ("check", opt_text(&check_text)),
            ("state", opt_text(&state_text)),
            ("checks", Sql::Integer(seeded_checks)),
            ("action", Sql::Text(action)),
            ("when", Sql::Text(when_text)),
            ("tz", Sql::Text(tz)),
            ("next_due_at", Sql::Integer(due)),
            ("ends_at", ends_sql),
            ("remaining_runs", remaining.map(Sql::Integer).unwrap_or(Sql::Null)),
            ("time_created", Sql::Integer(now)),
            ("time_updated", Sql::Integer(now)),
        ])?;
        self.doorbell.open();
        Ok(row.info(None))
    }

    /// `list({sessionID})`: oldest first.
    pub fn list(&self, session_id: Option<&str>) -> Vec<Value> {
        self.rows(session_id).iter().map(|r| r.info(None)).collect()
    }

    pub fn rows(&self, session_id: Option<&str>) -> Vec<Row> {
        self.db
            .with(|c| {
                let filter = if session_id.is_some() { "WHERE session_id = ?" } else { "" };
                let mut stmt = c.prepare(&format!("SELECT {COLUMNS} FROM schedule {filter} ORDER BY time_created ASC, id ASC"))?;
                let rows = match session_id {
                    Some(s) => stmt.query_map([s], Row::read)?.collect::<rusqlite::Result<Vec<_>>>()?,
                    None => stmt.query_map([], Row::read)?.collect::<rusqlite::Result<Vec<_>>>()?,
                };
                Ok(rows)
            })
            .unwrap_or_default()
    }

    pub fn get(&self, id: &str) -> Result<Value, ScheduleError> {
        Ok(self.find(id)?.info(None))
    }

    pub fn pause(&self, id: &str) -> Result<Value, ScheduleError> {
        let row = self.find(id)?;
        if row.status != "active" {
            return Ok(row.info(None));
        }
        let now = self.clock.now();
        self.update(id, &[("status", Sql::Text("paused".into())), ("time_updated", Sql::Integer(now))]);
        self.get(id)
    }

    pub fn resume(&self, id: &str) -> Result<Value, ScheduleError> {
        let row = self.find(id)?;
        if row.status != "paused" {
            return Ok(row.info(None));
        }
        let now = self.clock.now();
        // A one-shot that fell due while paused fires once, late; a repeating schedule resumes at its next slot.
        let next = match row.next_due_at {
            None => None,
            Some(due) if due > now => Some(due),
            Some(due) => Some(following(&row.when, &row.tz, row.next_due_at, now).unwrap_or(due)),
        };
        let done = next.is_none() || finished(row.ends_at, next, row.remaining_runs);
        let _ = self.db.with(|c| {
            c.execute(
                "UPDATE schedule SET status = ?, next_due_at = ?, failures = 0, retry_at = NULL, time_updated = ? WHERE id = ? AND status = 'paused'",
                rusqlite::params![if done { "done" } else { "active" }, if done { None } else { next }, now, id],
            )
        });
        self.doorbell.open();
        self.get(id)
    }

    pub fn remove(&self, id: &str) -> Result<(), ScheduleError> {
        self.find(id)?;
        let _ = self.db.with(|c| c.execute("DELETE FROM schedule WHERE id = ?", [id]));
        Ok(())
    }
}

/// The location settings from its config entries: the `schedules` flag and `timezone`.
pub fn settings_of(entries: &[oc_config::Entry]) -> Settings {
    Settings {
        enabled: oc_config::flag::enabled(entries, "schedules"),
        timezone: oc_config::fold::latest(entries, |i| i.timezone.as_ref()).cloned(),
    }
}
