//! Wire protocol between the Rust server and the Bun sidecar.
//!
//! Newline-delimited JSON-RPC 2.0 on the sidecar's stdin (Rust → sidecar) and stdout
//! (sidecar → Rust). Both sides send requests; ids are scoped per direction. A message with
//! `method` and `id` is a request, `method` without `id` a notification, `id` without `method` a
//! response. Errors are `{message, name?, type?, data?, metadata?}`. `$/cancel {id}` aborts an
//! in-flight request in either direction. The sidecar logs to stderr as JSON lines
//! `{"$log": "<level>", "message": ..., ...fields}`; any other stderr line is plugin output.
//!
//! See README.md for the full method list.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Protocol version negotiated by `initialize`.
pub const PROTOCOL_VERSION: u64 = 1;

/// A server location (`Location.Ref`): directory plus optional workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LocationRef {
    pub directory: String,
    #[serde(rename = "workspaceID", default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

impl LocationRef {
    pub fn new(directory: impl Into<String>) -> Self {
        Self { directory: directory.into(), workspace_id: None }
    }

    /// Key used on the wire to name the location.
    pub fn key(&self) -> String {
        match &self.workspace_id {
            Some(workspace) => format!("{}\u{0}{workspace}", self.directory),
            None => self.directory.clone(),
        }
    }
}

/// A configured plugin operation (`ConfigPluginSource.Operation`); serializes like oc-config's
/// `PluginOperation` (`{"type":"add","target","options"}` / `{"type":"remove","target"}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Operation {
    Add {
        target: String,
        #[serde(default)]
        options: Map<String, Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mtime: Option<u64>,
    },
    Remove { target: String },
}

impl Operation {
    pub fn add(target: impl Into<String>) -> Self {
        Operation::Add { target: target.into(), options: Map::new(), mtime: None }
    }

    pub fn target(&self) -> &str {
        match self {
            Operation::Add { target, .. } | Operation::Remove { target } => target,
        }
    }
}

/// Error object carried in responses.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct WireError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl WireError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), ..Default::default() }
    }
}

/// A runtime hook registration (`<domain>.<name>`, optional provider scope).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookSpec {
    pub key: String,
    #[serde(rename = "providerID", default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
}

/// Rust-side description of an installed npm package handed to `module.load`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Installed {
    pub directory: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

/// `module.load` answer.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum ModuleLoad {
    Loaded { id: String, revision: String, #[serde(default)] features: Value, #[serde(default)] source: Value },
    Failed { error: String, #[serde(default)] r#ref: Option<String> },
}

/// `location.activate` answer.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct Activation {
    #[serde(default)]
    pub inventory: Vec<Value>,
    #[serde(default)]
    pub hooks: Vec<HookSpec>,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub rpc: Vec<String>,
}

/// One JSON-RPC message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

impl Message {
    pub fn request(id: u64, method: &str, params: Value) -> Self {
        Self { jsonrpc: Some("2.0".into()), id: Some(id.into()), method: Some(method.into()), params: Some(params), result: None, error: None }
    }

    pub fn notification(method: &str, params: Value) -> Self {
        Self { jsonrpc: Some("2.0".into()), id: None, method: Some(method.into()), params: Some(params), result: None, error: None }
    }

    pub fn response(id: Value, result: Result<Value, WireError>) -> Self {
        let (result, error) = match result {
            Ok(value) => (Some(value), None),
            Err(error) => (None, Some(error)),
        };
        Self { jsonrpc: Some("2.0".into()), id: Some(id), method: None, params: None, result: if error.is_none() { Some(result.unwrap_or(Value::Null)) } else { None }, error }
    }
}

/// `{"$fn": handle}` marker for functions crossing the boundary.
pub const FN: &str = "$fn";

/// Returns the handle if `value` is a function marker.
pub fn function_handle(value: &Value) -> Option<&str> {
    value.as_object().filter(|o| o.len() == 1).and_then(|o| o.get(FN)).and_then(Value::as_str)
}

/// A function marker value.
pub fn function(handle: &str) -> Value {
    serde_json::json!({ FN: handle })
}

/// Whether a handle belongs to the sidecar (`js:` prefix) rather than Rust.
pub fn is_plugin_handle(handle: &str) -> bool {
    handle.starts_with("js:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn operation_round_trip() {
        let op: Operation = serde_json::from_value(json!({"type": "add", "target": "x", "options": {"a": 1}})).unwrap();
        assert_eq!(op.target(), "x");
        assert_eq!(serde_json::to_value(&op).unwrap(), json!({"type": "add", "target": "x", "options": {"a": 1}}));
        let op: Operation = serde_json::from_value(json!({"type": "remove", "target": "y"})).unwrap();
        assert_eq!(op, Operation::Remove { target: "y".into() });
    }

    #[test]
    fn location_key() {
        assert_eq!(LocationRef::new("/a").key(), "/a");
        let r = LocationRef { directory: "/a".into(), workspace_id: Some("w".into()) };
        assert_eq!(r.key(), "/a\u{0}w");
        assert_eq!(serde_json::to_value(&r).unwrap(), json!({"directory": "/a", "workspaceID": "w"}));
    }

    #[test]
    fn function_markers() {
        assert_eq!(function_handle(&json!({"$fn": "js:1"})), Some("js:1"));
        assert_eq!(function_handle(&json!({"$fn": "js:1", "x": 1})), None);
        assert!(is_plugin_handle("js:1.2"));
        assert!(!is_plugin_handle("rs:tool.read"));
    }

    #[test]
    fn module_load_variants() {
        let ok: ModuleLoad = serde_json::from_value(json!({"id": "a", "revision": "r", "features": {}, "source": {}})).unwrap();
        assert!(matches!(ok, ModuleLoad::Loaded { .. }));
        let err: ModuleLoad = serde_json::from_value(json!({"error": "boom", "ref": "err_1"})).unwrap();
        assert_eq!(err, ModuleLoad::Failed { error: "boom".into(), r#ref: Some("err_1".into()) });
    }

    #[test]
    fn response_shape() {
        let m = Message::response(json!(1), Ok(Value::Null));
        assert_eq!(serde_json::to_value(&m).unwrap(), json!({"jsonrpc": "2.0", "id": 1, "result": null}));
        let m = Message::response(json!(2), Err(WireError::new("x")));
        assert_eq!(serde_json::to_value(&m).unwrap(), json!({"jsonrpc": "2.0", "id": 2, "error": {"message": "x"}}));
    }
}
