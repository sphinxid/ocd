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

pub const DESCRIPTION: &str = "Edit the contents of a file by finding and replacing exact text. When editing text from Read output, preserve the exact indentation (tabs or spaces) and omit the line-number prefix, such as `1: `. Never include the prefix in oldString or newString. Write line breaks as \\n; the file's existing CRLF or LF line endings are matched and preserved. To change line endings themselves, make oldString and newString differ only in \\r; that edit applies to the exact bytes. If there is no exact match, the edit retries while ignoring typographic quote, dash, and space variants, then trailing whitespace on each line. The edit fails if oldString is not found. By default, oldString must identify a UNIQUE location. Multiple matches FAIL unless replaceAll is true. Add more surrounding context to disambiguate, or set replaceAll to true to replace every occurrence. Use replaceAll when the change should apply to every occurrence, such as renaming a variable.";

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

/// `lineEnding`: the dominant line ending in `text`, or `fallback` when there is none (or a tie).
pub fn line_ending<'a>(text: &str, fallback: &'a str) -> &'a str {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    if crlf == lf {
        fallback
    } else if crlf > lf {
        "\r\n"
    } else {
        "\n"
    }
}

/// `alignLines`: for each line of `after`, the index of the `before` line it keeps (longest common
/// subsequence) or, between kept lines, the line it takes the place of; `None` for lines with no counterpart.
pub fn align_lines(before: &[String], after: &[String]) -> Vec<Option<usize>> {
    let width = after.len() + 1;
    let mut table = vec![0u32; (before.len() + 1) * width];
    for i in (0..before.len()).rev() {
        for j in (0..after.len()).rev() {
            table[i * width + j] = if before[i] == after[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }
    let mut result: Vec<Option<usize>> = vec![None; after.len()];
    let mut gap_before: Vec<usize> = Vec::new();
    let mut gap_after: Vec<usize> = Vec::new();
    let flush = |result: &mut Vec<Option<usize>>, gap_before: &mut Vec<usize>, gap_after: &mut Vec<usize>| {
        for (k, j) in gap_after.iter().enumerate() {
            result[*j] = gap_before.get(k).copied();
        }
        gap_before.clear();
        gap_after.clear();
    };
    let (mut i, mut j) = (0, 0);
    while i < before.len() && j < after.len() {
        if before[i] == after[j] {
            flush(&mut result, &mut gap_before, &mut gap_after);
            result[j] = Some(i);
            i += 1;
            j += 1;
            continue;
        }
        if table[(i + 1) * width + j] >= table[i * width + j + 1] {
            gap_before.push(i);
            i += 1;
        } else {
            gap_after.push(j);
            j += 1;
        }
    }
    gap_before.extend(i..before.len());
    gap_after.extend(j..after.len());
    flush(&mut result, &mut gap_before, &mut gap_after);
    result
}

/// `withRegionEndings`: joins the replacement's lines with line endings taken from the matched source region.
/// A replacement line that keeps a region line (compared like the trailing-whitespace tier) reuses that line's
/// ending; a line that changes in place reuses the ending of the line it replaces; only lines beyond those take
/// `fallback`. Unchanged context inside a match therefore keeps its bytes even in a mixed-ending file.
pub fn with_region_endings(region: &str, replacement: &str, fallback: &str) -> String {
    let mut before: Vec<&str> = Vec::new();
    let mut endings: Vec<&str> = Vec::new();
    let mut start = 0;
    while let Some(i) = region[start..].find('\n') {
        let nl = start + i;
        let crlf = nl > start && region.as_bytes()[nl - 1] == b'\r';
        before.push(&region[start..if crlf { nl - 1 } else { nl }]);
        endings.push(if crlf { "\r\n" } else { "\n" });
        start = nl + 1;
    }
    before.push(&region[start..]);
    let after: Vec<&str> = replacement.split('\n').collect();
    let key = |line: &str| normalize_unicode(js::trim_end(line));
    let source = align_lines(
        &before.iter().map(|l| key(l)).collect::<Vec<_>>(),
        &after.iter().map(|l| key(l)).collect::<Vec<_>>(),
    );
    let mut out = String::with_capacity(replacement.len());
    for (index, line) in after.iter().enumerate() {
        out.push_str(line);
        if index + 1 < after.len() {
            out.push_str(source[index].and_then(|s| endings.get(s).copied()).unwrap_or(fallback));
        }
    }
    out
}

/// `countBefore`: number of sorted positions strictly before `offset`.
fn count_before(positions: &[usize], offset: usize) -> usize {
    positions.partition_point(|p| *p < offset)
}

/// `matchView`: matches against the LF view Read shows the model, then maps offsets back to the source, so
/// CRLF and mixed line endings neither block matches nor leak into untouched regions. Returns each match in
/// source offsets with its replacement text.
pub fn match_view(source: &str, old: &str, new: &str) -> Vec<(Match, String)> {
    let view = source.replace("\r\n", "\n");
    let removed: Vec<usize> = source.match_indices("\r\n").enumerate().map(|(index, (at, _))| at - index).collect();
    let file_ending = line_ending(source, "\n");
    let search = old.replace("\r\n", "\n");
    let replacement = new.replace("\r\n", "\n");
    find_matches(&view, &search)
        .into_iter()
        .map(|m| {
            let m = Match { start: m.start + count_before(&removed, m.start), end: m.end + count_before(&removed, m.end) };
            let region = &source[m.start..m.end];
            (m, with_region_endings(region, &replacement, line_ending(region, file_ending)))
        })
        .collect()
}

/// The edit's matches with their replacements, and whether it only changes line endings: strings that differ
/// only in `\r` apply to the exact bytes; every other edit goes through [`match_view`].
pub fn plan(source: &str, old: &str, new: &str) -> (bool, Vec<(Match, String)>) {
    if old.replace('\r', "") == new.replace('\r', "") {
        return (true, find_occurrences(source, old).into_iter().map(|m| (m, new.to_string())).collect());
    }
    (false, match_view(source, old, new))
}

/// Apply the first planned replacement (or all) right-to-left.
pub fn apply_planned(source: &str, planned: &[(Match, String)], all: bool) -> String {
    let selected = if all { planned } else { &planned[..planned.len().min(1)] };
    let mut content = source.to_string();
    for (m, replacement) in selected.iter().rev() {
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

const HINT_LINES: usize = 12;
const HINT_LINE_LENGTH: usize = 200;

/// `\s+` in a JavaScript regex.
fn collapse_whitespace(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut space = false;
    for c in value.chars() {
        if js::is_space(c) {
            if !space {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    out
}

/// `/[\p{L}\p{N}]/u`.
fn distinct(value: &str) -> bool {
    static LETTER_OR_NUMBER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"[\p{L}\p{N}]").unwrap());
    LETTER_OR_NUMBER.is_match(value)
}

/// `missHint`: a short hint for an oldString miss, so the next attempt can succeed without rereading the file:
/// newString is already there (often an edit that already applied), or the lines closest to oldString, numbered
/// like Read output. Lines vote for where oldString starts when they match ignoring all whitespace; at least half
/// must agree. Only the span from the first to the last line that matched is shown, so padding oldString cannot
/// reveal the lines after it.
fn miss_hint(source: &str, old_string: &str, new_string: &str) -> Option<String> {
    let view = source.replace("\r\n", "\n");
    let replacement = new_string.replace("\r\n", "\n");
    if js::utf16_len(js::trim(&replacement)) >= 20 && normalize_unicode(&view).contains(&normalize_unicode(&replacement)) {
        return Some("newString is already in the file, so this edit may already be applied.".into());
    }
    let key = |line: &str| js::trim(&collapse_whitespace(&normalize_unicode(line))).to_string();
    let lines: Vec<&str> = view.split('\n').collect();
    let search_text = old_string.replace("\r\n", "\n");
    let search: Vec<&str> = search_text.split('\n').collect();
    let mut positions: std::collections::HashMap<String, Vec<i64>> = std::collections::HashMap::new();
    for (index, line) in lines.iter().enumerate() {
        let value = key(line);
        if !distinct(&value) {
            continue;
        }
        positions.entry(value).or_default().push(index as i64);
    }
    // Insertion-ordered like a JS Map, so ties go to the first start that got a vote.
    let mut votes: indexmap::IndexMap<i64, usize> = indexmap::IndexMap::new();
    let mut wanted = 0;
    for (offset, line) in search.iter().enumerate() {
        let value = key(line);
        if !distinct(&value) {
            continue;
        }
        wanted += 1;
        for index in positions.get(&value).into_iter().flatten() {
            *votes.entry(index - offset as i64).or_insert(0) += 1;
        }
    }
    let (start, count) = votes.iter().fold((0i64, 0usize), |best, (&start, &count)| if count > best.1 { (start, count) } else { best });
    if count == 0 || count * 2 < wanted {
        return None;
    }
    let matched: Vec<usize> = search
        .iter()
        .enumerate()
        .filter_map(|(offset, line)| {
            let value = key(line);
            let index = start + offset as i64;
            (!value.is_empty() && index >= 0 && (index as usize) < lines.len() && key(lines[index as usize]) == value).then_some(index as usize)
        })
        .collect();
    let from = *matched.first()?;
    let to = (matched.last()? + 1).min(from + HINT_LINES);
    let mut out = vec!["The closest lines in the file:".to_string()];
    for (index, line) in lines[from..to].iter().enumerate() {
        let shown = if js::utf16_len(line) > HINT_LINE_LENGTH { format!("{}…", js::utf16_prefix(line, HINT_LINE_LENGTH)) } else { line.to_string() };
        out.push(format!("{}: {shown}", from + index + 1));
    }
    Some(out.join("\n"))
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
        // Strings that differ only in \r ask to change line endings themselves, so they apply to the exact
        // bytes. Every other edit matches the LF view Read shows the model and keeps the file's endings.
        let (endings_only, planned) = plan(source, &input.old_string, &input.new_string);
        let replacements = planned.len();
        let replace_all = input.replace_all == Some(true);
        let replaced = apply_planned(source, &planned, replace_all);
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
            // The hint shows file contents, so only when reading this file is already allowed.
            let readable = ctx.host.permission_allows(ctx.permission("read", vec![target.resource.clone()], Some(vec!["*".into()]), None)).await;
            let hint = if readable { miss_hint(source, &input.old_string, &input.new_string) } else { None };
            return Err(ToolError::new(format!(
                "Could not find oldString in {}. It must match exactly, including whitespace and indentation.{}",
                input.path,
                hint.map(|h| format!("\n\n{h}")).unwrap_or_default()
            )));
        }
        if replacements > 1 && !replace_all {
            return Err(ToolError::new(format!(
                "Found {replacements} matches for oldString, but expected exactly one. Add more surrounding context to make oldString unique, or set replaceAll to true to replace every occurrence."
            )));
        }
        if replaced == *source {
            return Err(ToolError::new(if endings_only {
                format!("No changes to apply: {} already has those line endings.", input.path)
            } else {
                format!(
                    "No changes to apply: the edit leaves {} unchanged. Line endings follow the file; to change them, write \\r explicitly in oldString or newString.",
                    input.path
                )
            }));
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

    /// tool-edit.test.ts "points a missed oldString at the closest lines, or at newString already applied" (#60689).
    #[test]
    fn miss_hint_cases() {
        let file = "import { a } from './a'\n\nexport function run() {\n  const value = a()\n  return value + 1\n}\n";
        assert_eq!(
            miss_hint(file, "export function run() {\n\tconst value = a()\n\treturn value + 2\n}", "x").as_deref(),
            Some("The closest lines in the file:\n3: export function run() {\n4:   const value = a()\n5:   return value + 1\n6: }")
        );
        assert_eq!(miss_hint(file, "  return value + 2", "  return value + 1"), None);
        assert_eq!(
            miss_hint(file, "const value = b()", "  const value = a()\n  return value + 1").as_deref(),
            Some("newString is already in the file, so this edit may already be applied.")
        );
        assert_eq!(miss_hint(file, "nothing like it", "anything"), None);
        assert_eq!(miss_hint(file, "import { a } from './a'\n\n\n\n\n\n", "x").as_deref(), Some("The closest lines in the file:\n1: import { a } from './a'"));
        // CRLF files, typographic variants and long lines.
        let crlf = format!("one line here\r\n{}\r\nthird “quoted” line\r\n", "x".repeat(250));
        assert_eq!(
            miss_hint(&crlf, "one line here\nZZZ\nthird \"quoted\" line", "y").as_deref(),
            Some(format!("The closest lines in the file:\n1: one line here\n2: {}…\n3: third “quoted” line", "x".repeat(200)).as_str())
        );
        // At most twelve lines are shown.
        let long: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        let search: Vec<String> = (1..=20).map(|n| format!("line {n}")).chain(["extra".into()]).collect();
        let hint = miss_hint(&long, &search.join("\n"), "y").unwrap();
        assert_eq!(hint.lines().count(), 13);
        assert!(hint.ends_with("12: line 12"));
    }

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
