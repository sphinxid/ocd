//! The mini runtime (`src/mini/runtime.ts`, `runtime.lifecycle.ts`, `runtime.queue.ts`,
//! `runtime.boot.ts`, `cli/src/mini.ts`, `mini-host.ts`, `session-target.ts`): terminal setup, the
//! session target, the event stream with hydration and settling, the serial prompt queue and the
//! exit sequence, all driven from one event loop.

use super::catalog;
use super::footer::{Footer, FooterAction, FooterEvent, FooterOptions};
use super::prompt_shared::{is_compact_command, is_exit_command, is_new_command};
use super::scrollback::{Commit, Sink};
use super::split::Split;
use super::splash;
use super::subagents::{Job, JobResult};
use super::theme::{self as mtheme, RunTheme};
use super::transport::{Out, Transport, Wait, ShellWait, blocker_status, event_id_from_message};
use super::types::*;
use super::variant::{cycle_variant, format_model_label, pick_variant, resolve_variant};
use super::verbosity::{change_writes, resolve_mini_settings};
use crate::api::types::{Event, FormInfo, PermissionRequest, SessionInfo};
use crate::api::{Client, sse::SseParser};
use crate::app::TuiOptions;
use crate::keymap::{self, Keybinds};
use crossterm::event::{Event as CEvent, EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use futures::StreamExt;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{IsTerminal, Read, Write};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

const FOOTER_HEIGHT: usize = 4;
const RESIZE_DELAY: Duration = Duration::from_millis(250);
const LOCAL_REPLAY_ROW_LIMIT: usize = 100;
const SHELL_OUTPUT_GRACE: Duration = Duration::from_millis(1500);

/// Mini-only flags (`ocd mini --replay/--no-replay --replay-limit`).
#[derive(Clone, Debug, Default)]
pub struct MiniOptions {
    /// `--replay` / `--no-replay`; `None` means true.
    pub replay: Option<bool>,
    /// `--replay-limit`; `None` means 200.
    pub replay_limit: Option<i64>,
}

fn fail(msg: &str) -> i32 {
    eprintln!("\x1b[91m\x1b[1mError: \x1b[0m{msg}");
    1
}

/// `mergeInput(piped, prompt)`.
pub fn merge_input(piped: Option<&str>, prompt: Option<&str>) -> Option<String> {
    let piped = piped.filter(|p| !p.is_empty());
    match (piped, prompt.filter(|p| !p.is_empty())) {
        (p, None) => p.map(str::to_string),
        (None, Some(b)) => Some(b.to_string()),
        (Some(a), Some(b)) => Some(format!("{a}\n{b}")),
    }
}

// --- messages from async tasks ---------------------------------------------------------------

#[derive(Debug, Clone)]
struct Resolved {
    session: SessionInfo,
    location: String,
    model: Option<ModelSel>,
    variant: Option<String>,
    agent: Option<String>,
    resume: bool,
}

#[derive(Debug)]
struct Hydration {
    projected: Vec<Value>,
    inbox: Vec<Value>,
    permissions: Vec<PermissionRequest>,
    sources: Vec<Value>,
    forms: Vec<FormInfo>,
    globals: Option<(Vec<FormInfo>, String)>,
    active: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HydrateMode {
    Connect,
    Resize,
}

#[derive(Debug)]
enum Msg {
    Session(Result<Resolved, String>),
    SessionInfo { first: bool, history: Vec<RunPrompt>, model: Option<ModelSel>, variant: Option<String> },
    NewSession(Result<Resolved, String>),
    Catalog { agents: Vec<RunAgent>, references: Vec<RunReference>, commands: Vec<RunCommand> },
    Models(Vec<RunProvider>),
    DefaultModel(Option<ModelSel>),
    Connected(u64),
    SseEvent(u64, Box<Event>),
    Disconnected(u64),
    Hydrated(u64, HydrateMode, Box<Result<Hydration, String>>),
    Admitted(Value),
    TurnSettled { token: u64, messages: Result<Vec<Value>, String> },
    TurnFailed { token: u64, error: String },
    CommandSent { token: u64, error: Option<String> },
    ShellSent { token: u64, error: Option<String> },
    QueueAdmitted { error: Option<(RunPrompt, String)> },
    Settled { messages: Result<Vec<Value>, String> },
    Interrupted { epoch: u64, ok: bool },
    Files { query: String, files: Vec<String> },
    Job(JobResult),
    Editor(Result<Option<String>, ()>),
    SettingsSaved(Result<MiniSettings, String>),
    PermissionFailed,
    FormFailed(String),
    FormSettled { session_id: String, form_id: String },
    QueuedActionFailed(String),
}

struct Sink2<'a> {
    split: &'a mut Split,
}

impl Sink for Sink2<'_> {
    fn width(&self) -> usize {
        self.split.width()
    }
    fn tail_column(&self) -> usize {
        self.split.tail_column()
    }
    fn commit(&mut self, c: Commit) {
        self.split.enqueue(c);
    }
}

#[derive(Clone, Debug)]
struct ActiveTurn {
    token: u64,
    prompt: RunPrompt,
    start: Instant,
    /// `admissionVersion` when the turn started.
    version: u64,
    done: bool,
    shell_deadline: Option<Instant>,
}

/// `OPENCODE_DIRECT_TRACE`: JSONL trace under `<data>/log/direct/`.
struct Trace {
    file: std::fs::File,
}

impl Trace {
    fn open(argv: &[String], cwd: &str) -> Option<Trace> {
        std::env::var_os("OPENCODE_DIRECT_TRACE")?;
        let dir = crate::util::paths::data_dir().join("log").join("direct");
        std::fs::create_dir_all(&dir).ok()?;
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let pid = std::process::id();
        let path = dir.join(format!("{stamp}-{pid}.jsonl"));
        let latest = json!({ "time": chrono::Utc::now().to_rfc3339(), "pid": pid, "cwd": cwd, "argv": argv, "path": path });
        let _ = std::fs::write(dir.join("latest.json"), format!("{latest}\n"));
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok()?;
        let mut t = Trace { file };
        t.write("trace.start", json!({ "argv": argv, "cwd": cwd, "path": path }));
        Some(t)
    }

    fn write(&mut self, kind: &str, data: Value) {
        let line = json!({ "time": chrono::Utc::now().to_rfc3339(), "pid": std::process::id(), "type": kind, "data": data });
        let _ = writeln!(self.file, "{line}");
    }
}

pub struct Runtime {
    trace: Option<Trace>,
    client: Client,
    tx: UnboundedSender<Msg>,
    opts: TuiOptions,
    config: crate::config::Config,
    replay: bool,
    replay_limit: u32,
    directory: String,
    split: Split,
    footer: Footer,
    settings: MiniSettings,
    colors: Option<crate::theme::TerminalColors>,
    // session
    session_id: String,
    session_title: Option<String>,
    location: String,
    agent: Option<String>,
    model: Option<ModelSel>,
    default_model: Option<ModelSel>,
    variant: Option<String>,
    session_variant: Option<String>,
    providers: Vec<RunProvider>,
    variants: Vec<String>,
    shown: bool,
    history: Vec<RunPrompt>,
    local_rows: Vec<StreamCommit>,
    resolving: bool,
    // transport
    transport: Option<Transport>,
    generation: u64,
    booting: bool,
    hydrating: bool,
    buffered: Vec<Event>,
    connect_task: Option<tokio::task::JoinHandle<()>>,
    replaying: bool,
    replay_buffer: Vec<Event>,
    // queue
    queue: VecDeque<RunPrompt>,
    active: Option<ActiveTurn>,
    token: u64,
    admission_version: u64,
    admissions_inflight: usize,
    settling: bool,
    initial_input: Option<String>,
    queue_started: bool,
    // misc
    resize_at: Option<Instant>,
    title_set: bool,
    wrote_exit: bool,
    done: bool,
    kitty: bool,
    pending_editor: Option<String>,
    fatal: Option<String>,
}

/// Query the terminal palette (OSC 10/11 and 256 OSC 4 entries, terminated by DA1).
#[cfg(unix)]
fn probe_palette(timeout_ms: u64) -> Option<crate::theme::TerminalColors> {
    use std::os::fd::AsRawFd;
    let mut out = std::io::stdout();
    let mut q = String::from("\x1b]10;?\x07\x1b]11;?\x07");
    for i in 0..256 {
        q.push_str(&format!("\x1b]4;{i};?\x07"));
    }
    q.push_str("\x1b[c");
    out.write_all(q.as_bytes()).ok()?;
    out.flush().ok()?;
    let tty = std::fs::OpenOptions::new().read(true).open("/dev/tty").ok();
    let stdin = std::io::stdin();
    let fd = tty.as_ref().map(|t| t.as_raw_fd()).unwrap_or(stdin.as_raw_fd());
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut data: Vec<u8> = Vec::new();
    let da1 = regex::bytes::Regex::new(r"\x1b\[\?[0-9;]*c").unwrap();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis() as i32) };
        if r <= 0 {
            break;
        }
        let mut buf = [0u8; 8192];
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
        data.extend_from_slice(&buf[..n as usize]);
        if da1.is_match(&data) {
            break;
        }
    }
    let text = String::from_utf8_lossy(&data);
    let mut colors = crate::theme::TerminalColors { palette: vec![None; 256], ..Default::default() };
    let re = regex::Regex::new(r"\x1b\](4;(\d+)|10|11);([^\x07\x1b]+)(?:\x07|\x1b\\)").unwrap();
    for c in re.captures_iter(&text) {
        let Some(hex) = crate::terminal::parse_rgb(&c[3]) else { continue };
        match &c[1] {
            "10" => colors.default_foreground = Some(hex),
            "11" => colors.default_background = Some(hex),
            _ => {
                if let Some(i) = c.get(2).and_then(|m| m.as_str().parse::<usize>().ok())
                    && i < 256
                {
                    colors.palette[i] = Some(hex);
                }
            }
        }
    }
    if colors.default_background.is_none() && colors.palette.iter().all(|p| p.is_none()) {
        return None;
    }
    Some(colors)
}

