//! MCP configuration model (`packages/schema/src/mcp.ts`, `packages/schema/src/config/mcp.ts`),
//! the config-document merge done by `opencode.config.mcp` (`packages/core/src/config/plugin/mcp.ts`),
//! the built-in `opencode.mcp.codemode.defaults` plugin and the v1 → v2 server migration
//! (`packages/core/src/v1/config/migrate.ts:79-104`).
//!
//! Field order of every struct matches the upstream schema so serialized JSON keeps the same key
//! order.

use indexmap::IndexMap;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default `timeout.startup` (`client.ts:34`).
pub const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 30_000;
/// Default `timeout.catalog` (`client.ts:35`).
pub const DEFAULT_CATALOG_TIMEOUT_MS: u64 = 30_000;
/// Default `timeout.execution`: 12 hours (`client.ts:36`).
pub const DEFAULT_EXECUTION_TIMEOUT_MS: u64 = 12 * 60 * 60 * 1_000;

/// `Mcp.TimeoutConfig`: every field an optional positive int in milliseconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeoutConfig {
    /// Maximum time in milliseconds to establish and initialize the MCP server.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "positive")]
    pub startup: Option<u64>,
    /// Maximum time in milliseconds to wait for MCP discovery requests such as tools/list and prompts/list.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "positive")]
    pub catalog: Option<u64>,
    /// Maximum time in milliseconds to wait for MCP tool and prompt execution.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "positive")]
    pub execution: Option<u64>,
}

impl TimeoutConfig {
    pub fn startup_ms(timeout: Option<&TimeoutConfig>) -> u64 {
        timeout.and_then(|t| t.startup).unwrap_or(DEFAULT_STARTUP_TIMEOUT_MS)
    }
    pub fn catalog_ms(timeout: Option<&TimeoutConfig>) -> u64 {
        timeout.and_then(|t| t.catalog).unwrap_or(DEFAULT_CATALOG_TIMEOUT_MS)
    }
    pub fn execution_ms(timeout: Option<&TimeoutConfig>) -> u64 {
        timeout.and_then(|t| t.execution).unwrap_or(DEFAULT_EXECUTION_TIMEOUT_MS)
    }

    /// `Object.assign(self, other)` restricted to defined leaves.
    pub fn overlay(&mut self, other: &TimeoutConfig) {
        if other.startup.is_some() {
            self.startup = other.startup;
        }
        if other.catalog.is_some() {
            self.catalog = other.catalog;
        }
        if other.execution.is_some() {
            self.execution = other.execution;
        }
    }
}

fn positive<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    let value = Option::<u64>::deserialize(deserializer)?;
    match value {
        Some(0) => Err(de::Error::custom("Expected a positive integer")),
        other => Ok(other),
    }
}

/// `Mcp.Protocol`.
///
/// MCP protocol negotiation. "legacy" (default) opens with the initialize handshake and speaks
/// protocol revisions up to 2025-11-25. "auto" probes for the 2026-07-28 revision and falls back
/// to legacy when the server does not support it. "2026-07-28" requires that revision and fails
/// otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    #[serde(rename = "legacy")]
    Legacy,
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "2026-07-28")]
    V2026_07_28,
}

impl Protocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Auto => "auto",
            Self::V2026_07_28 => "2026-07-28",
        }
    }
}

/// `Mcp.LocalConfig` (without the `type` tag, which lives on [`ServerConfig`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalConfig {
    pub command: Vec<String>,
    /// Working directory for the MCP server process. Relative paths resolve from the workspace directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<IndexMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    /// Expose this server's tools through Code Mode. Defaults to true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codemode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<TimeoutConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
}

/// `Mcp.OAuthConfig` (snake_case keys).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "port")]
    pub callback_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    /// URL of the OAuth or OpenID Connect authorization server metadata document. Set when the MCP
    /// server does not publish protected resource metadata that names its authorization server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_server_metadata_url: Option<String>,
}

fn port<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u16>, D::Error> {
    let value = Option::<i64>::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(v) if (1..=65535).contains(&v) => Ok(Some(v as u16)),
        Some(_) => Err(de::Error::custom("Expected a value between 1 and 65535")),
    }
}

/// `oauth?: OAuthConfig | false`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthSetting {
    Config(OAuthConfig),
    /// `false`: disables OAuth integration registration.
    Disabled,
}

impl Serialize for OAuthSetting {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Config(config) => config.serialize(serializer),
            Self::Disabled => serializer.serialize_bool(false),
        }
    }
}

