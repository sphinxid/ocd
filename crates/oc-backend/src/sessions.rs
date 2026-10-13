//! `session.*` operations (`packages/server/src/handlers/session.ts` over `core/src/session.ts`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use indexmap::IndexMap;
use oc_core::inbox::{self, Scope};
use oc_core::session::{CreateInput, PromptInput, SessionError};
use oc_schema::event::{Delivery, InboxItem, InstructionEntryValue, SessionEvent};
use oc_schema::types::*;
use oc_server::ApiError;
use oc_server::generated as api;
use serde_json::{Map, Value, json};

use crate::engine::Engine;
use crate::store;
use crate::util::{conv, defect, from_json, is_lifecycle_conflict};

/// `Session.NotFoundError` / anything else → API error.
pub fn session_error(e: SessionError) -> ApiError {
    match e {
        SessionError::NotFound(id) => ApiError::session_not_found(id),
        SessionError::PromptConflict(id) => ApiError::conflict(
            format!("Prompt message ID conflicts with an existing durable record: {id}"),
            Some(id),
        ),
        SessionError::Other(e) => defect(format!("{e:#}")),
    }
}

fn delivery(d: Option<api::SessionInboxDelivery>) -> Option<Delivery> {
    d.map(|d| match d {
        api::SessionInboxDelivery::Steer => Delivery::Steer,
        api::SessionInboxDelivery::Queue => Delivery::Queue,
    })
}

/// Decode a projected message row into the API union (`MessageDecodeError` → 500 UnknownError).
pub fn decode_message(row: &store::MessageRow) -> Result<api::SessionMessageInfo, ApiError> {
    let json = row.json().ok_or_else(|| ApiError::unknown(format!("failed to decode session message {}", row.id)))?;
    serde_json::from_value(json).map_err(|e| ApiError::unknown(format!("failed to decode session message {}: {e}", row.id)))
}

impl Engine {
    pub fn session_api(&self, s: &SessionInfo) -> Result<api::SessionInfo, ApiError> {
        conv(s)
    }

    pub async fn create_session(&self, cwd: &str, body: api::SessionCreateBody) -> Result<api::SessionInfo, ApiError> {
        // Upstream resolves the project of a relative directory and fails with a defect (500, empty
        // body); `AbsolutePath` carries no check, so it is not a 400.
        if let Some(dir) = body.location.as_ref().map(|l| &l.directory)
            && body.parent_id.is_none()
            && !std::path::Path::new(dir).is_absolute()
        {
            return Err(ApiError::defect(format!("relative session directory: {dir}")));
        }
        let input = CreateInput {
            id: body.id,
            parent_id: body.parent_id.clone(),
            location: if body.parent_id.is_some() {
                None
            } else {
                Some(LocationRef {
                    directory: body.location.map(|l| l.directory).unwrap_or_else(|| cwd.to_string()),
                    workspace_id: None,
                })
            },
            title: body.title,
            agent: body.agent,
            model: body.model.map(conv).transpose()?,
            metadata: body.metadata.map(conv).transpose()?,
            permissions: body.permissions.map(conv).transpose()?,
        };
        let session = self.app.sessions.create(input).await.map_err(session_error)?;
        // Upstream activates the session's location right away (`project.updated` follows
        // `session.created`); boot it here too so location events keep that order.
        let _ = self.locations.get(&session.location.directory).await;
        self.session_api(&session)
    }

    /// `Session.remove`: interrupt, wait idle, remove children, clear environment, delete.
    pub fn remove_session<'a>(&'a self, id: &'a str) -> futures::future::BoxFuture<'a, Result<(), ApiError>> {
        Box::pin(async move {
            self.session(id)?;
            if let Some(c) = self.app.coordinator.get() {
                c.interrupt(id, Some(oc_core::coordinator::Reason::User), false).await;
                c.await_idle(id).await;
            }
            self.app.model_transport.close(id);
            for child in store::children(&self.app.db, id).map_err(defect)? {
                self.remove_session(&child).await?;
            }
            self.app.environments.lock().remove(id);
            self.app.bus.commit_one(SessionEvent::Deleted { session_id: id.into() }).map_err(|e| defect(format!("{e:#}")))?;
            store::remove_aggregate(&self.app.db, id).map_err(defect)?;
            Ok(())
        })
    }

    pub async fn update_session(&self, id: &str, body: api::SessionUpdateBody) -> Result<(), ApiError> {
        self.session(id)?;
        if let Some(title) = body.title {
            if title.is_empty() {
                match &self.runner {
                    Some(r) => {
                        if let Err(e) = r.generate_title(id).await {
                            tracing::warn!(session = id, error = %e, "title generation failed");
                        }
                    }
                    None => tracing::debug!(session = id, "title generation requested but no session runner is installed"),
                }
            } else {
                self.app.sessions.rename(id, &title).map_err(session_error)?;
            }
        }
        if let Some(m) = body.metadata {
            self.app.sessions.set_metadata(id, conv(m)?).map_err(session_error)?;
        }
        if let Some(p) = body.permissions {
            self.app.sessions.set_permissions(id, conv(p)?).map_err(session_error)?;
        }
        Ok(())
    }

