//! The subagent tracker (`src/mini/stream-v2.subagent.ts` `createSubagentTracker`): child sessions
//! of the root session as footer tabs, their transcripts (frames) and blockers. HTTP work is returned
//! as [`Job`]s for the runtime; results come back through [`SubagentTracker::apply`].

use super::tool::{normalize_tool, tool_display_content, tool_output_text};
use super::transport::{Fragments, fragment_ref, tool_commit, tool_final_phase, tool_image_commits, user_image_commits};
use super::types::*;
use crate::api::types::{Event, FormInfo, PermissionRequest, SessionInfo};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

const CHILD_FRAME_LIMIT: usize = 80;
const CHILD_EVENT_BUFFER_LIMIT: usize = 64;
const FAMILY_LIST_LIMIT: usize = 100;
const FALLBACK_LABEL: &str = "Subagent";

#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    /// `message.list(desc, 80)` + `inbox.list` of a child.
    HydrateChild(String),
    /// `permission.list` + `session.form.list` of a child.
    HydrateBlockers(String),
    /// `session.get` lineage walk for an unknown session.
    Discover(String),
    /// `session.list({ parentID })` of an owner (family index).
    Family(String),
}

#[derive(Clone, Debug)]
pub enum JobResult {
    Child { session_id: String, messages_desc: Vec<Value>, inbox: Vec<Value> },
    ChildFailed(String),
    Blockers { session_id: String, permissions: Vec<PermissionRequest>, forms: Vec<FormInfo> },
    Discovered { session_id: String, lineage: Vec<SessionInfo> },
    DiscoverFailed(String),
    Family { owner: String, sessions: Vec<SessionInfo> },
}

#[derive(Clone, Debug)]
struct Child {
    session_id: String,
    label: String,
    description: String,
    status: SubagentStatus,
    background: bool,
    title: Option<String>,
    last_updated: i64,
    frames: Vec<(String, StreamCommit)>,
    fragments: Fragments,
    tools: HashMap<String, ToolPart>,
    tool_sources: HashMap<String, ToolPart>,
    finished_tools: HashSet<String>,
    permissions: Vec<MiniPermission>,
    forms: Vec<MiniForm>,
    message_ids: HashSet<String>,
    prompts: HashMap<String, Value>,
    hydrated: bool,
    hydrating: bool,
    buffered: Vec<Event>,
    detail_stale: bool,
}

fn source_key(message_id: &str, id: &str) -> String {
    format!("{message_id}\u{0}{id}")
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or_default()
}

fn text(v: Option<&Value>) -> Option<String> {
    v.and_then(|x| x.as_str()).map(str::trim).filter(|t| !t.is_empty()).map(str::to_string)
}

/// `childSessionID(metadata)`.
fn child_session_id(metadata: Option<&Value>) -> Option<(String, bool)> {
    let m = metadata?;
    let sid = text(m.get("sessionID"))?;
    if !sid.starts_with("ses") {
        return None;
    }
    match m.get("status").and_then(|x| x.as_str()) {
        Some("running") => Some((sid, true)),
        Some("completed") => Some((sid, false)),
        _ => None,
    }
}

pub struct SubagentTracker {
    session_id: String,
    thinking: bool,
    directory: Option<String>,
    children: indexmap::IndexMap<String, Child>,
    pending_calls: HashMap<String, Value>,
    checked: Vec<String>,
    pending_events: HashMap<String, Vec<Event>>,
    discovering: HashSet<String>,
    family_seen: HashSet<String>,
    selected: Option<String>,
    dirty: bool,
}