#[cfg(not(unix))]
fn probe_palette(_timeout_ms: u64) -> Option<crate::theme::TerminalColors> {
    None
}

/// Resolve the run theme (`resolveRunTheme(renderer, config.theme, mono)`).
fn resolve_theme(config: &crate::config::Config, directory: &str, colors: Option<&crate::theme::TerminalColors>, mono: bool) -> (RunTheme, crate::theme::Mode, String) {
    let detected = colors.and_then(crate::theme::terminal_mode);
    let mode = detected.unwrap_or(if config.theme_mode() == "light" { crate::theme::Mode::Light } else { crate::theme::Mode::Dark });
    let name = config.theme_name().unwrap_or("opencode").to_string();
    if mono {
        return (mtheme::mono_theme(mode), mode, name);
    }
    if name == "system" && colors.is_none_or(|c| !c.usable()) {
        return (mtheme::fallback(mode), mode, name);
    }
    let mut themes = crate::theme::ThemeManager::new(Some(&name), mode, Some(mode));
    if let Ok(custom) = crate::theme::discover(std::path::Path::new(directory)) {
        themes.set_custom(custom);
    }
    themes.set_system_palette(colors.cloned());
    let theme = mtheme::resolve(&themes.current, colors, name == "system");
    (theme, mode, name)
}

fn cursor_style(config: &crate::config::Config) -> u8 {
    let Some(c) = config.get("cursor") else { return 0 };
    let style = c.get("style").and_then(|s| s.as_str()).unwrap_or("block");
    let blinking = c.get("blinking").and_then(|b| b.as_bool()).unwrap_or(true);
    match (style, blinking) {
        ("default", _) => 0,
        ("underline", true) => 3,
        ("underline", false) => 4,
        ("line", true) => 5,
        ("line", false) => 6,
        (_, true) => 1,
        (_, false) => 2,
    }
}

fn model_ref(m: &ModelSel, variant: Option<&str>) -> Value {
    let mut v = json!({ "providerID": m.provider_id, "id": m.model_id });
    if let Some(x) = variant {
        v["variant"] = json!(x);
    }
    v
}

fn err_text(e: &anyhow::Error) -> String {
    if let Some(api) = e.downcast_ref::<crate::api::ApiError>() {
        if !api.message.is_empty() {
            return api.message.clone();
        }
        if !api.tag.is_empty() {
            return api.tag.clone();
        }
    }
    let s = e.to_string();
    if s.is_empty() { "unknown error".into() } else { s }
}

/// `resolveSessionTarget(...)` for the launch (`-s`, `--continue`, `--fork`) or a new session.
#[allow(clippy::too_many_arguments)]
async fn resolve_target(client: Client, directory: String, session: Option<String>, continue_last: bool, fork: bool, model: Option<(ModelSel, Option<String>)>, agent: Option<String>, environment: Option<Value>) -> Result<Resolved, String> {
    let e = |e: anyhow::Error| err_text(&e);
    let mut selected: Option<SessionInfo> = None;
    let mut location: Option<String> = None;
    if let Some(id) = &session {
        match client.session_get(id).await {
            Ok(s) => {
                selected = Some(if fork { client.session_fork(&s.id, None).await.map_err(e)? } else { s });
            }
            Err(err) => {
                let tag = err.downcast_ref::<crate::api::ApiError>().map(|a| a.tag.clone()).unwrap_or_default();
                if tag != "SessionNotFoundError" && !err.to_string().contains("not found") {
                    return Err(err_text(&err));
                }
                if fork {
                    return Err("Session not found".into());
                }
            }
        }
    } else if continue_last {
        let loc = client.location_get(Some(&directory)).await.map_err(e)?.directory;
        let mut cursor: Option<String> = None;
        loop {
            let mut q: Vec<(&str, String)> = vec![("directory", loc.clone()), ("parentID", "null".into()), ("limit", "50".into()), ("order", "desc".into())];
            if let Some(c) = &cursor {
                q.push(("cursor", c.clone()));
            }
            let page = client.session_list(&q).await.map_err(e)?;
            if let Some(s) = page.data.iter().find(|s| s.location.directory == loc && s.parent_id.is_none()) {
                selected = Some(if fork { client.session_fork(&s.id, None).await.map_err(e)? } else { s.clone() });
                break;
            }
            match page.cursor.next {
                Some(n) if !page.data.is_empty() => cursor = Some(n),
                _ => break,
            }
        }
        location = Some(loc);
    }
    let location = match location {
        Some(l) if selected.is_none() => l,
        _ => {
            let dir = selected.as_ref().map(|s| s.location.directory.clone()).unwrap_or(directory.clone());
            client.location_get(Some(&dir)).await.map_err(e)?.directory
        }
    };
    let prepared_model = model.clone().or_else(|| selected.as_ref().and_then(|s| s.model.as_ref()).map(|m| (ModelSel { provider_id: m.provider_id.clone(), model_id: m.id.clone() }, m.variant.clone())));
    let prepared_agent = agent.clone().or_else(|| selected.as_ref().and_then(|s| s.agent.clone()));
    let resume = selected.is_some();
    let session = match selected {
        Some(s) => s,
        None => {
            let mut body = json!({ "location": { "directory": location } });
            if let Some(id) = &session {
                body["id"] = json!(id);
            }
            if let Some(a) = &prepared_agent {
                body["agent"] = json!(a);
            }
            if let Some((m, v)) = &prepared_model {
                body["model"] = model_ref(m, v.as_deref());
            }
            client.session_create(body).await.map_err(e)?
        }
    };
    if let Some(env) = environment {
        client.environment(&session.id, env).await.map_err(e)?;
    }
    Ok(Resolved {
        agent: prepared_agent.or(session.agent.clone()),
        model: prepared_model.as_ref().map(|m| m.0.clone()),
        variant: prepared_model.and_then(|m| m.1),
        location,
        session,
        resume,
    })
}

impl Runtime {
    fn write_out(&mut self) {
        let bytes = self.split.take_output();
        if bytes.is_empty() {
            return;
        }
        let mut out = std::io::stdout();
        let _ = out.write_all(&bytes);
        let _ = out.flush();
    }

    /// One frame: footer actions, height, scrollback commits, footer paint.
    fn frame(&mut self) {
        for _ in 0..16 {
            let actions = std::mem::take(&mut self.footer.actions);
            if actions.is_empty() {
                break;
            }
            for a in actions {
                self.footer_action(a);
            }
        }
        let now = Instant::now();
        self.footer.tick(now);
        let height = self.footer.height as u32;
        if height != self.split.footer_height() {
            if self.split.has_pending() {
                let (buf, cursor) = self.footer.render(now);
                self.split.render(&buf, cursor);
            }
            self.split.set_footer_height(height);
        }
        let mut sink = Sink2 { split: &mut self.split };
        self.footer.drain(&mut sink);
        let (buf, cursor) = self.footer.render(now);
        self.split.render(&buf, cursor);
        self.write_out();
    }

    fn set_title(&mut self, title: Option<&str>) {
        if !cfg!(target_os = "linux") {
            return;
        }
        let text = match title {
            Some(t) if !crate::util::locale::is_default_title(t) => {
                let chars: Vec<char> = t.chars().collect();
                format!("OC | {}", if chars.len() > 40 { chars[..37].iter().collect::<String>() + "…" } else { t.to_string() })
            }
            _ => "OpenCode".into(),
        };
        let _ = write!(std::io::stdout(), "\x1b]0;{text}\x07");
        self.title_set = true;
    }

    // --- transport ---------------------------------------------------------------------------

    fn apply_outs(&mut self) {
        let Some(t) = self.transport.as_mut() else { return };
        let outs = t.take();
        let remembered = std::mem::take(&mut t.remembered);
        for c in remembered {
            self.remember_local(c);
        }
        for o in outs {
            match o {
                Out::Commit(c) => self.footer.append(*c),
                Out::Patch(p) => self.footer.event(FooterEvent::Patch(p)),
                Out::View(v) => self.footer.event(FooterEvent::View(v)),
                Out::Subagent(s) => self.footer.event(FooterEvent::Subagent(s)),
                Out::Queued(q) => self.footer.event(FooterEvent::QueuedPrompts(q)),
                Out::Title(t) => {
                    self.session_title = Some(t.clone());
                    self.set_title(Some(&t));
                }
                Out::CatalogRefresh => self.load_catalog(true),
                Out::Job(j) => self.run_job(j),
            }
        }
    }

    /// `rememberLocal(commit)`.
    fn remember_local(&mut self, commit: StreamCommit) {
        if let Some(last) = self.local_rows.last()
            && matches!(commit.kind, EntryKind::Assistant | EntryKind::Reasoning)
            && last.kind == commit.kind
            && last.source == commit.source
            && last.message_id == commit.message_id
            && last.part_id == commit.part_id
            && last.tool == commit.tool
        {
            *self.local_rows.last_mut().unwrap() = commit;
            return;
        }
        self.local_rows.push(commit);
        if self.local_rows.len() > LOCAL_REPLAY_ROW_LIMIT {
            let n = self.local_rows.len() - LOCAL_REPLAY_ROW_LIMIT;
            self.local_rows.drain(..n);
        }
    }

