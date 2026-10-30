//! `/goal` (`core/src/plugin/goal.ts`): the built-in `opencode.goal` plugin, written natively.
//!
//! A session's goal lives in the plugin's storage (`plugin:<utf-16 hex "opencode.goal">:session:<id>`). Clients
//! read it through the `opencode.goal` RPC (`get`) and follow `rpc.opencode.goal.changed`, published in the
//! session's location on every write. The `goal` command sets, pauses, resumes and clears it; the idle hook asks
//! a reviewer whether the objective is met and either continues the same execution with a synthetic continuation
//! or settles the goal; the `context` hook reminds the model of an active goal a compaction may have hidden; a
//! 30-minute watchdog pauses a goal that waits on background work that never reports.
//!
//! Upstream runs one plugin instance per location; the state is global, so ocd keeps one instance per server
//! process ([`Goals`] on the runner engine) and checks per location whether the built-in is enabled.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use oc_ai::types::{ContentPart, Message as LlmMessage, Role};
use oc_schema::Message;
use oc_schema::event::Delivery;
use oc_schema::types::{Metadata, SessionInfo};
use oc_storage::Database;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::bus::Bus;
use crate::runner::Engine;

pub const PLUGIN_ID: &str = "opencode.goal";
pub const COMMAND: &str = "goal";
pub const DESCRIPTION: &str = "keep working until an objective is met [pause|resume|clear]";
pub const USAGE: &str = "Usage: /goal <objective> | pause | resume | clear";
/// The RPC id clients call (`POST /api/rpc/opencode.goal/get`).
pub const RPC_ID: &str = "opencode.goal";
/// `rpc.<id>.changed`, published on every write.
pub const CHANGED: &str = "rpc.opencode.goal.changed";
pub const MAX_OBJECTIVE: usize = 4_000;
pub const MAX_ROUNDS: u64 = 50;
pub const MAX_STALLS: f64 = 3.0;
/// How long an active goal may sit idle, waiting on background work that never reports, before it pauses.
pub const WAIT_LIMIT: Duration = Duration::from_secs(30 * 60);
/// Storage key prefix of a session's goal (inside the plugin's namespace).
const PREFIX: &str = "session:";

const KICKOFF: &str = include_str!("../../../assets/prompt/goal/kickoff.txt");
const CONTINUATION: &str = include_str!("../../../assets/prompt/goal/continuation.txt");
const JUDGE: &str = include_str!("../../../assets/prompt/goal/judge.txt");
const REMINDER: &str = include_str!("../../../assets/prompt/goal/reminder.txt");

/// The events the goal plugin follows (`ctx.event.subscribe({types})`).
const WATCHED: [&str; 7] = [
    "session.execution.started",
    "session.execution.succeeded",
    "session.execution.interrupted",
    "session.execution.failed",
    "session.step.started",
    "session.step.ended",
    "session.deleted",
];

/// `Goal.State` field names in schema order.
const FIELDS: [&str; 9] = ["id", "objective", "status", "reason", "rounds", "maxRounds", "stalls", "boot", "updatedAt"];

/// `Goal.State`. The object keeps the key order the plugin wrote it in (a JS spread over the decoded state
/// appends new keys), which is what storage holds; RPC answers and events encode it in schema order.
#[derive(Debug, Clone, PartialEq)]
pub struct Goal(Map<String, Value>);

impl Goal {
    /// `Schema.decodeUnknownOption(Goal.State)`: the declared keys in schema order, or `None`.
    pub fn decode(value: &Value) -> Option<Goal> {
        let object = value.as_object()?;
        let mut out = Map::new();
        for field in FIELDS {
            let value = object.get(field);
            match field {
                "reason" | "stalls" => {
                    let Some(value) = value else { continue };
                    let ok = if field == "reason" { value.is_string() } else { value.is_number() };
                    if !ok {
                        return None;
                    }
                    out.insert(field.into(), value.clone());
                }
                "rounds" | "maxRounds" | "updatedAt" => {
                    out.insert(field.into(), value.filter(|v| v.is_number())?.clone());
                }
                "status" => {
                    let status = value?.as_str()?;
                    if !matches!(status, "active" | "paused" | "complete") {
                        return None;
                    }
                    out.insert(field.into(), json!(status));
                }
                _ => {
                    out.insert(field.into(), json!(value?.as_str()?));
                }
            }
        }
        Some(Goal(out))
    }

