//! Entry and exit splash banners (`src/mini/splash.ts`). Both are scrollback commits.

use super::cell::{Buffer, Color, StyledChunk, attr};
use super::mono::mono_truncate;
use super::scrollback::Commit;
use super::text::{WrapMode, layout};
use super::theme::SplashTheme;
use crate::util::locale;

const SPLASH_TITLE_LIMIT: usize = 50;
const SPLASH_TITLE_FALLBACK: &str = "Untitled session";

/// The resume command printed by the exit splash (upstream: `opencode mini -s <id>`).
pub const RESUME_COMMAND: &str = "ocd mini -s";

/// `go.right` rows of the `go` logo, without the blank first row.
const GO_RIGHT: [&str; 3] = ["█▀▀█", "█__█", "▀▀▀▀"];

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct EntryLayout {
    pub label: String,
    pub version: String,
    pub path: String,
    pub metadata: String,
}

/// `entrySplashLayout`: admit the label, the leaf directory, the version and then ever longer path
/// suffixes, stopping at the first stage that does not fit.
pub fn entry_splash_layout(width: usize, version: &str, detail: Option<&str>, mono: bool) -> EntryLayout {
    let detail = detail.unwrap_or("");
    let segments: Vec<&str> = detail.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    let leaf = segments.last().copied().unwrap_or(detail);
    let separator = if mono { " - " } else { " · " };
    let ellipsis = if mono { "..." } else { "…" };
    let slash = if detail.contains('\\') { "\\" } else { "/" };
    let mut paths: Vec<String> = (1..segments.len()).map(|i| format!("{ellipsis}{slash}{}", segments[i..].join(slash))).collect();
    paths.reverse();
    paths.retain(|p| locale::width(p) < locale::width(detail));
    let mut out = EntryLayout { label: locale::take_width("oc mini", width), ..Default::default() };
    enum Stage {
        Label(String),
        Path(String),
        Version(String),
    }
    let mut stages = vec![Stage::Label(format!("{} oc mini", if mono { "[O]" } else { "▪" }))];
    if !leaf.is_empty() {
        stages.push(Stage::Path(leaf.to_string()));
    }
    if !version.is_empty() {
        stages.push(Stage::Version(version.to_string()));
    }
    for p in paths {
        stages.push(Stage::Path(p));
    }
    if !detail.is_empty() {
        stages.push(Stage::Path(detail.to_string()));
    }
    for stage in stages {
        let mut next = out.clone();
        match stage {
            Stage::Label(l) => next.label = l,
            Stage::Path(p) => next.path = p,
            Stage::Version(v) => next.version = v,
        }
        let metadata = format!(
            "{}{}",
            if next.version.is_empty() { String::new() } else { format!(" v{}", next.version) },
            if next.path.is_empty() { String::new() } else { format!("{separator}{}", next.path) }
        );
        if locale::width(&format!("{}{}", next.label, metadata)) > width {
            break;
        }
        next.metadata = metadata;
        out = next;
    }
    out
}

/// `entrySplash`: a blank row, then `▪ oc mini v<version> · <path>`.
pub fn entry_splash(width: usize, version: &str, detail: Option<&str>, mono: bool, theme: &SplashTheme) -> Commit {
    let width = width.max(1);
    let l = entry_splash_layout(width, version, detail, mono);
    let mut buf = Buffer::new(width as u16, 2);
    buf.draw_chunks(0, 1, &[StyledChunk::plain(l.label, theme.right), StyledChunk::plain(l.metadata, theme.left)], width);
    Commit { buf, row_columns: width, start_on_new_line: true, trailing_newline: false }
}

/// `title()`: collapse whitespace, fall back to `Untitled session`, truncate to 50.
pub fn splash_title(text: Option<&str>, mono: bool) -> String {
    let Some(text) = text else { return SPLASH_TITLE_FALLBACK.into() };
    let mut value = String::new();
    let mut gap = false;
    for c in text.trim().chars() {
        if matches!(c, ' ' | '\n' | '\r' | '\t') {
            gap = true;
            continue;
        }
        if gap && !value.is_empty() {
            value.push(' ');
        }
        value.push(c);
        gap = false;
    }
    if value.is_empty() {
        return SPLASH_TITLE_FALLBACK.into();
    }
    if mono { mono_truncate(&value, SPLASH_TITLE_LIMIT, true) } else { locale::truncate(&value, SPLASH_TITLE_LIMIT) }
}

fn put(buf: &mut Buffer, x: usize, y: usize, text: &str, fg: Color, bg: Color, attrs: u8) {
    let w = buf.width as usize;
    if x >= w {
        return;
    }
    buf.draw_chunks(x as i32, y as i32, &[StyledChunk { text: text.into(), fg, bg, attrs }], w - x);
}

