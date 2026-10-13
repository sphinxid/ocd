//! Busy status line (an ocd addition; upstream only shows the KnightRider bar in the prompt footer).
//!
//! From the moment Enter is pressed until the session goes idle, a line near the prompt shows a
//! spinner, what the session is doing right now, the elapsed time, the output token count and the
//! interrupt hint:
//!
//! ```text
//! ⠹ Waiting for Claude Sonnet 4.5… (12s · esc to interrupt)
//! ⠼ Running shell: npm test… (1m 05s · ↓ 1.2k tokens · esc to interrupt)
//! ```
//!
//! The phase is derived only from what the TUI already has: the optimistic submission (outbox), the
//! inbox, the projected messages (`session.step.*`, `session.text.*`, `session.reasoning.*`,
//! `session.tool.*`, `session.retry.scheduled`, `session.compaction.*`, `session.shell.*`), pending
//! permissions/forms and the MCP catalog. No endpoint or event type is added.

use super::App;
use crate::api::types::*;
use crate::store::Store;
use crate::theme::{Rgba, Theme};
use crate::ui::text::*;
use crate::ui::transcript::{canonical_tool, format_path};
use crate::util::locale;
use serde_json::Value;
use std::collections::HashSet;

/// Drop a turn whose submission was acknowledged but whose execution never started after this long.
pub const STALE_MS: i64 = 10_000;

/// One busy period of a session, from Enter (or the first sign of a run) until the session is idle.
#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    /// Epoch ms when Enter was pressed (or when the run was first observed).
    pub started: i64,
    /// Epoch ms when the server acknowledged the submission; `None` while the request is in flight.
    pub acked: Option<i64>,
}

/// What the session is doing.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// The prompt request is in flight.
    Sending,
    /// The server accepted the prompt but has not taken it into a step yet (location load, plugin
    /// activation, MCP connection, instructions).
    SettingUp,
    /// Like `SettingUp`, while MCP servers of the location are still connecting.
    ConnectingMcp,
    /// The request to the model is being prepared or is out; no output yet (model name).
    Waiting(String),
    Thinking,
    Writing,
    /// A tool call's input is streaming (tool display name).
    Preparing(String),
    /// A tool is executing (label, number of other running tools).
    Running(String, usize),
    /// A provider retry is scheduled.
    Retrying { at: i64, attempt: i64 },
    Compacting,
    Permission,
    Question,
}

impl Phase {
    /// The status phrase, ending with an ellipsis.
    pub fn phrase(&self, now: i64) -> String {
        match self {
            Phase::Sending => "Sending…".into(),
            Phase::SettingUp => "Setting up…".into(),
            Phase::ConnectingMcp => "Connecting to MCP servers…".into(),
            Phase::Waiting(model) if model.is_empty() => "Waiting for the model…".into(),
            Phase::Waiting(model) => format!("Waiting for {model}…"),
            Phase::Thinking => "Thinking…".into(),
            Phase::Writing => "Writing…".into(),
            Phase::Preparing(tool) => format!("Preparing {tool}…"),
            Phase::Running(label, 0) => format!("{label}…"),
            Phase::Running(label, more) => format!("{label}… (+{more} more)"),
            Phase::Retrying { at, attempt } => {
                // Same countdown as the transcript's retry row, which refreshes once per wall-clock second.
                let now = now - now.rem_euclid(1000);
                let secs = ((at - now).max(0) + 999) / 1000;
                if secs > 0 { format!("Retrying in {secs}s (attempt {attempt})…") } else { format!("Retrying (attempt {attempt})…") }
            }
            Phase::Compacting => "Compacting…".into(),
            Phase::Permission => "Waiting for permission…".into(),
            Phase::Question => "Waiting for your answer…".into(),
        }
    }

    /// Phases that need the user's attention or report trouble use the warning color.
    pub fn warning(&self) -> bool {
        matches!(self, Phase::Retrying { .. } | Phase::Permission | Phase::Question)
    }
}

