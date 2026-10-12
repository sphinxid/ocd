//! Shell result text (`core/src/shell/result.ts`).

use oc_schema::Metadata;
use oc_schema::message::{ShellInfo, ShellStatus};
use serde_json::{Value, json};

pub const MISSING: &str = "Shell command output is no longer available.";

/// Captured tail (`capture`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    pub output: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShellResult {
    pub info: ShellInfo,
    pub capture: Option<Capture>,
}

/// `ShellResult.output` shape.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Output {
    pub output: String,
    pub truncated: bool,
    pub exit: Option<i64>,
    pub signal: Option<String>,
    pub timeout: Option<bool>,
}

pub fn output(result: &ShellResult) -> Output {
    Output {
        output: result.capture.as_ref().map(|c| c.output.clone()).unwrap_or_else(|| MISSING.into()),
        truncated: result.capture.as_ref().is_some_and(|c| c.truncated),
        exit: result.info.exit,
        signal: result.info.signal.clone(),
        timeout: (result.info.status == ShellStatus::Timeout).then_some(true),
    }
}

pub fn notice(output: &Output) -> Option<String> {
    if output.timeout == Some(true) {
        return Some("Timed out before completion".into());
    }
    if let Some(signal) = &output.signal {
        return Some(format!("Killed by {signal}"));
    }
    match output.exit {
        Some(code) if code != 0 => Some(format!("Exited with code {code}")),
        _ => None,
    }
}

pub fn metadata(output: &Output) -> Metadata {
    let mut map = Metadata::new();
    map.insert("truncated".into(), json!(output.truncated));
    if let Some(exit) = output.exit {
        map.insert("exit".into(), json!(exit));
    }
    if let Some(signal) = &output.signal {
        map.insert("signal".into(), json!(signal));
    }
    if let Some(timeout) = output.timeout {
        map.insert("timeout".into(), json!(timeout));
    }
    map
}

pub struct Notification {
    pub text: String,
    pub metadata: Metadata,
}

/// `ShellResult.notification`.
pub fn notification(shell_id: &str, job_id: Option<&str>, command: &str, state: &str, text: &str, output: Option<&Output>) -> Notification {
    let mut meta = Metadata::new();
    meta.insert("source".into(), json!("shell"));
    meta.insert("shellID".into(), json!(shell_id));
    if let Some(job) = job_id {
        meta.insert("jobID".into(), json!(job));
    }
    meta.insert("state".into(), json!(state));
    if let Some(output) = output {
        for (k, v) in metadata(output) {
            meta.insert(k, v);
        }
    }
    Notification {
        text: format!("<shell id=\"{}\" state=\"{state}\" command=\"{command}\">\n{text}\n</shell>", job_id.unwrap_or(shell_id)),
        metadata: meta,
    }
}

/// `ShellResult.userNotification` (shell commands run by the user).
pub fn user_notification(result: &ShellResult) -> Notification {
    let captured = output(result);
    let killed = result.info.status == ShellStatus::Killed;
    let status = if killed {
        Some("Cancelled".to_string())
    } else if captured.exit == Some(0) {
        None
    } else {
        Some(notice(&captured).unwrap_or_else(|| "Exited with code unknown".into()))
    };
    let text = [Some(captured.output.clone()), status].into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n");
    let message = notification(&result.info.id, None, &result.info.command, if killed { "cancelled" } else { "completed" }, &text, Some(&captured));
    Notification { text: format!("The following shell command was executed by the user:\n{}", message.text), metadata: message.metadata }
}

/// Bounded tail of an output file (`Shell.result` capture).
pub fn capture_tail(page: &str, file_size: u64, newlines: u64, max_lines: usize, max_bytes: usize, file: &str) -> Capture {
    let mut lines: Vec<&str> = page.split('\n').collect();
    if page.ends_with('\n') {
        lines.pop();
    }
    let truncated = file_size > max_bytes as u64 || lines.len() > max_lines;
    let text = if lines.len() > max_lines { lines[lines.len() - max_lines..].join("\n") } else { page.to_string() };
    let total = newlines + u64::from(!page.ends_with('\n'));
    let shown = lines.len().min(max_lines) as u64;
    let notice = if truncated {
        format!("\n\n[showing lines {}-{total} of {total}; full output saved to {file}]", total - shown + 1)
    } else {
        String::new()
    };
    Capture { output: format!("{}{notice}", if text.is_empty() { "(no output)" } else { &text }), truncated }
}

pub fn value(output: &Output) -> Value {
    let mut v = json!({"output": output.output, "truncated": output.truncated});
    if let Some(exit) = output.exit {
        v["exit"] = json!(exit);
    }
    if let Some(signal) = &output.signal {
        v["signal"] = json!(signal);
    }
    if let Some(timeout) = output.timeout {
        v["timeout"] = json!(timeout);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_truncation() {
        let c = capture_tail("a\nb\n", 4, 2, 2000, 51200, "/f");
        assert_eq!(c, Capture { output: "a\nb\n".into(), truncated: false });
        let c = capture_tail("", 0, 0, 2000, 51200, "/f");
        assert_eq!(c.output, "(no output)");
        let c = capture_tail("1\n2\n3\n4\n", 8, 4, 2, 51200, "/f");
        assert_eq!(c.output, "3\n4\n\n[showing lines 3-4 of 4; full output saved to /f]");
        assert!(c.truncated);
        // byte-truncated page: total counts all newlines in the file
        let c = capture_tail("x\ny", 100, 10, 2000, 3, "/f");
        assert_eq!(c.output, "x\ny\n\n[showing lines 10-11 of 11; full output saved to /f]");
    }

    #[test]
    fn notices() {
        assert_eq!(notice(&Output { timeout: Some(true), ..Default::default() }).unwrap(), "Timed out before completion");
        assert_eq!(notice(&Output { signal: Some("SIGTERM".into()), ..Default::default() }).unwrap(), "Killed by SIGTERM");
        assert_eq!(notice(&Output { exit: Some(2), ..Default::default() }).unwrap(), "Exited with code 2");
        assert!(notice(&Output { exit: Some(0), ..Default::default() }).is_none());
        let n = notification("sh_1", Some("sh_1"), "ls", "completed", "out", None);
        assert_eq!(n.text, "<shell id=\"sh_1\" state=\"completed\" command=\"ls\">\nout\n</shell>");
    }
}
