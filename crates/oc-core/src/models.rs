//! Providers, credentials and model resolution (`core/provider.ts`, `credential.ts`,
//! `model-resolver.ts`, `session/runner/model.ts`) on top of oc-ai's catalog.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::params;
use serde_json::Value;

use oc_ai::catalog::{self, Access, Catalog, CatalogCache, ConfigProviders, IntegrationMethod, ModelsDev, ModelsDevOptions, ResolvedModel};
use oc_ai::credentials::store::{plan_create, replacement, selected, CreateCredential, CredentialEvent, CredentialUpdate, Row};
use oc_ai::credentials::{Credential, CredentialStore, CredentialValue, Integrations, Method};
use oc_config::schema::Entry;
use oc_schema::types::ModelRef;
use oc_storage::Database;

use crate::bus::Bus;

// ---------- credential table ----------

pub struct DbCredentials {
    db: Database,
    bus: Bus,
}

impl DbCredentials {
    pub fn new(db: Database, bus: Bus) -> Arc<Self> {
        Arc::new(DbCredentials { db, bus })
    }

    fn rows(&self) -> anyhow::Result<Vec<Row>> {
        Self::rows_of(&self.db)
    }

    fn rows_of(db: &Database) -> anyhow::Result<Vec<Row>> {
        db.with(|c| {
            let mut s = c.prepare("SELECT id, integration_id, label, value, active, time_created, time_updated FROM credential")?;
            let r = s
                .query_map([], |r| {
                    let value: String = r.get(3)?;
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?, value, r.get::<_, Option<bool>>(4)?, r.get::<_, i64>(5)?, r.get::<_, i64>(6)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(r.into_iter()
                .filter_map(|(id, integration_id, label, value, active, time_created, time_updated)| {
                    Some(Row { id, integration_id, label, value: serde_json::from_str(&value).ok()?, active, time_created, time_updated })
                })
                .collect())
        })
    }

    fn sorted(mut rows: Vec<Row>) -> Vec<Credential> {
        rows.sort_by(oc_ai::credentials::store::list_order);
        rows.iter().filter_map(Row::credential).collect()
    }

    fn publish(&self, e: CredentialEvent) {
        let data = match &e {
            CredentialEvent::Updated => serde_json::json!({}),
            CredentialEvent::Switched { integration_id, credential_id } => {
                serde_json::json!({"integrationID": integration_id, "credentialID": credential_id})
            }
        };
        self.bus.publish(e.event_type(), data, None);
    }
}

#[async_trait]
impl CredentialStore for DbCredentials {
    async fn all(&self) -> anyhow::Result<Vec<Credential>> {
        Ok(Self::sorted(self.rows()?))
    }

    async fn list(&self, integration_id: &str) -> anyhow::Result<Vec<Credential>> {
        Ok(Self::sorted(self.rows()?.into_iter().filter(|r| r.integration_id.as_deref() == Some(integration_id)).collect()))
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Credential>> {
        Ok(self.rows()?.iter().find(|r| r.id == id).and_then(Row::credential))
    }

    async fn create(&self, input: CreateCredential) -> anyhow::Result<Credential> {
        let now = oc_schema::id::now_ms() as i64;
        let id = input.id.unwrap_or_else(oc_schema::id::credential);
        let label = input.label.unwrap_or_else(|| "default".into());
        let existing: Vec<Row> =
            self.rows()?.into_iter().filter(|r| r.integration_id.as_deref() == Some(input.integration_id.as_str())).collect();
        let plan = plan_create(&existing, input.activate);
        let value = serde_json::to_string(&input.value)?;
        self.db.transaction(|c| {
            if plan.activate {
                c.execute("UPDATE credential SET active = 0 WHERE integration_id = ?", [&input.integration_id])?;
            } else if let Some(pin) = &plan.pin {
                c.execute("UPDATE credential SET active = 1 WHERE id = ?", [pin])?;
            }
            c.execute(
                "INSERT INTO credential (id, integration_id, label, value, active, time_created, time_updated) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![id, input.integration_id, label, value, plan.activate, now, now],
            )?;
            Ok(())
        })?;
        self.publish(CredentialEvent::Updated);
        if plan.activate {
            self.publish(CredentialEvent::Switched { integration_id: input.integration_id.clone(), credential_id: Some(id.clone()) });
        }
        Ok(Credential { id, integration_id: input.integration_id, label, value: input.value, active: Some(plan.activate) })
    }

    async fn activate(&self, id: &str) -> anyhow::Result<()> {
        let rows = self.rows()?;
        let Some(integration) = rows.iter().find(|r| r.id == id).and_then(|r| r.integration_id.clone()) else { return Ok(()) };
        let current = selected(rows.iter().filter(|r| r.integration_id.as_deref() == Some(&integration))).map(|r| r.id.clone());
        if current.as_deref() == Some(id) {
            return Ok(());
        }
        self.db.with(|c| c.execute("UPDATE credential SET active = (id = ?) WHERE integration_id = ?", params![id, integration]))?;
        self.publish(CredentialEvent::Switched { integration_id: integration, credential_id: Some(id.into()) });
        Ok(())
    }

    async fn update(&self, id: &str, update: CredentialUpdate) -> anyhow::Result<()> {
        if update.label.is_none() && update.value.is_none() {
            return Ok(());
        }
        let rows = self.rows()?;
        let Some(row) = rows.iter().find(|r| r.id == id) else { return Ok(()) };
        if row.integration_id.is_none() || (update.label.as_deref() == Some(row.label.as_str()) && update.value.is_none()) {
            return Ok(());
        }
        let changed = update.label.as_ref().is_some_and(|l| *l != row.label);
        let label = update.label.clone().unwrap_or_else(|| row.label.clone());
        let value = match &update.value {
            Some(v) => serde_json::to_string(v)?,
            None => serde_json::to_string(&row.value)?,
        };
        self.db.with(|c| {
            c.execute(
                "UPDATE credential SET label = ?, value = ?, time_updated = ? WHERE id = ?",
                params![label, value, oc_schema::id::now_ms() as i64, id],
            )
        })?;
        if changed {
            self.publish(CredentialEvent::Updated);
        }
        Ok(())
    }

    async fn remove(&self, id: &str) -> anyhow::Result<()> {
        let rows = self.rows()?;
        let Some(row) = rows.iter().find(|r| r.id == id) else { return Ok(()) };
        let integration = row.integration_id.clone();
        let was_selected = integration.as_ref().is_some_and(|i| {
            selected(rows.iter().filter(|r| r.integration_id.as_ref() == Some(i))).is_some_and(|r| r.id == id)
        });
        self.db.with(|c| c.execute("DELETE FROM credential WHERE id = ?", [id]))?;
        self.publish(CredentialEvent::Updated);
        if let (true, Some(integration)) = (was_selected, integration) {
            let remaining: Vec<Row> = rows.into_iter().filter(|r| r.id != id && r.integration_id.as_deref() == Some(&integration)).collect();
            let next = replacement(remaining.iter()).map(|r| r.id.clone());
            if let Some(n) = &next {
                self.db.with(|c| c.execute("UPDATE credential SET active = (id = ?) WHERE integration_id = ?", params![n, integration]))?;
            }
            self.publish(CredentialEvent::Switched { integration_id: integration, credential_id: next });
        }
        Ok(())
    }
}

// ---------- models.dev cache in kv ----------

pub struct KvCatalogCache(pub Database);

#[async_trait]
impl CatalogCache for KvCatalogCache {
    async fn get(&self, key: &str) -> Option<Value> {
        self.0.kv_get(key).ok().flatten()
    }
    async fn set(&self, key: &str, value: Value) -> Result<(), String> {
        self.0.kv_set(key, &value).map_err(|e| e.to_string())
    }
    async fn remove(&self, key: &str) {
        let _ = self.0.kv_remove(key);
    }
}

// ---------- model service ----------

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("No model is available for session {0}")]
    NotSelected(String),
    #[error("Model unavailable: {0}/{1}")]
    Unavailable(String, String),
    #[error("{0}")]
    Resolve(String),
}

pub struct Models {
    pub integrations: Arc<Integrations>,
    pub models_dev: Arc<ModelsDev>,
    db: Database,
    user_agent: String,
    /// The provider plugins' OAuth implementations are registered once, after the first catalog build placed
    /// the models.dev `key`/`env` slots.
    oauth: std::sync::Once,
}

/// Plugin storage of `opencode.provider.chatgpt` (`PluginHost.storage`: `plugin:<utf-16 hex id>:<key>` in kv).
struct PluginKv {
    db: Database,
    namespace: String,
}

impl PluginKv {
    fn new(db: Database, plugin: &str) -> Self {
        let hex: String = plugin.encode_utf16().map(|u| format!("{u:04x}")).collect();
        PluginKv { db, namespace: format!("plugin:{hex}:") }
    }
}

#[async_trait]
impl oc_ai::credentials::KvStore for PluginKv {
    async fn get(&self, key: &str) -> anyhow::Result<Option<Value>> {
        self.db.kv_get(&format!("{}{key}", self.namespace))
    }
    async fn set(&self, key: &str, value: Value) -> anyhow::Result<()> {
        self.db.kv_set(&format!("{}{key}", self.namespace), &value)
    }
    async fn remove(&self, key: &str) -> anyhow::Result<()> {
        self.db.kv_remove(&format!("{}{key}", self.namespace))
    }
}

fn method(m: &IntegrationMethod) -> Method {
    match m {
        IntegrationMethod::Key { label } => Method::Key { label: label.clone(), form: None },
        IntegrationMethod::Env { names } => Method::Env { names: names.clone() },
    }
}

impl Models {
    pub fn new(db: Database, bus: Bus, channel: &str, version: &str) -> Models {
        let store: Arc<dyn CredentialStore> = DbCredentials::new(db.clone(), bus);
        let ua = oc_ai::credentials::user_agent(channel, version, "opencode");
        Models {
            integrations: Integrations::new(store),
            models_dev: Arc::new(ModelsDev::with_reqwest(ModelsDevOptions::from_env(ua.clone()), Arc::new(KvCatalogCache(db.clone())))),
            db,
            user_agent: ua,
            oauth: std::sync::Once::new(),
        }
    }

