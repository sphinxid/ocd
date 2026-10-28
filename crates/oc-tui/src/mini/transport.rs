//! The session transport (`src/mini/stream-v2.transport.ts`, `stream-v2.fragment.ts`,
//! `stream-v2.image.ts`, `session-data.ts`, `stream.ts`): reduces V2 events and projected messages
//! into scrollback commits and footer updates. This is the synchronous core; the runtime performs the
//! HTTP work (hydration, settling, admission) and feeds results in.

use super::subagents::SubagentTracker;
use super::tool::{normalize_tool, tool_display_content, tool_output_text, tool_view};
use super::types::*;
use crate::api::types::{Event, FormInfo, PermissionRequest};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Output of the transport to the footer and runtime.
#[derive(Clone, Debug)]
pub enum Out {
    Commit(Box<StreamCommit>),
    Patch(FooterPatch),
    View(FooterView),
    Subagent(SubagentState),
    Queued(Vec<QueuedPrompt>),
    Title(String),
    CatalogRefresh,
    /// Ask the runtime to run an async subagent job.
    Job(super::subagents::Job),
}

// --- fragments ------------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct Fragment {
    text: String,
    projected: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FragmentUpdate {
    pub key: String,
    pub part_id: String,
    pub previous: String,
    pub text: String,
}

pub enum Restore {
    Append(String),
    Covered,
    Conflict,
}

/// `createFragmentReconciler()`.
#[derive(Clone, Debug, Default)]
pub struct Fragments {
    map: HashMap<String, Fragment>,
}

pub fn fragment_ref(message_id: &str, kind: &str, ordinal: i64) -> (String, String) {
    (message_id.to_string(), format!("{kind}:{ordinal}"))
}

fn key(message_id: &str, part_id: &str) -> String {
    format!("{message_id}\u{0}{part_id}")
}

impl Fragments {
    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn value(&self, message_id: &str, part_id: &str) -> Option<&str> {
        self.map.get(&key(message_id, part_id)).map(|f| f.text.as_str())
    }

    pub fn project(&mut self, message_id: &str, part_id: &str, text: &str, visible: bool) -> FragmentUpdate {
        let id = key(message_id, part_id);
        let current = self.map.get(&id).cloned();
        self.map.insert(id.clone(), Fragment { text: text.into(), projected: if visible { Some(text.into()) } else { current.as_ref().and_then(|c| c.projected.clone()) } });
        FragmentUpdate { key: id, part_id: part_id.into(), previous: current.map(|c| c.text).unwrap_or_default(), text: text.into() }
    }

    pub fn delta(&mut self, message_id: &str, part_id: &str, delta: &str) -> Option<FragmentUpdate> {
        let id = key(message_id, part_id);
        if let Some(cur) = self.map.get_mut(&id)
            && let Some(projected) = cur.projected.as_mut()
            && !projected.is_empty()
            && let Some(i) = projected.find(delta)
        {
            *projected = projected[i + delta.len()..].to_string();
            return None;
        }
        let current = self.map.get(&id).cloned();
        let previous = current.as_ref().map(|c| c.text.clone()).unwrap_or_default();
        let text = format!("{previous}{delta}");
        self.map.insert(id.clone(), Fragment { text: text.clone(), projected: current.and_then(|c| c.projected) });
        Some(FragmentUpdate { key: id, part_id: part_id.into(), previous, text })
    }

    pub fn end(&mut self, message_id: &str, part_id: &str, text: &str) -> FragmentUpdate {
        let id = key(message_id, part_id);
        let previous = self.map.get(&id).map(|c| c.text.clone()).unwrap_or_default();
        self.map.insert(id.clone(), Fragment { text: text.into(), projected: None });
        FragmentUpdate { key: id, part_id: part_id.into(), previous, text: text.into() }
    }

    pub fn restore(&mut self, message_id: &str, part_id: &str, text: &str) -> Restore {
        let id = key(message_id, part_id);
        match self.map.get(&id).cloned() {
            None => {
                self.map.insert(id, Fragment { text: text.into(), projected: Some(text.into()) });
                Restore::Append(text.into())
            }
            Some(cur) if text.starts_with(&cur.text) => {
                let suffix = text[cur.text.len()..].to_string();
                self.map.insert(id, Fragment { text: text.into(), projected: Some(text.into()) });
                Restore::Append(suffix)
            }
            Some(cur) if cur.text.starts_with(text) => Restore::Covered,
            _ => Restore::Conflict,
        }
    }
}

// --- helpers --------------------------------------------------------------------------------

/// `toolCommit(part, messageID, phase, value, directory, version)`.
pub fn tool_commit(input: &ToolPart, message_id: &str, phase: Phase, value: Option<&str>, directory: Option<&str>, version: u32) -> StreamCommit {
    let part = normalize_tool(input);
    let status = part.status;
    let output = if status == ToolStatus::Streaming { String::new() } else { tool_output_text(&part.name, tool_display_content(&part)) };
    let partial = status == ToolStatus::Error && phase == Phase::Progress && value.is_some();
    let text = if status == ToolStatus::Running || partial {
        value.map(str::to_string).unwrap_or_else(|| if part.name == "subagent" { "running subagent".into() } else { format!("running {}", part.name) })
    } else if status == ToolStatus::Completed {
        value.map(str::to_string).unwrap_or(output)
    } else if status == ToolStatus::Error {
        part.error_message()
    } else {
        String::new()
    };
    StreamCommit {
        kind: EntryKind::Tool,
        source: Source::Tool,
        text,
        phase,
        message_id: Some(message_id.into()),
        part_id: Some(format!("prt_{}{}", part.id, if version > 0 { format!(":snapshot:{version}") } else { String::new() })),
        tool: Some(part.name.clone()),
        directory: directory.map(str::to_string),
        tool_state: Some(if status == ToolStatus::Error && !partial {
            ToolStateKind::Error
        } else if status == ToolStatus::Completed {
            ToolStateKind::Completed
        } else {
            ToolStateKind::Running
        }),
        tool_error: (status == ToolStatus::Error && !partial).then(|| part.error_message()),
        part: Some(part),
        ..Default::default()
    }
}

/// `toolFinalPhase(part)`.
pub fn tool_final_phase(part: &ToolPart) -> Phase {
    let tool = normalize_tool(part);
    if tool.status != ToolStatus::Completed {
        return Phase::Final;
    }
    if tool_view(Some(&tool.name)).output && !tool_output_text(&tool.name, &tool.content).is_empty() { Phase::Progress } else { Phase::Final }
}

fn data_str<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or_default()
}

/// `pickBlockerView`.
pub fn pick_blocker_view(permission: Option<&MiniPermission>, form: Option<&MiniForm>) -> FooterView {
    if let Some(p) = permission {
        return FooterView::Permission(Box::new(p.clone()));
    }
    if let Some(f) = form {
        return FooterView::Form(Box::new(f.clone()));
    }
    FooterView::Prompt
}

/// `blockerStatus`.
pub fn blocker_status(view: &FooterView) -> &'static str {
    match view {
        FooterView::Permission(_) => "awaiting permission",
        FooterView::Form(_) => "awaiting form",
        FooterView::Prompt => "",
    }
}

fn error_message(error: &Value) -> String {
    let m = data_str(error, "message");
    if !m.is_empty() {
        return m.into();
    }
    let t = data_str(error, "_tag");
    if !t.is_empty() {
        return t.into();
    }
    "Session execution failed".into()
}

