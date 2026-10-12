//! Pure helpers core uses around MCP: tool naming (`packages/core/src/tool/mcp.ts:16-17`), tool
//! execution result mapping (`tool/mcp.ts:49-117`), MCP prompts as slash commands
//! (`packages/core/src/plugin/command.ts`), the MCP resource tools' model-visible strings
//! (`packages/core/src/tool/plugin/mcp-resource.ts`) and the `<mcp_instructions>` rendering
//! (`packages/core/src/mcp/instructions.ts`).

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use crate::types::{CallToolContent, PromptResult, ResourceContent, ResourceContentPart, ToolResult};

static UNSAFE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^a-zA-Z0-9_-]").unwrap());

/// `/[^a-zA-Z0-9_-]/g → "_"`. Works per UTF-16 code unit upstream; non-BMP characters (two code
/// units) therefore become two underscores.
pub fn sanitize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            for _ in 0..ch.len_utf16() {
                out.push('_');
            }
        }
    }
    debug_assert!(!UNSAFE.is_match(&out));
    out
}

/// Registry namespace and permission namespace for a server: `namespace(server)`.
pub fn namespace(server: &str) -> String {
    sanitize(server)
}

/// Direct tool id / permission action: `${namespace(server)}_${sanitize(tool)}`.
pub fn tool_name(server: &str, tool: &str) -> String {
    format!("{}_{}", namespace(server), sanitize(tool))
}

/// Slash-command name for an MCP prompt: `<sanitize(server)>:<sanitize(prompt)>`.
pub fn prompt_command_name(server: &str, prompt: &str) -> String {
    format!("{}:{}", sanitize(server), sanitize(prompt))
}

static ARGS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?i)(?:\[Image\s+\d+\]|"[^"]*"|'[^']*'|[^\s"']+)"#).unwrap());
static QUOTE_TRIM: Lazy<Regex> = Lazy::new(|| Regex::new(r#"^["']|["']$"#).unwrap());

/// Splits slash-command text into positional prompt arguments (`parseArguments`).
pub fn parse_prompt_arguments(input: &str) -> Vec<String> {
    ARGS.find_iter(input).map(|m| QUOTE_TRIM.replace_all(m.as_str(), "").into_owned()).collect()
}

/// Maps positional arguments onto `prompt.arguments` by name; missing ones become `""`.
pub fn prompt_arguments(declared: &[crate::types::PromptArgument], input: &str) -> indexmap::IndexMap<String, String> {
    let args = parse_prompt_arguments(input);
    declared
        .iter()
        .enumerate()
        .map(|(index, argument)| (argument.name.clone(), args.get(index).cloned().unwrap_or_default()))
        .collect()
}

/// `promptMessageText`.
pub fn prompt_message_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("text") => {
            o.get("text").and_then(Value::as_str).unwrap_or("").to_string()
        }
        _ => String::new(),
    }
}

/// The session prompt text produced from a prompt result: message texts joined with `\n`, trimmed.
pub fn prompt_result_text(result: &PromptResult) -> String {
    result
        .messages
        .iter()
        .map(|m| prompt_message_text(m.get("content").unwrap_or(&Value::Null)))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// `MCP prompt not found: <server>:<name>`.
pub fn prompt_not_found(server: &str, name: &str) -> String {
    format!("MCP prompt not found: {server}:{name}")
}

/// One content part of a tool execution result handed to the model.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "type")]
pub enum ToolContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "file")]
    File { uri: String, mime: String },
}

/// What an MCP tool's `execute` returns on success.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub output: Value,
    /// `None` when there are no content parts (upstream omits the key).
    pub content: Option<Vec<ToolContentPart>>,
}

/// `MCP server "<s>" is not available` (`MCP.NotFoundError` while executing a tool).
pub fn tool_server_not_available(server: &str) -> String {
    format!("MCP server \"{server}\" is not available")
}

/// `Unable to execute <prefixed name>`.
pub fn tool_unable_to_execute(server: &str, tool: &str) -> String {
    format!("Unable to execute {}", tool_name(server, tool))
}

