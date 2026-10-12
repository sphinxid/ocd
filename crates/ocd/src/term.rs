//! Terminal helpers: TTY detection and the `os.EOL` constant.

pub const EOL: &str = if cfg!(windows) { "\r\n" } else { "\n" };

#[cfg(unix)]
fn isatty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

#[cfg(not(unix))]
fn isatty(_fd: i32) -> bool {
    false
}

pub fn stdin_is_tty() -> bool {
    isatty(0)
}

pub fn stdout_is_tty() -> bool {
    isatty(1)
}

/// `process.stdout.columns` (None when not a TTY).
pub fn columns() -> Option<usize> {
    #[cfg(unix)]
    unsafe {
        if !stdout_is_tty() {
            return None;
        }
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            return Some(ws.ws_col as usize);
        }
        None
    }
    #[cfg(not(unix))]
    None
}

pub fn write_stdout(s: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

pub fn write_stderr(s: &str) {
    use std::io::Write;
    let mut out = std::io::stderr().lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

/// JS `localeCompare` approximation (ICU root collation): case-insensitive first, then lowercase
/// before uppercase, then code points.
pub fn locale_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let key = |s: &str| -> Vec<(u32, u8)> {
        s.chars()
            .map(|c| {
                let lower = c.to_lowercase().next().unwrap_or(c);
                (collation_weight(lower), if c.is_uppercase() { 1 } else { 0 })
            })
            .collect()
    };
    let (ka, kb) = (key(a), key(b));
    let primary = ka.iter().map(|k| k.0).cmp(kb.iter().map(|k| k.0));
    if primary != std::cmp::Ordering::Equal {
        return primary;
    }
    let tertiary = ka.iter().map(|k| k.1).cmp(kb.iter().map(|k| k.1));
    if tertiary != std::cmp::Ordering::Equal {
        return tertiary;
    }
    a.cmp(b)
}

/// Punctuation and symbols sort before digits, digits before letters (ICU root order).
fn collation_weight(c: char) -> u32 {
    const PUNCT: &str = "\t\n\u{b}\u{c}\r _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    let n = c as u32;
    if let Some(i) = PUNCT.find(c) {
        return 0x100 + i as u32;
    }
    if c.is_ascii_alphabetic() {
        0x20000 + n
    } else if c.is_ascii_digit() {
        0x10000 + n
    } else if c.is_alphanumeric() {
        0x30000 + n
    } else {
        n
    }
}

/// `new Date(ms).toLocaleString()` in the en-US default locale: `10/12/2026, 3:04:05 PM`.
pub fn locale_string(ms: i64) -> String {
    use chrono::{Local, TimeZone, Timelike};
    let Some(t) = Local.timestamp_millis_opt(ms).single() else { return "Invalid Date".into() };
    let (pm, h12) = t.hour12();
    format!(
        "{}, {}:{:02}:{:02} {}",
        t.format("%-m/%-d/%Y"),
        h12,
        t.minute(),
        t.second(),
        if pm { "PM" } else { "AM" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_compare_orders_like_icu() {
        let mut v = vec!["b", "B", "a", "_x", "1", "A"];
        v.sort_by(|a, b| locale_cmp(a, b));
        assert_eq!(v, vec!["_x", "1", "a", "A", "b", "B"]);
    }
}
