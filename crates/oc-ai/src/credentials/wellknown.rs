//! Well-known discovery (`packages/core/src/wellknown.ts`, `wellknown/plugin.ts`, spec §6.5).
//!
//! `GET <origin>/.well-known/opencode` (Accept JSON, 15 s) →
//! `{auth?: {command: string[], env: string}, config?: object|null, remote_config?: {url, headers?}}`.

use super::integration::{Integrations, Method};
use super::kv::KvStore;
use super::EnvSource;
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
pub const SOURCES_KEY: &str = "wellknown:sources";

pub fn manifest_key(origin: &str) -> String {
    format!("wellknown:manifest:{origin}")
}

pub fn config_key(origin: &str) -> String {
    format!("wellknown:config:{origin}")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Auth {
    pub command: Vec<String>,
    pub env: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
}

/// `WellKnown.Manifest`. `config` is `Some(None)` for an explicit `null`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "nullable")]
    pub config: Option<Option<Map<String, Value>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_config: Option<RemoteConfig>,
}

fn nullable<'de, D>(deserializer: D) -> Result<Option<Option<Map<String, Value>>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Map<String, Value>>::deserialize(deserializer).map(Some)
}

/// A registered source.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub origin: String,
    pub integration_id: String,
    pub manifest: Manifest,
}

/// One resolved configuration object.
pub type Config = Map<String, Value>;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct WellKnownError {
    pub message: String,
    pub cause: Option<String>,
}

impl WellKnownError {
    fn new(message: impl Into<String>, cause: Option<String>) -> Self {
        Self { message: message.into(), cause }
    }
}

/// `origin.replace(/\/+$/, "")`.
pub fn trim_origin(origin: &str) -> String {
    origin.trim_end_matches('/').to_string()
}

pub fn manifest_url(origin: &str) -> String {
    format!("{}/.well-known/opencode", trim_origin(origin))
}

/// `{env:NAME}` substitution from `variables`, then the process env (via `env`), else empty.
pub fn substitute(value: &str, variables: &HashMap<String, String>, env: &EnvSource) -> String {
    static PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r"\{env:([^}]+)\}").expect("valid regex"));
    PATTERN
        .replace_all(value, |caps: &regex::Captures<'_>| {
            let name = &caps[1];
            variables.get(name).cloned().or_else(|| env(name)).unwrap_or_default()
        })
        .to_string()
}

/// `inspect(origin)`: fetches and decodes the manifest.
pub async fn inspect(http: &reqwest::Client, origin: &str) -> Result<Manifest, WellKnownError> {
    let url = manifest_url(origin);
    let fail = |cause: String| WellKnownError::new(format!("Failed to load wellknown manifest from {url}"), Some(cause));
    let result = async {
        let mut request = http.get(&url).header("Accept", "application/json").timeout(REQUEST_TIMEOUT);
        for (k, v) in oc_util::trace::headers() {
            request = request.header(k, v);
        }
        let response = request
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("non 2xx status code ({})", response.status().as_u16()));
        }
        let value: Value = response.json().await.map_err(|e| e.to_string())?;
        serde_json::from_value::<Manifest>(value).map_err(|e| e.to_string())
    }
    .await;
    result.map_err(fail)
}