    /// The provider and web-search plugins' integration registrations (crate::integrations), replayed after the
    /// models.dev ones on every catalog build, like upstream's integration State rebuild.
    fn register_builtin_integrations(&self, snapshots: &[catalog::models_dev::Snapshot], sections: &[Value]) {
        let initial = snapshots.iter().filter_map(|s| s.info.settings.clone().map(|settings| (s.info.id.clone(), settings)));
        let environment = crate::integrations::Environment {
            env: oc_ai::credentials::process_env(),
            home: crate::integrations::global_home(),
            settings: crate::integrations::settings(initial, sections),
            azure_cli: which::which("az").is_ok(),
        };
        crate::integrations::register(&self.integrations, &environment);
        self.oauth.call_once(|| {
            let db = self.db.clone();
            let chatgpt_client_id: oc_ai::credentials::flows::chatgpt::SavedClientId = Arc::new(move || {
                let rows = DbCredentials::rows_of(&db).ok()?;
                let openai: Vec<&Row> = rows.iter().filter(|r| r.integration_id.as_deref() == Some("openai")).collect();
                match &selected(openai)?.value {
                    CredentialValue::Oauth { method_id, metadata, .. } if method_id == "chatgpt-token-sharing" => {
                        metadata.as_ref()?.get("clientID")?.as_str().map(str::to_string)
                    }
                    _ => None,
                }
            });
            crate::integrations::register_oauth(
                &self.integrations,
                crate::integrations::OAuthDeps {
                    ctx: oc_ai::credentials::http::FlowContext::new(self.user_agent.clone()),
                    env: oc_ai::credentials::process_env(),
                    chatgpt_storage: Arc::new(PluginKv::new(self.db.clone(), "opencode.provider.chatgpt")),
                    chatgpt_client_id,
                },
            );
        });
    }