impl SubagentTracker {
    pub fn new(session_id: &str, thinking: bool, directory: Option<String>) -> SubagentTracker {
        SubagentTracker { session_id: session_id.into(), thinking, directory, children: indexmap::IndexMap::new(), pending_calls: HashMap::new(), checked: Vec::new(), pending_events: HashMap::new(), discovering: HashSet::new(), family_seen: HashSet::new(), selected: None, dirty: false }
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    fn emit(&mut self) {
        self.dirty = true;
    }

    fn notify_detail(&mut self, sid: &str) {
        if self.selected.as_deref() == Some(sid) {
            self.dirty = true;
        }
    }

    fn admit(&mut self, sid: &str) -> bool {
        if !self.children.contains_key(sid) {
            if self.children.len() >= FAMILY_LIST_LIMIT {
                return false;
            }
            self.children.insert(
                sid.into(),
                Child {
                    session_id: sid.into(),
                    label: FALLBACK_LABEL.into(),
                    description: String::new(),
                    status: SubagentStatus::Running,
                    background: false,
                    title: None,
                    last_updated: 0,
                    frames: Vec::new(),
                    fragments: Fragments::default(),
                    tools: HashMap::new(),
                    tool_sources: HashMap::new(),
                    finished_tools: HashSet::new(),
                    permissions: Vec::new(),
                    forms: Vec::new(),
                    message_ids: HashSet::new(),
                    prompts: HashMap::new(),
                    hydrated: false,
                    hydrating: false,
                    buffered: Vec::new(),
                    detail_stale: false,
                },
            );
        }
        if let Some(buffered) = self.pending_events.remove(sid) {
            for e in buffered {
                self.reduce(sid, &e);
            }
        }
        true
    }

    fn touch(child: &mut Child, ts: i64) {
        child.last_updated = child.last_updated.max(ts);
    }

    fn set_frame(child: &mut Child, key: String, commit: StreamCommit) {
        if let Some(f) = child.frames.iter_mut().find(|f| f.0 == key) {
            f.1 = commit;
            return;
        }
        child.frames.push((key, commit));
        if child.frames.len() > CHILD_FRAME_LIMIT {
            let n = child.frames.len() - CHILD_FRAME_LIMIT;
            child.frames.drain(..n);
        }
    }

    fn apply_meta(child: &mut Child, meta: Option<&Value>) {
        let Some(meta) = meta else { return };
        if let Some(agent) = text(meta.get("agent")) {
            child.label = crate::util::locale::titlecase(&agent);
        }
        if let Some(d) = text(meta.get("description")) {
            child.description = d;
        }
        if meta.get("background").and_then(|b| b.as_bool()) == Some(true) {
            child.background = true;
        }
    }

    fn user_frame(child: &mut Child, message_id: &str, prompt: &Value) -> bool {
        if child.message_ids.contains(message_id) {
            return false;
        }
        child.message_ids.insert(message_id.into());
        let t = s(prompt, "text");
        if !t.trim().is_empty() {
            Self::set_frame(child, format!("user:{message_id}"), StreamCommit { kind: EntryKind::User, source: Source::System, text: t.into(), phase: Phase::Start, message_id: Some(message_id.into()), ..Default::default() });
        }
        let files = prompt.get("files").and_then(|f| f.as_array()).cloned().unwrap_or_default();
        for c in user_image_commits(message_id, &files) {
            let k = source_key(message_id, c.part_id.as_deref().unwrap_or(""));
            Self::set_frame(child, k, c);
        }
        true
    }

    fn child_tool(child: &mut Child, item: &ToolPart, message_id: &str, directory: Option<&str>) {
        let part = normalize_tool(item);
        let key = source_key(message_id, &part.id);
        let frame = format!("tool:{key}");
        child.tool_sources.insert(key.clone(), part.clone());
        if part.status == ToolStatus::Streaming {
            child.tools.insert(key, part);
            return;
        }
        let current = child.tools.get(&key).cloned();
        let output = tool_output_text(&part.name, tool_display_content(&part));
        if part.status == ToolStatus::Running {
            let ready = part.name != "websearch" || part.metadata.get("provider").is_some_and(|p| p.is_string());
            let awaiting = current.as_ref().is_some_and(|c| c.name == "websearch" && c.status == ToolStatus::Running && !c.metadata.get("provider").is_some_and(|p| p.is_string()));
            if ready && (current.is_none() || current.as_ref().is_some_and(|c| c.status == ToolStatus::Streaming) || awaiting) {
                Self::set_frame(child, frame.clone(), tool_commit(&part, message_id, Phase::Start, None, directory, 0));
            }
            if !output.is_empty() {
                Self::set_frame(child, frame, tool_commit(&part, message_id, Phase::Progress, Some(&output), directory, 0));
            }
            child.tools.insert(key, part);
            return;
        }
        child.finished_tools.insert(key.clone());
        child.tools.remove(&key);
        let partial = part.status == ToolStatus::Error && !output.is_empty();
        if partial {
            Self::set_frame(child, frame.clone(), tool_commit(&part, message_id, Phase::Progress, Some(&output), directory, 0));
        }
        Self::set_frame(child, if partial { format!("{frame}:final") } else { frame }, tool_commit(&part, message_id, tool_final_phase(&part), None, directory, 0));
        for c in tool_image_commits(&part, message_id) {
            let k = source_key(message_id, c.part_id.as_deref().unwrap_or(""));
            Self::set_frame(child, k, c);
        }
    }

    fn rebuild(&mut self, sid: &str, messages: &[Value]) {
        let thinking = self.thinking;
        let directory = self.directory.clone();
        let Some(child) = self.children.get_mut(sid) else { return };
        child.frames.clear();
        child.fragments.clear();
        child.finished_tools.clear();
        child.tool_sources.clear();
        child.message_ids.clear();
        for m in messages {
            let id = s(m, "id").to_string();
            match s(m, "type") {
                "user" => {
                    child.prompts.remove(&id);
                    Self::user_frame(child, &id, m);
                }
                "assistant" => {
                    child.message_ids.insert(id.clone());
                    let (mut to, mut ro) = (0, 0);
                    for item in m.get("content").and_then(|c| c.as_array()).into_iter().flatten() {
                        match s(item, "type") {
                            "text" => {
                                let (mm, p) = fragment_ref(&id, "text", to);
                                to += 1;
                                let u = child.fragments.project(&mm, &p, s(item, "text"), true);
                                Self::set_frame(child, u.key, StreamCommit { kind: EntryKind::Assistant, source: Source::Assistant, text: s(item, "text").into(), phase: Phase::Progress, message_id: Some(id.clone()), part_id: Some(p), ..Default::default() });
                            }
                            "reasoning" => {
                                let (mm, p) = fragment_ref(&id, "reasoning", ro);
                                ro += 1;
                                let u = child.fragments.project(&mm, &p, s(item, "text"), true);
                                if thinking {
                                    Self::set_frame(child, u.key, StreamCommit { kind: EntryKind::Reasoning, source: Source::Reasoning, text: format!("Thinking: {}", s(item, "text")), phase: Phase::Progress, message_id: Some(id.clone()), part_id: Some(p), ..Default::default() });
                                }
                            }
                            _ => Self::child_tool(child, &ToolPart::from_value(item), &id, directory.as_deref()),
                        }
                    }
                    if let Some(e) = m.get("error").filter(|e| !e.is_null()) {
                        Self::set_frame(child, format!("error:{id}"), StreamCommit { kind: EntryKind::Error, source: Source::System, text: s(e, "message").into(), phase: Phase::Start, message_id: Some(id.clone()), ..Default::default() });
                    }
                }
                _ => {}
            }
        }
    }

    fn reduce(&mut self, sid: &str, event: &Event) {
        let thinking = self.thinking;
        let directory = self.directory.clone();
        let data = &event.data;
        let am = s(data, "assistantMessageID").to_string();
        let kind = event.kind.as_str();
        let mut emit = false;
        let mut detail = false;
        {
            let Some(child) = self.children.get_mut(sid) else { return };
            match kind {
                "session.inbox.enqueued" => {
                    if s(data.get("item").unwrap_or(&Value::Null), "type") == "user" {
                        child.prompts.insert(s(data, "inboxID").into(), data["item"]["payload"].clone());
                    }
                }
                "session.inbox.delivered" => {
                    let id = s(data, "inboxID").to_string();
                    if let Some(p) = child.prompts.remove(&id)
                        && Self::user_frame(child, &id, &p)
                    {
                        Self::touch(child, event.created);
                        detail = true;
                    }
                }
                "session.inbox.cancelled" => {
                    child.prompts.remove(s(data, "inboxID"));
                }
                "session.step.started" => {
                    Self::touch(child, event.created);
                    if child.label == FALLBACK_LABEL
                        && let Some(a) = text(data.get("agent"))
                    {
                        child.label = crate::util::locale::titlecase(&a);
                    }
                    child.status = SubagentStatus::Running;
                    emit = true;
                }
                "session.text.delta" | "session.text.ended" => {
                    let (m, p) = fragment_ref(&am, "text", data.get("ordinal").and_then(|o| o.as_i64()).unwrap_or(0));
                    let update = if kind == "session.text.delta" { child.fragments.delta(&m, &p, s(data, "delta")) } else { Some(child.fragments.end(&m, &p, s(data, "text"))) };
                    let Some(update) = update else { return };
                    let t = if kind == "session.text.delta" { update.text.clone() } else { s(data, "text").to_string() };
                    Self::set_frame(child, update.key.clone(), StreamCommit { kind: EntryKind::Assistant, source: Source::Assistant, text: t, phase: Phase::Progress, message_id: Some(am.clone()), part_id: Some(update.part_id.clone()), ..Default::default() });
                    Self::touch(child, event.created);
                    detail = true;
                }
                "session.reasoning.delta" | "session.reasoning.ended" => {
                    let (m, p) = fragment_ref(&am, "reasoning", data.get("ordinal").and_then(|o| o.as_i64()).unwrap_or(0));
                    let update = if kind == "session.reasoning.delta" { child.fragments.delta(&m, &p, s(data, "delta")) } else { Some(child.fragments.end(&m, &p, s(data, "text"))) };
                    let Some(update) = update else { return };
                    if !thinking {
                        return;
                    }
                    let t = if kind == "session.reasoning.delta" { update.text.clone() } else { s(data, "text").to_string() };
                    Self::set_frame(child, update.key.clone(), StreamCommit { kind: EntryKind::Reasoning, source: Source::Reasoning, text: format!("Thinking: {t}"), phase: Phase::Progress, message_id: Some(am.clone()), part_id: Some(update.part_id.clone()), ..Default::default() });
                    detail = true;
                }
                "session.tool.input.started" => {
                    if child.finished_tools.contains(&source_key(&am, s(data, "id"))) {
                        return;
                    }
                    let part = ToolPart { id: s(data, "id").into(), name: s(data, "name").into(), status: ToolStatus::Streaming, input: Value::String(String::new()), created: event.created, ..Default::default() };
                    Self::child_tool(child, &part, &am, directory.as_deref());
                }
                "session.tool.input.delta" | "session.tool.input.ended" => {
                    let Some(cur) = child.tools.get(&source_key(&am, s(data, "id"))).cloned() else { return };
                    if cur.status != ToolStatus::Streaming {
                        return;
                    }
                    let input = if kind == "session.tool.input.ended" { s(data, "text").to_string() } else { format!("{}{}", cur.input.as_str().unwrap_or_default(), s(data, "delta")) };
                    Self::child_tool(child, &ToolPart { input: Value::String(input), ..cur }, &am, directory.as_deref());
                }
                "session.tool.called" | "session.tool.progress" | "session.tool.success" | "session.tool.failed" => {
                    let k = source_key(&am, s(data, "id"));
                    if child.finished_tools.contains(&k) {
                        return;
                    }
                    let cur = child.tools.get(&k).cloned();
                    let prior_input = cur.as_ref().filter(|c| c.status != ToolStatus::Streaming).map(|c| c.input.clone()).unwrap_or(Value::Object(Default::default()));
                    let part = match kind {
                        "session.tool.called" => ToolPart {
                            id: s(data, "id").into(),
                            name: cur.as_ref().map(|c| c.name.clone()).unwrap_or("tool".into()),
                            status: ToolStatus::Running,
                            input: data.get("input").cloned().unwrap_or(Value::Object(Default::default())),
                            metadata: Value::Object(Default::default()),
                            created: cur.as_ref().map(|c| c.created).unwrap_or(event.created),
                            ran: Some(event.created),
                            executed: data.get("executed").cloned(),
                            native: data.get("native").cloned(),
                            ..Default::default()
                        },
                        "session.tool.progress" => ToolPart {
                            id: s(data, "id").into(),
                            name: cur.as_ref().map(|c| c.name.clone()).unwrap_or("tool".into()),
                            status: ToolStatus::Running,
                            input: prior_input,
                            metadata: data.get("metadata").cloned().unwrap_or(Value::Object(Default::default())),
                            created: cur.as_ref().map(|c| c.created).unwrap_or(event.created),
                            ran: cur.as_ref().and_then(|c| c.ran).or(Some(event.created)),
                            ..Default::default()
                        },
                        _ => {
                            let failed = kind == "session.tool.failed";
                            ToolPart {
                                id: s(data, "id").into(),
                                name: cur.as_ref().map(|c| c.name.clone()).unwrap_or("tool".into()),
                                status: if failed { ToolStatus::Error } else { ToolStatus::Completed },
                                input: prior_input,
                                metadata: data.get("metadata").cloned().unwrap_or(Value::Object(Default::default())),
                                content: data.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default(),
                                error: if failed { data.get("error").cloned() } else { None },
                                created: cur.as_ref().map(|c| c.created).unwrap_or(event.created),
                                ran: cur.as_ref().and_then(|c| c.ran),
                                completed: Some(event.created),
                                ..Default::default()
                            }
                        }
                    };
                    Self::child_tool(child, &part, &am, directory.as_deref());
                    Self::touch(child, event.created);
                    detail = true;
                }
                "permission.asked" => {
                    if let Ok(request) = serde_json::from_value::<PermissionRequest>(data.clone())
                        && !child.permissions.iter().any(|p| p.request.id == request.id)
                    {
                        let tool = super::transport::permission_source_key(&request).and_then(|k| child.tool_sources.get(&k).cloned());
                        child.permissions.push(MiniPermission { request, tool });
                    }
                    emit = true;
                }
                "permission.replied" => {
                    let id = s(data, "requestID").to_string();
                    child.permissions.retain(|p| p.request.id != id);
                    emit = true;
                }
                "form.created" => {
                    if let Ok(form) = serde_json::from_value::<FormInfo>(data.get("form").cloned().unwrap_or(Value::Null))
                        && !child.forms.iter().any(|f| f.form.id == form.id)
                    {
                        child.forms.push(MiniForm { form, location: None });
                    }
                    emit = true;
                }
                "form.replied" | "form.cancelled" => {
                    let id = s(data, "id").to_string();
                    child.forms.retain(|f| f.form.id != id);
                    emit = true;
                }
                "session.step.failed" => {
                    Self::set_frame(child, format!("error:{am}"), StreamCommit { kind: EntryKind::Error, source: Source::System, text: s(&data["error"], "message").into(), phase: Phase::Start, message_id: Some(am.clone()), ..Default::default() });
                    Self::touch(child, event.created);
                    detail = true;
                }
                "session.execution.started" => {
                    child.status = SubagentStatus::Running;
                    Self::touch(child, event.created);
                    emit = true;
                }
                "session.execution.succeeded" | "session.execution.failed" | "session.execution.interrupted" => {
                    child.status = match kind {
                        "session.execution.succeeded" => SubagentStatus::Completed,
                        "session.execution.interrupted" => SubagentStatus::Cancelled,
                        _ => SubagentStatus::Error,
                    };
                    Self::touch(child, event.created);
                    emit = true;
                }
                _ => {}
            }
        }
        if emit {
            self.emit();
        }
        if detail {
            self.notify_detail(sid);
        }
    }

    /// `main(event)` for root-session events.
    pub fn main(&mut self, event: &Event) -> Vec<Job> {
        let data = &event.data;
        let k = source_key(s(data, "assistantMessageID"), s(data, "id"));
        match event.kind.as_str() {
            "session.tool.input.started" => {
                if super::util::canonical_tool_name(s(data, "name")) == "subagent" {
                    self.pending_calls.insert(k, Value::Object(Default::default()));
                }
                return vec![];
            }
            "session.tool.called" => {
                if self.pending_calls.contains_key(&k) {
                    self.pending_calls.insert(k, data.get("input").cloned().unwrap_or(Value::Null));
                }
                return vec![];
            }
            "session.tool.progress" | "session.tool.success" | "session.tool.failed" => {}
            _ => return vec![],
        }
        let pending = self.pending_calls.get(&k).cloned();
        if event.kind != "session.tool.progress" {
            self.pending_calls.remove(&k);
        }
        let Some((sid, running)) = child_session_id(data.get("metadata")) else { return vec![] };
        if !self.admit(&sid) {
            return vec![];
        }
        let child = self.children.get_mut(&sid).unwrap();
        Self::apply_meta(child, pending.as_ref());
        if event.kind == "session.tool.success" && running {
            child.background = true;
            child.status = SubagentStatus::Running;
        }
        if event.kind == "session.tool.success" && !running && child.status == SubagentStatus::Running {
            child.status = SubagentStatus::Completed;
        }
        Self::touch(child, event.created);
        let mut jobs = vec![Job::HydrateBlockers(sid.clone())];
        if !child.hydrated && !child.hydrating {
            child.hydrating = true;
            jobs.push(Job::HydrateChild(sid.clone()));
        }
        self.emit();
        jobs
    }

    /// `foreign(sessionID, event)`.
    pub fn foreign(&mut self, sid: &str, event: &Event) -> Vec<Job> {
        if let Some(child) = self.children.get_mut(sid) {
            if child.hydrating {
                if child.buffered.len() < CHILD_EVENT_BUFFER_LIMIT {
                    child.buffered.push(event.clone());
                }
                return vec![];
            }
            self.reduce(sid, event);
            return vec![];
        }
        let mut jobs = Vec::new();
        if self.children.len() < FAMILY_LIST_LIMIT && !self.checked.iter().any(|c| c == sid) && sid != self.session_id && !self.discovering.contains(sid) {
            self.discovering.insert(sid.into());
            self.pending_events.insert(sid.into(), Vec::new());
            jobs.push(Job::Discover(sid.into()));
        }
        if let Some(b) = self.pending_events.get_mut(sid)
            && b.len() < CHILD_EVENT_BUFFER_LIMIT
        {
            b.push(event.clone());
        }
        jobs
    }

    /// `mainTool(item, active)` during hydration.
    fn main_tool(&mut self, item: &ToolPart, active: &Value) {
        let tool = normalize_tool(item);
        if tool.name != "subagent" || tool.status == ToolStatus::Streaming {
            return;
        }
        let Some((sid, running)) = child_session_id(Some(&tool.metadata)) else { return };
        if !self.admit(&sid) {
            return;
        }
        let child = self.children.get_mut(&sid).unwrap();
        Self::apply_meta(child, Some(&tool.input));
        if tool.status == ToolStatus::Completed && running {
            child.background = true;
        }
        if child.status == SubagentStatus::Running {
            let still = running && active.get(&sid).is_some();
            child.status = if still { SubagentStatus::Running } else { SubagentStatus::Completed };
        }
        Self::touch(child, tool.completed.unwrap_or(tool.created));
    }

    /// `hydrate({ messages, active, reconnect })`.
    pub fn hydrate(&mut self, messages: &[Value], active: &Value, reconnect: bool) -> Vec<Job> {
        if reconnect {
            for c in self.children.values_mut() {
                c.hydrated = false;
                c.detail_stale = true;
            }
        }
        for m in messages {
            if s(m, "type") != "assistant" {
                continue;
            }
            for item in m.get("content").and_then(|c| c.as_array()).into_iter().flatten() {
                if s(item, "type") == "tool" {
                    self.main_tool(&ToolPart::from_value(item), active);
                }
            }
        }
        self.family_seen.insert(self.session_id.clone());
        let mut jobs = vec![Job::Family(self.session_id.clone())];
        if let Some(o) = active.as_object() {
            for sid in o.keys() {
                if !self.children.contains_key(sid) && sid != &self.session_id && !self.discovering.contains(sid) {
                    self.discovering.insert(sid.clone());
                    self.pending_events.insert(sid.clone(), Vec::new());
                    jobs.push(Job::Discover(sid.clone()));
                }
            }
        }
        for c in self.children.values_mut() {
            if c.status == SubagentStatus::Running && active.get(&c.session_id).is_none() {
                c.status = SubagentStatus::Completed;
            }
            jobs.push(Job::HydrateBlockers(c.session_id.clone()));
        }
        if let Some(sel) = self.selected.clone()
            && let Some(c) = self.children.get_mut(&sel)
            && !c.hydrating
        {
            c.hydrating = true;
            jobs.push(Job::HydrateChild(sel));
        }
        if !self.children.is_empty() {
            self.emit();
        }
        jobs
    }

    /// Apply a finished job.
    pub fn apply(&mut self, result: JobResult) -> Vec<Job> {
        let mut jobs = Vec::new();
        match result {
            JobResult::Child { session_id, messages_desc, inbox } => {
                let Some(child) = self.children.get_mut(&session_id) else { return jobs };
                for item in &inbox {
                    if s(item, "type") == "user" {
                        child.prompts.insert(s(item, "id").into(), item.get("payload").cloned().unwrap_or(Value::Null));
                    }
                }
                let messages: Vec<Value> = messages_desc.into_iter().rev().collect();
                self.rebuild(&session_id, &messages);
                let child = self.children.get_mut(&session_id).unwrap();
                let sources = child.tool_sources.clone();
                for p in child.permissions.iter_mut() {
                    if p.tool.is_none() {
                        p.tool = super::transport::permission_source_key(&p.request).and_then(|k| sources.get(&k).cloned());
                    }
                }
                child.hydrating = false;
                let buffered = std::mem::take(&mut child.buffered);
                for e in buffered {
                    self.reduce(&session_id, &e);
                }
                let child = self.children.get_mut(&session_id).unwrap();
                child.hydrated = true;
                child.detail_stale = false;
                self.notify_detail(&session_id);
            }
            JobResult::ChildFailed(sid) => {
                if let Some(c) = self.children.get_mut(&sid) {
                    c.hydrating = false;
                    let buffered = std::mem::take(&mut c.buffered);
                    for e in buffered {
                        self.reduce(&sid, &e);
                    }
                }
            }
            JobResult::Blockers { session_id, permissions, forms } => {
                let Some(child) = self.children.get_mut(&session_id) else { return jobs };
                child.permissions = permissions.into_iter().map(|request| {
                    let tool = super::transport::permission_source_key(&request).and_then(|k| child.tool_sources.get(&k).cloned());
                    MiniPermission { request, tool }
                }).collect();
                child.forms = forms.into_iter().map(|form| MiniForm { form, location: None }).collect();
                self.emit();
            }
            JobResult::Discovered { session_id, lineage } => {
                self.discovering.remove(&session_id);
                let root = self.session_id.clone();
                // The lineage runs from the discovered session up; adopt it when it reaches the root or a known child.
                let attach = lineage.iter().position(|sess| sess.parent_id.as_deref() == Some(root.as_str()) || sess.parent_id.as_ref().is_some_and(|p| self.children.contains_key(p)));
                match attach {
                    Some(i) => {
                        let buffered = self.pending_events.remove(&session_id).unwrap_or_default();
                        for sess in lineage[..=i].iter().rev() {
                            if !self.admit(&sess.id) {
                                break;
                            }
                            let c = self.children.get_mut(&sess.id).unwrap();
                            if let Some(a) = &sess.agent {
                                c.label = crate::util::locale::titlecase(a);
                            }
                            c.title = sess.title.clone();
                            Self::touch(c, sess.time.updated);
                        }
                        if self.children.contains_key(&session_id) {
                            for e in buffered {
                                self.reduce(&session_id, &e);
                            }
                            self.emit();
                            jobs.push(Job::HydrateBlockers(session_id.clone()));
                            let c = self.children.get_mut(&session_id).unwrap();
                            if !c.hydrating {
                                c.hydrating = true;
                                jobs.push(Job::HydrateChild(session_id));
                            }
                        }
                    }
                    None => {
                        self.pending_events.remove(&session_id);
                        self.checked.retain(|c| c != &session_id);
                        self.checked.push(session_id);
                        if self.checked.len() > FAMILY_LIST_LIMIT {
                            self.checked.remove(0);
                        }
                    }
                }
            }
            JobResult::DiscoverFailed(sid) => {
                self.discovering.remove(&sid);
                self.pending_events.remove(&sid);
            }
            JobResult::Family { owner: _, sessions } => {
                for sess in sessions {
                    if self.family_seen.contains(&sess.id) || self.children.len() >= FAMILY_LIST_LIMIT {
                        continue;
                    }
                    self.family_seen.insert(sess.id.clone());
                    if !self.admit(&sess.id) {
                        break;
                    }
                    let c = self.children.get_mut(&sess.id).unwrap();
                    if let Some(a) = &sess.agent
                        && c.label == FALLBACK_LABEL
                    {
                        c.label = crate::util::locale::titlecase(a);
                    }
                    if c.title.is_none() {
                        c.title = sess.title.clone();
                    }
                    Self::touch(c, sess.time.updated);
                    jobs.push(Job::Family(sess.id.clone()));
                    jobs.push(Job::HydrateBlockers(sess.id.clone()));
                }
                if !self.children.is_empty() {
                    self.emit();
                }
            }
        }
        jobs
    }

    /// `select(sessionID)`.
    pub fn select(&mut self, session_id: Option<String>) -> Vec<Job> {
        self.selected = session_id.clone();
        let mut jobs = Vec::new();
        if let Some(sid) = session_id
            && let Some(c) = self.children.get_mut(&sid)
            && !c.hydrated
            && !c.hydrating
        {
            c.hydrating = true;
            jobs.push(Job::HydrateChild(sid));
        }
        self.emit();
        jobs
    }

    pub fn settle_form(&mut self, session_id: &str, form_id: &str) {
        if let Some(c) = self.children.get_mut(session_id) {
            c.forms.retain(|f| f.form.id != form_id);
            self.emit();
        }
    }

    /// `snapshot()`.
    pub fn snapshot(&self) -> SubagentState {
        let mut children: Vec<&Child> = self.children.values().collect();
        children.sort_by(|a, b| {
            let active = (b.status == SubagentStatus::Running) as i32 - (a.status == SubagentStatus::Running) as i32;
            if active != 0 {
                return active.cmp(&0);
            }
            b.last_updated.cmp(&a.last_updated)
        });
        let tabs = children
            .iter()
            .map(|c| SubagentTab {
                session_id: c.session_id.clone(),
                label: c.label.clone(),
                description: if !c.description.is_empty() { c.description.clone() } else { c.title.clone().unwrap_or_default() },
                status: c.status,
                background: c.background,
                title: c.title.clone(),
            })
            .collect();
        let mut details = indexmap::IndexMap::new();
        if let Some(sel) = &self.selected
            && let Some(c) = self.children.get(sel)
            && !c.detail_stale
        {
            details.insert(sel.clone(), c.frames.iter().map(|f| f.1.clone()).collect());
        }
        SubagentState { tabs, details, permissions: self.children.values().flat_map(|c| c.permissions.clone()).collect(), forms: self.children.values().flat_map(|c| c.forms.clone()).collect() }
    }
}
