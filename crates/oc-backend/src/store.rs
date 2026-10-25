//! Direct SQL reads/writes that upstream keeps in `SessionStore`, `SessionHistory`,
//! `InstructionEntry`, `InstructionState`, `PermissionSaved` and `Bus.remove`.

use oc_schema::types::SessionInfo;
use oc_schema::Message;
use oc_server::{CursorDirection, ParentFilter, SessionListQuery};
use oc_server::generated::SessionListOrder;
use oc_storage::Database;
use oc_storage::session::{SESSION_COLUMNS, session_from_row};
use rusqlite::{OptionalExtension, params_from_iter, types::Value as Sql};
use serde_json::{Map, Value};

/// `SessionStore.list`.
pub fn list_sessions(db: &Database, q: &SessionListQuery, limit: Option<i64>) -> anyhow::Result<Vec<SessionInfo>> {
    let direction = q.anchor.as_ref().map(|a| a.direction).unwrap_or(CursorDirection::Next);
    let requested = q.order.unwrap_or(SessionListOrder::Desc);
    let asc = match (direction, requested) {
        (CursorDirection::Previous, SessionListOrder::Asc) => false,
        (CursorDirection::Previous, SessionListOrder::Desc) => true,
        (_, SessionListOrder::Asc) => true,
        (_, SessionListOrder::Desc) => false,
    };
    let mut cond: Vec<String> = Vec::new();
    let mut args: Vec<Sql> = Vec::new();
    // `Schema.Union([Directory, Project, All])`: a directory query wins over a project query.
    if let Some(d) = &q.directory {
        cond.push("directory = ?".into());
        args.push(Sql::Text(d.clone()));
    } else if let Some(p) = &q.project {
        cond.push("project_id = ?".into());
        args.push(Sql::Text(p.clone()));
        if let Some(s) = &q.subpath {
            cond.push("path = ?".into());
            args.push(Sql::Text(s.clone()));
        }
    }
    if let Some(s) = q.search.as_ref().filter(|s| !s.is_empty()) {
        cond.push("title LIKE ?".into());
        args.push(Sql::Text(format!("%{s}%")));
    }
    match &q.parent_id {
        Some(ParentFilter::Root) => cond.push("parent_id IS NULL".into()),
        Some(ParentFilter::Session(id)) => {
            cond.push("parent_id = ?".into());
            args.push(Sql::Text(id.clone()));
        }
        None => {}
    }
    if let Some(a) = &q.anchor {
        let op = if asc { ">" } else { "<" };
        cond.push(format!("(time_updated {op} ? OR (time_updated = ? AND id {op} ?))"));
        args.push(Sql::Integer(a.time));
        args.push(Sql::Integer(a.time));
        args.push(Sql::Text(a.id.clone()));
    }
    let dir = if asc { "ASC" } else { "DESC" };
    let mut sql = format!("SELECT {SESSION_COLUMNS} FROM session_v2");
    if !cond.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&cond.join(" AND "));
    }
    sql.push_str(&format!(" ORDER BY time_updated {dir}, id {dir}"));
    if let Some(l) = limit {
        sql.push_str(&format!(" LIMIT {l}"));
    }
    let mut rows: Vec<SessionInfo> = db.with(|c| {
        let mut s = c.prepare(&sql)?;
        
        s.query_map(params_from_iter(args.iter()), session_from_row)?.collect()
    })?;
    if direction == CursorDirection::Previous {
        rows.reverse();
    }
    Ok(rows)
}

/// Child session ids (`list({parentID})`, default order).
pub fn children(db: &Database, parent: &str) -> anyhow::Result<Vec<String>> {
    db.with(|c| {
        let mut s = c.prepare("SELECT id FROM session_v2 WHERE parent_id = ? ORDER BY time_updated DESC, id DESC")?;
        
        s.query_map([parent], |r| r.get(0))?.collect()
    })
}

/// `Bus.remove(aggregateID)`.
pub fn remove_aggregate(db: &Database, aggregate: &str) -> anyhow::Result<()> {
    db.transaction(|c| {
        c.execute("DELETE FROM event_sequence WHERE aggregate_id = ?", [aggregate])?;
        c.execute("DELETE FROM event WHERE aggregate_id = ?", [aggregate])?;
        Ok(())
    })
}

/// A raw `session_message` row.
pub struct MessageRow {
    pub seq: i64,
    pub id: String,
    pub kind: String,
    pub data: String,
}

impl MessageRow {
    /// `{...data, id, type}` (API `Session.Message.Info` before schema encoding). Rows written before v2.1
    /// are upgraded to the current shape (`history.ts` `rowInput`).
    pub fn json(&self) -> Option<Value> {
        oc_schema::message::row_input(&self.id, &self.kind, &self.data).ok().map(Value::Object)
    }