    fn run_job(&self, job: Job) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = match job {
                Job::HydrateChild(sid) => {
                    let (m, i) = tokio::join!(client.messages(&sid, 80, None), client.inbox(&sid));
                    match (m, i) {
                        (Ok(m), Ok(i)) => JobResult::Child { session_id: sid, messages_desc: m.data, inbox: i.into_iter().map(|x| serde_json::to_value(x).unwrap_or(Value::Null)).collect() },
                        _ => JobResult::ChildFailed(sid),
                    }
                }
                Job::HydrateBlockers(sid) => {
                    let (p, f) = tokio::join!(client.session_permissions(&sid), client.session_forms(&sid));
                    JobResult::Blockers { session_id: sid, permissions: p.unwrap_or_default(), forms: f.unwrap_or_default() }
                }
                Job::Discover(sid) => {
                    let mut lineage = Vec::new();
                    let mut current = sid.clone();
                    let mut ok = true;
                    for _ in 0..100 {
                        match client.session_get(&current).await {
                            Ok(s) => {
                                let parent = s.parent_id.clone();
                                lineage.push(s);
                                match parent {
                                    Some(p) if !lineage.iter().any(|l| l.id == p) => current = p,
                                    _ => break,
                                }
                            }
                            Err(_) => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if ok { JobResult::Discovered { session_id: sid, lineage } } else { JobResult::DiscoverFailed(sid) }
                }
                Job::Family(owner) => {
                    let sessions = client.session_list(&[("parentID", owner.clone()), ("limit", "100".into()), ("order", "desc".into())]).await.map(|p| p.data).unwrap_or_default();
                    JobResult::Family { owner, sessions }
                }
            };
            let _ = tx.send(Msg::Job(result));
        });
    }

    /// Start the event stream (`ensureStream` → `createSessionTransport` → `connect`).
    fn ensure_stream(&mut self) {
        if self.transport.is_some() || self.session_id.is_empty() || self.footer.is_closed() {
            return;
        }
        let mut t = Transport::new(&self.session_id, Some(self.location.clone()), self.settings.thinking, self.settings.tools, self.replay);
        for p in &self.providers {
            for (id, m) in &p.models {
                if let Some(c) = m.context {
                    t.context_limits.insert(format!("{}\u{0}{id}", p.id), c);
                }
            }
        }
        self.transport = Some(t);
        self.connect();
    }

    fn connect(&mut self) {
        self.generation += 1;
        let generation = self.generation;
        self.booting = true;
        self.buffered.clear();
        if let Some(h) = self.connect_task.take() {
            h.abort();
        }
        let client = self.client.clone();
        let tx = self.tx.clone();
        self.connect_task = Some(tokio::spawn(async move {
            let resp = match client.event_stream().await {
                Ok(r) => r,
                Err(_) => {
                    let _ = tx.send(Msg::Disconnected(generation));
                    return;
                }
            };
            let mut stream = resp.bytes_stream();
            let mut parser = SseParser::default();
            let mut first = true;
            while let Some(chunk) = stream.next().await {
                let Ok(chunk) = chunk else { break };
                for data in parser.feed(&String::from_utf8_lossy(&chunk)) {
                    let Ok(ev) = serde_json::from_str::<Event>(&data) else { continue };
                    if first {
                        first = false;
                        if ev.kind != "server.connected" {
                            let _ = tx.send(Msg::Disconnected(generation));
                            return;
                        }
                        let _ = tx.send(Msg::Connected(generation));
                        continue;
                    }
                    if tx.send(Msg::SseEvent(generation, Box::new(ev))).is_err() {
                        return;
                    }
                }
            }
            let _ = tx.send(Msg::Disconnected(generation));
        }));
    }

    fn hydrate(&mut self, mode: HydrateMode) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let sid = self.session_id.clone();
        let dir = self.location.clone();
        let limit = self.replay_limit;
        let generation = self.generation;
        self.hydrating = true;
        tokio::spawn(async move {
            let result: Result<Hydration, String> = async {
                let (messages, inbox, permissions, forms, globals, active) = tokio::join!(
                    client.messages(&sid, limit, None),
                    client.inbox(&sid),
                    client.session_permissions(&sid),
                    client.session_forms(&sid),
                    client.forms(Some(&dir)),
                    client.session_active(),
                );
                let mut projected = messages.map_err(|e| err_text(&e))?.data;
                projected.reverse();
                let permissions = permissions.map_err(|e| err_text(&e))?;
                // `resolvePermissionSources`: fetch messages of tool-sourced permissions.
                let mut ids: Vec<String> = Vec::new();
                for p in &permissions {
                    if let Some(s) = &p.source
                        && s.get("type").and_then(|t| t.as_str()) == Some("tool")
                        && let Some(m) = s.get("messageID").and_then(|m| m.as_str())
                        && !ids.iter().any(|i| i == m)
                    {
                        ids.push(m.to_string());
                    }
                }
                let mut sources = Vec::new();
                for id in ids {
                    if let Ok(m) = client.message_get(&sid, &id).await {
                        sources.push(m);
                    }
                }
                Ok(Hydration {
                    projected,
                    inbox: inbox.map_err(|e| err_text(&e))?.into_iter().map(|i| serde_json::to_value(i).unwrap_or(Value::Null)).collect(),
                    permissions,
                    sources,
                    forms: forms.map_err(|e| err_text(&e))?,
                    globals: globals.ok().map(|g| (g, dir.clone())),
                    active: active.unwrap_or(Value::Object(Default::default())),
                })
            }
            .await;
            let _ = tx.send(Msg::Hydrated(generation, mode, Box::new(result)));
        });
    }

    fn on_hydrated(&mut self, mode: HydrateMode, h: Hydration) {
        self.hydrating = false;
        let Some(t) = self.transport.as_mut() else { return };
        match mode {
            HydrateMode::Connect => {
                let render = if t.initial { self.replay } else { true };
                let reconnect = !t.initial;
                t.hydrate(&h.projected, &h.inbox, h.permissions, &h.sources, h.forms, h.globals, &h.active, render, reconnect);
                t.initial = false;
                self.apply_outs();
                let buffered = std::mem::take(&mut self.buffered);
                let loc = self.location.clone();
                if let Some(t) = self.transport.as_mut() {
                    for e in &buffered {
                        t.apply(e, Some(&loc));
                    }
                    t.connected = true;
                }
                self.booting = false;
                self.apply_outs();
                self.load_catalog(true);
                self.pump_queue();
            }
            HydrateMode::Resize => {
                t.buffering = true;
                t.hydrate(&h.projected, &h.inbox, h.permissions, &h.sources, h.forms, h.globals, &h.active, true, false);
                t.buffering = false;
                let rows = self.local_rows.clone();
                let replayed = self.transport.as_mut().map(|t| t.replay_local(&rows)).unwrap_or_default();
                self.apply_outs();
                for c in replayed {
                    self.footer.append(c);
                }
                self.replaying = false;
                let buffered = std::mem::take(&mut self.replay_buffer);
                let loc = self.location.clone();
                if let Some(t) = self.transport.as_mut() {
                    for e in &buffered {
                        t.apply(e, Some(&loc));
                    }
                }
                self.apply_outs();
            }
        }
    }

    fn on_event(&mut self, ev: Event) {
        if self.booting {
            self.buffered.push(ev);
            return;
        }
        if self.replaying {
            self.replay_buffer.push(ev);
            return;
        }
        let loc = self.location.clone();
        if let Some(t) = self.transport.as_mut() {
            t.apply(&ev, Some(&loc));
        }
        self.apply_outs();
        self.check_shell();
    }

    // --- session / catalog / models ----------------------------------------------------------

    fn start_session(&mut self) {
        if self.resolving || !self.session_id.is_empty() {
            return;
        }
        self.resolving = true;
        let client = self.client.clone();
        let tx = self.tx.clone();
        let dir = self.directory.clone();
        let o = self.opts.clone();
        let model = self.requested_model();
        let env = o.service.as_ref().map(|_| serde_json::to_value(&o.environment).unwrap_or(Value::Null));
        tokio::spawn(async move {
            let r = resolve_target(client, dir, o.session.clone(), o.continue_last, o.fork, model, o.agent.clone(), env).await;
            let _ = tx.send(Msg::Session(r));
        });
    }

    fn requested_model(&self) -> Option<(ModelSel, Option<String>)> {
        let m = self.opts.model.as_deref()?;
        let (base, variant) = match m.split_once('#') {
            Some((b, v)) => (b, Some(v.to_string())),
            None => (m, None),
        };
        let (p, id) = base.split_once('/')?;
        Some((ModelSel { provider_id: p.into(), model_id: id.into() }, variant))
    }

    fn on_session(&mut self, r: Resolved) {
        self.session_id = r.session.id.clone();
        self.session_title = r.session.title.clone();
        let t = self.session_title.clone();
        self.set_title(t.as_deref());
        self.agent = r.agent.clone();
        self.location = r.location.clone();
        self.model = r.model.clone();
        self.variant = r.variant.clone();
        self.footer.event(FooterEvent::Agent(self.agent.clone()));
        if r.resume {
            let client = self.client.clone();
            let tx = self.tx.clone();
            let sid = self.session_id.clone();
            let session = r.session.clone();
            let model = r.model.clone();
            tokio::spawn(async move {
                let msg = match client.messages(&sid, 200, None).await {
                    Ok(page) => {
                        let s = super::session_shared::current_session(&page.data, &session);
                        let history = super::session_shared::session_history(&s);
                        let variant = pick_variant(model.as_ref().or(s.model.as_ref()), &s);
                        Msg::SessionInfo { first: s.first, history, model: s.model.clone(), variant }
                    }
                    Err(_) => Msg::SessionInfo { first: true, history: vec![], model: None, variant: None },
                };
                let _ = tx.send(msg);
            });
            return;
        }
        self.after_session();
    }

    fn after_session(&mut self) {
        self.ensure_stream();
        self.load_models();
        self.load_catalog(false);
    }

    fn reset_for_replay(&mut self) {
        let mut sink = Sink2 { split: &mut self.split };
        self.footer.idle(&mut sink);
        let (buf, cursor) = self.footer.render(Instant::now());
        self.split.render(&buf, cursor);
        self.footer.reset_for_replay(true);
        self.split.reset_for_replay(true);
        let c = splash::entry_splash(self.split.width(), &self.opts.version, Some(&self.directory_label()), self.settings.mono, &self.footer.theme.splash);
        self.split.enqueue(c);
        self.write_out();
    }

    fn directory_label(&self) -> String {
        let home = super::util::home_dir();
        let dir = self.location.clone();
        if dir == home {
            return "~".into();
        }
        if !home.is_empty() && dir.starts_with(&format!("{home}/")) {
            return format!("~{}", &dir[home.len()..]);
        }
        dir
    }

    fn load_catalog(&self, models_too: bool) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let dir = self.location.clone();
        tokio::spawn(async move {
            let q = [("location[directory]", dir.clone())];
            let (a, r, c) = tokio::join!(client.get_value("/api/agent", &q), client.get_value("/api/reference", &q), client.get_value("/api/command", &q));
            let list = |v: anyhow::Result<Value>| v.ok().and_then(|v| v.get("data").and_then(|d| d.as_array()).cloned()).unwrap_or_default();
            let agents = list(a).iter().map(catalog::run_agent).collect();
            let references = catalog::run_references(&list(r));
            let commands = list(c).iter().map(catalog::run_command).collect();
            let _ = tx.send(Msg::Catalog { agents, references, commands });
        });
        if models_too {
            self.load_models();
        }
    }

    fn load_models(&self) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let dir = self.location.clone();
        let need_default = self.model.is_none();
        tokio::spawn(async move {
            let q = [("location[directory]", dir.clone())];
            let (p, m) = tokio::join!(client.get_value("/api/provider", &q), client.get_value("/api/model", &q));
            let list = |v: anyhow::Result<Value>| v.ok().and_then(|v| v.get("data").and_then(|d| d.as_array()).cloned()).unwrap_or_default();
            let providers = catalog::run_providers(&list(p), &list(m));
            let _ = tx.send(Msg::Models(providers));
            if need_default
                && let Ok(Some(d)) = client.default_model(Some(&dir)).await
            {
                let _ = tx.send(Msg::DefaultModel(Some(ModelSel { provider_id: d.provider_id, model_id: d.id })));
            }
        });
    }

    fn variants_for(&self, model: Option<&ModelSel>) -> Vec<String> {
        let Some(m) = model else { return vec![] };
        self.providers.iter().find(|p| p.id == m.provider_id).and_then(|p| p.models.get(&m.model_id)).map(|x| x.variants.clone()).unwrap_or_default()
    }

    fn model_events(&mut self) {
        let model = self.model.clone().or(self.default_model.clone());
        self.footer.event(FooterEvent::Variants { variants: self.variants.clone(), current: self.variant.clone() });
        if let Some(m) = model {
            let label = format_model_label(&m, self.variant.as_deref(), Some(&self.providers));
            self.footer.event(FooterEvent::Model { label, selection: Some(m) });
        }
    }

    fn preferences_path(&self) -> std::path::PathBuf {
        crate::util::paths::state_dir().join("model.json")
    }

    fn saved_variant(&self, model: &ModelSel) -> Option<String> {
        let text = std::fs::read_to_string(self.preferences_path()).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        v.get("variant").and_then(|x| x.get(format!("{}/{}", model.provider_id, model.model_id))).and_then(|x| x.as_str()).map(str::to_string)
    }

    fn save_variant(&self, model: &ModelSel, variant: Option<&str>) {
        let path = self.preferences_path();
        let mut v: Value = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(json!({}));
        if !v.is_object() {
            v = json!({});
        }
        let k = format!("{}/{}", model.provider_id, model.model_id);
        let map = v.as_object_mut().unwrap().entry("variant").or_insert(json!({}));
        if let Some(m) = map.as_object_mut() {
            match variant {
                Some(x) => {
                    m.insert(k, json!(x));
                }
                None => {
                    m.remove(&k);
                }
            }
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap_or_default());
    }

    // --- footer actions ----------------------------------------------------------------------

    fn footer_action(&mut self, a: FooterAction) {
        match a {
            FooterAction::Prompt(p) => self.submit(p),
            FooterAction::Close => {
                self.done = true;
                self.queue.clear();
            }
            FooterAction::CycleVariant => {
                let model = self.model.clone().or(self.default_model.clone());
                match model {
                    Some(m) if !self.variants.is_empty() => {
                        if self.model.is_none() {
                            self.model = Some(m.clone());
                        }
                        self.variant = cycle_variant(self.variant.as_deref(), &self.variants);
                        self.save_variant(&m, self.variant.as_deref());
                        let status = match &self.variant {
                            Some(v) => format!("variant {v}"),
                            None => "variant default".into(),
                        };
                        let label = format_model_label(&m, self.variant.as_deref(), Some(&self.providers));
                        self.footer.cycle_result(Some((self.variant.clone(), label, Some(status))));
                    }
                    _ => self.footer.cycle_result(Some((self.variant.clone(), self.footer.state.model.clone(), Some("no variants available".into())))),
                }
            }
            FooterAction::AgentSelect(a) => self.agent = Some(a),
            FooterAction::ModelSelect(m) => {
                if self.model.as_ref() == Some(&m) {
                    return;
                }
                self.model = Some(m.clone());
                self.variants = self.variants_for(Some(&m));
                let saved = self.saved_variant(&m);
                self.variant = resolve_variant(None, None, saved.as_deref(), &self.variants);
                let label = format_model_label(&m, self.variant.as_deref(), Some(&self.providers));
                self.footer.apply_selection(Some(self.variants.clone()), Some(self.variant.clone()), Some(label), Some(format!("model {}", m.model_id)));
            }
            FooterAction::VariantSelect(v) => {
                let model = self.model.clone().or(self.default_model.clone());
                let Some(m) = model.filter(|_| !self.variants.is_empty()) else {
                    self.footer.apply_selection(None, None, None, Some("no variants available".into()));
                    return;
                };
                if let Some(x) = &v
                    && !self.variants.contains(x)
                {
                    self.footer.apply_selection(None, None, None, Some(format!("variant {x} unavailable")));
                    return;
                }
                if self.model.is_none() {
                    self.model = Some(m.clone());
                }
                self.variant = v;
                self.save_variant(&m, self.variant.as_deref());
                let status = match &self.variant {
                    Some(x) => format!("variant {x}"),
                    None => "variant default".into(),
                };
                let label = format_model_label(&m, self.variant.as_deref(), Some(&self.providers));
                self.footer.apply_selection(Some(self.variants.clone()), Some(self.variant.clone()), Some(label), Some(status));
            }
            FooterAction::Interrupt => self.interrupt(),
            FooterAction::Background => {
                if !self.session_id.is_empty() {
                    let (client, sid) = (self.client.clone(), self.session_id.clone());
                    tokio::spawn(async move {
                        let _ = client.background(&sid).await;
                    });
                }
            }
            FooterAction::QueuedAction(action, inbox) => {
                if self.session_id.is_empty() {
                    return;
                }
                let (client, sid, tx) = (self.client.clone(), self.session_id.clone(), self.tx.clone());
                tokio::spawn(async move {
                    let r = match action {
                        QueuedPromptAction::Cancel => client.inbox_cancel(&sid, &inbox).await,
                        QueuedPromptAction::Steer => client.inbox_update(&sid, &inbox, "steer").await,
                        QueuedPromptAction::Queue => client.inbox_update(&sid, &inbox, "queue").await,
                    };
                    if let Err(e) = r {
                        let label = match action {
                            QueuedPromptAction::Cancel => "delete",
                            QueuedPromptAction::Steer => "steer",
                            QueuedPromptAction::Queue => "queue",
                        };
                        let _ = tx.send(Msg::QueuedActionFailed(format!("failed to {label} pending prompt: {}", err_text(&e))));
                    }
                });
            }
            FooterAction::EditorOpen(value) => self.pending_editor = Some(value),
            FooterAction::SubagentSelect(id) => {
                if let Some(t) = self.transport.as_mut() {
                    t.subagent_select(id);
                }
                self.apply_outs();
            }
            FooterAction::SubagentInterrupt(sid) => {
                let client = self.client.clone();
                tokio::spawn(async move {
                    let _ = client.interrupt(&sid, false).await;
                });
            }
            FooterAction::MiniSetting(change) => {
                let writes = change_writes(&change);
                for (k, v) in writes {
                    self.config.set(k, Some(v));
                }
                let settings = resolve_mini_settings(self.config.get("mini"));
                let _ = self.tx.send(Msg::SettingsSaved(Ok(settings)));
            }
            FooterAction::FindFiles(query) => {
                let (client, tx, dir) = (self.client.clone(), self.tx.clone(), self.location.clone());
                tokio::spawn(async move {
                    let files = client.file_find(&query, 50, Some(&dir)).await.map(|l| l.into_iter().map(|f| f.path).collect()).unwrap_or_default();
                    let _ = tx.send(Msg::Files { query, files });
                });
            }
            FooterAction::ClearScreen => {
                let (buf, cursor) = self.footer.render(Instant::now());
                self.split.render(&buf, cursor);
                self.split.reset_for_replay(false);
            }
            FooterAction::Paste => {}
            FooterAction::PermissionReply(r) => {
                let (client, tx) = (self.client.clone(), self.tx.clone());
                tokio::spawn(async move {
                    if client.permission_reply(&r.session_id, &r.request_id, r.decision, r.message.as_deref()).await.is_err() {
                        let _ = tx.send(Msg::PermissionFailed);
                    }
                });
            }
            FooterAction::FormReply(r) => {
                let (client, tx) = (self.client.clone(), self.tx.clone());
                tokio::spawn(async move {
                    let res = client.form_reply(&r.session_id, &r.form_id, Value::Object(r.answer.clone()), r.location.as_deref()).await;
                    match res {
                        Err(e) if !err_text(&e).contains("FormAlreadySettled") && e.downcast_ref::<crate::api::ApiError>().is_none_or(|a| a.tag != "FormAlreadySettledError") => {
                            let _ = tx.send(Msg::FormFailed(err_text(&e)));
                        }
                        _ => {
                            let _ = tx.send(Msg::FormSettled { session_id: r.session_id, form_id: r.form_id });
                        }
                    }
                });
            }
            FooterAction::FormCancel { session_id, form_id, location } => {
                let (client, tx) = (self.client.clone(), self.tx.clone());
                tokio::spawn(async move {
                    let res = client.form_cancel(&session_id, &form_id, location.as_deref()).await;
                    match res {
                        Err(e) if e.downcast_ref::<crate::api::ApiError>().is_none_or(|a| a.tag != "FormAlreadySettledError") => {
                            let _ = tx.send(Msg::FormFailed(err_text(&e)));
                        }
                        _ => {
                            let _ = tx.send(Msg::FormSettled { session_id, form_id });
                        }
                    }
                });
            }
        }
    }

    fn interrupt(&mut self) {
        if self.session_id.is_empty() {
            return;
        }
        let Some(t) = self.transport.as_mut() else {
            let (client, sid) = (self.client.clone(), self.session_id.clone());
            tokio::spawn(async move {
                let _ = client.interrupt(&sid, true).await;
            });
            return;
        };
        if t.shell_wait.is_some() {
            // Abort the blocking shell request; the command keeps its own lifecycle.
            t.shell_wait = None;
            if let Some(a) = self.active.as_mut()
                && a.prompt.shell
            {
                a.done = true;
            }
            self.finish_turn();
            return;
        }
        if let Some(w) = t.wait.as_mut() {
            w.interrupted = true;
        }
        let epoch = t.execution_epoch;
        let (client, sid, tx) = (self.client.clone(), self.session_id.clone(), self.tx.clone());
        tokio::spawn(async move {
            let ok = client.interrupt(&sid, true).await.is_ok();
            let _ = tx.send(Msg::Interrupted { epoch, ok });
        });
    }

    // --- prompt queue ------------------------------------------------------------------------

    /// `submit(prompt)` of `runPromptQueue`.
    fn submit(&mut self, prompt: RunPrompt) {
        if self.done || (prompt.text.trim().is_empty() && (prompt.shell || !prompt.parts.iter().any(|p| p.is_file()))) {
            return;
        }
        if !prompt.shell && is_exit_command(&prompt.text) {
            self.footer.close();
            return;
        }
        let active_ordinary = self.active.as_ref().is_some_and(|a| !a.prompt.shell);
        if active_ordinary
            && !prompt.shell
            && prompt.command.as_ref().and_then(|c| c.source.as_deref()) != Some("skill")
            && !is_new_command(&prompt.text)
            && !is_compact_command(&prompt.text)
            && !self.queue.iter().any(|q| !q.shell && (is_new_command(&q.text) || is_compact_command(&q.text)))
        {
            let mut sent = prompt.clone();
            sent.message_id = Some(crate::util::message_id());
            self.admission_version += 1;
            let delivery = prompt.delivery.unwrap_or(Delivery::Queue);
            self.on_send(&sent);
            self.admit(sent, delivery);
            return;
        }
        let special = !prompt.shell && (is_new_command(&prompt.text) || is_compact_command(&prompt.text));
        self.queue.push_back(prompt);
        if !special {
            self.footer.event(FooterEvent::First(false));
        }
        self.pump_queue();
    }

    fn on_send(&mut self, prompt: &RunPrompt) {
        self.shown = true;
        self.history.push(RunPrompt { delivery: None, ..prompt.clone() });
    }

    /// Admit a prompt into the server queue while a turn runs (`admitPromptTurn`).
    fn admit(&mut self, prompt: RunPrompt, delivery: Delivery) {
        self.admissions_inflight += 1;
        let ctx = self.turn_context();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let r = ctx.send_prompt(&prompt, delivery, true).await;
            let _ = tx.send(Msg::QueueAdmitted { error: r.err().map(|e| (prompt, e)) });
        });
    }

    fn turn_context(&self) -> TurnContext {
        TurnContext { client: self.client.clone(), session_id: self.session_id.clone(), agent: self.agent.clone(), model: self.model.clone(), variant: self.variant.clone(), location: self.location.clone(), tx: self.tx.clone() }
    }

    /// Drain the queue when idle.
    fn pump_queue(&mut self) {
        if self.done || self.active.is_some() || self.settling || self.session_id.is_empty() {
            return;
        }
        let connected = self.transport.as_ref().is_some_and(|t| t.connected);
        let Some(prompt) = self.queue.pop_front() else { return };
        if !prompt.shell && is_new_command(&prompt.text) {
            self.footer.event(FooterEvent::Patch(FooterPatch { phase: Some(FooterPhase::Running), status: Some("starting new session".into()), notice: Some(String::new()), ..Default::default() }));
            self.new_session();
            return;
        }
        if !prompt.shell && is_compact_command(&prompt.text) {
            self.footer.event(FooterEvent::Patch(FooterPatch { phase: Some(FooterPhase::Running), status: Some("compacting session".into()), notice: Some(String::new()), ..Default::default() }));
            let (client, sid, tx) = (self.client.clone(), self.session_id.clone(), self.tx.clone());
            self.token += 1;
            let token = self.token;
            self.active = Some(ActiveTurn { token, prompt: prompt.clone(), start: Instant::now(), version: self.admission_version, done: false, shell_deadline: None });
            tokio::spawn(async move {
                let r = client.compact(&sid, &crate::util::message_id()).await;
                let _ = tx.send(Msg::CommandSent { token, error: r.err().map(|e| err_text(&e)) });
            });
            return;
        }
        if !connected {
            self.queue.push_front(prompt);
            return;
        }
        let mut sent = prompt.clone();
        if !sent.shell {
            sent.message_id = Some(sent.message_id.clone().unwrap_or_else(crate::util::message_id));
        }
        self.token += 1;
        let token = self.token;
        self.footer.event(FooterEvent::TurnSend);
        self.active = Some(ActiveTurn { token, prompt: sent.clone(), start: Instant::now(), version: self.admission_version, done: false, shell_deadline: None });
        self.on_send(&sent);
        let ctx = self.turn_context();
        if sent.shell {
            let mid = crate::util::message_id();
            if let Some(t) = self.transport.as_mut() {
                t.shell_wait = Some(ShellWait { event_id: event_id_from_message(&mid), message_id: mid.clone(), id: None, resolved: false });
                t.out.push(Out::Patch(FooterPatch { phase: Some(FooterPhase::Running), status: Some("running shell".into()), ..Default::default() }));
            }
            self.apply_outs();
            tokio::spawn(async move {
                let r = ctx.client.post_value(&format!("/api/session/{}/shell", ctx.session_id), json!({ "id": mid, "command": sent.text })).await;
                let _ = ctx.tx.send(Msg::ShellSent { token, error: r.err().map(|e| err_text(&e)) });
            });
            return;
        }
        if sent.command.is_some() {
            tokio::spawn(async move {
                let r = ctx.send_prompt(&sent, sent.delivery.unwrap_or(Delivery::Steer), false).await;
                let _ = ctx.tx.send(Msg::CommandSent { token, error: r.err() });
            });
            return;
        }
        let mid = sent.message_id.clone().unwrap();
        if let Some(t) = self.transport.as_mut() {
            t.wait = Some(Wait { message_id: mid.clone(), failure_message_id: mid, ..Default::default() });
        }
        tokio::spawn(async move {
            match ctx.send_prompt(&sent, sent.delivery.unwrap_or(Delivery::Steer), false).await {
                Ok(()) => {
                    let messages = ctx.settle().await;
                    let _ = ctx.tx.send(Msg::TurnSettled { token, messages });
                }
                Err(e) => {
                    let _ = ctx.tx.send(Msg::TurnFailed { token, error: e });
                }
            }
        });
    }

    /// The active turn ended: duration, then the next queued item or idle.
    fn finish_turn(&mut self) {
        let Some(a) = self.active.take() else { return };
        if !a.prompt.shell && !(is_new_command(&a.prompt.text) || is_compact_command(&a.prompt.text)) {
            if a.prompt.message_id.is_some() {
                let mid = a.prompt.message_id.clone();
                self.local_rows.retain(|r| r.kind != EntryKind::User || r.message_id != mid);
            }
            let duration = crate::util::locale::duration(a.start.elapsed().as_millis() as i64);
            self.footer.event(FooterEvent::TurnDuration(duration));
        }
        if a.version != self.admission_version || self.admissions_inflight > 0 {
            self.settling = true;
            let ctx = self.turn_context();
            tokio::spawn(async move {
                let messages = ctx.settle().await;
                let _ = ctx.tx.send(Msg::Settled { messages });
            });
            return;
        }
        self.after_turn();
    }

    fn after_turn(&mut self) {
        if self.queue.is_empty() {
            self.footer.event(FooterEvent::TurnIdle);
        }
        self.pump_queue();
    }

    fn render_settled(&mut self, messages: Vec<Value>) {
        if let Some(t) = self.transport.as_mut() {
            for m in &messages {
                t.render_message(m, true);
            }
            let status = blocker_status(&t.view).to_string();
            t.paint_idle(&status);
        }
        self.apply_outs();
    }

    fn new_session(&mut self) {
        self.token += 1;
        let token = self.token;
        self.active = Some(ActiveTurn { token, prompt: RunPrompt::text("/new"), start: Instant::now(), version: self.admission_version, done: false, shell_deadline: None });
        let client = self.client.clone();
        let tx = self.tx.clone();
        let dir = match self.config.str("session.new_location") {
            Some("current") => self.location.clone(),
            _ => self.directory.clone(),
        };
        let model = self.model.clone().map(|m| (m, self.variant.clone()));
        let agent = self.agent.clone();
        let env = self.opts.service.as_ref().map(|_| serde_json::to_value(&self.opts.environment).unwrap_or(Value::Null));
        tokio::spawn(async move {
            let r = resolve_target(client, dir, None, false, false, model, agent, env).await;
            let _ = tx.send(Msg::NewSession(r));
        });
    }

    fn on_new_session(&mut self, r: Result<Resolved, String>) {
        self.active = None;
        match r {
            Ok(r) => {
                let mut sink = Sink2 { split: &mut self.split };
                self.footer.idle(&mut sink);
                if let Some(h) = self.connect_task.take() {
                    h.abort();
                }
                self.transport = None;
                self.shown = false;
                self.session_id = r.session.id.clone();
                self.session_title = r.session.title.clone();
                let t = self.session_title.clone();
                self.set_title(t.as_deref());
                self.agent = r.agent.or(self.agent.clone());
                self.location = r.location;
                self.model = r.model;
                self.variant = r.variant;
                self.footer.event(FooterEvent::Agent(self.agent.clone()));
                self.history.clear();
                self.local_rows.clear();
                self.footer.event(FooterEvent::Subagent(SubagentState::default()));
                self.footer.event(FooterEvent::View(FooterView::Prompt));
                self.footer.event(FooterEvent::QueuedPrompts(vec![]));
                self.footer.event(FooterEvent::Patch(FooterPatch { phase: Some(FooterPhase::Idle), usage: Some(None), first: Some(true), ..Default::default() }));
                self.footer.append(StreamCommit { kind: EntryKind::System, text: format!("new session {}", self.session_id), phase: Phase::Final, source: Source::System, ..Default::default() });
                self.ensure_stream();
            }
            Err(e) => {
                self.footer.event(FooterEvent::Patch(FooterPatch { phase: Some(FooterPhase::Idle), status: Some("failed to start new session".into()), notice: Some(String::new()), ..Default::default() }));
                let commit = StreamCommit { kind: EntryKind::Error, text: e, phase: Phase::Start, source: Source::System, message_id: Some(crate::util::message_id()), ..Default::default() };
                self.remember_local(commit.clone());
                self.footer.append(commit);
            }
        }
        self.after_turn();
    }

    fn render_prompt_error(&mut self, prompt: &RunPrompt, error: String) {
        if self.done {
            return;
        }
        let commit = StreamCommit { kind: EntryKind::Error, text: error, phase: Phase::Start, source: Source::System, message_id: prompt.message_id.clone(), ..Default::default() };
        self.remember_local(commit.clone());
        self.footer.append(commit);
    }

    fn check_shell(&mut self) {
        let resolved = self.transport.as_ref().and_then(|t| t.shell_wait.as_ref()).is_some_and(|w| w.resolved);
        if resolved && let Some(a) = self.active.as_ref()
            && a.prompt.shell
            && a.done
        {
            if let Some(t) = self.transport.as_mut() {
                t.shell_wait = None;
            }
            self.finish_turn();
        }
    }

    // --- messages ----------------------------------------------------------------------------

    fn on_msg(&mut self, m: Msg) {
        if let Some(t) = self.trace.as_mut() {
            let text = format!("{m:?}");
            let short: String = text.chars().take(400).collect();
            t.write("recv.msg", Value::String(short));
        }
        match m {
            Msg::Session(Ok(r)) => self.on_session(r),
            Msg::Session(Err(e)) => {
                self.resolving = false;
                self.fatal = Some(e);
                self.footer.close();
            }
            Msg::SessionInfo { first, history, model, variant } => {
                self.history = history.clone();
                self.shown = !first;
                if self.model.is_none() {
                    self.model = model;
                }
                let saved = self.model.as_ref().and_then(|m| self.saved_variant(m));
                self.variant = resolve_variant(self.variant.as_deref(), variant.as_deref(), saved.as_deref(), &[]);
                self.session_variant = self.variant.clone();
                self.footer.event(FooterEvent::History(history));
                self.footer.event(FooterEvent::First(first));
                self.reset_for_replay();
                self.after_session();
            }
            Msg::NewSession(r) => self.on_new_session(r),
            Msg::Catalog { agents, references, commands } => {
                self.footer.event(FooterEvent::Catalog { agents, references, commands: Some(commands) });
            }
            Msg::Models(providers) => {
                self.providers = providers;
                let model = self.model.clone().or(self.default_model.clone());
                self.variants = self.variants_for(model.as_ref());
                if let Some(m) = &model {
                    let saved = self.saved_variant(m);
                    self.variant = resolve_variant(self.requested_model().and_then(|r| r.1).as_deref(), self.variant.as_deref().or(self.session_variant.as_deref()), saved.as_deref(), &self.variants);
                } else if self.variant.as_ref().is_some_and(|v| !self.variants.contains(v)) {
                    self.variant = None;
                }
                if let Some(t) = self.transport.as_mut() {
                    for p in &self.providers {
                        for (id, m) in &p.models {
                            if let Some(c) = m.context {
                                t.context_limits.insert(format!("{}\u{0}{id}", p.id), c);
                            }
                        }
                    }
                }
                self.footer.event(FooterEvent::Models(self.providers.clone()));
                self.model_events();
            }
            Msg::DefaultModel(m) => {
                if self.model.is_some() {
                    return;
                }
                self.default_model = m.clone();
                self.variants = self.variants_for(m.as_ref());
                if let Some(m) = &m {
                    let saved = self.saved_variant(m);
                    self.variant = resolve_variant(None, self.variant.as_deref(), saved.as_deref(), &self.variants);
                    if self.variant.is_some() {
                        self.model = Some(m.clone());
                    }
                }
                self.model_events();
            }
            Msg::Connected(g) if g == self.generation => self.hydrate(HydrateMode::Connect),
            Msg::Connected(_) => {}
            Msg::SseEvent(g, ev) if g == self.generation => self.on_event(*ev),
            Msg::SseEvent(..) => {}
            Msg::Disconnected(g) if g == self.generation => {
                if let Some(t) = self.transport.as_mut() {
                    t.connected = false;
                }
                if self.done {
                    return;
                }
                self.footer.event(FooterEvent::Patch(FooterPatch { phase: Some(FooterPhase::Running), status: Some("reconnecting".into()), notice: Some(String::new()), ..Default::default() }));
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let _ = tx.send(Msg::Disconnected(u64::MAX));
                });
            }
            Msg::Disconnected(u64::MAX) => {
                if !self.done && self.transport.is_some() {
                    self.connect();
                }
            }
            Msg::Disconnected(..) => {}
            Msg::Hydrated(g, mode, r) if g == self.generation => match *r {
                Ok(h) => self.on_hydrated(mode, h),
                Err(_) => {
                    self.hydrating = false;
                    self.replaying = false;
                    if mode == HydrateMode::Connect {
                        self.connect();
                    }
                }
            },
            Msg::Hydrated(..) => {}
            Msg::Admitted(item) => {
                if let Some(t) = self.transport.as_mut() {
                    t.merge_pending(&item);
                }
                self.apply_outs();
            }
            Msg::TurnSettled { token, messages } => {
                if self.active.as_ref().is_none_or(|a| a.token != token) {
                    return;
                }
                if let Ok(m) = messages {
                    self.render_settled(m);
                }
                let (terminal, message_id, rendered) = self.transport.as_mut().and_then(|t| t.wait.take()).map(|w| (w.terminal_error, w.failure_message_id, w.failure_rendered)).unwrap_or((None, String::new(), true));
                if let Some(e) = terminal
                    && !rendered
                {
                    self.footer.append(StreamCommit { kind: EntryKind::Error, source: Source::System, text: e, phase: Phase::Start, message_id: Some(message_id), ..Default::default() });
                }
                self.finish_turn();
            }
            Msg::TurnFailed { token, error } => {
                if self.active.as_ref().is_none_or(|a| a.token != token) {
                    return;
                }
                if let Some(t) = self.transport.as_mut() {
                    t.wait = None;
                }
                let prompt = self.active.as_ref().map(|a| a.prompt.clone()).unwrap_or_default();
                self.render_prompt_error(&prompt, error);
                self.finish_turn();
            }
            Msg::CommandSent { token, error } => {
                if self.active.as_ref().is_none_or(|a| a.token != token) {
                    return;
                }
                if let Some(e) = error {
                    let prompt = self.active.as_ref().map(|a| a.prompt.clone()).unwrap_or_default();
                    self.render_prompt_error(&prompt, e);
                }
                self.finish_turn();
            }
            Msg::ShellSent { token, error } => {
                if self.active.as_ref().is_none_or(|a| a.token != token) {
                    return;
                }
                if let Some(e) = error {
                    if let Some(t) = self.transport.as_mut() {
                        t.shell_wait = None;
                    }
                    let prompt = self.active.as_ref().map(|a| a.prompt.clone()).unwrap_or_default();
                    self.render_prompt_error(&prompt, e);
                    self.finish_turn();
                    return;
                }
                if let Some(a) = self.active.as_mut() {
                    a.done = true;
                    a.shell_deadline = Some(Instant::now() + SHELL_OUTPUT_GRACE);
                }
                self.check_shell();
            }
            Msg::QueueAdmitted { error } => {
                self.admissions_inflight = self.admissions_inflight.saturating_sub(1);
                if let Some((p, e)) = error {
                    self.render_prompt_error(&p, e);
                }
            }
            Msg::Settled { messages } => {
                if let Ok(m) = messages {
                    self.render_settled(m);
                }
                self.settling = false;
                self.after_turn();
            }
            Msg::Interrupted { epoch, ok } => {
                if ok && let Some(t) = self.transport.as_mut()
                    && t.execution_epoch == epoch
                {
                    let status = blocker_status(&t.view).to_string();
                    t.paint_idle(&status);
                }
                self.apply_outs();
            }
            Msg::Files { query, files } => self.footer.files_found(&query, files),
            Msg::Job(r) => {
                if let Some(t) = self.transport.as_mut() {
                    t.subagent_result(r);
                }
                self.apply_outs();
            }
            Msg::Editor(r) => self.footer.editor_result(r),
            Msg::SettingsSaved(Ok(s)) => {
                let mono_changed = s.mono != self.settings.mono;
                self.settings = s.clone();
                let theme = mono_changed.then(|| resolve_theme(&self.config, &self.directory, self.colors.as_ref(), s.mono).0);
                if mono_changed {
                    let mut out = std::io::stdout();
                    if s.mono && self.kitty {
                        let _ = execute!(out, PopKeyboardEnhancementFlags);
                    } else if !s.mono && self.kitty {
                        let _ = execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS));
                    }
                }
                if let Some(t) = self.transport.as_mut() {
                    t.thinking = s.thinking;
                    t.tools = s.tools;
                }
                self.footer.settings_saved(s, theme);
            }
            Msg::SettingsSaved(Err(_)) => self.footer.settings_failed(),
            Msg::PermissionFailed => {}
            Msg::FormFailed(e) => {
                let _ = e;
            }
            Msg::FormSettled { session_id, form_id } => {
                if let Some(t) = self.transport.as_mut() {
                    t.settle_form(&session_id, &form_id);
                }
                self.apply_outs();
            }
            Msg::QueuedActionFailed(text) => self.footer.set_notice(text),
        }
    }

    // --- terminal events ---------------------------------------------------------------------

    fn on_terminal(&mut self, ev: CEvent) {
        let now = Instant::now();
        match ev {
            CEvent::Key(k) => {
                if let Some(s) = keymap::stroke_from_event(&k) {
                    self.footer.key(s, now);
                }
            }
            CEvent::Paste(text) => self.footer.paste(&text),
            CEvent::Resize(w, h) => {
                if self.split.has_pending() {
                    let (buf, cursor) = self.footer.render(now);
                    self.split.render(&buf, cursor);
                }
                self.split.resize(w as u32, h as u32);
                self.footer.resize(w as usize, h as usize);
                self.resize_at = Some(now + RESIZE_DELAY);
            }
            _ => {}
        }
    }

    fn resize_replay(&mut self) {
        if !self.replay || self.done {
            return;
        }
        let ready = self.transport.as_ref().is_some_and(|t| t.connected) && !self.hydrating;
        if !ready {
            return;
        }
        self.replaying = true;
        self.reset_for_replay();
        if let Some(t) = self.transport.as_mut() {
            t.reset_projection();
        }
        self.hydrate(HydrateMode::Resize);
    }

    fn open_editor(&mut self, value: String) {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[?2004l\x1b[?25h");
        let _ = out.flush();
        if self.kitty {
            let _ = execute!(out, PopKeyboardEnhancementFlags);
        }
        let _ = disable_raw_mode();
        let result = (|| -> Result<Option<String>, ()> {
            let cmd = std::env::var("VISUAL").ok().filter(|v| !v.is_empty()).or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.is_empty())).ok_or(())?;
            let path = std::env::temp_dir().join(format!("{}.md", crate::util::locale::now_ms()));
            std::fs::write(&path, &value).map_err(|_| ())?;
            let mut parts = cmd.split(' ').filter(|s| !s.is_empty());
            let program = parts.next().unwrap_or_default().to_string();
            let args: Vec<String> = parts.map(|s| s.to_string()).collect();
            let status = std::process::Command::new(&program).args(&args).arg(&path).current_dir(&self.location).status().map_err(|_| ())?;
            let text = std::fs::read_to_string(&path).ok();
            let _ = std::fs::remove_file(&path);
            if !status.success() {
                return Ok(None);
            }
            Ok(text)
        })();
        let _ = enable_raw_mode();
        let _ = out.write_all(b"\x1b[?2004h");
        if self.kitty {
            let _ = execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS));
        }
        let _ = out.flush();
        let _ = self.tx.send(Msg::Editor(result));
    }
}

