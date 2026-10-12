//! Credential storage (`packages/core/src/credential.ts`, `credential/sql.ts`).
//!
//! Core backs [`CredentialStore`] with the SQLite `credential` table
//! (`id TEXT PK, integration_id TEXT, label TEXT NOT NULL, value TEXT(JSON) NOT NULL, connector_id, method_id,
//! active INTEGER(bool), time_created, time_updated`). The selection rules are factored into pure functions
//! ([`list_order`], [`selected`], [`plan_create`], [`replacement`]) so the SQL implementation and
//! [`MemoryCredentialStore`] behave identically:
//!
//! * list order: `active ASC, time_created ASC, id ASC` (SQLite sorts NULL first);
//! * an integration's selected credential: first by `active DESC, time_created DESC, id DESC`;
//! * `create` activates the new credential unless `activate == Some(false)` and another one exists; when it does
//!   not activate and the current selection is a legacy row (`active` NULL/false), that row is pinned `active=1`;
//! * removing the selected credential activates the newest remaining one (`time_created DESC, id DESC`).
//!
//! Events: `credential.updated {}` and `credential.switched {integrationID, credentialID|null}`.

use super::{Credential, CredentialValue};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::cmp::Ordering;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Input of [`CredentialStore::create`] (`Credential.CreateInput`).
#[derive(Debug, Clone, PartialEq)]
pub struct CreateCredential {
    pub id: Option<String>,
    pub integration_id: String,
    pub value: CredentialValue,
    /// Defaults to `"default"` at the store level; [`super::Integrations`] picks unique names.
    pub label: Option<String>,
    pub activate: Option<bool>,
}

impl CreateCredential {
    pub fn new(integration_id: impl Into<String>, value: CredentialValue) -> Self {
        Self { id: None, integration_id: integration_id.into(), value, label: None, activate: None }
    }
}

/// `Partial<Pick<Info, "label" | "value">>`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CredentialUpdate {
    pub label: Option<String>,
    pub value: Option<CredentialValue>,
}

/// Bus events published by the store (always `global`).
#[derive(Debug, Clone, PartialEq)]
pub enum CredentialEvent {
    /// `credential.updated`
    Updated,
    /// `credential.switched`
    Switched { integration_id: String, credential_id: Option<String> },
}

impl CredentialEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Updated => "credential.updated",
            Self::Switched { .. } => "credential.switched",
        }
    }
}

/// Persistent credential storage (`Credential.Interface`). Implementations publish [`CredentialEvent`]s.
#[async_trait]
pub trait CredentialStore: Send + Sync {
    /// Every stored credential in list order. Rows without an integration id are skipped.
    async fn all(&self) -> anyhow::Result<Vec<Credential>>;
    /// Credentials of one integration in list order.
    async fn list(&self, integration_id: &str) -> anyhow::Result<Vec<Credential>>;
    async fn get(&self, id: &str) -> anyhow::Result<Option<Credential>>;
    /// Creates a credential; it becomes the selection unless `activate == Some(false)` and another exists.
    async fn create(&self, input: CreateCredential) -> anyhow::Result<Credential>;
    /// Selects a stored credential for its integration.
    async fn activate(&self, id: &str) -> anyhow::Result<()>;
    /// Updates the label and/or value.
    async fn update(&self, id: &str, update: CredentialUpdate) -> anyhow::Result<()>;
    async fn remove(&self, id: &str) -> anyhow::Result<()>;
}

/// One storage row as seen by the selection rules.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: String,
    pub integration_id: Option<String>,
    pub label: String,
    pub value: CredentialValue,
    /// `NULL` for legacy rows.
    pub active: Option<bool>,
    pub time_created: i64,
    pub time_updated: i64,
}

impl Row {
    pub fn credential(&self) -> Option<Credential> {
        Some(Credential {
            id: self.id.clone(),
            integration_id: self.integration_id.clone()?,
            label: self.label.clone(),
            value: self.value.clone(),
            active: self.active,
        })
    }
}

/// SQLite ordering of a nullable boolean: NULL < 0 < 1.
fn active_rank(active: Option<bool>) -> i8 {
    match active {
        None => -1,
        Some(false) => 0,
        Some(true) => 1,
    }
}

