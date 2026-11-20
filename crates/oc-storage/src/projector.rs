//! Durable event commit and session projection (`core/src/bus.ts` commitDurableEvent,
//! `core/src/session/projector.ts`, `core/src/session/message-updater.ts`).

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{Map, Value};

use oc_schema::event::{Delivery, InterruptReason, SessionEvent};
use oc_schema::id::message_from_event;
use oc_schema::message::*;
use oc_schema::types::*;
use oc_schema::Message;

use crate::db::{now_ms, Database};

/// A durable event being committed.
pub struct Committing<'a> {
    pub id: &'a str,
    pub created: u64,
    pub metadata: Option<&'a Metadata>,
    pub event: &'a SessionEvent,
    /// `SessionBlob.enabled` where the event projects: `experimental.media_blob` of the session's location (the
    /// parent's for `session.forked`, `ForkMedia`). On, inline media is stored in `blob` behind `media:` refs.
    pub media: bool,
    /// `partRowsEnabled`: `experimental.part_rows` of the config the event projects under (the session's location;
    /// off outside any location). On, each assistant part is its own `session_part` row written by the part methods
    /// and envelope updates leave the row's `content` alone; off, every assistant event rewrites the whole message
    /// (into the part rows the message already has, if any).
    pub part_rows: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("Session already projected")]
    SessionAlreadyProjected,
    #[error("Lifecycle conflict for {0}")]
    LifecycleConflict(String),
    #[error("{0}")]
    Other(String),
}

/// Sequence handling shared by every durable aggregate.
pub fn latest_seq(c: &Connection, aggregate: &str) -> rusqlite::Result<i64> {
    Ok(c.query_row("SELECT seq FROM event_sequence WHERE aggregate_id = ?", [aggregate], |r| r.get::<_, i64>(0))
        .optional()?
        .unwrap_or(-1))
}

pub fn store_seq(c: &Connection, aggregate: &str, seq: i64) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO event_sequence (aggregate_id, seq, owner_id) VALUES (?, ?, NULL) ON CONFLICT(aggregate_id) DO UPDATE SET seq = max(seq, excluded.seq)",
        params![aggregate, seq],
    )?;
    Ok(())
}

/// `Bus.reserveSequence`
pub fn reserve_seq(c: &Connection, aggregate: &str, seq: i64) -> rusqlite::Result<()> {
    store_seq(c, aggregate, seq)
}

impl Database {
    /// Commit a batch of durable session events for one session in one immediate transaction.
    /// `hook` runs after each event's projection (publisher `commit(seq)` hooks).
    /// Returns the assigned sequence numbers.
    pub fn commit_session_events(
        &self,
        events: &[Committing<'_>],
        mut hook: impl FnMut(&Connection, usize, i64) -> anyhow::Result<()>,
    ) -> anyhow::Result<Vec<i64>> {
        let Some(first) = events.first() else { return Ok(vec![]) };
        let aggregate = first.event.session_id().to_string();
        if events.iter().any(|e| e.event.session_id() != aggregate) {
            anyhow::bail!("Published events must belong to the same aggregate");
        }
        self.immediate(|c| {
            let latest = latest_seq(c, &aggregate)?;
            let mut seqs = Vec::with_capacity(events.len());
            for (i, e) in events.iter().enumerate() {
                let seq = latest + 1 + i as i64;
                project(c, e, seq)?;
                hook(c, i, seq)?;
                seqs.push(seq);
            }
            let last = *seqs.last().unwrap();
            if events.len() == 1 {
                store_seq(c, &aggregate, last)?;
            } else {
                c.execute(
                    "INSERT INTO event_sequence (aggregate_id, seq, owner_id) VALUES (?, ?, NULL) ON CONFLICT(aggregate_id) DO UPDATE SET seq = excluded.seq",
                    params![aggregate, last],
                )?;
            }
            Ok(seqs)
        })
    }

    /// Commit one durable event.
    pub fn commit_session_event(&self, e: Committing<'_>) -> anyhow::Result<i64> {
        Ok(self.commit_session_events(std::slice::from_ref(&e), |_, _, _| Ok(()))?[0])
    }

    /// Session usage totals for `session.usage.updated`.
    pub fn session_usage(&self, session_id: &str) -> anyhow::Result<Option<(f64, TokenUsage)>> {
        self.with(|c| {
            c.query_row(
                "SELECT cost, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write FROM session_v2 WHERE id = ?",
                [session_id],
                |r| {
                    Ok((
                        r.get::<_, f64>(0)?,
                        TokenUsage {
                            input: r.get::<_, i64>(1)? as u64,
                            output: r.get::<_, i64>(2)? as u64,
                            reasoning: r.get::<_, i64>(3)? as u64,
                            cache: CacheUsage { read: r.get::<_, i64>(4)? as u64, write: r.get::<_, i64>(5)? as u64 },
                        },
                    ))
                },
            )
            .optional()
        })
    }
}

fn json_opt<T: serde::Serialize>(v: &Option<T>) -> Option<String> {
    v.as_ref().map(|v| serde_json::to_string(v).unwrap())
}

fn touch(c: &Connection, session: &str, created: u64) -> rusqlite::Result<()> {
    c.execute("UPDATE session_v2 SET time_updated = ? WHERE id = ?", params![created as i64, session])?;
    Ok(())
}

fn apply_usage(c: &Connection, session: &str, cost: f64, t: &TokenUsage, updated: Option<u64>) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE session_v2 SET cost = cost + ?, tokens_input = tokens_input + ?, tokens_output = tokens_output + ?, tokens_reasoning = tokens_reasoning + ?, tokens_cache_read = tokens_cache_read + ?, tokens_cache_write = tokens_cache_write + ?, time_updated = coalesce(?, time_updated) WHERE id = ?",
        params![
            cost,
            t.input as i64,
            t.output as i64,
            t.reasoning as i64,
            t.cache.read as i64,
            t.cache.write as i64,
            updated.map(|u| u as i64),
            session
        ],
    )?;
    Ok(())
}

fn fork_title(value: Option<String>) -> Option<String> {
    let value = value?;
    let re = regex::Regex::new(r"^(.+) \(fork #(\d+)\)$").unwrap();
    if let Some(m) = re.captures(&value) {
        let n: u64 = m[2].parse().unwrap_or(0);
        return Some(format!("{} (fork #{})", &m[1], n + 1));
    }
    Some(format!("{value} (fork #1)"))
}

