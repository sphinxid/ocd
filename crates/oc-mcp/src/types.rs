//! Public data shapes: server status (`packages/schema/src/mcp.ts:69-97`), resources, the
//! tool/prompt shapes `Mcp.Service` returns, tool-call results (`client.ts:399-418`) and events
//! (`packages/schema/src/mcp-event.ts`, `index.ts:24`).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// `Mcp.Status` — tagged union on `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum Status {
    #[serde(rename = "connected")]
    Connected,
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "disabled")]
    Disabled,
    #[serde(rename = "failed")]
    Failed { error: String },
    #[serde(rename = "needs_auth")]
    NeedsAuth { error: String },
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Pending => "pending",
            Self::Disabled => "disabled",
            Self::Failed { .. } => "failed",
            Self::NeedsAuth { .. } => "needs_auth",
        }
    }

    pub fn failed(error: impl Into<String>) -> Self {
        Self::Failed { error: error.into() }
    }

    pub fn needs_auth(error: impl Into<String>) -> Self {
        Self::NeedsAuth { error: error.into() }
    }
}

/// `Mcp.Server` (`ServerInfo`): `{name, status, integrationID?}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub status: Status,
    #[serde(rename = "integrationID", default, skip_serializing_if = "Option::is_none")]
    pub integration_id: Option<String>,
}

/// `Mcp.Resource`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resource {
    pub server: String,
    pub name: String,
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// `Mcp.ResourceTemplate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceTemplate {
    pub server: String,
    pub name: String,
    #[serde(rename = "uriTemplate")]
    pub uri_template: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// `Mcp.ResourceCatalog`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceCatalog {
    pub resources: Vec<Resource>,
    pub templates: Vec<ResourceTemplate>,
}

/// `Mcp.ResourceContentPart` — tagged union on `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResourceContentPart {
    #[serde(rename = "text")]
    Text {
        uri: String,
        text: String,
        #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
    #[serde(rename = "blob")]
    Blob {
        uri: String,
        blob: String,
        #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
}

/// `Mcp.ResourceContent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceContent {
    pub server: String,
    pub uri: String,
    pub contents: Vec<ResourceContentPart>,
}

/// An MCP tool definition tagged with the server that owns it (`Mcp.Tool`). `raw` is the
/// definition exactly as the server sent it.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub server: String,
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Option<Value>,
    pub output_schema: Option<Value>,
    /// `config.codemode` when set (`toTool`, `index.ts:304-308`).
    pub codemode: Option<bool>,
    pub raw: Map<String, Value>,
}

impl Tool {
    pub(crate) fn from_raw(server: &str, codemode: Option<bool>, raw: Map<String, Value>) -> Option<Self> {
        let name = raw.get("name")?.as_str()?.to_string();
        Some(Self {
            server: server.to_string(),
            name,
            description: raw.get("description").and_then(Value::as_str).map(String::from),
            input_schema: raw.get("inputSchema").cloned(),
            output_schema: raw.get("outputSchema").cloned(),
            codemode,
            raw,
        })
    }

    /// Registry input schema: `inputSchema ?? {type:"object",properties:{}}`.
    pub fn registry_input(&self) -> Value {
        self.input_schema.clone().unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}))
    }

    /// Registry output schema: `outputSchema ?? {}`.
    pub fn registry_output(&self) -> Value {
        self.output_schema.clone().unwrap_or_else(|| Value::Object(Map::new()))
    }

    /// The direct tool id `<namespace>_<tool>`.
    pub fn prefixed_name(&self) -> String {
        crate::naming::tool_name(&self.server, &self.name)
    }
}

/// A prompt argument declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptArgument {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

/// An MCP prompt tagged with its server (`Mcp.Prompt`).
#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    pub server: String,
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub arguments: Vec<PromptArgument>,
    pub raw: Map<String, Value>,
}

impl Prompt {
    pub(crate) fn from_raw(server: &str, raw: Map<String, Value>) -> Option<Self> {
        let name = raw.get("name")?.as_str()?.to_string();
        let arguments = raw
            .get("arguments")
            .and_then(|v| serde_json::from_value::<Vec<PromptArgument>>(v.clone()).ok())
            .unwrap_or_default();
        Some(Self {
            server: server.to_string(),
            name,
            title: raw.get("title").and_then(Value::as_str).map(String::from),
            description: raw.get("description").and_then(Value::as_str).map(String::from),
            arguments,
            raw,
        })
    }
}

/// `GetPromptResult` tagged with server and prompt name (`Mcp.PromptResult`).
#[derive(Debug, Clone, PartialEq)]
pub struct PromptResult {
    pub server: String,
    pub name: String,
    pub description: Option<String>,
    /// `PromptMessage[]` as sent by the server (`{role, content}`).
    pub messages: Vec<Value>,
    pub raw: Map<String, Value>,
}

/// `McpClient.CallToolContent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CallToolContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "media")]
    Media {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        /// The embedded resource's URI (`resource.uri`), for blobs of `resource` parts.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
    },
}

/// `Mcp.ToolResult`: `{isError, structured?, content, server, tool}`.
#[derive(Debug, Clone, PartialEq)]
pub struct CallToolResult {
    pub is_error: bool,
    pub structured: Option<Value>,
    pub content: Vec<CallToolContent>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub server: String,
    pub tool: String,
    pub is_error: bool,
    pub structured: Option<Value>,
    pub content: Vec<CallToolContent>,
}

