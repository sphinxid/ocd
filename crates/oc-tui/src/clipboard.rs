//! Clipboard: host tools (pbcopy / wl-copy / xclip / xsel / clip.exe) plus OSC 52 (spec §7.6).

use base64::Engine;
use std::io::Write;
use std::process::{Command, Stdio};

/// OSC 52 sequence writing `text` to the system clipboard.
pub fn osc52(text: &str) -> String {
    osc52_for(text, std::env::var_os("TMUX").is_some())
}

/// tmux needs passthrough wrapping: `ESC P tmux;` + the sequence with every ESC doubled + `ESC \`.
fn osc52_for(text: &str, tmux: bool) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let seq = format!("\x1b]52;c;{b64}\x07");
    if tmux { format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b")) } else { seq }
}

fn host_writers() -> Vec<(&'static str, Vec<&'static str>)> {
    if cfg!(target_os = "macos") {
        vec![("pbcopy", vec![])]
    } else if cfg!(windows) {
        vec![("clip", vec![])]
    } else {
        let mut v = Vec::new();
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            v.push(("wl-copy", vec![]));
        }
        v.push(("xclip", vec!["-selection", "clipboard"]));
        v.push(("xsel", vec!["--clipboard", "--input"]));
        v
    }
}

fn host_readers() -> Vec<(&'static str, Vec<&'static str>)> {
    if cfg!(target_os = "macos") {
        vec![("pbpaste", vec![])]
    } else if cfg!(windows) {
        vec![("powershell", vec!["-NoProfile", "-Command", "Get-Clipboard"])]
    } else {
        let mut v = Vec::new();
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            v.push(("wl-paste", vec!["--no-newline"]));
        }
        v.push(("xclip", vec!["-selection", "clipboard", "-o"]));
        v.push(("xsel", vec!["--clipboard", "--output"]));
        v
    }
}

#[cfg(test)]
thread_local! {
    /// Tests never touch the host clipboard; the last write is recorded here instead.
    pub static TEST_CLIPBOARD: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Write to the host clipboard (if a tool exists) and the terminal (OSC 52).
/// Success means either the host write succeeded or the terminal write was attempted.

pub fn write(text: &str) -> bool {
    let text: String = text.chars().filter(|c| *c != '\0').collect();
    #[cfg(test)]
    {
        TEST_CLIPBOARD.with(|c| *c.borrow_mut() = Some(text));
        return true;
    }
    #[cfg_attr(test, allow(unreachable_code))]
    let mut ok = false;
    for (cmd, args) in host_writers() {
        if let Ok(mut child) = Command::new(cmd).args(&args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                ok = true;
                break;
            }
        }
    }
    let mut out = std::io::stdout();
    let _ = out.write_all(osc52(&text).as_bytes());
    let _ = out.flush();
    ok || true
}

/// Read clipboard text via host tools.
pub fn read_text() -> Option<String> {
    for (cmd, args) in host_readers() {
        if let Ok(out) = Command::new(cmd).args(&args).stderr(Stdio::null()).output()
            && out.status.success()
        {
            let s = String::from_utf8_lossy(&out.stdout).to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// Read a PNG image from the clipboard as base64.
pub fn read_png() -> Option<String> {
    let attempts: Vec<(&str, Vec<&str>)> = if cfg!(target_os = "macos") {
        vec![]
    } else {
        vec![("wl-paste", vec!["--type", "image/png"]), ("xclip", vec!["-selection", "clipboard", "-t", "image/png", "-o"])]
    };
    for (cmd, args) in attempts {
        if let Ok(out) = Command::new(cmd).args(&args).stderr(Stdio::null()).output()
            && out.status.success()
            && out.stdout.starts_with(&[0x89, b'P', b'N', b'G'])
        {
            return Some(base64::engine::general_purpose::STANDARD.encode(&out.stdout));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn osc() {
        assert_eq!(super::osc52_for("hi", false), "\x1b]52;c;aGk=\x07");
        assert_eq!(super::osc52_for("hi", true), "\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\");
    }
}
