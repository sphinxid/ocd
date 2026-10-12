//! `core/src/tool/plugin/file-diff.ts`.

use serde::{Deserialize, Serialize};

use crate::jsdiff;

/// `FileDiff.Info` (serialized in upstream object-literal order).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub file: String,
    pub patch: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
}

impl FileDiff {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }
}

/// `fileDiff(file, before, after, status)`.
pub fn file_diff(file: &str, before: &str, after: &str, status: &str) -> FileDiff {
    let (additions, deletions) = jsdiff::diff_lines_counts(before, after);
    FileDiff {
        file: file.to_string(),
        patch: jsdiff::create_two_files_patch(file, file, before, after),
        status: status.to_string(),
        additions,
        deletions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_example() {
        let diff = file_diff("src/a.txt", "a\nb\nc\n", "a\nB\nc\n", "modified");
        assert_eq!(
            diff.patch,
            "Index: src/a.txt\n===================================================================\n--- src/a.txt\n+++ src/a.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n"
        );
        assert_eq!((diff.additions, diff.deletions), (1, 1));
        assert_eq!(
            serde_json::to_string(&diff).unwrap(),
            format!("{{\"file\":\"src/a.txt\",\"patch\":{},\"status\":\"modified\",\"additions\":1,\"deletions\":1}}", serde_json::to_string(&diff.patch).unwrap())
        );
    }
}