/// `userImageCommits`: inline image files as caption entries.
pub fn user_image_commits(message_id: &str, files: &[Value]) -> Vec<StreamCommit> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for f in files {
        let mime = data_str(f, "mime");
        if !mime.starts_with("image/") {
            continue;
        }
        let data = data_str(f, "data");
        if seen.iter().any(|s| s == data) {
            continue;
        }
        seen.push(data.to_string());
        let index = out.len();
        let text = f.get("name").and_then(|n| n.as_str()).map(str::to_string).or_else(|| f.get("mention").and_then(|m| m.get("text")).and_then(|t| t.as_str()).map(str::to_string)).unwrap_or(format!("[Image {}]", index + 1));
        out.push(StreamCommit { kind: EntryKind::User, source: Source::System, text, image: Some(format!("data:{mime};base64,{data}")), phase: Phase::Final, message_id: Some(message_id.into()), part_id: Some(format!("image:{index}")), ..Default::default() });
    }
    out
}

/// `toolImageCommits`.
pub fn tool_image_commits(part: &ToolPart, message_id: &str) -> Vec<StreamCommit> {
    tool_display_content(part)
        .iter()
        .filter(|c| data_str(c, "type") == "file" && data_str(c, "mime").starts_with("image/") && data_str(c, "uri").starts_with("data:image/"))
        .enumerate()
        .map(|(i, c)| StreamCommit {
            kind: EntryKind::Tool,
            source: Source::Tool,
            text: c.get("name").and_then(|n| n.as_str()).map(str::to_string).unwrap_or(format!("[Image {}]", i + 1)),
            image: Some(data_str(c, "uri").into()),
            phase: Phase::Final,
            message_id: Some(message_id.into()),
            part_id: Some(format!("prt_{}:image:{i}", part.id)),
            ..Default::default()
        })
        .collect()
}

fn shell_commit(id: &str, command: &str, text: &str, phase: Phase, state: ToolStateKind, error: Option<String>) -> StreamCommit {
    StreamCommit { kind: EntryKind::Tool, source: Source::Tool, part_id: Some(format!("shell:{id}")), tool: Some("shell".into()), shell: Some(command.into()), text: text.into(), phase, tool_state: Some(state), tool_error: error, ..Default::default() }
}

/// `shellTerminal(id, command, shell, output)`.
fn shell_terminal(id: &str, command: &str, shell: &Value, output: &Value) -> Vec<StreamCommit> {
    let out = data_str(output, "output");
    let cursor = output.get("cursor").and_then(|c| c.as_i64()).unwrap_or(0);
    let size = output.get("size").and_then(|c| c.as_i64()).unwrap_or(0);
    let truncated = output.get("truncated").and_then(|c| c.as_bool()).unwrap_or(false);
    let incomplete = truncated || cursor < size;
    let text = format!("{out}{}", if incomplete { format!("{}[output truncated]", if out.ends_with('\n') || out.is_empty() { "" } else { "\n" }) } else { String::new() });
    let status = data_str(shell, "status");
    let exit = shell.get("exit");
    let exit_zero = exit.and_then(|e| e.as_i64()) == Some(0);
    let error = if status == "exited" && exit_zero {
        None
    } else if status == "exited" {
        Some(format!("Shell exited with code {}", exit.map(super::util::js_string).filter(|s| !s.is_empty() && s != "null").unwrap_or("unknown".into())))
    } else {
        Some(format!("Shell {status}"))
    };
    match error {
        None => vec![shell_commit(id, command, &text, Phase::Progress, ToolStateKind::Completed, None)],
        Some(e) => {
            let mut v = Vec::new();
            if !text.is_empty() {
                v.push(shell_commit(id, command, &text, Phase::Progress, ToolStateKind::Running, None));
            }
            v.push(shell_commit(id, command, &e, Phase::Final, ToolStateKind::Error, Some(e.clone())));
            v
        }
    }
}

fn skill_commit(message_id: &str, name: &str, skill_id: &str) -> StreamCommit {
    StreamCommit { kind: EntryKind::System, source: Source::System, message_id: Some(message_id.into()), part_id: Some(format!("skill:{skill_id}")), text: format!("→ Skill \"{name}\""), phase: Phase::Start, ..Default::default() }
}

fn compaction_commit(message_id: &str) -> StreamCommit {
    StreamCommit { kind: EntryKind::System, source: Source::System, message_id: Some(message_id.into()), part_id: Some("compaction:header".into()), text: "Compaction".into(), phase: Phase::Start, compaction: true, ..Default::default() }
}

fn compaction_summary(message_id: &str, text: &str, phase: Phase) -> StreamCommit {
    StreamCommit { kind: EntryKind::Assistant, source: Source::Assistant, message_id: Some(message_id.into()), part_id: Some("compaction:summary".into()), text: text.into(), phase, ..Default::default() }
}

fn compaction_error(message_id: &str, text: &str) -> StreamCommit {
    StreamCommit { kind: EntryKind::Error, source: Source::System, message_id: Some(message_id.into()), part_id: Some("compaction:error".into()), text: text.into(), phase: Phase::Start, ..Default::default() }
}

/// `messageIDFromEvent(id)`: `evt_…` → `msg_…`.
pub fn message_id_from_event(id: &str) -> String {
    match id.strip_prefix("evt_") {
        Some(rest) => format!("msg_{rest}"),
        None => id.to_string(),
    }
}

/// `eventIDFromMessage(id)`.
pub fn event_id_from_message(id: &str) -> String {
    match id.strip_prefix("msg_") {
        Some(rest) => format!("evt_{rest}"),
        None => id.to_string(),
    }
}

pub const CATALOG_EVENTS: [&str; 8] = ["provider.updated", "model.updated", "integration.updated", "credential.switched", "agent.updated", "command.updated", "skill.updated", "reference.updated"];

/// `pendingPrompt(item)` from an inbox item (`{id, type, payload, delivery}`).
pub fn pending_prompt(item: &Value) -> Option<(QueuedPrompt, Vec<Value>)> {
    if data_str(item, "type") != "user" {
        return None;
    }
    let id = data_str(item, "id").to_string();
    let payload = item.get("payload").cloned().unwrap_or(Value::Null);
    let mut prompt = super::session_shared::message_prompt(&payload);
    prompt.message_id = Some(id.clone());
    let skills = payload.get("skills").and_then(|s| s.as_array()).map(|a| a.iter().map(|s| (data_str(s, "id").to_string(), data_str(s, "name").to_string())).collect()).unwrap_or_default();
    let files = payload.get("files").and_then(|f| f.as_array()).cloned().unwrap_or_default();
    Some((QueuedPrompt { message_id: id, prompt, delivery: Delivery::parse(data_str(item, "delivery")), skills }, files))
}

#[derive(Clone, Debug, Default)]
struct ToolTrack {
    part: ToolPart,
    output: String,
    version: u32,
    started: bool,
}

/// The process-local wait for a prompt turn (`Wait`).
#[derive(Clone, Debug, Default)]
pub struct Wait {
    pub message_id: String,
    pub failure_message_id: String,
    pub promoted: bool,
    pub promotion_observed: bool,
    pub interrupted: bool,
    pub failure_rendered: bool,
    pub terminal_error: Option<String>,
}

/// An active direct shell call (`ShellWait`).
#[derive(Clone, Debug, Default)]
pub struct ShellWait {
    pub event_id: String,
    pub message_id: String,
    pub id: Option<String>,
    pub resolved: bool,
}

