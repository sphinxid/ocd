//! `grep` (`core/src/tool/plugin/grep.ts`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::file_access;
use crate::ripgrep::{GrepInput, Ripgrep, RipgrepError};
use crate::tool::{DefinitionContext, ErrorCause, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, decode, meta};
use crate::util;
use crate::validate::{Issue, PathSegment};

pub const DEFAULT_SEARCH_LIMIT: usize = 100;
pub const DEFAULT_SEARCH_TIMEOUT_MS: u64 = 30_000;
pub const DESCRIPTION: &str = "Search file contents using ripgrep's regular expression syntax or literal text matching. Use it to locate specific code, symbols, or text patterns, and narrow searches with `path` or `include`. Returns matching file paths, line numbers, and line previews.";

pub(crate) fn timeout_error() -> ToolError {
    ToolError::new(format!(
        "Search timed out after {} seconds. Consider using a more specific path or pattern.",
        DEFAULT_SEARCH_TIMEOUT_MS / 1_000
    ))
}

/// One formatted match (paths absolute).
pub struct DisplayMatch {
    pub path: String,
    pub line: u64,
    pub text: String,
}

/// `toModelContent(matches, truncated)`.
pub fn to_model_content(matches: &[DisplayMatch], truncated: bool) -> String {
    let mut lines = vec![if matches.is_empty() { "No matches found".to_string() } else { format!("Found {} matches", matches.len()) }];
    let mut current = String::new();
    for m in matches {
        if current != m.path {
            if !current.is_empty() {
                lines.push(String::new());
            }
            current = m.path.clone();
            lines.push(format!("{}:", m.path));
        }
        lines.push(format!("  Line {}: {}", m.line, m.text));
    }
    if truncated {
        lines.push(String::new());
        lines.push(format!(
            "(Results are truncated: showing first {} results. Consider using a more specific path or pattern.)",
            matches.len()
        ));
    }
    lines.join("\n")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Input {
    pattern: String,
    path: Option<String>,
    include: Option<String>,
    literal: Option<bool>,
    case_sensitive: Option<bool>,
    limit: Option<usize>,
}

pub struct GrepTool {
    ripgrep: Arc<Ripgrep>,
}

impl GrepTool {
    pub fn new(ripgrep: Arc<Ripgrep>) -> Self {
        Self { ripgrep }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::direct()
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        super::definition("grep", DESCRIPTION, include_str!("../../schemas/grep.json"))
    }

    fn validate(&self, input: &Value) -> Vec<Issue> {
        let schema = self.definition(&DefinitionContext::default()).input_schema;
        crate::validate::validate(&schema, input, crate::validate::Flavor::Effect)
            .into_iter()
            .map(|mut issue| {
                if issue.path == [PathSegment::Key("pattern".into())] && issue.message == "Expected a value with a length of at least 1" {
                    issue.message = "Pattern must not be empty".into();
                }
                issue
            })
            .collect()
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let input: Input = decode(input)?;
        let leaf = format!("Unable to grep for {}", input.pattern);
        let target = file_access::resolve(&ctx, input.path.as_deref().unwrap_or("."), None);
        file_access::authorize_external(&ctx, &[&target], None).await.map_err(super::perm(leaf.clone()))?;
        let mut metadata = meta(vec![("root", json!("."))]);
        for (key, value) in [
            ("path", input.path.as_ref().map(|v| json!(v))),
            ("include", input.include.as_ref().map(|v| json!(v))),
            ("literal", input.literal.map(|v| json!(v))),
            ("caseSensitive", input.case_sensitive.map(|v| json!(v))),
            ("limit", input.limit.map(|v| json!(v))),
        ] {
            if let Some(value) = value {
                metadata.insert(key.into(), value);
            }
        }
        super::assert(&ctx, "grep", vec![input.pattern.clone()], Some(vec!["*".into()]), Some(metadata))
            .await
            .map_err(super::perm(leaf.clone()))?;
        let root = target.absolute.clone();
        let kind = match std::fs::metadata(&root) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::new(format!("Search path does not exist: {}", input.path.as_deref().unwrap_or("."))));
            }
            Err(e) => return Err(ToolError::with_cause(leaf, ErrorCause::Error(e.to_string()))),
        };
        let is_dir = kind.is_dir();
        let cwd = if is_dir { root.clone() } else { util::dirname(&root) };
        let file_name = (!is_dir).then(|| util::basename(&root.to_string_lossy()));
        let limit = input.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
        let search = self.ripgrep.grep(GrepInput {
            cwd: &cwd,
            pattern: &input.pattern,
            file: file_name.as_deref(),
            include: input.include.as_deref(),
            literal: input.literal == Some(true),
            case_sensitive: input.case_sensitive,
            limit: limit + 1,
        });
        let matches = match tokio::time::timeout(Duration::from_millis(DEFAULT_SEARCH_TIMEOUT_MS), search).await {
            Err(_) => return Err(timeout_error()),
            Ok(Err(RipgrepError::InvalidPattern(message))) => return Err(ToolError::new(format!("Invalid regex pattern: {message}"))),
            Ok(Err(RipgrepError::Failed(message))) => return Err(ToolError::with_cause(leaf, ErrorCause::Error(message))),
            Ok(Ok(matches)) => matches,
        };
        let truncated = matches.len() > limit;
        let matches: Vec<_> = matches.into_iter().take(limit).collect();
        let mut output = Vec::new();
        let mut display = Vec::new();
        for m in &matches {
            let absolute = util::resolve(&cwd, &m.path);
            let relative = util::relative(&ctx.directory, &absolute);
            output.push(json!({
                "entry": {"path": relative, "type": "file"},
                "line": m.line,
                "offset": m.offset,
                "text": m.text,
                "submatches": m.submatches.iter().map(|s| json!({"text": s.text, "start": s.start, "end": s.end})).collect::<Vec<_>>(),
            }));
            display.push(DisplayMatch {
                path: util::resolve(&ctx.directory, &relative).to_string_lossy().into_owned(),
                line: m.line,
                text: m.text.clone(),
            });
        }
        Ok(ToolResult::text(to_model_content(&display, truncated))
            .with_output(Value::Array(output))
            .with_metadata(meta(vec![("matches", json!(matches.len())), ("truncated", json!(truncated))])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_groups() {
        let m = |p: &str, l: u64, t: &str| DisplayMatch { path: p.into(), line: l, text: t.into() };
        let text = to_model_content(&[m("/r/a.ts", 1, "x\n"), m("/r/a.ts", 3, "y\n"), m("/r/b.ts", 2, "z\n")], true);
        assert_eq!(
            text,
            "Found 3 matches\n/r/a.ts:\n  Line 1: x\n\n  Line 3: y\n\n\n/r/b.ts:\n  Line 2: z\n\n\n(Results are truncated: showing first 3 results. Consider using a more specific path or pattern.)"
        );
        assert_eq!(to_model_content(&[], false), "No matches found");
    }

    #[test]
    fn empty_pattern_message() {
        let tool = GrepTool::new(Arc::new(Ripgrep::new("/tmp".into(), reqwest::Client::new())));
        let issues = tool.validate(&json!({"pattern": "", "limit": 0}));
        let texts: Vec<_> = issues.iter().map(|i| (i.path_text(), i.message.clone())).collect();
        assert_eq!(
            texts,
            vec![("pattern".into(), "Pattern must not be empty".into()), ("limit".into(), "Expected a value greater than 0".into())]
        );
    }
}
