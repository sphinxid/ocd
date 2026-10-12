//! `glob` (`core/src/tool/plugin/glob.ts`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::grep::{DEFAULT_SEARCH_LIMIT, DEFAULT_SEARCH_TIMEOUT_MS, timeout_error};
use crate::file_access::{self, Kind};
use crate::ripgrep::{GlobInput, Ripgrep};
use crate::tool::{DefinitionContext, ErrorCause, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, decode, meta};
use crate::util;

pub const DESCRIPTION: &str = "Search file paths using a glob pattern (examples: \"**/*.ts\", \"src/**/*.tsx\").";

/// `toModelContent(entries, truncated)`.
pub fn to_model_content(paths: &[String], truncated: bool) -> String {
    let mut lines: Vec<String> = if paths.is_empty() { vec!["No files found".into()] } else { paths.to_vec() };
    if truncated {
        lines.push(String::new());
        lines.push(format!(
            "(Results are truncated: showing first {} results. Consider using a more specific path or pattern.)",
            paths.len()
        ));
    }
    lines.join("\n")
}

#[derive(Deserialize)]
struct Input {
    pattern: String,
    path: Option<String>,
    hidden: Option<bool>,
    limit: Option<usize>,
}

pub struct GlobTool {
    ripgrep: Arc<Ripgrep>,
}

impl GlobTool {
    pub fn new(ripgrep: Arc<Ripgrep>) -> Self {
        Self { ripgrep }
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::direct()
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        super::definition("glob", DESCRIPTION, include_str!("../../schemas/glob.json"))
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let input: Input = decode(input)?;
        let leaf = format!("Unable to find files matching {}", input.pattern);
        let search_path = input.path.clone().filter(|p| p != "undefined" && p != "null");
        let shown = search_path.clone().unwrap_or_else(|| ".".into());
        let target = file_access::resolve(&ctx, &shown, Some(Kind::Directory));
        file_access::authorize_external(&ctx, &[&target], None).await.map_err(super::perm(leaf.clone()))?;
        let mut metadata = meta(vec![("root", json!(shown))]);
        if let Some(p) = &search_path {
            metadata.insert("path".into(), json!(p));
        }
        if let Some(h) = input.hidden {
            metadata.insert("hidden".into(), json!(h));
        }
        if let Some(l) = input.limit {
            metadata.insert("limit".into(), json!(l));
        }
        super::assert(&ctx, "glob", vec![input.pattern.clone()], Some(vec!["*".into()]), Some(metadata))
            .await
            .map_err(super::perm(leaf.clone()))?;
        match std::fs::metadata(&target.absolute) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(ToolError::new(format!("Search path is not a directory: {shown}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::new(format!("Search path does not exist: {shown}")));
            }
            Err(e) => return Err(ToolError::with_cause(leaf, ErrorCause::Error(e.to_string()))),
        }
        let root = target.absolute.clone();
        let limit = input.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
        let search = self.ripgrep.glob(GlobInput { cwd: &root, pattern: &input.pattern, limit: limit + 1, hidden: input.hidden == Some(true), follow: false });
        let entries = match tokio::time::timeout(Duration::from_millis(DEFAULT_SEARCH_TIMEOUT_MS), search).await {
            Err(_) => return Err(timeout_error()),
            Ok(Err(error)) => return Err(ToolError::with_cause(leaf, ErrorCause::Error(error.to_string()))),
            Ok(Ok(entries)) => entries,
        };
        let truncated = entries.len() > limit;
        let relative: Vec<String> =
            entries.iter().take(limit).map(|e| util::relative(&ctx.directory, &util::resolve(&root, e))).collect();
        let absolute: Vec<String> = relative.iter().map(|r| util::resolve(&ctx.directory, r).to_string_lossy().into_owned()).collect();
        Ok(ToolResult::text(to_model_content(&absolute, truncated))
            .with_output(Value::Array(relative.iter().map(|p| json!({"path": p, "type": "file"})).collect()))
            .with_metadata(meta(vec![("count", json!(relative.len())), ("truncated", json!(truncated))])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(to_model_content(&[], false), "No files found");
        assert_eq!(
            to_model_content(&["/a".into(), "/b".into()], true),
            "/a\n/b\n\n(Results are truncated: showing first 2 results. Consider using a more specific path or pattern.)"
        );
    }
}
