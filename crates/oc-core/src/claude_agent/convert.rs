//! OpenCode → Claude Code conversions: prompt messages to user content blocks
//! (`language-model/convert-to-claude-message.ts`), tool names and descriptions (`tools/names.ts`)
//! and tool results to MCP results (`tools/result.ts`).

use std::collections::HashMap;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::js;
use super::sync::{self, kind, parts, role};

/// `new URL(value).href`.
pub fn href(value: &str) -> String {
    url::Url::parse(value).map(|u| u.to_string()).unwrap_or_else(|_| value.to_string())
}

fn base64(data: Option<&Value>) -> Value {
    match data {
        Some(Value::Object(o)) if o.contains_key("$bytes") => o.get("$bytes").cloned().unwrap_or(json!("")),
        Some(other) => other.clone(),
        None => Value::Null,
    }
}

fn url_of(data: Option<&Value>) -> Option<String> {
    data?.as_object()?.get("$url")?.as_str().map(href)
}

/// `file(part)`: a PDF document or an image block.
fn file(part: &Value) -> Result<Value, String> {
    let media = part.get("mediaType").and_then(Value::as_str).unwrap_or_default();
    let data = part.get("data");
    let block = if media == "application/pdf" {
        "document"
    } else if matches!(media, "image/png" | "image/jpeg" | "image/gif" | "image/webp") {
        "image"
    } else {
        let shown = part.get("mediaType").map(|v| js::stringify(v).trim_matches('"').to_string()).unwrap_or_else(|| "undefined".into());
        return Err(format!("Claude Code does not support {shown} attachments"));
    };
    if let Some(url) = url_of(data) {
        return Ok(json!({ "type": block, "source": { "type": "url", "url": url } }));
    }
    Ok(json!({ "type": block, "source": { "type": "base64", "media_type": media, "data": base64(data) } }))
}

fn text(value: String) -> Value {
    json!({ "type": "text", "text": value })
}

/// `ConvertToClaudeMessage.content(prompt)`: earlier turns become role-tagged text, attachments stay
/// real content blocks.
pub fn content(prompt: &[Value]) -> Result<Vec<Value>, String> {
    let mut blocks = vec![];
    let history = prompt.iter().any(|m| matches!(role(m), "assistant" | "tool"));
    for message in prompt {
        let role = role(message);
        if role == "system" {
            continue;
        }
        if history {
            blocks.push(text(format!("<{role}>")));
        }
        for part in parts(message) {
            match kind(part) {
                "text" => blocks.push(json!({ "type": "text", "text": part.get("text").cloned().unwrap_or(Value::Null) })),
                "file" => blocks.push(file(part)?),
                "tool-call" => {
                    let mut call = Map::new();
                    put(&mut call, "id", part.get("toolCallId"));
                    put(&mut call, "name", part.get("toolName"));
                    put(&mut call, "input", part.get("input"));
                    blocks.push(text(format!("<tool-call>\n{}\n</tool-call>", js::stringify(&Value::Object(call)))));
                }
                "tool-result" => {
                    let mut result = Map::new();
                    put(&mut result, "id", part.get("toolCallId"));
                    put(&mut result, "name", part.get("toolName"));
                    put(&mut result, "output", part.get("output"));
                    blocks.push(text(format!("<tool-result>\n{}\n</tool-result>", js::stringify(&Value::Object(result)))));
                }
                _ => {}
            }
        }
        if history {
            blocks.push(text(format!("</{role}>")));
        }
    }
    Ok(blocks)
}

fn put(out: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(value) = value {
        out.insert(key.into(), value.clone());
    }
}

// ---------------------------------------------------------------------------------------------
// Tool names (`tools/names.ts`)
// ---------------------------------------------------------------------------------------------

pub const PREFIX: &str = "mcp__opencode__";

fn allowed(c: u16) -> bool {
    c < 128 && (c as u8 as char).is_ascii_alphanumeric() || c == b'_' as u16 || c == b'-' as u16
}