/// Everything the derivation looks at.
pub struct Inputs<'a> {
    pub store: &'a Store,
    pub sid: &'a str,
    pub turn: Option<&'a Turn>,
    pub models: &'a [ModelInfo],
    /// Model the next request will use (session model or the prompt's selection).
    pub fallback_model: Option<&'a ModelRef>,
    /// Location directory, for relative paths in tool labels.
    pub cwd: Option<&'a str>,
    /// Some MCP server of the location is still connecting.
    pub mcp_connecting: bool,
    /// A permission prompt / form is waiting for the user.
    pub permission: bool,
    pub question: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub phase: Phase,
    /// Epoch ms the timer counts from (`None`: no known start).
    pub started: Option<i64>,
    /// Output (+ reasoning) tokens of the current run, and whether part of it is estimated.
    pub tokens: Option<(f64, bool)>,
}

/// Messages of the current run: after the last `idle` marker (or, for legacy histories without
/// markers, from the last user message), excluding inbox items not delivered yet.
pub fn current<'a>(store: &'a Store, sid: &str) -> Vec<&'a Message> {
    let pending: HashSet<&str> = store.pending.get(sid).map(|p| p.iter().map(|i| i.id.as_str()).collect()).unwrap_or_default();
    let msgs = store.messages(sid);
    let start = match msgs.iter().rposition(|m| m.kind == "idle") {
        Some(i) => i + 1,
        None => msgs.iter().rposition(|m| m.kind == "user" && !pending.contains(m.id.as_str())).unwrap_or(0),
    };
    msgs[start..].iter().filter(|m| !pending.contains(m.id.as_str())).collect()
}

/// Whether the session is busy from the client's point of view.
pub fn active(store: &Store, sid: &str, turn: Option<&Turn>) -> bool {
    store.is_running(sid) || turn.is_some()
}

/// Derive the status of a busy session (`None` when idle).
pub fn derive(i: &Inputs) -> Option<Status> {
    if !active(i.store, i.sid, i.turn) {
        return None;
    }
    let cur = current(i.store, i.sid);
    Some(Status { phase: phase(i, &cur), started: i.turn.map(|t| t.started), tokens: tokens(&cur) })
}

fn model_name(models: &[ModelInfo], m: Option<&ModelRef>) -> String {
    let Some(m) = m else { return String::new() };
    models
        .iter()
        .find(|x| x.provider_id == m.provider_id && x.id == m.id)
        .map(|x| x.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| m.id.clone())
}

fn phase(i: &Inputs, cur: &[&Message]) -> Phase {
    if i.permission {
        return Phase::Permission;
    }
    if i.question {
        return Phase::Question;
    }
    if i.turn.is_some_and(|t| t.acked.is_none()) {
        return Phase::Sending;
    }
    let waiting = |m: Option<&ModelRef>| Phase::Waiting(model_name(i.models, m.or(i.fallback_model)));
    for m in cur.iter().rev() {
        match m.kind.as_str() {
            "assistant" => return assistant_phase(i, m, waiting(m.model.as_ref())),
            "compaction" if m.status.as_deref() == Some("running") => return Phase::Compacting,
            "compaction" => return waiting(None),
            "shell" if m.status.as_deref() == Some("running") => {
                let cmd = m.command.as_deref().map(first_line).unwrap_or_default();
                return Phase::Running(if cmd.is_empty() { "Running shell".into() } else { format!("Running shell: {cmd}") }, 0);
            }
            "user" | "synthetic" => break,
            _ => continue,
        }
    }
    // No step since the last input.
    let pending = i.store.pending.get(i.sid).map(|p| p.as_slice()).unwrap_or(&[]);
    if pending.iter().any(|p| p.kind == "compaction") {
        return Phase::Compacting;
    }
    let undelivered = pending.iter().any(|p| p.kind != "compaction" && (p.delivery != "queue" || !i.store.is_running(i.sid)));
    if undelivered || !i.store.is_running(i.sid) {
        return if i.mcp_connecting { Phase::ConnectingMcp } else { Phase::SettingUp };
    }
    waiting(None)
}