pub struct Transport {
    pub session_id: String,
    pub directory: Option<String>,
    pub thinking: bool,
    pub tools: bool,
    pub replay: bool,
    pub permissions: Vec<MiniPermission>,
    pub forms: Vec<MiniForm>,
    pub global_forms: Vec<MiniForm>,
    pub view: FooterView,
    message_ids: HashSet<String>,
    promoted: HashSet<String>,
    image_ids: HashSet<String>,
    pub fragments: Fragments,
    tracks: HashMap<String, ToolTrack>,
    tool_sources: HashMap<String, ToolPart>,
    finished_tools: HashSet<String>,
    tool_messages: HashSet<String>,
    quiet_text: indexmap::IndexMap<String, Vec<(String, String)>>,
    skill_messages: HashSet<String>,
    shell_commands: HashMap<String, String>,
    shell_started: HashSet<String>,
    shell_ended: HashSet<String>,
    pub shell_wait: Option<ShellWait>,
    pub wait: Option<Wait>,
    pub connected: bool,
    pub closed: bool,
    pub initial: bool,
    pub root_active: bool,
    pub execution_epoch: u64,
    errors: HashSet<String>,
    pending: indexmap::IndexMap<String, (QueuedPrompt, Vec<Value>)>,
    admitted: HashSet<String>,
    step_model: Option<ModelSel>,
    active_compaction: Option<String>,
    synced_pending: Option<Vec<String>>,
    /// Commits are not remembered for replay while hydrating (initial) or replaying.
    pub buffering: bool,
    pub subagents: SubagentTracker,
    pub out: Vec<Out>,
    /// Local rows for resize replay (`onCommit` → `rememberLocal`).
    pub remembered: Vec<StreamCommit>,
    /// Context limits by `provider\0model` (`contextLimit(model)`).
    pub context_limits: HashMap<String, f64>,
}

impl Transport {
    pub fn new(session_id: &str, directory: Option<String>, thinking: bool, tools: bool, replay: bool) -> Transport {
        Transport {
            session_id: session_id.into(),
            subagents: SubagentTracker::new(session_id, thinking, directory.clone()),
            directory,
            thinking,
            tools,
            replay,
            permissions: Vec::new(),
            forms: Vec::new(),
            global_forms: Vec::new(),
            view: FooterView::Prompt,
            message_ids: HashSet::new(),
            promoted: HashSet::new(),
            image_ids: HashSet::new(),
            fragments: Fragments::default(),
            tracks: HashMap::new(),
            tool_sources: HashMap::new(),
            finished_tools: HashSet::new(),
            tool_messages: HashSet::new(),
            quiet_text: indexmap::IndexMap::new(),
            skill_messages: HashSet::new(),
            shell_commands: HashMap::new(),
            shell_started: HashSet::new(),
            shell_ended: HashSet::new(),
            shell_wait: None,
            wait: None,
            connected: false,
            closed: false,
            initial: true,
            root_active: false,
            execution_epoch: 0,
            errors: HashSet::new(),
            pending: indexmap::IndexMap::new(),
            admitted: HashSet::new(),
            step_model: None,
            active_compaction: None,
            synced_pending: None,
            buffering: false,
            out: Vec::new(),
            remembered: Vec::new(),
            context_limits: HashMap::new(),
        }
    }

    fn show_tools(&self) -> bool {
        self.tools
    }

    /// `write(commits, patch)`.
    fn write(&mut self, commits: Vec<StreamCommit>, patch: Option<FooterPatch>) {
        if self.closed {
            return;
        }
        if !self.initial && !self.buffering {
            for c in &commits {
                let remembered = match (&c.message_id, &c.part_id) {
                    (Some(m), Some(p)) if matches!(c.kind, EntryKind::Assistant | EntryKind::Reasoning) => {
                        let text = self.fragments.value(m, p).map(str::to_string);
                        let text = match (c.kind, text) {
                            (EntryKind::Reasoning, Some(t)) if !t.is_empty() => format!("Thinking: {t}"),
                            (_, Some(t)) => t,
                            (_, None) => c.text.clone(),
                        };
                        StreamCommit { text, ..c.clone() }
                    }
                    _ => c.clone(),
                };
                self.remembered.push(remembered);
            }
        }
        for c in commits {
            self.out.push(Out::Commit(Box::new(c)));
        }
        if let Some(mut p) = patch {
            if p.status.is_some() && p.phase.is_none() {
                p.phase = Some(FooterPhase::Running);
            }
            self.out.push(Out::Patch(p));
        }
    }

    fn patch(&mut self, phase: Option<FooterPhase>, status: Option<&str>) {
        self.write(vec![], Some(FooterPatch { phase, status: status.map(str::to_string), ..Default::default() }));
    }

    /// `paintIdle(status)`.
    pub fn paint_idle(&mut self, status: &str) {
        self.root_active = false;
        self.patch(Some(FooterPhase::Idle), Some(status));
    }

    fn sync_pending(&mut self) {
        let ids: Vec<String> = self.pending.values().map(|(q, _)| format!("{}:{}", q.message_id, q.delivery.as_str())).collect();
        if self.synced_pending.as_ref() == Some(&ids) {
            return;
        }
        self.synced_pending = Some(ids);
        self.out.push(Out::Queued(self.pending.values().map(|(q, _)| q.clone()).collect()));
    }

    fn fresh_images(&mut self, commits: Vec<StreamCommit>, render: bool) -> Vec<StreamCommit> {
        commits
            .into_iter()
            .filter(|c| {
                let k = key(c.message_id.as_deref().unwrap_or(""), c.part_id.as_deref().unwrap_or(""));
                if self.image_ids.contains(&k) {
                    return false;
                }
                self.image_ids.insert(k);
                render
            })
            .collect()
    }

    fn render_user(&mut self, message_id: &str, text: &str, files: &[Value], skills: &[(String, String)], render: bool) {
        let visible = self.message_ids.contains(message_id);
        self.message_ids.insert(message_id.into());
        let images = self.fresh_images(user_image_commits(message_id, files), render);
        if !render {
            return;
        }
        let mut commits = Vec::new();
        if !visible && self.show_tools() {
            let mut seen = Vec::new();
            for (id, name) in skills {
                if seen.contains(id) {
                    continue;
                }
                seen.push(id.clone());
                commits.push(skill_commit(message_id, name, id));
            }
        }
        if !visible && !text.trim().is_empty() {
            commits.push(StreamCommit { kind: EntryKind::User, source: Source::System, text: text.into(), phase: Phase::Start, message_id: Some(message_id.into()), ..Default::default() });
        }
        commits.extend(images);
        self.write(commits, None);
    }

    /// `mergePending(item)`.
    pub fn merge_pending(&mut self, item: &Value) {
        let Some((prompt, files)) = pending_prompt(item) else { return };
        if self.promoted.contains(&prompt.message_id) {
            let (id, text, skills) = (prompt.message_id.clone(), prompt.prompt.text.clone(), prompt.skills.clone());
            self.render_user(&id, &text, &files, &skills, true);
            return;
        }
        self.admitted.insert(prompt.message_id.clone());
        self.pending.insert(prompt.message_id.clone(), (prompt, files));
        self.sync_pending();
    }

    fn promote_wait(&mut self, observed: bool, message_id: Option<&str>) {
        let Some(w) = self.wait.as_mut() else { return };
        let message_id = message_id.map(str::to_string).unwrap_or(w.message_id.clone());
        let transition = message_id != w.failure_message_id || if observed { !w.promotion_observed } else { !w.promoted };
        w.promoted = true;
        if observed {
            w.promotion_observed = true;
        }
        if !transition {
            return;
        }
        w.failure_message_id = message_id;
        w.failure_rendered = false;
        w.terminal_error = None;
    }