    /// `Session.fork`.
    pub fn fork_session(&self, id: &str, before: Option<String>) -> Result<api::SessionInfo, ApiError> {
        let parent = self.session(id)?;
        let db = &self.app.db;
        let boundary: Option<String> = db
            .with(|c| {
                use rusqlite::OptionalExtension;
                match &before {
                    Some(b) => c
                        .query_row(
                            "SELECT id FROM session_message WHERE session_id = ? AND id = ? ORDER BY seq DESC LIMIT 1",
                            [id, b.as_str()],
                            |r| r.get(0),
                        )
                        .optional(),
                    None => c
                        .query_row("SELECT id FROM session_message WHERE session_id = ? ORDER BY seq DESC LIMIT 1", [id], |r| r.get(0))
                        .optional(),
                }
            })
            .map_err(defect)?;
        let Some(boundary) = boundary else {
            return Err(match before {
                Some(b) => ApiError::message_not_found(id, b),
                None => ApiError::invalid_request(format!("Cannot fork empty session: {id}")).with_kind("empty_session"),
            });
        };
        let new_id = oc_schema::id::session();
        let instructions = store::instruction_current(db, &parent.id).map_err(defect)?;
        let entries = store::instruction_entries(db, &parent.id, true)
            .map_err(defect)?
            .into_iter()
            .map(|(key, value, removed)| InstructionEntryValue { key, value, removed })
            .collect();
        self.app
            .bus
            .commit_one(SessionEvent::Forked {
                session_id: new_id.clone(),
                parent_id: parent.id.clone(),
                boundary: ForkBoundary {
                    kind: if before.is_some() { ForkBoundaryType::Before } else { ForkBoundaryType::Through },
                    message_id: boundary,
                },
                instructions,
                instruction_entries: Some(entries),
            })
            .map_err(|e| defect(format!("{e:#}")))?;
        let s = self.session(&new_id).map_err(|_| defect("forked session missing"))?;
        self.session_api(&s)
    }

    /// Prompt preparation (`SessionPrompt.prepare`): attachments and skills.
    async fn prepare_prompt(
        &self,
        loc: &crate::locations::Loc,
        files: Option<Vec<api::PromptInputFileAttachment>>,
        skills: Option<Vec<api::PromptInputSkillAttachment>>,
    ) -> Result<(Option<Vec<FileAttachment>>, Option<Vec<SkillAttachment>>), ApiError> {
        let files = match files {
            Some(files) => {
                let inputs: Vec<oc_core::attachment::Input> = files
                    .into_iter()
                    .map(|f| -> Result<_, ApiError> {
                        Ok(oc_core::attachment::Input {
                            uri: f.uri,
                            name: f.name,
                            description: f.description,
                            mention: f.mention.map(conv).transpose()?,
                        })
                    })
                    .collect::<Result<_, _>>()?;
                // `Image.normalize` with the location's `media.image` limits (`ConfigImagePlugin`).
                let settings = oc_config::fold::media_image(&loc.location.entries);
                let limits = oc_tools::tool::ImageLimits {
                    auto_resize: settings.auto_resize,
                    max_width: settings.max_width.min(u32::MAX as u64) as u32,
                    max_height: settings.max_height.min(u32::MAX as u64) as u32,
                    max_base64_bytes: settings.max_base64_bytes,
                };
                let out = tokio::task::spawn_blocking(move || {
                    let normalize = |label: &str, data: &str, mime: &str| {
                        oc_tools::image::normalize(label, data, mime, &limits)
                            .map(|n| (n.content, n.mime))
                            .map_err(|e| e.to_string())
                    };
                    inputs
                        .iter()
                        .map(|i| oc_core::attachment::materialize(i, Some(&normalize)))
                        .collect::<Result<Vec<_>, _>>()
                })
                .await
                .map_err(defect)?
                .map_err(|e| ApiError::invalid_request(e.message).with_field("files"))?;
                Some(out)
            }
            None => None,
        };
        let skills = match skills.filter(|s| !s.is_empty()) {
            Some(requested) => {
                let registry = self.skills(loc).await;
                let mut prepared: IndexMap<String, String> = IndexMap::new();
                let mut out = Vec::new();
                for attachment in requested {
                    let mention = attachment.mention.map(conv).transpose()?;
                    if let Some(name) = prepared.get(&attachment.id) {
                        out.push(SkillAttachment { id: attachment.id.clone(), name: name.clone(), text: None, mention });
                        continue;
                    }
                    let Some(skill) = registry.iter().find(|s| s.id == attachment.id) else {
                        return Err(ApiError::invalid_request(format!("Skill not found: {}", attachment.id)).with_field("skills"));
                    };
                    prepared.insert(skill.id.clone(), skill.name.clone());
                    let (name, path, content) = (skill.name.clone(), skill.path.clone(), skill.content.clone());
                    let text = tokio::task::spawn_blocking(move || oc_core::discovery::skill_block(&name, &path, &content))
                        .await
                        .map_err(defect)?;
                    out.push(SkillAttachment { id: skill.id.clone(), name: skill.name.clone(), text: Some(text), mention });
                }
                Some(out)
            }
            None => None,
        };
        Ok((files, skills))
    }