fn assistant_phase(i: &Inputs, m: &Message, waiting: Phase) -> Phase {
    if m.time.completed.is_some() {
        // Between steps: the next request is being prepared or is out.
        return waiting;
    }
    if let Some(r) = &m.retry {
        return Phase::Retrying { at: r.at, attempt: r.attempt };
    }
    let running: Vec<&ToolPart> = m.content.iter().filter_map(|c| match c {
        Content::Tool(t) if t.state.status == "running" => Some(t),
        _ => None,
    }).collect();
    let streaming = m.time.streamed.is_none();
    let run = |t: &ToolPart| Phase::Running(tool_label(t, i.cwd), running.len().saturating_sub(1));
    match m.content.last() {
        Some(Content::Tool(t)) if t.state.status == "streaming" && streaming => Phase::Preparing(tool_name(&t.name)),
        Some(Content::Tool(t)) if t.state.status == "running" => run(t),
        Some(Content::Reasoning { .. }) if streaming => Phase::Thinking,
        Some(Content::Text { .. }) if streaming => Phase::Writing,
        _ => match running.first() {
            Some(t) => run(t),
            None => waiting,
        },
    }
}

fn first_line(s: &str) -> String {
    locale::collapse_ws(s.trim().lines().next().unwrap_or(""))
}

/// Display name of a tool whose input is still streaming.
fn tool_name(name: &str) -> String {
    match canonical_tool(name) {
        "shell" => "shell command".into(),
        "execute" => "code".into(),
        n => n.to_string(),
    }
}

/// Label of a running tool (without the ellipsis).
pub fn tool_label(t: &ToolPart, cwd: Option<&str>) -> String {
    let empty = Value::Object(Default::default());
    let input = if t.state.input.is_object() { &t.state.input } else { &empty };
    let get = |k: &str| input.get(k).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
    let path = |k: &str| get(k).map(|p| format_path(p, cwd));
    let with = |verb: &str, arg: Option<String>| match arg {
        Some(a) => format!("{verb} {a}"),
        None => verb.to_string(),
    };
    match canonical_tool(&t.name) {
        "shell" => match get("command").map(first_line) {
            Some(c) => format!("Running shell: {c}"),
            None => "Running shell".into(),
        },
        "read" => with("Reading", path("path")),
        "write" => with("Writing", path("path")),
        "edit" => with("Editing", path("path")),
        "patch" => {
            let re = regex::Regex::new(r"\*\*\* (?:Add|Update|Delete) File: ([^\r\n]+)").unwrap();
            let targets: Vec<String> = get("patchText").map(|p| re.captures_iter(p).map(|c| format_path(c[1].trim(), cwd)).collect()).unwrap_or_default();
            match targets.len() {
                0 => "Applying patch".into(),
                1 => format!("Patching {}", targets[0]),
                n => format!("Patching {n} files"),
            }
        }
        "glob" => with("Finding files", get("pattern").map(|p| format!("\"{p}\""))),
        "grep" => with("Searching for", get("pattern").map(|p| format!("\"{p}\""))),
        "webfetch" => with("Fetching", get("url").map(str::to_string)),
        "websearch" => match get("query") {
            Some(q) => format!("Searching the web: {q}"),
            None => "Searching the web".into(),
        },
        "subagent" => {
            let agent = locale::titlecase(get("agent").or(get("subagent_type")).unwrap_or("general"));
            match get("description") {
                Some(d) => format!("Running {agent} subagent: {d}"),
                None => format!("Running {agent} subagent"),
            }
        }
        "execute" => {
            let call = t.state.metadata.as_ref().and_then(|m| m.get("toolCalls")).and_then(|c| c.as_array()).and_then(|calls| {
                calls.iter().rev().find(|c| c.get("status").and_then(|s| s.as_str()) == Some("running")).and_then(|c| c.get("tool")).and_then(|t| t.as_str())
            });
            with("Running code", call.map(|c| format!("› {c}")))
        }
        "skill" => with("Loading skill", get("id").or(get("name")).map(str::to_string)),
        "question" => "Asking questions".into(),
        _ => format!("Running {}", t.name),
    }
}

