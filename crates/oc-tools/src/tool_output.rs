//! Generic output bounding (`core/src/tool-output.ts`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use oc_schema::ToolContent;
use serde_json::Value;

use crate::tool::ToolResult;
use crate::util::js;

pub const MAX_LINES: usize = 2_000;
pub const MAX_BYTES: usize = 50 * 1024;
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub const DIRECTORY: &str = "tool-output";

pub fn file_name() -> String {
    format!("tool_{}", oc_schema::id::create(false, None))
}

pub fn directory(data: &Path) -> PathBuf {
    data.join(DIRECTORY)
}

/// `ToolOutput.truncate`. Applied by the session runner after every local tool execution.
pub fn truncate(result: ToolResult, max_lines: usize, max_bytes: usize, data: &Path) -> std::io::Result<ToolResult> {
    if result.metadata.as_ref().is_some_and(|m| m.contains_key("truncated")) {
        return Ok(result);
    }
    let text = result
        .content
        .iter()
        .filter_map(|c| match c {
            ToolContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    let total_bytes = text.len();
    if lines.len() <= max_lines && total_bytes <= max_bytes {
        let mut metadata = result.metadata.clone().unwrap_or_default();
        metadata.insert("truncated".into(), Value::Bool(false));
        return Ok(ToolResult { metadata: Some(metadata), ..result });
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    for line in lines.iter().take(max_lines) {
        let size = line.len() + usize::from(!kept.is_empty());
        if bytes + size > max_bytes {
            break;
        }
        kept.push(line);
        bytes += size;
    }
    let dir = directory(data);
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(file_name());
    std::fs::write(&file, &text)?;
    let shown = if kept.is_empty() { "0 lines".to_string() } else { format!("lines 1-{}", kept.len()) };
    let marker = format!("[showing {shown} of {}; full output saved to {}]", lines.len(), file.to_string_lossy());

    let mut bounded: Vec<ToolContent> = Vec::new();
    let mut remaining = js::utf16_len(&kept.join("\n"));
    let mut seen_text = false;
    let mut marked = false;
    for item in &result.content {
        let ToolContent::Text { text } = item else {
            bounded.push(item.clone());
            continue;
        };
        if seen_text && remaining > 0 {
            remaining -= 1;
        }
        seen_text = true;
        let length = js::utf16_len(text);
        if remaining >= length {
            bounded.push(item.clone());
            remaining -= length;
            continue;
        }
        if remaining > 0 {
            bounded.push(ToolContent::Text { text: js::utf16_prefix(text, remaining).to_string() });
        }
        if !marked {
            bounded.push(ToolContent::Text { text: marker.clone() });
        }
        remaining = 0;
        marked = true;
    }
    if !marked {
        bounded.push(ToolContent::Text { text: marker });
    }
    let mut metadata = result.metadata.clone().unwrap_or_default();
    metadata.insert("truncated".into(), Value::Bool(true));
    metadata.insert("outputPath".into(), Value::String(file.to_string_lossy().into_owned()));
    Ok(ToolResult { output: result.output, content: bounded, metadata: Some(metadata) })
}

/// `FileRetention.cleanup` over files matching `pattern`.
pub fn retention_cleanup(files: &[PathBuf], retention: Duration) {
    let Some(cutoff) = std::time::SystemTime::now().checked_sub(retention) else { return };
    for file in files {
        let Ok(meta) = std::fs::metadata(file) else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if mtime >= cutoff {
            continue;
        }
        let _ = std::fs::remove_file(file);
    }
}

/// Hourly cleanup body: delete `tool_[0-9a-f]{12}*` files older than 7 days.
pub fn cleanup(data: &Path) {
    let dir = directory(data);
    let re = regex::Regex::new(r"^tool_[0-9a-f]{12}").unwrap();
    let files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| re.is_match(&e.file_name().to_string_lossy()))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    retention_cleanup(&files, RETENTION);
}

/// Spawn the hourly cleanup loop (runs immediately, then every hour).
pub fn spawn_cleanup(data: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let dir = data.clone();
            let _ = tokio::task::spawn_blocking(move || cleanup(&dir)).await;
            tokio::time::sleep(Duration::from_secs(60 * 60)).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::meta;

    fn text(t: &str) -> ToolContent {
        ToolContent::Text { text: t.to_string() }
    }

    #[test]
    fn small_output_marks_untruncated() {
        let dir = tempfile::tempdir().unwrap();
        let result = truncate(ToolResult::text("hello"), 10, 100, dir.path()).unwrap();
        assert_eq!(result.metadata.unwrap().get("truncated"), Some(&Value::Bool(false)));
        assert_eq!(result.content, vec![text("hello")]);
    }

    #[test]
    fn respects_existing_truncated_flag() {
        let dir = tempfile::tempdir().unwrap();
        let input = ToolResult::text("a\n".repeat(100)).with_metadata(meta(vec![("truncated", Value::Bool(false))]));
        let result = truncate(input.clone(), 10, 100, dir.path()).unwrap();
        assert_eq!(result, input);
    }

    #[test]
    fn truncates_by_lines() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        let result = truncate(ToolResult::text(body.clone()), 3, 1000, dir.path()).unwrap();
        let metadata = result.metadata.unwrap();
        let path = metadata.get("outputPath").unwrap().as_str().unwrap().to_string();
        assert!(Path::new(&path).file_name().unwrap().to_string_lossy().starts_with("tool_"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        assert_eq!(
            result.content,
            vec![text("line1\nline2\nline3"), text(&format!("[showing lines 1-3 of 10; full output saved to {path}]"))]
        );
        assert_eq!(metadata.get("truncated"), Some(&Value::Bool(true)));
    }

    #[test]
    fn truncates_by_bytes_across_parts() {
        let dir = tempfile::tempdir().unwrap();
        let file = ToolContent::File { uri: "data:image/png;base64,AA".into(), mime: "image/png".into(), name: None };
        let input = ToolResult {
            output: None,
            content: vec![text("aaaa"), file.clone(), text("bbbb\ncccc"), text("dddd")],
            metadata: None,
        };
        // total text "aaaa\nbbbb\ncccc\ndddd" = 19 bytes, limit 12 keeps "aaaa","bbbb" (9 bytes)
        let result = truncate(input, 100, 12, dir.path()).unwrap();
        let path = result.metadata.as_ref().unwrap().get("outputPath").unwrap().as_str().unwrap().to_string();
        assert_eq!(
            result.content,
            vec![
                text("aaaa"),
                file,
                text("bbbb"),
                text(&format!("[showing lines 1-2 of 4; full output saved to {path}]")),
            ]
        );
    }

    #[test]
    fn zero_lines_marker() {
        let dir = tempfile::tempdir().unwrap();
        let result = truncate(ToolResult::text("x".repeat(50)), 10, 10, dir.path()).unwrap();
        let path = result.metadata.as_ref().unwrap().get("outputPath").unwrap().as_str().unwrap().to_string();
        assert_eq!(result.content, vec![text(&format!("[showing 0 lines of 1; full output saved to {path}]"))]);
    }

    #[test]
    fn cleanup_removes_old_files() {
        let dir = tempfile::tempdir().unwrap();
        let out = directory(dir.path());
        std::fs::create_dir_all(&out).unwrap();
        let old = out.join("tool_0123456789abXYZ");
        let other = out.join("keep.txt");
        std::fs::write(&old, "x").unwrap();
        std::fs::write(&other, "x").unwrap();
        let past = std::time::SystemTime::now() - Duration::from_secs(8 * 24 * 3600);
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(past).unwrap();
        std::fs::File::options().write(true).open(&other).unwrap().set_modified(past).unwrap();
        cleanup(dir.path());
        assert!(!old.exists());
        assert!(other.exists());
    }
}
