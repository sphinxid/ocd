//! Terminal palette probing (OSC 4/10/11 queries terminated by a DA1 request) and parsing.

use crate::theme::TerminalColors;

/// Parse an OSC color reply payload like `rgb:ffff/0000/8080` into `#ff0080`.
pub fn parse_rgb(spec: &str) -> Option<String> {
    let rest = spec.strip_prefix("rgb:").or_else(|| spec.strip_prefix("rgba:"))?;
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() < 3 {
        return None;
    }
    let conv = |p: &str| -> Option<u8> {
        let v = u32::from_str_radix(p, 16).ok()?;
        let max = (1u32 << (4 * p.len() as u32)) - 1;
        Some(((v as f64 / max as f64) * 255.0).round() as u8)
    };
    Some(format!("#{:02x}{:02x}{:02x}", conv(parts[0])?, conv(parts[1])?, conv(parts[2])?))
}

/// Parse all OSC 4/10/11 replies in `data`.
pub fn parse_replies(data: &str) -> TerminalColors {
    let mut colors = TerminalColors { palette: vec![None; 16], ..Default::default() };
    let re = regex::Regex::new(r"\x1b\](4;(\d+)|10|11);([^\x07\x1b]+)(?:\x07|\x1b\\)").unwrap();
    for c in re.captures_iter(data) {
        let Some(hex) = parse_rgb(&c[3]) else { continue };
        match &c[1] {
            "10" => colors.default_foreground = Some(hex),
            "11" => colors.default_background = Some(hex),
            _ => {
                if let Some(i) = c.get(2).and_then(|m| m.as_str().parse::<usize>().ok())
                    && i < 16
                {
                    colors.palette[i] = Some(hex);
                }
            }
        }
    }
    colors
}

/// Query the terminal palette. Must be called in raw mode before the input reader starts.
#[cfg(unix)]
pub fn probe(timeout_ms: u64) -> Option<TerminalColors> {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    let mut out = std::io::stdout();
    let mut q = String::from("\x1b]10;?\x07\x1b]11;?\x07");
    for i in 0..16 {
        q.push_str(&format!("\x1b]4;{i};?\x07"));
    }
    q.push_str("\x1b[c");
    out.write_all(q.as_bytes()).ok()?;
    out.flush().ok()?;
    let stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let mut data: Vec<u8> = Vec::new();
    let da1 = regex::bytes::Regex::new(r"\x1b\[\?[0-9;]*c").unwrap();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis() as i32) };
        if r <= 0 {
            break;
        }
        let mut buf = [0u8; 4096];
        let n = stdin.lock().read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if da1.is_match(&data) {
            break;
        }
    }
    let text = String::from_utf8_lossy(&data);
    let colors = parse_replies(&text);
    if colors.default_background.is_none() && colors.palette.iter().all(|p| p.is_none()) {
        return None;
    }
    Some(colors)
}

#[cfg(not(unix))]
pub fn probe(_timeout_ms: u64) -> Option<TerminalColors> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies() {
        assert_eq!(parse_rgb("rgb:ffff/0000/8080").as_deref(), Some("#ff0080"));
        assert_eq!(parse_rgb("rgb:ff/00/80").as_deref(), Some("#ff0080"));
        let c = parse_replies("\x1b]11;rgb:1010/1010/1010\x1b\\\x1b]10;rgb:dddd/dddd/dddd\x07\x1b]4;1;rgb:cccc/0000/0000\x07\x1b[?62;c");
        assert_eq!(c.default_background.as_deref(), Some("#101010"));
        assert_eq!(c.default_foreground.as_deref(), Some("#dddddd"));
        assert_eq!(c.palette[1].as_deref(), Some("#cc0000"));
    }
}