/// Output + reasoning tokens of the run: exact for finished steps, `chars / 4` for the step in
/// progress (flagged as estimated).
pub fn tokens(cur: &[&Message]) -> Option<(f64, bool)> {
    let mut known = 0.0;
    let mut chars = 0usize;
    for m in cur.iter().filter(|m| m.kind == "assistant") {
        match &m.tokens {
            Some(t) => known += t.output + t.reasoning,
            None if m.time.completed.is_none() => {
                for c in &m.content {
                    chars += match c {
                        Content::Text { text, .. } | Content::Reasoning { text, .. } => text.chars().count(),
                        Content::Tool(t) => match &t.state.input {
                            Value::String(s) => s.chars().count(),
                            Value::Null => 0,
                            v => v.to_string().chars().count(),
                        },
                        #[allow(unreachable_patterns)]
                        _ => 0,
                    };
                }
            }
            None => {}
        }
    }
    let estimate = (chars as f64 / 4.0).round();
    let total = known + estimate;
    (total > 0.0).then_some((total, estimate > 0.0))
}

/// `12s`, `1m 05s`, `2h 03m`.
pub fn format_elapsed(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// `950`, `1.2k`, `12k`, `3.4M`.
pub fn format_tokens(n: f64) -> String {
    let n = n.max(0.0).round();
    let short = |v: f64, unit: &str| {
        let s = if v >= 100.0 { format!("{v:.0}") } else { format!("{v:.1}") };
        format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s))
    };
    if n < 1000.0 {
        format!("{}", n as i64)
    } else if n < 1_000_000.0 {
        short(n / 1000.0, "k")
    } else {
        short(n / 1_000_000.0, "M")
    }
}

/// Rendering options for [`line`].
pub struct LineOpts<'a> {
    pub theme: &'a Theme,
    /// Spinner glyph (current frame, or a static glyph with animations off).
    pub spinner: &'a str,
    pub spinner_color: Rgba,
    /// Interrupt key (`""` hides the hint) and whether the first press already armed it.
    pub interrupt_key: &'a str,
    pub armed: bool,
    pub now: i64,
    pub width: usize,
}

/// The status line, fitted to `o.width`: details are dropped (tokens, then the interrupt hint)
/// before the phrase is truncated; the elapsed time stays.
pub fn line(st: &Status, o: &LineOpts) -> SLine {
    let t = o.theme;
    let phrase = st.phase.phrase(o.now);
    let phrase_c = if st.phase.warning() { t.text.feedback.warning.base } else { t.text.base };
    let elapsed = st.started.map(|s| format_elapsed(o.now - s));
    let tokens = st.tokens.map(|(n, est)| format!("↓ {}{} tokens", if est { "~" } else { "" }, format_tokens(n)));
    let hint: Option<Vec<Seg>> = (!o.interrupt_key.is_empty()).then(|| {
        if o.armed {
            vec![seg(format!("{} again to interrupt", o.interrupt_key), fg(t.text.feedback.warning.base))]
        } else {
            vec![seg(o.interrupt_key, fg(t.text.base)), seg(" to interrupt", fg(t.text.muted))]
        }
    });
    let details = |with_tokens: bool, with_hint: bool| -> Vec<Seg> {
        let mut parts: Vec<Vec<Seg>> = Vec::new();
        if let Some(e) = &elapsed {
            parts.push(vec![seg(e.clone(), fg(t.text.muted))]);
        }
        if with_tokens && let Some(k) = &tokens {
            parts.push(vec![seg(k.clone(), fg(t.text.muted))]);
        }
        if with_hint && let Some(h) = &hint {
            parts.push(h.clone());
        }
        if parts.is_empty() {
            return vec![];
        }
        let mut out = vec![seg(" (", fg(t.text.muted))];
        for (k, p) in parts.into_iter().enumerate() {
            if k > 0 {
                out.push(seg(" · ", fg(t.text.muted)));
            }
            out.extend(p);
        }
        out.push(seg(")", fg(t.text.muted)));
        out
    };
    let head = |p: String| vec![seg(o.spinner, fg(o.spinner_color)), seg(" ", fg(t.text.base)), seg(p, fg(phrase_c))];
    let width_of = |segs: &[Seg]| segs.iter().map(|s| s.width()).sum::<usize>();
    for (k, h) in [(true, true), (false, true), (true, false), (false, false)] {
        let mut segs = head(phrase.clone());
        segs.extend(details(k, h));
        if width_of(&segs) <= o.width {
            return SLine::new(segs);
        }
    }
    // Truncate the phrase, keeping the elapsed time when there is room for a few phrase cells.
    let tail = details(false, false);
    let room = o.width.saturating_sub(2 + width_of(&tail));
    let (phrase, tail) = if room >= 8 { (locale::truncate_width(&phrase, room), tail) } else { (locale::truncate_width(&phrase, o.width.saturating_sub(2)), vec![]) };
    if o.width < 2 {
        return SLine::new(vec![]);
    }
    let mut segs = head(phrase);
    segs.extend(tail);
    SLine::new(clip(&segs, o.width))
}

