//! `State.changes` (#56267): the public projection of each gated catalog domain, so `*.updated` is
//! published only when what clients would read changed.

use std::sync::{Arc, Weak};

use serde_json::{Value, json};

use crate::engine::Engine;
use crate::locations::Loc;

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

    /// Lets the plugin layer and location rebuilds gate `*.updated` events on these projections.
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