/// `exitSplash`: the `go` logo with `Session <title>` and `Continue ocd mini -s <id>`; on narrow
/// terminals only the command, wrapped by character.
pub fn exit_splash(width: usize, title: Option<&str>, session_id: &str, mono: bool, show_session: bool, theme: &SplashTheme) -> Commit {
    let width = width.max(1);
    let title = splash_title(title, mono);
    let mark: Vec<&str> = if mono { vec!["[O]"] } else { GO_RIGHT.to_vec() };
    let top = 1;
    let body_left = mark[0].chars().count() + 2;
    let session = "Session  ";
    let label = "Continue ";
    let command = format!("{RESUME_COMMAND} {session_id}");
    let wide = body_left + locale::width(&format!("{label}{command}")) <= width;
    let command_height = if wide { 1 } else { locale::width(&command).div_ceil(width) };
    let height = top + if wide { mark.len().max(2) } else { command_height };
    let mut buf = Buffer::new(width as u16, height as u16);
    if wide {
        for (i, row) in mark.iter().enumerate() {
            for (x, c) in row.chars().enumerate() {
                match c {
                    '_' => put(&mut buf, x, top + i, " ", theme.left, theme.left_shadow, 0),
                    '^' => put(&mut buf, x, top + i, "▀", theme.left, theme.left_shadow, 0),
                    '~' => put(&mut buf, x, top + i, "▀", theme.left_shadow, Color::TRANSPARENT, 0),
                    c => put(&mut buf, x, top + i, &c.to_string(), theme.left, Color::TRANSPARENT, 0),
                }
            }
        }
        if show_session {
            put(&mut buf, body_left, top, session, theme.left, Color::TRANSPARENT, 0);
            put(&mut buf, body_left + session.len(), top, &title, theme.right, Color::TRANSPARENT, attr::BOLD);
        }
        put(&mut buf, body_left, top + 1, label, theme.left, Color::TRANSPARENT, 0);
    }
    let (cx, cy, cw) = if wide { (body_left + label.len(), top + 1, width - body_left - label.len()) } else { (0, top, width) };
    let rows = layout(&[StyledChunk { text: command, fg: theme.right, bg: Color::TRANSPARENT, attrs: attr::BOLD }], cw.max(1), WrapMode::Char, false);
    for (i, r) in rows.iter().take(command_height).enumerate() {
        buf.draw_chunks(cx as i32, (cy + i) as i32, &r.chunks, cw);
    }
    Commit { buf, row_columns: width, start_on_new_line: true, trailing_newline: false }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREVIEW: &str = "1.18.4-preview.abcd1234567890";
    const SESSION: &str = "ses_fac1eb1b0ffeOk15TDttH2E1Oy";

    fn theme() -> SplashTheme {
        SplashTheme { left: Color::indexed(8, Color::rgb(0x66, 0x66, 0x66)), right: Color::default_fg(Color::rgb(0xcc, 0xcc, 0xcc)), left_shadow: Color::default_bg(Color::rgb(0x11, 0x11, 0x11)) }
    }

    fn rows(c: &Commit) -> Vec<String> {
        (0..c.buf.height).map(|y| c.buf.row_text(y)).collect()
    }

    #[test]
    fn entry_renders_metadata() {
        for (width, mono, detail, metadata) in [
            (80, false, "~/src/wt/oc-mini-v2", " vlocal · ~/src/wt/oc-mini-v2"),
            (80, true, "~/src/wt/oc-mini-v2", " vlocal - ~/src/wt/oc-mini-v2"),
            (20, false, "/home/研究/長いディレクトリ/画面/設定/界e\u{301}🙂", " · 界e\u{301}🙂"),
        ] {
            let c = entry_splash(width, "local", Some(detail), mono, &theme());
            let label = format!("{} oc mini", if mono { "[O]" } else { "▪" });
            assert_eq!(rows(&c), vec![String::new(), format!("{label}{metadata}")]);
            assert_eq!(c.buf.get(0, 1).fg, theme().right);
            assert_eq!(c.buf.get(locale::width(&label) as u16, 1).fg, theme().left);
            assert!(!c.trailing_newline && c.start_on_new_line);
        }
    }

    #[test]
    fn entry_progressive_layout() {
        for (width, mono, version, expected) in [
            (20, false, "local", "▪ oc mini"),
            (22, false, "local", "▪ oc mini · oc-mini-v2"),
            (29, false, "local", "▪ oc mini vlocal · oc-mini-v2"),
            (31, false, "local", "▪ oc mini vlocal · …/oc-mini-v2"),
            (20, false, PREVIEW, "▪ oc mini"),
            (24, false, PREVIEW, "▪ oc mini · oc-mini-v2"),
            (24, true, PREVIEW, "[O] oc mini - oc-mini-v2"),
            (32, false, PREVIEW, "▪ oc mini · oc-mini-v2"),
        ] {
            let l = entry_splash_layout(width, version, Some("~/src/wt/oc-mini-v2"), mono);
            assert_eq!(format!("{}{}", l.label, l.metadata), expected, "width {width}");
        }
    }

    #[test]
    fn entry_absent_and_root_locations() {
        for detail in [None, Some(""), Some("/"), Some("~/"), Some("C:\\projects\\mini\\")] {
            let l = entry_splash_layout(80, "local", detail, false);
            let d = detail.unwrap_or("");
            assert_eq!(l.metadata, format!(" vlocal{}", if d.is_empty() { String::new() } else { format!(" · {d}") }));
        }
    }

    #[test]
    fn entry_preserves_admitted_information() {
        let suffix = |v: &str| {
            let v = v.strip_prefix("…/").or_else(|| v.strip_prefix("…\\")).or_else(|| v.strip_prefix(".../")).or_else(|| v.strip_prefix("...\\")).unwrap_or(v);
            let mut s = String::new();
            let mut prev_slash = false;
            for c in v.chars() {
                let slash = c == '/' || c == '\\';
                if slash && prev_slash {
                    continue;
                }
                s.push(if slash { '/' } else { c });
                prev_slash = slash;
            }
            s.strip_suffix('/').map(str::to_string).unwrap_or(s)
        };
        for mono in [false, true] {
            for detail in [None, Some(""), Some("/"), Some("~/"), Some("x/y"), Some("a/b/c"), Some("~/src/wt/oc-mini-v2"), Some("C:\\projects\\mini\\"), Some("/home/研究/画面/界e\u{301}🙂"), Some("~/project-directory-that-cannot-meaningfully-fit")] {
                for version in ["", "local", PREVIEW] {
                    let mut previous = entry_splash_layout(0, version, detail, mono);
                    for width in 1..=160 {
                        let l = entry_splash_layout(width, version, detail, mono);
                        assert!(locale::width(&format!("{}{}", l.label, l.metadata)) <= width);
                        assert!(l.version.is_empty() || l.version == version);
                        if !previous.version.is_empty() {
                            assert_eq!(l.version, previous.version);
                        }
                        if !previous.path.is_empty() {
                            assert!(!l.path.is_empty());
                            assert!(suffix(&l.path).ends_with(&suffix(&previous.path)));
                        }
                        if !l.version.is_empty() && detail.is_some_and(|d| !d.is_empty()) {
                            assert!(!l.path.is_empty());
                        }
                        if !l.path.is_empty() {
                            assert!(suffix(detail.unwrap()).ends_with(&suffix(&l.path)));
                        }
                        let marked = format!("{} oc mini", if mono { "[O]" } else { "▪" });
                        let expected = if locale::width(&marked) <= width { marked } else { "oc mini"[..width.min(7)].to_string() };
                        assert_eq!(l.label, expected);
                        previous = l;
                    }
                    assert_eq!(previous.path, detail.unwrap_or(""));
                    assert_eq!(previous.version, version);
                }
            }
        }
    }

    #[test]
    fn entry_skips_longer_abbreviations() {
        for mono in [false, true] {
            let label = format!("{} oc mini", if mono { "[O]" } else { "▪" });
            let metadata = if mono { " vlocal - a/b/c" } else { " vlocal · a/b/c" };
            let l = entry_splash_layout(locale::width(&format!("{label}{metadata}")), "local", Some("a/b/c"), mono);
            assert_eq!((l.label.as_str(), l.metadata.as_str(), l.path.as_str()), (label.as_str(), metadata, "a/b/c"));
        }
    }

    #[test]
    fn exit_retains_resume_command() {
        for (width, show) in [(16, true), (56, true), (80, true), (80, false)] {
            for mono in [false, true] {
                let c = exit_splash(width, Some("Review mini layout"), SESSION, mono, show, &theme());
                let r = rows(&c);
                assert_eq!(r[0], "");
                let command = format!("{RESUME_COMMAND} {SESSION}");
                // Upstream's longer `opencode mini -s` command is narrow below 80 columns; ours fits at 56.
                let wide = (if mono { 3 } else { 4 }) + 2 + "Continue ".len() + command.len() <= width;
                let command_rows: Vec<String> = if wide { vec![r[2][r[2].find("ocd").unwrap()..].to_string()] } else { r[1..].to_vec() };
                let n = command_rows.len();
                let joined: String = command_rows.iter().enumerate().map(|(i, row)| if i + 1 < n { format!("{row:<width$}") } else { row.clone() }).collect();
                assert_eq!(joined, command);
                if wide {
                    assert!(r[1].starts_with(if mono { "[O]" } else { "█▀▀█" }));
                    assert_eq!(r[1].contains("Session  Review mini layout"), show);
                    assert!(r[2].contains(&format!("Continue {command}")));
                } else {
                    let all = r.join("\n");
                    assert!(!all.contains("Session") && !all.contains("Continue") && !all.contains('█') && !all.contains("[O]"));
                    assert_eq!(r.len(), 1 + command.len().div_ceil(width));
                }
                if mono {
                    assert!(r.join("").chars().all(|c| (' '..='~').contains(&c)));
                }
            }
        }
    }
}