    /// `syncBlockers()`.
    pub fn sync_blockers(&mut self) {
        if self.closed {
            return;
        }
        let descendant = self.subagents.snapshot();
        let permission = self.permissions.first().or(descendant.permissions.first()).cloned();
        let form = self.forms.first().or(descendant.forms.first()).or(self.global_forms.first()).cloned();
        let next = pick_blocker_view(permission.as_ref(), form.as_ref());
        if matches!(next, FooterView::Prompt) && matches!(self.view, FooterView::Prompt) {
            return;
        }
        if !matches!(next, FooterView::Prompt) && next.kind() == self.view.kind() && next.request_id() == self.view.request_id() {
            return;
        }
        self.view = next.clone();
        let patch = if matches!(next, FooterView::Prompt) {
            FooterPatch { phase: Some(if self.root_active { FooterPhase::Running } else { FooterPhase::Idle }), status: Some(blocker_status(&next).into()), ..Default::default() }
        } else {
            FooterPatch { phase: Some(FooterPhase::Running), status: Some(blocker_status(&next).into()), ..Default::default() }
        };
        self.out.push(Out::Patch(patch));
        self.out.push(Out::View(next));
    }

    fn source_pending(&self, k: &str) -> bool {
        self.permissions.iter().any(|r| permission_source_key(&r.request).as_deref() == Some(k))
    }

    fn prune_tool_sources(&mut self) {
        let keys: Vec<String> = self.tool_sources.keys().cloned().collect();
        for k in keys {
            if !self.tracks.contains_key(&k) && !self.source_pending(&k) {
                self.tool_sources.remove(&k);
            }
        }
    }

    fn remember_tool_message(&mut self, message_id: &str) {
        self.tool_messages.insert(message_id.into());
        self.quiet_text.shift_remove(message_id);
    }

    fn buffer_quiet_text(&mut self, message_id: &str, part_id: &str, text: &str, replace: bool) {
        if text.is_empty() || self.tool_messages.contains(message_id) {
            return;
        }
        let parts = self.quiet_text.entry(message_id.into()).or_default();
        if let Some(cur) = parts.iter_mut().find(|p| p.0 == part_id) {
            cur.1 = if replace { text.into() } else { format!("{}{text}", cur.1) };
            return;
        }
        parts.push((part_id.into(), text.into()));
    }

    fn flush_quiet_text(&mut self, message_id: &str) {
        let parts = self.quiet_text.shift_remove(message_id);
        let Some(parts) = parts else { return };
        if self.tool_messages.contains(message_id) {
            return;
        }
        let commits = parts
            .into_iter()
            .filter(|p| !p.1.is_empty())
            .map(|(part, text)| StreamCommit { kind: EntryKind::Assistant, source: Source::Assistant, text, phase: Phase::Progress, message_id: Some(message_id.into()), part_id: Some(part), ..Default::default() })
            .collect();
        self.write(commits, None);
    }

    /// `renderTool(messageID, item, render)`.
    pub fn render_tool(&mut self, message_id: &str, item: &ToolPart, render: bool) {
        let mut render = render;
        if !self.show_tools() {
            self.remember_tool_message(message_id);
            render = false;
        }
        let part = normalize_tool(item);
        let k = key(message_id, &part.id);
        if self.finished_tools.contains(&k) {
            if self.source_pending(&k) {
                self.tool_sources.insert(k, part);
            } else {
                self.tool_sources.remove(&k);
            }
            return;
        }
        self.tool_sources.insert(k.clone(), part.clone());
        if part.status == ToolStatus::Streaming {
            self.tracks.insert(k, ToolTrack { part, output: String::new(), version: 0, started: false });
            return;
        }
        let current = self.tracks.get(&k).cloned();
        let output = tool_output_text(&part.name, tool_display_content(&part));
        let prefix = current.as_ref().is_some_and(|c| output.starts_with(&c.output));
        let version = match &current {
            Some(c) if !prefix => c.version + 1,
            Some(c) => c.version,
            None => 0,
        };
        let delta = match &current {
            Some(c) if prefix => output[c.output.len()..].to_string(),
            _ => output.clone(),
        };
        let dir = self.directory.clone();
        if part.status == ToolStatus::Running {
            let started = current.as_ref().is_some_and(|c| c.started);
            let ready = part.name != "websearch" || part.metadata.get("provider").is_some_and(|p| p.is_string());
            if render && !started && ready {
                let name = part.name.clone();
                self.write(vec![tool_commit(&part, message_id, Phase::Start, None, dir.as_deref(), version)], Some(FooterPatch { phase: Some(FooterPhase::Running), status: Some(format!("running {name}")), ..Default::default() }));
            }
            if render && !delta.is_empty() {
                self.write(vec![tool_commit(&part, message_id, Phase::Progress, Some(&delta), dir.as_deref(), version)], None);
            }
            self.tracks.insert(k, ToolTrack { part, output, version, started: started || (render && ready) });
            return;
        }
        if render && !current.as_ref().is_some_and(|c| c.started) {
            self.write(vec![tool_commit(&part, message_id, Phase::Start, None, dir.as_deref(), version)], None);
        }
        self.finished_tools.insert(k.clone());
        self.tracks.remove(&k);
        if !self.source_pending(&k) {
            self.tool_sources.remove(&k);
        }
        let images = self.fresh_images(tool_image_commits(&part, message_id), render);
        if !render {
            return;
        }
        let phase = tool_final_phase(&part);
        if part.status == ToolStatus::Error && !delta.is_empty() {
            self.write(vec![tool_commit(&part, message_id, Phase::Progress, Some(&delta), dir.as_deref(), version)], None);
        }
        let mut commits = vec![tool_commit(&part, message_id, phase, if phase == Phase::Progress { Some(&delta) } else { None }, dir.as_deref(), version)];
        commits.extend(images);
        self.write(commits, None);
    }

