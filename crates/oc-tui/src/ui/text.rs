//! Styled text primitives: segments, lines, wrapping and drawing into a ratatui buffer.

use crate::theme::Rgba;
use crate::util::locale::{graphemes, width};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

/// Placeholder glyph replaced by the current spinner frame at draw time (keeps cached layouts
/// valid while spinners animate).
pub const SPINNER_GLYPH: char = '\u{E000}';
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn color(c: Rgba) -> Color {
    c.tui()
}

pub fn fg(c: Rgba) -> Style {
    Style::default().fg(c.tui())
}

pub fn fg_bg(f: Rgba, b: Rgba) -> Style {
    let mut s = Style::default().fg(f.tui());
    if b.a > 0 {
        s = s.bg(b.tui());
    }
    s
}

pub fn bold(s: Style) -> Style {
    s.add_modifier(Modifier::BOLD)
}

/// A run of text with one style.
#[derive(Clone, Debug, PartialEq)]
pub struct Seg {
    pub text: String,
    pub style: Style,
}

impl Seg {
    pub fn new(text: impl Into<String>, style: Style) -> Seg {
        Seg { text: text.into(), style }
    }
    pub fn width(&self) -> usize {
        width(&self.text)
    }
}

pub fn seg(text: impl Into<String>, style: Style) -> Seg {
    Seg::new(text, style)
}

/// Click target attached to a rendered line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Click {
    ToggleGroup(String),
    ToggleTool(String),
    ToggleError(String),
    OpenMessage(String),
    PendingSteer(String),
    Navigate(String),
    Execute(String, String),
    RevertClear,
    JumpBottom,
    QueuedDialog,
    ToggleThinking(String),
    ShellOutput(String),
    Custom(String),
}

/// One rendered row. `bg` fills the whole line width when set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SLine {
    pub segs: Vec<Seg>,
    pub bg: Option<Color>,
    pub click: Option<Click>,
}