/// `resolveEntry`: inline `config` plus `remote_config` (with `{env:NAME}` substitution in url and headers).
pub async fn resolve_entry(
    http: &reqwest::Client,
    entry: &Entry,
    variables: &HashMap<String, String>,
    env: &EnvSource,
) -> Result<Vec<Config>, WellKnownError> {
    let mut configs: Vec<Config> = entry.manifest.config.clone().flatten().into_iter().collect();
    let Some(remote) = &entry.manifest.remote_config else { return Ok(configs) };
    let url = substitute(&remote.url, variables, env);
    let mut request = http.get(&url).header("Accept", "application/json").timeout(REQUEST_TIMEOUT);
    for (key, value) in remote.headers.iter().flatten() {
        request = request.header(key.as_str(), substitute(value, variables, env));
    }
    let fail =
        |cause: String| WellKnownError::new(format!("Failed to load wellknown remote config from {url}"), Some(cause));
    let remote: Map<String, Value> = async {
        let mut request = request;
        for (k, v) in oc_util::trace::headers() {
            request = request.header(k, v);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("non 2xx status code ({})", response.status().as_u16()));
        }
        let value: Value = response.json().await.map_err(|e| e.to_string())?;
        match value {
            Value::Object(map) => Ok(map),
            _ => Err("expected an object".to_string()),
        }
    }
    .await
    .map_err(fail)?;
    match remote.get("config") {
        Some(Value::Object(config)) => configs.push(config.clone()),
        _ => configs.push(remote),
    }
    Ok(configs)
}

/// Integration registration for a source: id = origin, name = hostname, method `login` (command).
pub fn integration_method(entry: &Entry) -> Option<(String, String, Method)> {
    let auth = entry.manifest.auth.as_ref()?;
    let name = url::Url::parse(&entry.origin).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default();
    Some((
        entry.integration_id.clone(),
        name,
        Method::Command { id: "login".into(), label: "Log in".into(), command: auth.command.clone() },
    ))
}

/// Registers every source with `auth` on the integration registry (`WellKnownPlugin`).
pub fn register(integrations: &Integrations, entries: &[Entry]) {
    for entry in entries {
        if let Some((id, name, method)) = integration_method(entry) {
            integrations.update(&id, |r| r.name = name);
            integrations.register_method(&id, method);
        }
    }
}

/// `WellKnown.Service`.
pub struct WellKnown {
    http: reqwest::Client,
    kv: Arc<dyn KvStore>,
    env: EnvSource,
    cache: Mutex<IndexMap<String, Entry>>,
    lock: tokio::sync::Mutex<()>,
    events: broadcast::Sender<()>,
}

fn kv_error(error: anyhow::Error) -> WellKnownError {
    WellKnownError::new(error.to_string(), None)
}

impl WellKnown {
    pub fn new(http: reqwest::Client, kv: Arc<dyn KvStore>, env: EnvSource) -> Self {
        Self {
            http,
            kv,
            env,
            cache: Mutex::new(IndexMap::new()),
            lock: tokio::sync::Mutex::new(()),
            events: broadcast::channel(16).0,
        }
    }

