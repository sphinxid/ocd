//! Logo data (`src/logo.ts`, verbatim), logo rendering and the exit epilogue
//! (`src/util/presentation.ts`).

use super::text::{SLine, Seg};
use crate::theme::{Theme, tint};
use ratatui::style::{Modifier, Style};

pub const LOGO_LEFT: [&str; 4] = ["                   ", "█▀▀█ █▀▀█ █▀▀█ █▀▀▄", "█__█ █__█ █^^^ █__█", "▀▀▀▀ █▀▀▀ ▀▀▀▀ ▀~~▀"];
/// "code-d": upstream's "code" plus a hyphen (styled like the `e` crossbar) and the font's `d` (ocd branding).
pub const LOGO_RIGHT: [&str; 4] = [
    "             ▄             ▄",
    "█▀▀▀ █▀▀█ █▀▀█ █▀▀█     █▀▀█",
    "█___ █__█ █__█ █^^^ ^^^ █__█",
    "▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀     ▀▀▀▀",
];
pub const GO_LEFT: [&str; 4] = ["    ", "█▀▀▀", "█_^█", "▀▀▀▀"];
pub const GO_RIGHT: [&str; 4] = ["    ", "█▀▀█", "█__█", "▀▀▀▀"];

pub fn logo_line(text: &str, t: &Theme, fg: crate::theme::Rgba, bold: bool) -> Vec<Seg> {
    let shadow = tint(t.background.base, fg, 0.25);
    let mut base = Style::default().fg(fg.tui());
    if bold {
        base = base.add_modifier(Modifier::BOLD);
    }
    text.chars()
        .map(|c| match c {
            '_' => Seg::new(" ", base.bg(shadow.tui())),
            '^' => Seg::new("▀", base.bg(shadow.tui())),
            '~' => Seg::new("▀", Style::default().fg(shadow.tui())),
            ',' => Seg::new("▄", Style::default().fg(shadow.tui())),
            other => Seg::new(other.to_string(), base),
        })
        .collect()
}

/// Logo lines for the terminal size (`src/component/logo.tsx` breakpoints).
pub fn logo(t: &Theme, w: u16, h: u16) -> Vec<SLine> {
    if h < 12 {
        return vec![];
    }
    if w < 31 {
        return GO_RIGHT[1..].iter().map(|l| SLine::new(logo_line(l, t, t.text.base, true))).collect();
    }
    if w < 53 {
        let mut out: Vec<SLine> = LOGO_LEFT[1..].iter().map(|l| SLine::new(logo_line(l, t, t.text.muted, false))).collect();
        out.extend(LOGO_RIGHT.iter().map(|l| SLine::new(logo_line(l, t, t.text.base, true))));
        return out;
    }
    (0..4)
        .map(|i| {
            let mut segs = logo_line(LOGO_LEFT[i], t, t.text.muted, false);
            segs.push(Seg::new(" ", Style::default()));
            segs.extend(logo_line(LOGO_RIGHT[i], t, t.text.base, true));
            SLine::new(segs)
        })
        .collect()
}

fn ansi_logo_half(line: &str, fg: &str, shadow: &str, bg: &str) -> String {
    let reset = "\x1b[0m";
    let mut out = String::new();
    for c in line.chars() {
        match c {
            '_' => out.push_str(&format!("{bg} {reset}")),
            '^' => out.push_str(&format!("{fg}{bg}▀{reset}")),
            '~' => out.push_str(&format!("{shadow}▀{reset}")),
            ' ' => out.push(' '),
            other => out.push_str(&format!("{fg}{other}{reset}")),
        }
    }
    out
}

/// `sessionEpilogue({title, sessionID})`.
pub fn session_epilogue(title: &str, session_id: &str) -> String {
    let reset = "\x1b[0m";
    let bold = "\x1b[1m";
    let dim = "\x1b[90m";
    let mut lines = Vec::new();
    for i in 0..4 {
        let left = ansi_logo_half(LOGO_LEFT[i], "\x1b[90m", "\x1b[38;5;235m", "\x1b[48;5;235m");
        let right = ansi_logo_half(LOGO_RIGHT[i], reset, "\x1b[38;5;238m", "\x1b[48;5;238m");
        lines.push(format!("  {left} {right}"));
    }
    lines.push(String::new());
    lines.push(format!("  {dim}{:<10}{reset}{bold}{title}{reset}", "Session"));
    lines.push(format!("  {dim}{:<10}{reset}{bold}ocd -s {session_id}{reset}", "Continue"));
    lines.push(String::new());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    #[test]
    fn epilogue_format() {
        let e = super::session_epilogue("Fix bug", "ses_1");
        assert!(e.contains("\x1b[90mSession   \x1b[0m\x1b[1mFix bug\x1b[0m"));
        assert!(e.contains("Continue  \x1b[0m\x1b[1mocd -s ses_1"));
        assert_eq!(e.lines().count(), 7);
    }
}