/// `ToolNames.short(name)`: within Claude's 64-character tool name limit.
pub fn short(name: &str) -> String {
    let limit = 64 - PREFIX.len();
    let units: Vec<u16> = name.encode_utf16().collect();
    if units.len() <= limit && !units.is_empty() && units.iter().all(|c| allowed(*c)) {
        return name.to_string();
    }
    let hash = hex::encode(Sha256::digest(name.as_bytes()));
    let sanitized: String = units.iter().map(|c| if allowed(*c) { *c as u8 as char } else { '_' }).collect();
    let cut: String = sanitized.chars().take(limit - 11).collect();
    format!("{cut}_{}", &hash[..10])
}

fn tool_name(tool: &Value) -> &str {
    tool.get("name").and_then(Value::as_str).unwrap_or_default()
}

/// `ToolNames.original(name, tools)`: undefined for a tool we did not offer.
pub fn original(name: &str, tools: &[Value]) -> Option<String> {
    tools.iter().find(|t| format!("{PREFIX}{}", short(tool_name(t))) == name).map(|t| tool_name(t).to_string())
}

/// Claude Code cuts each MCP tool description at this many characters.
const DESCRIPTION_LIMIT: usize = 2048;

/// `ToolNames.description(tool)`.
pub fn description(tool: &Value) -> String {
    let text = tool.get("description").and_then(Value::as_str).unwrap_or_default();
    if tool_name(tool) != "subagent" || js::utf16_len(text) <= DESCRIPTION_LIMIT {
        return js::utf16_prefix(text, DESCRIPTION_LIMIT);
    }
    // The subagent list comes last in a long description, so move it first to keep it after the cut.
    let Some(agents) = js::utf16_index_of(text, "Available subagents:") else { return js::utf16_prefix(text, DESCRIPTION_LIMIT) };
    let moved = format!("{}\n\n{}", js::utf16_suffix(text, agents), js::utf16_prefix(text, agents));
    js::utf16_prefix(&moved, DESCRIPTION_LIMIT)
}

/// The MCP `tools/list` entries for the offered tools (`ToolServer.make`).
pub fn definitions(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let mut o = Map::new();
            o.insert("name".into(), json!(short(tool_name(tool))));
            o.insert("description".into(), json!(description(tool)));
            if let Some(schema) = tool.get("inputSchema") {
                o.insert("inputSchema".into(), schema.clone());
            }
            o.insert("annotations".into(), json!({ "readOnlyHint": false }));
            o.insert("_meta".into(), json!({ "anthropic/alwaysLoad": true, "anthropic/maxResultSizeChars": 500000 }));
            Value::Object(o)
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Tool results (`tools/result.ts`)
// ---------------------------------------------------------------------------------------------

/// `ToolResult.Result`: the MCP `CallToolResult` and, when media could not travel in MCP, the
/// content blocks the PostToolUse hook puts in the tool result instead.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub mcp: Value,
    pub replacement: Option<Vec<Value>>,
}

impl ToolResult {
    /// What a call gets when its turn is cancelled.
    pub fn cancelled() -> ToolResult {
        ToolResult { mcp: json!({ "isError": true, "content": [{ "type": "text", "text": "Cancelled by user." }] }), replacement: None }
    }
}

