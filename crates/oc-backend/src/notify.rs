//! `State.changes` (#56267): the public projection of each gated catalog domain, so `*.updated` is
//! published only when what clients would read changed.

use std::sync::{Arc, Weak};

use serde_json::{Value, json};

use oc_ai::credentials::CredentialEvent;

use crate::engine::Engine;
use crate::locations::Loc;

/// The built-in provider plugins whose `credential.switched` subscription reloads providers right after reading
/// the new connection (`opencode.provider.<id>` and the integration it filters on), in plugin order.
const SWITCH_RELOADS: [(&str, &str); 8] = [
    ("opencode.provider.azure", "azure"),
    ("opencode.provider.chatgpt", "openai"),
    ("opencode.provider.digitalocean", "digitalocean"),
    ("opencode.provider.gitlab", "gitlab"),
    ("opencode.provider.modal", "modal"),
    ("opencode.provider.snowflake.cortex", "snowflake-cortex"),
    ("opencode.provider.openai", "openai"),
    ("opencode.provider.openai-context-caps", "openai"),
];

fn pick(value: &Value, keys: &[&str]) -> Value {
    let mut out = serde_json::Map::new();
    for k in keys {
        if let Some(v) = value.get(*k) {
            out.insert((*k).to_string(), v.clone());
        }
    }
    Value::Object(out)
}

impl Engine {
    /// The serialized projection `domain`'s notify compares (`agent.ts`, `command.ts`, `skill.ts`,
    /// `reference.ts`, `websearch.ts`, `integration.ts`, `plugin.ts`); `None` when it cannot be computed.
    pub async fn projection(&self, loc: &Loc, domain: &str) -> Option<String> {
        let value = match domain {
            // `list(data)`: the default agent first, then the rest.
            "agent" => serde_json::to_value(self.agents(loc).await.list()).ok()?,
            // `[{name, description}]`: a template-only edit changes nothing clients list.
            "command" => Value::Array(
                self.commands(loc)
                    .await
                    .into_iter()
                    .map(|(name, (description, _))| json!({ "name": name, "description": description }))
                    .collect(),
            ),
            "skill" => Value::Array(self.skills(loc).await.iter().map(crate::catalog::skill_json).collect()),
            "reference" => Value::Array(
                self.app.plugins.references(&loc.location, &self.app.global).await.iter().map(|r| r.to_json()).collect(),
            ),
            // `[{id, name}]` sorted by name.
            "websearch" => {
                let providers = serde_json::to_value(self.configure_websearch(loc).providers().await).ok()?;
                let mut list: Vec<Value> = providers.as_array()?.iter().map(|p| pick(p, &["id", "name"])).collect();
                list.sort_by(|a, b| {
                    oc_mcp::manager::locale_cmp(a["name"].as_str().unwrap_or_default(), b["name"].as_str().unwrap_or_default())
                });
                Value::Array(list)
            }
            // Projected integrations sorted by name, with connections resolved without saved credentials.
            "integration" => {
                self.catalog(loc).await.ok()?;
                let list = self.models().integrations_for(Some(&loc.key)).list().await.ok()?;
                let mut list: Vec<Value> = serde_json::to_value(list).ok()?.as_array()?.clone();
                for item in list.iter_mut() {
                    if let Some(connections) = item.get_mut("connections").and_then(Value::as_array_mut) {
                        connections.retain(|c| c.get("type").and_then(Value::as_str) != Some("credential"));
                    }
                }
                list.sort_by(|a, b| {
                    oc_mcp::manager::locale_cmp(a["name"].as_str().unwrap_or_default(), b["name"].as_str().unwrap_or_default())
                });
                Value::Array(list)
            }
            // The inventory (GUI bundle revisions are not served by ocd).
            "plugin" => json!([self.app.plugins.inventory(&loc.key), []]),
            _ => return None,
        };
        Some(value.to_string())
    }