    /// `wellknown.updated` notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.events.subscribe()
    }

    async fn sources(&self) -> Result<Vec<String>, WellKnownError> {
        let value = self.kv.get(SOURCES_KEY).await.map_err(kv_error)?;
        Ok(match value {
            Some(Value::Array(items)) if items.iter().all(Value::is_string) => {
                items.into_iter().filter_map(|v| v.as_str().map(String::from)).collect()
            }
            _ => Vec::new(),
        })
    }

    /// Fetches the manifest (caching it); an unreachable source falls back to its cached manifest.
    async fn load_entry(&self, origin: &str) -> Result<Entry, WellKnownError> {
        let manifest = match inspect(&self.http, origin).await {
            Ok(manifest) => {
                let value = serde_json::to_value(&manifest).unwrap_or(Value::Null);
                self.kv.set(&manifest_key(origin), value).await.map_err(kv_error)?;
                manifest
            }
            Err(error) => {
                let cached = self.kv.get(&manifest_key(origin)).await.map_err(kv_error)?;
                match cached.and_then(|v| serde_json::from_value::<Manifest>(v).ok()) {
                    Some(manifest) => {
                        tracing::warn!(origin, error = %error, "failed to load wellknown manifest");
                        manifest
                    }
                    None => return Err(error),
                }
            }
        };
        Ok(Entry { origin: origin.to_string(), integration_id: origin.to_string(), manifest })
    }

    /// `entries()`: registered sources, loading the ones not cached yet.
    pub async fn entries(&self) -> Result<Vec<Entry>, WellKnownError> {
        let origins = self.sources().await?;
        let current = self.cache.lock().clone();
        let mut entries = Vec::new();
        for origin in origins {
            match current.get(&origin) {
                Some(entry) => entries.push(entry.clone()),
                None => entries.push(self.load_entry(&origin).await?),
            }
        }
        *self.cache.lock() = entries.iter().map(|e| (e.origin.clone(), e.clone())).collect();
        Ok(entries)
    }

    pub fn snapshot(&self) -> Vec<Entry> {
        self.cache.lock().values().cloned().collect()
    }

    /// Reloads every source; returns whether anything changed.
    pub async fn refresh(&self) -> Result<bool, WellKnownError> {
        let _guard = self.lock.lock().await;
        let origins = self.sources().await?;
        if origins.is_empty() {
            return Ok(false);
        }
        let mut next = IndexMap::new();
        for origin in origins {
            let entry = self.load_entry(&origin).await?;
            next.insert(entry.origin.clone(), entry);
        }
        let changed = *self.cache.lock() != next;
        if !changed {
            return Ok(false);
        }
        *self.cache.lock() = next;
        let _ = self.events.send(());
        Ok(true)
    }

    /// Adds a source; requires `auth` (`"No authentication method found at <origin>"`).
    pub async fn add(&self, value: &str) -> Result<Entry, WellKnownError> {
        let _guard = self.lock.lock().await;
        let origin = trim_origin(value);
        let entry = self.load_entry(&origin).await?;
        if entry.manifest.auth.is_none() {
            return Err(WellKnownError::new(format!("No authentication method found at {origin}"), None));
        }
        let mut origins = self.sources().await?;
        if !origins.contains(&origin) {
            origins.push(origin.clone());
        }
        self.kv.set(SOURCES_KEY, json!(origins)).await.map_err(kv_error)?;
        self.cache.lock().insert(origin, entry.clone());
        let _ = self.events.send(());
        Ok(entry)
    }

    pub async fn remove(&self, value: &str) -> Result<(), WellKnownError> {
        let _guard = self.lock.lock().await;
        let origin = trim_origin(value);
        let origins: Vec<String> = self.sources().await?.into_iter().filter(|o| *o != origin).collect();
        self.kv.set(SOURCES_KEY, json!(origins)).await.map_err(kv_error)?;
        self.kv.remove(&manifest_key(&origin)).await.map_err(kv_error)?;
        self.kv.remove(&config_key(&origin)).await.map_err(kv_error)?;
        self.cache.lock().shift_remove(&origin);
        let _ = self.events.send(());
        Ok(())
    }

    /// Resolves and caches the configuration for a registered source.
    pub async fn resolve(
        &self,
        entry: &Entry,
        variables: &HashMap<String, String>,
    ) -> Result<Vec<Config>, WellKnownError> {
        let configs = resolve_entry(&self.http, entry, variables, &self.env).await?;
        self.kv.set(&config_key(&entry.origin), json!(configs)).await.map_err(kv_error)?;
        Ok(configs)
    }

    /// Last configuration resolved for an origin.
    pub async fn cached(&self, origin: &str) -> Vec<Config> {
        match self.kv.get(&config_key(origin)).await.ok().flatten() {
            Some(Value::Array(items)) if items.iter().all(Value::is_object) => {
                items.into_iter().filter_map(|v| v.as_object().cloned()).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Variables for [`WellKnown::resolve`]: `{[auth.env]: <stored key>}`.
    pub fn variables(entry: &Entry, key: Option<&str>) -> HashMap<String, String> {
        let mut out = HashMap::new();
        if let (Some(auth), Some(key)) = (&entry.manifest.auth, key) {
            out.insert(auth.env.clone(), key.to_string());
        }
        out
    }
}
