//! Client data store: projects server events into session/message/permission/form state and
//! caches per-location catalogs (`client/src/solid/data.ts`). Pure: follow-up reads are returned
//! as [`Effect`]s for the app to execute.

use crate::api::types::*;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Catalog {
    Agents,
    Commands,
    Models,
    Providers,
    References,
    Skills,
    Mcp,
    Integrations,
    Vcs,
    Shells,
}

/// Follow-up reads requested by event handling.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    SessionSync(String),
    MessagesSync(String),
    PendingSync(String),
    ActiveSync,
    LocationSync,
    Catalog(String, Catalog),
    MessageGet(String, String),
}

#[derive(Clone, Debug, Default)]
pub struct LocationData {
    pub info: Option<LocationInfo>,
    pub vcs: Option<Value>,
    pub agents: Option<Vec<AgentInfo>>,
    pub commands: Option<Vec<CommandInfo>>,
    pub models: Option<Vec<ModelInfo>>,
    pub providers: Option<Vec<ProviderInfo>>,
    pub references: Option<Vec<ReferenceInfo>>,
    pub skills: Option<Vec<SkillInfo>>,
    pub mcp: Option<Vec<McpServer>>,
    pub integrations: Option<Vec<Value>>,
    pub shells: Vec<ShellInfo>,
}

#[derive(Clone, Debug, Default)]
pub struct Store {
    pub directory: String,
    pub sessions: HashMap<String, SessionInfo>,
    pub family: HashMap<String, Vec<String>>,
    pub active: HashMap<String, bool>,
    pub messages: HashMap<String, Vec<Message>>,
    pub message_cursor: HashMap<String, Option<String>>,
    pub pending: HashMap<String, Vec<InboxItem>>,
    pub permissions: HashMap<String, Vec<PermissionRequest>>,
    pub forms: HashMap<String, Vec<(FormInfo, Option<LocationRef>)>>,
    pub locations: HashMap<String, LocationData>,
    /// Optimistic admissions awaiting their durable echo.
    pub outbox: HashSet<String>,
    /// Loaded resource markers (`session:<id>`, `session.message:<id>`, ...).
    pub loaded: HashSet<String>,
    /// Shells observed per location.
    pub generation: u64,
}

impl Store {
    pub fn new(directory: &str) -> Store {
        Store { directory: directory.to_string(), ..Default::default() }
    }

    pub fn location(&self, dir: Option<&str>) -> Option<&LocationData> {
        self.locations.get(dir.unwrap_or(&self.directory))
    }

    pub fn location_mut(&mut self, dir: &str) -> &mut LocationData {
        self.locations.entry(dir.to_string()).or_default()
    }

    pub fn session(&self, id: &str) -> Option<&SessionInfo> {
        self.sessions.get(id)
    }

    /// Sessions sorted by `time.updated` desc.
    pub fn session_list(&self) -> Vec<&SessionInfo> {
        let mut v: Vec<&SessionInfo> = self.sessions.values().collect();
        v.sort_by(|a, b| b.time.updated.cmp(&a.time.updated).then(a.id.cmp(&b.id)));
        v
    }