/// `ORDER BY active ASC, time_created ASC, id ASC`.
pub fn list_order(a: &Row, b: &Row) -> Ordering {
    active_rank(a.active)
        .cmp(&active_rank(b.active))
        .then(a.time_created.cmp(&b.time_created))
        .then(a.id.cmp(&b.id))
}

/// `ORDER BY active DESC, time_created DESC, id DESC` → first row: the integration's selected credential.
pub fn selected<'a>(rows: impl IntoIterator<Item = &'a Row>) -> Option<&'a Row> {
    rows.into_iter().min_by(|a, b| list_order(b, a))
}

/// Newest remaining row (`ORDER BY time_created DESC, id DESC`).
pub fn replacement<'a>(rows: impl IntoIterator<Item = &'a Row>) -> Option<&'a Row> {
    rows.into_iter().min_by(|a, b| b.time_created.cmp(&a.time_created).then(b.id.cmp(&a.id)))
}

/// What `create` must do for the rows already stored for the integration.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatePlan {
    /// The new row is inserted `active = activate`; when true, all others are cleared first.
    pub activate: bool,
    /// Legacy selected row to pin `active = 1` before inserting a non-activated newer row.
    pub pin: Option<String>,
}

pub fn plan_create(existing: &[Row], activate: Option<bool>) -> CreatePlan {
    let current = selected(existing.iter());
    let activate = activate != Some(false) || current.is_none();
    let pin = match current {
        Some(row) if !activate && row.active != Some(true) => Some(row.id.clone()),
        _ => None,
    };
    CreatePlan { activate, pin }
}

/// Unique label for a new credential: `"<name>"`, `"<name> 2"`, `"<name> 3"`, ... (`integration.ts:385-394`).
pub fn unique_label(name: &str, existing: &[String]) -> String {
    let labels: std::collections::HashSet<&str> = existing.iter().map(String::as_str).collect();
    (0..=labels.len())
        .map(|index| if index == 0 { name.to_string() } else { format!("{name} {}", index + 1) })
        .find(|candidate| !labels.contains(candidate.as_str()))
        .unwrap_or_else(|| name.to_string())
}

/// In-memory [`CredentialStore`] following the SQLite semantics exactly (tests, ephemeral runs).
pub struct MemoryCredentialStore {
    rows: Mutex<Vec<Row>>,
    clock: Mutex<i64>,
    events: broadcast::Sender<CredentialEvent>,
}

impl Default for MemoryCredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryCredentialStore {
    pub fn new() -> Self {
        Self { rows: Mutex::new(Vec::new()), clock: Mutex::new(0), events: broadcast::channel(256).0 }
    }

    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CredentialEvent> {
        self.events.subscribe()
    }

    /// Inserts a raw row (e.g. a legacy row with `active = NULL`).
    pub fn insert_row(&self, row: Row) {
        self.rows.lock().push(row);
    }

    pub fn rows(&self) -> Vec<Row> {
        self.rows.lock().clone()
    }

    /// Monotonic creation timestamp (wall-clock ms, strictly increasing).
    fn tick(&self) -> i64 {
        let mut last = self.clock.lock();
        let now = super::now_ms() as i64;
        *last = if now > *last { now } else { *last + 1 };
        *last
    }

    fn publish(&self, event: CredentialEvent) {
        let _ = self.events.send(event);
    }

    fn sorted(rows: impl Iterator<Item = Row>) -> Vec<Credential> {
        let mut rows: Vec<Row> = rows.collect();
        rows.sort_by(list_order);
        rows.iter().filter_map(Row::credential).collect()
    }
}

#[async_trait]
impl CredentialStore for MemoryCredentialStore {
    async fn all(&self) -> anyhow::Result<Vec<Credential>> {
        Ok(Self::sorted(self.rows.lock().clone().into_iter()))
    }

