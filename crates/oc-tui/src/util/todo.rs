//! The experimental todo list (`util/todo.ts`, `component/todo-item.tsx`): parsing `todowrite` input,
//! the status markers, the session's current list and one rendered item.

use serde_json::Value;

use crate::api::types::{Content, Message};
use crate::theme::Theme;
use crate::ui::text::*;

#[derive(Clone, Debug, PartialEq)]
pub struct Todo {
    pub content: String,
    /// `pending` | `in_progress` | `completed` | `cancelled`.
    pub status: &'static str,
}

const STATUSES: [&str; 4] = ["pending", "in_progress", "completed", "cancelled"];

/// `parseTodos`: items with a non-empty string `content`; an unknown status reads as `pending`.
pub fn parse_todos(value: &Value) -> Vec<Todo> {
    let Some(items) = value.as_array() else { return vec![] };
    items
        .iter()
        .filter_map(|item| {
            let content = item.as_object()?.get("content")?.as_str().filter(|c| !c.is_empty())?;
            let status = item.get("status").and_then(|s| s.as_str()).and_then(|s| STATUSES.iter().find(|x| **x == s)).copied().unwrap_or("pending");
            Some(Todo { content: content.to_string(), status })
        })
        .collect()
}

/// `todoMarker`.
pub fn todo_marker(status: &str, ascii: bool) -> &'static str {
    match status {
        "completed" if ascii => "[x]",
        "completed" => "[✓]",
        "in_progress" if ascii => "[>]",
        "in_progress" => "[•]",
        "cancelled" => "[-]",
        _ => "[ ]",
    }
}

/// `sessionTodos`: the input of the latest completed `todowrite` call before the revert boundary.
pub fn session_todos(messages: &[Message], boundary: Option<&str>) -> Vec<Todo> {
    let end = boundary.and_then(|b| messages.iter().position(|m| m.id == b)).unwrap_or(messages.len());
    let tool = messages[..end]
        .iter()
        .filter(|m| m.kind == "assistant")
        .flat_map(|m| m.content.iter())
        .filter_map(|c| match c {
            Content::Tool(t) if t.name == "todowrite" && t.state.status == "completed" => Some(t),
            _ => None,
        })
        .next_back();
    tool.map(|t| parse_todos(t.state.input.get("todos").unwrap_or(&Value::Null))).unwrap_or_default()
}

/// `TodoItem`: the marker, then the content word-wrapped beside it. In progress uses the warning color,
/// cancelled the faint color with strikethrough, the rest muted.
pub fn todo_item(t: &Theme, todo: &Todo, width: usize) -> Vec<SLine> {
    let color = match todo.status {
        "in_progress" => t.text.feedback.warning.base,
        "cancelled" => t.text.faint,
        _ => t.text.muted,
    };
    let marker = format!("{} ", todo_marker(todo.status, false));
    let mw = width_of(&marker);
    let mut style = fg(color);
    if todo.status == "cancelled" {
        style = style.add_modifier(ratatui::style::Modifier::CROSSED_OUT);
    }
    let lines = wrap(&[Seg::new(todo.content.clone(), style)], width.saturating_sub(mw).max(1), Wrap::Word);
    lines
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            let head = if i == 0 { Seg::new(marker.clone(), fg(color)) } else { Seg::new(" ".repeat(mw), Default::default()) };
            SLine::new(l).prefixed(&[head])
        })
        .collect()
}

fn width_of(s: &str) -> usize {
    crate::util::locale::width(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_todos() {
        let todos = parse_todos(&json!([
            { "content": "a", "status": "completed" },
            { "content": "", "status": "pending" },
            { "content": 1, "status": "pending" },
            "x",
            { "content": "b", "status": "weird" },
            { "content": "c" },
        ]));
        assert_eq!(
            todos,
            vec![
                Todo { content: "a".into(), status: "completed" },
                Todo { content: "b".into(), status: "pending" },
                Todo { content: "c".into(), status: "pending" },
            ]
        );
        assert!(parse_todos(&json!({ "content": "a" })).is_empty());
    }

    #[test]
    fn markers() {
        assert_eq!(["completed", "in_progress", "cancelled", "pending"].map(|s| todo_marker(s, false)), ["[✓]", "[•]", "[-]", "[ ]"]);
        assert_eq!(["completed", "in_progress", "cancelled", "pending"].map(|s| todo_marker(s, true)), ["[x]", "[>]", "[-]", "[ ]"]);
    }

    fn assistant(id: &str, tools: Value) -> Message {
        serde_json::from_value(json!({ "id": id, "type": "assistant", "content": tools })).unwrap()
    }

    fn call(status: &str, todos: Value) -> Value {
        json!({ "type": "tool", "id": "t", "name": "todowrite", "state": { "status": status, "input": { "todos": todos } } })
    }

    #[test]
    fn latest_completed_before_revert() {
        let messages = vec![
            assistant("msg_1", json!([call("completed", json!([{ "content": "one", "status": "pending" }]))])),
            assistant("msg_2", json!([call("completed", json!([{ "content": "two", "status": "completed" }])), call("error", json!([]))])),
            assistant("msg_3", json!([call("completed", json!([]))])),
            assistant("msg_4", json!([call("running", json!([{ "content": "four", "status": "pending" }]))])),
        ];
        assert!(session_todos(&messages, None).is_empty());
        assert_eq!(session_todos(&messages, Some("msg_3"))[0].content, "two");
        assert_eq!(session_todos(&messages, Some("msg_2"))[0].content, "one");
        assert!(session_todos(&messages, Some("msg_1")).is_empty());
        assert!(session_todos(&messages, Some("msg_missing")).is_empty());
    }
}