    /// `renderMessage(message, render)`.
    pub fn render_message(&mut self, message: &Value, render: bool) {
        let id = data_str(message, "id").to_string();
        match data_str(message, "type") {
            "user" => {
                let waiting = self.wait.as_ref().is_some_and(|w| w.message_id == id);
                let admitted = self.admitted.remove(&id);
                if self.wait.is_some() && (admitted || (waiting && self.wait.as_ref().is_some_and(|w| w.failure_message_id == id))) {
                    self.promote_wait(false, Some(&id));
                }
                if self.pending.shift_remove(&id).is_some() {
                    self.sync_pending();
                }
                self.promoted.insert(id.clone());
                let files = message.get("files").and_then(|f| f.as_array()).cloned().unwrap_or_default();
                let skills: Vec<(String, String)> = message.get("skills").and_then(|s| s.as_array()).map(|a| a.iter().map(|s| (data_str(s, "id").to_string(), data_str(s, "name").to_string())).collect()).unwrap_or_default();
                self.render_user(&id, data_str(message, "text"), &files, &skills, render);
            }
            "skill" => {
                if self.wait.as_ref().is_some_and(|w| w.message_id == id) {
                    self.promote_wait(false, None);
                }
                if !render || !self.show_tools() || self.skill_messages.contains(&id) {
                    self.skill_messages.insert(id);
                    return;
                }
                self.skill_messages.insert(id.clone());
                let name = data_str(message, "name").to_string();
                self.write(vec![skill_commit(&id, &name, &id)], None);
            }
            "shell" => {
                let shell_id = data_str(message, "shellID").to_string();
                let command = data_str(message, "command").to_string();
                self.shell_commands.insert(shell_id.clone(), command.clone());
                if let Some(w) = self.shell_wait.as_mut()
                    && w.message_id == id
                {
                    w.id = Some(shell_id.clone());
                }
                let completed = message.get("time").and_then(|t| t.get("completed")).is_some_and(|c| !c.is_null());
                if !render {
                    if completed {
                        self.shell_started.insert(shell_id.clone());
                        self.shell_ended.insert(shell_id);
                    }
                    return;
                }
                if !self.shell_started.contains(&shell_id) {
                    self.shell_started.insert(shell_id.clone());
                    self.write(vec![shell_commit(&shell_id, &command, "running shell", Phase::Start, ToolStateKind::Running, None)], None);
                }
                if completed
                    && let Some(output) = message.get("output").filter(|o| !o.is_null())
                    && !self.shell_ended.contains(&shell_id)
                {
                    self.shell_ended.insert(shell_id.clone());
                    let commits = shell_terminal(&shell_id, &command, message, output);
                    self.write(commits, None);
                }
                if completed
                    && let Some(w) = self.shell_wait.as_mut()
                    && w.id.as_deref() == Some(&shell_id)
                {
                    w.resolved = true;
                }
            }
            "compaction" => {
                let visible = self.message_ids.contains(&id);
                self.message_ids.insert(id.clone());
                let status = data_str(message, "status");
                if status == "running" {
                    self.active_compaction = Some(id.clone());
                }
                if status != "running" && self.active_compaction.as_deref() == Some(&id) {
                    self.active_compaction = None;
                }
                if visible {
                    return;
                }
                if status == "failed" {
                    let err = message.get("error").cloned().unwrap_or(Value::Null);
                    if render && data_str(&err, "type") != "aborted" {
                        self.write(vec![compaction_commit(&id), compaction_error(&id, data_str(&err, "message"))], None);
                    }
                    return;
                }
                let summary = data_str(message, "summary").to_string();
                let show = render || status == "running";
                self.fragments.project(&id, "compaction:summary", &summary, show);
                if !show {
                    return;
                }
                let mut commits = vec![compaction_commit(&id)];
                if !summary.is_empty() {
                    commits.push(compaction_summary(&id, &summary, Phase::Progress));
                }
                if status == "completed" {
                    commits.push(compaction_summary(&id, "", Phase::Final));
                }
                self.write(commits, None);
            }
            "assistant" => {
                self.message_ids.insert(id.clone());
                let content = message.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default();
                let has_tools = content.iter().any(|c| data_str(c, "type") == "tool");
                if has_tools {
                    self.remember_tool_message(&id);
                }
                let (mut text_ord, mut reasoning_ord) = (0, 0);
                for item in &content {
                    match data_str(item, "type") {
                        "text" => {
                            let (m, p) = fragment_ref(&id, "text", text_ord);
                            text_ord += 1;
                            let text = data_str(item, "text").to_string();
                            let visible = render && (self.show_tools() || !has_tools);
                            let update = self.fragments.project(&m, &p, &text, visible);
                            if visible && text.len() > update.previous.len() {
                                let slice = text.get(update.previous.len()..).unwrap_or_default().to_string();
                                self.write(vec![StreamCommit { kind: EntryKind::Assistant, source: Source::Assistant, text: slice, phase: Phase::Progress, message_id: Some(id.clone()), part_id: Some(p), ..Default::default() }], None);
                            }
                        }
                        "reasoning" => {
                            let (m, p) = fragment_ref(&id, "reasoning", reasoning_ord);
                            reasoning_ord += 1;
                            let text = data_str(item, "text").to_string();
                            let update = self.fragments.project(&m, &p, &text, render);
                            if render && self.thinking && text.len() > update.previous.len() {
                                let t = if update.previous.is_empty() { format!("Thinking: {text}") } else { text.get(update.previous.len()..).unwrap_or_default().to_string() };
                                self.write(vec![StreamCommit { kind: EntryKind::Reasoning, source: Source::Reasoning, text: t, phase: Phase::Progress, message_id: Some(id.clone()), part_id: Some(p), ..Default::default() }], None);
                            }
                        }
                        _ => {
                            let part = ToolPart::from_value(item);
                            self.render_tool(&id, &part, render);
                        }
                    }
                }
                if let Some(err) = message.get("error").filter(|e| !e.is_null())
                    && !self.errors.contains(&id)
                {
                    self.errors.insert(id.clone());
                    if !render {
                        return;
                    }
                    if let Some(w) = self.wait.as_mut() {
                        w.failure_rendered = true;
                    }
                    self.write(vec![StreamCommit { kind: EntryKind::Error, source: Source::System, text: error_message(err), phase: Phase::Start, message_id: Some(id), ..Default::default() }], None);
                }
            }
            _ => {}
        }
    }

    /// Apply a hydration (`hydrate()` after its requests): projected messages (oldest first), the
    /// inbox, permissions (with their resolved tool sources), forms, global forms, active sessions.
    #[allow(clippy::too_many_arguments)]
    pub fn hydrate(&mut self, projected: &[Value], inbox: &[Value], permissions: Vec<PermissionRequest>, sources: &[Value], forms: Vec<FormInfo>, globals: Option<(Vec<FormInfo>, String)>, active: &Value, render: bool, reconnect: bool) {
        self.pending = inbox.iter().filter_map(pending_prompt).map(|(q, f)| (q.message_id.clone(), (q, f))).collect();
        self.sync_pending();
        self.permissions = permissions.into_iter().map(|request| MiniPermission { request, tool: None }).collect();
        self.prune_tool_sources();
        for m in projected {
            self.render_message(m, render);
        }
        // Tool sources of pending permissions from fetched messages.
        let pending: HashSet<String> = self.permissions.iter().filter_map(|p| permission_source_key(&p.request)).collect();
        for m in sources {
            if data_str(m, "type") != "assistant" {
                continue;
            }
            let mid = data_str(m, "id");
            for item in m.get("content").and_then(|c| c.as_array()).into_iter().flatten() {
                if data_str(item, "type") != "tool" {
                    continue;
                }
                let k = key(mid, data_str(item, "id"));
                if pending.contains(&k) {
                    self.tool_sources.insert(k, normalize_tool(&ToolPart::from_value(item)));
                }
            }
        }
        let sources = self.tool_sources.clone();
        for p in self.permissions.iter_mut() {
            p.tool = permission_source_key(&p.request).and_then(|k| sources.get(&k).cloned());
        }
        self.prune_tool_sources();
        self.forms = forms.into_iter().map(|form| MiniForm { form, location: None }).collect();
        self.global_forms = globals.map(|(list, dir)| list.into_iter().filter(|f| f.session_id == "global").map(|form| MiniForm { form, location: Some(dir.clone()) }).collect()).unwrap_or_default();
        self.root_active = active.get(&self.session_id).is_some();
        self.sync_blockers();
        let jobs = self.subagents.hydrate(projected, active, reconnect);
        self.push_subagent_jobs(jobs);
        let running = self.root_active;
        let status = if running { "assistant responding".to_string() } else { blocker_status(&self.view).to_string() };
        self.write(vec![], Some(FooterPatch { phase: Some(if running { FooterPhase::Running } else { FooterPhase::Idle }), status: Some(status), ..Default::default() }));
    }

    fn push_subagent_jobs(&mut self, jobs: Vec<super::subagents::Job>) {
        for j in jobs {
            self.out.push(Out::Job(j));
        }
        if self.subagents.take_dirty() {
            let snap = self.subagents.snapshot();
            self.out.push(Out::Subagent(snap));
            self.sync_blockers();
        }
    }