    fn new(fields: Vec<(&str, Value)>) -> Goal {
        Goal(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    fn str(&self, key: &str) -> &str {
        self.0.get(key).and_then(Value::as_str).unwrap_or_default()
    }

    fn num(&self, key: &str) -> f64 {
        self.0.get(key).and_then(Value::as_f64).unwrap_or_default()
    }

    pub fn id(&self) -> &str {
        self.str("id")
    }
    pub fn objective(&self) -> &str {
        self.str("objective")
    }
    pub fn status(&self) -> &str {
        self.str("status")
    }
    pub fn boot(&self) -> &str {
        self.str("boot")
    }
    pub fn reason(&self) -> Option<&str> {
        self.0.get("reason").and_then(Value::as_str)
    }
    pub fn rounds(&self) -> f64 {
        self.num("rounds")
    }
    pub fn max_rounds(&self) -> f64 {
        self.num("maxRounds")
    }
    pub fn stalls(&self) -> Option<f64> {
        self.0.get("stalls").and_then(Value::as_f64)
    }
    pub fn updated_at(&self) -> f64 {
        self.num("updatedAt")
    }

    /// `{...goal, ...updates}`: existing keys keep their place, new ones are appended.
    fn with(&self, updates: Vec<(&str, Value)>) -> Goal {
        let mut out = self.0.clone();
        for (k, v) in updates {
            out.insert(k.to_string(), v);
        }
        Goal(out)
    }

    /// The stored object (the plugin's own key order).
    pub fn stored(&self) -> Value {
        Value::Object(self.0.clone())
    }

    /// Encoded through `Goal.State` (schema key order), as RPC answers and events carry it.
    pub fn encoded(&self) -> Value {
        let mut out = Map::new();
        for field in FIELDS {
            if let Some(v) = self.0.get(field) {
                out.insert(field.into(), v.clone());
            }
        }
        Value::Object(out)
    }
}

/// A JS number as JSON (integral values without a fraction).
fn number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15 {
        json!(n as i64)
    } else {
        json!(n)
    }
}

/// `String(n)` for the numbers goals hold.
fn js_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 { format!("{}", n as i64) } else { format!("{n}") }
}

fn now() -> f64 {
    oc_schema::id::now_ms() as f64
}

/// The storage key of a session's goal.
pub fn key(session_id: &str) -> String {
    format!("{}{PREFIX}{session_id}", oc_plugin::storage::namespace(PLUGIN_ID))
}

/// `read(sessionID)`
pub fn read(db: &Database, session_id: &str) -> anyhow::Result<Option<Goal>> {
    Ok(db.kv_get(&key(session_id))?.as_ref().and_then(Goal::decode))
}

/// `write(sessionID, goal)`: store (or remove) it, then publish `rpc.opencode.goal.changed` in the session's
/// location.
pub fn write(db: &Database, bus: &Bus, session_id: &str, goal: Option<&Goal>) -> anyhow::Result<()> {
    match goal {
        Some(goal) => db.kv_set(&key(session_id), &goal.stored())?,
        None => db.kv_remove(&key(session_id))?,
    }
    let data = json!({ "sessionID": session_id, "goal": goal.map(Goal::encoded).unwrap_or(Value::Null) });
    bus.publish_session_ephemeral(session_id, CHANGED, data);
    Ok(())
}

/// `replace(sessionID, expected, next)`: writes only when the stored goal is still the one the decision was
/// made on (same id, update time and status).
pub fn replace(db: &Database, bus: &Bus, session_id: &str, expected: &Goal, next: &Goal) -> anyhow::Result<bool> {
    let Some(current) = read(db, session_id)? else { return Ok(false) };
    if current.id() != expected.id() || current.updated_at() != expected.updated_at() || current.status() != expected.status() {
        return Ok(false);
    }
    write(db, bus, session_id, Some(next))?;
    Ok(true)
}

/// `settle(sessionID, goal, status, reason)`
pub fn settle(db: &Database, bus: &Bus, session_id: &str, goal: &Goal, status: &str, reason: &str) -> anyhow::Result<()> {
    let next = goal.with(vec![("status", json!(status)), ("reason", json!(reason)), ("updatedAt", number(now()))]);
    replace(db, bus, session_id, goal, &next)?;
    Ok(())
}

/// `pauseActive(sessionID, reason)`
pub fn pause_active(db: &Database, bus: &Bus, session_id: &str, reason: &str) -> anyhow::Result<()> {
    match read(db, session_id)? {
        Some(goal) if goal.status() == "active" => settle(db, bus, session_id, &goal, "paused", reason),
        _ => Ok(()),
    }
}

/// The `opencode.goal` RPC: `get {sessionID}` → `Goal.State | null`. Errors carry the RPC failure type and
/// message (`rpc.method_not_found`, `rpc.invalid_input`).
pub fn rpc(db: &Database, method: &str, input: &Value) -> Result<Value, (String, String)> {
    if method != "get" {
        return Err(("rpc.method_not_found".into(), format!("Unknown RPC method: {RPC_ID}.{method}")));
    }
    let Some(session_id) = input.get("sessionID").and_then(Value::as_str) else {
        return Err(("rpc.invalid_input".into(), invalid_input(input)));
    };
    match read(db, session_id) {
        Ok(goal) => Ok(goal.map(|g| g.encoded()).unwrap_or(Value::Null)),
        Err(error) => {
            tracing::error!(rpc = RPC_ID, method, %error, "rpc handler failed");
            Err(("rpc.internal".into(), "RPC call failed".into()))
        }
    }
}