/// `toCallToolResult` (`client.ts:399-418`).
pub fn to_call_tool_result(result: &Value) -> CallToolResult {
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| parts.iter().flat_map(map_content_part).collect())
        .unwrap_or_default();
    CallToolResult {
        is_error: result.get("isError") == Some(&Value::Bool(true)),
        structured: result.get("structuredContent").cloned(),
        content,
    }
}

fn map_content_part(part: &Value) -> Vec<CallToolContent> {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(String::from);
    match part.get("type").and_then(Value::as_str) {
        Some("text") => vec![CallToolContent::Text { text: s(part, "text").unwrap_or_default() }],
        Some("image") | Some("audio") => vec![CallToolContent::Media {
            data: s(part, "data").unwrap_or_default(),
            mime_type: s(part, "mimeType").unwrap_or_default(),
            uri: None,
        }],
        Some("resource_link") => vec![CallToolContent::Text { text: s(part, "uri").unwrap_or_default() }],
        Some("resource") => {
            let resource = part.get("resource").cloned().unwrap_or(Value::Null);
            if let Some(text) = resource.get("text").and_then(Value::as_str) {
                return vec![CallToolContent::Text { text: text.to_string() }];
            }
            if let (Some(blob), Some(mime)) =
                (resource.get("blob").and_then(Value::as_str), resource.get("mimeType").and_then(Value::as_str))
            {
                return vec![CallToolContent::Media {
                    data: blob.to_string(),
                    mime_type: mime.to_string(),
                    uri: s(&resource, "uri"),
                }];
            }
            vec![CallToolContent::Text { text: s(&resource, "uri").unwrap_or_default() }]
        }
        _ => vec![],
    }
}

/// Client identity sent in `initialize` (`Implementation`). Upstream default is
/// `{name:"opencode", version:"unknown"}`; the server overrides the version with the app version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self { name: "opencode".into(), version: "unknown".into() }
    }
}

/// Ephemeral bus events published by the registry; payload is always `{server}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpEvent {
    /// `mcp.tools.changed`
    ToolsChanged { server: String },
    /// `mcp.resources.changed`
    ResourcesChanged { server: String },
    /// `mcp.status.changed`
    StatusChanged { server: String },
    /// `mcp.prompts.changed` (core-local)
    PromptsChanged { server: String },
}

impl McpEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::ToolsChanged { .. } => "mcp.tools.changed",
            Self::ResourcesChanged { .. } => "mcp.resources.changed",
            Self::StatusChanged { .. } => "mcp.status.changed",
            Self::PromptsChanged { .. } => "mcp.prompts.changed",
        }
    }

    pub fn server(&self) -> &str {
        match self {
            Self::ToolsChanged { server }
            | Self::ResourcesChanged { server }
            | Self::StatusChanged { server }
            | Self::PromptsChanged { server } => server,
        }
    }

    /// The bus payload `{"server": "<name>"}`.
    pub fn payload(&self) -> Value {
        serde_json::json!({ "server": self.server() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_shapes() {
        assert_eq!(serde_json::to_value(Status::Connected).unwrap(), json!({"status":"connected"}));
        assert_eq!(serde_json::to_value(Status::failed("x")).unwrap(), json!({"status":"failed","error":"x"}));
        assert_eq!(
            serde_json::to_value(ServerInfo { name: "a".into(), status: Status::needs_auth("y"), integration_id: Some("mcp_1".into()) })
                .unwrap(),
            json!({"name":"a","status":{"status":"needs_auth","error":"y"},"integrationID":"mcp_1"})
        );
    }

    #[test]
    fn tool_result_mapping() {
        let result = to_call_tool_result(&json!({
            "isError": true,
            "structuredContent": {"a": 1},
            "content": [
                {"type": "text", "text": "hi"},
                {"type": "image", "data": "AAA", "mimeType": "image/png"},
                {"type": "audio", "data": "BBB", "mimeType": "audio/wav"},
                {"type": "resource_link", "uri": "file:///x", "name": "x"},
                {"type": "resource", "resource": {"uri": "a://t", "text": "embedded"}},
                {"type": "resource", "resource": {"uri": "a://b", "blob": "CCC", "mimeType": "application/pdf"}},
                {"type": "resource", "resource": {"uri": "a://c", "blob": "DDD"}},
                {"type": "unknown"}
            ]
        }));
        assert!(result.is_error);
        assert_eq!(result.structured, Some(json!({"a": 1})));
        assert_eq!(
            serde_json::to_value(&result.content).unwrap(),
            json!([
                {"type":"text","text":"hi"},
                {"type":"media","data":"AAA","mimeType":"image/png"},
                {"type":"media","data":"BBB","mimeType":"audio/wav"},
                {"type":"text","text":"file:///x"},
                {"type":"text","text":"embedded"},
                {"type":"media","data":"CCC","mimeType":"application/pdf","uri":"a://b"},
                {"type":"text","text":"a://c"}
            ])
        );
        assert!(!to_call_tool_result(&json!({"content": [], "isError": "true"})).is_error);
    }
}