    pub async fn prompt(&self, id: &str, body: api::SessionPromptBody) -> Result<api::SessionInboxUser, ApiError> {
        let (_, loc) = self.session_scope(id).await?;
        // `SessionPrompt.prepare`: `Plugin.awaitActivation`, then the `session.prompt` hook before
        // attachments are materialised.
        self.app.plugins.await_activation(&loc.key).await;
        let mut body = body;
        if self.app.plugins.has_hook(&loc.key, "session.prompt", None) {
            let message_id = body.id.clone().unwrap_or_else(oc_schema::id::message);
            let mut prompt = json!({ "text": body.text });
            if let Some(f) = &body.files {
                prompt["files"] = serde_json::to_value(f).unwrap_or_default();
            }
            if let Some(a) = &body.agents {
                prompt["agents"] = serde_json::to_value(a).unwrap_or_default();
            }
            if let Some(sk) = &body.skills {
                prompt["skills"] = serde_json::to_value(sk).unwrap_or_default();
            }
            let mut event = json!({ "sessionID": id, "messageID": message_id, "prompt": prompt });
            if let Some(m) = &body.metadata {
                event["metadata"] = serde_json::to_value(m).unwrap_or_default();
            }
            event["delivery"] = body.delivery.as_ref().map(|d| serde_json::to_value(d).unwrap_or_default()).unwrap_or(json!("steer"));
            let out = self.app.plugins.trigger(&loc.key, "session.prompt", event).await.map_err(|e| defect(e.to_string()))?;
            let prompt = out.get("prompt").cloned().unwrap_or_default();
            body.id = Some(message_id);
            body.text = prompt.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
            body.files = prompt.get("files").cloned().filter(|v| !v.is_null()).and_then(|v| serde_json::from_value(v).ok());
            body.agents = prompt.get("agents").cloned().filter(|v| !v.is_null()).and_then(|v| serde_json::from_value(v).ok());
            body.skills = prompt.get("skills").cloned().filter(|v| !v.is_null()).and_then(|v| serde_json::from_value(v).ok());
            body.metadata = out.get("metadata").cloned().filter(|v| !v.is_null()).and_then(|v| serde_json::from_value(v).ok());
            body.delivery = out.get("delivery").cloned().and_then(|v| serde_json::from_value(v).ok()).or(body.delivery);
        }
        let (files, skills) = self.prepare_prompt(&loc, body.files, body.skills).await?;
        let input = PromptInput {
            id: body.id.clone(),
            text: body.text,
            files,
            agents: body.agents.map(conv).transpose()?,
            skills,
            metadata: body.metadata.map(conv).transpose()?,
            delivery: delivery(body.delivery),
        };
        let admitted = self.app.sessions.prompt(id, input).await.map_err(|e| match e {
            SessionError::Other(e) if is_lifecycle_conflict(&e) => {
                let mid = body.id.clone().unwrap_or_default();
                ApiError::conflict(format!("Prompt message ID conflicts with an existing durable record: {mid}"), Some(mid))
            }
            other => session_error(other),
        })?;
        if body.resume != Some(false) {
            self.wake(id, Scope::Input);
        }
        from_json(admitted)
    }

    pub async fn synthetic(&self, id: &str, body: api::SessionSyntheticBody) -> Result<api::SessionInboxSynthetic, ApiError> {
        self.session_scope(id).await?;
        let conflict = |mid: &str| {
            ApiError::conflict(format!("Synthetic input ID conflicts with an existing durable record: {mid}"), Some(mid.to_string()))
        };
        let admitted = self
            .app
            .sessions
            .synthetic(id, body.id.clone(), body.text, body.description, body.metadata.map(conv).transpose()?, delivery(body.delivery))
            .await
            .map_err(|e| match e {
                SessionError::PromptConflict(mid) => conflict(&mid),
                SessionError::Other(e) if is_lifecycle_conflict(&e) => conflict(body.id.as_deref().unwrap_or_default()),
                other => session_error(other),
            })?;
        if body.resume != Some(false) && self.session(id)?.revert.is_none() {
            self.wake(id, Scope::Input);
        }
        from_json(admitted)
    }

    pub async fn compact(&self, id: &str, body: api::SessionCompactBody) -> Result<api::SessionInboxCompaction, ApiError> {
        let (session, _) = self.session_scope(id).await?;
        self.app.sessions.commit_revert(&session).map_err(defect)?;
        let input_id = body.id.unwrap_or_else(oc_schema::id::message);
        let conflict = || {
            ApiError::conflict(
                format!("Compaction input ID conflicts with an existing durable record: {input_id}"),
                Some(input_id.clone()),
            )
        };
        let existing = inbox::list(&self.app.bus, id).map_err(defect)?.into_iter().find(|r| r.id == input_id);
        let row = match existing {
            Some(r) if r.kind == "compaction" => r,
            Some(_) => return Err(conflict()),
            None => {
                let d = delivery(body.delivery).unwrap_or(Delivery::Steer);
                inbox::admit(&self.app.bus, &self.app.sessions.locks, id, &input_id, "compaction", json!({}), d)
                    .await
                    .map_err(|e| if is_lifecycle_conflict(&e) { conflict() } else { defect(format!("{e:#}")) })?;
                inbox::list(&self.app.bus, id)
                    .map_err(defect)?
                    .into_iter()
                    .find(|r| r.id == input_id)
                    .ok_or_else(|| defect("admitted compaction missing"))?
            }
        };
        self.wake(id, Scope::Input);
        from_json(row.to_api())
    }

    pub async fn interrupt(&self, id: &str, resume: bool) -> Result<bool, ApiError> {
        self.session_scope(id).await?;
        let Some(c) = self.app.coordinator.get() else { return Ok(false) };
        let interrupted = c.interrupt(id, Some(oc_core::coordinator::Reason::User), false).await;
        if !resume {
            return Ok(interrupted);
        }
        if let Some(next) = inbox::next_promotable(&self.app.bus, id, Scope::Input).map_err(defect)?
            && (next.delivery == Delivery::Steer || next.kind == "compaction" || next.kind == "move") {
                c.wake(id, Scope::Steer);
            }
        Ok(interrupted)
    }

    pub fn inbox_list(&self, id: &str) -> Result<Vec<api::SessionInboxInfo>, ApiError> {
        self.session(id)?;
        inbox::list(&self.app.bus, id).map_err(defect)?.iter().map(|r| from_json(r.to_api())).collect()
    }

