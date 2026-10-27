//! `todowrite` (`core/src/tool/plugin/todo.ts`, `core/src/tool/todowrite.txt`), the experimental todo list.
//!
//! Registered by the built-in plugin `opencode.tool.todo`; the core keeps it out of a location's tools unless
//! that location's config sets `experimental.todo: true`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::tool::{DefinitionContext, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, decode, meta};

pub const NAME: &str = "todowrite";

pub const DESCRIPTION: &str = include_str!("../../../../assets/tool/todowrite.txt");

/// `TodoTool.Info`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Todo {
    pub content: String,
    pub status: String,
}

#[derive(Deserialize)]
struct Input {
    todos: Vec<Todo>,
}

/// `TodoTool.summarize`.
pub fn summarize(todos: &[Todo]) -> String {
    if todos.is_empty() {
        return "Todos cleared".into();
    }
    let count = |status: &str| todos.iter().filter(|todo| todo.status == status).count();
    let mut counts = vec![format!("{}/{} completed", count("completed"), todos.len())];
    for status in ["in_progress", "pending", "cancelled"] {
        let n = count(status);
        if n > 0 {
            counts.push(format!("{n} {}", status.replacen('_', " ", 1)));
        }
    }
    let active: Vec<String> = todos
        .iter()
        .filter(|todo| todo.status == "in_progress")
        .map(|todo| serde_json::to_string(&todo.content).unwrap_or_default())
        .collect();
    let active = if active.is_empty() { String::new() } else { format!(". In progress: {}", active.join(", ")) };
    format!("Todos updated: {}{active}", counts.join(", "))
}

pub struct TodoTool;

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        NAME
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::direct()
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        super::definition(NAME, DESCRIPTION, include_str!("../../schemas/todowrite.json"))
    }

    async fn execute(&self, input: serde_json::Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let input: Input = decode(input)?;
        super::assert(&ctx, NAME, vec!["*".into()], Some(vec!["*".into()]), None).await.map_err(super::perm("Unable to update todos"))?;
        let todos = json!(input.todos);
        Ok(ToolResult::text(summarize(&input.todos))
            .with_output(json!({ "todos": todos }))
            .with_metadata(meta(vec![("todos", todos)])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(content: &str, status: &str) -> Todo {
        Todo { content: content.into(), status: status.into() }
    }

    #[test]
    fn summarizes() {
        let todos = [
            todo("Write tests", "in_progress"),
            todo("Ship it", "pending"),
            todo("Read the code", "completed"),
            todo("Rewrite everything", "cancelled"),
        ];
        assert_eq!(summarize(&todos), "Todos updated: 1/4 completed, 1 in progress, 1 pending, 1 cancelled. In progress: \"Write tests\"");
        assert_eq!(summarize(&[]), "Todos cleared");
        assert_eq!(summarize(&[todo("a", "completed")]), "Todos updated: 1/1 completed");
        assert_eq!(
            summarize(&[todo("say \"hi\"\n", "in_progress"), todo("b", "in_progress")]),
            "Todos updated: 0/2 completed, 2 in progress. In progress: \"say \\\"hi\\\"\\n\", \"b\""
        );
    }

    #[test]
    fn definition_matches_upstream() {
        let definition = TodoTool.definition(&DefinitionContext::default());
        assert_eq!(definition.name, "todowrite");
        assert!(definition.description.starts_with("Create and update a todo list for the current session."));
        assert_eq!(definition.input_schema.pointer("/properties/todos/type"), Some(&json!("array")));
        assert_eq!(definition.input_schema.pointer("/properties/todos/items/required"), Some(&json!(["content", "status"])));
        assert!(!TodoTool.options().codemode);
    }
}