    /// Build the location catalog (models.dev → data plugins → configured providers).
    pub async fn catalog(&self, entries: &[Entry]) -> anyhow::Result<Catalog> {
        let snapshots = self.models_dev.get().await.map_err(|e| anyhow::anyhow!("{e}"))?;
        let sections: Vec<Value> = entries
            .iter()
            .filter_map(|e| match e {
                Entry::Document { info, .. } => info.providers.as_ref().map(|p| serde_json::to_value(p).unwrap_or(Value::Null)),
                _ => None,
            })
            .collect();
        // The configured top-level `model` is the catalog default (core/src/config/plugin/provider.ts).
        let default_model = oc_config::fold::model(entries).map(|m| catalog::config::ModelSelection {
            provider_id: m.provider_id.clone(),
            model: m.model.clone(),
            variant: m.variant.clone(),
        });
        let config = ConfigProviders::from_documents(&sections).with_default_model(default_model);
        // Plugin order (core/src/plugin/internal.ts): models.dev, then the provider and web-search plugins, then
        // the configured-provider plugin.
        let dev_specs = catalog::models_dev::integrations(&snapshots);
        let register_specs = |specs: &[catalog::IntegrationSpec]| {
            for spec in specs {
                if let Some(name) = &spec.name {
                    self.integrations.update(&spec.id, |r| r.name = name.clone());
                }
                for m in &spec.methods {
                    self.integrations.register_method(&spec.id, method(m));
                }
            }
        };
        register_specs(&dev_specs);
        self.register_builtin_integrations(&snapshots, &sections);
        let known: HashSet<String> = self.integrations.refs().into_iter().map(|r| r.id).collect();
        register_specs(&config.integrations(|id| known.contains(id)));
        let mut access = Access::new();
        for info in self.integrations.list().await.map_err(|e| anyhow::anyhow!("{e}"))? {
            access.set(info.id.clone(), info.connections.iter().map(|c| c.key()).collect());
        }
        let configured: HashSet<String> = self.integrations.refs().into_iter().map(|r| r.id).collect();
        let connected = std::env::var("OPENCODE_API_KEY").is_ok_and(|v| !v.is_empty())
            || access.connections("opencode").is_some_and(|c| !c.is_empty());
        let mut plugins = catalog::standard_plugins(&snapshots, configured, config);
        // Upstream registers the OpenCode Zen provider plugin among the provider plugins, before the
        // configured-provider plugin (core/src/plugin/internal.ts `pre`/`post`).
        let at = plugins.len().saturating_sub(1);
        plugins.insert(at, Box::new(catalog::plugins::OpencodeZen { connected }));
        Ok(Catalog::build(&mut plugins, &access))
    }

