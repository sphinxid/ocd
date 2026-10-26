//! MCP server tools (`core/src/tool/mcp.ts`). Core registers one `McpTool` per discovered tool.

use std::path::Path;

use async_trait::async_trait;
use oc_schema::ToolContent;
use serde_json::{Value, json};

use crate::host::{McpCallError, McpContent};
use crate::tools::read::MEDIA_MIMES;
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
        let args = if input.is_null() { json!({}) } else { input };
        let metadata = meta(vec![("input", args.clone())]);
        if let Err(error) = ctx.host.ask_permission(ctx.permission(&full, vec!["*".into()], Some(vec!["*".into()]), Some(metadata))).await {
            let mapped = error.into_tool_error("");
            return Err(if mapped.is_defect() { mapped } else { generic() });
        }
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
        // Upstream treats a failed write as a defect; here it fails the call.
        let content = result
            .content
            .iter()
            .map(|part| to_content(part, &ctx.global.data))
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|e| ToolError::new(e.to_string()))?;
        let text = texts.join("\n");
        let output = derive_output(result.structured, &text, self.info.output_schema.is_some());
        Ok(ToolResult { output: Some(output), content, metadata: None })
    }
}

/// `toContent`: only media the read tool also accepts is sent inline; other types would fail request lowering
/// on every later turn. Textual blobs become text; other binaries are saved to the tool-output directory.
pub fn to_content(part: &McpContent, data_dir: &Path) -> std::io::Result<ToolContent> {
    let (data, mime_type, uri) = match part {
        McpContent::Text(text) => return Ok(ToolContent::Text { text: text.clone() }),
        McpContent::Data { data, mime, uri } => (data, mime, uri),
    };
    let mime = mime_type.split(';').next().unwrap_or("").trim().to_lowercase();
    if MEDIA_MIMES.contains(&mime.as_str()) {
        return Ok(ToolContent::File { uri: format!("data:{mime_type};base64,{data}"), mime: mime_type.clone(), name: None });
    }
    let bytes = node_base64_decode(data);
    let label = uri.as_deref().unwrap_or("content");
    if crate::mime::is_textual(&mime) {
        return Ok(ToolContent::Text { text: format!("[Resource {label} ({mime_type})]\n{}", String::from_utf8_lossy(&bytes)) });
    }
    let extension = crate::mime::extension(&mime).map(|e| format!(".{e}")).unwrap_or_default();
    let file = crate::tool_output::save(data_dir, &bytes, &extension)?;
    Ok(ToolContent::Text { text: format!("[Binary resource {label} ({mime_type}, {} bytes) saved to {}]", bytes.len(), file.display()) })
}

