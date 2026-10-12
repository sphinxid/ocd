//! Durable session inbox (`core/src/session/inbox.ts`).

use std::collections::HashMap;
use std::sync::Arc;

use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use oc_schema::event::{Delivery, InboxItem, SessionEvent};

use crate::bus::Bus;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboxRow {
    pub id: String,
    pub session_id: String,
    pub kind: String,
    pub payload: Value,
    pub delivery: Delivery,
    pub enqueued_seq: i64,
    pub time_created: i64,
}

impl InboxRow {
    pub fn is_control(&self) -> bool {
        self.kind == "compaction" || self.kind == "move"
    }

    /// API `SessionInbox.Info`
    pub fn to_api(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "sessionID": self.session_id,
            "time": {"created": self.time_created},
            "type": self.kind,
            "payload": self.payload,
            "delivery": self.delivery,
        })
    }
}

/// `"input"` = either delivery mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Input,
    Steer,
}

#[derive(Clone, Default)]
pub struct Locks(Arc<parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>);

impl Locks {
    pub fn get(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.0.lock().entry(key.to_string()).or_default().clone()
    }
}

fn delivery(s: &str) -> Delivery {
    if s == "queue" { Delivery::Queue } else { Delivery::Steer }
}

fn rows(bus: &Bus, session: &str, filter: &str) -> anyhow::Result<Vec<InboxRow>> {
    bus.db().with(|c| {
        let mut s = c.prepare(&format!(
            "SELECT id, session_id, type, payload, delivery, enqueued_seq, time_created FROM session_inbox WHERE session_id = ? {filter} ORDER BY enqueued_seq ASC"
        ))?;
        let r = s
            .query_map(params![session], |r| {
                let payload: String = r.get(3)?;
                let d: String = r.get(4)?;
                Ok(InboxRow {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    kind: r.get(2)?,
                    payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
                    delivery: delivery(&d),
                    enqueued_seq: r.get(5)?,
                    time_created: r.get(6)?,
                })
            })?
            .collect();
        r
    })
}

pub fn list(bus: &Bus, session: &str) -> anyhow::Result<Vec<InboxRow>> {
    rows(bus, session, "")
}

/// Steers with a leading compaction pulled to the front (never across a move).
pub fn pending_steers(bus: &Bus, session: &str) -> anyhow::Result<Vec<InboxRow>> {
    let mut r = rows(bus, session, "AND delivery = 'steer'")?;
    if let Some(control) = r.iter().position(InboxRow::is_control) {
        if control > 0 && r[control].kind == "compaction" {
            let item = r.remove(control);
            r.insert(0, item);
        }
    }
    Ok(r)
}

fn oldest_queued(bus: &Bus, session: &str) -> anyhow::Result<Option<InboxRow>> {
    Ok(rows(bus, session, "AND delivery = 'queue'")?.into_iter().next())
}

pub fn next_promotable(bus: &Bus, session: &str, scope: Scope) -> anyhow::Result<Option<InboxRow>> {
    if let Some(s) = pending_steers(bus, session)?.into_iter().next() {
        return Ok(Some(s));
    }
    if scope != Scope::Input {
        return Ok(None);
    }
    oldest_queued(bus, session)
}

pub fn has(bus: &Bus, session: &str, scope: Option<Delivery>) -> anyhow::Result<bool> {
    let filter = match scope {
        None => "AND (delivery = 'steer' OR delivery = 'queue')",
        Some(Delivery::Steer) => "AND delivery = 'steer'",
        Some(Delivery::Queue) => "AND delivery = 'queue'",
    };
    Ok(!rows(bus, session, filter)?.is_empty())
}

fn deliver(bus: &Bus, session: &str, items: &[InboxRow]) -> anyhow::Result<usize> {
    for row in items {
        if row.kind == "compaction" {
            anyhow::bail!("Lifecycle conflict for {}", row.id);
        }
        bus.commit_one(SessionEvent::InboxDelivered { session_id: session.into(), inbox_id: row.id.clone() })?;
    }
    Ok(items.len())
}

fn until_control(items: Vec<InboxRow>) -> Vec<InboxRow> {
    match items.iter().position(InboxRow::is_control) {
        Some(i) => items.into_iter().take(i).collect(),
        None => items,
    }
}

/// Promote pending input; `None` when a control item must be handled first.
pub async fn promote(bus: &Bus, locks: &Locks, session: &str, scope: Scope) -> anyhow::Result<Option<usize>> {
    let lock = locks.get(session);
    let _g = lock.lock().await;
    let steers = pending_steers(bus, session)?;
    if !steers.is_empty() || scope == Scope::Steer {
        if steers.first().is_some_and(InboxRow::is_control) {
            return Ok(None);
        }
        return Ok(Some(deliver(bus, session, &until_control(steers))?));
    }
    let Some(queued) = oldest_queued(bus, session)? else { return Ok(Some(0)) };
    if queued.is_control() {
        return Ok(None);
    }
    let promoted = deliver(bus, session, std::slice::from_ref(&queued))?;
    let arrived = until_control(pending_steers(bus, session)?);
    Ok(Some(promoted + deliver(bus, session, &arrived)?))
}

/// Admit an item (`session.inbox.enqueued`).
pub async fn admit(
    bus: &Bus,
    locks: &Locks,
    session: &str,
    id: &str,
    kind: &str,
    payload: Value,
    delivery: Delivery,
) -> anyhow::Result<()> {
    let lock = locks.get(session);
    let _g = lock.lock().await;
    bus.commit_one(SessionEvent::InboxEnqueued {
        session_id: session.into(),
        inbox_id: id.into(),
        item: InboxItem { kind: kind.into(), payload, delivery },
    })?;
    Ok(())
}

pub async fn cancel(bus: &Bus, locks: &Locks, session: &str, id: &str) -> anyhow::Result<()> {
    let lock = locks.get(session);
    let _g = lock.lock().await;
    bus.commit_one(SessionEvent::InboxCancelled { session_id: session.into(), inbox_id: id.into() })?;
    Ok(())
}

pub async fn set_delivery(bus: &Bus, locks: &Locks, session: &str, id: &str, delivery: Delivery) -> anyhow::Result<()> {
    let lock = locks.get(session);
    let _g = lock.lock().await;
    bus.commit_one(SessionEvent::InboxDeliveryChanged { session_id: session.into(), inbox_id: id.into(), delivery })?;
    Ok(())
}
