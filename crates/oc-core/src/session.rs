//! Session service (`core/src/session.ts`, `core/src/session/session.ts`): durable mutations.
//! Execution (`wake`/`interrupt`) is delegated to the coordinator held by the runtime.

use std::path::Path;

use rusqlite::{params, OptionalExtension};
use serde_json::{json, Map, Value};

use oc_schema::event::{Delivery, SessionEvent};
use oc_schema::types::*;

use crate::bus::Bus;
use crate::inbox::{self, InboxRow, Locks};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Session not found: {0}")]
    NotFound(String),
    #[error("Prompt message ID conflicts with an existing durable record: {0}")]
    PromptConflict(String),
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Clone, Default)]
pub struct CreateInput {
    pub id: Option<String>,
    pub parent_id: Option<String>,
    pub location: Option<LocationRef>,
    pub title: Option<String>,
    pub agent: Option<String>,
    pub model: Option<ModelRef>,
    pub metadata: Option<Metadata>,
    pub permissions: Option<Ruleset>,
}

#[derive(Debug, Clone, Default)]
pub struct PromptInput {
    pub id: Option<String>,
    pub text: String,
    pub files: Option<Vec<FileAttachment>>,
    pub agents: Option<Vec<AgentAttachment>>,
    pub skills: Option<Vec<SkillAttachment>>,
    pub metadata: Option<Metadata>,
    pub delivery: Option<Delivery>,
}

#[derive(Clone)]
pub struct Sessions {
    pub bus: Bus,
    pub locks: Locks,
    pub version: String,
}

/// `path.resolve(base, rel)` (lexical).
pub fn relative_join(base: &Path, rel: &str) -> std::path::PathBuf {
    oc_util::variable::normalize(&base.join(rel))
}

pub fn relative(base: &Path, target: &Path) -> String {
    let mut b = base.components().peekable();
    let mut t = target.components().peekable();
    while let (Some(x), Some(y)) = (b.peek(), t.peek()) {
        if x != y {
            break;
        }
        b.next();
        t.next();
    }
    let mut parts: Vec<String> = b.map(|_| "..".to_string()).collect();
    parts.extend(t.map(|c| c.as_os_str().to_string_lossy().into_owned()));
    parts.join("/")
}

impl Sessions {
    pub fn new(bus: Bus, version: impl Into<String>) -> Self {
        Sessions { bus, locks: Locks::default(), version: version.into() }
    }

    pub fn get(&self, id: &str) -> Result<SessionInfo, SessionError> {
        self.bus.db().session_get(id)?.ok_or_else(|| SessionError::NotFound(id.into()))
    }

    pub async fn create(&self, input: CreateInput) -> Result<SessionInfo, SessionError> {
        let id = input.id.clone().unwrap_or_else(oc_schema::id::session);
        if let Some(existing) = self.bus.db().session_get(&id)? {
            return Ok(existing);
        }
        let parent = match &input.parent_id {
            Some(p) => Some(self.get(p)?),
            None => None,
        };
        // A new root session stores its directory canonically, so `/p/` and `/p` list together.
        let location = parent
            .as_ref()
            .map(|p| p.location.clone())
            .or(input.location.clone().map(|l| LocationRef { directory: crate::location::canonical_directory(&l.directory), ..l }))
            .ok_or_else(|| anyhow::anyhow!("Session.create requires either location or an existing parentID"))?;
        let project = crate::project::resolve_and_persist(&self.bus, Path::new(&location.directory)).await?;
        let subpath = relative(&project.directory, Path::new(&location.directory));
        let event = SessionEvent::Created {
            session_id: id.clone(),
            project_id: project.id.clone(),
            location: location.clone(),
            subpath: Some(subpath),
            parent_id: input.parent_id.clone(),
            slug: oc_storage::session::slug(),
            title: input.title.clone(),
            agent: input.agent.clone(),
            model: input.model.clone(),
            metadata: input.metadata.clone().or_else(|| parent.as_ref().and_then(|p| p.metadata.clone())),
            permissions: input.permissions.clone().or_else(|| parent.as_ref().and_then(|p| p.permissions.clone())),
            version: self.version.clone(),
        };
        match self.bus.commit_one(event) {
            Ok(_) => {}
            Err(e) => {
                if let Some(existing) = self.bus.db().session_get(&id)? {
                    return Ok(existing);
                }
                return Err(e.into());
            }
        }
        self.get(&id)
    }

