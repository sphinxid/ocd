//! `edit` (`core/src/tool/plugin/edit.ts`): exact replace with a three-step matcher cascade.

use std::sync::Arc;

use async_trait::async_trait;
use oc_schema::ToolContent;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::file_access::{self, Kind};
use crate::file_diff::{FileDiff, file_diff};
use crate::file_mutation::{self, FileError};
use crate::formatter::Formatter;
use crate::patch::normalize_unicode;
use crate::tool::{DefinitionContext, ErrorCause, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, decode, meta};
use crate::util::{bom, js};

pub const DESCRIPTION: &str = "Edit the contents of a file by finding and replacing exact text. When editing text from Read output, preserve the exact indentation (tabs or spaces) and omit the line-number prefix, such as `1: `. Never include the prefix in oldString or newString. The edit fails if oldString is not found. By default, oldString must identify a UNIQUE location. Multiple matches FAIL unless replaceAll is true. Add more surrounding context to disambiguate, or set replaceAll to true to replace every occurrence. Use replaceAll when the change should apply to every occurrence, such as renaming a variable.";

/// A match as byte offsets into the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    pub start: usize,
    pub end: usize,
}

/// Non-overlapping `indexOf` scan.
pub fn find_occurrences(content: &str, search: &str) -> Vec<Match> {
    let mut result = Vec::new();
    if search.is_empty() {
        return result;
    }
    let mut offset = 0;
    while let Some(found) = content[offset..].find(search) {
        let start = offset + found;
        result.push(Match { start, end: start + search.len() });
        offset = start + search.len();
    }
    result
}

/// Unicode-normalized scan; offsets are mapped back to the original (the mapping is 1:1 per char).
pub fn find_unicode_occurrences(content: &str, search: &str) -> Vec<Match> {
    let normalized = normalize_unicode(content);
    let needle = normalize_unicode(search);
    // Map normalized byte offsets → original byte offsets at char boundaries.
    let mut map = vec![0usize; normalized.len() + 1];
    let mut n = 0;
    for (o, c) in content.char_indices() {
        let nc = normalize_unicode(&c.to_string());
        map[n] = o;
        n += nc.len();
    }
    map[n] = content.len();
    find_occurrences(&normalized, &needle).into_iter().map(|m| Match { start: map[m.start], end: map[m.end] }).collect()
}

struct Line {
    start: usize,
    end: usize,
    text: String,
    content_end: usize,
    newline: bool,
}

/// Line-based matching that ignores trailing whitespace and unicode punctuation variants.
pub fn find_line_occurrences(content: &str, search: &str) -> Vec<Match> {
    let trailing_newline = search.ends_with('\n');
    let mut expected: Vec<&str> = search.split('\n').collect();
    if trailing_newline {
        expected.pop();
    }
    let mut lines: Vec<Line> = Vec::new();
    let mut start = 0;
    while start < content.len() {
        let (end, newline) = match content[start..].find('\n') {
            Some(i) => (start + i + 1, true),
            None => (content.len(), false),
        };
        let text = if newline { &content[start..end - 1] } else { &content[start..end] };
        lines.push(Line {
            start,
            end,
            text: text.to_string(),
            content_end: start + text.len() - usize::from(text.ends_with('\r')),
            newline,
        });
        start = end;
    }
    let mut candidates: Vec<Match> = Vec::new();
    for index in 0..lines.len() {
        if index + expected.len() > lines.len() {
            break;
        }
        let actual = &lines[index..index + expected.len()];
        if !actual
            .iter()
            .zip(&expected)
            .all(|(item, exp)| normalize_unicode(js::trim_end(&item.text)) == normalize_unicode(js::trim_end(exp)))
        {
            continue;
        }
        let Some(last) = actual.last() else { continue };
        if trailing_newline && !last.newline {
            continue;
        }
        candidates.push(Match { start: lines[index].start, end: if trailing_newline { last.end } else { last.content_end } });
    }
    let mut result: Vec<Match> = Vec::new();
    for candidate in candidates {
        if result.iter().any(|m| m.end > candidate.start && m.start < candidate.end) {
            continue;
        }
        result.push(candidate);
    }
    result
}

/// Matcher cascade: exact, unicode-normalized, line/trailing-whitespace.
pub fn find_matches(source: &str, old: &str) -> Vec<Match> {
    let exact = find_occurrences(source, old);
    if !exact.is_empty() {
        return exact;
    }
    let unicode = find_unicode_occurrences(source, old);
    if !unicode.is_empty() {
        return unicode;
    }
    find_line_occurrences(source, old)
}

