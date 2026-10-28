//! The built-in Azure provider plugin's Entra ID hooks (`core/src/plugin/provider/azure.ts`).
//!
//! The `azure-cli` sign-in stores no secret: the model resolves with the placeholder key `azure-cli`, and the
//! plugin's `http.request` and internal `experimental.ws.handshake` hooks replace it on every request of the
//! `azure` provider. When the location's active `azure` connection resolves to the Azure CLI method, they mint a
//! bearer token for the target URL's scope (`https://ai.azure.com/.default` for Foundry hosts outside `/models`,
//! else `https://cognitiveservices.azure.com/.default`; cached per scope until 5 minutes before expiry), remove
//! `api-key` and `x-api-key`, and set `authorization: Bearer <token>` and `user-agent: <App.useragent>`. Other
//! connections (API keys) pass through unchanged. A token failure fails the request (`Effect.orDie`).

use std::collections::HashMap;
use std::sync::Arc;

use oc_ai::credentials::azure::{AzCli, AzureCliTokens, METHOD_ID, SystemAz};
use oc_ai::credentials::{CredentialValue, Integrations};
use parking_lot::Mutex;

/// The built-in plugin's id (its hooks run only where it is enabled).
pub const PLUGIN_ID: &str = "opencode.provider.azure";
/// The provider (and integration) the hooks apply to.
pub const PROVIDER_ID: &str = "azure";

/// The plugin's token cache and each location's integration registry.
pub struct Azure {
    tokens: parking_lot::RwLock<Arc<AzureCliTokens>>,
    integrations: Mutex<HashMap<String, Arc<Integrations>>>,
}

impl Default for Azure {
    fn default() -> Self {
        Self::new(Arc::new(SystemAz))
    }
}

impl Azure {
    pub fn new(cli: Arc<dyn AzCli>) -> Self {
        Self { tokens: parking_lot::RwLock::new(Arc::new(AzureCliTokens::new(cli))), integrations: Mutex::new(HashMap::new()) }
    }

    /// Replaces the Azure CLI runner (and drops the cached tokens).
    pub fn set_cli(&self, cli: Arc<dyn AzCli>) {
        *self.tokens.write() = Arc::new(AzureCliTokens::new(cli));
    }

    /// Location `key`'s integration registry (where its `azure` connection is looked up).
    pub fn register(&self, key: &str, integrations: Arc<Integrations>) {
        self.integrations.lock().insert(key.to_string(), integrations);
    }

    pub fn remove(&self, key: &str) {
        self.integrations.lock().remove(key);
    }

    /// `bearer(url)`: `Bearer <token>` when location `key`'s active `azure` connection is the Azure CLI method,
    /// `None` otherwise. The error is the Azure CLI failure.
    pub async fn bearer(&self, key: &str, url: &str) -> Result<Option<String>, String> {
        let Some(integrations) = self.integrations.lock().get(key).cloned() else { return Ok(None) };
        let Some(connection) = integrations.active(PROVIDER_ID).await.ok().flatten() else { return Ok(None) };
        let credential = integrations.resolve(&connection).await.ok().flatten();
        if !matches!(&credential, Some(CredentialValue::External { method_id, .. }) if method_id == METHOD_ID) {
            return Ok(None);
        }
        let tokens = self.tokens.read().clone();
        tokens.bearer(url).await.map(Some)
    }
}

/// The hooks' header edit (on lower-case names): drop the key headers, set the bearer and the user agent.
pub fn apply(headers: &mut indexmap::IndexMap<String, String>, authorization: &str, user_agent: &str) {
    headers.shift_remove("api-key");
    headers.shift_remove("x-api-key");
    headers.insert("authorization".into(), authorization.to_string());
    headers.insert("user-agent".into(), user_agent.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_edit() {
        let mut headers: indexmap::IndexMap<String, String> =
            [("api-key", "azure-cli"), ("user-agent", "x"), ("openai-beta", "b")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        apply(&mut headers, "Bearer t", "opencode/latest/1/cli");
        assert_eq!(
            headers.into_iter().collect::<Vec<_>>(),
            [("user-agent", "opencode/latest/1/cli"), ("openai-beta", "b"), ("authorization", "Bearer t")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>()
        );
    }
}
