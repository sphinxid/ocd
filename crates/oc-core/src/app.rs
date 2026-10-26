//! The core runtime: shared services for one server process.

use std::sync::{Arc, OnceLock};

use parking_lot::RwLock;
use rusqlite::params;
use serde_json::Value;

use oc_schema::types::{Effect, PermissionRule, Ruleset};
use oc_storage::Database;
use oc_util::Global;

use crate::agent::Agents;
use crate::bus::Bus;
use crate::coordinator::Coordinator;
use crate::permission::{PermissionContext, Permissions};
use crate::session::Sessions;

pub struct App {
    pub global: Arc<Global>,
    pub db: Database,
    pub bus: Bus,
    pub sessions: Sessions,
    pub permissions: Arc<Permissions>,
    pub agents: Arc<RwLock<Agents>>,
    pub coordinator: OnceLock<Coordinator>,
    pub jobs: Arc<crate::job::Jobs>,
    pub forms: crate::form::Forms,
    pub ptys: crate::pty::Ptys,
    /// `SessionEnvironment`: full replacement environments per session.
    pub environments: Arc<parking_lot::Mutex<std::collections::HashMap<String, std::collections::HashMap<String, String>>>>,
    /// Server plugins (Bun sidecar, started on demand).
    pub plugins: Arc<crate::plugins::Plugins>,
    /// `SessionModelTransport`: per-session Responses WebSockets.
    pub model_transport: Arc<crate::model_transport::SessionModelTransport>,
    pub version: String,
}

struct PermCtx {
    db: Database,
    bus: Bus,
    agents: Arc<RwLock<Agents>>,
    plugins: Arc<crate::plugins::Plugins>,
}

impl PermCtx {
    fn project_of(&self, session: Option<&str>) -> Option<String> {
        let session = session?;
        self.db.session_get(session).ok().flatten().map(|s| s.project_id)
    }
}

impl PermissionContext for PermCtx {
    fn configured(&self, session_id: &str, agent: Option<&str>) -> Option<Ruleset> {
        let session = self.db.session_get(session_id).ok().flatten()?;
        let agents = self.agents.read();
        let id = agent.map(str::to_string).or(session.agent.clone());
        let base = agents.select(id.as_deref()).map(|a| a.permissions.clone()).unwrap_or_else(crate::permission::missing_agent_permissions);
        Some(crate::permission::merge(&[&base, session.permissions.as_deref().unwrap_or(&[])]))
    }

    fn saved(&self, session_id: &str) -> Vec<PermissionRule> {
        let Some(project) = self.project_of(Some(session_id)) else { return vec![] };
        self.db
            .with(|c| {
                let mut s = c.prepare("SELECT action, resource FROM permission WHERE project_id = ? ORDER BY time_created, id")?;
                let r = s
                    .query_map([project], |r| {
                        Ok(PermissionRule { action: r.get(0)?, resource: r.get(1)?, effect: Effect::Allow })
                    })?
                    .collect();
                r
            })
            .unwrap_or_default()
    }

    fn save(&self, session_id: &str, action: &str, resources: &[String]) {
        let Some(project) = self.project_of(Some(session_id)) else { return };
        let now = oc_schema::id::now_ms() as i64;
        let _ = self.db.with(|c| {
            for r in resources {
                c.execute(
                    "INSERT INTO permission (id, project_id, action, resource, time_created, time_updated) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
                    params![oc_schema::id::permission_saved(), project, action, r, now, now],
                )?;
            }
            Ok(())
        });
    }

    fn evaluate_hook(&self, session_id: &str, event: Value) -> futures::future::BoxFuture<'static, Value> {
        let plugins = self.plugins.clone();
        let key = plugins.session_key(session_id);
        Box::pin(async move {
            let Some(key) = key else { return event };
            let mut out = if !plugins.has_hook(&key, "permission.evaluate", None) {
                event
            } else {
                match plugins.trigger(&key, "permission.evaluate", event.clone()).await {
                    Ok(out) => out,
                    Err(error) => {
                        tracing::error!(%error, "permission.evaluate hook failed");
                        event
                    }
                }
            };
            // opencode.config.policy registers its hook last (a `post` plugin): policies deny last.
            crate::policy::evaluate(&plugins.policies_at(&key), &mut out);
            out
        })
    }

    fn publish(&self, kind: &str, data: Value) {
        let session = data.get("sessionID").and_then(Value::as_str).map(str::to_string);
        match session {
            Some(s) => {
                self.bus.publish_session_ephemeral(&s, kind, data);
            }
            None => {
                self.bus.publish(kind, data, None);
            }
        }
    }
}

impl App {
    pub fn new(global: Global, db: Database, version: impl Into<String>) -> Arc<App> {
        let version = version.into();
        let bus = Bus::new(db.clone());
        let agents = Arc::new(RwLock::new(Agents::builtin(&global.home)));
        let global = Arc::new(global);
        let plugins = crate::plugins::Plugins::new(global.clone(), db.clone(), bus.clone(), &version);
        let permissions = Arc::new(Permissions::new(Arc::new(PermCtx {
            db: db.clone(),
            bus: bus.clone(),
            agents: agents.clone(),
            plugins: plugins.clone(),
        })));
        Arc::new(App {
            plugins,
            global,
            sessions: Sessions::new(bus.clone(), version.clone()),
            jobs: Arc::new(crate::job::Jobs::new(db.clone())),
            forms: crate::form::Forms::new(bus.clone()),
            ptys: crate::pty::Ptys::new(bus.clone()),
            environments: Default::default(),
            db,
            bus,
            permissions,
            agents,
            coordinator: OnceLock::new(),
            model_transport: Arc::default(),
            version,
        })
    }

    pub fn coordinator(&self) -> &Coordinator {
        self.coordinator.get().expect("coordinator installed")
    }
}