    pub fn messages(&self, id: &str) -> &[Message] {
        self.messages.get(id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn status(&self, id: &str) -> &'static str {
        if self.active.get(id).copied().unwrap_or(false) { "running" } else { "idle" }
    }

    pub fn is_running(&self, id: &str) -> bool {
        self.active.get(id).copied().unwrap_or(false)
    }

    pub fn root(&self, id: &str) -> String {
        let mut current = id.to_string();
        let mut parent = self.sessions.get(id).and_then(|s| s.parent_id.clone());
        let mut seen: HashSet<String> = HashSet::from([id.to_string()]);
        while let Some(p) = parent {
            if !seen.insert(p.clone()) {
                break;
            }
            current = p.clone();
            parent = self.sessions.get(&p).and_then(|s| s.parent_id.clone());
        }
        current
    }

    pub fn family_of(&self, id: &str) -> Vec<String> {
        self.family.get(&self.root(id)).cloned().unwrap_or_default()
    }

    fn register(&mut self, id: &str) {
        if !self.sessions.contains_key(id) {
            return;
        }
        let root = self.root(id);
        if id != root
            && let Some(orphans) = self.family.remove(id)
        {
            let members = self.family.entry(root.clone()).or_default();
            for o in orphans {
                if !members.contains(&o) {
                    members.push(o);
                }
            }
        }
        let fam = self.family.entry(root).or_default();
        if !fam.iter().any(|x| x == id) {
            fam.push(id.to_string());
        }
    }

    pub fn remember(&mut self, info: SessionInfo) {
        let id = info.id.clone();
        self.sessions.insert(id.clone(), info);
        self.loaded.insert(format!("session:{id}"));
        self.register(&id);
    }

    pub fn remove_session(&mut self, id: &str) {
        for item in self.pending.get(id).cloned().unwrap_or_default() {
            self.outbox.remove(&item.id);
        }
        self.sessions.remove(id);
        self.active.remove(id);
        self.messages.remove(id);
        self.message_cursor.remove(id);
        self.pending.remove(id);
        self.permissions.remove(id);
        self.forms.remove(id);
        self.loaded.retain(|k| !k.ends_with(&format!(":{id}")));
        let mut empty = Vec::new();
        for (root, fam) in self.family.iter_mut() {
            fam.retain(|x| x != id);
            if fam.is_empty() {
                empty.push(root.clone());
            }
        }
        for r in empty {
            self.family.remove(&r);
        }
    }

    /// Clear heavy cached data for a session (retention eviction).
    pub fn evict(&mut self, id: &str) {
        let keep_pending: Vec<InboxItem> =
            self.pending.get(id).map(|p| p.iter().filter(|i| self.outbox.contains(&i.id)).cloned().collect()).unwrap_or_default();
        let keep_msgs: Vec<Message> =
            self.messages.get(id).map(|p| p.iter().filter(|i| self.outbox.contains(&i.id)).cloned().collect()).unwrap_or_default();
        self.messages.remove(id);
        self.message_cursor.remove(id);
        self.pending.remove(id);
        self.loaded.remove(&format!("session.message:{id}"));
        self.loaded.remove(&format!("session.pending:{id}"));
        if !keep_msgs.is_empty() {
            self.messages.insert(id.to_string(), keep_msgs);
        }
        if !keep_pending.is_empty() {
            self.pending.insert(id.to_string(), keep_pending);
        }
    }

    // ---- message helpers ---------------------------------------------------

    fn msgs(&mut self, sid: &str) -> &mut Vec<Message> {
        self.messages.entry(sid.to_string()).or_default()
    }

    fn insert_message(&mut self, sid: &str, m: Message) {
        let list = self.msgs(sid);
        if list.iter().any(|x| x.id == m.id) {
            return;
        }
        list.push(m);
    }

    fn edit_assistant(&mut self, sid: &str, mid: &str, f: impl FnOnce(&mut Message)) {
        if let Some(list) = self.messages.get_mut(sid)
            && let Some(m) = list.iter_mut().rev().find(|m| m.id == mid)
            && m.kind == "assistant"
        {
            f(m);
        }
    }

    fn edit_tool(&mut self, sid: &str, mid: &str, tid: &str, f: impl FnOnce(&mut ToolPart)) {
        self.edit_assistant(sid, mid, |m| {
            if let Some(Content::Tool(t)) =
                m.content.iter_mut().rev().find(|c| matches!(c, Content::Tool(t) if t.id == tid))
            {
                f(t);
            }
        });
    }

    fn edit_text(&mut self, sid: &str, mid: &str, f: impl FnOnce(&mut String)) {
        self.edit_assistant(sid, mid, |m| {
            if let Some(Content::Text { text, .. }) = m.content.iter_mut().rev().find(|c| matches!(c, Content::Text { .. })) {
                f(text);
            }
        });
    }

    fn edit_reasoning(&mut self, sid: &str, mid: &str, f: impl FnOnce(&mut String, &mut Option<Value>, &mut Option<PartTime>)) {
        self.edit_assistant(sid, mid, |m| {
            if let Some(Content::Reasoning { text, state, time }) = m.content.iter_mut().rev().find(|c| {
                matches!(c, Content::Reasoning { time, .. } if time.as_ref().and_then(|t| t.completed).is_none())
            }) {
                f(text, state, time);
            }
        });
    }

    fn active_assistant(list: &mut [Message]) -> Option<&mut Message> {
        list.iter_mut().rev().find(|m| m.kind == "assistant" && m.time.completed.is_none())
    }

    fn remove_pending(&mut self, sid: &str, inbox: &str) {
        if let Some(p) = self.pending.get_mut(sid) {
            p.retain(|i| i.id != inbox);
        }
    }

    pub fn pending_has_input(&self, sid: &str, inbox: &str) -> bool {
        self.pending.get(sid).is_some_and(|p| p.iter().any(|i| i.id == inbox && i.kind != "compaction"))
    }

    fn materialize(&mut self, item: &InboxItem) {
        if item.kind != "user" && item.kind != "synthetic" {
            return;
        }
        let mut obj = item.payload.as_object().cloned().unwrap_or_default();
        obj.insert("id".into(), json!(item.id));
        obj.insert("type".into(), json!(item.kind));
        obj.insert("time".into(), json!({ "created": item.time.created }));
        let Some(row) = Message::from_value(Value::Object(obj)) else { return };
        let list = self.msgs(&item.session_id);
        match list.iter().position(|m| m.id == row.id) {
            Some(i) => list[i] = row,
            None => list.push(row),
        }
    }

    /// Upsert an admitted inbox item into pending and the transcript.
    pub fn admit_local(&mut self, item: InboxItem) {
        let p = self.pending.entry(item.session_id.clone()).or_default();
        match p.iter().position(|e| e.id == item.id) {
            Some(i) => p[i] = item.clone(),
            None => p.push(item.clone()),
        }
        if item.kind == "compaction" {
            return;
        }
        self.materialize(&item);
    }

    /// Remove an inbox item from pending and the visible transcript.
    pub fn retract_local(&mut self, sid: &str, inbox: &str) {
        self.remove_pending(sid, inbox);
        if let Some(list) = self.messages.get_mut(sid) {
            list.retain(|m| m.id != inbox);
        }
    }

    /// `pending.status`: where a pending user prompt stands.
    pub fn pending_status(&self, sid: &str, inbox: &str) -> Option<&'static str> {
        let pending = self.pending.get(sid)?;
        let item = pending.iter().find(|e| e.id == inbox && e.kind == "user")?;
        if item.delivery == "queue" {
            return Some("queued");
        }
        let messages = self.messages(sid);
        let boundary = messages.iter().rposition(|m| m.kind == "idle");
        if !self.is_running(sid) {
            return Some(match boundary {
                Some(b) if item.time.created <= messages[b].time.created => "stranded",
                _ => "starting",
            });
        }
        let start = boundary.map(|b| b + 1).unwrap_or(0);
        let steering = messages[start..]
            .iter()
            .any(|m| m.kind == "user" && !pending.iter().any(|o| o.id == m.id));
        Some(if steering { "steering" } else { "starting" })
    }

    // ---- read results ------------------------------------------------------

    pub fn apply_messages(&mut self, sid: &str, mut fetched: Vec<Message>, next: Option<String>) {
        fetched.reverse();
        let ids: HashSet<String> = fetched.iter().map(|m| m.id.clone()).collect();
        let admitted: HashSet<String> = self
            .pending
            .get(sid)
            .map(|p| p.iter().filter(|i| i.kind == "user" || i.kind == "synthetic").map(|i| i.id.clone()).collect())
            .unwrap_or_default();
        let local: Vec<Message> = self
            .messages(sid)
            .iter()
            .filter(|m| !ids.contains(&m.id) && (self.outbox.contains(&m.id) || admitted.contains(&m.id)))
            .cloned()
            .collect();
        fetched.extend(local);
        self.messages.insert(sid.to_string(), fetched);
        self.message_cursor.insert(sid.to_string(), next);
        self.loaded.insert(format!("session.message:{sid}"));
    }

