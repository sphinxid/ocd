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
            let message: Message = serde_json::from_value(Value::Object(msg))?;
            insert_message(c, session_id, seq, &message)?;
        }
        E::InboxEnqueued { session_id, inbox_id, item } => {
            let exists = c
                .query_row("SELECT id FROM session_message WHERE id = ?", [inbox_id], |_| Ok(()))
                .optional()?
                .is_some();
            if exists {
                return Err(ProjectError::LifecycleConflict(inbox_id.clone()).into());
            }
            let n = c.execute(
                "INSERT INTO session_inbox (id, session_id, type, payload, delivery, enqueued_seq, time_created) VALUES (?,?,?,?,?,?,?) ON CONFLICT DO NOTHING",
                params![
                    inbox_id,
                    session_id,
                    item.kind,
                    item.payload.to_string(),
                    delivery_str(item.delivery),
                    seq,
                    created as i64
                ],
            )?;
            if n == 0 {
                return Err(ProjectError::LifecycleConflict(inbox_id.clone()).into());
            }
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
        }
        E::ExecutionStarted { .. } => {}
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

fn insert_message(c: &Connection, session: &str, seq: i64, m: &Message) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![m.id(), session, m.kind(), seq, m.created() as i64, now_ms(), m.to_row_data()],
    )?;
    Ok(())
}

fn update_message(c: &Connection, session: &str, m: &Message) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE session_message SET type = ?, time_created = ?, data = ?, time_updated = ? WHERE id = ? AND session_id = ?",
        params![m.kind(), m.created() as i64, m.to_row_data(), now_ms(), m.id(), session],
    )?;
    Ok(())
}

fn decode(id: String, kind: String, data: String) -> Option<Message> {
    Message::from_row(&id, &kind, &data).ok()
}

fn query_message(c: &Connection, sql: &str, p: impl rusqlite::Params) -> rusqlite::Result<Option<Message>> {
    Ok(c
        .query_row(sql, p, |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
        .optional()?
        .and_then(|(i, k, d)| decode(i, k, d)))
}

fn get_assistant(c: &Connection, session: &str, id: &str) -> rusqlite::Result<Option<Box<Assistant>>> {
    Ok(match query_message(
        c,
        "SELECT id, type, data FROM session_message WHERE id = ? AND session_id = ? AND type = 'assistant'",
        params![id, session],
    )? {
        Some(Message::Assistant(a)) => Some(a),
        _ => None,
    })
}

fn current_assistant(c: &Connection, session: &str) -> rusqlite::Result<Option<Box<Assistant>>> {
    Ok(match query_message(
        c,
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
        "SELECT id, type, data FROM session_message WHERE session_id = ? AND type = 'compaction' AND json_extract(data, '$.status') = 'running' ORDER BY seq DESC LIMIT 1",
        [session],
    )? {
        Some(Message::Compaction(m)) => Some(m),
        _ => None,
    })
}

fn with_assistant(
    c: &Connection,
    session: &str,
    id: &str,
    f: impl FnOnce(&mut Assistant),
) -> rusqlite::Result<()> {
    if let Some(mut a) = get_assistant(c, session, id)? {
        f(&mut a);
        update_message(c, session, &Message::Assistant(a))?;
    }
    Ok(())
}

fn last_tool<'a>(a: &'a mut Assistant, call: &str) -> Option<&'a mut AssistantContent> {
    a.content.iter_mut().rev().find(|c| matches!(c, AssistantContent::Tool { id, .. } if id == call))
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