    async fn list(&self, integration_id: &str) -> anyhow::Result<Vec<Credential>> {
        let rows = self.rows.lock().clone();
        Ok(Self::sorted(rows.into_iter().filter(|r| r.integration_id.as_deref() == Some(integration_id))))
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Credential>> {
        Ok(self.rows.lock().iter().find(|r| r.id == id).and_then(Row::credential))
    }

    async fn create(&self, input: CreateCredential) -> anyhow::Result<Credential> {
        let now = self.tick();
        let credential = Credential {
            id: input.id.unwrap_or_else(oc_schema::id::credential),
            integration_id: input.integration_id,
            label: input.label.unwrap_or_else(|| "default".to_string()),
            value: input.value,
            active: None,
        };
        let activated = {
            let mut rows = self.rows.lock();
            let existing: Vec<Row> = rows
                .iter()
                .filter(|r| r.integration_id.as_deref() == Some(credential.integration_id.as_str()))
                .cloned()
                .collect();
            let plan = plan_create(&existing, input.activate);
            for row in rows.iter_mut() {
                if row.integration_id.as_deref() != Some(credential.integration_id.as_str()) {
                    continue;
                }
                if plan.activate {
                    row.active = Some(false);
                } else if plan.pin.as_deref() == Some(row.id.as_str()) {
                    row.active = Some(true);
                }
            }
            rows.push(Row {
                id: credential.id.clone(),
                integration_id: Some(credential.integration_id.clone()),
                label: credential.label.clone(),
                value: credential.value.clone(),
                active: Some(plan.activate),
                time_created: now,
                time_updated: now,
            });
            plan.activate
        };
        self.publish(CredentialEvent::Updated);
        if activated {
            self.publish(CredentialEvent::Switched {
                integration_id: credential.integration_id.clone(),
                credential_id: Some(credential.id.clone()),
            });
        }
        Ok(Credential { active: Some(activated), ..credential })
    }

    async fn activate(&self, id: &str) -> anyhow::Result<()> {
        let integration = {
            let mut rows = self.rows.lock();
            let Some(integration) = rows.iter().find(|r| r.id == id).and_then(|r| r.integration_id.clone()) else {
                return Ok(());
            };
            let current = selected(rows.iter().filter(|r| r.integration_id.as_deref() == Some(&integration)))
                .map(|r| r.id.clone());
            if current.as_deref() == Some(id) {
                return Ok(());
            }
            for row in rows.iter_mut() {
                if row.integration_id.as_deref() == Some(&integration) {
                    row.active = Some(row.id == id);
                }
            }
            integration
        };
        self.publish(CredentialEvent::Switched { integration_id: integration, credential_id: Some(id.to_string()) });
        Ok(())
    }

    async fn update(&self, id: &str, update: CredentialUpdate) -> anyhow::Result<()> {
        if update.label.is_none() && update.value.is_none() {
            return Ok(());
        }
        let label_changed = {
            let mut rows = self.rows.lock();
            let Some(row) = rows.iter_mut().find(|r| r.id == id) else { return Ok(()) };
            if row.integration_id.is_none() {
                return Ok(());
            }
            if update.label.as_deref() == Some(row.label.as_str()) && update.value.is_none() {
                return Ok(());
            }
            let changed = update.label.as_ref().is_some_and(|l| *l != row.label);
            if let Some(label) = update.label {
                row.label = label;
            }
            if let Some(value) = update.value {
                row.value = value;
            }
            row.time_updated = super::now_ms() as i64;
            changed
        };
        if label_changed {
            self.publish(CredentialEvent::Updated);
        }
        Ok(())
    }

    async fn remove(&self, id: &str) -> anyhow::Result<()> {
        let switched = {
            let mut rows = self.rows.lock();
            let Some(index) = rows.iter().position(|r| r.id == id) else { return Ok(()) };
            let integration = rows[index].integration_id.clone();
            let was_selected = integration.as_ref().is_some_and(|integration| {
                selected(rows.iter().filter(|r| r.integration_id.as_ref() == Some(integration)))
                    .is_some_and(|r| r.id == id)
            });
            rows.remove(index);
            match integration {
                Some(integration) if was_selected => {
                    let next = replacement(rows.iter().filter(|r| r.integration_id.as_ref() == Some(&integration)))
                        .map(|r| r.id.clone());
                    if let Some(next) = &next {
                        for row in rows.iter_mut() {
                            if row.integration_id.as_ref() == Some(&integration) {
                                row.active = Some(row.id == *next);
                            }
                        }
                    }
                    Some((integration, next))
                }
                _ => None,
            }
        };
        self.publish(CredentialEvent::Updated);
        if let Some((integration_id, credential_id)) = switched {
            self.publish(CredentialEvent::Switched { integration_id, credential_id });
        }
        Ok(())
    }
}