/// Request context for a turn.
#[derive(Clone)]
struct TurnContext {
    client: Client,
    session_id: String,
    agent: Option<String>,
    model: Option<ModelSel>,
    variant: Option<String>,
    location: String,
    tx: UnboundedSender<Msg>,
}

impl TurnContext {
    /// Switch agent/model when needed, then send the prompt or command (`admitPrompt`).
    async fn send_prompt(&self, prompt: &RunPrompt, delivery: Delivery, queued: bool) -> Result<(), String> {
        let e = |e: anyhow::Error| err_text(&e);
        let command = prompt.command.clone();
        if command.is_none() || queued {
            if command.is_none()
                && let Some(a) = &self.agent
            {
                self.client.switch_agent(&self.session_id, a).await.map_err(e)?;
            }
            if command.is_none() {
                let selected = match &self.model {
                    Some(m) => Some(crate::api::types::ModelRef { id: m.model_id.clone(), provider_id: m.provider_id.clone(), variant: self.variant.clone() }),
                    None if self.variant.is_some() => {
                        let session = self.client.session_get(&self.session_id).await.ok().and_then(|s| s.model);
                        match session {
                            Some(m) => Some(crate::api::types::ModelRef { variant: self.variant.clone(), ..m }),
                            None => self.client.default_model(Some(&self.location)).await.ok().flatten().map(|d| crate::api::types::ModelRef { id: d.id, provider_id: d.provider_id, variant: self.variant.clone() }),
                        }
                    }
                    None => None,
                };
                if self.variant.is_some() && selected.is_none() {
                    return Err("Cannot select a variant before selecting a model".into());
                }
                if let Some(m) = selected {
                    self.client.switch_model(&self.session_id, &m).await.map_err(e)?;
                }
            }
        }
        let files: Vec<Value> = prompt
            .parts
            .iter()
            .filter_map(|p| match p {
                PromptPart::File { url, filename, description, source, .. } => {
                    let mut f = json!({ "uri": url });
                    if let Some(n) = filename {
                        f["name"] = json!(n);
                    }
                    if let Some(d) = description {
                        f["description"] = json!(d);
                    }
                    if let Some((_, r)) = source {
                        f["mention"] = json!({ "start": r.start, "end": r.end, "text": r.value });
                    }
                    Some(f)
                }
                _ => None,
            })
            .collect();
        let agents: Vec<Value> = prompt
            .parts
            .iter()
            .filter_map(|p| match p {
                PromptPart::Agent { name, source } => Some(match source {
                    Some(r) => json!({ "name": name, "mention": { "start": r.start, "end": r.end, "text": r.value } }),
                    None => json!({ "name": name }),
                }),
                _ => None,
            })
            .collect();
        let skills: Vec<Value> = prompt
            .parts
            .iter()
            .filter_map(|p| match p {
                PromptPart::Skill { id, source } => Some(match source {
                    Some(r) => json!({ "id": id, "mention": { "start": r.start, "end": r.end, "text": r.value } }),
                    None => json!({ "id": id }),
                }),
                _ => None,
            })
            .collect();
        let mut body = match &command {
            None => json!({ "id": prompt.message_id, "text": prompt.text, "delivery": delivery.as_str() }),
            Some(c) => json!({ "name": c.name, "text": c.arguments, "delivery": delivery.as_str() }),
        };
        if !files.is_empty() {
            body["files"] = json!(files);
        }
        if !agents.is_empty() {
            body["agents"] = json!(agents);
        }
        if !skills.is_empty() {
            body["skills"] = json!(skills);
        }
        match command {
            None => {
                let item = self.client.post_value(&format!("/api/session/{}/prompt", self.session_id), body).await.map_err(e)?;
                if let Some(data) = item.get("data").filter(|d| d.is_object()) {
                    let _ = self.tx.send(Msg::Admitted(data.clone()));
                }
            }
            Some(_) => {
                self.client.post_value(&format!("/api/session/{}/command", self.session_id), body).await.map_err(e)?;
            }
        }
        Ok(())
    }