    pub async fn inbox_cancel(&self, id: &str, inbox_id: &str) -> Result<(), ApiError> {
        self.session(id)?;
        match inbox::cancel(&self.app.bus, &self.app.sessions.locks, id, inbox_id).await {
            Ok(()) => Ok(()),
            Err(e) if is_lifecycle_conflict(&e) => {
                self.session(id)?;
                Ok(())
            }
            Err(e) => Err(defect(format!("{e:#}"))),
        }
    }

    pub async fn inbox_update(&self, id: &str, inbox_id: &str, to: api::SessionInboxDelivery) -> Result<(), ApiError> {
        self.session(id)?;
        let d = delivery(Some(to)).unwrap();
        match inbox::set_delivery(&self.app.bus, &self.app.sessions.locks, id, inbox_id, d).await {
            Ok(()) => {}
            Err(e) if is_lifecycle_conflict(&e) => {
                self.session(id)?;
                let name = if d == Delivery::Steer { "steer" } else { "queue" };
                return Err(ApiError::conflict(
                    format!("Pending input cannot change to {name}: {inbox_id}"),
                    Some(inbox_id.to_string()),
                ));
            }
            Err(e) => return Err(defect(format!("{e:#}"))),
        }
        if d == Delivery::Steer {
            self.wake(id, Scope::Input);
        }
        Ok(())
    }

    /// `Session.move`.
    pub async fn move_session(&self, id: &str, body: api::SessionMoveBody) -> Result<(), ApiError> {
        let session = self.session(id)?;
        let value = body.directory.trim();
        let home = self.home().to_path_buf();
        let expanded = if value == "~" {
            home
        } else if let Some(rest) = value.strip_prefix("~/") {
            home.join(rest)
        } else {
            PathBuf::from(value)
        };
        let directory = oc_git::paths::resolve(Path::new(&session.location.directory), &expanded);
        let dir_s = directory.display().to_string();
        let Ok(meta) = std::fs::metadata(&directory) else {
            return Err(ApiError::invalid_request(format!("Directory does not exist: {dir_s}")));
        };
        if !meta.is_dir() {
            return Err(ApiError::invalid_request(format!("Not a directory: {dir_s}")));
        }
        let project = oc_core::project::resolve_and_persist(&self.app.bus, &directory).await.map_err(defect)?;
        if let Err(e) = self.locations.get(&dir_s).await {
            tracing::warn!(directory = dir_s, error = %e, "session move destination unavailable");
            return Err(ApiError::invalid_request(format!("Directory is unavailable: {dir_s}")));
        }
        let subpath = oc_git::paths::relative(&project.directory, &directory).replace('\\', "/");
        let location = LocationRef { directory: dir_s.clone(), workspace_id: None };
        let unavailable = !self.is_active(id) && !std::fs::metadata(&session.location.directory).is_ok_and(|m| m.is_dir());
        let lock = self.app.sessions.locks.get(id);
        {
            let _g = lock.lock().await;
            let latest = self.session(id)?;
            if unavailable && latest.location.directory == session.location.directory && !self.is_active(id) {
                let mut events: Vec<oc_core::bus::Durable1> = inbox::list(&self.app.bus, id)
                    .map_err(defect)?
                    .into_iter()
                    .filter(|r| r.kind == "move")
                    .map(|r| SessionEvent::InboxCancelled { session_id: id.into(), inbox_id: r.id }.into())
                    .collect();
                events.push(
                    SessionEvent::Moved { session_id: id.into(), location, project_id: project.id.clone(), subpath: Some(subpath) }.into(),
                );
                self.app.bus.commit(events).map_err(|e| defect(format!("{e:#}")))?;
            } else {
                let payload = json!({"location": {"directory": dir_s}, "projectID": project.id, "subpath": subpath});
                self.app
                    .bus
                    .commit_one(SessionEvent::InboxEnqueued {
                        session_id: id.into(),
                        inbox_id: oc_schema::id::message(),
                        item: InboxItem { kind: "move".into(), payload, delivery: delivery(body.delivery).unwrap_or(Delivery::Steer) },
                    })
                    .map_err(|e| defect(format!("{e:#}")))?;
            }
        }
        self.wake(id, Scope::Input);
        Ok(())
    }

    // ---- messages -------------------------------------------------------------------------

    pub fn message_get(&self, id: &str, message_id: &str) -> Result<api::SessionMessageInfo, ApiError> {
        self.session(id)?;
        match store::message(&self.app.db, message_id).map_err(defect)? {
            Some((sid, row)) if sid == id => decode_message(&row),
            _ => Err(ApiError::message_not_found(id, message_id)),
        }
    }

    pub fn message_list(&self, req: oc_server::SessionMessageListRequest) -> Result<Vec<api::SessionMessageInfo>, ApiError> {
        self.session(&req.session_id)?;
        let asc = matches!(req.order, api::SessionMessageListOrder::Asc);
        let kind = req.r#type.and_then(|t| serde_json::to_value(t).ok().and_then(|v| v.as_str().map(str::to_string)));
        let cursor = req.cursor.as_ref().map(|c| (c.id.as_str(), c.direction));
        let rows = store::messages(&self.app.db, &req.session_id, Some(req.limit), asc, kind.as_deref(), cursor).map_err(defect)?;
        rows.iter().map(decode_message).collect()
    }

    pub fn context(&self, id: &str) -> Result<Vec<api::SessionMessageInfo>, ApiError> {
        self.session(id)?;
        store::context(&self.app.db, id).map_err(defect)?.iter().map(decode_message).collect()
    }