impl SLine {
    pub fn new(segs: Vec<Seg>) -> SLine {
        SLine { segs, bg: None, click: None }
    }
    pub fn blank() -> SLine {
        SLine::default()
    }
    pub fn with_bg(mut self, bg: Option<Color>) -> SLine {
        self.bg = bg;
        self
    }
    pub fn on_click(mut self, c: Option<Click>) -> SLine {
        self.click = c;
        self
    }
    pub fn width(&self) -> usize {
        self.segs.iter().map(|s| s.width()).sum()
    }
    pub fn text(&self) -> String {
        self.segs.iter().map(|s| s.text.as_str()).collect()
    }
    /// Prepend segments (e.g. a border bar and padding).
    pub fn prefixed(mut self, prefix: &[Seg]) -> SLine {
        let mut segs = prefix.to_vec();
        segs.append(&mut self.segs);
        self.segs = segs;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wrap {
    Word,
    Char,
    None,
}

/// A styled grapheme.
#[derive(Clone, Debug)]
struct Cell {
    g: String,
    style: Style,
    w: usize,
}

fn cells(segs: &[Seg]) -> Vec<Cell> {
    let mut out = Vec::new();
    for s in segs {
        for g in graphemes(&s.text) {
            out.push(Cell { g: g.to_string(), style: s.style, w: width(g) });
        }
    }
    out
}

fn to_segs(cells: &[Cell]) -> Vec<Seg> {
    let mut out: Vec<Seg> = Vec::new();
    for c in cells {
        match out.last_mut() {
            Some(last) if last.style == c.style => last.text.push_str(&c.g),
            _ => out.push(Seg::new(c.g.clone(), c.style)),
        }
    }
    out
}

/// Wrap styled segments into lines of at most `w` cells. `\n` forces a break.
pub fn wrap(segs: &[Seg], w: usize, mode: Wrap) -> Vec<Vec<Seg>> {
    let w = w.max(1);
    let all = cells(segs);
    // Split on hard newlines.
    let mut paragraphs: Vec<Vec<Cell>> = vec![Vec::new()];
    for c in all {
        if c.g == "\n" || c.g == "\r\n" {
            paragraphs.push(Vec::new());
        } else if c.g == "\t" {
            for _ in 0..2 {
                paragraphs.last_mut().unwrap().push(Cell { g: " ".into(), style: c.style, w: 1 });
            }
        } else {
            paragraphs.last_mut().unwrap().push(c);
        }
    }
    let mut out = Vec::new();
    for p in paragraphs {
        match mode {
            Wrap::None => {
                let mut line = Vec::new();
                let mut used = 0;
                for c in p {
                    if used + c.w > w {
                        break;
                    }
                    used += c.w;
                    line.push(c);
                }
                out.push(to_segs(&line));
            }
            Wrap::Char => {
                let mut line: Vec<Cell> = Vec::new();
                let mut used = 0;
                for c in p {
                    if used + c.w > w && !line.is_empty() {
                        out.push(to_segs(&line));
                        line.clear();
                        used = 0;
                    }
                    used += c.w;
                    line.push(c);
                }
                out.push(to_segs(&line));
            }
            Wrap::Word => {
                // Tokenize into words (non-space runs) and spaces.
                let mut tokens: Vec<(bool, Vec<Cell>)> = Vec::new();
                for c in p {
                    let space = c.g == " ";
                    match tokens.last_mut() {
                        Some((s, v)) if *s == space => v.push(c),
                        _ => tokens.push((space, vec![c])),
                    }
                }
                let mut line: Vec<Cell> = Vec::new();
                let mut used = 0;
                for (space, tok) in tokens {
                    let tw: usize = tok.iter().map(|c| c.w).sum();
                    if space {
                        if used + tw <= w {
                            used += tw;
                            line.extend(tok);
                        } else {
                            // Break at the space; drop it.
                            out.push(to_segs(&line));
                            line.clear();
                            used = 0;
                        }
                        continue;
                    }
                    if used + tw <= w {
                        used += tw;
                        line.extend(tok);
                        continue;
                    }
                    if tw <= w {
                        // Trim trailing spaces of the current line.
                        while line.last().is_some_and(|c| c.g == " ") {
                            line.pop();
                        }
                        if !line.is_empty() {
                            out.push(to_segs(&line));
                        }
                        line = tok;
                        used = tw;
                        continue;
                    }
                    // Word longer than the width: break by grapheme.
                    for c in tok {
                        if used + c.w > w && !line.is_empty() {
                            out.push(to_segs(&line));
                            line.clear();
                            used = 0;
                        }
                        used += c.w;
                        line.push(c);
                    }
                }
                out.push(to_segs(&line));
            }
        }
    }
    out
}

/// Wrap and produce lines with a prefix on each line.
pub fn wrap_lines(segs: &[Seg], w: usize, mode: Wrap, prefix: &[Seg]) -> Vec<SLine> {
    let pw: usize = prefix.iter().map(|s| s.width()).sum();
    wrap(segs, w.saturating_sub(pw).max(1), mode).into_iter().map(|l| SLine::new(l).prefixed(prefix)).collect()
}

/// Truncate segments to `w` cells (no ellipsis).
pub fn clip(segs: &[Seg], w: usize) -> Vec<Seg> {
    wrap(segs, w, Wrap::None).into_iter().next().unwrap_or_default()
}

/// Draw text at (x, y) clipped to `max` cells. Returns cells written.
pub fn put(buf: &mut Buffer, x: u16, y: u16, text: &str, style: Style, max: usize) -> usize {
    let area = buf.area;
    if y >= area.bottom() || x >= area.right() {
        return 0;
    }
    let mut cx = x as usize;
    let limit = (x as usize + max).min(area.right() as usize);
    for g in graphemes(text) {
        // A control character would reach the terminal raw and move its cursor: a tab takes one
        // cell, anything else none.
        let g = if g.chars().any(char::is_control) {
            if g == "\t" { " " } else { continue }
        } else {
            g
        };
        let gw = width(g);
        if gw == 0 {
            continue;
        }
        if cx + gw > limit {
            break;
        }
        let cell = &mut buf[(cx as u16, y)];
        cell.set_symbol(g);
        cell.set_style(style);
        for i in 1..gw {
            let c = &mut buf[((cx + i) as u16, y)];
            c.set_symbol("");
            c.set_style(style);
        }
        cx += gw;
    }
    cx - x as usize
}

pub fn put_segs(buf: &mut Buffer, x: u16, y: u16, segs: &[Seg], max: usize) -> usize {
    let mut used = 0;
    for s in segs {
        if used >= max {
            break;
        }
        used += put(buf, x + used as u16, y, &s.text, s.style, max - used);
    }
    used
}

/// Fill a rect with a background color (keeping symbols blank).
pub fn fill(buf: &mut Buffer, area: Rect, bg: Color) {
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let c = &mut buf[(x, y)];
            // An opaque box: nothing of the cell underneath (bold, underline) shows through.
            c.reset();
            c.set_symbol(" ");
            c.set_style(Style::default().bg(bg).fg(bg));
        }
    }
}

