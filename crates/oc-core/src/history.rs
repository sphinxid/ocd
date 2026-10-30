//! Session history reads (`core/session/history.ts`): the messages since the latest compaction that bounds a
//! read, with a summary's kept text rebuilt from `metadata.keepID`.

use oc_schema::message::CompactionStatus;
use oc_schema::types::ModelRef;
use oc_schema::Message;
use oc_storage::Database;
use rusqlite::types::Value as Sql;
use rusqlite::{params_from_iter, OptionalExtension};
use serde_json::Value;

/// Which completed compactions bound a history read. Local summaries always do. Native windows do for
/// model-neutral readers (`Latest`), never for the original transcript (`Local`), and only when the target
/// model can replay them.
#[derive(Debug, Clone, Copy)]
pub enum Boundary<'a> {
    Latest,
    Local,
    Model(&'a ModelRef),
}

/// `isNativeCompaction`: a completed provider compaction, which replays `native.messages` instead of a summary.
pub fn is_native_compaction(message: &Message) -> bool {
    matches!(
        message,
        Message::Compaction(c) if matches!(&c.status, CompactionStatus::Completed { native: Some(n), .. } if n.contains_key("messages"))
    )
}

/// The window of a native compaction (`native.messages`).
pub fn native_window(message: &Message) -> Option<&Value> {
    match message {
        Message::Compaction(c) => match &c.status {
            CompactionStatus::Completed { native: Some(n), .. } => n.get("messages"),
            _ => None,
        },
        _ => None,
    }
}

/// `NativeWindow`: a native compaction's window, canonical AI messages stored as JSON.
pub fn decode_window(messages: &Value) -> Option<Vec<oc_ai::types::Message>> {
    serde_json::from_value(messages.clone()).ok()
}

/// Like other native data, a window only replays on the model that made it. One that no longer decodes, for
/// example a malformed one from a compaction hook, is skipped the same way instead of failing the session.
fn replayable(message: &Message, boundary: Boundary) -> bool {
    if !is_native_compaction(message) {
        return true;
    }
    match boundary {
        Boundary::Latest => true,
        Boundary::Local => false,
        Boundary::Model(target) => {
            let Message::Compaction(c) = message else { return false };
            let CompactionStatus::Completed { model, .. } = &c.status else { return false };
            model.as_ref().is_some_and(|m| m.provider_id == target.provider_id && m.id == target.id)
                && native_window(message).and_then(decode_window).is_some()
        }
    }
}

/// `latestCompaction`: the sequence of the newest completed compaction (before `before`) that bounds the read.
fn latest_compaction(db: &Database, session: &str, boundary: Boundary, before: Option<i64>) -> anyhow::Result<Option<i64>> {
    let mut sql = "SELECT seq FROM session_message WHERE session_id = ? AND type = 'compaction' AND json_extract(data, '$.status') = 'completed'".to_string();
    let mut args: Vec<Sql> = vec![Sql::Text(session.into())];
    if let Some(before) = before {
        sql.push_str(" AND seq < ?");
        args.push(Sql::Integer(before));
    }
    // Rows written before v2.1 hold the window in `providerContext`.
    let local = "coalesce(json_extract(data, '$.native.messages'), json_extract(data, '$.providerContext')) IS NULL";
    match boundary {
        Boundary::Latest => {}
        Boundary::Local => sql.push_str(&format!(" AND {local}")),
        Boundary::Model(m) => {
            sql.push_str(&format!(
                " AND ({local} OR (json_extract(data, '$.model.providerID') = ? AND json_extract(data, '$.model.id') = ?))"
            ));
            args.push(Sql::Text(m.provider_id.clone()));
            args.push(Sql::Text(m.id.clone()));
        }
    }
    sql.push_str(" ORDER BY seq DESC LIMIT 1");
    Ok(db.with(|c| c.query_row(&sql, params_from_iter(args.iter()), |r| r.get(0)).optional())?)
}