    // ---- revert -----------------------------------------------------------------------------

    pub async fn revert_stage(&self, id: &str, message_id: &str, files: Option<bool>) -> Result<api::SessionRevert, ApiError> {
        let (session, loc) = self.session_scope(id).await?;
        if self.is_active(id) {
            return Err(ApiError::session_busy(id));
        }
        tracing::info!(session = id, message = message_id, ?files, "session.revert.stage");
        let snapshots = &loc.snapshots;
        let original = match session.revert.as_ref().and_then(|r| r.snapshot.clone()) {
            Some(s) => Some(oc_git::git::TreeId(s)),
            None => snapshots.capture().await,
        };
        // plan: files touched by assistant steps after the boundary, first start snapshot wins.
        let boundary = store::message_seq(&self.app.db, id, message_id).map_err(defect)?;
        let Some(boundary) = boundary else { return Err(ApiError::message_not_found(id, message_id)) };
        let mut next: IndexMap<String, String> = IndexMap::new();
        for row in store::rows_of_types(&self.app.db, id, &["assistant"]).map_err(defect)? {
            if row.seq <= boundary {
                continue;
            }
            let data = row.data();
            let Some(start) = data.pointer("/snapshot/start").and_then(Value::as_str) else { continue };
            for file in data.pointer("/snapshot/files").and_then(Value::as_array).into_iter().flatten() {
                if let Some(f) = file.as_str() {
                    next.entry(f.to_string()).or_insert_with(|| start.to_string());
                }
            }
        }
        let mut restore: IndexMap<String, String> = IndexMap::new();
        if let Some(o) = &original {
            for f in session.revert.as_ref().and_then(|r| r.files.as_ref()).into_iter().flatten() {
                restore.insert(f.file.clone(), o.0.clone());
            }
        }
        if files != Some(false) {
            for (f, t) in &next {
                restore.insert(f.clone(), t.clone());
            }
        }
        let snapshot_err = |e: oc_git::SnapshotError| ApiError::unknown(format!("failed to stage session revert ({id}): {e}"));
        if !restore.is_empty() {
            let plan: Vec<(String, oc_git::git::TreeId)> = restore.into_iter().map(|(f, t)| (f, oc_git::git::TreeId(t))).collect();
            snapshots.restore(&plan).await.map_err(snapshot_err)?;
        }
        let paths: Vec<String> = if files == Some(false) { vec![] } else { next.keys().cloned().collect() };
        let diffs = match &original {
            Some(o) => {
                let to = snapshots.capture().await.unwrap_or_else(|| o.clone());
                snapshots.diff(o, &to, None, Some(paths)).await.map_err(snapshot_err)?
            }
            None => vec![],
        };
        let revert = Revert {
            message_id: message_id.to_string(),
            part_id: None,
            snapshot: original.map(|o| o.0),
            files: Some(diffs),
        };
        self.app
            .bus
            .commit_one(SessionEvent::RevertStaged { session_id: id.into(), revert: revert.clone() })
            .map_err(|e| defect(format!("{e:#}")))?;
        conv(&revert)
    }

    pub async fn revert_clear(&self, id: &str) -> Result<(), ApiError> {
        let (session, loc) = self.session_scope(id).await?;
        if self.is_active(id) {
            return Err(ApiError::session_busy(id));
        }
        tracing::info!(session = id, "session.revert.clear");
        if let Some(revert) = &session.revert {
            if let Some(original) = &revert.snapshot {
                let plan: Vec<(String, oc_git::git::TreeId)> = revert
                    .files
                    .iter()
                    .flatten()
                    .map(|f| (f.file.clone(), oc_git::git::TreeId(original.clone())))
                    .collect();
                loc.snapshots
                    .restore(&plan)
                    .await
                    .map_err(|e| ApiError::unknown(format!("failed to clear session revert ({id}): {e}")))?;
            }
            self.app.bus.commit_one(SessionEvent::RevertCleared { session_id: id.into() }).map_err(|e| defect(format!("{e:#}")))?;
        }
        self.wake(id, Scope::Input);
        Ok(())
    }

    pub async fn revert_commit(&self, id: &str) -> Result<(), ApiError> {
        let (session, _) = self.session_scope(id).await?;
        if self.is_active(id) {
            return Err(ApiError::session_busy(id));
        }
        tracing::info!(session = id, "session.revert.commit");
        self.app.sessions.commit_revert(&session).map_err(|e| defect(format!("{e:#}")))
    }

