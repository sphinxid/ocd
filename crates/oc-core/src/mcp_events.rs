//! The MCP Events tool (`core/src/tool/plugin/mcp-events.ts`, plugin `opencode.tools.mcp-events`):
//! `list_mcp_events` in the `opencode` namespace (Code Mode `opencode.list_mcp_events`) lists the event types a
//! connected server pushes, shaped like the automation event trigger they feed (`{server, event, args}`).
//! Experimental: MCP Events is a beta extension.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use oc_mcp::manager::McpManager;
use oc_tools::tool::{DefinitionContext, ErrorCause, ToolOptions, meta};
use oc_tools::{Tool, ToolContext, ToolDefinition, ToolError, ToolResult};
use serde_json::{Map, Value, json};

pub const PLUGIN_ID: &str = "opencode.tools.mcp-events";
pub const NAME: &str = "list_mcp_events";
pub const DESCRIPTION: &str = "List the events an MCP server sends. Use one as an automation's event trigger.";
/// The permission listing asks for, per server.
pub const ACTION: &str = "opencode_list_mcp_events";
/// The permission an event automation asks for, per server, when it is created and before each stream and run.
pub const WATCH_ACTION: &str = "opencode_watch_mcp_event";

/// The tool's JSON Schemas as upstream's `definition` emits them (`scripts/mcp-events-schemas.ts`).
static SCHEMAS: LazyLock<(Value, Value)> = LazyLock::new(|| {
    let value: Value = serde_json::from_str(include_str!("../../../assets/mcp/list-mcp-events.json")).expect("list_mcp_events schemas");
    (value["inputSchema"].clone(), value["outputSchema"].clone())
});

/// `list_mcp_events`, over the Location's MCP service.
pub struct ListMcpEvents {
    mcp: Arc<McpManager>,
}

impl ListMcpEvents {
    pub fn new(mcp: Arc<McpManager>) -> Arc<ListMcpEvents> {
        Arc::new(ListMcpEvents { mcp })
    }
}

/// Whether a tool (un-namespaced) is the MCP Events tool.
pub fn is_events_tool(name: &str) -> bool {
    name == NAME
}

fn failure(message: impl Into<String>) -> ToolError {
    let message = message.into();
    ToolError::with_cause(message.clone(), ErrorCause::Error(message))
}

/// One listed type as the tool shows it: `{server, event, description?, args, data}`.
fn entry(server: &str, kind: &Value) -> Value {
    let mut out = Map::new();
    out.insert("server".into(), json!(server));
    out.insert("event".into(), kind.get("name").cloned().unwrap_or(Value::Null));
    if let Some(description) = kind.get("description") {
        out.insert("description".into(), description.clone());
    }
    out.insert("args".into(), kind.get("inputSchema").cloned().unwrap_or_else(|| json!({})));
    out.insert("data".into(), kind.get("payloadSchema").cloned().unwrap_or_else(|| json!({})));
    Value::Object(out)
}

#[async_trait]
impl Tool for ListMcpEvents {
    fn name(&self) -> &str {
        NAME
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::codemode("opencode")
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        ToolDefinition::new(format!("opencode_{NAME}"), DESCRIPTION, SCHEMAS.0.clone()).with_output(Some(SCHEMAS.1.clone()))
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let server = input.get("server").and_then(Value::as_str).unwrap_or_default().to_string();
        let request = ctx.permission(ACTION, vec![server.clone()], Some(vec![server.clone()]), Some(meta(vec![])));
        if let Err(error) = ctx.host.ask_permission(request).await {
            return Err(match error {
                oc_tools::host::PermissionError::Corrected { feedback } => ToolError::with_cause("", ErrorCause::PermissionRejected(feedback)),
                oc_tools::host::PermissionError::Blocked { message } => ToolError::with_cause(message.clone(), ErrorCause::PermissionRejected(message)),
                other => other.into_tool_error(""),
            });
        }
        let events = match self.mcp.events(&server).await {
            Ok(Some(events)) => events,
            // A failure the tool states itself (`tool.execution`), not one it wraps.
            Ok(None) => return Err(ToolError::new(format!("MCP server \"{server}\" is not connected or sends no events"))),
            Err(error) => return Err(failure(error.to_string())),
        };
        let types = events.list().await.map_err(|error| failure(error.to_string()))?;
        // Automations can only use events the server pushes.
        let events: Vec<Value> = types
            .iter()
            .filter(|kind| kind.get("delivery").and_then(Value::as_array).is_some_and(|d| d.iter().any(|v| v == "push")))
            .map(|kind| entry(&server, kind))
            .collect();
        Ok(ToolResult { output: Some(json!({ "events": events })), content: Vec::new(), metadata: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_shaped_like_the_event_trigger() {
        let kind = json!({"name": "job.finished", "description": "A job ended", "delivery": ["push"], "inputSchema": {}, "payloadSchema": {"type": "object"}});
        assert_eq!(
            serde_json::to_string(&entry("jobs", &kind)).unwrap(),
            r#"{"server":"jobs","event":"job.finished","description":"A job ended","args":{},"data":{"type":"object"}}"#
        );
        let bare = json!({"name": "a", "delivery": ["push"], "inputSchema": {}, "payloadSchema": {}});
        assert_eq!(entry("s", &bare), json!({"server": "s", "event": "a", "args": {}, "data": {}}));
    }

    #[test]
    fn the_definition_takes_one_server() {
        assert_eq!(SCHEMAS.0["required"], json!(["server"]));
        assert_eq!(SCHEMAS.1["required"], json!(["events"]));
    }
}