impl App {
    /// The busy status of the current session, if it is busy.
    pub fn busy_status(&self) -> Option<Status> {
        let sid = self.session_id()?;
        let turn = self.busy.get(sid);
        if !active(&self.store, sid, turn) {
            return None;
        }
        let models = self.model_list();
        let fallback = self.session().and_then(|s| s.model.clone()).or_else(|| self.current_model());
        let cwd = self.location_dir();
        let mcp_connecting = self.loc().and_then(|l| l.mcp.as_ref()).is_some_and(|l| l.iter().any(|s| s.status.status == "pending"));
        derive(&Inputs {
            store: &self.store,
            sid,
            turn,
            models: &models,
            fallback_model: fallback.as_ref(),
            cwd: Some(&cwd),
            mcp_connecting,
            permission: self.active_permission().is_some(),
            question: self.active_form().is_some(),
        })
    }

    /// The rendered status line for the current session (`None` when idle).
    pub(super) fn busy_line(&self, t: &Theme, width: usize, color: Rgba) -> Option<SLine> {
        let st = self.busy_status()?;
        let animations = self.config.animations();
        let spinner = if animations { self.spinner_frame() } else { "⋯" };
        // The hint only applies where the interrupt key works: prompt focused, not in shell mode.
        let key = if self.active_permission().is_none() && self.active_form().is_none() && !self.editor.shell && self.store.is_running(self.session_id()?) {
            // Shown like upstream's footer hint ("esc", not the keymap's "escape").
            self.keybinds.shortcut("session.interrupt").replace("escape", "esc")
        } else {
            String::new()
        };
        Some(line(&st, &LineOpts { theme: t, spinner, spinner_color: color, interrupt_key: &key, armed: self.interrupt > 0, now: self.now(), width }))
    }

    /// Start (or keep) the busy turn of a session when a prompt or shell command is submitted.
    pub fn turn_begin(&mut self, sid: &str) {
        let now = self.now();
        let running = self.store.is_running(sid);
        let fresh = self.busy.get(sid).is_none_or(|t| !running && t.acked.is_some());
        if fresh {
            self.busy.insert(sid.to_string(), Turn { started: now, acked: None });
        }
    }

    /// The submission request finished: `ok` acknowledges it, an error ends the turn unless a run
    /// is going on anyway. `oneshot` ends it right away when nothing is running (`!` shell commands
    /// complete within their request).
    pub fn turn_acked(&mut self, sid: &str, ok: bool, oneshot: bool) {
        let running = self.store.is_running(sid);
        if !running && (!ok || oneshot) {
            self.busy.remove(sid);
            return;
        }
        let now = self.now();
        if let Some(t) = self.busy.get_mut(sid) {
            t.acked.get_or_insert(now);
        }
    }

    /// Track execution lifecycle events.
    pub fn turn_event(&mut self, ev: &Event) {
        let Some(sid) = ev.session_id().map(str::to_string) else { return };
        match ev.kind.as_str() {
            "session.execution.started" => {
                let now = self.now();
                let t = self.busy.entry(sid).or_insert(Turn { started: now, acked: Some(now) });
                t.acked.get_or_insert(now);
            }
            "session.execution.succeeded" | "session.execution.failed" | "session.execution.interrupted" => {
                self.busy.remove(&sid);
            }
            _ => {}
        }
    }