/// Decoded rows `(seq, message)` of a session from `from` (inclusive) to `to` (exclusive), in sequence order.
/// Every row decodes through [`Message::from_row`], which upgrades rows written in older shapes.
fn rows(db: &Database, session: &str, from: Option<i64>, to: Option<i64>) -> anyhow::Result<Vec<(i64, Message)>> {
    let rows: Vec<(i64, String, String, String)> = db.with(|c| {
        let mut s = c.prepare(
            "SELECT seq, id, type, data FROM session_message WHERE session_id = ?1 AND seq >= ?2 AND seq < ?3 ORDER BY seq ASC",
        )?;
        let mut rows: Vec<(i64, String, String, String)> = s
            .query_map(rusqlite::params![session, from.unwrap_or(i64::MIN), to.unwrap_or(i64::MAX)], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        // `decodeMessageRows`: parts a newer release moved to `session_part`, then media from `blob`.
        oc_storage::stored::restore(c, &mut rows)?;
        Ok(rows)
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

/// `messageEntries`
pub fn entries(db: &Database, session: &str, boundary: Boundary) -> anyhow::Result<Vec<(i64, Message)>> {
    entries_before(db, session, boundary, None)
}

fn entries_before(db: &Database, session: &str, boundary: Boundary, before: Option<i64>) -> anyhow::Result<Vec<(i64, Message)>> {
    let compaction = latest_compaction(db, session, boundary, before)?;
    let decoded = rows(db, session, compaction, None)?;
    // A window that cannot replay leaves nothing to start from, so read from the compaction before it.
    if let (Some(seq), Some((_, start))) = (compaction, decoded.first())
        && !replayable(start, boundary)
    {
        return entries_before(db, session, boundary, Some(seq));
    }
    let entries = if compaction.is_some() { with_recent_text(db, session, decoded)? } else { decoded };
    // Re-expansion may cross a native checkpoint whose completion already advanced the instruction epoch: the
    // baseline supersedes the chronological updates before it. Forks seed their baseline at sequence 0 but
    // retain parent sequences, so the copied checkpoint still retires them.
    let native = entries.iter().rev().find(|(_, m)| is_native_compaction(m)).map(|(seq, _)| *seq);
    // Skipped native checkpoints are not textual summaries. Their original transcript remains available.
    Ok(entries
        .into_iter()
        .filter(|(seq, m)| !(matches!(m, Message::System { .. }) && native.is_some_and(|n| *seq < n)) && replayable(m, boundary))
        .collect())
}

/// `withRecentText`: a summary compaction references its kept messages by `metadata.keepID` instead of storing
/// them, so history rebuilds their text from the messages between that one and the compaction, in memory only
/// (see [`crate::recent_text::kept`]). Compactions written before the reference have none and keep the text
/// they stored.
fn with_recent_text(db: &Database, session: &str, mut entries: Vec<(i64, Message)>) -> anyhow::Result<Vec<(i64, Message)>> {
    let Some((first_seq, Message::Compaction(compaction))) = entries.first() else { return Ok(entries) };
    if !matches!(compaction.status, CompactionStatus::Completed { .. }) {
        return Ok(entries);
    }
    let Some(keep) = compaction.metadata.as_ref().and_then(|m| m.get("keepID")).and_then(Value::as_str) else {
        return Ok(entries);
    };
    // `Schema.is(SessionMessage.ID)`: anything else is not a message reference.
    if !keep.starts_with("msg_") {
        return Ok(entries);
    }
    let start: Option<i64> = db.with(|c| {
        c.query_row("SELECT seq FROM session_message WHERE session_id = ? AND id = ?", [session, keep], |r| r.get(0))
            .optional()
    })?;
    let Some(start) = start else { return Ok(entries) };
    let first_seq = *first_seq;
    let recent = rows(db, session, Some(start), Some(first_seq))?
        .iter()
        .map(|(_, m)| crate::recent_text::message_to_text(m))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if let Some((_, Message::Compaction(c))) = entries.first_mut() {
        c.metadata.get_or_insert_with(Default::default).insert("recent".into(), Value::String(recent));
    }
    Ok(entries)
}

/// `SessionHistory.load`
pub fn load(db: &Database, session: &str, boundary: Boundary) -> anyhow::Result<Vec<Message>> {
    Ok(entries(db, session, boundary)?.into_iter().map(|(_, m)| m).collect())
}