/// `ToolResult.convert(output, files, failed)`.
pub fn convert_result(output: &Value, files: &[Value], failed: bool) -> Result<ToolResult, String> {
    let mut items: Vec<Value> = vec![];
    let kind = output.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "text" | "error-text" => items.push(json!({ "type": "text", "text": output.get("value").cloned().unwrap_or(Value::Null) })),
        "json" | "error-json" => items.push(text(js::stringify(output.get("value").unwrap_or(&Value::Null)))),
        "execution-denied" => items.push(json!({
            "type": "text",
            "text": output.get("reason").and_then(Value::as_str).unwrap_or("Tool execution was denied by the user."),
        })),
        "content" => {
            for part in output.get("value").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
                match sync::kind(part) {
                    "text" => items.push(json!({ "type": "text", "text": part.get("text").cloned().unwrap_or(Value::Null) })),
                    "image-data" | "file-data" => {
                        let mut file = Map::new();
                        file.insert("type".into(), json!("file"));
                        put(&mut file, "mediaType", part.get("mediaType"));
                        put(&mut file, "data", part.get("data"));
                        items.push(Value::Object(file));
                    }
                    // Only picks an image block. URL images are sent without a media type.
                    "image-url" => items.push(json!({
                        "type": "file",
                        "mediaType": "image/png",
                        "data": { "$url": href(part.get("url").and_then(Value::as_str).unwrap_or_default()) },
                    })),
                    other => return Err(format!("Claude Code does not support {other} tool output")),
                }
            }
        }
        _ => {}
    }
    items.extend(files.iter().cloned());
    let content = self::content(&[json!({ "role": "user", "content": items })])?;
    let is_error = failed || matches!(kind, "error-text" | "error-json" | "execution-denied");
    let mut mcp_content = vec![];
    let mut replace = false;
    for block in &content {
        let source = block.get("source");
        if sync::kind(block) == "text" {
            mcp_content.push(json!({ "type": "text", "text": block.get("text").cloned().unwrap_or(Value::Null) }));
            continue;
        }
        if sync::kind(block) == "image" && source.and_then(|s| s.get("type")).and_then(Value::as_str) == Some("base64") {
            let source = source.unwrap();
            mcp_content.push(json!({ "type": "image", "mimeType": source["media_type"], "data": source["data"] }));
            continue;
        }
        replace = true;
        mcp_content.push(json!({ "type": "text", "text": "Attached document or image follows." }));
    }
    // The PostToolUse hook replaces tool_result.content, so it needs content blocks rather than an MCP envelope.
    Ok(ToolResult { mcp: json!({ "content": mcp_content, "isError": is_error }), replacement: replace.then_some(content) })
}

/// `ToolResult.collect(prompt, start)`.
#[derive(Debug, Default)]
pub struct Collected {
    pub results: HashMap<String, ToolResult>,
    pub end: usize,
    pub aborted: bool,
}