    /// Core reloads a location's configuration and announces `config.updated`: the cached services are
    /// dropped. With the session runner (which reloads configurations) they are rebuilt right away, so the
    /// reload notifies like upstream's States reloading in place, and then the plugin activation the reload
    /// requested starts (upstream's supervisor activates on `config.updated`, after those notifications).
    pub(crate) fn install_reloads(self: &Arc<Self>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        let eager = self.runner.is_some();
        if eager {
            self.app.plugins.defer_reload_activations();
        }
        let mut rx = self.app.bus.subscribe();
        let weak: Weak<Engine> = Arc::downgrade(self);
        handle.spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(e) if e.kind == "config.updated" => {
                        let Some(engine) = weak.upgrade() else { break };
                        let Some(directory) = e.location.as_ref().map(|l| l.directory.clone()) else { continue };
                        let loaded = engine.locations.config_updated(&directory);
                        if !eager {
                            continue;
                        }
                        tokio::spawn(async move {
                            if loaded {
                                let _ = engine.locations.get(&directory).await;
                            }
                            engine.app.plugins.release_reload(&directory);
                            engine.app.plugins.release_reload(&oc_core::location::canonical_directory(&directory));
                        });
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if weak.strong_count() == 0 {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    /// What upstream's `credential.updated`/`credential.switched` subscribers publish before the credential
    /// operation goes on (the bus resumes type subscribers inline, `bus.ts` `notify`): at every loaded location,
    /// in load order, the Provider State announces a change of the available providers (`provider.ts`, then
    /// `model.updated` from the Model State following it), and the policy plugin, when the account behind an
    /// integration's active credential changed, reloads providers and web search in one `State.batch`
    /// (`config/plugin/policy.ts`: `provider.updated`, `model.updated`, and `websearch.updated` when its list
    /// changed).
    pub(crate) fn install_credential_observer(self: &Arc<Self>) {
        let weak: Weak<Engine> = Arc::downgrade(self);
        // The Copilot plugin reloads at every loaded location on a switch, also where no catalog was built yet.
        let loaded = weak.clone();
        self.app.plugins.copilot.add_loaded_locations(Arc::new(move || {
            let weak = loaded.clone();
            Box::pin(async move {
                let Some(engine) = weak.upgrade() else { return vec![] };
                let mut out = vec![];
                for key in engine.locations.keys() {
                    if engine.app.plugins.copilot_enabled(&key) {
                        let integrations = engine.models().prepared_integrations(&key).await;
                        out.push((key, integrations));
                    }
                }
                out
            })
        }));
        let all = oc_core::models::DbCredentials::all_of(&self.app.db).unwrap_or_default();
        let accounts = Arc::new(parking_lot::Mutex::new(oc_core::policy::accounts(&all)));
        // Every loaded location's Provider State holds the snapshot it announced: one whose catalog was never
        // built builds it now, before the change.
        let preparing = weak.clone();
        self.models().prepare_credentials(Arc::new(move || {
            let weak = preparing.clone();
            Box::pin(async move {
                let Some(engine) = weak.upgrade() else { return };
                for key in engine.locations.keys() {
                    if engine.locations.activating(&key) {
                        continue;
                    }
                    let Some(loc) = engine.locations.loaded(&key) else { continue };
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine.app.plugins.await_activation(&key)).await;
                    if let Ok(catalog) = engine.catalog(&loc).await {
                        engine.models().note_availability(&key, &catalog);
                    }
                }
            })
        }));
        self.models().observe_credentials(Arc::new(move |event| {
            let (weak, accounts) = (weak.clone(), accounts.clone());
            Box::pin(async move {
                if let Some(engine) = weak.upgrade() {
                    engine.credential_changed(&event, &accounts).await;
                }
            })
        }));
    }

    async fn credential_changed(&self, event: &CredentialEvent, accounts: &parking_lot::Mutex<oc_core::policy::Accounts>) {
        let switched = match event {
            CredentialEvent::Switched { integration_id, .. } => Some(integration_id.as_str()),
            CredentialEvent::Updated => None,
        };
        let all = oc_core::models::DbCredentials::all_of(&self.app.db).unwrap_or_default();
        let next = oc_core::policy::accounts(&all);
        let reload = {
            let mut current = accounts.lock();
            let changed = *current != next;
            *current = next;
            changed
        };
        let bus = &self.app.bus;
        let pair = |at: &Option<oc_schema::types::LocationRef>| {
            bus.publish("provider.updated", json!({}), at.clone());
            bus.publish("model.updated", json!({}), at.clone());
        };
        for key in self.locations.keys() {
            let at = Some(oc_schema::types::LocationRef { directory: key.clone(), workspace_id: None });
            // A location still activating has its Provider State but no plugin subscribed yet; the State's first
            // notify always announces.
            if self.locations.activating(&key) {
                if self.models().first_notify(&key) {
                    pair(&at);
                }
                continue;
            }
            if self.models().availability_changed(&key).await {
                pair(&at);
            }
            // The provider plugins reloading on a switch of their integration, in plugin order (`ctx.provider.reload()`
            // after loading the new connection). GitHub Copilot, OpenCode and Claude Agent reload after their own
            // network or process round trip.
            if let Some(integration) = switched {
                for (plugin, of) in SWITCH_RELOADS {
                    if of == integration && self.app.plugins.builtin_enabled(&key, plugin) {
                        pair(&at);
                    }
                }
            }
            if reload && self.app.plugins.builtin_enabled(&key, oc_core::policy::PLUGIN_ID) {
                pair(&at);
                if self.app.plugins.domain_changed_since_boot(&key, "websearch").await {
                    bus.publish("websearch.updated", json!({}), at.clone());
                }
            }
        }
    }

    /// Lets the plugin layer and location rebuilds gate `*.updated` events on these projections.
    /// `PersistentPty` subscribes to `session.deleted` and removes the session's terminals (#60322). Events are
    /// handled one at a time, and failures are logged per event so one never ends the subscription.
    pub(crate) fn install_pty_cleanup(self: &Arc<Self>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        let mut rx = self.app.bus.subscribe();
        let weak: Weak<Engine> = Arc::downgrade(self);
        handle.spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(e) if e.kind == "session.deleted" => {
                        let Some(engine) = weak.upgrade() else { break };
                        let Some(session) = e.data.get("sessionID").and_then(|v| v.as_str()) else { continue };
                        engine.persistent().await.remove_session(session).await;
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
    }

    pub(crate) fn install_projector(self: &Arc<Self>) {
        let weak: Weak<Engine> = Arc::downgrade(self);
        let for_locations = weak.clone();
        self.locations.set_projector(Arc::new(move |loc: Arc<Loc>, domain: &'static str| {
            let weak = for_locations.clone();
            Box::pin(async move { weak.upgrade()?.projection(&loc, domain).await })
        }));
        self.app.plugins.set_projector(Arc::new(move |key: String, domain: &'static str| {
            let weak = weak.clone();
            Box::pin(async move {
                let engine = weak.upgrade()?;
                let loc = engine.locations.loaded(&key)?;
                engine.projection(&loc, domain).await
            })
        }));
    }
}