    /// `settleSession`: wait for the session to settle, then list projected messages (oldest first).
    async fn settle(&self) -> Result<Vec<Value>, String> {
        loop {
            match self.client.wait(&self.session_id).await {
                Ok(()) => break,
                Err(e) => {
                    if e.downcast_ref::<crate::api::ApiError>().is_some() {
                        return Err(err_text(&e));
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
        let page = self.client.messages(&self.session_id, 200, None).await.map_err(|e| err_text(&e))?;
        let mut m = page.data;
        m.reverse();
        Ok(m)
    }
}

impl Runtime {
    /// The exit sequence (`close()` in the lifecycle).
    fn close(&mut self) {
        self.footer.finish_startup();
        let mut sink = Sink2 { split: &mut self.split };
        self.footer.idle(&mut sink);
        let now = Instant::now();
        let (buf, cursor) = self.footer.render(now);
        self.split.render(&buf, cursor);
        let show = self.shown && !self.session_id.is_empty();
        if show && self.footer.settings.splash {
            let title = match &self.session_title {
                Some(t) if !crate::util::locale::is_default_title(t) => Some(t.clone()),
                t => self.history.iter().find(|h| !h.text.trim().is_empty()).map(|h| h.text.clone()).or(t.clone()),
            };
            let c = splash::exit_splash(self.split.width(), title.as_deref(), &self.session_id, self.footer.settings.mono, true, &self.footer.theme.splash);
            self.split.enqueue(c);
            let (buf, _) = self.footer.render(now);
            self.split.render(&buf, None);
            self.wrote_exit = true;
        }
        self.footer.destroy();
        if self.title_set {
            let _ = write!(std::io::stdout(), "\x1b]0;\x07");
        }
        self.split.leave();
        self.write_out();
    }
}

/// Run `ocd mini` until exit. Returns the process exit code.
pub async fn run_mini(opts: TuiOptions, mini: MiniOptions) -> anyhow::Result<i32> {
    if !std::io::stdout().is_terminal() {
        return Ok(fail("ocd mini requires a TTY stdout"));
    }
    if mini.replay_limit.is_some_and(|n| n <= 0) {
        return Ok(fail("--replay-limit must be a positive integer"));
    }
    if opts.fork && opts.session.is_none() && !opts.continue_last {
        return Ok(fail("--fork requires --continue or --session"));
    }
    let mut initial_input = opts.prompt.clone().filter(|p| !p.is_empty());
    if !std::io::stdin().is_terminal() {
        let mut piped = String::new();
        let _ = std::io::stdin().read_to_string(&mut piped);
        initial_input = merge_input(Some(&piped), opts.prompt.as_deref());
        #[cfg(unix)]
        if std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty").is_err() {
            return Ok(fail("ocd mini requires a controlling terminal for input"));
        }
    }
    let config = crate::config::Config::load();
    let settings = resolve_mini_settings(config.get("mini"));
    let directory = opts.directory.clone();
    let client = Client::new(&opts.url, opts.password.as_deref(), &directory);

    // Terminal: raw mode, the startup cursor seed, the palette, paste and keyboard flags.
    crate::restore::arm(crate::restore::PASTE);
    enable_raw_mode()?;
    let cursor_row = crossterm::cursor::position().map(|(_, r)| r as u32 + 1).unwrap_or(0);
    let colors = if settings.mono { None } else { probe_palette(1000) };
    let kitty = !settings.mono && crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    let mut out = std::io::stdout();
    out.write_all(b"\x1b[?2004h")?;
    if kitty {
        crate::restore::arm(crate::restore::PASTE | crate::restore::KITTY);
        execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS))?;
    }
    out.flush()?;
    let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
    let (theme, _, _) = resolve_theme(&config, &directory, colors.as_ref(), settings.mono);
    let mut split = Split::new(w as u32, h as u32, FOOTER_HEIGHT as u32, cursor_row);
    let animations = config.animations();
    let label = {
        let home = super::util::home_dir();
        if directory == home { "~".to_string() } else if !home.is_empty() && directory.starts_with(&format!("{home}/")) { format!("~{}", &directory[home.len()..]) } else { directory.clone() }
    };
    let startup = (settings.splash && !settings.mono && animations).then(|| (opts.version.clone(), label.clone()));
    if settings.splash && startup.is_none() {
        split.enqueue(splash::entry_splash(w as usize, &opts.version, Some(&label), settings.mono, &theme.splash));
    }
    let model = opts.model.as_deref().and_then(|m| {
        let base = m.split('#').next().unwrap_or(m);
        base.split_once('/').map(|(p, id)| ModelSel { provider_id: p.into(), model_id: id.into() })
    });
    let variant = opts.model.as_deref().and_then(|m| m.split_once('#').map(|x| x.1.to_string()));
    let keybinds = Keybinds::new(config.get("keybinds"), config.leader_timeout(), !cfg!(windows));
    let now = Instant::now();
    let footer = Footer::new(
        FooterOptions {
            agents: vec![],
            references: vec![],
            agent: opts.agent.clone(),
            model_label: match &model {
                Some(m) => format_model_label(m, variant.as_deref(), None),
                None => "Default model".into(),
            },
            model: model.clone(),
            variant: variant.clone(),
            first: true,
            history: vec![],
            theme,
            settings: settings.clone(),
            keybinds,
            animations,
            cursor_style: cursor_style(&config),
            wrote: true,
            startup,
            directory: directory.clone(),
            image_preview: config.bool("session.image_preview").unwrap_or(false),
        },
        w as usize,
        h as usize,
        FOOTER_HEIGHT,
        now,
    );
    let (tx, mut rx) = unbounded_channel::<Msg>();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut rt = Runtime {
        trace: Trace::open(&argv, &directory),
        client,
        tx,
        replay: mini.replay.unwrap_or(true),
        replay_limit: mini.replay_limit.unwrap_or(200).clamp(1, u32::MAX as i64) as u32,
        directory: directory.clone(),
        split,
        footer,
        settings,
        colors,
        session_id: String::new(),
        session_title: None,
        location: directory,
        agent: opts.agent.clone(),
        model,
        default_model: None,
        variant,
        session_variant: None,
        providers: vec![],
        variants: vec![],
        shown: false,
        history: vec![],
        local_rows: vec![],
        resolving: false,
        transport: None,
        generation: 0,
        booting: false,
        hydrating: false,
        buffered: vec![],
        connect_task: None,
        replaying: false,
        replay_buffer: vec![],
        queue: VecDeque::new(),
        active: None,
        token: 0,
        admission_version: 0,
        admissions_inflight: 0,
        settling: false,
        initial_input,
        queue_started: false,
        resize_at: None,
        title_set: false,
        wrote_exit: false,
        done: false,
        kitty,
        pending_editor: None,
        fatal: None,
        opts,
        config,
    };
    rt.footer.apply_height();
    rt.set_title(None);
    rt.frame();
    rt.start_session();
    let mut events = EventStream::new();
    loop {
        rt.frame();
        if rt.done {
            break;
        }
        if let Some(value) = rt.pending_editor.take() {
            rt.open_editor(value);
            continue;
        }
        if !rt.queue_started && !rt.session_id.is_empty() {
            rt.queue_started = true;
            let input = rt.initial_input.take().unwrap_or_default();
            rt.submit(RunPrompt::text(input));
        }
        if let Some(a) = rt.active.as_ref()
            && a.prompt.shell
            && a.done
            && a.shell_deadline.is_some_and(|d| Instant::now() >= d)
        {
            if let Some(t) = rt.transport.as_mut() {
                t.shell_wait = None;
            }
            rt.finish_turn();
            continue;
        }
        let wake = [rt.footer.next_wake(), rt.resize_at, rt.active.as_ref().and_then(|a| a.shell_deadline)].into_iter().flatten().min();
        let sleep = async {
            match wake {
                Some(t) => tokio::time::sleep_until(tokio::time::Instant::from_std(t)).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(ev)) => rt.on_terminal(ev),
                Some(Err(_)) | None => {
                    rt.footer.close();
                }
            },
            m = rx.recv() => {
                if let Some(m) = m {
                    rt.on_msg(m);
                    while let Ok(m) = rx.try_recv() {
                        rt.on_msg(m);
                    }
                }
            },
            _ = sleep => {
                if rt.resize_at.is_some_and(|t| Instant::now() >= t) {
                    rt.resize_at = None;
                    rt.resize_replay();
                }
            }
        }
    }
    if let Some(h) = rt.connect_task.take() {
        h.abort();
    }
    rt.close();
    let mut out = std::io::stdout();
    if kitty {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = out.write_all(b"\x1b[?2004l");
    if !rt.wrote_exit {
        let _ = out.write_all(b"\n");
    }
    let _ = out.flush();
    let _ = disable_raw_mode();
    crate::restore::disarm();
    if let Some(e) = rt.fatal.take() {
        eprintln!("\x1b[91m\x1b[1mError: \x1b[0m{e}");
        return Ok(1);
    }
    Ok(0)
}