pub fn collect(prompt: &[Value], start: usize) -> Result<Collected, String> {
    let mut out = Collected { end: start, ..Default::default() };
    let tail = &prompt[start.min(prompt.len())..];
    for group in sync::groups(tail, &[]) {
        if group.role != "tool" {
            break;
        }
        let messages = &tail[group.start..group.end];
        let mut tools = vec![];
        let mut files = vec![];
        for message in messages {
            if role(message) == "tool" {
                tools.extend(parts(message).iter().filter(|p| kind(p) == "tool-result").cloned());
            }
            if role(message) == "user" {
                files.extend(parts(message).iter().filter(|p| kind(p) == "file").cloned());
            }
        }
        // OpenCode puts the group's media in a following user message; attach it to the group's last result.
        for (i, tool) in tools.iter().enumerate() {
            let flags = tool.get("providerOptions").and_then(|o| o.get(super::PROVIDER_ID));
            if flags.and_then(|f| f.get("toolAborted")) == Some(&Value::Bool(true)) {
                out.aborted = true;
            }
            let failed = flags.and_then(|f| f.get("toolError")) == Some(&Value::Bool(true));
            let last = i == tools.len() - 1;
            let id = tool.get("toolCallId").and_then(Value::as_str).unwrap_or_default().to_string();
            let result = convert_result(tool.get("output").unwrap_or(&Value::Null), if last { &files } else { &[] }, failed)?;
            out.results.insert(id, result);
        }
        out.end = start + group.end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn function(name: &str) -> Value {
        json!({ "type": "function", "name": name, "inputSchema": { "type": "object" } })
    }

    #[test]
    fn names_round_trip_within_the_limit() {
        let tools: Vec<Value> = ["read".to_string(), "resource_".repeat(15), "resource_".repeat(15) + "b", "mcp.example/search".into()]
            .iter()
            .map(|n| function(n))
            .collect();
        let mut names = std::collections::HashSet::new();
        for tool in &tools {
            let name = format!("{PREFIX}{}", short(tool_name(tool)));
            assert!(name.len() <= 64);
            assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
            assert_eq!(original(&name, &tools).as_deref(), Some(tool_name(tool)));
            names.insert(name);
        }
        assert_eq!(names.len(), tools.len());
        assert!(original("Bash", &tools).is_none());
        assert_eq!(short("mcp.example/search"), format!("mcp_example_search_{}", &hex::encode(Sha256::digest(b"mcp.example/search"))[..10]));
    }

    #[test]
    fn long_subagent_descriptions_keep_the_agent_list() {
        let description = description(&json!({
            "type": "function",
            "name": "subagent",
            "inputSchema": {},
            "description": "Long instructions. ".repeat(200) + "\n\nAvailable subagents:\n- explore: Explore the repository.\n- general: General tasks.",
        }));
        assert!(description.len() <= 2048);
        assert!(description.starts_with("Available subagents:"));
        assert!(description.contains("- general:"));
    }

    #[test]
    fn failures_stay_errors_and_json_stays_intact() {
        assert_eq!(
            convert_result(&json!({ "type": "json", "value": { "items": [1, { "enabled": true }] } }), &[], false).unwrap().mcp,
            json!({ "content": [{ "type": "text", "text": "{\"items\":[1,{\"enabled\":true}]}" }], "isError": false })
        );
        for output in [
            json!({ "type": "error-text", "value": "failed" }),
            json!({ "type": "error-json", "value": { "error": "failed" } }),
            json!({ "type": "execution-denied", "reason": "denied" }),
        ] {
            assert_eq!(convert_result(&output, &[], false).unwrap().mcp["isError"], json!(true));
        }
    }

    #[test]
    fn tool_media_stays_content_blocks() {
        let prompt = vec![
            json!({ "role": "tool", "content": [
                { "type": "tool-result", "toolCallId": "a", "toolName": "read", "output": { "type": "text", "value": "first" } },
                { "type": "tool-result", "toolCallId": "b", "toolName": "read", "output": { "type": "text", "value": "second" } },
            ] }),
            json!({ "role": "user", "content": [
                { "type": "text", "text": "Attached media from tool result:" },
                { "type": "file", "mediaType": "image/png", "data": "aW1hZ2U=" },
                { "type": "file", "mediaType": "application/pdf", "data": "cGRm" },
            ] }),
            json!({ "role": "user", "content": [{ "type": "text", "text": "New instructions" }] }),
        ];
        let result = collect(&prompt, 0).unwrap();
        assert_eq!(result.end, 2);
        assert_eq!(result.results["a"].mcp["content"], json!([{ "type": "text", "text": "first" }]));
        assert_eq!(
            result.results["b"].replacement,
            Some(vec![
                json!({ "type": "text", "text": "second" }),
                json!({ "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "aW1hZ2U=" } }),
                json!({ "type": "document", "source": { "type": "base64", "media_type": "application/pdf", "data": "cGRm" } }),
            ])
        );
        let image = convert_result(
            &json!({ "type": "content", "value": [{ "type": "image-data", "mediaType": "image/png", "data": "aW1hZ2U=" }] }),
            &[],
            false,
        )
        .unwrap();
        assert_eq!(image.mcp["content"], json!([{ "type": "image", "mimeType": "image/png", "data": "aW1hZ2U=" }]));
        assert!(image.replacement.is_none());
    }

    #[test]
    fn content_tags_history_and_rejects_unknown_media() {
        let blocks = content(&[
            json!({ "role": "system", "content": "sys" }),
            json!({ "role": "user", "content": [{ "type": "text", "text": "hi" }] }),
            json!({ "role": "assistant", "content": [{ "type": "tool-call", "toolCallId": "c", "toolName": "read", "input": { "a": 1 } }] }),
        ])
        .unwrap();
        assert_eq!(blocks[0], json!({ "type": "text", "text": "<user>" }));
        assert_eq!(blocks[4], json!({ "type": "text", "text": "<tool-call>\n{\"id\":\"c\",\"name\":\"read\",\"input\":{\"a\":1}}\n</tool-call>" }));
        let error = content(&[json!({ "role": "user", "content": [{ "type": "file", "mediaType": "text/plain", "data": "eA==" }] })]).unwrap_err();
        assert_eq!(error, "Claude Code does not support text/plain attachments");
        let url = content(&[
            json!({ "role": "user", "content": [{ "type": "file", "mediaType": "image/png", "data": { "$url": "https://example.com" } }] }),
        ])
        .unwrap();
        assert_eq!(url[0], json!({ "type": "image", "source": { "type": "url", "url": "https://example.com/" } }));
    }
}
