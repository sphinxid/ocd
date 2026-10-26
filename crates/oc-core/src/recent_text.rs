//! The verbatim part of a summary compaction (`core/session/recent-text.ts`).

use oc_schema::message::{AssistantContent, ToolState};
use oc_schema::types::{AttachmentSource, ToolContent};
use oc_schema::Message;
use serde_json::Value;

pub const TOOL_OUTPUT_MAX_CHARS: usize = 1_250;

/// `kept`: the text of the messages a summary compaction kept verbatim. History rebuilds it from
/// `metadata.keepID`; compactions written before that reference carry the text they stored.
pub fn kept(message: &Message) -> &str {
    match message {
        Message::Compaction(c) => c.metadata.as_ref().and_then(|m| m.get("recent")).and_then(Value::as_str).unwrap_or(""),
        _ => "",
    }
}

/// `messageToText`: one message as the recent, verbatim part of a summary shows it. Empty for messages that
/// part leaves out.
pub fn message_to_text(message: &Message) -> String {
    match message {
        // Earlier summaries and instruction updates are handled outside the recent part.
        Message::Compaction(_) | Message::System { .. } => String::new(),
        Message::User { text, files, skills, .. } => {
            let mut lines: Vec<String> = skills
                .iter()
                .flatten()
                .filter_map(|s| s.text.as_ref().map(|t| format!("[Skill activated: {}]\n{}", s.name, t)))
                .collect();
            lines.push(format!("[User]: {text}"));
            for f in files.iter().flatten() {
                let name = f.name.clone().unwrap_or_else(|| match &f.source {
                    AttachmentSource::Uri { uri } => uri.clone(),
                    AttachmentSource::Inline => "inline attachment".into(),
                });
                lines.push(format!("[Attached {}: {}]", f.mime, name));
            }
            lines.join("\n")
        }
        Message::LocationSwitched { location, .. } => {
            format!("[User]: The working directory has been changed to {}.", location.directory)
        }
        Message::Assistant(a) => a
            .content
            .iter()
            .flat_map(|part| -> Vec<String> {
                match part {
                    AssistantContent::Text { text, .. } => vec![format!("[Assistant]: {text}")],
                    // Providers return summaries of the real reasoning, and written out as text it can read like
                    // an attempt to extract the model's thinking.
                    AssistantContent::Reasoning { .. } => vec![],
                    AssistantContent::Tool { name, state, .. } => {
                        let input = match state {
                            ToolState::Streaming { input } => input.clone(),
                            ToolState::Running { input, .. }
                            | ToolState::Completed { input, .. }
                            | ToolState::Error { input, .. } => oc_ai::json::encode(&Value::Object(input.clone())),
                        };
                        let call = format!("[Assistant tool call]: {name}({input})");
                        match state {
                            ToolState::Completed { content, .. } => {
                                vec![call, format!("[Tool result]: {}", truncate_tool_output(&serialize_tool_content(content)))]
                            }
                            ToolState::Error { error, .. } => vec![call, format!("[Tool error]: {}", error.message)],
                            _ => vec![call],
                        }
                    }
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Message::Synthetic { text, .. } => format!("[Synthetic context]: {text}"),
        Message::Skill { name, text, .. } => format!("[Skill activated: {name}]\n{text}"),
        Message::Shell { metadata, command, output, .. } => {
            if metadata.as_ref().and_then(|m| m.get("background")) == Some(&Value::Bool(true)) {
                return String::new();
            }
            format!(
                "[Shell]: {command}\n{}",
                truncate_tool_output(output.as_ref().map(|o| o.output.as_str()).unwrap_or(""))
            )
        }
        _ => String::new(),
    }
}

/// `truncateToolOutput`: more than 1,250 UTF-16 units keeps the first 1,250 code points plus `\n[truncated]`.
pub fn truncate_tool_output(value: &str) -> String {
    if value.encode_utf16().count() <= TOOL_OUTPUT_MAX_CHARS {
        return value.to_string();
    }
    let end = value.char_indices().nth(TOOL_OUTPUT_MAX_CHARS).map(|(i, _)| i).unwrap_or(value.len());
    if end == value.len() {
        return value.to_string();
    }
    format!("{}\n[truncated]", &value[..end])
}

/// `serializeToolContent`
pub fn serialize_tool_content(content: &[ToolContent]) -> String {
    content
        .iter()
        .map(|item| match item {
            ToolContent::Text { text } => text.clone(),
            ToolContent::File { mime, name, .. } => match name {
                None => format!("[Attached {mime}]"),
                Some(n) => format!("[Attached {mime}: {n}]"),
            },
        })
        .collect::<Vec<_>>()
        .join("\n")
}