fn clear_current_retry(c: &Connection, session: &str) -> rusqlite::Result<()> {
    if let Some(mut a) = current_assistant(c, session)? {
        if a.retry.is_some() {
            a.retry = None;
            update_message(c, session, &Message::Assistant(a))?;
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
    let append = |m: Message| insert_message(c, s, seq, &m);
    let idle = |outcome: Outcome, error: Option<SessionError>| -> rusqlite::Result<()> {
        clear_current_retry(c, s)?;
        append(Message::Idle { id: from_event.clone(), metadata: meta.clone(), time: Created { created }, outcome, error })
    };
    match e.event {
        E::MessageContentUpdated { message_id, content, .. } => {
            with_assistant(c, s, message_id, |a| a.content = content.clone())?;
        }
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
                clear_current_retry(c, s)?
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
                "SELECT id, type, data FROM session_message WHERE session_id = ? AND type = 'shell' AND json_extract(data, '$.shellID') = ? ORDER BY seq DESC LIMIT 1",
                params![s, shell.id],
            )?;
            if let Some(Message::Shell { id, metadata, time, shell_id, command, .. }) = current {
                update_message(
                    c,
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
            if let Some(mut a) = get_assistant(c, s, assistant_message_id)? {
                a.agent = agent.clone();
                a.model = model.clone();
                a.retry = None;
                a.error = None;
                a.finish = None;
                a.raw_finish = None;
                a.native = None;
                a.time = AssistantTime { created: *started, streamed: None, completed: None };
                if let Some(snap) = snapshot {
                    let mut sn = a.snapshot.clone().unwrap_or_default();
                    sn.start = Some(snap.clone());
                    a.snapshot = Some(sn);
                }
                update_message(c, s, &Message::Assistant(a))?;
                return Ok(());
            }
            if let Some(mut cur) = current_assistant(c, s)? {
                cur.retry = None;
                cur.time.completed = Some(created);
                update_message(c, s, &Message::Assistant(cur))?;
            }
            append(Message::Assistant(Box::new(Assistant {
                id: assistant_message_id.clone(),
                metadata: meta.clone(),
                time: AssistantTime { created: *started, streamed: None, completed: None },
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
        E::StepStreamed { assistant_message_id, .. } => {
            with_assistant(c, s, assistant_message_id, |a| a.time.streamed = Some(created))?
        }
        E::StepEnded { assistant_message_id, finish, raw_finish, native, cost, tokens, snapshot, files, .. } => {
            with_assistant(c, s, assistant_message_id, |a| {
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
        } => with_assistant(c, s, assistant_message_id, |a| {
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
        E::TextStarted { assistant_message_id, .. } => with_assistant(c, s, assistant_message_id, |a| {
            a.content.push(AssistantContent::Text { text: String::new(), native: None })
        })?,
        E::TextEnded { assistant_message_id, text, native, .. } => with_assistant(c, s, assistant_message_id, |a| {
            if let Some(AssistantContent::Text { text: t, native: n }) =
                a.content.iter_mut().rev().find(|c| matches!(c, AssistantContent::Text { .. }))
            {
                *t = text.clone();
                *n = native.clone();
            }
        })?,
        E::ToolInputStarted { assistant_message_id, id, name, .. } => with_assistant(c, s, assistant_message_id, |a| {
            a.content.push(AssistantContent::Tool {
                id: id.clone(),
                name: name.clone(),
                executed: None,
                native: None,
                state: ToolState::Streaming { input: String::new() },
                time: ToolTime { created, ran: None, completed: None },
            })
        })?,
        E::ToolInputEnded { assistant_message_id, id, text, .. } => with_assistant(c, s, assistant_message_id, |a| {
            if let Some(AssistantContent::Tool { state: ToolState::Streaming { input }, .. }) = last_tool(a, id) {
                *input = text.clone();
            }
        })?,
        E::ToolCalled { assistant_message_id, id, input, executed, native, .. } => {
            with_assistant(c, s, assistant_message_id, |a| {
                if let Some(AssistantContent::Tool { executed: ex, native: n, time, state: st, .. }) = last_tool(a, id) {
                    *ex = Some(*executed);
                    *n = native.clone();
                    time.ran = Some(created);
                    *st = ToolState::Running { input: input.clone(), metadata: Map::new() };
                }
            })?
        }
        E::ToolSuccess { assistant_message_id, id, content, metadata, executed, native, .. } => {
            with_assistant(c, s, assistant_message_id, |a| {
                if let Some(AssistantContent::Tool { executed: ex, native: n, time, state: st, .. }) = last_tool(a, id) {
                    if let ToolState::Running { input, .. } = st {
                        let input = input.clone();
                        *ex = Some(*executed || *ex == Some(true));
                        merge_native(n, native);
                        time.completed = Some(created);
                        *st = ToolState::Completed { input, content: content.clone(), metadata: metadata.clone() };
                    }
                }
            })?
        }
        E::ToolFailed { assistant_message_id, id, error, content, metadata, executed, native, .. } => {
            with_assistant(c, s, assistant_message_id, |a| {
                if let Some(AssistantContent::Tool { executed: ex, native: n, time, state: st, .. }) = last_tool(a, id) {
                    let input = match st {
                        ToolState::Streaming { .. } => Some(Map::new()),
                        ToolState::Running { input, .. } => Some(input.clone()),
                        _ => None,
                    };
                    if let Some(input) = input {
                        *ex = Some(*executed || *ex == Some(true));
                        merge_native(n, native);
                        time.completed = Some(created);
                        *st = ToolState::Error {
                            input,
                            error: error.clone(),
                            content: content.clone(),
                            metadata: metadata.clone(),
                        };
                    }
                }
            })?
        }
        E::ReasoningStarted { assistant_message_id, native, .. } => with_assistant(c, s, assistant_message_id, |a| {
            a.content.push(AssistantContent::Reasoning {
                text: String::new(),
                native: native.clone(),
                time: Some(CompletedTime { created, completed: None }),
            })
        })?,
        E::ReasoningEnded { assistant_message_id, text, native, .. } => with_assistant(c, s, assistant_message_id, |a| {
            if let Some(AssistantContent::Reasoning { text: t, native: n, time }) = a.content.iter_mut().rev().find(|c| {
                matches!(c, AssistantContent::Reasoning { time, .. } if time.as_ref().and_then(|t| t.completed).is_none())
            }) {
                *t = text.clone();
                *time = Some(CompletedTime { created: time.map(|t| t.created).unwrap_or(created), completed: Some(created) });
                if native.is_some() {
                    *n = native.clone();
                }
            }
        })?,
        E::RetryScheduled { assistant_message_id, attempt, at, error, .. } => {
            with_assistant(c, s, assistant_message_id, |a| {
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
                update_message(c, s, &Message::Compaction(cur))?;
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
                update_message(c, s, &Message::Compaction(Box::new(failed)))?;
            } else {
                append(Message::Compaction(Box::new(failed)))?;
            }
        }
        _ => {}
    }
    Ok(())
}