    /// `SessionDiff.turn`.
    pub async fn diff(&self, id: &str, from: Option<String>, to: Option<String>, context: Option<i64>) -> Result<Vec<api::FileDiffInfo>, ApiError> {
        let session = self.session(id)?;
        let db = &self.app.db;
        let mut kinds = vec!["user", "idle"];
        let rows = {
            let mut rows = store::rows_of_types(db, id, &kinds).map_err(defect)?;
            for extra in [from.as_deref(), to.as_deref()].into_iter().flatten() {
                if !rows.iter().any(|r| r.id == extra)
                    && let Some((sid, row)) = store::message(db, extra).map_err(defect)?
                        && sid == id {
                            rows.push(row);
                        }
            }
            rows.sort_by_key(|r| r.seq);
            rows
        };
        kinds.clear();
        let users: Vec<&store::MessageRow> = rows.iter().filter(|r| r.kind == "user").collect();
        let markers: Vec<&store::MessageRow> = rows.iter().filter(|r| r.kind == "idle").collect();
        let resolve = |field: &str, mid: &str| -> Result<&store::MessageRow, ApiError> {
            let row = rows.iter().find(|r| r.id == mid).ok_or_else(|| ApiError::message_not_found(id, mid))?;
            if row.kind != "user" {
                return Err(ApiError::invalid_request(format!("Message {mid} is not a user message")).with_field(field));
            }
            Ok(row)
        };
        let anchor = match &from {
            Some(f) => resolve("from", f)?,
            None => match users.last() {
                Some(u) => u,
                None => return Ok(vec![]),
            },
        };
        let last = match &to {
            Some(t) => resolve("to", t)?,
            None => anchor,
        };
        if last.seq < anchor.seq {
            return Err(ApiError::invalid_request(format!("Message {} precedes {}", last.id, anchor.id)).with_field("to"));
        }
        let legacy = markers.is_empty();
        let opened = markers.iter().rev().find(|r| r.seq < anchor.seq).map(|r| r.seq).unwrap_or(-1);
        let start = if legacy { anchor.seq } else { users.iter().find(|r| r.seq > opened).map(|r| r.seq).unwrap_or(anchor.seq) };
        let end = if legacy {
            users.iter().find(|r| r.seq > last.seq).map(|r| r.seq)
        } else {
            markers.iter().find(|r| r.seq > last.seq).map(|r| r.seq)
        };
        let steps: Vec<store::MessageRow> = store::rows_of_types(db, id, &["assistant"])
            .map_err(defect)?
            .into_iter()
            .filter(|r| r.seq > start && end.is_none_or(|e| r.seq < e))
            .collect();
        let (Some(first), Some(final_)) = (steps.first(), steps.last()) else { return Ok(vec![]) };
        let snap = |r: &store::MessageRow, k: &str| r.data().pointer(&format!("/snapshot/{k}")).and_then(Value::as_str).map(str::to_string);
        let Some(from_tree) = steps.iter().find_map(|s| snap(s, "start")) else { return Ok(vec![]) };
        let switches = store::rows_of_types(db, id, &["location-switched"]).map_err(defect)?;
        if switches.iter().any(|r| r.seq > first.seq && r.seq < final_.seq) {
            return Err(ApiError::invalid_request("Turn range spans a location change").with_field("to"));
        }
        let before = switches.iter().rev().find(|r| r.seq < first.seq).and_then(|r| r.data().pointer("/location/directory").and_then(Value::as_str).map(str::to_string));
        let after = switches
            .iter()
            .find(|r| r.seq > first.seq)
            .and_then(|r| r.data().pointer("/previous/location/directory").and_then(Value::as_str).map(str::to_string));
        let directory = before.or(after).unwrap_or_else(|| session.location.directory.clone());
        let recorded = steps.iter().rev().find_map(|s| snap(s, "end"));
        let loc = self.locations.get(&directory).await?;
        let running = self.is_active(id) && final_.data().pointer("/time/completed").is_none_or(Value::is_null);
        let to_tree = if running { loc.snapshots.capture().await.map(|t| t.0).or(recorded) } else { recorded };
        let Some(to_tree) = to_tree else { return Ok(vec![]) };
        let diffs = loc
            .snapshots
            .diff(
                &oc_git::git::TreeId(from_tree),
                &oc_git::git::TreeId(to_tree),
                Some(context.map(|c| c.max(0) as u64).unwrap_or(oc_git::patch::PATCH_CONTEXT_LINES)),
                None,
            )
            .await
            .map_err(|e| ApiError::unknown(format!("failed to diff session turn ({id}): {e}")))?;
        conv(diffs)
    }

    // ---- instruction entries ---------------------------------------------------------------

    pub async fn instruction_list(&self, id: &str) -> Result<Vec<api::InstructionEntryInfo>, ApiError> {
        self.session_scope(id).await?;
        store::instruction_entries(&self.app.db, id, false)
            .map_err(defect)?
            .into_iter()
            .map(|(key, value, _)| from_json(json!({"key": key, "value": value.unwrap_or(Value::Null)})))
            .collect()
    }

    pub async fn instruction_put(&self, id: &str, key: &str, value: Value) -> Result<(), ApiError> {
        self.session_scope(id).await?;
        const MAX: i64 = 256 * 1024;
        let actual = value.to_string().len() as i64;
        if actual > MAX {
            return Err(ApiError::instruction_entry_too_large(
                actual,
                MAX,
                format!("Instruction entry value is {actual} bytes; the limit is {MAX} bytes"),
            ));
        }
        store::instruction_put(&self.app.db, id, key, &value).map_err(defect)
    }

    pub async fn instruction_remove(&self, id: &str, key: &str) -> Result<(), ApiError> {
        self.session_scope(id).await?;
        store::instruction_remove(&self.app.db, id, key).map_err(defect)
    }

    // ---- skills / commands --------------------------------------------------------------------

    pub async fn activate_skill(&self, id: &str, skill_id: &str, resume: Option<bool>) -> Result<(), ApiError> {
        let (_, loc) = self.session_scope(id).await?;
        let skills = self.skills(&loc).await;
        let Some(skill) = skills.into_iter().find(|s| s.id == skill_id) else {
            return Err(ApiError::skill_not_found(skill_id));
        };
        self.app
            .bus
            .commit_one(SessionEvent::SkillActivated { session_id: id.into(), id: skill.id, name: skill.name, text: skill.content })
            .map_err(|e| defect(format!("{e:#}")))?;
        if resume != Some(false)
            && let Some(c) = self.app.coordinator.get().cloned() {
                let key = id.to_string();
                tokio::spawn(async move {
                    c.run(&key).await;
                });
            }
        Ok(())
    }

