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

/// `ShellResult.notice(output, timeout)`: how the command ended, for the model. `timeout` is the limit in ms that
/// applied (none or 0 leaves out how long it waited).
pub fn notice(output: &Output, timeout: Option<u64>) -> Option<String> {
    if output.timeout == Some(true) {
        let waited = match timeout.filter(|t| *t > 0) {
            Some(t) => format!(" after {} seconds", crate::util::js::number(t as f64 / 1_000.0)),
            None => String::new(),
        };
        return Some(format!(
            "Command timed out{waited}. If it needs longer and isn't waiting for input, rerun with a larger timeout.\nExit code: {}",
            exit_code(output)
        ));
    }
    if output.signal.is_some() || output.exit.is_some_and(|code| code != 0) {
        return Some(format!("Exit code: {}", exit_code(output)));
    }
    None
}

/// A signal reads as the shell's 128 + signal number convention, followed by its name (`exitCode`).
fn exit_code(output: &Output) -> String {
    if let Some(exit) = output.exit {
        return exit.to_string();
    }
    match output.signal.as_deref().and_then(signal_number) {
        Some(number) => format!("{} ({})", 128 + number, output.signal.as_deref().unwrap_or_default()),
        None => "unknown".into(),
    }
}

/// `os.constants.signals` of the platform, in the order libuv names a signal number (its first name wins).
fn signals() -> &'static [(&'static str, i32)] {
    #[cfg(unix)]
    {
        &[
            ("SIGHUP", libc::SIGHUP),
            ("SIGINT", libc::SIGINT),
            ("SIGQUIT", libc::SIGQUIT),
            ("SIGILL", libc::SIGILL),
            ("SIGTRAP", libc::SIGTRAP),
            ("SIGABRT", libc::SIGABRT),
            ("SIGIOT", libc::SIGIOT),
            ("SIGBUS", libc::SIGBUS),
            ("SIGFPE", libc::SIGFPE),
            ("SIGKILL", libc::SIGKILL),
            ("SIGUSR1", libc::SIGUSR1),
            ("SIGSEGV", libc::SIGSEGV),
            ("SIGUSR2", libc::SIGUSR2),
            ("SIGPIPE", libc::SIGPIPE),
            ("SIGALRM", libc::SIGALRM),
            ("SIGTERM", libc::SIGTERM),
            ("SIGCHLD", libc::SIGCHLD),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            ("SIGSTKFLT", libc::SIGSTKFLT),
            ("SIGCONT", libc::SIGCONT),
            ("SIGSTOP", libc::SIGSTOP),
            ("SIGTSTP", libc::SIGTSTP),
            ("SIGTTIN", libc::SIGTTIN),
            ("SIGTTOU", libc::SIGTTOU),
            ("SIGURG", libc::SIGURG),
            ("SIGXCPU", libc::SIGXCPU),
            ("SIGXFSZ", libc::SIGXFSZ),
            ("SIGVTALRM", libc::SIGVTALRM),
            ("SIGPROF", libc::SIGPROF),
            ("SIGWINCH", libc::SIGWINCH),
            ("SIGIO", libc::SIGIO),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            ("SIGPOLL", libc::SIGPOLL),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            ("SIGPWR", libc::SIGPWR),
            #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
            ("SIGINFO", libc::SIGINFO),
            ("SIGSYS", libc::SIGSYS),
        ]
    }
    #[cfg(not(unix))]
    {
        &[
            ("SIGHUP", 1),
            ("SIGINT", 2),
            ("SIGILL", 4),
            ("SIGABRT", 22),
            ("SIGFPE", 8),
            ("SIGKILL", 9),
            ("SIGSEGV", 11),
            ("SIGTERM", 15),
            ("SIGBREAK", 21),
            ("SIGWINCH", 28),
        ]
    }
}

/// `os.constants.signals[name]`.
pub fn signal_number(name: &str) -> Option<i32> {
    signals().iter().find(|(n, _)| *n == name).map(|(_, number)| *number)
}

/// The name a process killed by signal `number` reports (libuv's `signo_string`), or `SIG<number>`.
pub fn signal_name(number: i32) -> String {
    signals().iter().find(|(_, n)| *n == number).map(|(name, _)| name.to_string()).unwrap_or_else(|| format!("SIG{number}"))
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
        Some(notice(&captured, None).unwrap_or_else(|| "Exit code: unknown".into()))
    };
    let text = [Some(captured.output.clone()), status].into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n");
    let message = notification(&result.info.id, None, &result.info.command, if killed { "cancelled" } else { "completed" }, &text, Some(&captured));
    Notification { text: format!("The following shell command was executed by the user:\n{}", message.text), metadata: message.metadata }
}

/// What a command's output file looked like when its capture was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    /// Bytes written.
    pub size: u64,
    /// Newlines written.
    pub newlines: u64,
    /// Byte offsets where the last two lines start (`lineStarts`), so a notice can size a cut-off line.
    pub line_starts: [u64; 2],
}