fn project(c: &Connection, e: &Committing<'_>, seq: i64) -> anyhow::Result<()> {
    use SessionEvent as E;
    let created = e.created;
    match e.event {
        E::Created {
            session_id,
            project_id,
            location,
            subpath,
            parent_id,
            slug,
            title,
            agent,
            model,
            metadata,
            permissions,
            version,
        } => {
            let n = c.execute(
                "INSERT INTO session_v2 (id, project_id, workspace_id, parent_id, slug, directory, path, title, agent, model, metadata, permission, version, time_created, time_updated) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT DO NOTHING",
                params![
                    session_id,
                    project_id,
                    location.workspace_id,
                    parent_id,
                    slug,
                    location.directory,
                    subpath,
                    title,
                    agent,
                    json_opt(model),
                    json_opt(metadata),
                    json_opt(permissions),
                    version,
                    created as i64,
                    created as i64
                ],
            )?;
            if n == 0 {
                return Err(ProjectError::SessionAlreadyProjected.into());
            }
        }
        E::Moved { session_id, location, project_id, subpath } => {
            run(c, e, seq)?;
            c.execute(
                "UPDATE session_v2 SET directory = ?, path = ?, project_id = CASE WHEN ? = '' THEN project_id ELSE ? END, workspace_id = ?, time_updated = ? WHERE id = ?",
                params![location.directory, subpath, project_id, project_id, location.workspace_id, created as i64, session_id],
            )?;
        }
        E::Deleted { session_id } => {
            crate::stored::release_session(c, session_id)?;
            c.execute("DELETE FROM session_v2 WHERE id = ?", [session_id])?;
        }
        E::AgentSelected { session_id, agent, .. } => {
            run(c, e, seq)?;
            c.execute("UPDATE session_v2 SET agent = ?, time_updated = ? WHERE id = ?", params![agent, created as i64, session_id])?;
        }
        E::ModelSelected { session_id, model, .. } => {
            run(c, e, seq)?;
            c.execute(
                "UPDATE session_v2 SET model = ?, time_updated = ? WHERE id = ?",
                params![serde_json::to_string(model)?, created as i64, session_id],
            )?;
        }
        E::Renamed { session_id, title } => {
            c.execute("UPDATE session_v2 SET title = ?, time_updated = ? WHERE id = ?", params![title, created as i64, session_id])?;
        }
        E::MetadataUpdated { session_id, metadata } => {
            c.execute(
                "UPDATE session_v2 SET metadata = ?, time_updated = ? WHERE id = ?",
                params![serde_json::to_string(metadata)?, created as i64, session_id],
            )?;
        }
        E::Permissions { session_id, permissions } => {
            c.execute(
                "UPDATE session_v2 SET permission = ?, time_updated = ? WHERE id = ?",
                params![serde_json::to_string(permissions)?, created as i64, session_id],
            )?;
        }
        E::Viewed { session_id, idle } => {
            c.execute(
                "UPDATE session_v2 SET time_viewed = max(?1, coalesce(time_viewed, ?1)) WHERE id = ?2",
                params![*idle as i64, session_id],
            )?;
        }
        E::Archived { session_id } => {
            c.execute(
                "UPDATE session_v2 SET time_archived = ?1, time_updated = ?1 WHERE id = ?2",
                params![created as i64, session_id],
            )?;
        }
        E::Unarchived { session_id } => {
            c.execute(
                "UPDATE session_v2 SET time_archived = NULL, time_updated = ? WHERE id = ?",
                params![created as i64, session_id],
            )?;
        }
        E::MessageContentUpdated { .. } => run(c, e, seq)?,
        E::UsageRecorded { session_id, cost, tokens, .. } => apply_usage(c, session_id, *cost, tokens, None)?,
        E::Forked { .. } => project_fork(c, e, seq)?,
        E::InboxDelivered { session_id, inbox_id } => {
            let row: Option<(String, String)> = c
                .query_row(
                    "DELETE FROM session_inbox WHERE id = ? AND session_id = ? RETURNING type, payload",
                    params![inbox_id, session_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((kind, payload)) = row else {
                return Err(ProjectError::LifecycleConflict(inbox_id.clone()).into());
            };
            let payload: Value = serde_json::from_str(&payload)?;
            let mut msg = Map::new();
            match kind.as_str() {
                "user" => {
                    for k in ["metadata", "text", "files", "agents", "skills"] {
                        if let Some(v) = payload.get(k) {
                            msg.insert(k.into(), v.clone());
                        }
                    }
                }
                "synthetic" => {
                    for k in ["text", "description", "metadata"] {
                        if let Some(v) = payload.get(k) {
                            msg.insert(k.into(), v.clone());
                        }
                    }
                }
                _ => return Ok(()),
            }
            msg.insert("id".into(), Value::String(inbox_id.clone()));
            msg.insert("type".into(), Value::String(kind));
            msg.insert("time".into(), serde_json::json!({ "created": created }));
            let (decodable, refs) = crate::stored::without_refs(Value::Object(msg));
            let message: Message = serde_json::from_value(decodable)?;
            insert_message_with(c, session_id, seq, &message, &refs, e.media, e.created as i64)?;
        }
        E::InboxEnqueued { session_id, inbox_id, item } => {
            let exists = c
                .query_row("SELECT id FROM session_message WHERE id = ?", [inbox_id], |_| Ok(()))
                .optional()?
                .is_some();
            if exists {
                return Err(ProjectError::LifecycleConflict(inbox_id.clone()).into());
            }
            // `externalizePayload`: a user payload's inline files go to `blob` where the flag is on.
            let mut payload = item.payload.clone();
            if item.kind == "user" && e.media {
                let taken = crate::stored::externalize("user", &mut payload);
                crate::stored::save(c, &taken)?;
            }
            let n = c.execute(
                "INSERT INTO session_inbox (id, session_id, type, payload, delivery, enqueued_seq, time_created) VALUES (?,?,?,?,?,?,?) ON CONFLICT DO NOTHING",
                params![
                    inbox_id,
                    session_id,
                    item.kind,
                    payload.to_string(),
                    delivery_str(item.delivery),
                    seq,
                    created as i64
                ],
            )?;
            if n == 0 {
                return Err(ProjectError::LifecycleConflict(inbox_id.clone()).into());
            }
            crate::stored::record_refs(c, session_id, inbox_id, seq, &payload)?;
            touch(c, session_id, created)?;
        }
        E::InboxCancelled { session_id, inbox_id } => {
            let n = c.execute(
                "DELETE FROM session_inbox WHERE id = ? AND session_id = ? AND (delivery = 'queue' OR delivery = 'steer')",
                params![inbox_id, session_id],
            )?;
            if n == 0 {
                return Err(ProjectError::LifecycleConflict(inbox_id.clone()).into());
            }
            crate::stored::release_message(c, session_id, inbox_id)?;
        }
        E::InboxDeliveryChanged { session_id, inbox_id, delivery } => {
            let from = match delivery {
                Delivery::Steer => Delivery::Queue,
                Delivery::Queue => Delivery::Steer,
            };
            let n = c.execute(
                "UPDATE session_inbox SET delivery = ? WHERE id = ? AND session_id = ? AND delivery = ?",
                params![delivery_str(*delivery), inbox_id, session_id, delivery_str(from)],
            )?;
            if n == 0 {
                return Err(ProjectError::LifecycleConflict(inbox_id.clone()).into());
            }
        }
        E::ExecutionSucceeded { session_id } | E::ExecutionFailed { session_id, .. } | E::ExecutionInterrupted { session_id, .. } => {
            run(c, e, seq)?;
            let outcome = match e.event {
                E::ExecutionSucceeded { .. } => "succeeded",
                E::ExecutionFailed { .. } => "failed",
                E::ExecutionInterrupted { reason: InterruptReason::Shutdown, .. } => return Ok(()),
                _ => "interrupted",
            };
            c.execute(
                "UPDATE session_v2 SET time_idle = max(?1, coalesce(time_idle + 1, ?1)), idle_outcome = ?2 WHERE id = ?3",
                params![created as i64, outcome, session_id],
            )?;
        }
        E::InstructionsUpdated { session_id, delta, .. } => {
            run(c, e, seq)?;
            instruction_apply(c, session_id, seq, delta)?;
        }
        E::StepStarted { session_id, .. } => {
            run(c, e, seq)?;
            touch(c, session_id, created)?;
        }
        E::StepEnded { session_id, cost, tokens, .. } => {
            run(c, e, seq)?;
            apply_usage(c, session_id, *cost, tokens, Some(created))?;
        }
        E::StepFailed { session_id, cost, tokens, .. } => {
            run(c, e, seq)?;
            match (cost, tokens) {
                (Some(cost), Some(tokens)) => apply_usage(c, session_id, *cost, tokens, Some(created))?,
                _ => touch(c, session_id, created)?,
            }
        }
        E::CompactionEnded { session_id, .. } => {
            run(c, e, seq)?;
            c.execute(
                "UPDATE instruction_state SET epoch_start = ?1, through_seq = ?1, initial_values = current_values WHERE session_id = ?2",
                params![seq, session_id],
            )?;
        }
        E::RevertStaged { session_id, revert } => {
            c.execute(
                "UPDATE session_v2 SET revert = ?, time_updated = ? WHERE id = ?",
                params![serde_json::to_string(revert)?, created as i64, session_id],
            )?;
        }
        E::RevertCleared { session_id } => {
            c.execute("UPDATE session_v2 SET revert = NULL, time_updated = ? WHERE id = ?", params![created as i64, session_id])?;
        }
        E::RevertCommitted { session_id, to } => {
            let boundary: Option<i64> = c
                .query_row(
                    "SELECT seq FROM session_message WHERE session_id = ? AND id = ?",
                    params![session_id, to],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(boundary) = boundary else {
                return Err(ProjectError::Other(format!("Revert boundary message not found: {to}")).into());
            };
            c.execute("DELETE FROM session_message WHERE session_id = ? AND seq >= ?", params![session_id, boundary])?;
            c.execute("DELETE FROM session_inbox WHERE session_id = ? AND enqueued_seq >= ?", params![session_id, boundary])?;
            c.execute("UPDATE session_v2 SET revert = NULL, time_updated = ? WHERE id = ?", params![created as i64, session_id])?;
            // `InstructionState.rewind`: the fold is cleared only when the revert removed an update or the
            // epoch start; otherwise its baseline still matches the surviving history and keeps the prompt cache.
            c.execute(
                "DELETE FROM instruction_state WHERE session_id = ? AND through_seq >= ?",
                params![session_id, boundary],
            )?;
            crate::stored::release_from(c, session_id, boundary)?;
        }
        // The start of the session's latest execution; `time_updated` is left alone.
        E::ExecutionStarted { session_id } => {
            c.execute("UPDATE session_v2 SET time_busy = ? WHERE id = ?", params![created as i64, session_id])?;
        }
        _ => run(c, e, seq)?,
    }
    Ok(())
}

fn delivery_str(d: Delivery) -> &'static str {
    match d {
        Delivery::Steer => "steer",
        Delivery::Queue => "queue",
    }
}

fn instruction_apply(c: &Connection, session: &str, seq: i64, delta: &Map<String, Value>) -> anyhow::Result<()> {
    let stored: Option<String> = c
        .query_row("SELECT current_values FROM instruction_state WHERE session_id = ?", [session], |r| r.get(0))
        .optional()?;
    let mut current: Map<String, Value> =
        stored.as_deref().map(serde_json::from_str).transpose()?.unwrap_or_default();
    for (k, v) in delta {
        if v.as_str() == Some("removed") {
            current.shift_remove(k);
        } else {
            current.insert(k.clone(), v.clone());
        }
    }
    let json = Value::Object(current).to_string();
    if stored.is_none() {
        c.execute(
            "INSERT INTO instruction_state (session_id, epoch_start, through_seq, initial_values, current_values) VALUES (?, ?, ?, ?, ?)",
            params![session, seq, seq, json, json],
        )?;
    } else {
        c.execute(
            "UPDATE instruction_state SET through_seq = ?, current_values = ? WHERE session_id = ?",
            params![seq, json, session],
        )?;
    }
    Ok(())
}

fn project_fork(c: &Connection, e: &Committing<'_>, seq: i64) -> anyhow::Result<()> {
    let SessionEvent::Forked { session_id, parent_id, boundary, instructions, instruction_entries } = e.event else {
        unreachable!()
    };
    #[allow(clippy::type_complexity)]
    let parent: Option<(String, Option<String>, String, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, String)> = c
        .query_row(
            "SELECT project_id, workspace_id, directory, path, title, agent, model, metadata, permission, version FROM session_v2 WHERE id = ?",
            [parent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)),
        )
        .optional()?;
    let Some(parent) = parent else {
        return Err(ProjectError::Other(format!("Fork parent session not found: {parent_id}")).into());
    };
    let boundary_seq: Option<i64> = c
        .query_row(
            "SELECT seq FROM session_message WHERE session_id = ? AND id = ?",
            params![parent_id, boundary.message_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(boundary_seq) = boundary_seq else {
        return Err(ProjectError::Other(format!("Fork boundary message not found: {}", boundary.message_id)).into());
    };
    let cmp = match boundary.kind {
        ForkBoundaryType::Before => "<",
        ForkBoundaryType::Through => "<=",
    };
    let copied: Option<i64> = c
        .query_row(
            &format!("SELECT seq FROM session_message WHERE session_id = ? AND seq {cmp} ? ORDER BY seq DESC LIMIT 1"),
            params![parent_id, boundary_seq],
            |r| r.get(0),
        )
        .optional()?;
    let n = c.execute(
        "INSERT INTO session_v2 (id, parent_id, fork_session_id, fork_boundary, project_id, workspace_id, slug, directory, path, title, agent, model, metadata, permission, version, cost, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write, time_created, time_updated) VALUES (?, NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0, 0, 0, 0, 0, ?, ?) ON CONFLICT DO NOTHING",
        params![
            session_id,
            parent_id,
            serde_json::to_string(boundary)?,
            parent.0,
            parent.1,
            crate::session::slug(),
            parent.2,
            parent.3,
            fork_title(parent.4),
            parent.5,
            parent.6,
            parent.7,
            parent.8,
            parent.9,
            e.created as i64,
            e.created as i64
        ],
    )?;
    if n == 0 {
        return Err(ProjectError::SessionAlreadyProjected.into());
    }
    if let Some(entries) = instruction_entries {
        for entry in entries {
            c.execute(
                "INSERT INTO instruction_entry (session_id, key, value, removed, time_created, time_updated) VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    session_id,
                    entry.key,
                    entry.value.as_ref().map(|v| v.to_string()),
                    entry.removed,
                    e.created as i64,
                    e.created as i64
                ],
            )?;
        }
    }
    if let Some(copied) = copied {
        let prefix = message_from_event(e.id);
        c.execute(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data)
             SELECT ?1 || '_' || seq, ?2, type, seq, time_created, time_updated, data FROM session_message
             WHERE session_id = ?3 AND seq <= ?4
               AND (type != 'assistant' OR json_extract(data, '$.time.completed') IS NOT NULL)
               AND (type != 'shell' OR json_extract(data, '$.status') != 'running')
               AND (type != 'compaction' OR json_extract(data, '$.status') != 'running')
             ORDER BY seq",
            params![prefix, session_id, parent_id, copied],
        )?;
        fork_keep_ids(c, session_id, parent_id, &prefix)?;
        if e.media {
            fork_externalize(c, session_id)?;
        }
        fork_record_refs(c, session_id)?;
        fork_parts(c, session_id, parent_id, &prefix, copied, e.media)?;
        reserve_seq(c, session_id, copied)?;
    }
    if let Some(values) = instructions {
        let json = Value::Object(values.clone()).to_string();
        c.execute(
            "INSERT INTO instruction_state (session_id, epoch_start, through_seq, initial_values, current_values) VALUES (?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
            params![session_id, seq, seq, json, json],
        )?;
    }
    Ok(())
}

/// The parent's `session_part` rows of every copied assistant message are copied as part rows of the forked
/// message (same ordinal, type, call ID and times), their inline media moved to `blob` where the fork stores
/// media, and the refs they name recorded for the forked message (2.2.7; the envelopes stay empty).
fn fork_parts(c: &Connection, session: &str, parent: &str, prefix: &str, copied: i64, media: bool) -> anyhow::Result<()> {
    #[allow(clippy::type_complexity)]
    let parts: Vec<(i64, i64, String, Option<String>, i64, i64, String)> = {
        let mut s = c.prepare(
            "SELECT m.seq, p.ordinal, p.type, p.call_id, p.time_created, p.time_updated, p.data
             FROM session_part AS p JOIN session_message AS m ON m.id = p.message_id AND m.session_id = p.session_id
             WHERE p.session_id = ?1 AND m.seq <= ?2 AND m.type = 'assistant'
               AND json_extract(m.data, '$.time.completed') IS NOT NULL
             ORDER BY p.message_id ASC, p.ordinal ASC",
        )?;
        let rows = s
            .query_map(params![parent, copied], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        rows
    };
    let mut by_message: Vec<(i64, Vec<Value>)> = Vec::new();
    for (seq, ordinal, kind, call_id, created, updated, data) in parts {
        let mut data = data;
        if media {
            let part: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
            let mut wrapped = serde_json::json!({ "content": [part] });
            let taken = crate::stored::split(&mut wrapped);
            if !taken.is_empty() {
                crate::stored::save(c, &taken)?;
                data = wrapped["content"][0].to_string();
            }
        }
        let message = format!("{prefix}_{seq}");
        c.execute(
            "INSERT INTO session_part (message_id, session_id, ordinal, type, call_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![message, session, ordinal, kind, call_id, created, updated, data],
        )?;
        let value: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        match by_message.last_mut() {
            Some((last, list)) if *last == seq => list.push(value),
            _ => by_message.push((seq, vec![value])),
        }
    }
    for (seq, content) in by_message {
        crate::stored::record_refs(c, session, &format!("{prefix}_{seq}"), seq, &serde_json::json!({ "content": content }))?;
    }
    Ok(())
}

/// `SessionBlob.externalizeRow` for every copied row (`ForkMedia`): the child stores the parent's inline media
/// as blobs; the parent's rows are never rewritten. (Upstream copies a summary's original data when it
/// re-points its `keepID`; summaries carry no media.)
fn fork_externalize(c: &Connection, session: &str) -> anyhow::Result<()> {
    let rows: Vec<(String, String, String)> = {
        let mut s = c.prepare("SELECT id, type, data FROM session_message WHERE session_id = ? ORDER BY seq")?;
        let rows = s.query_map([session], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    for (id, kind, data) in rows {
        if kind == "compaction" {
            continue;
        }
        let Ok(mut value) = serde_json::from_str::<Value>(&data) else { continue };
        let taken = crate::stored::split(&mut value);
        if taken.is_empty() {
            continue;
        }
        crate::stored::save(c, &taken)?;
        c.execute("UPDATE session_message SET data = ? WHERE session_id = ? AND id = ?", params![value.to_string(), session, id])?;
    }
    Ok(())
}

/// `SessionBlob.recordRefs` for every copied row: the child keeps the blobs its copies name alive.
fn fork_record_refs(c: &Connection, session: &str) -> anyhow::Result<()> {
    if c.query_row("SELECT 1 FROM blob LIMIT 1", [], |_| Ok(())).optional()?.is_none() {
        return Ok(());
    }
    let rows: Vec<(i64, String, String)> = {
        let mut s = c.prepare("SELECT seq, id, data FROM session_message WHERE session_id = ? AND data LIKE '%\"media:%'")?;
        let rows = s.query_map([session], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    for (seq, id, data) in rows {
        let data: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        crate::stored::record_refs(c, session, &id, seq, &data)?;
    }
    Ok(())
}

/// A summary references the first message it kept (`metadata.keepID`), which the fork copied under a new ID.
fn fork_keep_ids(c: &Connection, session: &str, parent: &str, prefix: &str) -> anyhow::Result<()> {
    let rows: Vec<(String, String)> = {
        let mut s = c.prepare(
            "SELECT id, json_extract(data, '$.metadata.keepID') FROM session_message
             WHERE session_id = ? AND type = 'compaction' AND json_type(data, '$.metadata.keepID') = 'text'",
        )?;
        let rows = s.query_map([session], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    for (id, keep) in rows {
        // `Schema.is(SessionMessage.ID)`: anything else is not a message reference.
        if !keep.starts_with("msg_") {
            continue;
        }
        let kept: Option<i64> = c
            .query_row("SELECT seq FROM session_message WHERE session_id = ? AND id = ?", params![parent, keep], |r| r.get(0))
            .optional()?;
        let Some(kept) = kept else { continue };
        c.execute(
            "UPDATE session_message SET data = json_set(data, '$.metadata.keepID', ?) WHERE session_id = ? AND id = ?",
            params![format!("{prefix}_{kept}"), session, id],
        )?;
    }
    Ok(())
}

fn insert_message(c: &Connection, e: &Committing<'_>, session: &str, seq: i64, m: &Message) -> rusqlite::Result<()> {
    insert_message_with(c, session, seq, m, &[], e.media, e.created as i64)
}

/// `insertMessage`: the row (with `refs`, the user files a payload kept as refs, put back), its inline media
/// externalized where the flag is on, then the refs it names recorded. An assistant message's content goes to
/// `session_part` rows (ordinals from 0) and the row keeps an empty `content` envelope (2.2.7).
#[allow(clippy::too_many_arguments)]
fn insert_message_with(
    c: &Connection,
    session: &str,
    seq: i64,
    m: &Message,
    refs: &[(usize, String)],
    media: bool,
    event_created: i64,
) -> rusqlite::Result<()> {
    let mut data = m.to_row_data();
    if !refs.is_empty() || media {
        let mut value: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        crate::stored::restore_refs(&mut value, refs);
        if media {
            let taken = crate::stored::externalize(m.kind(), &mut value);
            crate::stored::save(c, &taken)?;
        }
        data = value.to_string();
    }
    let mut row_data = data.clone();
    let mut parts: Vec<Value> = Vec::new();
    if matches!(m, Message::Assistant(_)) {
        let mut row = crate::stored::parse_object(&data);
        if let Some(Value::Array(content)) = row.get("content")
            && !content.is_empty()
        {
            parts = content.clone();
            row.insert("content".into(), Value::Array(vec![]));
            row_data = Value::Object(row).to_string();
        }
    }
    c.execute(
        "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![m.id(), session, m.kind(), seq, m.created() as i64, now_ms(), row_data],
    )?;
    crate::stored::insert_parts(c, session, m.id(), m.created() as i64, event_created, &parts)?;
    if data.contains("\"media:") {
        let value: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        crate::stored::record_refs(c, session, m.id(), seq, &value)?;
    }
    Ok(())
}

/// `updateMessage`: a message whose parts a newer release moved to `session_part` keeps an empty
/// `content` envelope in its row and has the parts written there; others are written inline. A
/// content rewrite also syncs the blob refs the message names.
fn update_message(c: &Connection, e: &Committing<'_>, session: &str, m: &Message) -> rusqlite::Result<()> {
    let mut data = m.to_row_data();
    // `externalize`: tool `data:` files of an assistant message go to `blob` where the flag is on.
    let mut moved = false;
    if e.media && matches!(m, Message::Assistant(_)) && data.contains("data:") {
        let mut value: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        let taken = crate::stored::externalize(m.kind(), &mut value);
        if !taken.is_empty() {
            crate::stored::save(c, &taken)?;
            data = value.to_string();
            moved = true;
        }
    }
    let mut row: Map<String, Value> = crate::stored::parse_object(&data);
    let parts = match (m, row.contains_key("content")) {
        (Message::Assistant(_), true) => crate::stored::part_rows(c, m.id())?,
        _ => Vec::new(),
    };
    let content = if parts.is_empty() {
        None
    } else {
        let content = row.insert("content".into(), Value::Array(vec![]));
        Some(content.and_then(|v| if let Value::Array(a) = v { Some(a) } else { None }).unwrap_or_default())
    };
    let stored = if content.is_some() { Value::Object(row.clone()).to_string() } else { data.clone() };
    let seq: Option<i64> = c
        .query_row(
            "UPDATE session_message SET type = ?, time_created = ?, data = ?, time_updated = ? WHERE id = ? AND session_id = ? RETURNING seq",
            params![m.kind(), m.created() as i64, stored, now_ms(), m.id(), session],
            |r| r.get(0),
        )
        .optional()?;
    let Some(seq) = seq else { return Ok(()) };
    if let Some(content) = content {
        crate::stored::write_parts(c, session, m.id(), e.created as i64, &parts, &content)?;
    }
    if matches!(e.event, SessionEvent::MessageContentUpdated { .. }) {
        let value: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        crate::stored::sync_refs(c, session, m.id(), seq, &value)?;
    } else if moved {
        let value: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        crate::stored::record_refs(c, session, m.id(), seq, &value)?;
    }
    Ok(())
}

fn decode(id: String, kind: String, data: String) -> Option<Message> {
    Message::from_row(&id, &kind, &data).ok()
}

/// Read one message row for the updater. `parts`: fold the message's `session_part` rows into its `content` first
/// (the `experimental.part_rows` off reads, `withParts`); the part-row path decodes the row as stored, so an
/// assistant's `content` is whatever the row still holds inline. Media refs are kept either way.
fn query_message(c: &Connection, parts: bool, sql: &str, p: impl rusqlite::Params) -> rusqlite::Result<Option<Message>> {
    let row = c
        .query_row(sql, p, |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
        .optional()?;
    let Some(row) = row else { return Ok(None) };
    let mut rows = [row];
    if parts {
        crate::stored::with_parts(c, &mut rows)?;
    }
    let [(i, k, d)] = rows;
    Ok(decode(i, k, d))
}

/// `assistantReads.getAssistant` of the path `e` projects with.
fn get_assistant(c: &Connection, e: &Committing<'_>, session: &str, id: &str) -> rusqlite::Result<Option<Box<Assistant>>> {
    Ok(match query_message(
        c,
        !e.part_rows,
        "SELECT id, type, data FROM session_message WHERE id = ? AND session_id = ? AND type = 'assistant'",
        params![id, session],
    )? {
        Some(Message::Assistant(a)) => Some(a),
        _ => None,
    })
}

/// `assistantReads.getCurrentAssistant`: the newest assistant row, when it is still incomplete.
fn current_assistant(c: &Connection, e: &Committing<'_>, session: &str) -> rusqlite::Result<Option<Box<Assistant>>> {
    Ok(match query_message(
        c,
        !e.part_rows,
        "SELECT id, type, data FROM session_message WHERE session_id = ? AND type = 'assistant' ORDER BY seq DESC LIMIT 1",
        [session],
    )? {
        Some(Message::Assistant(a)) if a.time.completed.is_none() => Some(a),
        _ => None,
    })
}

fn running_compaction(c: &Connection, session: &str) -> rusqlite::Result<Option<Box<Compaction>>> {
    Ok(match query_message(
        c,
        false,
        "SELECT id, type, data FROM session_message WHERE session_id = ? AND type = 'compaction' AND json_extract(data, '$.status') = 'running' ORDER BY seq DESC LIMIT 1",
        [session],
    )? {
        Some(Message::Compaction(m)) => Some(m),
        _ => None,
    })
}

/// `updateAssistant` / `updateShell` / `updateCompaction`: with part rows off, the whole message
/// ([`update_message`]); on, only the envelope ([`update_envelope`]).
fn update(c: &Connection, e: &Committing<'_>, session: &str, m: &Message) -> rusqlite::Result<()> {
    if e.part_rows { update_envelope(c, e, session, m) } else { update_message(c, e, session, m) }
}

/// `updateMessage` of the part-row path. Assistant content is written only by the part methods, so an assistant's
/// envelope keeps whatever `content` its row already holds: inline for a legacy row that was never split, `[]` once
/// it was. `time_created` follows the message only on `session.step.started`; no media moves and no refs change.
fn update_envelope(c: &Connection, e: &Committing<'_>, session: &str, m: &Message) -> rusqlite::Result<()> {
    let started = matches!(e.event, SessionEvent::StepStarted { .. });
    let (data, assistant) = match m {
        Message::Assistant(a) => {
            let mut envelope = (**a).clone();
            envelope.content = Vec::new();
            (Message::Assistant(Box::new(envelope)).to_row_data(), true)
        }
        _ => (m.to_row_data(), false),
    };
    c.execute(
        "UPDATE session_message SET
           time_created = CASE WHEN ?1 THEN ?2 ELSE time_created END,
           data = CASE WHEN ?3 THEN json_set(?4, '$.content', json(coalesce(data -> '$.content', '[]'))) ELSE ?4 END,
           time_updated = ?5
         WHERE id = ?6 AND session_id = ?7",
        params![started, m.created() as i64, assistant, data, now_ms(), m.id(), session],
    )?;
    Ok(())
}

/// `updateOwnedAssistant`: reads the message, applies `f` and writes it back (`updateAssistant`).
fn with_assistant(
    c: &Connection,
    e: &Committing<'_>,
    session: &str,
    id: &str,
    f: impl FnOnce(&mut Assistant),
) -> rusqlite::Result<()> {
    if let Some(mut a) = get_assistant(c, e, session, id)? {
        f(&mut a);
        update(c, e, session, &Message::Assistant(a))?;
    }
    Ok(())
}

/// Which part `updateAssistantPart` changes: the last one of its type, with this call ID for a tool, and still
/// incomplete for reasoning.
enum PartMatch<'a> {
    Text,
    Tool(&'a str),
    IncompleteReasoning,
}

impl PartMatch<'_> {
    fn kind(&self) -> &'static str {
        match self {
            PartMatch::Text => "text",
            PartMatch::Tool(_) => "tool",
            PartMatch::IncompleteReasoning => "reasoning",
        }
    }

    fn matches(&self, part: &AssistantContent) -> bool {
        match (self, part) {
            (PartMatch::Text, AssistantContent::Text { .. }) => true,
            (PartMatch::Tool(call), AssistantContent::Tool { id, .. }) => id == call,
            (PartMatch::IncompleteReasoning, AssistantContent::Reasoning { time, .. }) => {
                time.as_ref().and_then(|t| t.completed).is_none()
            }
            _ => false,
        }
    }
}

/// `encodePart`: a part as its `session_part.data` JSON.
fn encode_part(part: &AssistantContent) -> Value {
    serde_json::to_value(part).expect("part serializes")
}

/// `type` and `call_id` columns of a part row.
fn part_columns(part: &Value) -> (&str, Option<&str>) {
    let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
    (kind, if kind == "tool" { part.get("id").and_then(Value::as_str) } else { None })
}

/// `SessionBlob.externalizePart`: with `experimental.media_blob` on, a tool part's `data:` result files go to `blob`.
/// Returns whether anything moved.
fn externalize_part(c: &Connection, e: &Committing<'_>, part: &mut Value) -> rusqlite::Result<bool> {
    if !e.media {
        return Ok(false);
    }
    let mut wrapped = serde_json::json!({ "content": [std::mem::take(part)] });
    let taken = crate::stored::externalize("assistant", &mut wrapped);
    *part = wrapped["content"][0].take();
    if taken.is_empty() {
        return Ok(false);
    }
    crate::stored::save(c, &taken)?;
    Ok(true)
}

fn insert_part_row(
    c: &Connection,
    session: &str,
    message: &str,
    ordinal: i64,
    created: i64,
    updated: i64,
    data: &Value,
) -> rusqlite::Result<()> {
    let (kind, call_id) = part_columns(data);
    c.execute(
        "INSERT INTO session_part (message_id, session_id, ordinal, type, call_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![message, session, ordinal, kind, call_id, created, updated, data.to_string()],
    )?;
    Ok(())
}

/// `prepareAssistantParts`, the only place a legacy inline assistant row moves to `session_part`, called right before
/// a part write. With `carry`, the row's inline parts are appended after any part rows it already has (an older build
/// that wrote inline content while part rows existed leaves both shapes on one message, and a downgrade and re-upgrade
/// loses nothing); without, they are dropped, for a write that replaces them all. The row is then rewritten with an
/// empty `content`. Returns the `seq` the message's part refs belong to, `None` when there is no such assistant row.
fn prepare_parts(c: &Connection, e: &Committing<'_>, session: &str, message: &str, carry: bool) -> anyhow::Result<Option<i64>> {
    let row: Option<(i64, Option<i64>)> = c
        .query_row(
            "SELECT seq, json_array_length(data, '$.content') FROM session_message WHERE id = ? AND session_id = ? AND type = 'assistant'",
            params![message, session],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((seq, inline)) = row else { return Ok(None) };
    if inline.unwrap_or(0) > 0 {
        let (created, data): (i64, String) = c.query_row(
            "SELECT time_created, data FROM session_message WHERE id = ?",
            [message],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let Message::Assistant(mut a) = Message::from_row(message, "assistant", &data)? else { unreachable!() };
        if carry {
            let last: Option<i64> =
                c.query_row("SELECT max(ordinal) FROM session_part WHERE message_id = ?", [message], |r| r.get(0))?;
            let first = last.unwrap_or(-1) + 1;
            for (index, part) in a.content.iter().enumerate() {
                insert_part_row(c, session, message, first + index as i64, created, e.created as i64, &encode_part(part))?;
            }
        }
        a.content = Vec::new();
        c.execute(
            "UPDATE session_message SET data = ?, time_updated = ? WHERE id = ? AND session_id = ?",
            params![Message::Assistant(a).to_row_data(), now_ms(), message, session],
        )?;
    }
    Ok(Some(seq))
}

/// `appendAssistantPart`: with part rows off, the part is pushed onto the whole message; on, it is a new
/// `session_part` row after the message's last ordinal.
fn append_part(c: &Connection, e: &Committing<'_>, session: &str, message: &str, part: AssistantContent) -> anyhow::Result<()> {
    if !e.part_rows {
        with_assistant(c, e, session, message, |a| a.content.push(part))?;
        return Ok(());
    }
    let Some(seq) = prepare_parts(c, e, session, message, true)? else { return Ok(()) };
    let mut data = encode_part(&part);
    externalize_part(c, e, &mut data)?;
    let next: i64 = c.query_row(
        "SELECT coalesce(max(ordinal) + 1, 0) FROM session_part WHERE message_id = ?",
        [message],
        |r| r.get(0),
    )?;
    insert_part_row(c, session, message, next, e.created as i64, e.created as i64, &data)?;
    crate::stored::record_refs(c, session, message, seq, &serde_json::json!({ "content": [data] }))?;
    Ok(())
}

/// `updateAssistantPart`: applies `recipe` to the last part `m` matches. With part rows off the whole message is
/// rewritten; on, only that part's row, and only when `recipe` changed it (it returns whether it did).
fn update_part(
    c: &Connection,
    e: &Committing<'_>,
    session: &str,
    message: &str,
    m: PartMatch<'_>,
    recipe: impl FnOnce(&mut AssistantContent) -> bool,
) -> anyhow::Result<()> {
    if !e.part_rows {
        with_assistant(c, e, session, message, |a| {
            if let Some(part) = a.content.iter_mut().rev().find(|p| m.matches(p)) {
                recipe(part);
            }
        })?;
        return Ok(());
    }
    let Some(seq) = prepare_parts(c, e, session, message, true)? else { return Ok(()) };
    let mut sql = "SELECT ordinal, type, data FROM session_part WHERE message_id = ? AND session_id = ? AND type = ?".to_string();
    let mut args: Vec<rusqlite::types::Value> = vec![message.to_string().into(), session.to_string().into(), m.kind().to_string().into()];
    match &m {
        PartMatch::Tool(call) => {
            sql.push_str(" AND call_id = ?");
            args.push(call.to_string().into());
        }
        PartMatch::IncompleteReasoning => sql.push_str(" AND json_extract(data, '$.time.completed') IS NULL"),
        PartMatch::Text => {}
    }
    sql.push_str(" ORDER BY ordinal DESC LIMIT 1");
    let row: Option<(i64, String, String)> = c
        .query_row(&sql, rusqlite::params_from_iter(args), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let Some((ordinal, kind, data)) = row else { return Ok(()) };
    let mut part = decode_part_row(&kind, &data)?;
    if !recipe(&mut part) {
        return Ok(());
    }
    let mut data = encode_part(&part);
    let moved = externalize_part(c, e, &mut data)?;
    c.execute(
        "UPDATE session_part SET data = ?, time_updated = ? WHERE message_id = ? AND ordinal = ?",
        params![data.to_string(), e.created as i64, message, ordinal],
    )?;
    if moved {
        crate::stored::record_refs(c, session, message, seq, &serde_json::json!({ "content": [data] }))?;
    }
    Ok(())
}

/// `replaceAssistantParts`: the message's whole content. With part rows on, its part rows are deleted and the parts
/// written from ordinal 0 (inline media moved to `blob` where the flag is on), and refs the new parts no longer name
/// are released.
fn replace_parts(c: &Connection, e: &Committing<'_>, session: &str, message: &str, parts: &[AssistantContent]) -> anyhow::Result<()> {
    if !e.part_rows {
        with_assistant(c, e, session, message, |a| a.content = parts.to_vec())?;
        return Ok(());
    }
    let Some(seq) = prepare_parts(c, e, session, message, false)? else { return Ok(()) };
    c.execute("DELETE FROM session_part WHERE message_id = ?", [message])?;
    let mut content = Vec::with_capacity(parts.len());
    for part in parts {
        let mut data = encode_part(part);
        externalize_part(c, e, &mut data)?;
        content.push(data);
    }
    for (ordinal, data) in content.iter().enumerate() {
        insert_part_row(c, session, message, ordinal as i64, e.created as i64, e.created as i64, data)?;
    }
    crate::stored::sync_refs(c, session, message, seq, &serde_json::json!({ "content": content }))?;
    Ok(())
}

/// `decodePartRowSync`: a part row's data with its `type` column, upgraded from older shapes.
fn decode_part_row(kind: &str, data: &str) -> anyhow::Result<AssistantContent> {
    let mut value: Map<String, Value> = serde_json::from_str(data)?;
    value.insert("type".into(), Value::String(kind.to_string()));
    Ok(serde_json::from_value(oc_schema::legacy::part(Value::Object(value)))?)
}

/// A result event's native data merges into the tool's call data (result keys win).
fn merge_native(target: &mut Option<Metadata>, result: &Option<Metadata>) {
    if let Some(result) = result {
        target.get_or_insert_with(Map::new).extend(result.clone());
    }
}

fn terminal_snapshot(a: &mut Assistant, snapshot: &Option<String>, files: &Option<Vec<String>>) {
    if snapshot.is_some() || files.is_some() {
        let mut s = a.snapshot.clone().unwrap_or_default();
        s.end = snapshot.clone();
        s.files = files.clone();
        a.snapshot = Some(s);
    }
}

fn clear_current_retry(c: &Connection, e: &Committing<'_>, session: &str) -> rusqlite::Result<()> {
    if let Some(mut a) = current_assistant(c, e, session)? {
        if a.retry.is_some() {
            a.retry = None;
            update(c, e, session, &Message::Assistant(a))?;
        }
    }
    Ok(())
}

/// `SessionMessageUpdater.update` with the SQL adapter from `projector.ts`.
fn run(c: &Connection, e: &Committing<'_>, seq: i64) -> anyhow::Result<()> {
    use SessionEvent as E;
    let created = e.created;
    let meta = e.metadata.cloned();
    let from_event = message_from_event(e.id);
    let session = e.event.session_id().to_string();
    let s = session.as_str();
    let append = |m: Message| insert_message(c, e, s, seq, &m);
    let idle = |outcome: Outcome, error: Option<SessionError>| -> rusqlite::Result<()> {
        clear_current_retry(c, e, s)?;
        append(Message::Idle { id: from_event.clone(), metadata: meta.clone(), time: Created { created }, outcome, error })
    };
    match e.event {
        E::MessageContentUpdated { message_id, content, .. } => replace_parts(c, e, s, message_id, content)?,
        E::AgentSelected { agent, previous, .. } => {
            let previous = match previous {
                Some(p) => Some(p.clone()),
                None => c
                    .query_row("SELECT agent FROM session_v2 WHERE id = ?", [s], |r| r.get::<_, Option<String>>(0))
                    .optional()?
                    .flatten()
                    .filter(|a| !a.is_empty()),
            };
            append(Message::AgentSwitched {
                id: from_event.clone(),
                metadata: meta.clone(),
                time: Created { created },
                agent: agent.clone(),
                previous,
            })?;
        }
        E::ModelSelected { model, previous, .. } => {
            let previous = match previous {
                Some(p) => Some(p.clone()),
                None => c
                    .query_row("SELECT model FROM session_v2 WHERE id = ?", [s], |r| r.get::<_, Option<String>>(0))
                    .optional()?
                    .flatten()
                    .and_then(|m| serde_json::from_str::<ModelRef>(&m).ok()),
            };
            append(Message::ModelSwitched {
                id: from_event.clone(),
                metadata: meta.clone(),
                time: Created { created },
                model: model.clone(),
                previous,
            })?;
        }
        E::Moved { location, project_id, subpath, .. } => {
            let previous = c
                .query_row("SELECT directory, workspace_id, project_id, path FROM session_v2 WHERE id = ?", [s], |r| {
                    Ok(PreviousLocation {
                        location: LocationRef {
                            directory: r.get(0)?,
                            workspace_id: r.get::<_, Option<String>>(1)?.filter(|w| !w.is_empty()),
                        },
                        project_id: Some(r.get(2)?),
                        subpath: r.get(3)?,
                    })
                })
                .optional()?;
            append(Message::LocationSwitched {
                id: from_event.clone(),
                metadata: meta.clone(),
                time: Created { created },
                location: location.clone(),
                project_id: Some(project_id.clone()),
                subpath: subpath.clone(),
                previous,
            })?;
        }
        E::ExecutionSucceeded { .. } => idle(Outcome::Succeeded, None)?,
        E::ExecutionFailed { error, .. } => idle(Outcome::Failed, Some(error.clone()))?,
        E::ExecutionInterrupted { reason, .. } => {
            if *reason == InterruptReason::Shutdown {
                clear_current_retry(c, e, s)?
            } else {
                idle(Outcome::Interrupted, None)?
            }
        }
        E::InstructionsUpdated { delta, text, .. } => {
            if let Some(text) = text {
                let keys: Vec<String> = delta.keys().cloned().collect();
                let mut m = meta.clone().unwrap_or_default();
                m.insert("notice".into(), Value::String("instructions".into()));
                m.insert("instructionSources".into(), serde_json::json!(keys));
                append(Message::System {
                    id: from_event.clone(),
                    metadata: Some(m),
                    time: Created { created },
                    text: text.clone(),
                    description: Some(format!("Instructions updated: {}", keys.join(", "))),
                })?;
            }
        }
        E::Synthetic { text, description, metadata, .. } => append(Message::Synthetic {
            id: from_event.clone(),
            metadata: metadata.clone(),
            time: Created { created },
            text: text.clone(),
            description: description.clone(),
        })?,
        E::SkillActivated { id, name, text, .. } => append(Message::Skill {
            id: from_event.clone(),
            metadata: meta.clone(),
            time: Created { created },
            skill: id.clone(),
            name: name.clone(),
            text: text.clone(),
        })?,
        E::ShellStarted { shell, .. } => {
            let metadata = if shell.metadata.get("background") == Some(&Value::Bool(true)) {
                let mut m = meta.clone().unwrap_or_default();
                m.insert("background".into(), Value::Bool(true));
                Some(m)
            } else {
                meta.clone()
            };
            append(Message::Shell {
                id: from_event.clone(),
                metadata,
                time: CompletedTime { created, completed: None },
                shell_id: shell.id.clone(),
                command: shell.command.clone(),
                status: shell.status,
                exit: None,
                output: None,
            })?;
        }
        E::ShellEnded { shell, output, .. } => {
            let current = query_message(
                c,
                false,
                "SELECT id, type, data FROM session_message WHERE session_id = ? AND type = 'shell' AND json_extract(data, '$.shellID') = ? ORDER BY seq DESC LIMIT 1",
                params![s, shell.id],
            )?;
            if let Some(Message::Shell { id, metadata, time, shell_id, command, .. }) = current {
                update(
                    c,
                    e,
                    s,
                    &Message::Shell {
                        id,
                        metadata,
                        time: CompletedTime { created: time.created, completed: Some(created) },
                        shell_id,
                        command,
                        status: shell.status,
                        exit: shell.exit,
                        output: Some(output.clone()),
                    },
                )?;
            }
        }
        E::StepStarted { assistant_message_id, agent, model, snapshot, started, .. } => {
            if let Some(mut a) = get_assistant(c, e, s, assistant_message_id)? {
                a.agent = agent.clone();
                a.model = model.clone();
                a.retry = None;
                a.error = None;
                a.finish = None;
                a.raw_finish = None;
                a.native = None;
                a.time = AssistantTime { created: *started, first_token: None, streamed: None, completed: None };
                if let Some(snap) = snapshot {
                    let mut sn = a.snapshot.clone().unwrap_or_default();
                    sn.start = Some(snap.clone());
                    a.snapshot = Some(sn);
                }
                update(c, e, s, &Message::Assistant(a))?;
                return Ok(());
            }
            if let Some(mut cur) = current_assistant(c, e, s)? {
                cur.retry = None;
                cur.time.completed = Some(created);
                update(c, e, s, &Message::Assistant(cur))?;
            }
            append(Message::Assistant(Box::new(Assistant {
                id: assistant_message_id.clone(),
                metadata: meta.clone(),
                time: AssistantTime { created: *started, first_token: None, streamed: None, completed: None },
                agent: agent.clone(),
                model: model.clone(),
                content: vec![],
                snapshot: snapshot.as_ref().map(|s| AssistantSnapshot { start: Some(s.clone()), end: None, files: None }),
                finish: None,
                raw_finish: None,
                native: None,
                cost: None,
                tokens: None,
                error: None,
                retry: None,
            })))?;
        }
        E::StepFirstToken { assistant_message_id, .. } => {
            with_assistant(c, e, s, assistant_message_id, |a| a.time.first_token = Some(created))?
        }
        E::StepStreamed { assistant_message_id, .. } => {
            with_assistant(c, e, s, assistant_message_id, |a| a.time.streamed = Some(created))?
        }
        E::StepEnded { assistant_message_id, finish, raw_finish, native, cost, tokens, snapshot, files, .. } => {
            with_assistant(c, e, s, assistant_message_id, |a| {
                a.time.completed = Some(created);
                a.finish = Some(*finish);
                a.raw_finish = raw_finish.clone();
                a.native = native.clone();
                a.cost = Some(*cost);
                a.tokens = Some(*tokens);
                terminal_snapshot(a, snapshot, files);
            })?
        }
        E::StepFailed {
            assistant_message_id, error, finish, raw_finish, native, cost, tokens, snapshot, files, ..
        } => with_assistant(c, e, s, assistant_message_id, |a| {
            a.time.completed = Some(created);
            a.finish = Some(finish.unwrap_or(FinishReason::Error));
            a.raw_finish = raw_finish.clone();
            a.native = native.clone();
            a.error = Some(error.clone());
            a.retry = None;
            if let (Some(cost), Some(tokens)) = (cost, tokens) {
                a.cost = Some(*cost);
                a.tokens = Some(*tokens);
            }
            terminal_snapshot(a, snapshot, files);
        })?,
        E::TextStarted { assistant_message_id, .. } => {
            append_part(c, e, s, assistant_message_id, AssistantContent::Text { text: String::new(), native: None })?
        }
        E::TextEnded { assistant_message_id, text, native, .. } => {
            update_part(c, e, s, assistant_message_id, PartMatch::Text, |part| {
                if let AssistantContent::Text { text: t, native: n } = part {
                    *t = text.clone();
                    *n = native.clone();
                }
                true
            })?
        }
        E::ToolInputStarted { assistant_message_id, id, name, .. } => append_part(
            c,
            e,
            s,
            assistant_message_id,
            AssistantContent::Tool {
                id: id.clone(),
                name: name.clone(),
                executed: None,
                native: None,
                state: ToolState::Streaming { input: String::new() },
                time: ToolTime { created, ran: None, completed: None },
            },
        )?,
        E::ToolInputEnded { assistant_message_id, id, text, .. } => {
            update_part(c, e, s, assistant_message_id, PartMatch::Tool(id), |part| match part {
                AssistantContent::Tool { state: ToolState::Streaming { input }, .. } if input != text => {
                    *input = text.clone();
                    true
                }
                _ => false,
            })?
        }
        E::ToolCalled { assistant_message_id, id, input, executed, native, .. } => {
            update_part(c, e, s, assistant_message_id, PartMatch::Tool(id), |part| {
                if let AssistantContent::Tool { executed: ex, native: n, time, state: st, .. } = part {
                    *ex = Some(*executed);
                    *n = native.clone();
                    time.ran = Some(created);
                    *st = ToolState::Running { input: input.clone(), metadata: Map::new() };
                }
                true
            })?
        }
        E::ToolSuccess { assistant_message_id, id, content, metadata, executed, native, .. } => {
            update_part(c, e, s, assistant_message_id, PartMatch::Tool(id), |part| {
                let AssistantContent::Tool { executed: ex, native: n, time, state: st, .. } = part else { return false };
                let ToolState::Running { input, .. } = st else { return false };
                let input = input.clone();
                *ex = Some(*executed || *ex == Some(true));
                merge_native(n, native);
                time.completed = Some(created);
                *st = ToolState::Completed { input, content: content.clone(), metadata: metadata.clone() };
                true
            })?
        }
        E::ToolFailed { assistant_message_id, id, error, content, metadata, executed, native, .. } => {
            update_part(c, e, s, assistant_message_id, PartMatch::Tool(id), |part| {
                let AssistantContent::Tool { executed: ex, native: n, time, state: st, .. } = part else { return false };
                let input = match st {
                    ToolState::Streaming { .. } => Map::new(),
                    ToolState::Running { input, .. } => input.clone(),
                    _ => return false,
                };
                *ex = Some(*executed || *ex == Some(true));
                merge_native(n, native);
                time.completed = Some(created);
                *st = ToolState::Error { input, error: error.clone(), content: content.clone(), metadata: metadata.clone() };
                true
            })?
        }
        E::ReasoningStarted { assistant_message_id, native, .. } => append_part(
            c,
            e,
            s,
            assistant_message_id,
            AssistantContent::Reasoning {
                text: String::new(),
                native: native.clone(),
                time: Some(CompletedTime { created, completed: None }),
            },
        )?,
        E::ReasoningEnded { assistant_message_id, text, native, .. } => {
            update_part(c, e, s, assistant_message_id, PartMatch::IncompleteReasoning, |part| {
                if let AssistantContent::Reasoning { text: t, native: n, time } = part {
                    *t = text.clone();
                    *time = Some(CompletedTime { created: time.map(|t| t.created).unwrap_or(created), completed: Some(created) });
                    if native.is_some() {
                        *n = native.clone();
                    }
                }
                true
            })?
        }
        E::RetryScheduled { assistant_message_id, attempt, at, error, .. } => {
            with_assistant(c, e, s, assistant_message_id, |a| {
                a.retry = Some(AssistantRetry { attempt: *attempt, at: *at, error: error.clone() })
            })?
        }
        E::CompactionStarted { reason, input_id, .. } => append(Message::Compaction(Box::new(Compaction {
            id: input_id.clone().unwrap_or_else(|| from_event.clone()),
            metadata: meta.clone(),
            time: Created { created },
            status: CompactionStatus::Running { reason: *reason, summary: String::new() },
        })))?,
        E::CompactionEnded { reason, model, native, text, cost, tokens, .. } => {
            let status = CompactionStatus::Completed {
                reason: *reason,
                model: model.clone(),
                native: native.clone(),
                summary: text.clone(),
                cost: *cost,
                tokens: *tokens,
            };
            if let Some(mut cur) = running_compaction(c, s)? {
                if let Some(m) = &meta {
                    let mut merged = cur.metadata.clone().unwrap_or_default();
                    merged.extend(m.clone());
                    cur.metadata = Some(merged);
                }
                cur.status = status;
                update(c, e, s, &Message::Compaction(cur))?;
            } else {
                append(Message::Compaction(Box::new(Compaction {
                    id: from_event.clone(),
                    metadata: meta.clone(),
                    time: Created { created },
                    status,
                })))?;
            }
        }
        E::CompactionFailed { reason, error, input_id, cost, tokens, .. } => {
            let current = running_compaction(c, s)?;
            let failed = Compaction {
                id: current
                    .as_ref()
                    .map(|c| c.id.clone())
                    .or_else(|| input_id.clone())
                    .unwrap_or_else(|| from_event.clone()),
                metadata: current.as_ref().and_then(|c| c.metadata.clone()).or_else(|| meta.clone()),
                time: current.as_ref().map(|c| c.time).unwrap_or(Created { created }),
                status: CompactionStatus::Failed { reason: *reason, error: error.clone(), cost: *cost, tokens: *tokens },
            };
            if current.is_some() {
                update(c, e, s, &Message::Compaction(Box::new(failed)))?;
            } else {
                append(Message::Compaction(Box::new(failed)))?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `insertMessage` (2.2.7): an assistant message inserted with content keeps an empty envelope and writes its
    /// parts to `session_part` (ordinals from 0, created at the message's time, updated at the event's). No
    /// event inserts one today (steps start empty), as upstream.
    #[test]
    fn inserts_an_assistant_message_with_content_as_part_rows() {
        let db = crate::Database::memory().unwrap();
        db.with(|c| {
            c.execute_batch(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) VALUES ('global', '/', 0, 0, '[]');
                 INSERT INTO session_v2 (id, project_id, slug, directory, version, time_created, time_updated) VALUES ('ses_x', 'global', 's', '/tmp', 'v', 1, 1);",
            )
        })
        .unwrap();
        let content = serde_json::json!([
            {"type": "text", "text": "hello"},
            {"type": "tool", "id": "call_1", "name": "read", "executed": true,
             "state": {"status": "completed", "input": {}, "content": [{"type": "text", "text": "ok"}]},
             "time": {"created": 3, "ran": 3, "completed": 4}}
        ]);
        let message: Message = serde_json::from_value(serde_json::json!({
            "id": "msg_a", "type": "assistant", "agent": "build",
            "model": {"id": "m", "providerID": "p", "variant": "default"},
            "content": content, "time": {"created": 3, "completed": 5}
        }))
        .unwrap();
        db.with(|c| insert_message_with(c, "ses_x", 7, &message, &[], false, 9)).unwrap();
        let data: String = db.with(|c| c.query_row("SELECT data FROM session_message WHERE id = 'msg_a'", [], |r| r.get(0))).unwrap();
        let data: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(data["content"], serde_json::json!([]));
        #[allow(clippy::type_complexity)]
        let parts: Vec<(i64, String, Option<String>, i64, i64, String)> = db
            .with(|c| {
                let mut s = c.prepare(
                    "SELECT ordinal, type, call_id, time_created, time_updated, data FROM session_part WHERE message_id = 'msg_a' ORDER BY ordinal",
                )?;
                let rows = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
                rows.collect()
            })
            .unwrap();
        assert_eq!(parts.iter().map(|p| (p.0, p.1.as_str(), p.2.as_deref(), p.3, p.4)).collect::<Vec<_>>(), [
            (0, "text", None, 3, 9),
            (1, "tool", Some("call_1"), 3, 9)
        ]);
        let mut rows = [("msg_a".to_string(), "assistant".to_string(), data.to_string())];
        db.with(|c| crate::stored::with_parts(c, &mut rows)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&rows[0].2).unwrap()["content"], content);
    }
}