    pub async fn command(&self, id: &str, body: api::SessionCommandBody) -> Result<(), ApiError> {
        let (session, loc) = self.session_scope(id).await?;
        // `SessionCommand.execute`: `Plugin.awaitActivation`, then `Command.execute`.
        self.app.plugins.await_activation(&loc.key).await;
        let commands = self.commands(&loc).await;
        let Some((_, def)) = commands.get(&body.name) else {
            return Err(ApiError::command_not_found(&body.name, format!("Command not found: {}", body.name)));
        };
        if let crate::catalog::CommandDef::Plugin(handle) = def {
            let mut prompt = json!({ "text": body.text });
            if let Some(f) = &body.files {
                prompt["files"] = serde_json::to_value(f).unwrap_or_default();
            }
            if let Some(a) = &body.agents {
                prompt["agents"] = serde_json::to_value(a).unwrap_or_default();
            }
            if let Some(sk) = &body.skills {
                prompt["skills"] = serde_json::to_value(sk).unwrap_or_default();
            }
            let delivery = body.delivery.as_ref().map(|d| serde_json::to_value(d).unwrap_or_default()).unwrap_or(json!("steer"));
            let invocation = json!({ "sessionID": id, "prompt": prompt, "delivery": delivery });
            return self.app.plugins.execute_command(&loc.key, handle, invocation).await.map_err(|message| {
                tracing::error!(command = body.name, error = message, "command execution failed");
                ApiError::command_execution(&body.name, message)
            });
        }
        self.run_command(id, session, loc.clone(), def, body).await
    }

    /// A native command invoked by a plugin through its `rs:command.<name>` handle.
    pub async fn native_command(&self, _directory: &str, name: &str, invocation: Value) -> Result<(), ApiError> {
        let id = invocation.get("sessionID").and_then(Value::as_str).unwrap_or_default().to_string();
        let (session, loc) = self.session_scope(&id).await?;
        let commands = self.native_commands(&loc).await;
        let Some((_, def)) = commands.get(name) else {
            return Err(ApiError::command_not_found(name, format!("Command not found: {name}")));
        };
        let prompt = invocation.get("prompt").cloned().unwrap_or_default();
        let body = api::SessionCommandBody {
            name: name.to_string(),
            text: prompt.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
            files: prompt.get("files").cloned().and_then(|v| serde_json::from_value(v).ok()),
            agents: prompt.get("agents").cloned().and_then(|v| serde_json::from_value(v).ok()),
            skills: prompt.get("skills").cloned().and_then(|v| serde_json::from_value(v).ok()),
            delivery: invocation.get("delivery").cloned().and_then(|v| serde_json::from_value(v).ok()),
        };
        self.run_command(&id, session, loc.clone(), def, body).await
    }

    async fn run_command(
        &self,
        id: &str,
        session: oc_schema::types::SessionInfo,
        loc: Arc<crate::locations::Loc>,
        def: &crate::catalog::CommandDef,
        body: api::SessionCommandBody,
    ) -> Result<(), ApiError> {
        let name = body.name.clone();
        let failed = |message: String| {
            tracing::error!(command = name, error = message, "command execution failed");
            ApiError::command_execution(&name, message)
        };
        let project_dir = loc.project_directory().display().to_string();
        let mut agent_switch: Option<String> = None;
        let mut model_switch: Option<ModelRef> = None;
        let text = match def {
            crate::catalog::CommandDef::Plugin(_) => return Err(failed("Plugin commands run in the plugin host".into())),
            crate::catalog::CommandDef::Init => {
                crate::catalog::append(&crate::catalog::PROMPT_INITIALIZE.replace("${path}", &project_dir), &body.text)
            }
            crate::catalog::CommandDef::Review => {
                crate::catalog::append(&crate::catalog::PROMPT_REVIEW.replace("${path}", &project_dir), &body.text)
            }
            crate::catalog::CommandDef::Mcp(prompt) => {
                let args = oc_config::template::parse_arguments(&body.text);
                let map: indexmap::IndexMap<String, String> = prompt
                    .arguments
                    .iter()
                    .enumerate()
                    .map(|(i, a)| (a.name.clone(), args.get(i).cloned().unwrap_or_default()))
                    .collect();
                let manager = self.mcp(&loc).await.ok_or_else(|| failed("MCP is not available".into()))?;
                let result = manager.prompt(&prompt.server, &prompt.name, Some(map)).await.map_err(|e| failed(e.to_string()))?;
                let Some(result) = result else {
                    return Err(failed(format!("MCP prompt not found: {}:{}", prompt.server, prompt.name)));
                };
                result
                    .messages
                    .iter()
                    .map(|m| crate::catalog::prompt_message_text(m.get("content").unwrap_or(&Value::Null)))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim()
                    .to_string()
            }
            crate::catalog::CommandDef::Config(command) => {
                let agent = command.agent.clone();
                let agents = self.agents(&loc).await;
                let agent_info = agent.as_ref().and_then(|a| agents.items.get(a)).cloned();
                if let Some(a) = &agent
                    && agent_info.is_none() {
                        return Err(failed(format!("Agent not found: {a}")));
                    }
                let model = match &command.model {
                    Some(m) => Some(ModelRef { id: m.model.clone(), provider_id: m.provider_id.clone(), variant: m.variant.clone() }),
                    None => agent_info.as_ref().and_then(|a| a.model.clone()),
                };
                let subagent = command.subagent.or(command.subtask).unwrap_or(false)
                    || agent_info.as_ref().is_some_and(|a| a.mode == oc_core::agent::Mode::Subagent);
                let shell = crate::catalog::template_shell(&loc);
                let cwd = loc.directory().to_path_buf();
                let text = oc_config::template::evaluate(&command.template, &body.text, |source| {
                    let shell = shell.clone();
                    let cwd = cwd.clone();
                    async move {
                        let out = tokio::process::Command::new(&shell)
                            .arg("-c")
                            .arg(&source)
                            .current_dir(&cwd)
                            .stdin(std::process::Stdio::null())
                            .output()
                            .await
                            .map_err(|e| e.to_string())?;
                        let mut bytes = out.stdout;
                        bytes.extend(out.stderr);
                        Ok(String::from_utf8_lossy(&bytes).into_owned())
                    }
                })
                .await
                .map_err(failed)?;
                if subagent {
                    let description = command.description.clone().unwrap_or_else(|| name.clone());
                    return self
                        .subagent_command(&loc, &session, agent, model, description, text, body)
                        .await
                        .map_err(|e| match e {
                            e @ ApiError::Typed { .. } => e,
                            other => failed(other.to_string()),
                        });
                }
                if let Some(a) = agent.filter(|a| session.agent.as_deref() != Some(a)) {
                    agent_switch = Some(a);
                }
                model_switch = model;
                text
            }
        };
        if let Some(a) = agent_switch {
            self.app.sessions.switch_agent(id, &a).map_err(session_error)?;
        }
        if let Some(m) = model_switch {
            self.app.sessions.switch_model(id, m).map_err(session_error)?;
        }
        let prompt = api::SessionPromptBody {
            id: None,
            text,
            files: body.files,
            agents: body.agents,
            skills: body.skills,
            metadata: None,
            delivery: body.delivery,
            resume: None,
        };
        self.prompt(id, prompt).await.map(|_| ()).map_err(|e| match e {
            e @ ApiError::Typed { .. } => e,
            other => failed(other.to_string()),
        })
    }