/// Node `Buffer.from(s, "base64")`: both alphabets, whitespace and other characters skipped, stops at `=`.
fn node_base64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
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

    use std::sync::Arc;

    use crate::host::{McpCallResult, PermissionError, PermissionRequest, ToolHost};
    use crate::tool::ToolConfig;

    /// Answers every permission ask and returns the fixture tools of upstream's `mcp.test.ts`.
    #[derive(Default)]
    struct Host {
        requests: parking_lot::Mutex<Vec<PermissionRequest>>,
    }

    #[async_trait]
    impl ToolHost for Host {
        async fn ask_permission(&self, request: PermissionRequest) -> Result<(), PermissionError> {
            self.requests.lock().push(request);
            Ok(())
        }

        async fn mcp_call_tool(&self, _server: &str, name: &str, _args: Value, _session: &str) -> Result<McpCallResult, McpCallError> {
            let content = match name {
                "media" => vec![
                    McpContent::Text("Here is the image".into()),
                    McpContent::Data { data: "aGVsbG8=".into(), mime: "image/png".into(), uri: None },
                ],
                "attachments" => vec![
                    McpContent::Data { data: "IyBOb3Rlcw==".into(), mime: "text/markdown".into(), uri: Some("file:///README.md".into()) },
                    McpContent::Data {
                        data: "UEFSMQ==".into(),
                        mime: "application/vnd.apache.parquet".into(),
                        uri: Some("file:///data.parquet".into()),
                    },
                ],
                _ => vec![McpContent::Text("ok".into())],
            };
            Ok(McpCallResult { content, structured: None, is_error: false })
        }
    }

    fn context(dir: &Path, host: Arc<Host>) -> ToolContext {
        let mut global = oc_util::Global::resolve();
        global.data = dir.join(".data");
        global.bin = dir.join(".bin");
        global.home = dir.join(".home");
        ToolContext {
            session_id: "ses_mcp".into(),
            message_id: "msg_mcp".into(),
            call_id: "call_mcp".into(),
            agent: "build".into(),
            directory: dir.to_path_buf(),
            worktree: dir.to_path_buf(),
            project_id: "prj".into(),
            workspace_id: None,
            global: Arc::new(global),
            cancel: tokio_util::sync::CancellationToken::new(),
            progress: Arc::new(|_| {}),
            config: Arc::new(ToolConfig::default()),
            host,
        }
    }

    fn tool(name: &str) -> McpTool {
        McpTool::new(McpToolInfo { server: "direct".into(), name: name.into(), description: None, input_schema: None, output_schema: None, codemode: false })
    }

    // mcp.test.ts "preserves MCP text and media content for the model"
    #[tokio::test]
    async fn keeps_read_tool_media_inline() {
        let dir = tempfile::tempdir().unwrap();
        let result = tool("media").execute(json!({}), context(dir.path(), Arc::default())).await.unwrap();
        assert_eq!(
            result.content,
            vec![
                ToolContent::Text { text: "Here is the image".into() },
                ToolContent::File { uri: "data:image/png;base64,aGVsbG8=".into(), mime: "image/png".into(), name: None },
            ]
        );
    }

    // mcp.test.ts "decodes text blobs and saves other MCP blobs instead of attaching them"
    #[tokio::test]
    async fn decodes_text_blobs_and_saves_other_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path(), Arc::default());
        let data = ctx.global.data.clone();
        let result = tool("attachments").execute(json!({}), ctx).await.unwrap();
        let texts: Vec<String> = result
            .content
            .iter()
            .map(|part| match part {
                ToolContent::Text { text } => text.clone(),
                ToolContent::File { mime, .. } => mime.clone(),
            })
            .collect();
        assert_eq!(texts[0], "[Resource file:///README.md (text/markdown)]\n# Notes");
        let saved = texts[1]
            .strip_prefix("[Binary resource file:///data.parquet (application/vnd.apache.parquet, 4 bytes) saved to ")
            .and_then(|rest| rest.strip_suffix(']'))
            .expect(&texts[1]);
        assert!(saved.starts_with(&data.join(crate::tool_output::DIRECTORY).join("tool_").display().to_string()), "{saved}");
        assert_eq!(std::fs::read_to_string(saved).unwrap(), "PAR1");
    }

    #[test]
    fn saved_blobs_take_the_mime_db_extension() {
        let dir = tempfile::tempdir().unwrap();
        let part = |mime: &str, data: &str| McpContent::Data { data: data.into(), mime: mime.into(), uri: None };
        let ToolContent::Text { text } = to_content(&part("audio/wav", "UklGRg"), dir.path()).unwrap() else { panic!() };
        assert!(text.starts_with("[Binary resource content (audio/wav, 4 bytes) saved to "), "{text}");
        assert!(text.ends_with(".wav]"), "{text}");
        // Parameters are ignored when matching the inline media set, but kept in the data URL.
        assert_eq!(
            to_content(&part("IMAGE/PNG; q=1", "AA"), dir.path()).unwrap(),
            ToolContent::File { uri: "data:IMAGE/PNG; q=1;base64,AA".into(), mime: "IMAGE/PNG; q=1".into(), name: None }
        );
        // SVG is textual, not inline media.
        assert_eq!(
            to_content(&part("image/svg+xml", "PHN2Zy8+"), dir.path()).unwrap(),
            ToolContent::Text { text: "[Resource content (image/svg+xml)]\n<svg/>".into() }
        );
    }

    // mcp.test.ts "waits for permission before calling an MCP tool": the ask carries the input.
    #[tokio::test]
    async fn permission_ask_carries_the_input() {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(Host::default());
        tool("search").execute(json!({"query": "open issues"}), context(dir.path(), host.clone())).await.unwrap();
        tool("search").execute(Value::Null, context(dir.path(), host.clone())).await.unwrap();
        let requests = host.requests.lock();
        assert_eq!(requests[0].action, "direct_search");
        assert_eq!(requests[0].resources, vec!["*".to_string()]);
        assert_eq!(requests[0].save, Some(vec!["*".to_string()]));
        assert_eq!(requests[0].metadata.as_ref().map(|m| Value::Object(m.clone())), Some(json!({"input": {"query": "open issues"}})));
        assert_eq!(requests[1].metadata.as_ref().map(|m| Value::Object(m.clone())), Some(json!({"input": {}})));
    }
}