    /// Prepend an older page of messages (`loadMore`).
    pub fn prepend_messages(&mut self, sid: &str, mut older: Vec<Message>, next: Option<String>) {
        older.reverse();
        let existing = self.messages.remove(sid).unwrap_or_default();
        let ids: HashSet<String> = existing.iter().map(|m| m.id.clone()).collect();
        older.retain(|m| !ids.contains(&m.id));
        older.extend(existing);
        self.messages.insert(sid.to_string(), older);
        self.message_cursor.insert(sid.to_string(), next);
    }

    pub fn apply_pending(&mut self, sid: &str, snapshot: Vec<InboxItem>) {
        for i in &snapshot {
            self.outbox.remove(&i.id);
        }
        let inflight: Vec<InboxItem> = self
            .pending
            .get(sid)
            .map(|p| p.iter().filter(|i| self.outbox.contains(&i.id)).cloned().collect())
            .unwrap_or_default();
        let mut merged = snapshot;
        merged.extend(inflight);
        self.pending.insert(sid.to_string(), merged.clone());
        for item in merged {
            self.materialize(&item);
        }
        self.loaded.insert(format!("session.pending:{sid}"));
    }

    pub fn apply_active(&mut self, active: &Value) {
        self.active.clear();
        if let Some(o) = active.as_object() {
            for k in o.keys() {
                self.active.insert(k.clone(), true);
            }
        }
    }

    // ---- events ------------------------------------------------------------