    /// Existing inbox row or already-promoted message with this id (`reconcile`).
    fn reconcile(&self, session: &str, id: &str, kind: &str) -> Result<Option<Value>, SessionError> {
        let row: Option<(String, String, String, String, i64)> = self.bus.db().with(|c| {
            c.query_row(
                "SELECT session_id, type, payload, delivery, time_created FROM session_inbox WHERE id = ?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
        })?;
        if let Some((sid, k, payload, delivery, created)) = row {
            if sid != session || k != kind {
                return Err(SessionError::PromptConflict(id.into()));
            }
            let mut payload = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
            self.bus.db().with(|c| oc_storage::stored::hydrate_inbox(c, &mut [&mut payload]))?;
            return Ok(Some(json!({
                "id": id, "sessionID": sid, "time": {"created": created}, "type": k,
                "payload": payload, "delivery": delivery,
            })));
        }
        let msg: Option<(String, String, String, i64)> = self.bus.db().with(|c| {
            c.query_row(
                "SELECT session_id, type, data, time_created FROM session_message WHERE id = ?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
        })?;
        if let Some((sid, k, data, created)) = msg {
            if sid != session || k != kind {
                return Err(SessionError::PromptConflict(id.into()));
            }
            let mut rows = [(id.to_string(), k.clone(), data)];
            self.bus.db().with(|c| oc_storage::stored::restore(c, &mut rows))?;
            let [(_, _, data)] = rows;
            let mut payload: Map<String, Value> = serde_json::from_str(&data).unwrap_or_default();
            payload.remove("time");
            return Ok(Some(json!({
                "id": id, "sessionID": sid, "time": {"created": created}, "type": k,
                "payload": payload, "delivery": "steer",
            })));
        }
        Ok(None)
    }

    /// Commit a staged revert before new input (`SessionRevert.commit`).
    pub fn commit_revert(&self, session: &SessionInfo) -> anyhow::Result<()> {
        if let Some(revert) = &session.revert {
            self.bus.commit_one(SessionEvent::RevertCommitted {
                session_id: session.id.clone(),
                to: revert.message_id.clone(),
            })?;
        }
        Ok(())
    }

    /// Admit a prepared user prompt; returns the inbox `User` record JSON.
    /// Attachment/skill preparation happens before this call.
    pub async fn prompt(&self, session_id: &str, input: PromptInput) -> Result<Value, SessionError> {
        let session = self.get(session_id)?;
        let id = input.id.clone().unwrap_or_else(oc_schema::id::message);
        if let Some(existing) = self.reconcile(session_id, &id, "user")? {
            return Ok(existing);
        }
        self.commit_revert(&session)?;
        let mut payload = Map::new();
        payload.insert("text".into(), json!(input.text));
        if let Some(a) = input.agents.as_ref().filter(|a| !a.is_empty()) {
            payload.insert("agents".into(), serde_json::to_value(a).unwrap());
        }
        if let Some(f) = input.files.as_ref().filter(|f| !f.is_empty()) {
            payload.insert("files".into(), serde_json::to_value(f).unwrap());
        }
        if let Some(s) = input.skills.as_ref().filter(|s| !s.is_empty()) {
            payload.insert("skills".into(), serde_json::to_value(s).unwrap());
        }
        if let Some(m) = &input.metadata {
            payload.insert("metadata".into(), Value::Object(m.clone()));
        }
        let delivery = input.delivery.unwrap_or(Delivery::Steer);
        inbox::admit(&self.bus, &self.locks, session_id, &id, "user", Value::Object(payload), delivery).await?;
        Ok(self.inbox_item(session_id, &id)?.map(|r| r.to_api()).unwrap_or(Value::Null))
    }

    pub async fn synthetic(
        &self,
        session_id: &str,
        id: Option<String>,
        text: String,
        description: Option<String>,
        metadata: Option<Metadata>,
        delivery: Option<Delivery>,
    ) -> Result<Value, SessionError> {
        self.get(session_id)?;
        let id = id.unwrap_or_else(oc_schema::id::message);
        if let Some(existing) = self.reconcile(session_id, &id, "synthetic")? {
            return Ok(existing);
        }
        let mut payload = Map::new();
        payload.insert("text".into(), json!(text));
        if let Some(d) = description {
            payload.insert("description".into(), json!(d));
        }
        if let Some(m) = metadata {
            payload.insert("metadata".into(), Value::Object(m));
        }
        inbox::admit(&self.bus, &self.locks, session_id, &id, "synthetic", Value::Object(payload), delivery.unwrap_or(Delivery::Steer)).await?;
        Ok(self.inbox_item(session_id, &id)?.map(|r| r.to_api()).unwrap_or(Value::Null))
    }

    pub async fn compact(&self, session_id: &str, delivery: Option<Delivery>) -> Result<Value, SessionError> {
        let session = self.get(session_id)?;
        self.commit_revert(&session)?;
        let id = oc_schema::id::message();
        inbox::admit(&self.bus, &self.locks, session_id, &id, "compaction", json!({}), delivery.unwrap_or(Delivery::Steer)).await?;
        Ok(self.inbox_item(session_id, &id)?.map(|r| r.to_api()).unwrap_or(Value::Null))
    }

    fn inbox_item(&self, session_id: &str, id: &str) -> anyhow::Result<Option<InboxRow>> {
        Ok(inbox::list(&self.bus, session_id)?.into_iter().find(|r| r.id == id))
    }

    pub fn rename(&self, session_id: &str, title: &str) -> Result<(), SessionError> {
        self.get(session_id)?;
        self.bus.commit_one(SessionEvent::Renamed { session_id: session_id.into(), title: title.into() })?;
        Ok(())
    }

    /// `Session.archive`: publishes `session.archived` unless the session is already archived.
    pub fn archive(&self, session_id: &str) -> Result<(), SessionError> {
        let s = self.get(session_id)?;
        if s.time.archived.is_some() {
            return Ok(());
        }
        self.bus.commit_one(SessionEvent::Archived { session_id: session_id.into() })?;
        Ok(())
    }

    /// `Session.unarchive`: publishes `session.unarchived` unless the session is not archived.
    pub fn unarchive(&self, session_id: &str) -> Result<(), SessionError> {
        let s = self.get(session_id)?;
        if s.time.archived.is_none() {
            return Ok(());
        }
        self.bus.commit_one(SessionEvent::Unarchived { session_id: session_id.into() })?;
        Ok(())
    }

    pub fn set_metadata(&self, session_id: &str, metadata: Metadata) -> Result<(), SessionError> {
        self.get(session_id)?;
        self.bus.commit_one(SessionEvent::MetadataUpdated { session_id: session_id.into(), metadata })?;
        Ok(())
    }

    pub fn set_permissions(&self, session_id: &str, permissions: Ruleset) -> Result<(), SessionError> {
        self.get(session_id)?;
        self.bus.commit_one(SessionEvent::Permissions { session_id: session_id.into(), permissions })?;
        Ok(())
    }

    pub fn switch_agent(&self, session_id: &str, agent: &str) -> Result<(), SessionError> {
        let s = self.get(session_id)?;
        if s.agent.as_deref() == Some(agent) {
            return Ok(());
        }
        self.bus.commit_one(SessionEvent::AgentSelected {
            session_id: session_id.into(),
            agent: agent.into(),
            previous: s.agent.clone(),
        })?;
        Ok(())
    }

    pub fn switch_model(&self, session_id: &str, model: ModelRef) -> Result<(), SessionError> {
        let s = self.get(session_id)?;
        let same = s.model.as_ref().is_some_and(|m| {
            m.provider_id == model.provider_id
                && m.id == model.id
                && m.variant.as_deref().unwrap_or("default") == model.variant.as_deref().unwrap_or("default")
        });
        if same {
            return Ok(());
        }
        self.bus.commit_one(SessionEvent::ModelSelected { session_id: session_id.into(), model, previous: s.model.clone() })?;
        Ok(())
    }

    pub fn view(&self, session_id: &str, idle: u64) -> Result<(), SessionError> {
        let s = self.get(session_id)?;
        let Some(t) = s.time.idle else { return Ok(()) };
        if idle > t || s.time.viewed.is_some_and(|v| v >= idle) {
            return Ok(());
        }
        self.bus.commit_one(SessionEvent::Viewed { session_id: session_id.into(), idle })?;
        Ok(())
    }

    /// Execution claim (`store.claim` / `release`), run inside the event commit.
    pub fn claim(c: &rusqlite::Connection, session: &str) -> rusqlite::Result<()> {
        c.execute(
            "UPDATE session_v2 SET time_suspended = ? WHERE id = ? AND time_suspended IS NULL",
            params![oc_schema::id::now_ms() as i64, session],
        )?;
        Ok(())
    }

    pub fn release(c: &rusqlite::Connection, session: &str) -> rusqlite::Result<()> {
        c.execute("UPDATE session_v2 SET time_suspended = NULL, resume_attempts = 0 WHERE id = ?", [session])?;
        Ok(())
    }
}

// ---------- fork / delete (`core/src/session.ts:319-377`) ----------

#[derive(Debug, thiserror::Error)]
pub enum ForkError {
    #[error("Session has no messages to fork")]
    Empty,
    #[error("Message not found: {0}")]
    MessageNotFound(String),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Sessions {
    /// Fork `session_id` before `before` (exclusive) or through its latest message.
    pub fn fork(&self, session_id: &str, before: Option<&str>) -> Result<SessionInfo, ForkError> {
        self.get(session_id)?;
        let msgs = self.bus.db().session_messages(session_id)?;
        let boundary = match before {
            Some(id) => {
                if !msgs.iter().any(|(_, m)| m.id() == id) {
                    return Err(ForkError::MessageNotFound(id.into()));
                }
                ForkBoundary { kind: ForkBoundaryType::Before, message_id: id.into() }
            }
            None => {
                let Some((_, last)) = msgs.last() else { return Err(ForkError::Empty) };
                ForkBoundary { kind: ForkBoundaryType::Through, message_id: last.id().into() }
            }
        };
        let instructions = crate::instruction_state::current(&self.bus, session_id)?;
        let entries: Vec<oc_schema::event::InstructionEntryValue> = self.bus.db().with(|c| {
            let mut s = c.prepare("SELECT key, value, removed FROM instruction_entry WHERE session_id = ? ORDER BY key")?;
            let r = s
                .query_map([session_id], |r| {
                    let v: Option<String> = r.get(1)?;
                    Ok(oc_schema::event::InstructionEntryValue {
                        key: r.get(0)?,
                        value: v.and_then(|v| serde_json::from_str(&v).ok()),
                        removed: r.get(2)?,
                    })
                })?
                .collect();
            r
        })?;
        let id = oc_schema::id::session();
        self.bus.commit_one(SessionEvent::Forked {
            session_id: id.clone(),
            parent_id: session_id.into(),
            boundary,
            instructions,
            instruction_entries: (!entries.is_empty()).then_some(entries),
        })?;
        Ok(self.get(&id)?)
    }

    /// Delete a session and its children (callers interrupt execution first).
    pub fn remove(&self, session_id: &str) -> Result<(), SessionError> {
        self.get(session_id)?;
        let children: Vec<String> = self.bus.db().with(|c| {
            let mut s = c.prepare("SELECT id FROM session_v2 WHERE parent_id = ?")?;
            let r = s.query_map([session_id], |r| r.get(0))?.collect();
            r
        })?;
        for child in children {
            let _ = self.remove(&child);
        }
        self.bus.commit_one(SessionEvent::Deleted { session_id: session_id.into() })?;
        self.bus.db().with(|c| {
            c.execute("DELETE FROM event WHERE aggregate_id = ?", [session_id])?;
            c.execute("DELETE FROM event_sequence WHERE aggregate_id = ?", [session_id])
        })?;
        Ok(())
    }

    pub fn revert_clear(&self, session_id: &str) -> Result<(), SessionError> {
        self.get(session_id)?;
        self.bus.commit_one(SessionEvent::RevertCleared { session_id: session_id.into() })?;
        Ok(())
    }

    pub fn revert_commit_now(&self, session_id: &str) -> Result<(), SessionError> {
        let s = self.get(session_id)?;
        self.commit_revert(&s)?;
        Ok(())
    }
}