impl<'de> Deserialize<'de> for OAuthSetting {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::Bool(false) => Ok(Self::Disabled),
            Value::Object(_) => serde_json::from_value(value).map(Self::Config).map_err(de::Error::custom),
            _ => Err(de::Error::custom("Expected an OAuth config object or false")),
        }
    }
}

/// `Mcp.RemoteConfig` (without the `type` tag).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthSetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    /// Expose this server's tools through Code Mode. Defaults to true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codemode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<TimeoutConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
}

impl RemoteConfig {
    /// `config.oauth || undefined`.
    pub fn oauth_config(&self) -> Option<&OAuthConfig> {
        match &self.oauth {
            Some(OAuthSetting::Config(config)) => Some(config),
            _ => None,
        }
    }

    pub fn oauth_disabled(&self) -> bool {
        matches!(self.oauth, Some(OAuthSetting::Disabled))
    }
}

/// `Mcp.ServerConfig`: tagged union on `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerConfig {
    #[serde(rename = "local")]
    Local(LocalConfig),
    #[serde(rename = "remote")]
    Remote(RemoteConfig),
}

impl ServerConfig {
    pub fn disabled(&self) -> bool {
        match self {
            Self::Local(c) => c.disabled == Some(true),
            Self::Remote(c) => c.disabled == Some(true),
        }
    }

    pub fn codemode(&self) -> Option<bool> {
        match self {
            Self::Local(c) => c.codemode,
            Self::Remote(c) => c.codemode,
        }
    }

    pub fn timeout(&self) -> Option<&TimeoutConfig> {
        match self {
            Self::Local(c) => c.timeout.as_ref(),
            Self::Remote(c) => c.timeout.as_ref(),
        }
    }

    pub fn timeout_mut(&mut self) -> &mut Option<TimeoutConfig> {
        match self {
            Self::Local(c) => &mut c.timeout,
            Self::Remote(c) => &mut c.timeout,
        }
    }

    pub fn protocol(&self) -> Option<Protocol> {
        match self {
            Self::Local(c) => c.protocol,
            Self::Remote(c) => c.protocol,
        }
    }

    pub fn as_remote(&self) -> Option<&RemoteConfig> {
        match self {
            Self::Remote(c) => Some(c),
            _ => None,
        }
    }

    pub fn startup_timeout_ms(&self) -> u64 {
        TimeoutConfig::startup_ms(self.timeout())
    }
    pub fn catalog_timeout_ms(&self) -> u64 {
        TimeoutConfig::catalog_ms(self.timeout())
    }
    pub fn execution_timeout_ms(&self) -> u64 {
        TimeoutConfig::execution_ms(self.timeout())
    }
}

/// `Config.MCP`: the `mcp` section of one config document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<TimeoutConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub servers: Option<IndexMap<String, ServerConfig>>,
}

/// Folds the `mcp` sections of all config documents (low → high precedence), exactly as
/// `opencode.config.mcp` does:
/// - the global timeout is `Object.assign({}, ...doc.mcp.timeout)` in config order;
/// - servers merge by name, a later document replacing the whole entry;
/// - each server is stored as `{...server, timeout: {...globalTimeout, ...server.timeout}}`.
pub fn merge_documents<'a>(sections: impl IntoIterator<Item = &'a McpSection>) -> IndexMap<String, ServerConfig> {
    let mut timeout = TimeoutConfig::default();
    let mut servers: IndexMap<String, ServerConfig> = IndexMap::new();
    for section in sections {
        if let Some(t) = &section.timeout {
            timeout.overlay(t);
        }
        if let Some(map) = &section.servers {
            for (name, server) in map {
                servers.insert(name.clone(), server.clone());
            }
        }
    }
    servers
        .into_iter()
        .map(|(name, mut server)| {
            let mut merged = timeout.clone();
            if let Some(own) = server.timeout() {
                merged.overlay(own);
            }
            *server.timeout_mut() = Some(merged);
            (name, server)
        })
        .collect()
}

static EXECUTOR_URL: Lazy<Regex> = Lazy::new(|| Regex::new(r"^https://executor\.sh/[^/]+/mcp$").unwrap());
static POSTHOG_HOST: Lazy<Regex> = Lazy::new(|| Regex::new(r"^mcp(?:-eu|\.us|\.eu)?\.posthog\.com$").unwrap());
const POSTHOG_MODE_HEADER: &str = "x-posthog-mcp-mode";

