//! Structured tool snapshots in scrollback (`RunEntryContent` in `scrollback.writer.tsx`): numbered code
//! for writes, unified patch diffs (`component/patch-diff.tsx` over OpenTUI `DiffRenderable`), subagent
//! task cards, todo lists and question answers. Static snapshots render code unhighlighted, as upstream's
//! synchronous writer does.

use super::cell::{Buffer, Color, Glyph, StyledChunk, attr, str_width};
use super::markdown::blit;
use super::text::{WrapMode, layout};
use super::theme::{MiniSyntax, RunTheme};
use super::tool::{DiffItem, ToolSnapshot};

fn text(content: &str, fg: Color, attrs: u8, width: usize, mode: WrapMode) -> Buffer {
    let lines = layout(&[StyledChunk { text: content.to_string(), fg, bg: Color::TRANSPARENT, attrs }], width.max(1), mode, false);
    let mut buf = Buffer::new(width.max(1) as u16, lines.len().max(1) as u16);
    for (i, l) in lines.iter().enumerate() {
        buf.draw_chunks(0, i as i32, &l.chunks, width);
    }
    buf
}

/// Stack buffers vertically with `gap` rows between them.
fn column(parts: Vec<Buffer>, width: usize, gap: usize) -> Buffer {
    let n = parts.len();
    let h: usize = parts.iter().map(|b| b.height as usize).sum::<usize>() + gap * n.saturating_sub(1);
    let mut out = Buffer::new(width.max(1) as u16, h.max(1) as u16);
    let mut y = 0;
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            y += gap;
        }
        blit(&mut out, p, 0, y as i32);
        y += p.height as usize;
    }
    out
}

fn padded(inner: &Buffer, width: usize, left: usize) -> Buffer {
    let mut out = Buffer::new(width.max(1) as u16, inner.height);
    blit(&mut out, inner, left as i32, 0);
    out
}

/// A gutter + content view (`LineNumberRenderable` over a code target). `rows` are the content's
/// logical lines already wrapped: `(logical index, laid-out row)`.
struct Gutter {
    width: usize,
    after_width: usize,
    padding_right: usize,
}

fn digits(n: usize) -> usize {
    n.max(1).to_string().len()
}

/// `PatchDiff`'s `gutter.minDigits` (OpenTUI 0.7): hunks and split sides share one width, the digits of the last
/// line any hunk header's range reaches on either side.
fn min_digits(patch: &str) -> usize {
    let re = regex::Regex::new(r"(?m)^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").unwrap();
    let mut max: i64 = 1;
    for c in re.captures_iter(patch) {
        let n = |i: usize, default: i64| c.get(i).and_then(|m| m.as_str().parse::<i64>().ok()).unwrap_or(default);
        max = max.max(n(1, 0) + n(2, 1) - 1).max(n(3, 0) + n(4, 1) - 1);
    }
    max.to_string().len()
}

/// Paint one gutter row: cleared to white spaces, then the number and the `after` sign.
fn paint_gutter(buf: &mut Buffer, y: usize, g: &Gutter, number: Option<usize>, sign: Option<(&str, Color)>, fg: Color) {
    for x in 0..g.width {
        buf.set_blend(x as i32, y as i32, Glyph::Text(" ".into()), Color::WHITE, Color::TRANSPARENT, 0);
    }
    if let Some(n) = number {
        let s = n.to_string();
        let x = g.width as i64 - g.after_width as i64 - g.padding_right as i64 - s.len() as i64;
        if x >= 1 {
            buf.draw_chunks(x as i32, y as i32, &[StyledChunk::plain(s, fg)], g.width);
        }
    }
    if let Some((sign, color)) = sign {
        let x = g.width - g.padding_right - g.after_width;
        buf.draw_chunks(x as i32, y as i32, &[StyledChunk::plain(sign, color)], g.width);
    }
}

/// Numbered code (write snapshot): `minWidth 3`, `paddingRight 1`, char wrap.
fn numbered_code(content: &str, width: usize, theme: &RunTheme) -> Buffer {
    let lines: Vec<&str> = content.split('\n').collect();
    let d = digits(lines.len());
    let g = Gutter { width: (1 + d + 1).max(3), after_width: 0, padding_right: 1 };
    let code_w = width.saturating_sub(g.width).max(1);
    let mut rows: Vec<(usize, Vec<StyledChunk>)> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        for (k, v) in layout(&[StyledChunk::plain(*l, theme.block.text)], code_w, WrapMode::Char, false).into_iter().enumerate() {
            rows.push((if k == 0 { i + 1 } else { 0 }, v.chunks));
        }
    }
    let mut buf = Buffer::new(width as u16, rows.len().max(1) as u16);
    for (y, (n, chunks)) in rows.iter().enumerate() {
        paint_gutter(&mut buf, y, &g, (*n > 0).then_some(*n), None, theme.block.muted);
        buf.draw_chunks(g.width as i32, y as i32, chunks, code_w);
    }
    buf
}

struct DLine {
    sign: char,
    text: String,
    number: usize,
}

