//! A Location: one working directory with its resolved project and configuration
//! (`core/src/location*.ts`, config folding via oc-config).

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use oc_config::fold::{self, AgentState};
use oc_config::schema::Entry;
use serde_json::Value;

use crate::agent::{Agent, Agents, Mode};
use crate::project::{self, Resolved};

pub struct Location {
    /// The directory exactly as requested (location cache key, plugin location name).
    pub key: String,
    pub directory: PathBuf,
    pub project: Resolved,
    pub entries: Vec<Entry>,
    pub sources: oc_config::discovery::Sources,
}

fn mode_of(m: Option<&oc_config::schema::AgentMode>) -> Option<Mode> {
    let v = serde_json::to_value(m?).ok()?;
    serde_json::from_value(v).ok()
}

fn to_state(a: &Agent) -> AgentState {
    AgentState {
        model: a.model.as_ref().map(|m| oc_config::schema::ModelSelection {
            provider_id: m.provider_id.clone(),
            model: m.id.clone(),
            variant: m.variant.clone(),
        }),
        headers: a.request.headers.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect(),
        body: a.request.body.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        system: a.system.clone(),
        description: a.description.clone(),
        mode: serde_json::to_value(a.mode).ok().and_then(|v| v.as_str().and_then(oc_config::schema::AgentMode::parse)),
        hidden: Some(a.hidden),
        color: a.color.clone(),
        steps: a.steps.map(u64::from),
        permissions: a.permissions.clone(),
    }
}

impl Location {
    pub async fn load(global: &oc_util::Global, bus: &crate::bus::Bus, directory: &Path) -> anyhow::Result<Location> {
        let key = directory.display().to_string();
        let directory = std::fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
        let project = project::resolve_and_persist(bus, &directory).await?;
        let loader = oc_config::load::Loader::new(global, directory.clone(), oc_config::discovery::Options::from_env());
        let (sources, entries) = loader.entries().await.map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Location { key, directory, project, entries, sources })
    }

    /// Built-in agents with configuration applied (AgentPlugin → PlanPlugin → ConfigAgent).
    pub fn agents(&self, home: &Path) -> Agents {
        self.agents_from(Agents::builtin(home), home, true)
    }

    /// `agents` after the built-in agents went through plugin transforms: configuration
    /// (`opencode.config.agent`) and the browser deny (`opencode.browser`, when enabled) apply on top.
    pub fn agents_from(&self, builtin: Agents, home: &Path, browser: bool) -> Agents {
        let existing: IndexMap<String, AgentState> =
            builtin.items.iter().map(|(id, a)| (id.clone(), to_state(a))).collect();
        let mut docs = self.entries.clone();
        docs.extend(oc_config::sources::agent_documents(&self.entries));
        let folded = fold::agents(&docs, home, existing);
        let mut out = Agents { items: IndexMap::new(), default: folded.default_agent.clone().or(builtin.default.clone()) };
        for (id, mut state) in folded.agents {
            let known = builtin.items.get(&id).cloned();
            let base = known.clone().unwrap_or_else(|| Agent::default_for(&id));
            // A configured agent that did not exist yet starts from `Info.default(id)` (default
            // permissions), and the config plugin appends the top-level rules after them
            // (core/src/agent.ts editor.update, core/src/config/plugin/agent.ts).
            if known.is_none() {
                let mut permissions = base.permissions.clone();
                permissions.append(&mut state.permissions);
                state.permissions = permissions;
            }
            // The built-in browser plugin, registered after the config agent plugin, appends a browser
            // deny to every agent until a desktop attachment allows it (plugin-browser/src/connection.ts).
            if browser {
                state.permissions.push(oc_schema::types::PermissionRule {
                    action: "browser".into(),
                    resource: "*".into(),
                    effect: oc_schema::types::Effect::Deny,
                });
            }
            out.items.insert(
                id.clone(),
                Agent {
                    model: state.model.map(|m| oc_schema::types::ModelRef {
                        id: m.model,
                        provider_id: m.provider_id,
                        variant: m.variant,
                    }),
                    request: crate::agent::Request {
                        settings: base.request.settings.clone(),
                        headers: state.headers.into_iter().map(|(k, v)| (k, Value::String(v))).collect(),
                        body: state.body.into_iter().collect(),
                    },
                    system: state.system,
                    description: state.description,
                    mode: mode_of(state.mode.as_ref()).unwrap_or(base.mode),
                    hidden: state.hidden.unwrap_or(base.hidden),
                    color: state.color,
                    steps: state.steps.map(|s| s as u32),
                    permissions: state.permissions,
                    ..base
                },
            );
        }
        out
    }
}
