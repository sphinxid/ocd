//! `core/src/vcs/patch.ts`: unified-diff helpers shared by snapshot and VCS diffs.

use std::collections::HashMap;

/// Effectively "full file" context; adapters use it when no context is requested.
pub const PATCH_CONTEXT_LINES: u64 = 2_147_483_647;
pub const MAX_PATCH_BYTES: usize = 10_000_000;
pub const MAX_TOTAL_PATCH_BYTES: usize = 10_000_000;

#[derive(Debug, Clone, Default)]
pub struct Patch {
    pub text: String,
    pub truncated: bool,
}

/// `formatPatch(structuredPatch(file, file, "", "", "", "", { context: 0 }))` from `diff@8.0.4`.
pub fn empty_patch(file: &str) -> String {
    format!("Index: {file}\n===================================================================\n--- {file}\t\n+++ {file}\t\n")
}

/// Count changed lines in a unified diff, ignoring `+++`/`---` file headers.
pub fn count_patch(patch: &str) -> (u64, u64) {
    let mut additions = 0;
    let mut deletions = 0;
    for line in patch.split('\n') {
        if line.starts_with('+') && !line.starts_with("+++") {
            additions += 1
        } else if line.starts_with('-') && !line.starts_with("---") {
            deletions += 1
        }
    }
    (additions, deletions)
}

fn parse_quoted_path(value: &str) -> Option<(String, usize)> {
    let chars: Vec<(usize, char)> = value.char_indices().collect();
    let mut out = String::new();
    let mut idx = 1;
    while idx < chars.len() {
        let (pos, ch) = chars[idx];
        if ch == '"' {
            return Some((out, pos + 1));
        }
        if ch != '\\' {
            out.push(ch);
            idx += 1;
            continue;
        }
        idx += 1;
        match chars.get(idx).map(|c| c.1) {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(next) => out.push(next),
            None => {}
        }
        idx += 1;
    }
    None
}

fn parse_path_token(value: &str) -> String {
    if !value.starts_with('"') {
        return value.split('\t').next().unwrap_or("").to_string();
    }
    parse_quoted_path(value).map(|v| v.0).unwrap_or_else(|| value.to_string())
}

fn file_from_diff_path(value: Option<&str>) -> Option<String> {
    let value = value?;
    if value == "/dev/null" {
        return None;
    }
    let file = parse_path_token(value);
    if file.starts_with("a/") || file.starts_with("b/") {
        return Some(file[2..].to_string());
    }
    Some(file)
}

fn file_from_git_header(header: &str) -> Option<String> {
    if header.starts_with('"') {
        let (_, end) = parse_quoted_path(header)?;
        let second = header[end..].trim_start();
        if second.is_empty() {
            return None;
        }
        if !second.starts_with('"') {
            return file_from_diff_path(Some(second));
        }
        let parsed = parse_quoted_path(second).map(|v| v.0);
        return file_from_diff_path(parsed.as_deref());
    }
    let separator = header.find(" b/")?;
    file_from_diff_path(Some(&header[separator + 1..]))
}

/// First JS-multiline-regex match of `^<prefix>(.+)$`: lines split on every JS line terminator.
fn line_value<'a>(chunk: &'a str, prefix: &str) -> Option<&'a str> {
    chunk
        .split(['\n', '\r', '\u{2028}', '\u{2029}'])
        .find_map(|line| line.strip_prefix(prefix).filter(|rest| !rest.is_empty()))
}

pub fn file_from_patch_chunk(chunk: &str) -> Option<String> {
    let next = line_value(chunk, "+++ ");
    let before = line_value(chunk, "--- ");
    if let Some(file) = file_from_diff_path(next).or_else(|| file_from_diff_path(before)) {
        return Some(file);
    }
    let header = line_value(chunk, "diff --git ").unwrap_or("");
    file_from_git_header(header)
}

/// Split `git diff`-format output into per-file chunks, dropping a trailing partial chunk when truncated.
pub fn split_git_patch(patch: &Patch) -> Vec<&str> {
    let text = patch.text.as_str();
    let mut starts = Vec::new();
    if text.starts_with("diff --git ") {
        starts.push(0);
    }
    let mut from = 0;
    while let Some(found) = text[from..].find("\ndiff --git ") {
        let at = from + found;
        starts.push(at + 1);
        from = at + 1;
    }
    let chunks: Vec<&str> = starts
        .iter()
        .enumerate()
        .map(|(index, start)| &text[*start..starts.get(index + 1).copied().unwrap_or(text.len())])
        .collect();
    if !patch.truncated {
        return chunks;
    }
    let keep = chunks.len().saturating_sub(1);
    chunks.into_iter().take(keep).collect()
}

/// Map per-file chunks by the file they touch, concatenating chunks for the same file.
pub fn chunks_by_file(patch: &Patch, fallback: impl Fn(usize) -> Option<String>) -> HashMap<String, String> {
    let mut acc: HashMap<String, String> = HashMap::new();
    for (index, chunk) in split_git_patch(patch).into_iter().enumerate() {
        let Some(file) = file_from_patch_chunk(chunk).or_else(|| fallback(index)) else { continue };
        acc.entry(file).or_default().push_str(chunk);
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_patch_matches_jsdiff() {
        assert_eq!(
            empty_patch("a b.txt"),
            "Index: a b.txt\n===================================================================\n--- a b.txt\t\n+++ a b.txt\t\n"
        );
    }

    #[test]
    fn splits_and_maps_chunks() {
        let text = "diff --git a/x.txt b/x.txt\nindex 1..2 100644\n--- a/x.txt\n+++ b/x.txt\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/gone b/gone\ndeleted file mode 100644\n--- a/gone\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\ndiff --git \"a/q\\\"x\" \"b/q\\\"x\"\nnew file mode 100644\nBinary files /dev/null and \"b/q\\\"x\" differ\n";
        let patch = Patch { text: text.to_string(), truncated: false };
        let chunks = split_git_patch(&patch);
        assert_eq!(chunks.len(), 3);
        let map = chunks_by_file(&patch, |_| None);
        assert!(map["x.txt"].contains("+b\n"));
        assert!(map["gone"].contains("-x\n"));
        assert!(map.contains_key("q\"x"));
        let truncated = Patch { text: text.to_string(), truncated: true };
        assert_eq!(split_git_patch(&truncated).len(), 2);
    }

    #[test]
    fn counts_patch_lines() {
        assert_eq!(count_patch("--- a\n+++ b\n@@\n-x\n+y\n+z\n"), (2, 1));
    }
}
