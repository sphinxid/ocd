//! Session reads (`core/src/session/info.ts`, `store.ts`, `history.ts`).

use rusqlite::{params, OptionalExtension, Row};
use serde_json::Value;

use oc_schema::types::*;
use oc_schema::Message;

use crate::db::Database;

const ADJECTIVES: &[&str] = &[
    "brave", "calm", "clever", "cosmic", "crisp", "curious", "eager", "gentle", "glowing", "happy", "hidden", "jolly",
    "kind", "lucky", "mighty", "misty", "neon", "nimble", "playful", "proud", "quick", "quiet", "shiny", "silent",
    "stellar", "sunny", "swift", "tidy", "witty",
];
const NOUNS: &[&str] = &[
    "cabin", "cactus", "canyon", "circuit", "comet", "eagle", "engine", "falcon", "forest", "garden", "harbor",
    "island", "knight", "lagoon", "meadow", "moon", "mountain", "nebula", "orchid", "otter", "panda", "pixel",
    "planet", "river", "rocket", "sailor", "squid", "star", "tiger", "wizard", "wolf",
];

/// `Slug.create()`
pub fn slug() -> String {
    format!(
        "{}-{}",
        ADJECTIVES[rand::random_range(0..ADJECTIVES.len())],
        NOUNS[rand::random_range(0..NOUNS.len())]
    )
}

pub const SESSION_COLUMNS: &str = "id, project_id, title, parent_id, fork_session_id, fork_boundary, agent, model, cost, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write, directory, workspace_id, path, metadata, permission, revert, idle_outcome, time_created, time_updated, time_idle, time_viewed, time_archived";

fn json<T: serde::de::DeserializeOwned>(s: Option<String>) -> Option<T> {
    s.and_then(|s| serde_json::from_str(&s).ok())
}

/// Decode a persisted revert, accepting the v1 shape (`files[].path` → `file`).
fn decode_revert(raw: Option<String>) -> Option<Revert> {
    let mut v: Value = serde_json::from_str(&raw?).ok()?;
    if let Some(files) = v.get_mut("files").and_then(Value::as_array_mut) {
        for f in files {
            if let Some(o) = f.as_object_mut() {
                if !o.contains_key("file") {
                    if let Some(p) = o.remove("path") {
                        o.insert("file".into(), p);
                    }
                }
            }
        }
    }
    if let Some(o) = v.as_object_mut() {
        o.remove("diff");
    }
    serde_json::from_value(v).ok()
}

/// `fromRow` in `info.ts`. Expects [`SESSION_COLUMNS`] order.
pub fn session_from_row(r: &Row) -> rusqlite::Result<SessionInfo> {
    let parent: Option<String> = r.get(3)?;
    let fork_session: Option<String> = r.get(4)?;
    let fork_boundary: Option<ForkBoundary> = json(r.get(5)?);
    let model: Option<Value> = json(r.get(7)?);
    let workspace: Option<String> = r.get(15)?;
    let path: Option<String> = r.get(16)?;
    let archived: Option<i64> = r.get(25)?;
    let outcome: Option<String> = r.get(20)?;
    Ok(SessionInfo {
        id: r.get(0)?,
        parent_id: parent.filter(|p| !p.is_empty()),
        fork: match (fork_session, fork_boundary) {
            (Some(session_id), Some(boundary)) => Some(SessionFork { session_id, boundary }),
            _ => None,
        },
        project_id: r.get(1)?,
        agent: r.get::<_, Option<String>>(6)?.filter(|a| !a.is_empty()),
        model: model.map(|m| ModelRef {
            id: m["id"].as_str().unwrap_or_default().to_string(),
            provider_id: m["providerID"].as_str().unwrap_or_default().to_string(),
            variant: Some(m["variant"].as_str().unwrap_or("default").to_string()),
        }),
        cost: r.get(8)?,
        tokens: TokenUsage {
            input: r.get::<_, i64>(9)? as u64,
            output: r.get::<_, i64>(10)? as u64,
            reasoning: r.get::<_, i64>(11)? as u64,
            cache: CacheUsage { read: r.get::<_, i64>(12)? as u64, write: r.get::<_, i64>(13)? as u64 },
        },
        outcome: outcome.and_then(|o| serde_json::from_value(Value::String(o)).ok()),
        time: SessionTime {
            created: r.get::<_, i64>(21)? as u64,
            updated: r.get::<_, i64>(22)? as u64,
            idle: r.get::<_, Option<i64>>(23)?.map(|v| v as u64),
            viewed: r.get::<_, Option<i64>>(24)?.map(|v| v as u64),
            archived: archived.map(|v| v as u64),
        },
        title: r.get(2)?,
        location: LocationRef { directory: r.get(14)?, workspace_id: workspace.filter(|w| !w.is_empty()) },
        subpath: path.filter(|p| !p.is_empty()),
        metadata: json(r.get(17)?),
        permissions: json(r.get(18)?),
        revert: decode_revert(r.get(19)?),
    })
}

impl Database {
    pub fn session_get(&self, id: &str) -> anyhow::Result<Option<SessionInfo>> {
        self.with(|c| {
            c.query_row(&format!("SELECT {SESSION_COLUMNS} FROM session_v2 WHERE id = ?"), [id], session_from_row)
                .optional()
        })
    }

    /// All messages of a session in sequence order.
    pub fn session_messages(&self, session: &str) -> anyhow::Result<Vec<(i64, Message)>> {
        let rows: Vec<(i64, String, String, String)> = self.with(|c| {
            let mut s = c.prepare("SELECT seq, id, type, data FROM session_message WHERE session_id = ? ORDER BY seq ASC")?;
            let r = s.query_map([session], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect();
            r
        })?;
        Ok(rows
            .into_iter()
            .filter_map(|(seq, id, kind, data)| match Message::from_row(&id, &kind, &data) {
                Ok(m) => Some((seq, m)),
                Err(e) => {
                    tracing::warn!(id, kind, error = %e, "failed to decode session message");
                    None
                }
            })
            .collect())
    }

    pub fn session_seq(&self, session: &str) -> anyhow::Result<i64> {
        self.with(|c| crate::projector::latest_seq(c, session))
    }

    /// Inbox rows `(id, type, payload, delivery, enqueued_seq, time_created)` ordered by `enqueued_seq`.
    pub fn session_inbox(&self, session: &str) -> anyhow::Result<Vec<(String, String, Value, String, i64, i64)>> {
        self.with(|c| {
            let mut s = c.prepare(
                "SELECT id, type, payload, delivery, enqueued_seq, time_created FROM session_inbox WHERE session_id = ? ORDER BY enqueued_seq ASC",
            )?;
            let r = s
                .query_map(params![session], |r| {
                    let payload: String = r.get(2)?;
                    Ok((r.get(0)?, r.get(1)?, serde_json::from_str(&payload).unwrap_or(Value::Null), r.get(3)?, r.get(4)?, r.get(5)?))
                })?
                .collect();
            r
        })
    }
}