    /// A subagent job finished (hydrations, discovery).
    pub fn subagent_result(&mut self, result: super::subagents::JobResult) {
        let jobs = self.subagents.apply(result);
        self.push_subagent_jobs(jobs);
    }

    pub fn subagent_select(&mut self, session_id: Option<String>) {
        let jobs = self.subagents.select(session_id);
        self.push_subagent_jobs(jobs);
        let snap = self.subagents.snapshot();
        self.out.push(Out::Subagent(snap));
    }

    /// `apply(attempt, event)`.
    pub fn apply(&mut self, event: &Event, location: Option<&str>) {
        if self.closed {
            return;
        }
        let kind = event.kind.as_str();
        let data = &event.data;
        if CATALOG_EVENTS.contains(&kind) {
            if let (Some(loc), Some(ev)) = (location, event.location.as_ref())
                && ev.directory != loc
            {
                return;
            }
            self.out.push(Out::CatalogRefresh);
            return;
        }
        let source = if kind == "form.created" { data.get("form").and_then(|f| f.get("sessionID")).and_then(|s| s.as_str()) } else { data.get("sessionID").and_then(|s| s.as_str()) };
        if source == Some("global") && matches!(kind, "form.created" | "form.replied" | "form.cancelled") {
            let same = event.location.as_ref().is_some_and(|l| Some(l.directory.as_str()) == location);
            if !same {
                return;
            }
            if kind == "form.created" {
                if let Ok(form) = serde_json::from_value::<FormInfo>(data.get("form").cloned().unwrap_or(Value::Null))
                    && !self.global_forms.iter().any(|f| f.form.id == form.id)
                {
                    self.global_forms.push(MiniForm { form, location: location.map(str::to_string) });
                }
            } else {
                let id = data_str(data, "id").to_string();
                self.global_forms.retain(|f| f.form.id != id);
            }
            self.sync_blockers();
            return;
        }
        if source != Some(self.session_id.as_str()) {
            if let Some(s) = source {
                let jobs = self.subagents.foreign(s, event);
                self.push_subagent_jobs(jobs);
            }
            return;
        }
        let jobs = self.subagents.main(event);
        self.push_subagent_jobs(jobs);
        let am = data_str(data, "assistantMessageID").to_string();
        match kind {
            "session.renamed" => self.out.push(Out::Title(data_str(data, "title").into())),
            "session.inbox.enqueued" => {
                let item = data.get("item").cloned().unwrap_or(Value::Null);
                if data_str(&item, "type") != "user" {
                    return;
                }
                let mut merged = item.clone();
                if let Some(o) = merged.as_object_mut() {
                    o.insert("id".into(), Value::String(data_str(data, "inboxID").into()));
                }
                self.merge_pending(&merged);
            }
            "session.inbox.delivered" => {
                let inbox = data_str(data, "inboxID").to_string();
                if self.wait.is_some() {
                    self.promote_wait(true, Some(&inbox));
                }
                self.admitted.remove(&inbox);
                self.promoted.insert(inbox.clone());
                let pending = self.pending.shift_remove(&inbox);
                self.sync_pending();
                if let Some((q, files)) = pending {
                    self.render_user(&q.message_id, &q.prompt.text, &files, &q.skills, true);
                }
                self.patch(Some(FooterPhase::Running), Some("waiting for assistant"));
            }
            "session.inbox.delivery.changed" => {
                let inbox = data_str(data, "inboxID").to_string();
                if let Some(p) = self.pending.get_mut(&inbox) {
                    p.0.delivery = Delivery::parse(data_str(data, "delivery"));
                    self.sync_pending();
                }
            }
            "session.inbox.cancelled" => {
                let inbox = data_str(data, "inboxID").to_string();
                self.admitted.remove(&inbox);
                if self.pending.shift_remove(&inbox).is_some() {
                    self.sync_pending();
                }
            }
            "session.step.started" => {
                let m = data.get("model").cloned().unwrap_or(Value::Null);
                self.step_model = Some(ModelSel { provider_id: data_str(&m, "providerID").into(), model_id: data_str(&m, "id").into() });
                self.patch(Some(FooterPhase::Running), Some("assistant responding"));
            }
            "session.skill.activated" => {
                let mid = message_id_from_event(&event.id);
                if self.wait.as_ref().is_some_and(|w| w.message_id == mid) {
                    self.promote_wait(true, None);
                }
                if self.skill_messages.contains(&mid) {
                    return;
                }
                self.skill_messages.insert(mid.clone());
                if self.show_tools() {
                    let name = data_str(data, "name").to_string();
                    self.write(vec![skill_commit(&mid, &name, &mid)], None);
                }
            }
            "session.compaction.started" => {
                let mid = data.get("inputID").and_then(|i| i.as_str()).map(str::to_string).unwrap_or_else(|| message_id_from_event(&event.id));
                self.active_compaction = Some(mid.clone());
                if self.message_ids.contains(&mid) {
                    return;
                }
                self.message_ids.insert(mid.clone());
                self.write(vec![compaction_commit(&mid)], Some(FooterPatch { phase: Some(FooterPhase::Running), status: Some("compacting session".into()), ..Default::default() }));
            }
            "session.compaction.delta" => {
                let Some(mid) = self.active_compaction.clone() else { return };
                let text = data_str(data, "text").to_string();
                if self.fragments.delta(&mid, "compaction:summary", &text).is_none() {
                    return;
                }
                self.write(vec![compaction_summary(&mid, &text, Phase::Progress)], None);
            }
            "session.compaction.ended" => {
                let Some(mid) = self.active_compaction.take() else { return };
                let text = data_str(data, "text").to_string();
                let update = self.fragments.end(&mid, "compaction:summary", &text);
                let mut commits = Vec::new();
                if text.len() > update.previous.len() {
                    commits.push(compaction_summary(&mid, text.get(update.previous.len()..).unwrap_or_default(), Phase::Progress));
                }
                commits.push(compaction_summary(&mid, "", Phase::Final));
                self.write(commits, None);
            }
            "session.compaction.failed" => {
                let Some(mid) = self.active_compaction.take() else { return };
                let err = data.get("error").cloned().unwrap_or(Value::Null);
                if data_str(&err, "type") == "aborted" {
                    self.write(vec![compaction_summary(&mid, "", Phase::Final)], None);
                    return;
                }
                self.write(vec![compaction_summary(&mid, "", Phase::Final), compaction_error(&mid, data_str(&err, "message"))], None);
            }
            "session.shell.started" => {
                let shell = data.get("shell").cloned().unwrap_or(Value::Null);
                let sid = data_str(&shell, "id").to_string();
                let command = data_str(&shell, "command").to_string();
                self.shell_commands.insert(sid.clone(), command.clone());
                if let Some(w) = self.shell_wait.as_mut()
                    && w.event_id == event.id
                {
                    w.id = Some(sid.clone());
                }
                if self.shell_started.contains(&sid) {
                    return;
                }
                self.shell_started.insert(sid.clone());
                self.write(vec![shell_commit(&sid, &command, "running shell", Phase::Start, ToolStateKind::Running, None)], Some(FooterPatch { phase: Some(FooterPhase::Running), status: Some("running shell".into()), ..Default::default() }));
            }
            "session.shell.ended" => {
                let shell = data.get("shell").cloned().unwrap_or(Value::Null);
                let sid = data_str(&shell, "id").to_string();
                let command = self.shell_commands.get(&sid).cloned().unwrap_or(data_str(&shell, "command").to_string());
                let mut commits = Vec::new();
                if !self.shell_started.contains(&sid) {
                    self.shell_started.insert(sid.clone());
                    if !command.is_empty() {
                        commits.push(shell_commit(&sid, &command, "running shell", Phase::Start, ToolStateKind::Running, None));
                    }
                }
                if !self.shell_ended.contains(&sid) {
                    self.shell_ended.insert(sid.clone());
                    commits.extend(shell_terminal(&sid, &command, &shell, data.get("output").unwrap_or(&Value::Null)));
                }
                let owned = self.shell_wait.as_ref().is_some_and(|w| w.id.as_deref() == Some(&sid));
                let patch = if owned || self.wait.is_some() || self.shell_wait.is_some() { None } else { Some(FooterPatch { phase: Some(FooterPhase::Idle), status: Some(String::new()), ..Default::default() }) };
                self.write(commits, patch);
                if owned && let Some(w) = self.shell_wait.as_mut() {
                    w.resolved = true;
                }
            }
            "session.text.delta" => {
                let (m, p) = fragment_ref(&am, "text", data.get("ordinal").and_then(|o| o.as_i64()).unwrap_or(0));
                let delta = data_str(data, "delta").to_string();
                if self.fragments.delta(&m, &p, &delta).is_none() {
                    return;
                }
                if !self.show_tools() {
                    self.buffer_quiet_text(&am, &p, &delta, false);
                    return;
                }
                self.write(vec![StreamCommit { kind: EntryKind::Assistant, source: Source::Assistant, text: delta, phase: Phase::Progress, message_id: Some(am), part_id: Some(p), ..Default::default() }], None);
            }
            "session.text.ended" => {
                let (m, p) = fragment_ref(&am, "text", data.get("ordinal").and_then(|o| o.as_i64()).unwrap_or(0));
                let text = data_str(data, "text").to_string();
                let update = self.fragments.end(&m, &p, &text);
                if !self.show_tools() {
                    self.buffer_quiet_text(&am, &update.part_id, &text, true);
                    return;
                }
                if text.len() > update.previous.len() {
                    self.write(vec![StreamCommit { kind: EntryKind::Assistant, source: Source::Assistant, text: text.get(update.previous.len()..).unwrap_or_default().into(), phase: Phase::Progress, message_id: Some(am), part_id: Some(update.part_id), ..Default::default() }], None);
                }
            }
            "session.reasoning.delta" => {
                let (m, p) = fragment_ref(&am, "reasoning", data.get("ordinal").and_then(|o| o.as_i64()).unwrap_or(0));
                let delta = data_str(data, "delta").to_string();
                let Some(update) = self.fragments.delta(&m, &p, &delta) else { return };
                if self.thinking {
                    let text = if update.previous.is_empty() { format!("Thinking: {delta}") } else { delta };
                    self.write(vec![StreamCommit { kind: EntryKind::Reasoning, source: Source::Reasoning, text, phase: Phase::Progress, message_id: Some(am), part_id: Some(update.part_id), ..Default::default() }], None);
                }
            }
            "session.reasoning.ended" => {
                let (m, p) = fragment_ref(&am, "reasoning", data.get("ordinal").and_then(|o| o.as_i64()).unwrap_or(0));
                let text = data_str(data, "text").to_string();
                let update = self.fragments.end(&m, &p, &text);
                if self.thinking && text.len() > update.previous.len() {
                    let t = if update.previous.is_empty() { format!("Thinking: {text}") } else { text.get(update.previous.len()..).unwrap_or_default().to_string() };
                    self.write(vec![StreamCommit { kind: EntryKind::Reasoning, source: Source::Reasoning, text: t, phase: Phase::Progress, message_id: Some(am), part_id: Some(update.part_id), ..Default::default() }], None);
                }
            }
            "session.tool.input.started" => {
                let part = ToolPart { id: data_str(data, "id").into(), name: data_str(data, "name").into(), status: ToolStatus::Streaming, input: Value::String(String::new()), created: event.created, ..Default::default() };
                self.render_tool(&am, &part, true);
            }
            "session.tool.input.delta" | "session.tool.input.ended" => {
                let k = key(&am, data_str(data, "id"));
                let Some(cur) = self.tracks.get(&k).cloned() else { return };
                if cur.part.status != ToolStatus::Streaming {
                    return;
                }
                let input = if kind == "session.tool.input.ended" { data_str(data, "text").to_string() } else { format!("{}{}", cur.part.input.as_str().unwrap_or_default(), data_str(data, "delta")) };
                let part = ToolPart { input: Value::String(input), ..cur.part };
                self.render_tool(&am, &part, true);
            }
            "session.tool.called" => {
                let k = key(&am, data_str(data, "id"));
                if self.finished_tools.contains(&k) {
                    return;
                }
                let cur = self.tracks.get(&k).map(|t| t.part.clone());
                let part = ToolPart {
                    id: data_str(data, "id").into(),
                    name: cur.as_ref().map(|c| c.name.clone()).unwrap_or("tool".into()),
                    status: ToolStatus::Running,
                    input: data.get("input").cloned().unwrap_or(Value::Object(Default::default())),
                    metadata: Value::Object(Default::default()),
                    content: vec![],
                    error: None,
                    created: cur.as_ref().map(|c| c.created).unwrap_or(event.created),
                    ran: Some(event.created),
                    completed: None,
                    executed: data.get("executed").cloned(),
                    native: data.get("native").cloned(),
                };
                self.render_tool(&am, &part, true);
            }
            "session.tool.progress" => {
                let k = key(&am, data_str(data, "id"));
                if self.finished_tools.contains(&k) {
                    return;
                }
                let cur = self.tracks.get(&k).map(|t| t.part.clone());
                let part = ToolPart {
                    id: data_str(data, "id").into(),
                    name: cur.as_ref().map(|c| c.name.clone()).unwrap_or("tool".into()),
                    status: ToolStatus::Running,
                    input: cur.as_ref().filter(|c| c.status != ToolStatus::Streaming).map(|c| c.input.clone()).unwrap_or(Value::Object(Default::default())),
                    metadata: data.get("metadata").cloned().unwrap_or(Value::Object(Default::default())),
                    content: vec![],
                    error: None,
                    created: cur.as_ref().map(|c| c.created).unwrap_or(event.created),
                    ran: cur.as_ref().and_then(|c| c.ran).or(Some(event.created)),
                    completed: None,
                    executed: cur.as_ref().and_then(|c| c.executed.clone()),
                    native: cur.as_ref().and_then(|c| c.native.clone()),
                };
                self.render_tool(&am, &part, true);
            }
            "session.tool.success" | "session.tool.failed" => {
                let k = key(&am, data_str(data, "id"));
                let cur = self.tracks.get(&k).map(|t| t.part.clone());
                let failed = kind == "session.tool.failed";
                let native = match (data.get("native").filter(|n| !n.is_null()), cur.as_ref().and_then(|c| c.native.clone())) {
                    (Some(n), Some(Value::Object(mut base))) => {
                        if let Some(o) = n.as_object() {
                            for (k, v) in o {
                                base.insert(k.clone(), v.clone());
                            }
                        }
                        Some(Value::Object(base))
                    }
                    (Some(n), _) => Some(n.clone()),
                    (None, c) => c,
                };
                let part = ToolPart {
                    id: data_str(data, "id").into(),
                    name: cur.as_ref().map(|c| c.name.clone()).unwrap_or("tool".into()),
                    status: if failed { ToolStatus::Error } else { ToolStatus::Completed },
                    input: cur.as_ref().filter(|c| c.status != ToolStatus::Streaming).map(|c| c.input.clone()).unwrap_or(Value::Object(Default::default())),
                    metadata: data.get("metadata").cloned().unwrap_or(Value::Object(Default::default())),
                    content: data.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default(),
                    error: if failed { data.get("error").cloned() } else { None },
                    created: cur.as_ref().map(|c| c.created).unwrap_or(event.created),
                    ran: cur.as_ref().and_then(|c| c.ran),
                    completed: Some(event.created),
                    executed: data.get("executed").cloned(),
                    native,
                };
                self.render_tool(&am, &part, true);
            }
            "permission.asked" => {
                if let Ok(request) = serde_json::from_value::<PermissionRequest>(data.clone())
                    && !self.permissions.iter().any(|p| p.request.id == request.id)
                {
                    let tool = permission_source_key(&request).and_then(|k| self.tool_sources.get(&k).cloned());
                    self.permissions.push(MiniPermission { request, tool });
                }
                self.sync_blockers();
            }
            "permission.replied" => {
                let id = data_str(data, "requestID").to_string();
                self.permissions.retain(|p| p.request.id != id);
                self.prune_tool_sources();
                self.sync_blockers();
            }
            "form.created" => {
                if let Ok(form) = serde_json::from_value::<FormInfo>(data.get("form").cloned().unwrap_or(Value::Null))
                    && !self.forms.iter().any(|f| f.form.id == form.id)
                {
                    self.forms.push(MiniForm { form, location: None });
                }
                self.sync_blockers();
            }
            "form.replied" | "form.cancelled" => {
                let id = data_str(data, "id").to_string();
                self.forms.retain(|f| f.form.id != id);
                self.sync_blockers();
            }
            "session.step.ended" => {
                let t = data.get("tokens").cloned().unwrap_or(Value::Null);
                let n = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
                let total = n(&t, "input") + n(&t, "output") + n(&t, "reasoning") + t.get("cache").map(|c| n(c, "read") + n(c, "write")).unwrap_or(0.0);
                let limit = self.step_model.take().and_then(|m| self.context_limits.get(&key(&m.provider_id, &m.model_id)).copied());
                if !self.show_tools() {
                    self.flush_quiet_text(&am);
                }
                let cost = data.get("cost").and_then(|c| c.as_f64()).filter(|c| *c != 0.0);
                let usage = if total > 0.0 || cost.is_some() { Some(Usage { tokens: total, percent: limit.map(|l| ((total / l) * 100.0).round() as i64), cost }) } else { None };
                self.write(vec![], Some(FooterPatch { usage: Some(usage), ..Default::default() }));
            }
            "session.step.failed" => {
                self.step_model = None;
                if !self.show_tools() {
                    self.flush_quiet_text(&am);
                }
                let rendered = self.errors.contains(&am);
                self.errors.insert(am.clone());
                if let Some(w) = self.wait.as_mut() {
                    w.failure_rendered = true;
                }
                if rendered {
                    return;
                }
                let err = data.get("error").cloned().unwrap_or(Value::Null);
                self.write(vec![StreamCommit { kind: EntryKind::Error, source: Source::System, text: error_message(&err), phase: Phase::Start, message_id: Some(am), ..Default::default() }], None);
            }
            "session.execution.started" => {
                self.execution_epoch += 1;
                self.root_active = true;
                self.write(vec![], Some(FooterPatch { phase: Some(FooterPhase::Running), ..Default::default() }));
            }
            "session.execution.succeeded" | "session.execution.failed" | "session.execution.interrupted" => {
                self.execution_epoch += 1;
                if !self.show_tools() {
                    let ids: Vec<String> = self.quiet_text.keys().cloned().collect();
                    for id in ids {
                        self.flush_quiet_text(&id);
                    }
                }
                self.paint_idle("");
                let Some(w) = self.wait.as_mut() else { return };
                if w.interrupted && kind == "session.execution.interrupted" && data_str(data, "reason") == "user" {
                    return;
                }
                if kind == "session.execution.failed" {
                    if !w.failure_rendered {
                        w.terminal_error = Some(error_message(data.get("error").unwrap_or(&Value::Null)));
                    }
                    return;
                }
                if kind == "session.execution.interrupted" {
                    w.terminal_error = Some(format!("Session interrupted: {}", data_str(data, "reason")));
                }
            }
            _ => {}
        }
    }

