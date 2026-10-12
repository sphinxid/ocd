//! `patch` (`core/src/tool/plugin/patch.ts`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::file_access::{self, Kind, Target};
use crate::file_diff::{FileDiff, file_diff};
use crate::file_mutation::{self, FileError};
use crate::formatter::Formatter;
use crate::patch::{self as grammar, Hunk};
use crate::tool::{DefinitionContext, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, decode, meta};
use crate::util::{bom, js};

pub const DESCRIPTION: &str = include_str!("../../../../assets/tool/patch.txt");

/// Session hook: `gpt-` models (except `oss`/`gpt-4`) use `patch`; others use `edit`/`write`.
pub fn use_patch(ctx: &DefinitionContext) -> bool {
    match &ctx.model_id {
        Some(id) => id.contains("gpt-") && !id.contains("oss") && !id.contains("gpt-4"),
        None => false,
    }
}

/// True when the request context has a model (hooks only run for real requests).
fn hooked(ctx: &DefinitionContext) -> bool {
    ctx.model_id.is_some()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Input {
    patch_text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Add,
    Delete,
    Update,
}

impl Op {
    fn as_str(&self) -> &'static str {
        match self {
            Op::Add => "add",
            Op::Delete => "delete",
            Op::Update => "update",
        }
    }
}

struct Prepared {
    op: Op,
    target: Target,
    content: String,
    before: String,
    after: String,
    move_target: Option<Target>,
}

struct Applied {
    op: Op,
    resource: String,
    target: PathBuf,
}

pub struct PatchTool {
    formatter: Arc<Formatter>,
}

impl PatchTool {
    pub fn new(formatter: Arc<Formatter>) -> Self {
        Self { formatter }
    }
}