/// The first byte of the bounded tail: the last `max_bytes` of the file.
pub fn capture_start(size: u64, max_bytes: usize) -> u64 {
    size.saturating_sub(max_bytes as u64)
}

/// Moves past UTF-8 continuation bytes (at most 3) so a byte cut never splits a character (`charBoundary`).
fn char_boundary(bytes: &[u8], offset: usize) -> usize {
    let limit = bytes.len().min(offset + 3);
    (offset..limit).find(|&index| bytes[index] & 0xc0 != 0x80).unwrap_or(limit)
}

/// `formatSize`: whole KB, or MB with one decimal (JS `toFixed(1)`, which rounds ties up).
fn format_size(bytes: u64) -> String {
    let kb = bytes as f64 / 1024.0;
    if kb > 1024.0 {
        let mb = kb / 1024.0;
        // Rust rounds an exact tie (x.x5, only exact for quarters) to even, toFixed upwards; other values round alike.
        let tie = (mb * 4.0).fract() == 0.0 && (mb * 2.0).fract() != 0.0;
        if tie { format!("{:.1} MB", (mb * 10.0).ceil() / 10.0) } else { format!("{mb:.1} MB") }
    } else {
        format!("{} KB", (kb + 0.5).floor())
    }
}

/// Bounded tail of an output file (the `Shell.result` capture). `window` holds the file's bytes from one byte before
/// [`capture_start`] (or from 0) to its end; that byte tells whether the tail starts on a line boundary. A tail that
/// starts mid-line drops that partial line so the line numbers stay exact, or names how much of it is shown when it is
/// the only line.
pub fn capture_tail(window: &[u8], written: Written, max_lines: usize, max_bytes: usize, file: &str) -> Capture {
    let start = capture_start(written.size, max_bytes);
    let before = start.saturating_sub(1);
    let page = String::from_utf8_lossy(&window[char_boundary(window, (start - before) as usize).min(window.len())..]).into_owned();
    let partial = start > 0 && window.first() != Some(&b'\n');
    let ends_with_newline = page.ends_with('\n');
    let mut lines: Vec<&str> = page.split('\n').collect();
    if ends_with_newline {
        lines.pop();
    }
    let total = written.newlines + u64::from(!ends_with_newline);
    let suffix = format!("full output saved to {file}]");
    if partial && lines.len() == 1 {
        let line_start = if ends_with_newline { written.line_starts[0] } else { written.line_starts[1] };
        let line_size = written.size - u64::from(ends_with_newline) - line_start;
        let shown = lines[0].len() as u64;
        return Capture {
            output: format!(
                "{}\n\n[showing the last {} of line {total} ({}); {suffix}",
                lines[0],
                format_size(shown),
                format_size(line_size)
            ),
            truncated: true,
        };
    }
    if start > 0 || lines.len() > max_lines {
        // JS `lines.slice(-count)`: a count of 0 keeps every line.
        let count = (max_lines as i64).min(lines.len() as i64 - i64::from(partial));
        let kept: &[&str] = match count {
            0 => &lines[..],
            n if n > 0 => &lines[lines.len() - n as usize..],
            n => &lines[(n.unsigned_abs() as usize).min(lines.len())..],
        };
        return Capture {
            output: format!("{}\n\n[showing lines {}-{total} of {total}; {suffix}", kept.join("\n"), total + 1 - kept.len() as u64),
            truncated: true,
        };
    }
    Capture { output: if page.is_empty() { "(no output)".into() } else { page }, truncated: false }
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

    /// The capture of a whole file `content` (as the service reads it), with its line starts tracked.
    fn capture_of(content: &[u8], max_lines: usize, max_bytes: usize) -> Capture {
        let mut line_starts = [0u64; 2];
        for (index, byte) in content.iter().enumerate() {
            if *byte == b'\n' {
                line_starts = [line_starts[1], index as u64 + 1];
            }
        }
        let written = Written {
            size: content.len() as u64,
            newlines: content.iter().filter(|b| **b == b'\n').count() as u64,
            line_starts,
        };
        let before = capture_start(written.size, max_bytes).saturating_sub(1) as usize;
        capture_tail(&content[before..], written, max_lines, max_bytes, "/f")
    }

    #[test]
    fn tail_truncation() {
        assert_eq!(capture_of(b"a\nb\n", 2000, 51200), Capture { output: "a\nb\n".into(), truncated: false });
        assert_eq!(capture_of(b"", 2000, 51200), Capture { output: "(no output)".into(), truncated: false });
        let c = capture_of(b"1\n2\n3\n4\n", 2, 51200);
        assert_eq!(c, Capture { output: "3\n4\n\n[showing lines 3-4 of 4; full output saved to /f]".into(), truncated: true });
        // A window that starts on a line boundary keeps its first line.
        let c = capture_of(b"aaaa\nx\ny", 2000, 3);
        assert_eq!(c.output, "x\ny\n\n[showing lines 2-3 of 3; full output saved to /f]");
        // One that starts mid-line drops the partial line so the numbers stay exact.
        let c = capture_of(b"aaaa\nbbx\ny", 2000, 3);
        assert_eq!(c.output, "y\n\n[showing lines 3-3 of 3; full output saved to /f]");
    }

    /// tool-shell.test.ts "drops the partial first line so the truncation note counts only whole lines" (#62141).
    #[test]
    fn drops_the_partial_first_line() {
        let lines: Vec<String> = (1..=10).map(|i| format!("{i}{}", "x".repeat(10 * 1024 - i.to_string().len()))).collect();
        let c = capture_of((lines.join("\n") + "\n").as_bytes(), 2000, 51200);
        assert!(c.output.starts_with("7xxx"));
        assert!(c.output.contains(&format!("{}\n\n[showing lines 7-10 of 10; full output saved to ", lines[9])));
        assert!(c.truncated);
    }

    /// "says how much of a single long line is shown".
    #[test]
    fn sizes_a_single_long_line() {
        let content = format!("head\n{}", "x".repeat(200 * 1024));
        let c = capture_of(content.as_bytes(), 2000, 51200);
        assert!(c.output.starts_with(&format!("{}\n\n", "x".repeat(51200))));
        assert!(c.output.ends_with("[showing the last 50 KB of line 2 (200 KB); full output saved to /f]"));
        // "puts the exit status after the truncation marker": a newline-terminated line of 51 KB.
        let content = format!("{}\n", "y".repeat(52 * 1024));
        let c = capture_of(content.as_bytes(), 2000, 51200);
        assert!(c.output.ends_with("[showing the last 50 KB of line 1 (52 KB); full output saved to /f]"), "{}", &c.output[51200..]);
        assert_eq!((format_size(1536 * 1024), format_size(1280 * 1024), format_size(3 * 1024 * 1024 + 1)), ("1.5 MB".into(), "1.3 MB".into(), "3.0 MB".into()));
        assert_eq!((format_size(511), format_size(512), format_size(1024 * 1024)), ("0 KB".into(), "1 KB".into(), "1024 KB".into()));
    }

    /// "starts the truncated view on a whole UTF-8 character".
    #[test]
    fn starts_on_a_whole_character() {
        let content = format!("{}€{}", "a".repeat(100), "b".repeat(51200 - 1));
        let c = capture_of(content.as_bytes(), 2000, 51200);
        assert!(c.output.starts_with(&format!("{}\n\n", "b".repeat(51200 - 1))));
        assert!(!c.output.contains('\u{FFFD}'));
    }

    /// "numbers lines exactly when the line limit truncates".
    #[test]
    fn numbers_lines_exactly() {
        let lines: Vec<String> = (1..=2500).map(|i| format!("line {i}")).collect();
        let c = capture_of((lines.join("\n") + "\n").as_bytes(), 2000, 51200);
        assert!(c.output.starts_with("line 501\n"));
        assert!(c.output.contains("line 2500\n\n[showing lines 501-2500 of 2500; full output saved to "));
    }

    /// tool-shell.test.ts "formats exit notices" (#62141).
    #[test]
    fn notices() {
        let out = |exit: Option<i64>, signal: Option<&str>, timeout: bool| Output {
            exit,
            signal: signal.map(String::from),
            timeout: timeout.then_some(true),
            ..Default::default()
        };
        assert_eq!(notice(&out(Some(7), None, false), None).unwrap(), "Exit code: 7");
        assert_eq!(notice(&out(Some(143), None, false), None).unwrap(), "Exit code: 143");
        assert_eq!(notice(&out(None, Some("SIGTERM"), false), None).unwrap(), "Exit code: 143 (SIGTERM)");
        assert_eq!(notice(&out(None, Some("SIGKILL"), false), None).unwrap(), "Exit code: 137 (SIGKILL)");
        assert_eq!(notice(&out(None, Some("SIGNOPE"), false), None).unwrap(), "Exit code: unknown");
        assert_eq!(
            notice(&out(None, Some("SIGTERM"), true), Some(120_000)).unwrap(),
            "Command timed out after 120 seconds. If it needs longer and isn't waiting for input, rerun with a larger timeout.\nExit code: 143 (SIGTERM)"
        );
        assert_eq!(
            notice(&out(None, None, true), None).unwrap(),
            "Command timed out. If it needs longer and isn't waiting for input, rerun with a larger timeout.\nExit code: unknown"
        );
        assert_eq!(
            notice(&out(None, None, true), Some(50)).unwrap(),
            "Command timed out after 0.05 seconds. If it needs longer and isn't waiting for input, rerun with a larger timeout.\nExit code: unknown"
        );
        assert!(notice(&out(Some(0), None, false), None).is_none());
        assert_eq!((signal_name(15), signal_name(9), signal_number("SIGTERM")), ("SIGTERM".to_string(), "SIGKILL".to_string(), Some(15)));
        let n = notification("sh_1", Some("sh_1"), "ls", "completed", "out", None);
        assert_eq!(n.text, "<shell id=\"sh_1\" state=\"completed\" command=\"ls\">\nout\n</shell>");
    }
}