    /// Clear per-projection state before a resize replay.
    pub fn reset_projection(&mut self) {
        self.message_ids.clear();
        self.image_ids.clear();
        self.fragments.clear();
        self.tracks.clear();
        self.tool_sources.clear();
        self.finished_tools.clear();
        self.tool_messages.clear();
        self.quiet_text.clear();
        self.skill_messages.clear();
        self.shell_commands.clear();
        self.shell_started.clear();
        self.active_compaction = None;
        self.shell_ended.clear();
        self.errors.clear();
    }

    /// Re-append local rows after a resize replay (`performResizeReplay`).
    pub fn replay_local(&mut self, rows: &[StreamCommit]) -> Vec<StreamCommit> {
        let mut out = Vec::new();
        for c in rows {
            if let (Some(_), Some(m), Some(p)) = (&c.image, &c.message_id, &c.part_id) {
                let k = key(m, p);
                if self.image_ids.contains(&k) {
                    continue;
                }
                self.image_ids.insert(k);
                out.push(c.clone());
                continue;
            }
            if let (Some(m), Some(p)) = (&c.message_id, &c.part_id)
                && matches!(c.kind, EntryKind::Assistant | EntryKind::Reasoning)
            {
                let prefix = if c.kind == EntryKind::Reasoning { "Thinking: " } else { "" };
                let text = c.text.strip_prefix(prefix).unwrap_or(&c.text).to_string();
                match self.fragments.restore(m, p, &text) {
                    Restore::Covered => continue,
                    Restore::Append(suffix) => {
                        if !suffix.is_empty() {
                            out.push(if suffix == text { c.clone() } else { StreamCommit { text: suffix, ..c.clone() } });
                        }
                        continue;
                    }
                    Restore::Conflict => {}
                }
            }
            if c.kind == EntryKind::Error
                && let Some(m) = &c.message_id
            {
                if self.errors.contains(m) {
                    continue;
                }
                self.errors.insert(m.clone());
                out.push(c.clone());
                continue;
            }
            if c.message_id.as_ref().is_some_and(|m| self.message_ids.contains(m)) {
                continue;
            }
            out.push(c.clone());
        }
        out
    }

    pub fn settle_form(&mut self, session_id: &str, form_id: &str) {
        if session_id == self.session_id {
            self.forms.retain(|f| f.form.id != form_id);
        } else if session_id == "global" {
            self.global_forms.retain(|f| f.form.id != form_id);
        } else {
            self.subagents.settle_form(session_id, form_id);
        }
        self.sync_blockers();
    }

    pub fn take(&mut self) -> Vec<Out> {
        std::mem::take(&mut self.out)
    }
}

/// `permissionSourceKey` of a tool-sourced permission.
pub fn permission_source_key(r: &PermissionRequest) -> Option<String> {
    let s = r.source.as_ref()?;
    if s.get("type").and_then(|t| t.as_str()) != Some("tool") {
        return None;
    }
    Some(key(s.get("messageID").and_then(|m| m.as_str()).unwrap_or_default(), s.get("id").and_then(|m| m.as_str()).unwrap_or_default()))
}
