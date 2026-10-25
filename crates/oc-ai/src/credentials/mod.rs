//! Credentials, integrations, OAuth flows and well-known discovery (spec §6).
//!
//! * [`store`]: the [`CredentialStore`] trait (backed by SQLite in core), selection/ordering rules and an
//!   in-memory implementation.
//! * [`integration`]: integration/method/connection model, active-connection resolution with OAuth refresh,
//!   key/external connects, the generic OAuth attempt lifecycle and command-method runner.
//! * [`flows`]: the provider OAuth flows of §6.4.
//! * [`wellknown`]: `/.well-known/opencode` discovery (§6.5).
//! * [`azure`]: Azure CLI Entra ID token helper.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod azure;
pub mod callback;
pub mod flows;
pub mod form;
pub mod http;
pub mod integration;
pub mod kv;
pub mod page;
pub mod pkce;
pub mod store;
pub mod wellknown;

pub use integration::{
    AttemptMode, AttemptStatus, AttemptTime, AuthorizeResult, CommandAttemptStatus, ConnectionInfo,
    ConnectionStatus, IntegrationError, IntegrationInfo, Integrations, Method, OAuthCallback, OAuthMethod,
};
pub use kv::{KvStore, MemoryKv};
pub use store::{CreateCredential, CredentialEvent, CredentialStore, CredentialUpdate, MemoryCredentialStore};

/// `Credential.Value` (`packages/schema/src/credential.ts`), stored as JSON in the `credential` table.
/// Values of either shape decode, so stored rows and `auth export` files written before v2.1 are upgraded
/// ([`LegacyCredentialValue`]); encoding writes only the current shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", from = "LegacyCredentialValue")]
pub enum CredentialValue {
    #[serde(rename_all = "camelCase")]
    Oauth {
        #[serde(rename = "methodID")]
        method_id: String,
        refresh: String,
        access: String,
        /// Epoch ms.
        expires: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Map<String, Value>>,
    },
    Key {
        key: String,
        /// Setup-form answers and other provider settings.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Map<String, Value>>,
    },
    External {
        #[serde(rename = "methodID")]
        method_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Map<String, Value>>,
    },
}

/// `CredentialLegacy.Value` (`packages/schema/src/credential-legacy.ts`): before v2.1 key credentials kept
/// their setup-form answers in `configuration`; they now live in `metadata`. Only decoding sees this shape.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum LegacyCredentialValue {
    #[serde(rename_all = "camelCase")]
    Oauth {
        #[serde(rename = "methodID")]
        method_id: String,
        refresh: String,
        access: String,
        expires: u64,
        #[serde(default)]
        metadata: Option<serde_json::Map<String, Value>>,
    },
    Key {
        key: String,
        #[serde(default)]
        metadata: Option<serde_json::Map<String, Value>>,
        #[serde(default)]
        configuration: Option<serde_json::Map<String, Value>>,
    },
    External {
        #[serde(rename = "methodID")]
        method_id: String,
        #[serde(default)]
        metadata: Option<serde_json::Map<String, Value>>,
    },
}

impl From<LegacyCredentialValue> for CredentialValue {
    fn from(value: LegacyCredentialValue) -> Self {
        match value {
            LegacyCredentialValue::Oauth { method_id, refresh, access, expires, metadata } => {
                CredentialValue::Oauth { method_id, refresh, access, expires, metadata }
            }
            LegacyCredentialValue::Key { key, metadata, configuration } => {
                // Form answers overrode metadata when both reached provider settings, so they keep precedence.
                let metadata = match configuration {
                    None => metadata,
                    Some(answers) => {
                        let mut merged = metadata.unwrap_or_default();
                        merged.extend(answers);
                        Some(merged)
                    }
                };
                CredentialValue::Key { key, metadata }
            }
            LegacyCredentialValue::External { method_id, metadata } => CredentialValue::External { method_id, metadata },
        }
    }
}

/// One stored credential row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Credential {
    pub id: String,
    #[serde(rename = "integrationID")]
    pub integration_id: String,
    pub label: String,
    pub value: CredentialValue,
    pub active: Option<bool>,
}

/// JSON object used for credential metadata and form answers.
pub type Metadata = serde_json::Map<String, Value>;

/// Borrowed/owned view of an OAuth credential value (`Credential.OAuth`).
#[derive(Debug, Clone, PartialEq)]
pub struct OAuthValue {
    pub method_id: String,
    pub refresh: String,
    pub access: String,
    pub expires: u64,
    pub metadata: Option<Metadata>,
}

impl OAuthValue {
    pub fn into_value(self) -> CredentialValue {
        CredentialValue::Oauth {
            method_id: self.method_id,
            refresh: self.refresh,
            access: self.access,
            expires: self.expires,
            metadata: self.metadata,
        }
    }

    /// String metadata entry.
    pub fn meta_str(&self, key: &str) -> Option<&str> {
        self.metadata.as_ref().and_then(|m| m.get(key)).and_then(Value::as_str)
    }
}

impl From<OAuthValue> for CredentialValue {
    fn from(value: OAuthValue) -> Self {
        value.into_value()
    }
}

impl CredentialValue {
    /// `type` discriminator: `"oauth" | "key" | "external"`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Oauth { .. } => "oauth",
            Self::Key { .. } => "key",
            Self::External { .. } => "external",
        }
    }

    pub fn metadata(&self) -> Option<&Metadata> {
        match self {
            Self::Oauth { metadata, .. } | Self::Key { metadata, .. } | Self::External { metadata, .. } => {
                metadata.as_ref()
            }
        }
    }

    /// `methodID` of OAuth / external values.
    pub fn method_id(&self) -> Option<&str> {
        match self {
            Self::Oauth { method_id, .. } | Self::External { method_id, .. } => Some(method_id),
            Self::Key { .. } => None,
        }
    }

    /// The OAuth fields, when this is an OAuth value.
    pub fn as_oauth(&self) -> Option<OAuthValue> {
        match self {
            Self::Oauth { method_id, refresh, access, expires, metadata } => Some(OAuthValue {
                method_id: method_id.clone(),
                refresh: refresh.clone(),
                access: access.clone(),
                expires: *expires,
                metadata: metadata.clone(),
            }),
            _ => None,
        }
    }

    pub fn key(key: impl Into<String>) -> Self {
        Self::Key { key: key.into(), metadata: None }
    }
}

/// Current wall-clock time in epoch ms.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `App.useragent(app)`: `opencode/<channel>/<version>/<name>`.
pub fn user_agent(channel: &str, version: &str, name: &str) -> String {
    format!("opencode/{channel}/{version}/{name}")
}

/// Reads one environment variable. Injectable so tests never mutate the process environment.
pub type EnvSource = std::sync::Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// [`EnvSource`] over the process environment (empty values count as unset where upstream tests truthiness).
pub fn process_env() -> EnvSource {
    std::sync::Arc::new(|name: &str| std::env::var(name).ok())
}

/// [`EnvSource`] over a fixed map (tests).
pub fn map_env<I, K, V>(entries: I) -> EnvSource
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    let map: std::collections::HashMap<String, String> =
        entries.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
    std::sync::Arc::new(move |name: &str| map.get(name).cloned())
}

/// Epoch-ms clock. Injectable for attempt-lifecycle tests.
pub type Clock = std::sync::Arc<dyn Fn() -> u64 + Send + Sync>;

pub fn system_clock() -> Clock {
    std::sync::Arc::new(now_ms)
}