struct Hunk {
    header: Option<String>,
    lines: Vec<DLine>,
}

/// `splitPatchHunks` + `parsePatch` (jsdiff) for the unified view.
fn hunks(patch: &str) -> Vec<Hunk> {
    let re = regex::Regex::new(r"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@.*$").unwrap();
    let mut out: Vec<Hunk> = Vec::new();
    let (mut old, mut new) = (0usize, 0usize);
    let multi = patch.lines().filter(|l| re.is_match(l)).count() > 1;
    for line in patch.split('\n') {
        if let Some(c) = re.captures(line) {
            old = c[1].parse().unwrap_or(1);
            new = c[2].parse().unwrap_or(1);
            out.push(Hunk { header: multi.then(|| line.to_string()), lines: Vec::new() });
            continue;
        }
        let Some(h) = out.last_mut() else { continue };
        match line.chars().next() {
            Some('+') => {
                h.lines.push(DLine { sign: '+', text: line[1..].to_string(), number: new });
                new += 1;
            }
            Some('-') => {
                h.lines.push(DLine { sign: '-', text: line[1..].to_string(), number: old });
                old += 1;
            }
            Some(' ') => {
                h.lines.push(DLine { sign: ' ', text: line[1..].to_string(), number: new });
                old += 1;
                new += 1;
            }
            _ => {}
        }
    }
    out
}

/// `PatchDiff` (unified, line numbers, no backgrounds in scrollback).
fn patch_diff(diff: &str, width: usize, theme: &RunTheme, suppress: bool) -> Buffer {
    let b = &theme.block;
    let bg = |c: Color| if suppress { Color::TRANSPARENT } else { c };
    let mut parts: Vec<Buffer> = Vec::new();
    let min = min_digits(diff);
    for (hi, h) in hunks(diff).iter().enumerate() {
        if hi > 0 {
            let mut row = Buffer::new(width as u16, 1);
            let lnbg = bg(b.diff_context_bg);
            row.fill_rect(0, 0, width as i32, 1, lnbg);
            row.draw_chunks(0, 0, &[StyledChunk { text: format!(" {}", h.header.clone().unwrap_or_default()), fg: b.diff_line_number, bg: lnbg, attrs: 0 }], width);
            parts.push(row);
        }
        let max = h.lines.iter().map(|l| l.number).max().unwrap_or(1);
        let d = digits(max).max(min);
        let g = Gutter { width: 1 + d + 1 + 2, after_width: 2, padding_right: 1 };
        let code_w = width.saturating_sub(g.width).max(1);
        let mut rows: Vec<(Option<&DLine>, Vec<StyledChunk>)> = Vec::new();
        for l in &h.lines {
            for (k, v) in layout(&[StyledChunk::plain(&l.text, b.text)], code_w, WrapMode::Word, false).into_iter().enumerate() {
                rows.push((if k == 0 { Some(l) } else { None }, v.chunks));
            }
        }
        let mut buf = Buffer::new(width as u16, rows.len().max(1) as u16);
        for (y, (line, chunks)) in rows.iter().enumerate() {
            let sign = line.and_then(|l| match l.sign {
                '+' => Some((" +", b.diff_highlight_added)),
                '-' => Some((" -", b.diff_highlight_removed)),
                _ => None,
            });
            paint_gutter(&mut buf, y, &g, line.map(|l| l.number), sign, b.diff_line_number);
            buf.draw_chunks(g.width as i32, y as i32, chunks, code_w);
        }
        parts.push(buf);
    }
    column(parts, width, 0)
}

fn diff_items(items: &[DiffItem], width: usize, theme: &RunTheme, suppress: bool) -> Buffer {
    let parts = items
        .iter()
        .map(|item| {
            let title = text(&item.title, theme.block.muted, 0, width, WrapMode::Word);
            let body = if item.diff.trim().is_empty() {
                let n = item.deletions.unwrap_or(0);
                text(&format!("-{n} line{}", if n == 1 { "" } else { "s" }), theme.block.diff_removed, 0, width, WrapMode::Word)
            } else {
                padded(&patch_diff(&item.diff, width.saturating_sub(1).max(1), theme, suppress), width, 1)
            };
            column(vec![title, body], width, 1)
        })
        .collect();
    column(parts, width, 1)
}

