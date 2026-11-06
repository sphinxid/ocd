//! Well-known organization config (`packages/core/src/config.ts` `loadWellknownEntry`/`loadWellknown` and the
//! reload subscriptions): every registered well-known source contributes the configs its manifest resolves to,
//! as `ConfigIntegration.WellKnownDocument`s tagged with the source's integration (its origin). The resolution
//! uses the latest stored credential of that integration when it is a key; a failed resolution (or no usable
//! credential) keeps the last resolved configs.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use oc_ai::credentials::wellknown::WellKnown as Service;
use oc_ai::credentials::{CredentialStore, CredentialValue};
use oc_config::load::{WellKnown, WellKnownEntry};
use serde_json::Value;

/// How often loaded locations refresh the well-known manifests (`Effect.sleep("10 minutes")`).
pub const REFRESH: Duration = Duration::from_secs(10 * 60);

/// The config loader's view of the well-known service and the credential store.
pub struct ConfigWellKnown {
    pub service: Arc<Service>,
    pub store: Arc<dyn CredentialStore>,
}

#[async_trait::async_trait]
impl WellKnown for ConfigWellKnown {
    async fn entries(&self) -> anyhow::Result<Vec<WellKnownEntry>> {
        let entries = self.service.entries().await.map_err(|e| anyhow::anyhow!(e.message))?;
        Ok(entries.into_iter().map(|e| WellKnownEntry { origin: e.origin, integration_id: e.integration_id }).collect())
    }

    async fn variables(&self, entry: &WellKnownEntry) -> Option<HashMap<String, String>> {
        let manifest = self.service.snapshot().into_iter().find(|e| e.origin == entry.origin)?.manifest;
        let auth = manifest.auth?;
        let credential = self.store.list(&entry.integration_id).await.ok()?.pop()?;
        match credential.value {
            CredentialValue::Key { key, .. } => Some(HashMap::from([(auth.env, key)])),
            _ => None,
        }
    }

    async fn resolve(&self, entry: &WellKnownEntry, variables: &HashMap<String, String>) -> anyhow::Result<Vec<Value>> {
        let found = self
            .service
            .snapshot()
            .into_iter()
            .find(|e| e.origin == entry.origin)
            .ok_or_else(|| anyhow::anyhow!("unknown wellknown source {}", entry.origin))?;
        let configs = self.service.resolve(&found, variables).await.map_err(|e| anyhow::anyhow!(e.message))?;
        Ok(configs.into_iter().map(Value::Object).collect())
    }

    async fn cached(&self, origin: &str) -> Vec<Value> {
        self.service.cached(origin).await.into_iter().map(Value::Object).collect()
    }
}

impl crate::runner::Engine {
    /// The config service's well-known subscriptions: a source added, removed or changed (`wellknown.updated`),
    /// or a credential switched for a well-known integration, reloads the loaded locations; every 10 minutes the
    /// manifests refresh while there are sources, and an unchanged refresh still reloads (a changed one announces
    /// itself).
    pub fn watch_wellknown(self: &Arc<Self>, service: Arc<Service>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        let mut updated = service.subscribe();
        let mut events = self.app.bus.subscribe();
        let me = Arc::downgrade(self);
        handle.spawn(async move {
            let mut refresh = tokio::time::interval_at(tokio::time::Instant::now() + REFRESH, REFRESH);
            loop {
                let reload = tokio::select! {
                    changed = updated.recv() => match changed {
                        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                        Err(_) => return,
                    },
                    event = events.recv() => match event {
                        Ok(event) if event.kind == "credential.switched" => {
                            let id = event.data.get("integrationID").and_then(Value::as_str).unwrap_or_default().to_string();
                            service.entries().await.is_ok_and(|entries| entries.iter().any(|e| e.integration_id == id))
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                        _ => false,
                    },
                    _ = refresh.tick() => {
                        if service.snapshot().is_empty() {
                            false
                        } else {
                            match service.refresh().await {
                                Ok(changed) => !changed,
                                Err(error) => {
                                    tracing::warn!(error = %error.message, "failed to refresh wellknown manifests");
                                    true
                                }
                            }
                        }
                    }
                };
                if !reload {
                    continue;
                }
                let Some(engine) = me.upgrade() else { return };
                for directory in engine.loaded_locations() {
                    if let Err(error) = engine.reload_location(&directory).await {
                        tracing::error!(directory = %directory, error = %error, "failed to reload wellknown config");
                    }
                }
            }
        });
    }
}
