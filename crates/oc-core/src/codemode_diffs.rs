//! The changes a Code Mode program made to files (`core/src/codemode/diffs.ts`, 2.4.1 #65002/#65247): each inner
//! edit, patch or write call's own result reports its changes, which the `execute` call lists in `metadata.files`
//! while it runs and when it ends. Nothing here reads a file.

use oc_tools::ToolResult;
use serde_json::{Map, Value, json};

/// The most patch text an execute call keeps on its part; changes past it keep only their counts.
pub const MAX_PATCH_BYTES: usize = 200_000;
/// The most changes an execute call lists.
pub const MAX_FILES: usize = 50;

/// One change a program made to a file, in the order it made them. Counts are absent where they aren't known.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub file: String,
    /// `added`, `deleted` or `modified`.
    pub status: String,
    pub additions: Option<f64>,
    pub deletions: Option<f64>,
    pub patch: Option<String>,
}

impl Change {
    /// JSON in upstream's key order (`file, status, additions?, deletions?, patch?`).
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("file".into(), json!(self.file));
        out.insert("status".into(), json!(self.status));
        if let Some(v) = self.additions {
            out.insert("additions".into(), number(v));
        }
        if let Some(v) = self.deletions {
            out.insert("deletions".into(), number(v));
        }
        if let Some(v) = &self.patch {
            out.insert("patch".into(), json!(v));
        }
        Value::Object(out)
    }
}

fn number(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9.0e15 { json!(v as i64) } else { json!(v) }
}

/// The collector of one execute call (`CodeModeDiffs.make`): each change is kept as it arrives, within the budget:
/// past [`MAX_FILES`] it is dropped, and past the patch budget it keeps only its counts.
#[derive(Debug, Clone)]
pub struct Diffs {
    entries: Vec<Change>,
    budget: usize,
}

impl Default for Diffs {
    fn default() -> Self {
        Diffs { entries: Vec::new(), budget: MAX_PATCH_BYTES }
    }
}

impl Diffs {
    pub fn record(&mut self, change: Change) {
        if self.entries.len() >= MAX_FILES {
            return;
        }
        let size = change.patch.as_ref().map(String::len).unwrap_or(0);
        if change.patch.is_none() || size > self.budget {
            self.entries.push(Change { patch: None, ..change });
            return;
        }
        self.budget -= size;
        self.entries.push(change);
    }

    pub fn files(&self) -> &[Change] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn to_json(&self) -> Value {
        Value::Array(self.entries.iter().map(Change::to_json).collect())
    }
}

/// `ChangedFile` decoded from one item of an edit's or patch's `metadata.files` (`status` defaults to `modified`).
fn decode_file(value: &Value) -> Option<Change> {
    let object = value.as_object()?;
    let file = object.get("file")?.as_str()?.to_string();
    let status = match object.get("status") {
        None => "modified".to_string(),
        Some(Value::String(s)) if matches!(s.as_str(), "added" | "deleted" | "modified") => s.clone(),
        Some(_) => return None,
    };
    let count = |key: &str| -> Result<Option<f64>, ()> {
        match object.get(key) {
            None => Ok(None),
            Some(v) => v.as_f64().map(Some).ok_or(()),
        }
    };
    let additions = count("additions").ok()?;
    let deletions = count("deletions").ok()?;
    let patch = match object.get("patch") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return None,
    };
    Some(Change { file, status, additions, deletions, patch })
}