    pub async fn credential(&self, catalog: &Catalog, provider_id: &str) -> Option<CredentialValue> {
        let integration = catalog
            .provider(provider_id)
            .and_then(|p| p.integration_id.clone())
            .unwrap_or_else(|| provider_id.to_string());
        let connection = self.integrations.active(&integration).await.ok().flatten()?;
        self.integrations.resolve(&connection).await.ok().flatten()
    }

    /// `SessionRunnerModel.resolve`: session model → config model → first available text model.
    pub async fn resolve_for_session(
        &self,
        catalog: &Catalog,
        session_id: &str,
        session_model: Option<&ModelRef>,
        config_model: Option<&ModelRef>,
    ) -> anyhow::Result<ResolvedModel> {
        let pick = session_model.or(config_model).cloned().or_else(|| {
            catalog.select(None).map(|m| ModelRef { id: m.id.clone(), provider_id: m.provider_id.clone(), variant: None })
        });
        let Some(r) = pick else { return Err(ModelError::NotSelected(session_id.into()).into()) };
        if catalog.model(&r.provider_id, &r.id).is_none() {
            return Err(ModelError::Unavailable(r.provider_id.clone(), r.id.clone()).into());
        }
        let credential = self.credential(catalog, &r.provider_id).await;
        let variant = r.variant.as_deref().filter(|v| *v != "default");
        catalog
            .resolve(&r.provider_id, &r.id, variant, credential.as_ref())
            .map_err(|e| ModelError::Resolve(e.to_string()).into())
    }
}
