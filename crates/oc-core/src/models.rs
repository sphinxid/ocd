//! Providers, credentials and model resolution (`core/provider.ts`, `credential.ts`,
//! `model-resolver.ts`, `session/runner/model.ts`) on top of oc-ai's catalog.

use std::collections::{HashMap, HashSet};
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

/// One location's integration registry (upstream `Integration` is a location node): methods and
/// OAuth implementations, including plugin-registered ones, attempts and statuses. Credentials are
/// global (one store shared by every instance).
struct LocationIntegrations {
    integrations: Arc<Integrations>,
    /// The provider plugins' OAuth implementations are registered once per instance.
    oauth: std::sync::Once,
    /// The Azure plugin lists the `az login` account's resources once per location, in the background.
    azure_started: std::sync::atomic::AtomicBool,
    azure_resources: Arc<parking_lot::RwLock<Option<Vec<Value>>>>,
}

impl LocationIntegrations {
    fn new(integrations: Arc<Integrations>) -> Arc<Self> {
        Arc::new(LocationIntegrations {
            integrations,
            oauth: std::sync::Once::new(),
            azure_started: std::sync::atomic::AtomicBool::new(false),
            azure_resources: Arc::new(parking_lot::RwLock::new(None)),
        })
    }
}

pub struct Models {
    /// The registry of catalogs built without a location (CLI listings, tests).
    pub integrations: Arc<Integrations>,
    default_location: Arc<LocationIntegrations>,
    by_location: parking_lot::Mutex<HashMap<String, Arc<LocationIntegrations>>>,
    store: Arc<dyn CredentialStore>,
    /// Azure Resource Manager base URL for the resource listing (tests point it at a mock).
    azure_management: parking_lot::RwLock<String>,
    pub models_dev: Arc<ModelsDev>,
    db: Database,
    user_agent: String,
    /// Server plugins (`provider`/`model` transforms between the internal provider plugins and the
    /// configured-provider plugin).
    plugins: parking_lot::RwLock<Option<Arc<crate::plugins::Plugins>>>,
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
        let integrations = Integrations::new(store.clone());
        Models {
            integrations: integrations.clone(),
            default_location: LocationIntegrations::new(integrations),
            by_location: parking_lot::Mutex::new(HashMap::new()),
            store,
            azure_management: parking_lot::RwLock::new(crate::integrations::AZURE_MANAGEMENT.into()),
            models_dev: Arc::new(ModelsDev::with_reqwest(ModelsDevOptions::from_env(ua.clone()), Arc::new(KvCatalogCache(db.clone())))),
            db,
            user_agent: ua,
            plugins: parking_lot::RwLock::new(None),
        }
    }

    fn location_integrations(&self, key: &str) -> Arc<LocationIntegrations> {
        self.by_location
            .lock()
            .entry(key.to_string())
            .or_insert_with(|| LocationIntegrations::new(Integrations::new(self.store.clone())))
            .clone()
    }

    /// The integration registry of location `key` (`None`: the location-less one).
    pub fn integrations_for(&self, key: Option<&str>) -> Arc<Integrations> {
        match key {
            Some(key) => self.location_integrations(key).integrations.clone(),
            None => self.integrations.clone(),
        }
    }

    /// Points the Azure resource listing at another Resource Manager base URL (tests).
    pub fn set_azure_management(&self, url: impl Into<String>) {
        *self.azure_management.write() = url.into();
    }

    /// Drops the registry of a closed location (its attempts end with it, like upstream's scope).
    pub fn close_location(&self, key: &str) {
        self.by_location.lock().remove(key);
    }

    /// Runs plugin `provider`/`model` transforms in catalog builds of plugin locations.
    pub fn set_plugins(&self, plugins: Arc<crate::plugins::Plugins>) {
        *self.plugins.write() = Some(plugins);
    }

    /// The provider and web-search plugins' integration registrations (crate::integrations), replayed after the
    /// models.dev ones on every catalog build, like upstream's integration State rebuild.
    fn register_builtin_integrations(&self, state: &Arc<LocationIntegrations>, snapshots: &[catalog::models_dev::Snapshot], sections: &[Value]) {
        let integrations = &state.integrations;
        let initial = snapshots.iter().filter_map(|s| s.info.settings.clone().map(|settings| (s.info.id.clone(), settings)));
        let environment = crate::integrations::Environment {
            env: oc_ai::credentials::process_env(),
            home: crate::integrations::global_home(),
            settings: crate::integrations::settings(initial, sections),
            azure_cli: which::which("az").is_ok(),
            azure_resources: state.azure_resources.read().clone(),
        };
        crate::integrations::register(integrations, &environment);
        // azure.ts:404-408: list the `az login` account's resources in the background, once.
        if environment.azure_cli
            && !crate::integrations::azure_resource_configured(&environment)
            && !state.azure_started.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let (weak, resources) = (Arc::downgrade(integrations), state.azure_resources.clone());
            let (home, env) = (environment.home.clone(), environment.env.clone());
            let (management, user_agent) = (self.azure_management.read().clone(), self.user_agent.clone());
            tokio::spawn(async move {
                let tokens = oc_ai::credentials::azure::AzureCliTokens::new(Arc::new(oc_ai::credentials::azure::SystemAz));
                let http = reqwest::Client::new();
                match crate::integrations::list_azure_resources(&home, &env, &tokens, &http, &management, &user_agent).await {
                    Ok(Some(list)) => {
                        *resources.write() = Some(list.clone());
                        // `ctx.integration.reload()`: the method now offers the resources.
                        if let Some(integrations) = weak.upgrade() {
                            integrations.register_method("azure", crate::integrations::azure_cli_method(false, Some(&list)));
                        }
                    }
                    Ok(None) => {}
                    Err(cause) => tracing::debug!(%cause, "failed to list Azure resources"),
                }
            });
        }
        state.oauth.call_once(|| {
            // chatgpt.ts:218-237: the saved client of the `openai` integration's active connection.
            let chatgpt_client_id = oc_ai::credentials::flows::chatgpt::active_connection_client_id(Arc::downgrade(integrations));
            crate::integrations::register_oauth(
                integrations,
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
        self.catalog_at(entries, None).await
    }

    /// [`Models::catalog`] of the plugin location `key`: plugin `provider` and `model` transforms
    /// run after the internal provider plugins, before the configured-provider plugin.
    pub async fn catalog_at(&self, entries: &[Entry], key: Option<&str>) -> anyhow::Result<Catalog> {
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
        let state = match key {
            Some(k) => self.location_integrations(k),
            None => self.default_location.clone(),
        };
        let integrations = &state.integrations;
        let register_specs = |specs: &[catalog::IntegrationSpec]| {
            for spec in specs {
                if let Some(name) = &spec.name {
                    integrations.update(&spec.id, |r| r.name = name.clone());
                }
                for m in &spec.methods {
                    integrations.register_method(&spec.id, method(m));
                }
            }
        };
        register_specs(&dev_specs);
        self.register_builtin_integrations(&state, &snapshots, &sections);
        // Plugin `integration` transforms run between the internal plugins and the configured ones.
        let host = self.plugins.read().clone();
        if let (Some(host), Some(key)) = (host, key) {
            host.integrations(key, integrations).await;
        }
        let known: HashSet<String> = integrations.refs().into_iter().map(|r| r.id).collect();
        register_specs(&config.integrations(|id| known.contains(id)));
        let mut access = Access::new();
        for info in integrations.list().await.map_err(|e| anyhow::anyhow!("{e}"))? {
            access.set(info.id.clone(), info.connections.iter().map(|c| c.key()).collect());
        }
        let configured: HashSet<String> = integrations.refs().into_iter().map(|r| r.id).collect();
        let connected = std::env::var("OPENCODE_API_KEY").is_ok_and(|v| !v.is_empty())
            || access.connections("opencode").is_some_and(|c| !c.is_empty());
        // Upstream registers the OpenCode Zen provider plugin among the provider plugins, before the
        // configured-provider plugin (core/src/plugin/internal.ts `pre`/`post`).
        let host = self.plugins.read().clone();
        let build = |copilot: Option<catalog::copilot::State>| {
            let mut plugins: Vec<Box<dyn catalog::CatalogPlugin + Send>> = vec![
                Box::new(catalog::ModelsDevPlugin::new(&snapshots)),
                Box::new(catalog::DataPlugins::new(configured.clone())),
            ];
            // opencode.provider.github.copilot (github-copilot.ts:216-244) with the location's loaded state.
            if let Some(state) = copilot {
                plugins.push(Box::new(catalog::copilot::GithubCopilot { state }));
            }
            plugins.push(Box::new(catalog::plugins::OpencodeZen { connected }));
            plugins.push(Box::new(config.clone()));
            plugins
        };
        let (Some(plugins_host), Some(key)) = (host, key) else {
            let mut plugins = build(Some(catalog::copilot::State::default()));
            let post = plugins.len() - 1;
            return Ok(Catalog::build_staged(&mut plugins, post, &access, async |r| r, async |m, d, _| (m, d)).await);
        };
        let key = key.to_string();
        let copilot = plugins_host.copilot_enabled(&key);
        let mut first = true;
        loop {
            let mut plugins = build(copilot.then(|| plugins_host.copilot.state(&key)));
            let post = plugins.len() - 1;
            let (p1, k1, p2, k2) = (plugins_host.clone(), key.clone(), plugins_host.clone(), key.clone());
            let catalog = Catalog::build_staged(
                &mut plugins,
                post,
                &access,
                async move |records| p1.provider_records(&k1, records).await,
                async move |models, default, records| p2.model_records(&k2, models, default, records).await,
            )
            .await;
            if !copilot || !first {
                return Ok(catalog);
            }
            first = false;
            // The plugin loads when the location first needs it (upstream: on activation); the first
            // build waits for that load and is redone when it changed the state.
            let headers = catalog.provider(catalog::copilot::PROVIDER_ID).and_then(|p| p.headers.clone());
            if !plugins_host.copilot.register(&key, integrations, headers).await {
                return Ok(catalog);
            }
        }
    }

    /// The active connection's credential for `provider_id`, resolved through location `key`'s
    /// integration registry (its env methods and OAuth refresh implementations).
    pub async fn credential(&self, key: Option<&str>, catalog: &Catalog, provider_id: &str) -> Option<CredentialValue> {
        let integration = catalog
            .provider(provider_id)
            .and_then(|p| p.integration_id.clone())
            .unwrap_or_else(|| provider_id.to_string());
        let integrations = self.integrations_for(key);
        let connection = integrations.active(&integration).await.ok().flatten()?;
        integrations.resolve(&connection).await.ok().flatten()
    }

    /// Resolution dependencies for location `key` (upstream `ModelResolver` is a location node):
    /// `aisdk:` packages run through the AI SDK bridge in the plugin sidecar.
    pub fn resolve_options(&self, key: Option<&str>) -> catalog::resolve::ResolveOptions {
        let mut options = catalog::resolve::ResolveOptions::default();
        if let (Some(plugins), Some(key)) = (self.plugins.read().clone(), key) {
            options.aisdk = Some(Arc::new(crate::aisdk::LocationBridge::new(plugins, key)));
        }
        options
    }

    /// `ModelResolver.resolveModel`: [`Catalog::resolve_with`] plus the AI SDK package load.
    pub async fn resolve_model(
        &self,
        key: Option<&str>,
        catalog: &Catalog,
        provider_id: &str,
        model_id: &str,
        variant: Option<&str>,
        credential: Option<&CredentialValue>,
    ) -> Result<ResolvedModel, catalog::resolve::ResolveError> {
        let resolved = catalog.resolve_with(provider_id, model_id, variant, credential, &self.resolve_options(key))?;
        resolved.initialize().await?;
        Ok(resolved)
    }

    /// `SessionRunnerModel.resolve`: session model → config model → first available text model.
    pub async fn resolve_for_session(
        &self,
        key: Option<&str>,
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
        let credential = self.credential(key, catalog, &r.provider_id).await;
        // The requested variant stays in the resolved ref (`"default"` included, as a switched session
        // model carries it); `withVariant` treats `"default"` as no overlay.
        self.resolve_model(key, catalog, &r.provider_id, &r.id, r.variant.as_deref(), credential.as_ref())
            .await
            .map_err(|e| ModelError::Resolve(e.to_string()).into())
    }
}
