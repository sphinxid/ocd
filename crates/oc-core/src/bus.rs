//! Event bus (`core/src/bus.ts`): durable session events are committed through the storage
//! projector, then broadcast with ephemeral events to live subscribers.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::broadcast;

use oc_schema::event::{Durable, Envelope, SessionEvent};
use oc_schema::types::{LocationRef, Metadata};
use oc_storage::projector::Committing;
use oc_storage::Database;

pub type Event = Arc<Envelope>;

#[derive(Clone)]
pub struct Bus {
    db: Database,
    tx: broadcast::Sender<Event>,
}

/// A durable session event to publish.
pub struct Durable1 {
    pub event: SessionEvent,
    pub metadata: Option<Metadata>,
    /// Explicit event id (e.g. derived from a client message id).
    pub id: Option<String>,
}

impl From<SessionEvent> for Durable1 {
    fn from(event: SessionEvent) -> Self {
        Durable1 { event, metadata: None, id: None }
    }
}

impl Bus {
    pub fn new(db: Database) -> Self {
        let (tx, _) = broadcast::channel(4096);
        Bus { db, tx }
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    fn session_location(&self, session_id: &str) -> Option<LocationRef> {
        self.db
            .with(|c| {
                use rusqlite::OptionalExtension;
                c.query_row("SELECT directory, workspace_id FROM session_v2 WHERE id = ?", [session_id], |r| {
                    Ok(LocationRef { directory: r.get(0)?, workspace_id: r.get(1)? })
                })
                .optional()
            })
            .ok()
            .flatten()
    }

    /// Ephemeral event (never stored).
    pub fn publish(&self, kind: &str, data: Value, location: Option<LocationRef>) -> Event {
        let env = Arc::new(Envelope {
            id: oc_schema::id::event(),
            created: oc_schema::id::now_ms(),
            metadata: None,
            kind: kind.to_string(),
            durable: None,
            location,
            data,
        });
        let _ = self.tx.send(env.clone());
        env
    }

    /// Broadcast an already-committed envelope.
    pub fn broadcast(&self, env: Envelope) -> Event {
        let env = Arc::new(env);
        let _ = self.tx.send(env.clone());
        env
    }

    /// Ephemeral session event routed to the session's location.
    pub fn publish_session_ephemeral(&self, session_id: &str, kind: &str, data: Value) -> Event {
        let loc = self.session_location(session_id);
        self.publish(kind, data, loc)
    }

    /// Commit durable session events (one aggregate) atomically, then broadcast.
    /// `hook(conn, index, seq)` runs inside the transaction after each projection.
    pub fn commit_with(
        &self,
        events: Vec<Durable1>,
        hook: impl FnMut(&rusqlite::Connection, usize, i64) -> anyhow::Result<()>,
    ) -> anyhow::Result<Vec<Event>> {
        if events.is_empty() {
            return Ok(vec![]);
        }
        let session = events[0].event.session_id().to_string();
        let before = self.session_location(&session);
        let ids: Vec<String> = events.iter().map(|e| e.id.clone().unwrap_or_else(oc_schema::id::event)).collect();
        let created = oc_schema::id::now_ms();
        let committing: Vec<Committing> = events
            .iter()
            .zip(&ids)
            .map(|(e, id)| Committing { id, created, metadata: e.metadata.as_ref(), event: &e.event })
            .collect();
        let seqs = self.db.commit_session_events(&committing, hook)?;
        let after = self.session_location(&session);
        let mut out = Vec::with_capacity(events.len());
        let mut usage = false;
        for ((e, id), seq) in events.into_iter().zip(ids).zip(seqs) {
            usage |= matches!(
                &e.event,
                SessionEvent::StepEnded { .. }
                    | SessionEvent::UsageRecorded { .. }
                    | SessionEvent::StepFailed { cost: Some(_), tokens: Some(_), .. }
            );
            // session.moved routes to both; session.created/forked use the new row.
            // Execution lifecycle events come from the global coordinator upstream (no Location service in
            // context), so their envelopes carry no location.
            let location = match &e.event {
                SessionEvent::ExecutionStarted { .. }
                | SessionEvent::ExecutionSucceeded { .. }
                | SessionEvent::ExecutionFailed { .. }
                | SessionEvent::ExecutionInterrupted { .. } => None,
                // `session.remove` runs outside any Location scope upstream (the route has no
                // location middleware, server/src/handlers/session.ts), so `bus.publish` attaches
                // no location (core/src/bus.ts `publish`).
                SessionEvent::Deleted { .. } => None,
                _ => after.clone().or_else(|| before.clone()),
            };
            let env = Arc::new(Envelope {
                id,
                created,
                metadata: e.metadata,
                kind: e.event.type_name(),
                durable: Some(Durable { aggregate_id: session.clone(), seq, version: e.event.version() }),
                location,
                data: e.event.data(),
            });
            let _ = self.tx.send(env.clone());
            if let (SessionEvent::Moved { .. }, Some(prev)) = (&e.event, &before) {
                let mut copy = (*env).clone();
                copy.location = Some(prev.clone());
                let _ = self.tx.send(Arc::new(copy));
            }
            out.push(env);
        }
        if usage {
            if let Ok(Some((cost, tokens))) = self.db.session_usage(&session) {
                // Published by the (global) projector upstream: no envelope location.
                self.publish("session.usage.updated", serde_json::json!({"sessionID": session, "cost": cost, "tokens": tokens}), None);
            }
        }
        Ok(out)
    }

    pub fn commit(&self, events: Vec<Durable1>) -> anyhow::Result<Vec<Event>> {
        self.commit_with(events, |_, _, _| Ok(()))
    }

    pub fn commit_one(&self, event: SessionEvent) -> anyhow::Result<Event> {
        Ok(self.commit(vec![event.into()])?.remove(0))
    }
}