/// Apply the first match (or all) right-to-left.
pub fn apply(source: &str, matches: &[Match], replacement: &str, all: bool) -> String {
    let selected = if all { matches } else { &matches[..matches.len().min(1)] };
    let mut content = source.to_string();
    for m in selected.iter().rev() {
        content = format!("{}{}{}", &content[..m.start], replacement, &content[m.end..]);
    }
    content
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Input {
    path: String,
    old_string: String,
    new_string: String,
    replace_all: Option<bool>,
}

pub struct EditTool {
    formatter: Arc<Formatter>,
}

impl EditTool {
    pub fn new(formatter: Arc<Formatter>) -> Self {
        Self { formatter }
    }
}

fn file_failure(leaf: &str, error: FileError) -> ToolError {
    ToolError::with_cause(leaf, ErrorCause::Error(error.patch_message()))
}

impl EditTool {
    async fn run(&self, input: &Input, ctx: &ToolContext) -> Result<(Vec<FileDiff>, usize), ToolError> {
        let leaf = format!("Unable to edit {}", input.path);
        if input.old_string == input.new_string {
            return Err(ToolError::new("No changes to apply: oldString and newString are identical."));
        }
        if input.old_string.is_empty() {
            return Err(ToolError::new("oldString must not be empty. Use write to create or overwrite a file."));
        }
        let target = file_access::resolve(ctx, &input.path, Some(Kind::File));
        file_access::authorize_external(ctx, &[&target], None).await.map_err(super::perm(leaf.clone()))?;
        let original = match file_mutation::read_text(&target.absolute) {
            Ok(text) => text,
            Err(FileError::NotFound) => return Err(ToolError::new(format!("File not found: {}", input.path))),
            Err(FileError::WrongKind(kind)) if kind == "directory" => {
                return Err(ToolError::new(format!("Path is a directory, not a file: {}", input.path)));
            }
            Err(error) => return Err(file_failure(&leaf, error)),
        };
        let source = &original.text;
        let ending = if source.contains("\r\n") { "\r\n" } else { "\n" };
        let old = input.old_string.replace("\r\n", "\n").replace('\n', ending);
        let new = input.new_string.replace("\r\n", "\n").replace('\n', ending);
        let matches = find_matches(source, &old);
        let replacements = matches.len();
        let replace_all = input.replace_all == Some(true);
        let replaced = apply(source, &matches, &new, replace_all);
        let preview = (replacements > 0 && (replacements == 1 || replace_all)).then(|| file_diff(&target.resource, source, &replaced, "modified"));
        super::assert(
            ctx,
            "edit",
            vec![target.resource.clone()],
            Some(vec!["*".into()]),
            preview.map(|p| meta(vec![("files", json!([p.to_json()]))])),
        )
        .await
        .map_err(super::perm(leaf.clone()))?;
        if replacements == 0 {
            return Err(ToolError::new(format!(
                "Could not find oldString in {}. It must match exactly, including whitespace and indentation.",
                input.path
            )));
        }
        if replacements > 1 && !replace_all {
            return Err(ToolError::new(format!(
                "Found {replacements} matches for oldString, but expected exactly one. Add more surrounding context to make oldString unique, or set replaceAll to true to replace every occurrence."
            )));
        }
        let keep_bom = original.bom || replaced.starts_with('\u{FEFF}');
        let result = file_mutation::write(&target.absolute, &target.resource, bom::join(&replaced, keep_bom).as_bytes())
            .await
            .map_err(|e| file_failure(&leaf, e))?;
        let formatted = if super::format_file(&self.formatter, ctx, &target.absolute).await {
            file_mutation::sync_text_bom(&target.absolute, keep_bom).map_err(|e| file_failure(&leaf, e))?
        } else {
            file_mutation::read_text(&target.absolute).map_err(|e| file_failure(&leaf, e))?.text
        };
        ctx.host.files_changed(&ctx.session_id, std::slice::from_ref(&target.absolute)).await;
        Ok((vec![file_diff(&result.resource, source, &formatted, "modified")], replacements))
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn options(&self) -> ToolOptions {
        ToolOptions { permission: Some("edit".into()), ..ToolOptions::direct() }
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        super::definition("edit", DESCRIPTION, include_str!("../../schemas/edit.json"))
    }

    fn advertised(&self, ctx: &DefinitionContext) -> bool {
        !super::patch::use_patch(ctx)
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let input: Input = decode(input)?;
        let lock = file_access::resolve_path(&ctx.directory, &input.path, &ctx.global.home);
        let _guard = file_mutation::with_lock(&[lock]).await;
        let (files, replacements) = self.run(&input, &ctx).await?;
        let files_json: Vec<Value> = files.iter().map(FileDiff::to_json).collect();
        let text = format!(
            "Edited {} ({replacements} replacement{})",
            files.first().map(|f| f.file.as_str()).unwrap_or("undefined"),
            if replacements == 1 { "" } else { "s" }
        );
        Ok(ToolResult {
            output: Some(json!({"files": files_json, "replacements": replacements})),
            content: vec![ToolContent::Text { text }],
            metadata: Some(meta(vec![("files", Value::Array(files_json))])),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(source: &str, old: &str, new: &str, all: bool) -> (usize, String) {
        let matches = find_matches(source, old);
        (matches.len(), apply(source, &matches, new, all))
    }

    #[test]
    fn exact_matches() {
        assert_eq!(run("a b a", "a", "c", false), (2, "c b a".into()));
        assert_eq!(run("a b a", "a", "c", true), (2, "c b c".into()));
        assert_eq!(run("aaaa", "aa", "b", true), (2, "bb".into()));
        assert_eq!(run("abc", "zz", "y", false), (0, "abc".into()));
    }

    #[test]
    fn unicode_normalized() {
        let source = "let s = “héllo” — ‘x’\u{00A0}y;";
        let (n, out) = run(source, "\"héllo\" - 'x' y", "Z", false);
        assert_eq!(n, 1);
        assert_eq!(out, "let s = Z;");
    }

    #[test]
    fn trailing_whitespace_lines() {
        let source = "fn a() {   \n  body();\t\n}\nrest\n";
        let (n, out) = run(source, "fn a() {\n  body();\n}\n", "X\n", false);
        assert_eq!(n, 1);
        assert_eq!(out, "X\nrest\n");
        // without trailing newline the match ends before the line break
        let (n, out) = run(source, "fn a() {\n  body();", "X", false);
        assert_eq!(n, 1);
        assert_eq!(out, "X\n}\nrest\n");
        // CRLF content end excludes \r
        let (n, out) = run("a  \r\nb\r\n", "a\nb", "Q", false);
        assert_eq!(n, 1);
        assert_eq!(out, "Q\r\n");
    }

    #[test]
    fn overlapping_line_candidates_dropped() {
        let source = "x\nx\nx\n";
        let matches = find_line_occurrences(source, "x \nx ");
        assert_eq!(matches, vec![Match { start: 0, end: 3 }]);
    }
}