/// The Standard Schema failure message of `Struct({sessionID: String})`, as the RPC service reports it.
fn invalid_input(input: &Value) -> String {
    match input {
        Value::Object(o) => match o.get("sessionID") {
            None => "Missing key\n  at [\"sessionID\"]".to_string(),
            Some(_) => "Expected string\n  at [\"sessionID\"]".to_string(),
        },
        _ => "Expected object".to_string(),
    }
}

// ---------------------------------------------------------------------------------- text helpers

/// JS `\s`.
fn js_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}')
}

/// `String.prototype.trim`.
fn js_trim(s: &str) -> &str {
    s.trim_matches(js_space)
}

/// `text.replace(/\s+/g, " ")`
fn collapse(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.chars() {
        if js_space(c) {
            if !space {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    out
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `truncate(text, length)`: whitespace collapsed, at most `length` UTF-16 units with `…`.
pub fn truncate(text: &str, length: usize) -> String {
    let line = collapse(text);
    if utf16_len(&line) <= length {
        return line;
    }
    let mut out = String::new();
    let mut units = 0;
    for c in line.chars() {
        if units + c.len_utf16() > length - 1 {
            break;
        }
        units += c.len_utf16();
        out.push(c);
    }
    out.push('…');
    out
}

/// `escape(text)`
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// `render(template, values)`: one pass over `${key}`, unknown keys kept.
pub fn render(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let name_len = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(after.len());
        if name_len > 0 && after[name_len..].starts_with('}') {
            let name = &after[..name_len];
            match values.iter().find(|(k, _)| *k == name) {
                Some((_, v)) => out.push_str(v),
                None => out.push_str(&rest[start..start + 2 + name_len + 1]),
            }
            rest = &after[name_len + 1..];
        } else {
            out.push_str("${");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// `unquote(text)`: `/goal "Ship the release"` and `/goal Ship the release` set the same objective.
pub fn unquote(text: &str) -> String {
    let mut chars = text.chars();
    let (Some(first), Some(last)) = (chars.next(), chars.next_back()) else { return text.to_string() };
    let close = match first {
        '"' => '"',
        '\'' => '\'',
        '“' => '”',
        '‘' => '’',
        _ => return text.to_string(),
    };
    if !matches!(last, '"' | '\'' | '”' | '’') || last != close {
        return text.to_string();
    }
    js_trim(&text[first.len_utf8()..text.len() - last.len_utf8()]).to_string()
}

fn continuation(goal: &Goal, feedback: &[Option<&str>]) -> String {
    let feedback: Vec<&str> = feedback.iter().filter_map(|f| f.filter(|s| !s.is_empty())).collect();
    render(CONTINUATION, &[("objective", &escape(goal.objective())), ("feedback", &feedback.join("\n"))])
}

/// Every goal prompt wraps the objective the same way, so this marks the objective as visible to the model.
fn objective_block(objective: &str) -> String {
    format!("<untrusted_objective>\n{}\n</untrusted_objective>", escape(objective))
}

fn same_reason(previous: Option<&str>, next: &str) -> bool {
    let normalize = |t: &str| collapse(&js_trim(t).to_lowercase());
    previous.is_some_and(|p| normalize(p) == normalize(next))
}

fn describe(goal: &Goal) -> String {
    let status = match goal.status() {
        "active" => "Goal active",
        "paused" => "Goal paused",
        _ => "Goal achieved",
    };
    let mut parts = vec![format!("{status}: {}", goal.objective())];
    if let Some(reason) = goal.reason().filter(|r| !r.is_empty()) {
        parts.push(reason.to_string());
    }
    parts.join("\n")
}

/// The reviewer's answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub verdict: String,
    pub reason: String,
    pub next_step: Option<String>,
}

/// `parseVerdict(text)`: reviewers sometimes wrap the JSON in a code fence or add a sentence around it.
pub fn parse_verdict(text: &str) -> Option<Verdict> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    let value: Value = serde_json::from_str(&text[start..=end]).ok()?;
    let object = value.as_object()?;
    let verdict = object.get("verdict")?.as_str()?;
    if !matches!(verdict, "done" | "continue" | "blocked" | "paused" | "wait") {
        return None;
    }
    let reason = object.get("reason")?.as_str()?.to_string();
    let next_step = match object.get("next_step") {
        None => None,
        Some(v) => Some(v.as_str()?.to_string()),
    };
    Some(Verdict { verdict: verdict.to_string(), reason, next_step })
}

/// The judge's outcome.
#[derive(Debug, Clone, PartialEq)]
enum Judged {
    Overflow,
    Verdict(Option<Verdict>),
}

/// What a `goal` command admits after writing the goal (the caller admits it through the session API).
#[derive(Debug, Clone, PartialEq)]
pub enum Admit {
    None,
    /// `ctx.session.prompt({...prompt, text, metadata, delivery})`: a kickoff that carries attachments.
    Prompt { text: String, metadata: Metadata },
    /// `ctx.session.synthetic({text, description?, metadata, delivery?})`.
    Synthetic { text: String, description: Option<String>, metadata: Metadata, delivery: bool },
}

fn goal_metadata(goal_id: &str) -> Metadata {
    let mut m = Metadata::new();
    m.insert("source".into(), json!("goal"));
    m.insert("goalID".into(), json!(goal_id));
    m
}

/// Background work of an idle session (`SessionIdleBackground`).
pub type Background = Vec<Value>;

// ---------------------------------------------------------------------------------- service

/// The process-wide goal plugin state.
pub struct Goals {
    /// Identifies this server process: a goal armed by an earlier process comes back paused.
    boot: String,
    /// One compaction per round when the reviewer's request is too large: session → `{goalID}:{rounds}`.
    compacted: Mutex<HashMap<String, String>>,
    /// Watchdog timers: session → (generation, task).
    timers: Mutex<HashMap<String, (u64, tokio::task::JoinHandle<()>)>>,
    generation: AtomicU64,
    /// The wait limit the watchdog sleeps for ([`WAIT_LIMIT`]; tests shorten it).
    wait: Mutex<Duration>,
}

impl Default for Goals {
    fn default() -> Self {
        Goals {
            boot: uuid::Uuid::new_v4().to_string(),
            compacted: Mutex::new(HashMap::new()),
            timers: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
            wait: Mutex::new(WAIT_LIMIT),
        }
    }
}

impl Goals {
    pub fn boot(&self) -> &str {
        &self.boot
    }

    /// Test hook: how long the watchdog waits (the pause reason still names [`WAIT_LIMIT`]).
    pub fn set_wait_limit(&self, wait: Duration) {
        *self.wait.lock() = wait;
    }

    fn remove_timer(&self, session_id: &str) {
        if let Some((_, handle)) = self.timers.lock().remove(session_id) {
            handle.abort();
        }
    }
}

/// The `context` hook (primary requests): until the next continuation restates the objective, a compacted
/// history may not show it, so add it near the tail without persisting it.
pub fn remind(db: &Database, session_id: &str, messages: &mut Vec<LlmMessage>) {
    let goal = match read(db, session_id) {
        Ok(goal) => goal,
        Err(error) => {
            tracing::warn!(%error, "failed to remind the model of the goal");
            return;
        }
    };
    let Some(goal) = goal.filter(|g| g.status() == "active") else { return };
    let objective = objective_block(goal.objective());
    let text_of = |m: &LlmMessage| {
        m.content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    if messages.iter().any(|m| m.role == Role::User && text_of(m).contains(&objective)) {
        return;
    }
    let at = if messages.last().is_some_and(|m| m.role == Role::User) { messages.len() - 1 } else { messages.len() };
    messages.insert(at, LlmMessage::user_text(render(REMINDER, &[("objective", &escape(goal.objective()))])));
}

impl Engine {
    /// Whether the built-in goal plugin runs at the session's location (`-opencode.goal` removes it).
    pub fn goal_enabled(&self, session: &SessionInfo) -> bool {
        self.app.plugins.builtin_enabled(&crate::location::canonical_directory(&session.location.directory), PLUGIN_ID)
    }

    /// `ctx.session.synthetic(...)`: admit and wake the session unless it is reverted.
    pub async fn goal_synthetic(
        &self,
        session_id: &str,
        text: String,
        description: Option<String>,
        metadata: Metadata,
        delivery: Option<Delivery>,
    ) -> anyhow::Result<()> {
        self.app.sessions.synthetic(session_id, None, text, description, Some(metadata), delivery).await?;
        if self.app.sessions.get(session_id)?.revert.is_none()
            && let Some(c) = self.app.coordinator.get()
        {
            c.wake(session_id, crate::inbox::Scope::Input);
        }
        Ok(())
    }

    /// The `goal` command's `execute`. `text` is the prompt text, `attached` whether the prompt carries files,
    /// agents or skills. Errors are the command's failure message.
    pub async fn goal_command(&self, session_id: &str, text: &str, attached: bool) -> Result<Admit, String> {
        let (db, bus) = (&self.app.db, &self.app.bus);
        let fail = |e: anyhow::Error| e.to_string();
        let text = unquote(js_trim(text));
        let control = text.to_lowercase();
        let goal = read(db, session_id).map_err(fail)?;
        if text.is_empty() {
            return Err(match goal {
                Some(goal) => format!("{}\n{USAGE}", describe(&goal)),
                None => USAGE.to_string(),
            });
        }
        let boot = self.goals.boot.clone();
        match control.as_str() {
            "pause" => {
                let Some(goal) = goal.filter(|g| g.status() == "active") else { return Err("No active goal to pause.".into()) };
                // The user's command wins over the loop, so it writes without the loop's guard.
                let next = goal.with(vec![("status", json!("paused")), ("reason", json!("Paused by the user")), ("updatedAt", number(now()))]);
                write(db, bus, session_id, Some(&next)).map_err(fail)?;
                return Ok(Admit::None);
            }
            "resume" => {
                let Some(goal) = goal.filter(|g| g.status() == "paused") else { return Err("No paused goal to resume.".into()) };
                let resumed = Goal::new(vec![
                    ("id", json!(goal.id())),
                    ("objective", json!(goal.objective())),
                    ("status", json!("active")),
                    // The resume continuation is the first round of the new budget.
                    ("rounds", json!(1)),
                    ("maxRounds", number(goal.max_rounds())),
                    ("boot", json!(boot)),
                    ("updatedAt", number(now())),
                ]);
                write(db, bus, session_id, Some(&resumed)).map_err(fail)?;
                return Ok(Admit::Synthetic {
                    text: continuation(&resumed, &[Some("The user resumed the goal.")]),
                    description: None,
                    metadata: goal_metadata(resumed.id()),
                    delivery: false,
                });
            }
            "clear" => {
                if goal.is_none() {
                    return Err("No goal is set.".into());
                }
                write(db, bus, session_id, None).map_err(fail)?;
                return Ok(Admit::None);
            }
            _ => {}
        }
        let length = utf16_len(&text);
        if length > MAX_OBJECTIVE {
            return Err(format!("A goal objective is limited to {MAX_OBJECTIVE} characters (got {length})."));
        }
        let created = Goal::new(vec![
            ("id", json!(uuid::Uuid::new_v4().to_string())),
            ("objective", json!(text)),
            ("status", json!("active")),
            ("rounds", json!(0)),
            ("maxRounds", json!(MAX_ROUNDS)),
            ("boot", json!(boot)),
            ("updatedAt", number(now())),
        ]);
        write(db, bus, session_id, Some(&created)).map_err(fail)?;
        let session = self.app.sessions.get(session_id).map_err(|e| e.to_string())?;
        // Titles come from the first user message, and a notice kickoff is not one, so name the session here.
        if crate::title::is_untitled(&session) {
            self.app.sessions.rename(session_id, &truncate(&text, 60)).map_err(|e| e.to_string())?;
        }
        let kickoff = render(KICKOFF, &[("objective", &escape(&text))]);
        let metadata = goal_metadata(created.id());
        // Timelines show a described synthetic message as one notice row instead of the whole kickoff. Only a
        // prompt can carry attachments, so those still go through as the user's message.
        if attached {
            return Ok(Admit::Prompt { text: kickoff, metadata });
        }
        Ok(Admit::Synthetic { text: kickoff, description: Some(format!("Goal: {}", truncate(&text, 120))), metadata, delivery: true })
    }

    /// `SessionBackground.outstanding`: background jobs still running for the session or any descendant.
    pub fn outstanding(&self, session_id: &str) -> Background {
        let descends = |from: &str| {
            let mut seen = std::collections::HashSet::new();
            let mut current = Some(from.to_string());
            while let Some(id) = current {
                if id == session_id {
                    return true;
                }
                if !seen.insert(id.clone()) {
                    return false;
                }
                current = self.app.db.session_get(&id).ok().flatten().and_then(|s| s.parent_id);
            }
            false
        };
        use oc_tools::host::JobRecovery;
        self.app
            .jobs
            .running_background()
            .into_iter()
            .filter(|r| match r {
                JobRecovery::Subagent { parent_session_id, .. } => descends(parent_session_id),
                JobRecovery::Shell { session_id, .. } | JobRecovery::Image { session_id, .. } => descends(session_id),
            })
            .map(|r| match r {
                JobRecovery::Shell { session_id, shell_id, command } => {
                    json!({ "kind": "shell", "sessionID": session_id, "shellID": shell_id, "command": command })
                }
                JobRecovery::Image { session_id, path, model } => {
                    json!({ "kind": "image", "sessionID": session_id, "path": path, "model": model })
                }
                JobRecovery::Subagent { parent_session_id, child_session_id, agent, description } => json!({
                    "kind": "subagent",
                    "sessionID": parent_session_id,
                    "childSessionID": child_session_id,
                    "agent": agent,
                    "description": description,
                }),
            })
            .collect()
    }

    /// The goal plugin's `experimental.idle` hook. Failures pause the goal instead of failing the execution.
    pub async fn goal_idle(self: &Arc<Self>, session_id: &str, background: &Background) {
        if let Err(error) = self.goal_idle_inner(session_id, background).await {
            tracing::warn!(sessionID = session_id, %error, "goal continuation failed");
            let _ = pause_active(&self.app.db, &self.app.bus, session_id, "The goal stopped after an error");
        }
    }

    async fn goal_idle_inner(self: &Arc<Self>, session_id: &str, background: &Background) -> anyhow::Result<()> {
        let (db, bus) = (&self.app.db, &self.app.bus);
        let Some(goal) = read(db, session_id)?.filter(|g| g.status() == "active") else { return Ok(()) };
        if goal.boot() != self.goals.boot {
            return settle(db, bus, session_id, &goal, "paused", "The server restarted while the goal was active");
        }
        // The session's own running subagent or image job wakes it with its notice, and the hook runs again then.
        // A grandchild reports to its own parent, so it would never wake this session.
        let own = |work: &Value, kinds: &[&str]| {
            work.get("kind").and_then(Value::as_str).is_some_and(|k| kinds.contains(&k))
                && work.get("sessionID").and_then(Value::as_str) == Some(session_id)
        };
        if background.iter().any(|w| own(w, &["subagent", "image"])) {
            return Ok(());
        }
        if goal.rounds() >= goal.max_rounds() {
            let reason = format!("Reached the limit of {} rounds", js_number(goal.max_rounds()));
            return settle(db, bus, session_id, &goal, "paused", &reason);
        }
        // Only the session's own shells wake it when they finish.
        let shells: Vec<String> = background
            .iter()
            .filter(|w| own(w, &["shell"]))
            .filter_map(|w| w.get("command").and_then(Value::as_str).map(str::to_string))
            .collect();
        let verdict = self.goal_judge(session_id, &goal, &shells).await?;
        if verdict != Judged::Overflow {
            self.goals.compacted.lock().remove(session_id);
        }
        // `/goal pause`, `clear` or a new goal may have landed while the reviewer ran.
        let Some(latest) = read(db, session_id)?.filter(|g| g.id() == goal.id() && g.status() == "active") else { return Ok(()) };
        let verdict = match verdict {
            Judged::Overflow => {
                let marker = format!("{}:{}", latest.id(), js_number(latest.rounds()));
                let repeated = self.goals.compacted.lock().get(session_id) == Some(&marker);
                if repeated {
                    self.goals.compacted.lock().remove(session_id);
                    return settle(db, bus, session_id, &latest, "paused", "The goal reviewer's request is too large for the model");
                }
                self.goals.compacted.lock().insert(session_id.to_string(), marker);
                // The runner compacts before going idle, then runs this hook again on the smaller history.
                self.goal_compact(session_id).await?;
                return Ok(());
            }
            Judged::Verdict(None) => {
                return settle(db, bus, session_id, &latest, "paused", "The goal reviewer gave no usable verdict");
            }
            Judged::Verdict(Some(v)) => v,
        };
        match verdict.verdict.as_str() {
            "wait" => return Ok(()),
            "done" => return settle(db, bus, session_id, &latest, "complete", &verdict.reason),
            "continue" => {}
            _ => return settle(db, bus, session_id, &latest, "paused", &verdict.reason),
        }
        let stalled = same_reason(latest.reason(), &verdict.reason) || !self.goal_used_tools(session_id, latest.id())?;
        let stalls = if stalled { latest.stalls().unwrap_or(0.0) + 1.0 } else { 0.0 };
        if stalls >= MAX_STALLS {
            return settle(db, bus, session_id, &latest, "paused", &format!("Not making progress: {}", verdict.reason));
        }
        let next = latest.with(vec![
            ("rounds", number(latest.rounds() + 1.0)),
            ("stalls", number(stalls)),
            ("reason", json!(verdict.reason)),
            ("updatedAt", number(now())),
        ]);
        if !replace(db, bus, session_id, &latest, &next)? {
            return Ok(());
        }
        // No description: timelines hide the continuation, and the pinned goal shows that the loop is still running.
        let next_step = verdict.next_step.as_deref().filter(|s| !s.is_empty()).map(|s| format!("Next step: {s}"));
        let text = continuation(&next, &[Some(&verdict.reason), next_step.as_deref()]);
        self.goal_synthetic(session_id, text, None, goal_metadata(next.id()), None).await
    }

    /// `ctx.session.compact({sessionID})`: admit a compaction and wake the session.
    async fn goal_compact(&self, session_id: &str) -> anyhow::Result<()> {
        self.app.sessions.compact(session_id, None).await?;
        if let Some(c) = self.app.coordinator.get() {
            c.wake(session_id, crate::inbox::Scope::Input);
        }
        Ok(())
    }

    /// Whether the model called any tool since the goal's last kickoff or continuation. Without that message in
    /// view, for example after a compaction, the round counts as progress.
    fn goal_used_tools(&self, session_id: &str, goal_id: &str) -> anyhow::Result<bool> {
        let messages = crate::history::load(&self.app.db, session_id, crate::history::Boundary::Latest)?;
        let of_goal = |m: &Message| match m {
            Message::Synthetic { metadata, .. } | Message::User { metadata, .. } => {
                metadata.as_ref().and_then(|m| m.get("goalID")).and_then(Value::as_str) == Some(goal_id)
            }
            _ => false,
        };
        let Some(start) = messages.iter().rposition(of_goal) else { return Ok(true) };
        Ok(messages[start + 1..].iter().any(|m| {
            matches!(m, Message::Assistant(a) if a.content.iter().any(|c| matches!(c, oc_schema::message::AssistantContent::Tool { .. })))
        }))
    }

    /// `judge(sessionID, goal, shells)`: asks the reviewer twice at most.
    async fn goal_judge(self: &Arc<Self>, session_id: &str, goal: &Goal, shells: &[String]) -> anyhow::Result<Judged> {
        let background = if shells.is_empty() {
            "No background shells are running.".to_string()
        } else {
            let mut lines = vec!["Background shells still running:".to_string()];
            lines.extend(shells.iter().map(|c| format!("- {c}")));
            lines.push(String::new());
            lines.push("Answer \"wait\" only when the goal needs the result of one of these shells. Never wait for a long-running server such as a dev server.".to_string());
            lines.join("\n")
        };
        let wait = if shells.is_empty() { "" } else { "{\"verdict\":\"wait\",\"reason\":\"<the running shell whose result the goal needs>\"}\n" };
        let prompt = render(JUDGE, &[("objective", &escape(goal.objective())), ("background", &background), ("wait", wait)]);
        for attempt in 0..2 {
            let judged = match self.session_generate(session_id, &prompt).await {
                Ok(text) => {
                    let verdict = parse_verdict(&text);
                    // "wait" is only offered while a shell runs; without one, nothing would wake the session to judge again.
                    Judged::Verdict(verdict.filter(|v| !(v.verdict == "wait" && shells.is_empty())))
                }
                Err(e) if e.downcast_ref::<oc_ai::AIError>().is_some_and(oc_ai::AIError::is_context_overflow) => Judged::Overflow,
                Err(e) => return Err(e),
            };
            if judged != Judged::Verdict(None) || attempt == 1 {
                return Ok(judged);
            }
        }
        unreachable!()
    }

    /// Starts the goal plugin's event subscription (watchdog and lifecycle).
    pub(crate) fn goal_spawn(self: &Arc<Self>) {
        let mut rx = self.app.bus.subscribe();
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let event = match rx.recv().await {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                if !WATCHED.contains(&event.kind.as_str()) {
                    continue;
                }
                let Some(engine) = me.upgrade() else { return };
                let session_id = event.data.get("sessionID").and_then(Value::as_str).unwrap_or_default().to_string();
                engine.goal_event(&event.kind, &session_id, &event.data);
            }
        });
    }

    fn goal_event(self: &Arc<Self>, kind: &str, session_id: &str, data: &Value) {
        let (db, bus) = (&self.app.db, &self.app.bus);
        if kind == "session.deleted" {
            self.goals.remove_timer(session_id);
            let _ = db.kv_remove(&key(session_id));
            return;
        }
        // Only sessions whose location runs the plugin.
        if let Ok(Some(session)) = db.session_get(session_id)
            && !self.goal_enabled(&session)
        {
            return;
        }
        match kind {
            "session.step.started" | "session.step.ended" => {
                if let Err(error) = self.goal_touch(session_id) {
                    tracing::warn!(sessionID = session_id, %error, "failed to note goal activity");
                }
            }
            "session.execution.started" => self.goals.remove_timer(session_id),
            "session.execution.succeeded" => {
                if let Err(error) = self.goal_watch(session_id) {
                    tracing::warn!(sessionID = session_id, %error, "failed to watch a waiting goal");
                }
            }
            "session.execution.interrupted" | "session.execution.failed" => {
                // Shutdown keeps the goal active; the next process sees the boot change and pauses it.
                if kind == "session.execution.interrupted" && data.get("reason").and_then(Value::as_str) != Some("user") {
                    return;
                }
                let reason = if kind == "session.execution.failed" {
                    format!("The session failed: {}", data.pointer("/error/message").and_then(Value::as_str).unwrap_or_default())
                } else {
                    "Stopped by the user".to_string()
                };
                if let Err(error) = pause_active(db, bus, session_id, &reason) {
                    tracing::warn!(sessionID = session_id, %error, "failed to pause goal");
                }
            }
            _ => {}
        }
    }

    /// `watch(sessionID)`: (re)start the session's wait timer while its goal is active.
    fn goal_watch(self: &Arc<Self>, session_id: &str) -> anyhow::Result<()> {
        let Some(goal) = read(&self.app.db, session_id)?.filter(|g| g.status() == "active") else { return Ok(()) };
        let generation = self.goals.generation.fetch_add(1, Ordering::SeqCst);
        let me = Arc::downgrade(self);
        let id = session_id.to_string();
        let wait = *self.goals.wait.lock();
        let mut timers = self.goals.timers.lock();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let Some(engine) = me.upgrade() else { return };
            let reason = format!("Nothing ran for {} minutes while the goal waited on background work", WAIT_LIMIT.as_secs() / 60);
            if let Err(error) = settle(&engine.app.db, &engine.app.bus, &id, &goal, "paused", &reason) {
                tracing::warn!(sessionID = %id, %error, "failed to pause a waiting goal");
            }
            let mut timers = engine.goals.timers.lock();
            if timers.get(&id).is_some_and(|(g, _)| *g == generation) {
                timers.remove(&id);
            }
        });
        if let Some((_, previous)) = timers.insert(session_id.to_string(), (generation, handle)) {
            previous.abort();
        }
        Ok(())
    }

    /// `touch(sessionID)`: a step anywhere below a waiting goal restarts its timer, so a healthy subagent working
    /// past the wait limit does not pause the goal it works for.
    fn goal_touch(self: &Arc<Self>, session_id: &str) -> anyhow::Result<()> {
        if self.goals.timers.lock().is_empty() {
            return Ok(());
        }
        let get = |id: &str| -> anyhow::Result<SessionInfo> {
            self.app.db.session_get(id)?.ok_or_else(|| anyhow::anyhow!("Session not found: {id}"))
        };
        let mut current = get(session_id)?.parent_id;
        while let Some(id) = current {
            if self.goals.timers.lock().contains_key(&id) {
                return self.goal_watch(&id);
            }
            current = get(&id)?.parent_id;
        }
        Ok(())
    }

    /// Pauses goals an earlier process left active and drops goals of deleted sessions (run when a location
    /// activates the plugin). This assumes one server process per database.
    pub(crate) fn goal_scan(&self) {
        let (db, bus) = (&self.app.db, &self.app.bus);
        let prefix = key("");
        let mut after: Option<String> = None;
        loop {
            let (entries, next) = match db.kv_scan(&prefix, after.as_deref(), Some(100)) {
                Ok(page) => page,
                Err(error) => {
                    tracing::warn!(%error, "failed to pause goals left by an earlier process");
                    return;
                }
            };
            for (entry, value) in entries {
                let session_id = &entry[prefix.len()..];
                match db.session_get(session_id) {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        let _ = db.kv_remove(&entry);
                        continue;
                    }
                    Err(_) => continue,
                }
                let Some(goal) = Goal::decode(&value).filter(|g| g.status() == "active" && g.boot() != self.goals.boot) else { continue };
                if let Err(error) = settle(db, bus, session_id, &goal, "paused", "The server restarted while the goal was active") {
                    tracing::warn!(%error, "failed to pause goals left by an earlier process");
                }
            }
            match next {
                Some(n) => after = Some(n),
                None => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquotes_matching_pairs_only() {
        assert_eq!(unquote("\"Ship the release\""), "Ship the release");
        assert_eq!(unquote("“ Ship ”"), "Ship");
        assert_eq!(unquote("‘a’"), "a");
        assert_eq!(unquote("\"mixed'"), "\"mixed'");
        assert_eq!(unquote("“a\""), "“a\"");
        assert_eq!(unquote("\""), "\"");
        assert_eq!(unquote("plain"), "plain");
    }

    #[test]
    fn renders_once() {
        assert_eq!(render("a ${x} ${y} ${x}", &[("x", "${y}"), ("y", "2")]), "a ${y} 2 ${y}");
        assert_eq!(render("${missing} $ {x} ${", &[("x", "1")]), "${missing} $ {x} ${");
    }

    #[test]
    fn truncates_by_utf16_units() {
        assert_eq!(truncate("a  b\n\tc", 60), "a b c");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abcd", 4), "abcd");
    }

    #[test]
    fn parses_verdicts() {
        assert_eq!(
            parse_verdict("```json\n{\"verdict\":\"done\",\"reason\":\"ok\"}\n```"),
            Some(Verdict { verdict: "done".into(), reason: "ok".into(), next_step: None })
        );
        assert_eq!(parse_verdict("{\"verdict\":\"maybe\",\"reason\":\"x\"}"), None);
        assert_eq!(parse_verdict("{\"verdict\":\"done\"}"), None);
        assert_eq!(parse_verdict("{\"verdict\":\"done\",\"reason\":\"x\",\"next_step\":null}"), None);
        assert_eq!(parse_verdict("Looks good"), None);
    }

    #[test]
    fn spreads_append_new_keys_and_encode_in_schema_order() {
        let goal = Goal::decode(&json!({"updatedAt": 1, "boot": "b", "maxRounds": 50, "rounds": 0, "status": "active", "objective": "o", "id": "i", "extra": 1})).unwrap();
        assert_eq!(goal.stored().to_string(), r#"{"id":"i","objective":"o","status":"active","rounds":0,"maxRounds":50,"boot":"b","updatedAt":1}"#);
        let next = goal.with(vec![("rounds", json!(1)), ("stalls", json!(0)), ("reason", json!("r")), ("updatedAt", json!(2))]);
        assert_eq!(
            next.stored().to_string(),
            r#"{"id":"i","objective":"o","status":"active","rounds":1,"maxRounds":50,"boot":"b","updatedAt":2,"stalls":0,"reason":"r"}"#
        );
        assert_eq!(
            next.encoded().to_string(),
            r#"{"id":"i","objective":"o","status":"active","reason":"r","rounds":1,"maxRounds":50,"stalls":0,"boot":"b","updatedAt":2}"#
        );
        assert!(Goal::decode(&json!({"id": "i"})).is_none());
    }

    #[test]
    fn reminder_goes_before_the_last_user_message() {
        let mut messages = vec![LlmMessage::user_text("first"), LlmMessage::assistant(vec![ContentPart::text("ok")]), LlmMessage::user_text("Carry on")];
        let goal = Goal::new(vec![("objective", json!("Make <CI> green"))]);
        let text = render(REMINDER, &[("objective", &escape(goal.objective()))]);
        assert!(text.contains("<untrusted_objective>\nMake &lt;CI&gt; green\n</untrusted_objective>"));
        let at = messages.len() - 1;
        messages.insert(at, LlmMessage::user_text(text));
        assert_eq!(messages.len(), 4);
    }
}