    /// Apply one server event. Returns follow-up reads.
    pub fn apply(&mut self, ev: &Event) -> Vec<Effect> {
        self.generation += 1;
        let mut fx = Vec::new();
        let sid = ev.session_id().unwrap_or("").to_string();
        let d = &ev.data;
        let s = |k: &str| d.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        match ev.kind.as_str() {
            "server.connected" => {
                fx.push(Effect::ActiveSync);
                fx.push(Effect::LocationSync);
                fx.push(Effect::Catalog(self.directory.clone(), Catalog::Vcs));
            }
            "session.created" => {
                self.outbox.remove(&sid);
                fx.push(Effect::SessionSync(sid.clone()));
                self.loaded.insert(format!("session.pending:{sid}"));
                self.loaded.insert(format!("session.message:{sid}"));
            }
            "session.deleted" => self.remove_session(&sid),
            "session.usage.updated" => {
                if let Some(info) = self.sessions.get_mut(&sid) {
                    info.cost = d.get("cost").and_then(|v| v.as_f64()).unwrap_or(info.cost);
                    if let Some(t) = d.get("tokens").and_then(|t| serde_json::from_value(t.clone()).ok()) {
                        info.tokens = t;
                    }
                }
            }
            "session.agent.selected" => {
                let previous = self.sessions.get(&sid).and_then(|s| s.agent.clone());
                if let Some(info) = self.sessions.get_mut(&sid) {
                    info.agent = Some(s("agent"));
                }
                let m = Message {
                    id: ev.message_id(),
                    kind: "agent-switched".into(),
                    agent: Some(s("agent")),
                    previous: previous.map(Value::String),
                    time: MessageTime { created: ev.created, ..Default::default() },
                    ..Default::default()
                };
                self.insert_message(&sid, m);
            }
            "session.model.selected" => {
                let model: Option<ModelRef> = d.get("model").and_then(|m| serde_json::from_value(m.clone()).ok());
                if let Some(info) = self.sessions.get_mut(&sid) {
                    info.model = model.clone();
                }
                if !self.messages.contains_key(&sid) {
                    return fx;
                }
                let m = Message {
                    id: ev.message_id(),
                    kind: "model-switched".into(),
                    model,
                    time: MessageTime { created: ev.created, ..Default::default() },
                    ..Default::default()
                };
                self.insert_message(&sid, m);
                fx.push(Effect::MessageGet(sid.clone(), ev.message_id()));
            }
            "session.renamed" => {
                if let Some(info) = self.sessions.get_mut(&sid) {
                    info.title = Some(s("title"));
                } else {
                    fx.push(Effect::SessionSync(sid.clone()));
                }
            }
            "session.permissions" => {}
            "session.moved" => {
                if let Some(current) = self.sessions.get(&sid).cloned() {
                    let location: LocationRef = d.get("location").and_then(|l| serde_json::from_value(l.clone()).ok()).unwrap_or_default();
                    let previous = json!({
                        "location": current.location,
                        "projectID": current.project_id,
                        "subpath": current.subpath,
                    });
                    let info = self.sessions.get_mut(&sid).unwrap();
                    info.location = location.clone();
                    if let Some(p) = d.get("projectID").and_then(|p| p.as_str()) {
                        info.project_id = p.to_string();
                    }
                    info.subpath = d.get("subpath").and_then(|p| p.as_str()).map(|s| s.to_string());
                    let m = Message {
                        id: ev.message_id(),
                        kind: "location-switched".into(),
                        location: Some(location),
                        previous: Some(previous),
                        time: MessageTime { created: ev.created, ..Default::default() },
                        ..Default::default()
                    };
                    self.insert_message(&sid, m);
                }
            }
            "session.inbox.delivered" => {
                let inbox = s("inboxID");
                let admitted = self.pending_has_input(&sid, &inbox);
                self.remove_pending(&sid, &inbox);
                if admitted && let Some(list) = self.messages.get_mut(&sid) && let Some(pos) = list.iter().position(|m| m.id == inbox) {
                    let mut m = list.remove(pos);
                    m.time.created = ev.created;
                    list.push(m);
                }
            }
            "session.inbox.delivery.changed" => {
                let inbox = s("inboxID");
                let delivery = s("delivery");
                if let Some(p) = self.pending.get_mut(&sid)
                    && let Some(i) = p.iter_mut().find(|i| i.id == inbox)
                {
                    i.delivery = delivery;
                }
            }
            "session.inbox.cancelled" => {
                let inbox = s("inboxID");
                self.retract_local(&sid, &inbox);
            }
            "session.inbox.enqueued" => {
                let inbox = s("inboxID");
                self.outbox.remove(&inbox);
                let item_v = d.get("item").cloned().unwrap_or(Value::Null);
                let item = InboxItem {
                    id: inbox,
                    session_id: sid.clone(),
                    time: MessageTime { created: ev.created, ..Default::default() },
                    kind: item_v.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    payload: item_v.get("payload").cloned().unwrap_or(json!({})),
                    delivery: item_v.get("delivery").and_then(|v| v.as_str()).unwrap_or("steer").to_string(),
                };
                if item.kind == "compaction" {
                    // Retire speculative compaction rows that coalesced to another id.
                    let speculative: Vec<String> = self
                        .pending
                        .get(&sid)
                        .map(|p| p.iter().filter(|i| i.kind == "compaction" && i.id != item.id && self.outbox.contains(&i.id)).map(|i| i.id.clone()).collect())
                        .unwrap_or_default();
                    for id in speculative {
                        self.outbox.remove(&id);
                        self.remove_pending(&sid, &id);
                    }
                }
                self.admit_local(item);
            }
            "session.instructions.updated" => {
                let Some(text) = d.get("text").and_then(|t| t.as_str()) else { return fx };
                let keys: Vec<String> = d.get("delta").and_then(|v| v.as_object()).map(|o| o.keys().cloned().collect()).unwrap_or_default();
                let m = Message {
                    id: ev.message_id(),
                    kind: "system".into(),
                    text: Some(text.to_string()),
                    description: Some(format!("Instructions updated: {}", keys.join(", "))),
                    metadata: ev.metadata.clone(),
                    time: MessageTime { created: ev.created, ..Default::default() },
                    ..Default::default()
                };
                self.insert_message(&sid, m);
            }
            "session.synthetic" => {
                let m = Message {
                    id: ev.message_id(),
                    kind: "synthetic".into(),
                    text: Some(s("text")),
                    description: d.get("description").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    metadata: d.get("metadata").cloned(),
                    time: MessageTime { created: ev.created, ..Default::default() },
                    ..Default::default()
                };
                self.insert_message(&sid, m);
            }
            "session.skill.activated" => {
                let m = Message {
                    id: ev.message_id(),
                    kind: "skill".into(),
                    skill: Some(s("id")),
                    name: Some(s("name")),
                    text: Some(s("text")),
                    time: MessageTime { created: ev.created, ..Default::default() },
                    ..Default::default()
                };
                self.insert_message(&sid, m);
            }
            "session.shell.started" => {
                let shell = d.get("shell").cloned().unwrap_or(Value::Null);
                let background = shell.pointer("/metadata/background") == Some(&Value::Bool(true));
                let mut metadata = ev.metadata.clone();
                if background {
                    let mut o = metadata.and_then(|m| m.as_object().cloned()).unwrap_or_default();
                    o.insert("background".into(), Value::Bool(true));
                    metadata = Some(Value::Object(o));
                }
                let m = Message {
                    id: ev.message_id(),
                    kind: "shell".into(),
                    shell_id: shell.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    command: shell.get("command").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    status: shell.get("status").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    exit: shell.get("exit").and_then(|v| v.as_i64()),
                    metadata,
                    time: MessageTime { created: ev.created, ..Default::default() },
                    ..Default::default()
                };
                self.insert_message(&sid, m);
            }
            "session.shell.ended" => {
                let shell = d.get("shell").cloned().unwrap_or(Value::Null);
                let shell_id = shell.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if let Some(list) = self.messages.get_mut(&sid)
                    && let Some(m) = list.iter_mut().rev().find(|m| m.kind == "shell" && m.shell_id.as_deref() == Some(&shell_id))
                {
                    m.status = shell.get("status").and_then(|v| v.as_str()).map(|s| s.to_string());
                    m.exit = shell.get("exit").and_then(|v| v.as_i64());
                    m.output = d.get("output").and_then(|o| serde_json::from_value(o.clone()).ok());
                    m.time.completed = Some(ev.created);
                }
            }
            "session.step.started" => {
                let mid = s("assistantMessageID");
                let agent = s("agent");
                let model: Option<ModelRef> = d.get("model").and_then(|m| serde_json::from_value(m.clone()).ok());
                let started = d.get("started").and_then(|v| v.as_i64()).unwrap_or(ev.created);
                let list = self.msgs(&sid);
                if let Some(existing) = list.iter_mut().find(|m| m.id == mid && m.kind == "assistant") {
                    existing.agent = Some(agent);
                    existing.model = model;
                    existing.retry = None;
                    existing.error = None;
                    existing.finish = None;
                    existing.time.created = started;
                    existing.time.streamed = None;
                    existing.time.completed = None;
                    return fx;
                }
                if let Some(cur) = Self::active_assistant(list) {
                    cur.retry = None;
                    cur.time.completed = Some(ev.created);
                }
                if !list.iter().any(|m| m.id == mid) {
                    list.push(Message {
                        id: mid,
                        kind: "assistant".into(),
                        agent: Some(agent),
                        model,
                        metadata: ev.metadata.clone(),
                        time: MessageTime { created: started, ..Default::default() },
                        ..Default::default()
                    });
                }
            }
            "session.step.streamed" => {
                let c = ev.created;
                self.edit_assistant(&sid, &s("assistantMessageID"), |m| m.time.streamed = Some(c));
            }
            "session.step.ended" => {
                let c = ev.created;
                let d2 = d.clone();
                self.edit_assistant(&sid, &s("assistantMessageID"), move |m| {
                    m.time.completed = Some(c);
                    m.finish = d2.get("finish").and_then(|v| v.as_str()).map(|s| s.to_string());
                    m.cost = d2.get("cost").and_then(|v| v.as_f64());
                    m.tokens = d2.get("tokens").and_then(|t| serde_json::from_value(t.clone()).ok());
                });
            }
            "session.step.failed" => {
                let c = ev.created;
                let d2 = d.clone();
                self.edit_assistant(&sid, &s("assistantMessageID"), move |m| {
                    m.time.completed = Some(c);
                    m.finish = Some(d2.get("finish").and_then(|v| v.as_str()).unwrap_or("error").to_string());
                    m.error = d2.get("error").and_then(|e| serde_json::from_value(e.clone()).ok());
                    m.retry = None;
                    if let (Some(cost), Some(tokens)) = (d2.get("cost").and_then(|v| v.as_f64()), d2.get("tokens")) {
                        m.cost = Some(cost);
                        m.tokens = serde_json::from_value(tokens.clone()).ok();
                    }
                });
            }
            "session.text.started" => {
                self.edit_assistant(&sid, &s("assistantMessageID"), |m| {
                    m.content.push(Content::Text { text: String::new(), state: None })
                });
            }
            "session.text.delta" => {
                let delta = s("delta");
                self.edit_text(&sid, &s("assistantMessageID"), |t| t.push_str(&delta));
            }
            "session.text.ended" => {
                let text = s("text");
                self.edit_text(&sid, &s("assistantMessageID"), |t| *t = text);
            }
            "session.tool.input.started" => {
                let (id, name, c) = (s("id"), s("name"), ev.created);
                self.edit_assistant(&sid, &s("assistantMessageID"), |m| {
                    m.content.push(Content::Tool(ToolPart {
                        id,
                        name,
                        executed: None,
                        state: ToolState { status: "streaming".into(), input: Value::String(String::new()), ..Default::default() },
                        time: ToolTime { created: c, ..Default::default() },
                    }))
                });
            }
            "session.tool.input.delta" => {
                let delta = s("delta");
                self.edit_tool(&sid, &s("assistantMessageID"), &s("id"), |t| {
                    if t.state.status == "streaming" {
                        let cur = t.state.input.as_str().unwrap_or("").to_string();
                        t.state.input = Value::String(cur + &delta);
                    }
                });
            }
            "session.tool.input.ended" => {
                let text = s("text");
                self.edit_tool(&sid, &s("assistantMessageID"), &s("id"), |t| {
                    if t.state.status == "streaming" {
                        t.state.input = Value::String(text);
                    }
                });
            }
            "session.tool.called" => {
                let c = ev.created;
                let input = d.get("input").cloned().unwrap_or(json!({}));
                let executed = d.get("executed").and_then(|v| v.as_bool());
                self.edit_tool(&sid, &s("assistantMessageID"), &s("id"), move |t| {
                    t.time.ran = Some(c);
                    t.executed = executed;
                    t.state = ToolState { status: "running".into(), input, metadata: Some(json!({})), ..Default::default() };
                });
            }
            "session.tool.progress" => {
                let meta = d.get("metadata").cloned();
                self.edit_tool(&sid, &s("assistantMessageID"), &s("id"), move |t| {
                    if t.state.status == "running" {
                        t.state.metadata = meta;
                    }
                });
            }
            "session.tool.success" => {
                let c = ev.created;
                let d2 = d.clone();
                self.edit_tool(&sid, &s("assistantMessageID"), &s("id"), move |t| {
                    if t.state.status != "running" {
                        return;
                    }
                    t.state = ToolState {
                        status: "completed".into(),
                        input: t.state.input.clone(),
                        metadata: d2.get("metadata").cloned(),
                        content: d2.get("content").and_then(|c| serde_json::from_value(c.clone()).ok()),
                        error: None,
                    };
                    let executed = d2.get("executed").and_then(|v| v.as_bool()).unwrap_or(false);
                    t.executed = Some(executed || t.executed == Some(true));
                    t.time.completed = Some(c);
                });
            }
            "session.tool.failed" => {
                let c = ev.created;
                let d2 = d.clone();
                self.edit_tool(&sid, &s("assistantMessageID"), &s("id"), move |t| {
                    if t.state.status != "streaming" && t.state.status != "running" {
                        return;
                    }
                    let input = if t.state.input.is_string() { json!({}) } else { t.state.input.clone() };
                    t.state = ToolState {
                        status: "error".into(),
                        input,
                        metadata: d2.get("metadata").cloned(),
                        content: d2.get("content").and_then(|c| serde_json::from_value(c.clone()).ok()),
                        error: d2.get("error").and_then(|e| serde_json::from_value(e.clone()).ok()),
                    };
                    let executed = d2.get("executed").and_then(|v| v.as_bool()).unwrap_or(false);
                    t.executed = Some(executed || t.executed == Some(true));
                    t.time.completed = Some(c);
                });
            }
            "session.reasoning.started" => {
                let c = ev.created;
                let state = d.get("state").cloned();
                self.edit_assistant(&sid, &s("assistantMessageID"), move |m| {
                    m.content.push(Content::Reasoning {
                        text: String::new(),
                        state,
                        time: Some(PartTime { created: c, completed: None }),
                    })
                });
            }
            "session.reasoning.delta" => {
                let delta = s("delta");
                self.edit_reasoning(&sid, &s("assistantMessageID"), |t, _, _| t.push_str(&delta));
            }
            "session.reasoning.ended" => {
                let (text, c) = (s("text"), ev.created);
                let st = d.get("state").cloned();
                self.edit_reasoning(&sid, &s("assistantMessageID"), move |t, state, time| {
                    *t = text;
                    let created = time.as_ref().map(|t| t.created).unwrap_or(c);
                    *time = Some(PartTime { created, completed: Some(c) });
                    if st.is_some() {
                        *state = st;
                    }
                });
            }
            "session.retry.scheduled" => {
                let r = Retry {
                    attempt: d.get("attempt").and_then(|v| v.as_i64()).unwrap_or(0),
                    at: d.get("at").and_then(|v| v.as_i64()).unwrap_or(0),
                    error: d.get("error").and_then(|e| serde_json::from_value(e.clone()).ok()).unwrap_or_default(),
                };
                self.edit_assistant(&sid, &s("assistantMessageID"), move |m| m.retry = Some(r));
            }
            "session.execution.started" => {
                self.active.insert(sid.clone(), true);
            }
            "session.compaction.started" => {
                let input = d.get("inputID").and_then(|v| v.as_str()).map(|s| s.to_string());
                if let Some(i) = &input {
                    self.remove_pending(&sid, i);
                }
                let m = Message {
                    id: input.unwrap_or_else(|| ev.message_id()),
                    kind: "compaction".into(),
                    status: Some("running".into()),
                    reason: d.get("reason").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    summary: Some(String::new()),
                    time: MessageTime { created: ev.created, ..Default::default() },
                    ..Default::default()
                };
                self.insert_message(&sid, m);
            }
            "session.execution.succeeded" | "session.execution.failed" | "session.execution.interrupted" => {
                self.active.insert(sid.clone(), false);
                if let Some(list) = self.messages.get_mut(&sid)
                    && let Some(cur) = Self::active_assistant(list)
                {
                    cur.retry = None;
                }
                if ev.kind == "session.execution.interrupted" && s("reason") == "shutdown" {
                    return fx;
                }
                let outcome = match ev.kind.as_str() {
                    "session.execution.succeeded" => "succeeded",
                    "session.execution.failed" => "failed",
                    _ => "interrupted",
                };
                let m = Message {
                    id: ev.message_id(),
                    kind: "idle".into(),
                    outcome: Some(outcome.into()),
                    error: if outcome == "failed" { d.get("error").and_then(|e| serde_json::from_value(e.clone()).ok()) } else { None },
                    time: MessageTime { created: ev.created, ..Default::default() },
                    ..Default::default()
                };
                self.insert_message(&sid, m);
                let stuck = self.messages(&sid).iter().any(|m| {
                    m.kind == "assistant"
                        && m.content.iter().any(|c| matches!(c, Content::Tool(t) if t.state.status == "streaming" || t.state.status == "running"))
                });
                if stuck {
                    fx.push(Effect::MessagesSync(sid.clone()));
                }
                if self.sessions.contains_key(&sid) {
                    fx.push(Effect::SessionSync(sid.clone()));
                }
            }
            "session.viewed" => {
                if self.sessions.contains_key(&sid) {
                    fx.push(Effect::SessionSync(sid.clone()));
                }
            }
            "session.revert.staged" => {
                if let Some(info) = self.sessions.get_mut(&sid) {
                    info.revert = d.get("revert").and_then(|r| serde_json::from_value(r.clone()).ok());
                }
            }
            "session.revert.cleared" => {
                if let Some(info) = self.sessions.get_mut(&sid) {
                    info.revert = None;
                }
            }
            "session.revert.committed" => {
                let to = s("to");
                if let Some(info) = self.sessions.get_mut(&sid) {
                    info.revert = None;
                }
                if let Some(p) = self.pending.get_mut(&sid) {
                    p.retain(|i| i.id < to);
                }
                if let Some(list) = self.messages.get_mut(&sid)
                    && let Some(pos) = list.iter().position(|m| m.id >= to)
                {
                    list.truncate(pos);
                }
            }
            "session.compaction.delta" => {
                let text = s("text");
                if let Some(list) = self.messages.get_mut(&sid)
                    && let Some(m) = list.iter_mut().rev().find(|m| m.kind == "compaction" && m.status.as_deref() == Some("running"))
                {
                    m.summary.get_or_insert_default().push_str(&text);
                }
            }
            "session.compaction.ended" => {
                let list = self.msgs(&sid);
                let summary = d.get("text").and_then(|v| v.as_str()).map(|s| s.to_string());
                let tokens: Option<TokenUsage> = d.get("tokens").and_then(|t| serde_json::from_value(t.clone()).ok());
                let reason = d.get("reason").and_then(|v| v.as_str()).map(|s| s.to_string());
                let ctx = d.get("providerContext").cloned();
                if let Some(m) = list.iter_mut().rev().find(|m| m.kind == "compaction" && m.status.as_deref() == Some("running")) {
                    m.status = Some("completed".into());
                    m.summary = summary;
                    m.tokens = tokens;
                    m.reason = reason;
                    m.provider_context = ctx;
                    m.cost = d.get("cost").and_then(|v| v.as_f64());
                } else {
                    list.push(Message {
                        id: ev.message_id(),
                        kind: "compaction".into(),
                        status: Some("completed".into()),
                        summary,
                        tokens,
                        reason,
                        provider_context: ctx,
                        time: MessageTime { created: ev.created, ..Default::default() },
                        ..Default::default()
                    });
                }
            }
            "session.compaction.failed" => {
                let input = d.get("inputID").and_then(|v| v.as_str()).map(|s| s.to_string());
                if let Some(i) = &input {
                    self.remove_pending(&sid, i);
                }
                let error: SessionError = d.get("error").and_then(|e| serde_json::from_value(e.clone()).ok()).unwrap_or(SessionError {
                    kind: "compaction.failed".into(),
                    message: "Compaction failed before recording an error".into(),
                    status: None,
                });
                let reason = d.get("reason").and_then(|v| v.as_str()).unwrap_or("manual").to_string();
                let created = ev.created;
                let mid = ev.message_id();
                let list = self.msgs(&sid);
                if let Some(m) = list.iter_mut().rev().find(|m| m.kind == "compaction" && m.status.as_deref() == Some("running")) {
                    m.status = Some("failed".into());
                    m.error = Some(error);
                    m.reason = Some(reason);
                } else {
                    list.push(Message {
                        id: input.unwrap_or(mid),
                        kind: "compaction".into(),
                        status: Some("failed".into()),
                        error: Some(error),
                        reason: Some(reason),
                        time: MessageTime { created, ..Default::default() },
                        ..Default::default()
                    });
                }
            }
            "permission.asked" => {
                if let Ok(req) = serde_json::from_value::<PermissionRequest>(d.clone()) {
                    let list = self.permissions.entry(req.session_id.clone()).or_default();
                    if !list.iter().any(|r| r.id == req.id) {
                        list.push(req);
                    }
                }
            }
            "permission.replied" => {
                let rid = s("requestID");
                if let Some(list) = self.permissions.get_mut(&sid) {
                    list.retain(|r| r.id != rid);
                }
            }
            "form.replied" | "form.cancelled" => {
                let fid = s("id");
                self.remove_form(&sid, &fid);
            }
            "form.created" => {
                if let Some(form) = d.get("form").and_then(|f| serde_json::from_value::<FormInfo>(f.clone()).ok()) {
                    let loc = if form.session_id == "global" { ev.location.clone() } else { None };
                    let list = self.forms.entry(form.session_id.clone()).or_default();
                    if !list.iter().any(|(f, _)| f.id == form.id) {
                        list.push((form, loc));
                    }
                }
            }
            "project.updated" => {}
            _ => {}
        }
        // Location-scoped catalog invalidations.
        if let Some(loc) = &ev.location {
            let dir = loc.directory.clone();
            let cat = match ev.kind.as_str() {
                "provider.updated" => vec![Catalog::Providers],
                "model.updated" => vec![Catalog::Models],
                "agent.updated" => vec![Catalog::Agents],
                "command.updated" => vec![Catalog::Commands],
                "skill.updated" => vec![Catalog::Skills],
                "reference.updated" => vec![Catalog::References],
                "integration.updated" => vec![Catalog::Integrations, Catalog::Models, Catalog::Providers],
                "mcp.status.changed" => vec![Catalog::Mcp],
                _ => vec![],
            };
            for c in cat {
                fx.push(Effect::Catalog(dir.clone(), c));
            }
            match ev.kind.as_str() {
                "vcs.branch.updated" => {
                    let branch = d.get("branch").cloned().unwrap_or(Value::Null);
                    let ld = self.location_mut(&dir);
                    let mut vcs = ld.vcs.take().unwrap_or(json!({}));
                    if !vcs.get("branch").is_some_and(|b| b.is_object()) {
                        vcs["branch"] = json!({});
                    }
                    vcs["branch"]["current"] = branch;
                    ld.vcs = Some(vcs);
                }
                "shell.created" => {
                    if let Some(info) = d.get("info").and_then(|i| serde_json::from_value::<ShellInfo>(i.clone()).ok()) {
                        let ld = self.location_mut(&dir);
                        ld.shells.retain(|s| s.id != info.id);
                        ld.shells.push(info);
                    }
                }
                "shell.exited" | "shell.deleted" => {
                    let id = s("id");
                    self.location_mut(&dir).shells.retain(|s| s.id != id);
                }
                _ => {}
            }
        }
        if matches!(ev.kind.as_str(), "credential.updated" | "credential.switched") {
            let dirs: Vec<String> = self.locations.keys().cloned().collect();
            for dir in dirs {
                fx.push(Effect::Catalog(dir.clone(), Catalog::Integrations));
                fx.push(Effect::Catalog(dir.clone(), Catalog::Models));
                fx.push(Effect::Catalog(dir, Catalog::Providers));
            }
        }
        fx
    }