    /// `Session.background`: moves backgroundable jobs; without a job service nothing is running.
    /// `Session.background`: move blocking jobs into background observation and tell the model.
    pub async fn background(&self, id: &str) -> Result<(), ApiError> {
        self.session(id)?;
        let jobs = self.app.jobs.background_all(id, None);
        if jobs.is_empty() {
            return Ok(());
        }
        let mut lines = vec![
            "User requested that active blocking work be moved to the background.".to_string(),
            String::new(),
            "Backgrounded work:".to_string(),
        ];
        for job in &jobs {
            let title = job.title.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| job.id.clone());
            lines.push(format!("- {}: {}", job.kind, title));
        }
        lines.push(String::new());
        lines.push("The backgrounded work is still unfinished. Move on to other work if you can. If there is nothing else useful to do, finish your response. Do not wait, sleep, poll, or report the backgrounded work as complete until a later completion notification is added to the conversation.".to_string());
        self.app
            .sessions
            .synthetic(id, None, lines.join("\n"), None, None, None)
            .await
            .map_err(session_error)?;
        if self.session(id)?.revert.is_none() {
            self.wake(id, Scope::Input);
        }
        Ok(())
    }

    /// Config command run as a subagent: child session + background `SubagentJob`.
    #[allow(clippy::too_many_arguments)]
    async fn subagent_command(
        &self,
        loc: &crate::locations::Loc,
        parent: &SessionInfo,
        agent: Option<String>,
        model: Option<ModelRef>,
        description: String,
        text: String,
        body: api::SessionCommandBody,
    ) -> Result<(), ApiError> {
        let wanted = agent.clone().or(parent.agent.clone());
        let selected = self
            .agents(loc)
            .await
            .select(wanted.as_deref())
            .cloned()
            .ok_or_else(|| ApiError::defect(format!("Agent not found: {}", wanted.unwrap_or_default())))?;
        let child = self
            .app
            .sessions
            .create(CreateInput {
                parent_id: Some(parent.id.clone()),
                title: Some(description.clone()),
                agent: Some(selected.id.clone()),
                model: model.or(selected.model.clone()).or(parent.model.clone()),
                ..Default::default()
            })
            .await
            .map_err(session_error)?;
        let prompt = api::SessionPromptBody {
            id: None,
            text: [oc_config::sources::SUBAGENT_PREFIX, &text].join("\n"),
            files: body.files,
            agents: body.agents,
            skills: body.skills,
            metadata: None,
            delivery: None,
            resume: Some(false),
        };
        self.prompt(&child.id, prompt).await?;
        let recovery = oc_tools::host::JobRecovery::Subagent {
            parent_session_id: parent.id.clone(),
            child_session_id: child.id.clone(),
            agent: selected.id.clone(),
            description,
        };
        use oc_tools::host::ToolHost;
        let host = oc_core::host::CoreHost { app: self.app.clone(), skills: Arc::new(vec![]), mcp: None, location: None };
        host.subagent_job_start(recovery.clone()).await.map_err(ApiError::defect)?;
        host.subagent_job_background(recovery).await.map_err(ApiError::defect)?;
        Ok(())
    }

    pub fn set_environment(&self, id: &str, variables: IndexMap<String, String>) -> Result<(), ApiError> {
        self.session(id)?;
        self.app.environments.lock().insert(id.to_string(), variables.into_iter().collect());
        Ok(())
    }
}

/// `{type:"running"}` map of active drains.
pub fn active_map(ids: Vec<String>) -> Map<String, Value> {
    ids.into_iter().map(|id| (id, json!({"type": "running"}))).collect()
}