/// The built-in `opencode.mcp.codemode.defaults` plugin
/// (`packages/core/src/plugin/mcp-codemode-defaults.ts`). Only servers with `codemode` unset are
/// touched.
pub fn apply_codemode_defaults(servers: &mut IndexMap<String, ServerConfig>) {
    for server in servers.values_mut() {
        if server.codemode().is_some() {
            continue;
        }
        match server {
            ServerConfig::Local(local) => {
                if local.command.first().map(String::as_str) == Some("executor")
                    && local.command.get(1).map(String::as_str) == Some("mcp")
                {
                    local.codemode = Some(false);
                }
            }
            ServerConfig::Remote(remote) => {
                let Ok(url) = url::Url::parse(&remote.url) else { continue };
                let endpoint = format!("{}{}", url.origin().ascii_serialization(), url.path().trim_end_matches('/'));
                if EXECUTOR_URL.is_match(&endpoint) {
                    remote.codemode = Some(false);
                }
                let host = url.host_str().unwrap_or("");
                let has_mode = url.query_pairs().any(|(k, _)| k == "mode");
                let has_header = remote
                    .headers
                    .as_ref()
                    .is_some_and(|h| h.keys().any(|k| k.to_lowercase() == POSTHOG_MODE_HEADER));
                if POSTHOG_HOST.is_match(host) && !has_mode && !has_header {
                    remote
                        .headers
                        .get_or_insert_with(IndexMap::new)
                        .insert(POSTHOG_MODE_HEADER.to_string(), "tools".to_string());
                }
            }
        }
    }
}