/// Set the background of existing cells without clearing them.
pub fn set_bg(buf: &mut Buffer, area: Rect, bg: Color) {
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_bg(bg);
        }
    }
}

/// Draw lines into `area`, starting at line index `offset`. Spinner placeholders are replaced
/// with `spinner`. Returns hit regions for clickable lines.
pub fn draw_lines(buf: &mut Buffer, area: Rect, lines: &[SLine], offset: usize, spinner: &str) -> Vec<(Rect, Click)> {
    draw_line_iter(buf, area, lines.iter().skip(offset), spinner)
}

/// [`draw_lines`] over borrowed rows (e.g. a window into a cached transcript).
pub fn draw_line_iter<'a>(buf: &mut Buffer, area: Rect, lines: impl Iterator<Item = &'a SLine>, spinner: &str) -> Vec<(Rect, Click)> {
    let mut hits = Vec::new();
    for (i, line) in lines.take(area.height as usize).enumerate() {
        let y = area.y + i as u16;
        if let Some(bg) = line.bg {
            fill(buf, Rect::new(area.x, y, area.width, 1), bg);
        }
        let mut x = area.x;
        for s in &line.segs {
            if x >= area.right() {
                break;
            }
            let max = (area.right() - x) as usize;
            let text = if s.text.contains(SPINNER_GLYPH) {
                s.text.replace(SPINNER_GLYPH, spinner)
            } else {
                s.text.clone()
            };
            let mut style = s.style;
            if style.bg.is_none()
                && let Some(bg) = line.bg
            {
                style = style.bg(bg);
            }
            x += put(buf, x, y, &text, style, max) as u16;
        }
        if let Some(c) = &line.click {
            hits.push((Rect::new(area.x, y, area.width, 1), c.clone()));
        }
    }
    hits
}

/// Alpha-blend every cell of `area` toward black (`RGBA(0,0,0,alpha)` overlays).
pub fn dim(buf: &mut Buffer, area: Rect, alpha: u8, base_bg: Rgba) {
    let a = alpha as f64 / 255.0;
    let mix = |c: Color, fallback: Rgba| -> Color {
        let (r, g, b) = match c {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => (fallback.r, fallback.g, fallback.b),
        };
        let f = |v: u8| ((v as f64) * (1.0 - a)).round() as u8;
        Color::Rgb(f(r), f(g), f(b))
    };
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            let bg = mix(cell.bg, base_bg);
            let fg = mix(cell.fg, Rgba::rgb(0xee, 0xee, 0xee));
            cell.set_bg(bg);
            cell.set_fg(fg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: Vec<Vec<Seg>>) -> Vec<String> {
        lines.into_iter().map(|l| l.into_iter().map(|s| s.text).collect()).collect()
    }

    #[test]
    fn put_keeps_control_characters_off_the_screen() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        let written = put(&mut buf, 0, 0, "a\tb\r\x1bc", Style::default(), 10);
        assert_eq!(written, 4);
        let row: String = (0..10).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row, "a bc      ");
    }

    #[test]
    fn word_wrap() {
        let s = vec![Seg::new("hello world this is text", Style::default())];
        assert_eq!(texts(wrap(&s, 11, Wrap::Word)), vec!["hello world", "this is", "text"]);
        let s = vec![Seg::new("abcdefghij", Style::default())];
        assert_eq!(texts(wrap(&s, 4, Wrap::Word)), vec!["abcd", "efgh", "ij"]);
        let s = vec![Seg::new("a\nb", Style::default())];
        assert_eq!(texts(wrap(&s, 4, Wrap::Word)), vec!["a", "b"]);
        assert_eq!(texts(wrap(&[Seg::new("abcdef", Style::default())], 3, Wrap::Char)), vec!["abc", "def"]);
        assert_eq!(texts(wrap(&[Seg::new("abcdef", Style::default())], 3, Wrap::None)), vec!["abc"]);
    }

    #[test]
    fn styles_preserved() {
        let s = vec![Seg::new("ab ", Style::default().fg(Color::Red)), Seg::new("cd", Style::default())];
        let out = wrap(&s, 10, Wrap::Word);
        assert_eq!(out[0].len(), 2);
    }
}
