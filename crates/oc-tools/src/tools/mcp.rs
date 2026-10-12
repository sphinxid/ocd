//! MCP server tools (`core/src/tool/mcp.ts`). Core registers one `McpTool` per discovered tool.

use async_trait::async_trait;
use oc_schema::ToolContent;
use serde_json::{Value, json};

use crate::host::{McpCallError, McpContent};
use crate::tool::{DefinitionContext, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, meta, normalized_name};
use crate::validate::{Flavor, Issue};

/// Registry namespace for a server.
pub fn namespace(server: &str) -> String {
    normalized_name(server)
}

/// Permission action / effective name `<namespace>_<tool>`.
pub fn name(server: &str, tool: &str) -> String {
    format!("{}_{}", namespace(server), normalized_name(tool))
}

/// A discovered MCP tool.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolInfo {
    pub server: String,
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Option<Value>,
    pub output_schema: Option<Value>,
    /// `tool.codemode !== false`.
    pub codemode: bool,
}

pub struct McpTool {
    pub info: McpToolInfo,
}

impl McpTool {
    pub fn new(info: McpToolInfo) -> Self {
        Self { info }
    }
}

/// `output()` derivation for an MCP result.
pub fn derive_output(structured: Option<Value>, text: &str, has_output_schema: bool) -> Value {
    if let Some(structured) = structured {
        return structured;
    }
    if text.is_empty() {
        return Value::Null;
    }
    if !has_output_schema && (text.starts_with('{') || text.starts_with('[')) {
        if let Ok(value) = serde_json::from_str(text) {
            return value;
        }
    }
    Value::String(text.to_string())
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.info.name
    }

    fn options(&self) -> ToolOptions {
        ToolOptions { namespace: Some(namespace(&self.info.server)), permission: None, codemode: self.info.codemode, pinned: false }
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        ToolDefinition::new(
            name(&self.info.server, &self.info.name),
            self.info.description.clone().unwrap_or_default(),
            self.info.input_schema.clone().unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        )
        .with_output(Some(self.info.output_schema.clone().unwrap_or_else(|| json!({}))))
    }

    fn validate(&self, input: &Value) -> Vec<Issue> {
        let schema = self.info.input_schema.clone().unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        crate::validate::validate(&schema, input, Flavor::Json)
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let full = name(&self.info.server, &self.info.name);
        let generic = || ToolError::new(format!("Unable to execute {full}"));
        if let Err(error) = ctx.host.ask_permission(ctx.permission(&full, vec!["*".into()], Some(vec!["*".into()]), Some(meta(vec![])))).await {
            let mapped = error.into_tool_error("");
            return Err(if mapped.is_defect() { mapped } else { generic() });
        }
        let args = if input.is_null() { json!({}) } else { input };
        let result = match ctx.host.mcp_call_tool(&self.info.server, &self.info.name, args, &ctx.session_id).await {
            Ok(result) => result,
            Err(McpCallError::NotFound { server }) => return Err(ToolError::new(format!("MCP server \"{server}\" is not available"))),
            Err(McpCallError::ToolCall { message }) => return Err(ToolError::new(message)),
            Err(McpCallError::Other(_)) => return Err(generic()),
        };
        let texts: Vec<&str> = result
            .content
            .iter()
            .filter_map(|c| match c {
                McpContent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        if result.is_error {
            let joined = texts.join("\n");
            let trimmed = crate::util::js::trim(&joined);
            return Err(ToolError::new(if trimmed.is_empty() { "MCP tool returned an error".to_string() } else { trimmed.to_string() }));
        }
        let content: Vec<ToolContent> = result
            .content
            .iter()
            .map(|c| match c {
                McpContent::Text(t) => ToolContent::Text { text: t.clone() },
                McpContent::Data { data, mime } => ToolContent::File { uri: format!("data:{mime};base64,{data}"), mime: mime.clone(), name: None },
            })
            .collect();
        let text = texts.join("\n");
        let output = derive_output(result.structured, &text, self.info.output_schema.is_some());
        Ok(ToolResult { output: Some(output), content, metadata: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_output() {
        assert_eq!(name("my.server", "do-thing"), "my_server_do-thing");
        assert_eq!(derive_output(None, "", false), Value::Null);
        assert_eq!(derive_output(None, "{\"a\":1}", false), json!({"a": 1}));
        assert_eq!(derive_output(None, "{\"a\":1}", true), json!("{\"a\":1}"));
        assert_eq!(derive_output(None, "[bad", false), json!("[bad"));
        assert_eq!(derive_output(Some(json!(1)), "x", false), json!(1));
        let tool = McpTool::new(McpToolInfo { server: "s".into(), name: "t".into(), description: None, input_schema: None, output_schema: None, codemode: true });
        let def = tool.definition(&DefinitionContext::default());
        assert_eq!(def.input_schema, json!({"type": "object", "properties": {}}));
        assert_eq!(def.output_schema, Some(json!({})));
        assert_eq!(tool.options().namespace.as_deref(), Some("s"));
    }
}