    pub fn data(&self) -> Value {
        match serde_json::from_str::<Map<String, Value>>(&self.data) {
            Ok(data) => Value::Object(oc_schema::legacy::message(&self.kind, data)),
            Err(_) => Value::Null,
        }
    }

    fn from_message(seq: i64, message: &Message) -> MessageRow {
        MessageRow { seq, id: message.id().into(), kind: message.kind().into(), data: message.to_row_data() }
    }
}

fn message_row(r: &rusqlite::Row) -> rusqlite::Result<MessageRow> {
    Ok(MessageRow { seq: r.get(0)?, id: r.get(1)?, kind: r.get(2)?, data: r.get(3)? })
}

pub fn message_seq(db: &Database, session: &str, id: &str) -> anyhow::Result<Option<i64>> {
    db.with(|c| {
        c.query_row("SELECT seq FROM session_message WHERE session_id = ? AND id = ?", [session, id], |r| r.get(0))
            .optional()
    })
}

/// `SessionStore.messages`.
pub fn messages(
    db: &Database,
    session: &str,
    limit: Option<i64>,
    asc_requested: bool,
    kind: Option<&str>,
    cursor: Option<(&str, CursorDirection)>,
) -> anyhow::Result<Vec<MessageRow>> {
    let direction = cursor.map(|c| c.1).unwrap_or(CursorDirection::Next);
    let asc = if direction == CursorDirection::Previous { !asc_requested } else { asc_requested };
    let anchor = match cursor {
        Some((id, _)) => match message_seq(db, session, id)? {
            Some(seq) => Some(seq),
            None => return Ok(vec![]),
        },
        None => None,
    };
    let mut sql = "SELECT seq, id, type, data FROM session_message WHERE session_id = ?".to_string();
    let mut args: Vec<Sql> = vec![Sql::Text(session.into())];
    if let Some(seq) = anchor {
        sql.push_str(if asc { " AND seq > ?" } else { " AND seq < ?" });
        args.push(Sql::Integer(seq));
    }
    if let Some(k) = kind {
        sql.push_str(" AND type = ?");
        args.push(Sql::Text(k.into()));
    }
    sql.push_str(if asc { " ORDER BY seq ASC" } else { " ORDER BY seq DESC" });
    if let Some(l) = limit {
        sql.push_str(&format!(" LIMIT {l}"));
    }
    let mut rows: Vec<MessageRow> = db.with(|c| {
        let mut s = c.prepare(&sql)?;
        
        s.query_map(params_from_iter(args.iter()), message_row)?.collect()
    })?;
    if direction == CursorDirection::Previous {
        rows.reverse();
    }
    Ok(rows)
}

/// One message by id (any session).
pub fn message(db: &Database, id: &str) -> anyhow::Result<Option<(String, MessageRow)>> {
    db.with(|c| {
        c.query_row("SELECT seq, id, type, data, session_id FROM session_message WHERE id = ?", [id], |r| {
            Ok((r.get::<_, String>(4)?, message_row(r)?))
        })
        .optional()
    })
}

/// Rows of the given types in sequence order.
pub fn rows_of_types(db: &Database, session: &str, kinds: &[&str]) -> anyhow::Result<Vec<MessageRow>> {
    let placeholders = vec!["?"; kinds.len()].join(",");
    let sql = format!("SELECT seq, id, type, data FROM session_message WHERE session_id = ? AND type IN ({placeholders}) ORDER BY seq ASC");
    let mut args: Vec<Sql> = vec![Sql::Text(session.into())];
    args.extend(kinds.iter().map(|k| Sql::Text(k.to_string())));
    db.with(|c| {
        let mut s = c.prepare(&sql)?;
        
        s.query_map(params_from_iter(args.iter()), message_row)?.collect()
    })
}

/// `SessionHistory.load(db, sessionID, "latest")` (`session.context`): a summary's kept text is rebuilt in
/// `metadata.recent`.
pub fn context(db: &Database, session: &str) -> anyhow::Result<Vec<MessageRow>> {
    let entries = oc_core::history::entries(db, session, oc_core::history::Boundary::Latest)?;
    Ok(entries.iter().map(|(seq, m)| MessageRow::from_message(*seq, m)).collect())
}

// ---- instruction entries ---------------------------------------------------------------------

pub fn instruction_entries(db: &Database, session: &str, include_removed: bool) -> anyhow::Result<Vec<(String, Option<Value>, bool)>> {
    let sql = if include_removed {
        "SELECT key, value, removed FROM instruction_entry WHERE session_id = ? ORDER BY key ASC"
    } else {
        "SELECT key, value, removed FROM instruction_entry WHERE session_id = ? AND removed = 0 ORDER BY key ASC"
    };
    db.with(|c| {
        let mut s = c.prepare(sql)?;
        
        s
            .query_map([session], |r| {
                let value: Option<String> = r.get(1)?;
                Ok((r.get(0)?, value.and_then(|v| serde_json::from_str(&v).ok()), r.get::<_, bool>(2)?))
            })?
            .collect()
    })
}