/// `changes(tool, input, result)`: the changes an inner call made, from its own result. Edit and patch return each
/// file's diff in `metadata.files`. Write returns none, so its change comes from what it was asked to write: a created
/// file's lines are its content; an overwrite's counts aren't known without reading the file. `tool` is the name the
/// program called (its registry ID), so a namespaced tool that happens to be named edit or write records nothing.
pub fn changes(tool: &str, input: &Value, result: &ToolResult) -> Vec<Change> {
    if tool == "edit" || tool == "patch" {
        let Some(files) = result.metadata.as_ref().and_then(|m| m.get("files")).and_then(Value::as_array) else { return Vec::new() };
        return files.iter().filter_map(decode_file).collect();
    }
    if tool != "write" {
        return Vec::new();
    }
    // `decodeWrite`: the call's own `{ path, content }`.
    let (Some(_), Some(content)) = (input.get("path").and_then(Value::as_str), input.get("content").and_then(Value::as_str)) else {
        return Vec::new();
    };
    let Some(Value::String(output)) = &result.output else { return Vec::new() };
    let (verb, file) = if let Some(file) = output.strip_prefix("Created file successfully: ") {
        ("Created", file)
    } else if let Some(file) = output.strip_prefix("Wrote file successfully: ") {
        ("Wrote", file)
    } else {
        return Vec::new();
    };
    // `/^(Created|Wrote) file successfully: (.+)$/`: one line, not empty.
    if file.is_empty() || file.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
        return Vec::new();
    }
    if verb == "Wrote" {
        return vec![Change { file: file.to_string(), status: "modified".into(), additions: None, deletions: None, patch: None }];
    }
    let diff = oc_tools::file_diff::file_diff(file, "", content, "added");
    vec![Change { file: file.to_string(), status: "added".into(), additions: Some(diff.additions as f64), deletions: Some(0.0), patch: Some(diff.patch) }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_tools::file_diff::file_diff;

    fn patch_result(files: &[oc_tools::file_diff::FileDiff]) -> ToolResult {
        let mut metadata = oc_schema::Metadata::new();
        metadata.insert("files".into(), Value::Array(files.iter().map(|f| f.to_json()).collect()));
        ToolResult { output: Some(json!({})), content: Vec::new(), metadata: Some(metadata) }
    }

    fn change(file: &str, status: &str, additions: Option<f64>, deletions: Option<f64>, patch: Option<&str>) -> Change {
        Change { file: file.into(), status: status.into(), additions, deletions, patch: patch.map(String::from) }
    }

    /// `codemode/diffs.test.ts`: two patches to one file are two changes, in order, each with its result's diff.
    #[test]
    fn two_patches_to_one_file_are_two_changes() {
        let mut diffs = Diffs::default();
        let first = file_diff("src/app.ts", "one\ntwo\n", "one\nTWO\n", "modified");
        let second = file_diff("src/app.ts", "one\nTWO\n", "one\nTWO\nthree\n", "modified");
        for file in [&first, &second] {
            for c in changes("patch", &json!({}), &patch_result(std::slice::from_ref(file))) {
                diffs.record(c);
            }
        }
        assert_eq!(
            diffs.files(),
            &[
                change("src/app.ts", "modified", Some(1.0), Some(1.0), Some(&first.patch)),
                change("src/app.ts", "modified", Some(1.0), Some(0.0), Some(&second.patch)),
            ]
        );
    }

    /// An edit's result gives its diff; a write's created file counts its content, an overwrite keeps no counts.
    #[test]
    fn edit_and_write_changes() {
        let edit = file_diff("a.ts", "a\n", "b\n", "modified");
        assert_eq!(changes("edit", &json!({}), &patch_result(std::slice::from_ref(&edit))), vec![change("a.ts", "modified", Some(1.0), Some(1.0), Some(&edit.patch))]);
        let created = ToolResult::text("x").with_output(json!("Created file successfully: n.md"));
        let found = changes("write", &json!({"path": "n.md", "content": "# N\nx\n"}), &created);
        assert_eq!((found[0].file.as_str(), found[0].status.as_str(), found[0].additions, found[0].deletions), ("n.md", "added", Some(2.0), Some(0.0)));
        let wrote = ToolResult::text("x").with_output(json!("Wrote file successfully: n.md"));
        assert_eq!(changes("write", &json!({"path": "n.md", "content": "y\n"}), &wrote), vec![change("n.md", "modified", None, None, None)]);
        assert_eq!(changes("read", &json!({"path": "a.ts"}), &patch_result(&[edit])), vec![]);
        // A namespaced tool's registry ID is not `edit`.
        assert_eq!(changes("mcp_edit", &json!({}), &patch_result(&[])), vec![]);
    }

    /// The patch budget and the change cap apply as changes arrive.
    #[test]
    fn budget_and_cap() {
        let mut diffs = Diffs::default();
        let big = "x".repeat(MAX_PATCH_BYTES);
        diffs.record(change("small.txt", "added", Some(1.0), Some(0.0), Some("+a")));
        diffs.record(change("big.txt", "added", Some(1.0), Some(0.0), Some(&big)));
        diffs.record(change("after.txt", "added", Some(1.0), Some(0.0), Some("+b")));
        let kept: Vec<(&str, bool)> = diffs.files().iter().map(|f| (f.file.as_str(), f.patch.is_some())).collect();
        assert_eq!(kept, vec![("small.txt", true), ("big.txt", false), ("after.txt", true)]);
        for index in 0..MAX_FILES {
            diffs.record(change(&format!("f{index}.txt"), "deleted", None, None, None));
        }
        assert_eq!(diffs.files().len(), MAX_FILES);
    }
}
