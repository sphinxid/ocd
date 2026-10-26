//! MCP resource tools (`core/src/tool/plugin/mcp-resource.ts`, namespace `opencode`).

use std::sync::Arc;

use async_trait::async_trait;
use oc_schema::ToolContent;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tool::{DefinitionContext, ErrorCause, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, decode, meta};

pub const LIST_DESCRIPTION: &str = "List documents, records, and other data exposed by MCP servers. Use this when the user refers to something that is not a local file, such as a URI with a custom scheme, then load a match with read_mcp_resource. Each entry names the server to read it from. Templates are resources addressed by a parameter such as a record ID; fill in the uriTemplate placeholders before reading.";
pub const READ_DESCRIPTION: &str = "Read one MCP resource by server and URI. Not for local files. Always return the full contents rather than slicing or filtering them; oversized output is truncated automatically and the full content is saved to a file you can read. Images and PDFs are shown to you directly.";

pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(ListMcpResources), Arc::new(ReadMcpResource)]
}

pub struct ListMcpResources;
pub struct ReadMcpResource;

#[derive(Deserialize)]
struct ListInput {
    server: Option<String>,
}

#[derive(Deserialize)]
struct ReadInput {
    server: String,
    uri: String,
}

#[async_trait]
impl Tool for ListMcpResources {
    fn name(&self) -> &str {
        "list_mcp_resources"
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::codemode("opencode")
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        super::definition("opencode_list_mcp_resources", LIST_DESCRIPTION, include_str!("../../schemas/opencode_list_mcp_resources.json"))
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let input: ListInput = decode(input)?;
        let fail = |m: String| ToolError::with_cause(m.clone(), ErrorCause::Error(m));
        let servers = match &input.server {
            Some(s) => vec![s.clone()],
            None => ctx.host.mcp_servers().await.map_err(fail)?,
        };
        let request = ctx.permission("opencode_list_mcp_resources", servers.clone(), Some(servers), Some(meta(vec![])));
        if let Err(error) = ctx.host.ask_permission(request).await {
            return Err(match error {
                crate::host::PermissionError::Corrected { feedback } => ToolError::with_cause("", ErrorCause::PermissionRejected(feedback)),
                crate::host::PermissionError::Blocked { message } => ToolError::with_cause(message.clone(), ErrorCause::PermissionRejected(message)),
                other => other.into_tool_error(""),
            });
        }
        let output = match &input.server {
            None => ctx.host.mcp_resource_catalog().await,
            Some(server) => ctx.host.mcp_resources(server).await,
        }
        .map_err(fail)?;
        Ok(ToolResult { output: Some(output), content: Vec::new(), metadata: None })
    }
}

#[async_trait]
impl Tool for ReadMcpResource {
    fn name(&self) -> &str {
        "read_mcp_resource"
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::codemode("opencode")
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        super::definition("opencode_read_mcp_resource", READ_DESCRIPTION, include_str!("../../schemas/opencode_read_mcp_resource.json"))
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let input: ReadInput = decode(input)?;
        let resource = format!("{}:{}", input.server, input.uri);
        let fail = |m: String| ToolError::with_cause(format!("Unable to read MCP resource {resource}: {m}"), ErrorCause::Error(m));
        let request = ctx.permission(
            "opencode_read_mcp_resource",
            vec![resource.clone()],
            Some(vec![format!("{}:*", input.server)]),
            Some(meta(vec![("server", json!(input.server)), ("uri", json!(input.uri))])),
        );
        if let Err(error) = ctx.host.ask_permission(request).await {
            let message = match &error {
                crate::host::PermissionError::Corrected { feedback } => feedback.clone(),
                crate::host::PermissionError::Blocked { message } => message.clone(),
                crate::host::PermissionError::Other { message } => message.clone(),
                _ => String::new(),
            };
            return Err(error.into_tool_error(format!("Unable to read MCP resource {resource}: {message}")));
        }
        let Some(output) = ctx.host.mcp_read_resource(&input.server, &input.uri).await.map_err(fail)? else {
            return Err(ToolError::new(format!("MCP server \"{}\" is not connected or does not expose resources", input.server)));
        };
        let content: Vec<ToolContent> = output
            .get("contents")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| {
                        let mime = part.get("mimeType").and_then(Value::as_str)?;
                        let blob = part.get("blob").and_then(Value::as_str)?;
                        if part.get("type").and_then(Value::as_str) != Some("blob") || !crate::tools::read::MEDIA_MIMES.contains(&mime) {
                            return None;
                        }
                        Some(ToolContent::File { uri: format!("data:{mime};base64,{blob}"), mime: mime.to_string(), name: None })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(ToolResult { output: Some(output), content, metadata: None })
    }
}
