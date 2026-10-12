//! The interface the core implements so plugins can call back into ocd (`ctx.*` API groups,
//! storage, native function handles). The crate never depends on oc-core.
//!
//! # Methods passed to [`PluginApi::call`]
//!
//! HTTP-equivalent methods: `method` is the upstream endpoint identifier and `input` is the merged
//! object of the endpoint's path params, query and JSON body (exactly what the HTTP handler
//! receives after decoding). The answer must be what the endpoint would send; when the endpoint's
//! success schema is a lone `{ data }` wrapper, answer the inner `data` value only. `NoContent`
//! endpoints may answer `null`.
//!
//! | `ctx` member | method | HTTP endpoint |
//! |---|---|---|
//! | `agent.get` / `agent.list` | `agent.get`, `agent.list` | `GET /api/agent/:agentID`, `GET /api/agent` |
//! | `command.list` | `command.list` | `GET /api/command` |
//! | `experimental.terminal.read` | `persistentPty.read` | `GET /api/experimental/session/:sessionID/terminal/read` |
//! | `generate.text` | `generate.text` | `POST /api/experimental/generate` |
//! | `integration.list/get` | `integration.list`, `integration.get` | `GET /api/integration[/:integrationID]` |
//! | `integration.connect.key/external` | `integration.connect.key`, `integration.connect.external` | `POST /api/integration/:id/connect/{key,external}` |
//! | `integration.oauth.*` | `integration.oauth.{connect,status,complete,cancel}` | `/api/integration/:id/connect/oauth[...]` |
//! | `integration.command.*` | `integration.command.{connect,status,cancel}` | `/api/integration/:id/connect/command[...]` |
//! | `mcp.list` | `mcp.list` | `GET /api/mcp` |
//! | `model.list` / `model.default` | `model.list`, `model.default` | `GET /api/model`, `GET /api/model/default` |
//! | `permission.list/get/reply` | `session.permission.{list,get,reply}` | `/api/session/:sessionID/permission[...]` |
//! | `plugin.list` | `plugin.list` | `GET /api/plugin` |
//! | `provider.list/get` | `provider.list`, `provider.get` | `GET /api/provider[/:providerID]` |
//! | `reference.list` | `reference.list` | `GET /api/reference` |
//! | `session.*` | `session.{create,get,remove,switchAgent,switchModel,prompt,generate,command,compact,synthetic,interrupt,update,move,wait,context}` | `/api/session/...` |
//! | `skill.list` | `skill.list` | `GET /api/skill` |
//! | `vcs.get/base/branch.list/status/diff` | `vcs.{get,base,branch.list,status,diff}` | `/api/vcs/...` |
//! | `websearch.providers/query` | `websearch.providers`, `websearch.query` | `/api/websearch[...]` |
//! | `worktree.list/create/refresh/remove` | `worktree.{list,create,refresh,remove}` | `/api/worktree[...]` |
//!
//! Host-service methods (no HTTP endpoint):
//!
//! | method | input | answer |
//! |---|---|---|
//! | `<domain>.reload` (`agent`, `command`, `integration`, `mcp`, `model`, `provider`, `reference`, `skill`, `tool`, `vcs`, `websearch`, `worktree`) | `{}` | `null`; invalidate the domain and rebuild it (which calls [`crate::PluginHost::transform`]) |
//! | `tool.list` | `{}` | the current tool domain value (`{tools, namespaces}`, see README "Transform values") |
//! | `integration.connection.active` | `{integrationID}` | `ConnectionInfo` or `null` |
//! | `integration.connection.resolve` | `{connection}` | `Credential.Value` or `null` |
//! | `integration.connection.status` | `{integrationID, connection, status?}` | `null` |
//! | `event.publish` | `{type, data}` | `null`; publish an ephemeral bus event of that type with id/created and the location attached |
//! | `rpc.call` | `{rpcID, method, input}` | output, or an error whose `name` is the RPC failure `type` (e.g. `rpc.unavailable`) — only for RPCs not owned by the sidecar |
//!
//! `session.create` without `parentID` and `location` must default `location` to the calling
//! location (`packages/core/src/plugin/host.ts:536-551`).
//!
//! Storage (`ctx.storage`) arrives through the `storage_*` methods with keys already namespaced
//! (`plugin:<utf16 hex>:<key>`, see [`crate::storage::namespace`]): store them in the same KV table
//! upstream uses.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::protocol::LocationRef;

/// An error returned to the plugin. `name` becomes the JS error's `name`; for `rpc.call` it is the
/// RPC failure type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct ApiError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ApiError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), name: None, data: None }
    }

    pub fn named(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self { message: message.into(), name: Some(name.into()), data: None }
    }

    pub fn unsupported(method: &str) -> Self {
        Self::new(format!("Plugin API method is not supported by ocd: {method}"))
    }
}

/// One API call from a plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiRequest {
    pub location: LocationRef,
    /// Calling plugin id (`None` for host-internal calls such as `event.publish` from an RPC).
    pub plugin: Option<String>,
    pub method: String,
    pub input: Value,
}

/// A page of storage entries (`KV.scan`), keys fully namespaced.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct StorageScan {
    pub entries: Vec<StorageEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageEntry {
    pub key: String,
    pub value: Value,
}

/// Implemented by the core. All methods may be called concurrently.
#[async_trait]
pub trait PluginApi: Send + Sync + 'static {
    /// A `ctx` API call; see the module docs for the method list.
    async fn call(&self, request: ApiRequest) -> Result<Value, ApiError>;

    /// Calls a Rust-owned function handle (`{"$fn": "<handle>"}` the core put into a transform
    /// value, e.g. a native tool executor wrapped by a plugin). `args` are JSON; JS functions in
    /// arguments are dropped.
    async fn invoke(&self, location: &LocationRef, handle: &str, args: Vec<Value>) -> Result<Value, ApiError> {
        let _ = (location, args);
        Err(ApiError::new(format!("Unknown function handle: {handle}")))
    }

    async fn storage_get(&self, key: &str) -> Result<Option<Value>, ApiError>;
    async fn storage_set(&self, key: &str, value: Value) -> Result<(), ApiError>;
    async fn storage_remove(&self, key: &str) -> Result<(), ApiError>;
    /// `prefix`/`after` are fully namespaced; entries must be in key order.
    async fn storage_scan(&self, prefix: &str, after: Option<&str>, limit: Option<u64>) -> Result<StorageScan, ApiError>;
}