/// Migrates one v1 `mcp.<name>` entry (`{type, command|url, enabled?, timeout?: N, oauth?: camelCase}`)
/// to the v2 [`ServerConfig`] (`migrateMcp`). Returns `None` when the value is not a v1 server.
pub fn migrate_v1_server(value: &Value) -> Option<ServerConfig> {
    let obj = value.as_object()?;
    let disabled = obj.get("enabled").and_then(Value::as_bool).map(|e| !e);
    let timeout = obj.get("timeout").and_then(Value::as_u64).filter(|t| *t > 0).map(|t| TimeoutConfig {
        startup: None,
        catalog: Some(t),
        execution: Some(t),
    });
    let string_map = |key: &str| -> Option<IndexMap<String, String>> {
        obj.get(key).and_then(Value::as_object).map(|m| {
            m.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect()
        })
    };
    match obj.get("type").and_then(Value::as_str)? {
        "local" => {
            let command = obj
                .get("command")?
                .as_array()?
                .iter()
                .map(|v| v.as_str().map(String::from))
                .collect::<Option<Vec<_>>>()?;
            Some(ServerConfig::Local(LocalConfig {
                command,
                cwd: obj.get("cwd").and_then(Value::as_str).map(String::from),
                environment: string_map("environment"),
                disabled,
                codemode: None,
                timeout,
                protocol: None,
            }))
        }
        "remote" => {
            let url = obj.get("url")?.as_str()?.to_string();
            let oauth = match obj.get("oauth") {
                Some(Value::Bool(false)) => Some(OAuthSetting::Disabled),
                Some(Value::Object(o)) => {
                    let s = |k: &str| o.get(k).and_then(Value::as_str).map(String::from);
                    Some(OAuthSetting::Config(OAuthConfig {
                        client_id: s("clientId"),
                        client_secret: s("clientSecret"),
                        scope: s("scope"),
                        callback_port: o
                            .get("callbackPort")
                            .and_then(Value::as_i64)
                            .filter(|p| (1..=65535).contains(p))
                            .map(|p| p as u16),
                        redirect_uri: s("redirectUri"),
                        auth_server_metadata_url: None,
                    }))
                }
                _ => None,
            };
            Some(ServerConfig::Remote(RemoteConfig {
                url,
                headers: string_map("headers"),
                oauth,
                disabled,
                codemode: None,
                timeout,
                protocol: None,
            }))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_and_serializes_in_schema_order() {
        let value = json!({
            "type": "remote",
            "url": "https://example.com/mcp",
            "oauth": false,
            "headers": {"a": "b"},
            "timeout": {"execution": 5, "startup": 1}
        });
        let config: ServerConfig = serde_json::from_value(value).unwrap();
        let out = serde_json::to_string(&config).unwrap();
        assert_eq!(
            out,
            r#"{"type":"remote","url":"https://example.com/mcp","headers":{"a":"b"},"oauth":false,"timeout":{"startup":1,"execution":5}}"#
        );
        let local: ServerConfig =
            serde_json::from_value(json!({"type":"local","command":["x","y"],"protocol":"2026-07-28"})).unwrap();
        assert_eq!(serde_json::to_string(&local).unwrap(), r#"{"type":"local","command":["x","y"],"protocol":"2026-07-28"}"#);
        assert!(serde_json::from_value::<ServerConfig>(json!({"type":"remote","url":"x","oauth":true})).is_err());
        assert!(serde_json::from_value::<ServerConfig>(json!({"type":"local","command":[],"timeout":{"startup":0}})).is_err());
        assert!(
            serde_json::from_value::<ServerConfig>(json!({"type":"remote","url":"x","oauth":{"callback_port":70000}}))
                .is_err()
        );
    }

    #[test]
    fn merge_applies_global_timeouts_and_replaces_whole_entries() {
        let a: McpSection = serde_json::from_value(json!({
            "timeout": {"startup": 1000, "catalog": 2000},
            "servers": {
                "x": {"type": "local", "command": ["a"], "environment": {"K": "V"}},
                "y": {"type": "remote", "url": "https://y", "timeout": {"catalog": 5}}
            }
        }))
        .unwrap();
        let b: McpSection = serde_json::from_value(json!({
            "timeout": {"catalog": 3000},
            "servers": {"x": {"type": "local", "command": ["b"]}}
        }))
        .unwrap();
        let merged = merge_documents([&a, &b]);
        let keys: Vec<_> = merged.keys().cloned().collect();
        assert_eq!(keys, vec!["x", "y"]);
        assert_eq!(
            serde_json::to_value(&merged["x"]).unwrap(),
            json!({"type":"local","command":["b"],"timeout":{"startup":1000,"catalog":3000}})
        );
        assert_eq!(
            serde_json::to_value(&merged["y"]).unwrap(),
            json!({"type":"remote","url":"https://y","timeout":{"startup":1000,"catalog":5}})
        );
        assert_eq!(merged["y"].execution_timeout_ms(), DEFAULT_EXECUTION_TIMEOUT_MS);
    }

    #[test]
    fn codemode_defaults() {
        let mut servers: IndexMap<String, ServerConfig> = serde_json::from_value(json!({
            "exec": {"type": "local", "command": ["executor", "mcp", "--x"]},
            "execset": {"type": "local", "command": ["executor", "mcp"], "codemode": true},
            "remote": {"type": "remote", "url": "https://executor.sh/team/mcp/"},
            "ph": {"type": "remote", "url": "https://mcp-eu.posthog.com/mcp"},
            "ph2": {"type": "remote", "url": "https://mcp.posthog.com/mcp?mode=x"},
            "ph3": {"type": "remote", "url": "https://mcp.posthog.com/mcp", "headers": {"X-PostHog-MCP-Mode": "a"}}
        }))
        .unwrap();
        apply_codemode_defaults(&mut servers);
        assert_eq!(servers["exec"].codemode(), Some(false));
        assert_eq!(servers["execset"].codemode(), Some(true));
        assert_eq!(servers["remote"].codemode(), Some(false));
        assert_eq!(servers["ph"].codemode(), None);
        let headers = servers["ph"].as_remote().unwrap().headers.clone().unwrap();
        assert_eq!(headers.get("x-posthog-mcp-mode").map(String::as_str), Some("tools"));
        assert!(servers["ph2"].as_remote().unwrap().headers.is_none());
        assert_eq!(servers["ph3"].as_remote().unwrap().headers.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn migrates_v1() {
        let server = migrate_v1_server(&json!({
            "type": "remote", "url": "https://x", "enabled": false, "timeout": 9,
            "oauth": {"clientId": "id", "callbackPort": 1234, "redirectUri": "http://127.0.0.1:1234/cb"}
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&server).unwrap(),
            json!({"type":"remote","url":"https://x","oauth":{"client_id":"id","callback_port":1234,"redirect_uri":"http://127.0.0.1:1234/cb"},"disabled":true,"timeout":{"catalog":9,"execution":9}})
        );
        let local = migrate_v1_server(&json!({"type":"local","command":["a"],"enabled":true})).unwrap();
        assert_eq!(serde_json::to_value(&local).unwrap(), json!({"type":"local","command":["a"],"disabled":false}));
    }
}