    /// Periodic upkeep: adopt runs observed without a turn (resumed or remotely started sessions)
    /// and drop turns whose execution never started.
    pub fn turn_tick(&mut self) {
        let now = self.now();
        let before = self.busy.len();
        let store = &self.store;
        self.busy.retain(|sid, t| store.is_running(sid) || t.acked.is_none_or(|a| now - a < STALE_MS));
        if self.busy.len() != before {
            self.dirty = true;
        }
        if let Some(sid) = self.session_id().map(str::to_string)
            && self.store.is_running(&sid)
            && !self.busy.contains_key(&sid)
        {
            let started = current(&self.store, &sid).first().map(|m| m.time.created).filter(|c| *c > 0).unwrap_or(now).min(now);
            self.busy.insert(sid, Turn { started, acked: Some(now) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SID: &str = "ses_1";

    fn ev(kind: &str, mut data: Value) -> Event {
        data["sessionID"] = json!(SID);
        Event { id: format!("evt_{}", crate::util::id_body(false)), created: 1000, kind: kind.into(), data, ..Default::default() }
    }

    fn models() -> Vec<ModelInfo> {
        vec![ModelInfo { id: "m1".into(), provider_id: "p".into(), name: "Mock Model".into(), ..Default::default() }]
    }

    fn status(store: &Store, turn: Option<&Turn>) -> Option<Status> {
        let models = models();
        derive(&Inputs {
            store,
            sid: SID,
            turn,
            models: &models,
            fallback_model: Some(&ModelRef { provider_id: "p".into(), id: "m1".into(), variant: None }),
            cwd: Some("/work"),
            mcp_connecting: false,
            permission: false,
            question: false,
        })
    }

    fn phrase(store: &Store, turn: Option<&Turn>) -> String {
        status(store, turn).map(|s| s.phase.phrase(0)).unwrap_or_else(|| "<idle>".into())
    }

    const A: &str = "msg_a";

    fn step(s: &mut Store) {
        s.apply(&ev("session.step.started", json!({"assistantMessageID":A,"agent":"build","model":{"id":"m1","providerID":"p"},"started":5})));
    }

    #[test]
    fn prompt_lifecycle() {
        let mut s = Store::new("/work");
        assert_eq!(phrase(&s, None), "<idle>");
        // Enter: optimistic admission, request in flight.
        s.admit_prompt(SID, "msg_u", "hi", json!([]), "steer");
        let mut turn = Turn { started: 0, acked: None };
        assert_eq!(phrase(&s, Some(&turn)), "Sending…");
        // Accepted and enqueued; the execution starts but the input is not taken yet (setup).
        turn.acked = Some(1);
        s.apply(&ev("session.inbox.enqueued", json!({"inboxID":"msg_u","item":{"type":"user","payload":{"text":"hi"},"delivery":"steer"}})));
        assert_eq!(phrase(&s, Some(&turn)), "Setting up…");
        s.apply(&ev("session.execution.started", json!({})));
        assert_eq!(phrase(&s, Some(&turn)), "Setting up…");
        // Input delivered: the request is assembled / out.
        s.apply(&ev("session.inbox.delivered", json!({"inboxID":"msg_u"})));
        assert_eq!(phrase(&s, Some(&turn)), "Waiting for Mock Model…");
        step(&mut s);
        assert_eq!(phrase(&s, Some(&turn)), "Waiting for Mock Model…");
        s.apply(&ev("session.reasoning.started", json!({"assistantMessageID":A,"ordinal":0})));
        assert_eq!(phrase(&s, Some(&turn)), "Thinking…");
        s.apply(&ev("session.reasoning.ended", json!({"assistantMessageID":A,"ordinal":0,"text":"hmm"})));
        s.apply(&ev("session.text.started", json!({"assistantMessageID":A,"ordinal":0})));
        s.apply(&ev("session.text.delta", json!({"assistantMessageID":A,"ordinal":0,"delta":"Let me run the tests."})));
        assert_eq!(phrase(&s, Some(&turn)), "Writing…");
        s.apply(&ev("session.tool.input.started", json!({"assistantMessageID":A,"id":"t1","name":"bash"})));
        assert_eq!(phrase(&s, Some(&turn)), "Preparing shell command…");
        s.apply(&ev("session.tool.called", json!({"assistantMessageID":A,"id":"t1","input":{"command":"npm test\necho done"},"executed":true})));
        assert_eq!(phrase(&s, Some(&turn)), "Running shell: npm test…");
        // Stream ends while the tool still runs.
        s.apply(&ev("session.step.streamed", json!({"assistantMessageID":A})));
        assert_eq!(phrase(&s, Some(&turn)), "Running shell: npm test…");
        s.apply(&ev("session.tool.success", json!({"assistantMessageID":A,"id":"t1","content":[]})));
        assert_eq!(phrase(&s, Some(&turn)), "Waiting for Mock Model…");
        s.apply(&ev("session.step.ended", json!({"assistantMessageID":A,"finish":"tool-calls","tokens":{"input":100,"output":40,"reasoning":2}})));
        assert_eq!(phrase(&s, Some(&turn)), "Waiting for Mock Model…");
        assert_eq!(status(&s, Some(&turn)).unwrap().tokens, Some((42.0, false)));
        s.apply(&ev("session.execution.succeeded", json!({})));
        assert_eq!(phrase(&s, None), "<idle>");
    }

    #[test]
    fn retry_compaction_and_attention() {
        let mut s = Store::new("/work");
        s.apply(&ev("session.execution.started", json!({})));
        step(&mut s);
        s.apply(&ev("session.retry.scheduled", json!({"assistantMessageID":A,"attempt":2,"at":5_000,"error":{"type":"x","message":"overloaded"}})));
        let st = status(&s, None).unwrap();
        assert_eq!(st.phase.phrase(0), "Retrying in 5s (attempt 2)…");
        assert_eq!(st.phase.phrase(4_200), "Retrying in 1s (attempt 2)…");
        assert_eq!(st.phase.phrase(6_000), "Retrying (attempt 2)…");
        assert!(st.phase.warning());
        // A new attempt clears the retry.
        step(&mut s);
        assert_eq!(phrase(&s, None), "Waiting for Mock Model…");
        s.apply(&ev("session.step.ended", json!({"assistantMessageID":A,"finish":"stop"})));
        s.apply(&ev("session.compaction.started", json!({"reason":"auto"})));
        assert_eq!(phrase(&s, None), "Compacting…");
        s.apply(&ev("session.compaction.ended", json!({"text":"summary","reason":"auto"})));
        assert_eq!(phrase(&s, None), "Waiting for Mock Model…");
        // Permission / form prompts win over everything.
        let models = models();
        let mut i = Inputs { store: &s, sid: SID, turn: None, models: &models, fallback_model: None, cwd: None, mcp_connecting: false, permission: true, question: false };
        assert_eq!(derive(&i).unwrap().phase, Phase::Permission);
        i.permission = false;
        i.question = true;
        assert_eq!(derive(&i).unwrap().phase.phrase(0), "Waiting for your answer…");
    }

    #[test]
    fn setup_mcp_and_subagent_and_parallel_tools() {
        let mut s = Store::new("/work");
        s.apply(&ev("session.inbox.enqueued", json!({"inboxID":"msg_u","item":{"type":"user","payload":{"text":"hi"},"delivery":"steer"}})));
        s.apply(&ev("session.execution.started", json!({})));
        let models = models();
        let i = Inputs { store: &s, sid: SID, turn: None, models: &models, fallback_model: None, cwd: None, mcp_connecting: true, permission: false, question: false };
        assert_eq!(derive(&i).unwrap().phase.phrase(0), "Connecting to MCP servers…");
        s.apply(&ev("session.inbox.delivered", json!({"inboxID":"msg_u"})));
        step(&mut s);
        s.apply(&ev("session.tool.input.started", json!({"assistantMessageID":A,"id":"t1","name":"task"})));
        s.apply(&ev("session.tool.called", json!({"assistantMessageID":A,"id":"t1","input":{"agent":"explore","description":"Find the config loader"}})));
        assert_eq!(phrase(&s, None), "Running Explore subagent: Find the config loader…");
        s.apply(&ev("session.tool.input.started", json!({"assistantMessageID":A,"id":"t2","name":"read"})));
        s.apply(&ev("session.tool.called", json!({"assistantMessageID":A,"id":"t2","input":{"path":"/work/src/main.rs"}})));
        assert_eq!(phrase(&s, None), "Reading src/main.rs… (+1 more)");
        // Old completed turns before an idle marker do not leak into the current run.
        s.apply(&ev("session.execution.interrupted", json!({"reason":"user"})));
        assert_eq!(phrase(&s, None), "<idle>");
        s.apply(&ev("session.execution.started", json!({})));
        assert_eq!(phrase(&s, None), "Waiting for Mock Model…");
        assert_eq!(status(&s, None).unwrap().tokens, None);
    }

    #[test]
    fn user_shell_and_estimates() {
        let mut s = Store::new("/work");
        let turn = Turn { started: 0, acked: Some(1) };
        s.apply(&ev("session.shell.started", json!({"shell":{"id":"sh_1","command":"sleep 5","status":"running"}})));
        assert_eq!(phrase(&s, Some(&turn)), "Running shell: sleep 5…");
        s.apply(&ev("session.execution.started", json!({})));
        step(&mut s);
        s.apply(&ev("session.text.started", json!({"assistantMessageID":A,"ordinal":0})));
        s.apply(&ev("session.text.delta", json!({"assistantMessageID":A,"ordinal":0,"delta":"x".repeat(400)})));
        assert_eq!(status(&s, Some(&turn)).unwrap().tokens, Some((100.0, true)));
    }

    #[test]
    fn formatting() {
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(12_999), "12s");
        assert_eq!(format_elapsed(65_000), "1m 05s");
        assert_eq!(format_elapsed(3_600_000 + 3 * 60_000 + 9_000), "1h 03m");
        assert_eq!(format_elapsed(-5), "0s");
        assert_eq!(format_tokens(950.0), "950");
        assert_eq!(format_tokens(1000.0), "1k");
        assert_eq!(format_tokens(1234.0), "1.2k");
        assert_eq!(format_tokens(12_345.0), "12.3k");
        assert_eq!(format_tokens(123_456.0), "123k");
        assert_eq!(format_tokens(3_400_000.0), "3.4M");
    }

    fn text(l: &SLine) -> String {
        l.segs.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn line_fits_width() {
        let theme = crate::theme::ThemeManager::new(None, crate::theme::Mode::Dark, None).current;
        let st = Status { phase: Phase::Waiting("Claude Sonnet 4.5".into()), started: Some(0), tokens: Some((1234.0, false)) };
        let o = |width: usize, armed: bool| LineOpts { theme: &theme, spinner: "⠋", spinner_color: theme.text.base, interrupt_key: "esc", armed, now: 65_000, width };
        assert_eq!(text(&line(&st, &o(100, false))), "⠋ Waiting for Claude Sonnet 4.5… (1m 05s · ↓ 1.2k tokens · esc to interrupt)");
        assert_eq!(text(&line(&st, &o(100, true))), "⠋ Waiting for Claude Sonnet 4.5… (1m 05s · ↓ 1.2k tokens · esc again to interrupt)");
        assert_eq!(text(&line(&st, &o(60, false))), "⠋ Waiting for Claude Sonnet 4.5… (1m 05s · esc to interrupt)");
        assert_eq!(text(&line(&st, &o(41, false))), "⠋ Waiting for Claude Sonnet 4.5… (1m 05s)");
        assert_eq!(text(&line(&st, &o(40, false))), "⠋ Waiting for Claude Sonnet 4.… (1m 05s)");
        assert_eq!(text(&line(&st, &o(30, false))), "⠋ Waiting for Claude… (1m 05s)");
        assert_eq!(text(&line(&st, &o(12, false))), "⠋ Waiting f…");
        for w in 0..120 {
            assert!(line(&st, &o(w, false)).width() <= w, "width {w}");
        }
    }
}