/// Render a structured snapshot at `width`.
pub fn render(snap: &ToolSnapshot, theme: &RunTheme, _syntax: &MiniSyntax, width: usize, mono: bool, suppress: bool) -> Buffer {
    let b = &theme.block;
    match snap {
        ToolSnapshot::Code { title, content, .. } => {
            let t = text(title, b.muted, 0, width, WrapMode::Word);
            let code = padded(&numbered_code(content, width.saturating_sub(1).max(1), theme), width, 1);
            column(vec![t, code], width, 1)
        }
        ToolSnapshot::Diff { items } => diff_items(items, width, theme, suppress),
        ToolSnapshot::Task { title, rows, tail } => {
            let t = text(title, b.muted, 0, width, WrapMode::Word);
            let inner_w = width.saturating_sub(1).max(1);
            let mut inner: Vec<Buffer> = rows.iter().map(|r| text(r, b.text, 0, inner_w, WrapMode::Word)).collect();
            if !tail.is_empty() {
                inner.push(text(tail, b.muted, 0, inner_w, WrapMode::Word));
            }
            let body = padded(&column(inner, inner_w, 0), width, 1);
            column(vec![t, body], width, 1)
        }
        ToolSnapshot::Todo { items } => {
            let t = text("# Todos", b.muted, 0, width, WrapMode::Word);
            let rows: Vec<Buffer> = items
                .iter()
                .map(|item| {
                    let color = match item.status {
                        "in_progress" => b.warning,
                        "cancelled" => b.faint,
                        _ => b.muted,
                    };
                    let marker = format!("{} ", crate::util::todo::todo_marker(item.status, mono));
                    let mw = str_width(&marker);
                    let content = text(&item.content, color, if item.status == "cancelled" { attr::STRIKETHROUGH } else { 0 }, width.saturating_sub(mw).max(1), WrapMode::Word);
                    let mut row = Buffer::new(width as u16, content.height);
                    row.draw_chunks(0, 0, &[StyledChunk::plain(marker, color)], mw);
                    blit(&mut row, &content, mw as i32, 0);
                    row
                })
                .collect();
            column(vec![t, column(rows, width, 0)], width, 1)
        }
        ToolSnapshot::Question { items, tail } => {
            let t = text("# Questions", b.muted, 0, width, WrapMode::Word);
            let mut parts: Vec<Buffer> = items
                .iter()
                .map(|(q, a)| column(vec![text(q, b.muted, 0, width, WrapMode::Word), text(a, b.text, 0, width, WrapMode::Word)], width, 0))
                .collect();
            if !tail.is_empty() {
                parts.push(text(tail, b.muted, 0, width, WrapMode::Word));
            }
            column(vec![t, column(parts, width, 1)], width, 1)
        }
    }
}

/// `PatchDiff` as the permission dock shows it: unified, line numbers, line backgrounds.
pub fn patch_diff_dock(diff: &str, width: usize, b: &super::theme::BlockTheme, fg: Color) -> Buffer {
    let mut parts: Vec<Buffer> = Vec::new();
    let min = min_digits(diff);
    for (hi, h) in hunks(diff).iter().enumerate() {
        if hi > 0 {
            let mut row = Buffer::new(width as u16, 1);
            row.fill_rect(0, 0, width as i32, 1, b.diff_context_bg);
            row.draw_chunks(0, 0, &[StyledChunk { text: format!(" {}", h.header.clone().unwrap_or_default()), fg: b.diff_line_number, bg: b.diff_context_bg, attrs: 0 }], width);
            parts.push(row);
        }
        let max = h.lines.iter().map(|l| l.number).max().unwrap_or(1);
        let d = digits(max).max(min);
        let gw = 1 + d + 1 + 2;
        let code_w = width.saturating_sub(gw).max(1);
        let mut rows: Vec<(char, Option<usize>, Vec<StyledChunk>)> = Vec::new();
        for l in &h.lines {
            for (k, v) in layout(&[StyledChunk::plain(&l.text, fg)], code_w, WrapMode::Word, false).into_iter().enumerate() {
                rows.push((l.sign, (k == 0).then_some(l.number), v.chunks));
            }
        }
        let mut buf = Buffer::new(width as u16, rows.len().max(1) as u16);
        for (y, (sign, number, chunks)) in rows.iter().enumerate() {
            let (line_bg, num_bg) = match sign {
                '+' => (b.diff_added_bg, b.diff_added_line_number_bg),
                '-' => (b.diff_removed_bg, b.diff_removed_line_number_bg),
                _ => (b.diff_context_bg, b.diff_context_bg),
            };
            buf.fill_rect(0, y as i32, gw as i32, 1, num_bg);
            buf.fill_rect(gw as i32, y as i32, code_w as i32, 1, line_bg);
            if let Some(n) = number {
                let s = n.to_string();
                buf.draw_chunks((gw - 3 - s.len()) as i32, y as i32, &[StyledChunk { text: s, fg: b.diff_line_number, bg: num_bg, attrs: 0 }], gw);
                let sign_text = match sign {
                    '+' => Some((" +", b.diff_highlight_added)),
                    '-' => Some((" -", b.diff_highlight_removed)),
                    _ => None,
                };
                if let Some((t, c)) = sign_text {
                    buf.draw_chunks((gw - 3) as i32, y as i32, &[StyledChunk { text: t.into(), fg: c, bg: num_bg, attrs: 0 }], 2);
                }
            }
            let chunks: Vec<StyledChunk> = chunks.iter().map(|c| StyledChunk { bg: line_bg, ..c.clone() }).collect();
            buf.draw_chunks(gw as i32, y as i32, &chunks, code_w);
        }
        parts.push(buf);
    }
    column(parts, width, 0)
}