    pub fn remove_form(&mut self, sid: &str, fid: &str) {
        if let Some(list) = self.forms.get_mut(sid) {
            list.retain(|(f, _)| f.id != fid);
        }
    }

    /// Pending permission requests for a session family (first = shown).
    pub fn permissions_for(&self, sid: &str) -> Vec<&PermissionRequest> {
        self.permissions.get(sid).map(|v| v.iter().collect()).unwrap_or_default()
    }

    pub fn forms_for(&self, sid: &str) -> Vec<&FormInfo> {
        self.forms.get(sid).map(|v| v.iter().map(|(f, _)| f).collect()).unwrap_or_default()
    }

    /// Global (MCP elicitation) forms plus home-route forms for the default location.
    pub fn global_forms(&self) -> Vec<(&FormInfo, Option<&LocationRef>)> {
        self.forms.get("global").map(|v| v.iter().map(|(f, l)| (f, l.as_ref())).collect()).unwrap_or_default()
    }

    /// Optimistic prompt admission (`data.session.prompt`).
    pub fn admit_prompt(&mut self, sid: &str, id: &str, text: &str, agents: Value, delivery: &str) {
        if self.messages(sid).iter().any(|m| m.id == id) {
            return;
        }
        self.outbox.insert(id.to_string());
        let mut payload = json!({ "text": text });
        if agents.as_array().is_some_and(|a| !a.is_empty()) {
            payload["agents"] = agents;
        }
        self.admit_local(InboxItem {
            id: id.to_string(),
            session_id: sid.to_string(),
            time: MessageTime { created: crate::util::locale::now_ms(), ..Default::default() },
            kind: "user".into(),
            payload,
            delivery: delivery.to_string(),
        });
    }

