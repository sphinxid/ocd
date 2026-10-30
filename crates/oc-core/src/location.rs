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
    /// The last fetched copy of each configured instruction URL (`ConfigInstructionPlugin` `remote`).
    pub remote_instructions: parking_lot::Mutex<std::collections::HashMap<String, crate::instructions::File>>,
    /// Set once the location is shut down (`LocationLifecycle.isClosed`): steps that borrowed it hand off to a
    /// fresh load at their next boundary.
    closed: std::sync::atomic::AtomicBool,
}

/// The rules `opencode.browser` appends to every agent: the browser tools and the `browser` skill,
/// which a desktop attachment allows together (plugin-browser/src/connection.ts `access`).
pub fn browser_denies() -> [oc_schema::types::PermissionRule; 2] {
    let deny = |action: &str, resource: &str| oc_schema::types::PermissionRule {
        action: action.into(),
        resource: resource.into(),
        effect: oc_schema::types::Effect::Deny,
    };
    [deny("browser", "*"), deny("skill", "browser")]
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
        // The OpenCode Console's workspace config, when the location is signed in (`Config.integration`).
        let loader = oc_config::load::Loader::new(global, directory.clone(), oc_config::discovery::Options::from_env())
            .with_integrations(crate::console_config::documents(&key));
        let (sources, entries) = loader.entries().await.map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Location { key, directory, project, entries, sources, remote_instructions: Default::default(), closed: Default::default() })
    }

    /// `ConfigInstructionPlugin.reconfigure`: fetch the configured instruction URLs, keeping the
    /// last good copy from `previous` (the location this one replaces) when a fetch fails.
    pub async fn fetch_instructions(&self, previous: Option<&Location>) {
        let sources = crate::discovery::configured_sources(&self.entries);
        if !sources.iter().any(|s| crate::discovery::is_url(s)) {
            return;
        }
        let last = previous.map(|p| p.remote_instructions.lock().clone()).unwrap_or_default();
        let fetched = crate::discovery::fetch_remote(&sources, &last).await;
        *self.remote_instructions.lock() = fetched;
    }

    /// `LocationLifecycle.isClosed()`
    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn mark_closed(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
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
        // Config documents and each config directory's Markdown agents, in entry order. `agent_documents`
        // already includes every document; adding the entries again applied top-level and agent
        // permissions twice.
        let docs = oc_config::sources::agent_documents(&self.entries);
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
            // The built-in browser plugin, registered after the config agent plugin, appends the browser
            // tools' and skill's denies to every agent until a desktop attachment allows them
            // (plugin-browser/src/connection.ts).
            if browser {
                state.permissions.extend(browser_denies());
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

/// Node's `path.normalize` for one flavour: collapse repeated separators, drop `.` segments, resolve
/// `..` lexically (never above the root), keep a trailing separator. Returns `(root, normalized)`.
fn normalize_with(path: &str, sep: char, windows: bool) -> (String, String) {
    let unified: String = if windows { path.replace('/', "\\") } else { path.to_string() };
    // Root: `/` (posix); `\`, `C:\`, `C:` or `\\server\share\` (win32).
    let (root, rest) = if !windows {
        if unified.starts_with('/') { ("/".to_string(), unified.trim_start_matches('/').to_string()) } else { (String::new(), unified) }
    } else {
        let b = unified.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            let drive = unified[..2].to_string();
            let after = &unified[2..];
            if after.starts_with('\\') { (format!("{drive}\\"), after.trim_start_matches('\\').to_string()) } else { (drive, after.to_string()) }
        } else if unified.starts_with("\\\\") {
            let parts: Vec<&str> = unified[2..].splitn(3, '\\').collect();
            if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
                (format!("\\\\{}\\{}\\", parts[0], parts[1]), parts.get(2).copied().unwrap_or("").to_string())
            } else {
                ("\\".to_string(), unified.trim_start_matches('\\').to_string())
            }
        } else if unified.starts_with('\\') {
            ("\\".to_string(), unified.trim_start_matches('\\').to_string())
        } else {
            (String::new(), unified)
        }
    };
    let absolute = root.ends_with(sep);
    let trailing = rest.ends_with(sep);
    let mut segments: Vec<&str> = Vec::new();
    for part in rest.split(sep) {
        match part {
            "" | "." => {}
            ".." => {
                if segments.last().is_some_and(|s| *s != "..") {
                    segments.pop();
                } else if !absolute {
                    segments.push("..");
                }
            }
            other => segments.push(other),
        }
    }
    let mut body = segments.join(&sep.to_string());
    if body.is_empty() && !absolute {
        body = ".".into();
    }
    if trailing && !body.is_empty() && !(absolute && segments.is_empty()) {
        body.push(sep);
    }
    let normalized = format!("{root}{body}");
    (root, normalized)
}

/// `LocationServiceMap.canonical`: equivalent placements of one directory (`/p/`, `/p//`, `/a/../p`) become
/// one key. Node's `path.normalize` (win32 rules on Windows unless the path starts with `/`, since workspace
/// refs can hold remote POSIX paths), then one trailing separator is stripped unless the path is the root.
pub fn canonical_directory(directory: &str) -> String {
    let windows = cfg!(windows) && !directory.starts_with('/');
    let sep = if windows { '\\' } else { '/' };
    let (root, normalized) = normalize_with(directory, sep, windows);
    if normalized.ends_with(sep) && normalized.len() > root.len() {
        normalized[..normalized.len() - 1].to_string()
    } else {
        normalized
    }
}

#[cfg(test)]
mod canonical_tests {
    use super::canonical_directory;

    /// `location-layer.test.ts` "variants" and `session-create.test.ts` trailing-slash placements.
    #[test]
    fn equivalent_directories_share_one_key() {
        for (input, expected) in [
            ("/p", "/p"),
            ("/p/", "/p"),
            ("/p//", "/p"),
            ("//p", "/p"),
            ("/x//p", "/x/p"),
            ("/p/./child/../", "/p"),
            ("/alias/../packages/app", "/packages/app"),
            ("/", "/"),
            ("//", "/"),
            ("/..", "/"),
            ("/a/b/../../..", "/"),
        ] {
            assert_eq!(canonical_directory(input), expected, "{input}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Config documents went into the agent fold twice (once directly, once through `agent_documents`), so
    // top-level and agent permissions were applied twice.
    #[test]
    fn config_permissions_apply_once() {
        let info = oc_config::schema::Info::from_value(&json!({
            "permissions": [{ "action": "edit", "resource": "*", "effect": "ask" }],
            "agents": { "custom": { "permissions": [{ "action": "read", "resource": "*", "effect": "deny" }] } },
        }))
        .unwrap();
        let location = Location {
            key: "/p".into(),
            directory: "/p".into(),
            project: Resolved { id: "global".into(), previous: None, directory: "/p".into(), canonical: "/p".into(), vcs: None },
            entries: vec![Entry::Document { integration: None, path: None, info }],
            sources: Default::default(),
            remote_instructions: Default::default(),
            closed: Default::default(),
        };
        let agents = location.agents(Path::new("/home/u"));
        let count = |id: &str, action: &str| {
            agents.items[id].permissions.iter().filter(|r| r.action == action && r.resource == "*").count()
        };
        assert_eq!(count("build", "edit"), 1);
        assert_eq!(count("custom", "edit"), 1);
        assert_eq!(count("custom", "read"), 1);
    }
}