pub fn instruction_put(db: &Database, session: &str, key: &str, value: &Value) -> anyhow::Result<()> {
    let now = crate::util::now_ms();
    let text: Option<String> = if value.is_null() { None } else { Some(value.to_string()) };
    db.with(|c| {
        c.execute(
            "INSERT INTO instruction_entry (session_id, key, value, removed, time_created, time_updated) VALUES (?1, ?2, ?3, 0, ?4, ?4)
             ON CONFLICT (session_id, key) DO UPDATE SET value = ?3, removed = 0, time_updated = ?4
             WHERE instruction_entry.removed = 1 OR (CASE WHEN ?3 IS NULL THEN instruction_entry.value IS NOT NULL ELSE (instruction_entry.value IS NULL OR instruction_entry.value <> ?3) END)",
            rusqlite::params![session, key, text, now],
        )
    })?;
    Ok(())
}

pub fn instruction_remove(db: &Database, session: &str, key: &str) -> anyhow::Result<()> {
    let now = crate::util::now_ms();
    db.with(|c| {
        c.execute(
            "UPDATE instruction_entry SET value = NULL, removed = 1, time_updated = ? WHERE session_id = ? AND key = ? AND removed = 0",
            rusqlite::params![now, session, key],
        )
    })?;
    Ok(())
}

/// `InstructionState.current`.
pub fn instruction_current(db: &Database, session: &str) -> anyhow::Result<Option<Map<String, Value>>> {
    let raw: Option<String> = db.with(|c| {
        c.query_row("SELECT current_values FROM instruction_state WHERE session_id = ?", [session], |r| r.get(0)).optional()
    })?;
    Ok(raw.and_then(|r| serde_json::from_str(&r).ok()))
}

// ---- saved permissions -------------------------------------------------------------------------

pub fn saved_permissions(db: &Database, project: Option<&str>) -> anyhow::Result<Vec<Value>> {
    let (sql, args): (&str, Vec<Sql>) = match project {
        Some(p) => ("SELECT id, project_id, action, resource, time_created, time_updated FROM permission WHERE project_id = ?", vec![Sql::Text(p.into())]),
        None => ("SELECT id, project_id, action, resource, time_created, time_updated FROM permission", vec![]),
    };
    db.with(|c| {
        let mut s = c.prepare(sql)?;
        
        s
            .query_map(params_from_iter(args.iter()), |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, String>(0)?,
                    "projectID": r.get::<_, String>(1)?,
                    "action": r.get::<_, String>(2)?,
                    "resource": r.get::<_, String>(3)?,
                    "time": {"created": r.get::<_, i64>(4)?, "updated": r.get::<_, i64>(5)?},
                }))
            })?
            .collect()
    })
}

pub fn remove_saved_permission(db: &Database, id: &str) -> anyhow::Result<()> {
    db.with(|c| c.execute("DELETE FROM permission WHERE id = ?", [id]))?;
    Ok(())
}

// ---- event log -------------------------------------------------------------------------------

/// Stored durable events of an aggregate in `(after, through]`.
pub fn read_events(db: &Database, aggregate: &str, after: i64, through: i64) -> anyhow::Result<Vec<(i64, Value)>> {
    db.with(|c| {
        let mut s = c.prepare(
            "SELECT id, seq, created, type, data FROM event WHERE aggregate_id = ? AND seq > ? AND seq <= ? ORDER BY seq ASC LIMIT 512",
        )?;
        
        s
            .query_map(rusqlite::params![aggregate, after, through], |r| {
                let id: String = r.get(0)?;
                let seq: i64 = r.get(1)?;
                let created: i64 = r.get(2)?;
                let stored: String = r.get(3)?;
                let data: String = r.get(4)?;
                let (kind, version) = match stored.rsplit_once('.') {
                    Some((k, v)) if v.chars().all(|c| c.is_ascii_digit()) && !v.is_empty() => (k.to_string(), v.parse::<i64>().unwrap_or(1)),
                    _ => (stored.clone(), 1),
                };
                Ok((
                    seq,
                    serde_json::json!({
                        "id": id,
                        "created": created,
                        "type": kind,
                        "durable": {"aggregateID": aggregate, "seq": seq, "version": version},
                        // Payloads written before v2.1 are upgraded (`bus.ts` `decodeStoredData`).
                        "data": oc_schema::legacy::event(&kind, serde_json::from_str::<Value>(&data).unwrap_or(Value::Null)),
                    }),
                ))
            })?
            .collect()
    })
}

pub fn latest_seq(db: &Database, aggregate: &str) -> anyhow::Result<i64> {
    db.session_seq(aggregate)
}