/// Maps a successful `callTool` into the tool output (`tool/mcp.ts:79-110`). Returns `Err` with
/// the ToolFailure message when `isError` is set.
pub fn tool_output(result: &ToolResult, output_schema: Option<&Value>) -> Result<ToolOutput, String> {
    if result.is_error {
        let text = result
            .content
            .iter()
            .filter_map(|p| match p {
                CallToolContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text = text.trim();
        return Err(if text.is_empty() { "MCP tool returned an error".to_string() } else { text.to_string() });
    }
    let content: Vec<ToolContentPart> = result
        .content
        .iter()
        .map(|part| match part {
            CallToolContent::Text { text } => ToolContentPart::Text { text: text.clone() },
            CallToolContent::Media { data, mime_type } => ToolContentPart::File {
                uri: format!("data:{mime_type};base64,{data}"),
                mime: mime_type.clone(),
            },
        })
        .collect();
    let text = content
        .iter()
        .filter_map(|p| match p {
            ToolContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let output = if let Some(structured) = &result.structured {
        structured.clone()
    } else if text.is_empty() {
        Value::Null
    } else if output_schema.is_none() && (text.starts_with('{') || text.starts_with('[')) {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    } else {
        Value::String(text)
    };
    Ok(ToolOutput { output, content: if content.is_empty() { None } else { Some(content) } })
}

/// `list_mcp_resources` description (verbatim).
pub const LIST_RESOURCES_DESCRIPTION: &str = "List documents, records, and other data exposed by MCP servers. Use this when the user refers to something that is not a local file, such as a URI with a custom scheme, then load a match with read_mcp_resource. Each entry names the server to read it from. Templates are resources addressed by a parameter such as a record ID; fill in the uriTemplate placeholders before reading.";
/// `read_mcp_resource` description (verbatim).
pub const READ_RESOURCE_DESCRIPTION: &str = "Read one MCP resource by server and URI. Not for local files. Always return the full contents rather than slicing or filtering them; oversized output is truncated automatically and the full content is saved to a file you can read. Images and PDFs are shown to you directly.";

/// `MCP server "<s>" is not connected or does not expose resources`.
pub fn resource_unavailable(server: &str) -> String {
    format!("MCP server \"{server}\" is not connected or does not expose resources")
}

/// `Unable to read MCP resource <s>:<uri>: <msg>`.
pub fn resource_read_failed(server: &str, uri: &str, message: &str) -> String {
    format!("Unable to read MCP resource {server}:{uri}: {message}")
}

/// File attachments for `read_mcp_resource`: blob parts that are images or PDFs.
pub fn resource_files(resource: &ResourceContent) -> Vec<ToolContentPart> {
    resource
        .contents
        .iter()
        .filter_map(|part| match part {
            ResourceContentPart::Blob { blob, mime_type: Some(mime), .. }
                if mime.starts_with("image/") || mime == "application/pdf" =>
            {
                Some(ToolContentPart::File { uri: format!("data:{mime};base64,{blob}"), mime: mime.clone() })
            }
            _ => None,
        })
        .collect()
}

/// One server's entry in the `core/mcp-guidance` instruction source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionSummary {
    pub server: String,
    pub instructions: String,
    /// `false` when the server's tools are exposed directly; `true` means Code Mode.
    pub codemode: bool,
}

fn instruction_entries(servers: &[InstructionSummary]) -> Vec<String> {
    let mut out = Vec::new();
    for server in servers {
        out.push(format!("  <server name=\"{}\">", server.server));
        if server.codemode {
            out.push(format!(
                "    Use tools from this server through `execute` under `tools[{}]`.",
                serde_json::to_string(&namespace(&server.server)).unwrap()
            ));
        }
        out.extend(server.instructions.split('\n').map(|line| format!("    {line}")));
        out.push("  </server>".to_string());
    }
    out
}

/// Initial render of `<mcp_instructions>`.
pub fn render_instructions(servers: &[InstructionSummary]) -> String {
    let mut lines = vec!["<mcp_instructions>".to_string()];
    lines.extend(instruction_entries(servers));
    lines.push("</mcp_instructions>".to_string());
    lines.join("\n")
}

/// Render when the instruction set changes.
pub fn render_instructions_update(previous: &[InstructionSummary], current: &[InstructionSummary]) -> String {
    let added: Vec<InstructionSummary> =
        current.iter().filter(|c| !previous.iter().any(|p| p.server == c.server)).cloned().collect();
    let removed: Vec<&InstructionSummary> =
        previous.iter().filter(|p| !current.iter().any(|c| c.server == p.server)).collect();
    let changed = current.iter().any(|c| {
        previous
            .iter()
            .find(|p| p.server == c.server)
            .is_some_and(|p| p.instructions != c.instructions || p.codemode != c.codemode)
    });
    if changed || (added.is_empty() && removed.is_empty()) {
        return [
            "The available MCP server instructions have changed. This list supersedes the previous one.".to_string(),
            render_instructions(current),
        ]
        .join("\n");
    }
    let mut lines = Vec::new();
    if !added.is_empty() {
        lines.push("New MCP server instructions are available in addition to those previously listed:".to_string());
        lines.extend(instruction_entries(&added));
    }
    if !removed.is_empty() {
        lines.push(format!(
            "Instructions for the following MCP servers are no longer available: {}.",
            removed.iter().map(|s| s.server.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    lines.join("\n")
}

/// Render when every server's instructions disappeared.
pub const INSTRUCTIONS_REMOVED: &str = "MCP server instructions are no longer available.";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names() {
        assert_eq!(namespace("my.server/x"), "my_server_x");
        assert_eq!(tool_name("a.b", "c d"), "a_b_c_d");
        assert_eq!(tool_name("a_b", "c"), tool_name("a.b", "c"));
        assert_eq!(tool_name("srv", "ok-name_1"), "srv_ok-name_1");
        assert_eq!(sanitize("é😀"), "___");
        assert_eq!(prompt_command_name("my server", "do.it"), "my_server:do_it");
    }

    #[test]
    fn prompt_args() {
        assert_eq!(
            parse_prompt_arguments(r#"one "two three" 'four' [Image 12] five"#),
            vec!["one", "two three", "four", "[Image 12]", "five"]
        );
        let declared = vec![
            crate::types::PromptArgument { name: "a".into(), description: None, required: None },
            crate::types::PromptArgument { name: "b".into(), description: None, required: None },
        ];
        let mapped = prompt_arguments(&declared, "x");
        assert_eq!(mapped.get("a").unwrap(), "x");
        assert_eq!(mapped.get("b").unwrap(), "");
    }

    #[test]
    fn tool_output_mapping() {
        let result = |content, structured, is_error| ToolResult {
            server: "s".into(),
            tool: "t".into(),
            is_error,
            structured,
            content,
        };
        let err = tool_output(&result(vec![CallToolContent::Text { text: " bad \n".into() }], None, true), None);
        assert_eq!(err.unwrap_err(), "bad");
        let err = tool_output(&result(vec![], None, true), None);
        assert_eq!(err.unwrap_err(), "MCP tool returned an error");
        let ok = tool_output(&result(vec![CallToolContent::Text { text: "{\"a\":1}".into() }], None, false), None).unwrap();
        assert_eq!(ok.output, json!({"a": 1}));
        let ok = tool_output(
            &result(vec![CallToolContent::Text { text: "{\"a\":1}".into() }], None, false),
            Some(&json!({"type":"object"})),
        )
        .unwrap();
        assert_eq!(ok.output, json!("{\"a\":1}"));
        let ok = tool_output(&result(vec![], None, false), None).unwrap();
        assert_eq!(ok.output, Value::Null);
        assert!(ok.content.is_none());
        let ok = tool_output(
            &result(vec![CallToolContent::Media { data: "AA".into(), mime_type: "image/png".into() }], Some(json!(1)), false),
            None,
        )
        .unwrap();
        assert_eq!(ok.output, json!(1));
        assert_eq!(
            ok.content.unwrap(),
            vec![ToolContentPart::File { uri: "data:image/png;base64,AA".into(), mime: "image/png".into() }]
        );
    }

    #[test]
    fn instructions_rendering() {
        let a = InstructionSummary { server: "a.b".into(), instructions: "line1\nline2".into(), codemode: true };
        let b = InstructionSummary { server: "c".into(), instructions: "x".into(), codemode: false };
        assert_eq!(
            render_instructions(&[a.clone(), b.clone()]),
            "<mcp_instructions>\n  <server name=\"a.b\">\n    Use tools from this server through `execute` under `tools[\"a_b\"]`.\n    line1\n    line2\n  </server>\n  <server name=\"c\">\n    x\n  </server>\n</mcp_instructions>"
        );
        assert_eq!(
            render_instructions_update(&[a.clone()], &[a.clone(), b.clone()]),
            "New MCP server instructions are available in addition to those previously listed:\n  <server name=\"c\">\n    x\n  </server>"
        );
        assert_eq!(
            render_instructions_update(&[a.clone(), b.clone()], &[a.clone()]),
            "Instructions for the following MCP servers are no longer available: c."
        );
        assert!(render_instructions_update(&[a.clone()], &[a.clone()]).starts_with("The available MCP server"));
    }
}