/// Remove the common leading indentation of diff body lines.
pub fn trim_diff(diff: &str) -> String {
    let lines: Vec<&str> = diff.split('\n').collect();
    let is_body = |line: &str| (line.starts_with('+') || line.starts_with('-') || line.starts_with(' ')) && !line.starts_with("---") && !line.starts_with("+++");
    let content: Vec<&str> = lines.iter().copied().filter(|l| is_body(l)).collect();
    if content.is_empty() {
        return diff.to_string();
    }
    let mut indent = usize::MAX;
    for line in &content {
        let value = &line[1..];
        if js::trim(value).is_empty() {
            continue;
        }
        let leading = value.chars().take_while(|c| js::is_space(*c)).map(char::len_utf16).sum::<usize>();
        indent = indent.min(leading);
    }
    if indent == usize::MAX || indent == 0 {
        return diff.to_string();
    }
    lines
        .iter()
        .map(|line| {
            if is_body(line) {
                let rest = &line[1..];
                let skip = js::utf16_prefix_end(rest, indent);
                format!("{}{}", &line[..1], &rest[skip..])
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn patch_file(change: &Prepared, after: Option<&str>) -> FileDiff {
    let resource = change.move_target.as_ref().map(|t| t.resource.clone()).unwrap_or_else(|| change.target.resource.clone());
    let status = match change.op {
        Op::Add => "added",
        Op::Delete => "deleted",
        Op::Update => "modified",
    };
    let diff = file_diff(&change.target.absolute.to_string_lossy(), &change.before, after.unwrap_or(&change.after), status);
    FileDiff { file: resource, patch: trim_diff(&diff.patch), ..diff }
}

fn verification(message: impl Into<String>) -> ToolError {
    ToolError::new(format!("patch verification failed: {}", message.into()))
}

impl PatchTool {
    async fn resolve_target(&self, ctx: &ToolContext, value: &str) -> Result<Target, ToolError> {
        let target = file_access::resolve(ctx, value, Some(Kind::File));
        if let Some(external) = &target.external_directory {
            let metadata = meta(vec![
                ("filepath", json!(target.absolute.to_string_lossy())),
                ("parentDir", json!(external.directory.to_string_lossy())),
            ]);
            file_access::authorize_external(ctx, &[&target], Some(metadata))
                .await
                .map_err(super::perm(format!("Unable to prepare patch at {value}")))?;
        }
        Ok(target)
    }

    async fn run(&self, hunks: Vec<Hunk>, ctx: &ToolContext, applied: &mut Vec<Applied>) -> Result<Vec<FileDiff>, ToolError> {
        let mut prepared: Vec<Prepared> = Vec::new();
        let mut updates: HashMap<PathBuf, String> = HashMap::new();
        for hunk in &hunks {
            let target = self.resolve_target(ctx, hunk.path()).await?;
            match hunk {
                Hunk::Add { contents, .. } => {
                    let content = if contents.ends_with('\n') || contents.is_empty() { contents.clone() } else { format!("{contents}\n") };
                    let after = bom::split(&content).text;
                    prepared.push(Prepared { op: Op::Add, target, content, before: String::new(), after, move_target: None });
                }
                Hunk::Delete { .. } => {
                    let text = file_mutation::read_text(&target.absolute).map_err(|e| {
                        verification(format!("Failed to delete {}: {}", target.resource, e.patch_message()))
                    })?;
                    prepared.push(Prepared { op: Op::Delete, target, content: String::new(), before: text.text, after: String::new(), move_target: None });
                }
                Hunk::Update { path, move_path, chunks } => {
                    let original = match updates.get(&target.absolute) {
                        Some(previous) => previous.clone(),
                        None => {
                            let text = file_mutation::read_text(&target.absolute).map_err(|e| {
                                verification(format!(
                                    "Failed to read file to update {}: {}",
                                    target.absolute.to_string_lossy(),
                                    e.patch_message()
                                ))
                            })?;
                            bom::join(&text.text, text.bom)
                        }
                    };
                    let before = bom::split(&original).text;
                    let update = grammar::derive(path, chunks, &original).map_err(verification)?;
                    let move_target = match move_path {
                        Some(dest) => Some(self.resolve_target(ctx, dest).await?),
                        None => None,
                    };
                    let content = grammar::join_bom(&update.content, update.bom);
                    if move_target.is_none() {
                        updates.insert(target.absolute.clone(), content.clone());
                    }
                    prepared.push(Prepared { op: Op::Update, target, content, before, after: update.content, move_target });
                }
            }
        }

        let patch_files: Vec<FileDiff> = prepared.iter().map(|c| patch_file(c, None)).collect();
        let targets: Vec<&Target> = prepared.iter().flat_map(|c| std::iter::once(&c.target).chain(c.move_target.iter())).collect();
        let mut resources: Vec<String> = Vec::new();
        for t in &targets {
            if !resources.contains(&t.resource) {
                resources.push(t.resource.clone());
            }
        }
        let metadata = meta(vec![
            ("filepath", json!(targets.iter().map(|t| t.resource.as_str()).collect::<Vec<_>>().join(", "))),
            ("diff", json!(patch_files.iter().map(|f| format!("{}\n", f.patch)).collect::<String>())),
            ("files", Value::Array(patch_files.iter().map(FileDiff::to_json).collect())),
        ]);
        super::assert(ctx, "edit", resources, Some(vec!["*".into()]), Some(metadata)).await.map_err(super::perm("Unable to apply patch"))?;

        let fail = |operation: String, error: FileError, applied: &Vec<Applied>| {
            let completed = applied.iter().map(|a| a.resource.as_str()).collect::<Vec<_>>().join(", ");
            ToolError::new(format!(
                "{operation}: {}{}",
                error.patch_message(),
                if completed.is_empty() { String::new() } else { format!(". Completed before failure: {completed}") }
            ))
        };
        for change in &prepared {
            match (change.op, &change.move_target) {
                (Op::Delete, _) => {
                    file_mutation::remove(&change.target.absolute)
                        .map_err(|e| fail(format!("Failed to delete {}", change.target.resource), e, applied))?;
                    applied.push(Applied { op: change.op, resource: change.target.resource.clone(), target: change.target.absolute.clone() });
                }
                (Op::Update, Some(dest)) => {
                    file_mutation::write_bytes(&dest.absolute, change.content.as_bytes())
                        .map_err(|e| fail(format!("Failed to write {}", dest.resource), e, applied))?;
                    file_mutation::remove(&change.target.absolute).map_err(|e| {
                        fail(format!("Wrote {} but failed to remove {}", dest.resource, change.target.resource), e, applied)
                    })?;
                    applied.push(Applied { op: change.op, resource: dest.resource.clone(), target: dest.absolute.clone() });
                }
                _ => {
                    file_mutation::write_bytes(&change.target.absolute, change.content.as_bytes())
                        .map_err(|e| fail(format!("Failed to write {}", change.target.resource), e, applied))?;
                    applied.push(Applied { op: change.op, resource: change.target.resource.clone(), target: change.target.absolute.clone() });
                }
            }
        }
        let mut formatted: HashMap<PathBuf, String> = HashMap::new();
        let mut distinct: Vec<PathBuf> = Vec::new();
        for item in applied.iter().filter(|a| a.op != Op::Delete) {
            if !distinct.contains(&item.target) {
                distinct.push(item.target.clone());
            }
        }
        for target in distinct {
            let display = target.to_string_lossy().into_owned();
            let current = file_mutation::read_text(&target).map_err(|e| fail(format!("Failed to read {display}"), e, applied))?;
            let text = if super::format_file(&self.formatter, ctx, &target).await {
                file_mutation::sync_text_bom(&target, current.bom).map_err(|e| fail(format!("Failed to sync {display}"), e, applied))?
            } else {
                current.text
            };
            formatted.insert(target, text);
        }
        let changed: Vec<PathBuf> = applied.iter().map(|a| a.target.clone()).collect();
        ctx.host.files_changed(&ctx.session_id, &changed).await;
        Ok(prepared
            .iter()
            .map(|change| {
                if change.op == Op::Delete {
                    return patch_file(change, None);
                }
                let target = change.move_target.as_ref().unwrap_or(&change.target);
                patch_file(change, formatted.get(&target.absolute).map(String::as_str))
            })
            .collect())
    }
}

#[async_trait]
impl Tool for PatchTool {
    fn name(&self) -> &str {
        "patch"
    }

    fn options(&self) -> ToolOptions {
        ToolOptions { permission: Some("edit".into()), ..ToolOptions::direct() }
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        super::definition("patch", DESCRIPTION, include_str!("../../schemas/patch.json"))
    }

    fn advertised(&self, ctx: &DefinitionContext) -> bool {
        !hooked(ctx) || use_patch(ctx)
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let input: Input = decode(input)?;
        let parsed = grammar::parse(&input.patch_text);
        let mut locks: Vec<PathBuf> = Vec::new();
        if let Ok(hunks) = &parsed {
            for hunk in hunks {
                locks.push(file_access::resolve_path(&ctx.directory, hunk.path(), &ctx.global.home));
                if let Hunk::Update { move_path: Some(dest), .. } = hunk {
                    locks.push(file_access::resolve_path(&ctx.directory, dest, &ctx.global.home));
                }
            }
        }
        let _guard = file_mutation::with_lock(&locks).await;
        if input.patch_text.is_empty() {
            return Err(ToolError::new("patchText is required"));
        }
        let hunks = parsed.map_err(|e| ToolError::new(format!("patch verification failed: {e}")))?;
        if hunks.is_empty() {
            return Err(ToolError::new("patch rejected: empty patch"));
        }
        let mut applied: Vec<Applied> = Vec::new();
        let files = self.run(hunks, &ctx, &mut applied).await?;
        let files_json: Vec<Value> = files.iter().map(FileDiff::to_json).collect();
        let mut lines = vec!["Success. Updated the following files:".to_string()];
        for item in &applied {
            let letter = match item.op {
                Op::Add => "A",
                Op::Delete => "D",
                Op::Update => "M",
            };
            lines.push(format!("{letter} {}", item.resource));
        }
        let applied_json: Vec<Value> = applied
            .iter()
            .map(|a| json!({"type": a.op.as_str(), "resource": a.resource, "target": a.target.to_string_lossy()}))
            .collect();
        Ok(ToolResult {
            output: Some(json!({"applied": applied_json, "files": files_json})),
            content: vec![oc_schema::ToolContent::Text { text: lines.join("\n") }],
            metadata: Some(meta(vec![("files", Value::Array(files_json))])),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_common_indent() {
        let diff = "Index: a\n===\n--- a\n+++ a\n@@ -1,2 +1,2 @@\n     x\n-    y\n+    z\n";
        assert_eq!(trim_diff(diff), "Index: a\n===\n--- a\n+++ a\n@@ -1,2 +1,2 @@\n x\n-y\n+z\n");
        assert_eq!(trim_diff("@@\n x\n"), "@@\n x\n");
    }

    #[test]
    fn model_selection_hook() {
        let ctx = |m: &str| DefinitionContext { model_id: Some(m.into()), ..Default::default() };
        assert!(use_patch(&ctx("gpt-5")));
        assert!(!use_patch(&ctx("gpt-4o")));
        assert!(!use_patch(&ctx("gpt-oss-120b")));
        assert!(!use_patch(&ctx("claude-sonnet")));
        let tool = PatchTool::new(Arc::new(Formatter::new()));
        assert!(tool.advertised(&DefinitionContext::default()));
        assert!(!tool.advertised(&ctx("claude")));
        assert!(tool.advertised(&ctx("gpt-5-codex")));
    }
}