    /// Roll back an unacknowledged optimistic admission.
    pub fn rollback_prompt(&mut self, sid: &str, id: &str) {
        if self.outbox.remove(id) {
            self.retract_local(sid, id);
        }
    }

    /// Optimistic session creation.
    pub fn admit_session(&mut self, id: &str, location: LocationRef, agent: Option<String>, model: Option<ModelRef>) {
        if self.sessions.contains_key(id) {
            return;
        }
        let now = crate::util::locale::now_ms();
        let project = self.location(Some(&location.directory)).and_then(|l| l.info.as_ref()).map(|i| i.project.id.clone()).unwrap_or_default();
        self.outbox.insert(id.to_string());
        self.remember(SessionInfo {
            id: id.to_string(),
            project_id: project,
            agent,
            model,
            time: SessionTime { created: now, updated: now, ..Default::default() },
            location,
            ..Default::default()
        });
        self.loaded.insert(format!("session.family:{id}"));
        self.loaded.insert(format!("session.pending:{id}"));
        self.loaded.insert(format!("session.message:{id}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: &str, data: Value) -> Event {
        Event { id: format!("evt_{}", crate::util::id_body(false)), created: 1000, kind: kind.into(), data, ..Default::default() }
    }

    #[test]
    fn streaming_assistant() {
        let mut s = Store::new("/tmp");
        s.apply(&ev("session.step.started", json!({"sessionID":"ses_1","assistantMessageID":"msg_a","agent":"build","model":{"id":"m","providerID":"p"},"started":5})));
        s.apply(&ev("session.text.started", json!({"sessionID":"ses_1","assistantMessageID":"msg_a","ordinal":0})));
        s.apply(&ev("session.text.delta", json!({"sessionID":"ses_1","assistantMessageID":"msg_a","ordinal":0,"delta":"Hel"})));
        s.apply(&ev("session.text.delta", json!({"sessionID":"ses_1","assistantMessageID":"msg_a","ordinal":0,"delta":"lo"})));
        let m = &s.messages("ses_1")[0];
        assert!(matches!(&m.content[0], Content::Text { text, .. } if text == "Hello"));
        s.apply(&ev("session.tool.input.started", json!({"sessionID":"ses_1","assistantMessageID":"msg_a","id":"t1","name":"read"})));
        s.apply(&ev("session.tool.input.delta", json!({"sessionID":"ses_1","assistantMessageID":"msg_a","id":"t1","delta":"{\"pa"})));
        s.apply(&ev("session.tool.called", json!({"sessionID":"ses_1","assistantMessageID":"msg_a","id":"t1","input":{"path":"a.ts"},"executed":true})));
        s.apply(&ev("session.tool.success", json!({"sessionID":"ses_1","assistantMessageID":"msg_a","id":"t1","content":[{"type":"text","text":"x"}],"executed":true})));
        let m = &s.messages("ses_1")[0];
        let Content::Tool(t) = &m.content[1] else { panic!() };
        assert_eq!(t.state.status, "completed");
        assert_eq!(t.state.input["path"], "a.ts");
        s.apply(&ev("session.step.ended", json!({"sessionID":"ses_1","assistantMessageID":"msg_a","finish":"stop","cost":0.1,"tokens":{"input":1,"output":2,"reasoning":0,"cache":{"read":0,"write":0}}})));
        assert_eq!(s.messages("ses_1")[0].finish.as_deref(), Some("stop"));
        s.apply(&ev("session.execution.started", json!({"sessionID":"ses_1"})));
        assert!(s.is_running("ses_1"));
        s.apply(&ev("session.execution.succeeded", json!({"sessionID":"ses_1"})));
        assert!(!s.is_running("ses_1"));
        assert_eq!(s.messages("ses_1").last().unwrap().kind, "idle");
    }

    #[test]
    fn inbox_flow() {
        let mut s = Store::new("/tmp");
        s.admit_prompt("ses_1", "msg_1", "hi", json!([]), "steer");
        assert_eq!(s.messages("ses_1").len(), 1);
        assert!(s.outbox.contains("msg_1"));
        s.apply(&ev("session.inbox.enqueued", json!({"sessionID":"ses_1","inboxID":"msg_1","item":{"type":"user","payload":{"text":"hi"},"delivery":"steer"}})));
        assert!(!s.outbox.contains("msg_1"));
        assert_eq!(s.pending_status("ses_1", "msg_1"), Some("starting"));
        s.apply(&ev("session.inbox.delivered", json!({"sessionID":"ses_1","inboxID":"msg_1"})));
        assert!(s.pending["ses_1"].is_empty());
        assert_eq!(s.messages("ses_1").len(), 1);
        s.admit_prompt("ses_1", "msg_2", "again", json!([]), "queue");
        s.rollback_prompt("ses_1", "msg_2");
        assert_eq!(s.messages("ses_1").len(), 1);
    }

    #[test]
    fn permissions_forms() {
        let mut s = Store::new("/tmp");
        s.apply(&ev("permission.asked", json!({"id":"per_1","sessionID":"ses_1","action":"edit","resources":["a"]})));
        s.apply(&ev("permission.asked", json!({"id":"per_1","sessionID":"ses_1","action":"edit","resources":["a"]})));
        assert_eq!(s.permissions_for("ses_1").len(), 1);
        s.apply(&ev("permission.replied", json!({"requestID":"per_1","sessionID":"ses_1","reply":"once"})));
        assert!(s.permissions_for("ses_1").is_empty());
        let mut e = ev("form.created", json!({"form":{"id":"frm_1","sessionID":"ses_1","title":"Q","fields":[{"key":"a","type":"boolean"}]}}));
        e.location = Some(LocationRef { directory: "/tmp".into(), workspace_id: None });
        s.apply(&e);
        assert_eq!(s.forms_for("ses_1").len(), 1);
        s.apply(&ev("form.replied", json!({"id":"frm_1","sessionID":"ses_1","answer":{}})));
        assert!(s.forms_for("ses_1").is_empty());
    }

    #[test]
    fn revert_commit_truncates() {
        let mut s = Store::new("/tmp");
        s.apply_messages(
            "ses_1",
            vec![
                Message { id: "msg_3".into(), kind: "user".into(), ..Default::default() },
                Message { id: "msg_2".into(), kind: "user".into(), ..Default::default() },
                Message { id: "msg_1".into(), kind: "user".into(), ..Default::default() },
            ],
            None,
        );
        assert_eq!(s.messages("ses_1")[0].id, "msg_1");
        s.apply(&ev("session.revert.committed", json!({"sessionID":"ses_1","to":"msg_2"})));
        assert_eq!(s.messages("ses_1").len(), 1);
    }

    #[test]
    fn family_index() {
        let mut s = Store::new("/tmp");
        s.remember(SessionInfo { id: "ses_c".into(), parent_id: Some("ses_p".into()), ..Default::default() });
        assert_eq!(s.family_of("ses_c"), vec!["ses_c".to_string()]);
        s.remember(SessionInfo { id: "ses_p".into(), ..Default::default() });
        let mut fam = s.family_of("ses_c");
        fam.sort();
        assert_eq!(fam, vec!["ses_c".to_string(), "ses_p".to_string()]);
        s.apply(&ev("session.deleted", json!({"sessionID":"ses_c"})));
        assert_eq!(s.family_of("ses_p"), vec!["ses_p".to_string()]);
    }
}
