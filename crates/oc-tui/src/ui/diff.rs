//! Unified-patch parsing and unified/split diff rendering (`src/component/patch-diff.tsx`,
//! `src/util/diff.ts`).

use super::highlight::highlight;
use super::text::{SLine, Seg, Wrap, wrap};
use crate::theme::{SyntaxStyle, Theme};
use ratatui::style::Style;

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Context,
    Add,
    Remove,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DLine {
    pub kind: Kind,
    pub old: Option<usize>,
    pub new: Option<usize>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DLine>,
}

/// Parse a unified diff into hunks (split at `@@` headers).
pub fn parse(patch: &str) -> Vec<Hunk> {
    let re = regex::Regex::new(r"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@").unwrap();
    let mut hunks: Vec<Hunk> = Vec::new();
    let (mut old, mut new) = (0usize, 0usize);
    for line in patch.lines() {
        if let Some(c) = re.captures(line) {
            old = c[1].parse().unwrap_or(1);
            new = c[2].parse().unwrap_or(1);
            hunks.push(Hunk { header: line.to_string(), lines: Vec::new() });
            continue;
        }
        let Some(h) = hunks.last_mut() else { continue };
        if line.starts_with("\\ No newline") {
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            h.lines.push(DLine { kind: Kind::Add, old: None, new: Some(new), text: rest.to_string() });
            new += 1;
        } else if let Some(rest) = line.strip_prefix('-') {
            h.lines.push(DLine { kind: Kind::Remove, old: Some(old), new: None, text: rest.to_string() });
            old += 1;
        } else {
            let rest = line.strip_prefix(' ').unwrap_or(line);
            h.lines.push(DLine { kind: Kind::Context, old: Some(old), new: Some(new), text: rest.to_string() });
            old += 1;
            new += 1;
        }
    }
    hunks
}

pub struct DiffOpts<'a> {
    pub theme: &'a Theme,
    pub syntax: &'a SyntaxStyle,
    pub filetype: &'a str,
    pub split: bool,
    pub wrap: bool,
}

fn digits(n: usize) -> usize {
    n.max(1).to_string().len()
}

/// The gutter's `minDigits` (`PatchDiff`, OpenTUI 0.7 `gutter`): the digits of the last line any hunk
/// header's range reaches on either side, so hunks and split sides line up.
pub fn header_digits(hunks: &[Hunk]) -> usize {
    let re = regex::Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").unwrap();
    let mut max: i64 = 1;
    for h in hunks {
        if let Some(c) = re.captures(&h.header) {
            let n = |i: usize, default: i64| c.get(i).and_then(|m| m.as_str().parse::<i64>().ok()).unwrap_or(default);
            max = max.max(n(1, 0) + n(2, 1) - 1).max(n(3, 0) + n(4, 1) - 1);
        }
    }
    max.to_string().len()
}

fn highlight_lines(lines: &[&DLine], o: &DiffOpts) -> Vec<Vec<Seg>> {
    let code: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
    highlight(&code.join("\n"), o.filetype, o.syntax)
}

fn with_bg(segs: Vec<Seg>, bg: ratatui::style::Color) -> Vec<Seg> {
    segs.into_iter().map(|s| Seg::new(s.text, s.style.bg(bg))).collect()
}

/// Render hunks into lines of exactly `width` cells.
pub fn render(hunks: &[Hunk], width: usize, o: &DiffOpts) -> Vec<SLine> {
    let t = o.theme;
    let d = &t.diff;
    let max_num = hunks.iter().flat_map(|h| h.lines.iter()).flat_map(|l| [l.old.unwrap_or(0), l.new.unwrap_or(0)]).max().unwrap_or(1);
    let nw = digits(max_num).max(header_digits(hunks));
    let mut out = Vec::new();
    for (hi, h) in hunks.iter().enumerate() {
        if hi > 0 {
            let header = format!(" {}", h.header);
            let mut segs = vec![Seg::new(header, Style::default().fg(d.text_hunk_header.tui()).bg(d.bg_context.tui()))];
            segs = super::text::clip(&segs, width);
            out.push(SLine::new(segs).with_bg(Some(d.bg_context.tui())));
        }
        if o.split {
            out.extend(render_split(h, width, nw, o));
        } else {
            out.extend(render_unified(h, width, nw, o));
        }
    }
    out
}

fn line_colors(kind: &Kind, o: &DiffOpts) -> (ratatui::style::Color, ratatui::style::Color, ratatui::style::Color, &'static str) {
    let d = &o.theme.diff;
    match kind {
        Kind::Add => (d.bg_added.tui(), d.line_number_bg_added.tui(), d.highlight_added.tui(), "+"),
        Kind::Remove => (d.bg_removed.tui(), d.line_number_bg_removed.tui(), d.highlight_removed.tui(), "-"),
        Kind::Context => (d.bg_context.tui(), d.bg_context.tui(), d.line_number_text.tui(), " "),
    }
}

fn render_unified(h: &Hunk, width: usize, nw: usize, o: &DiffOpts) -> Vec<SLine> {
    let lines: Vec<&DLine> = h.lines.iter().collect();
    let hl = highlight_lines(&lines, o);
    let mut out = Vec::new();
    let code_w = width.saturating_sub(nw + 3).max(1);
    for (i, l) in lines.iter().enumerate() {
        let (bg, nbg, sign_fg, sign) = line_colors(&l.kind, o);
        let num = match l.kind {
            Kind::Remove => l.old,
            _ => l.new,
        };
        let code = with_bg(hl.get(i).cloned().unwrap_or_default(), bg);
        let wrapped = wrap(&code, code_w, if o.wrap { Wrap::Word } else { Wrap::None });
        for (wi, w) in wrapped.into_iter().enumerate() {
            let n = if wi == 0 { num.map(|n| format!("{n:>nw$} ")).unwrap_or_else(|| " ".repeat(nw + 1)) } else { " ".repeat(nw + 1) };
            let mut segs = vec![
                Seg::new(n, Style::default().fg(o.theme.diff.line_number_text.tui()).bg(nbg)),
                Seg::new(if wi == 0 { format!("{sign} ") } else { "  ".into() }, Style::default().fg(sign_fg).bg(bg)),
            ];
            segs.extend(w);
            out.push(SLine::new(segs).with_bg(Some(bg)));
        }
    }
    out
}

fn render_split(h: &Hunk, width: usize, nw: usize, o: &DiffOpts) -> Vec<SLine> {
    // Pair removes with adds within each change run.
    let mut rows: Vec<(Option<&DLine>, Option<&DLine>)> = Vec::new();
    let mut i = 0;
    let ls = &h.lines;
    while i < ls.len() {
        if ls[i].kind == Kind::Context {
            rows.push((Some(&ls[i]), Some(&ls[i])));
            i += 1;
            continue;
        }
        let mut dels = Vec::new();
        let mut adds = Vec::new();
        while i < ls.len() && ls[i].kind == Kind::Remove {
            dels.push(&ls[i]);
            i += 1;
        }
        while i < ls.len() && ls[i].kind == Kind::Add {
            adds.push(&ls[i]);
            i += 1;
        }
        for k in 0..dels.len().max(adds.len()) {
            rows.push((dels.get(k).copied(), adds.get(k).copied()));
        }
    }
    let half = width / 2;
    let right_w = width - half;
    let left_lines: Vec<&DLine> = rows.iter().filter_map(|r| r.0).collect();
    let right_lines: Vec<&DLine> = rows.iter().filter_map(|r| r.1).collect();
    let lh = highlight_lines(&left_lines, o);
    let rh = highlight_lines(&right_lines, o);
    let (mut li, mut ri) = (0, 0);
    let mut out = Vec::new();
    let empty_bg = o.theme.diff.bg_context.tui();
    for (l, r) in rows {
        let side = |line: Option<&DLine>, hl: &[Vec<Seg>], idx: &mut usize, w: usize, old: bool| -> Vec<Vec<Seg>> {
            let code_w = w.saturating_sub(nw + 3).max(1);
            match line {
                None => vec![vec![Seg::new(" ".repeat(w), Style::default().bg(empty_bg))]],
                Some(l) => {
                    let (bg, nbg, sign_fg, sign) = line_colors(&l.kind, o);
                    let code = with_bg(hl.get(*idx).cloned().unwrap_or_default(), bg);
                    *idx += 1;
                    let num = if old { l.old } else { l.new };
                    let wrapped = wrap(&code, code_w, if o.wrap { Wrap::Word } else { Wrap::None });
                    wrapped
                        .into_iter()
                        .enumerate()
                        .map(|(wi, wl)| {
                            let n = if wi == 0 { num.map(|n| format!("{n:>nw$} ")).unwrap_or_else(|| " ".repeat(nw + 1)) } else { " ".repeat(nw + 1) };
                            let mut segs = vec![
                                Seg::new(n, Style::default().fg(o.theme.diff.line_number_text.tui()).bg(nbg)),
                                Seg::new(if wi == 0 { format!("{sign} ") } else { "  ".into() }, Style::default().fg(sign_fg).bg(bg)),
                            ];
                            let used: usize = wl.iter().map(|s| s.width()).sum();
                            segs.extend(wl);
                            segs.push(Seg::new(" ".repeat(code_w.saturating_sub(used)), Style::default().bg(bg)));
                            segs
                        })
                        .collect()
                }
            }
        };
        let left = side(l, &lh, &mut li, half, true);
        let right = side(r, &rh, &mut ri, right_w, false);
        for k in 0..left.len().max(right.len()) {
            let mut segs = left.get(k).cloned().unwrap_or_else(|| vec![Seg::new(" ".repeat(half), Style::default().bg(empty_bg))]);
            segs = super::text::clip(&segs, half);
            let used: usize = segs.iter().map(|s| s.width()).sum();
            if used < half {
                segs.push(Seg::new(" ".repeat(half - used), Style::default().bg(empty_bg)));
            }
            segs.extend(right.get(k).cloned().unwrap_or_else(|| vec![Seg::new(" ".repeat(right_w), Style::default().bg(empty_bg))]));
            out.push(SLine::new(super::text::clip(&segs, width)));
        }
    }
    out
}

/// Rows of a diff-viewer card (`PatchDiff` with `showLineNumbers`): every row is exactly `width`
/// cells. The gutter is `gutter_pad` cells, one cell, the line number right-aligned to the widest
/// number of either side, a space and the sign, a space, then (split view) padding up to that width
/// for the narrower side; it takes the line-number background, the code the line background. Split
/// rows give the left side the odd cell. Hunks after the first start with their `@@` header. Also
/// returns the row of each hunk start.
pub fn render_viewer(hunks: &[Hunk], width: usize, split: bool, gutter_pad: usize, o: &DiffOpts) -> (Vec<SLine>, Vec<usize>) {
    let d = &o.theme.diff;
    let mut out: Vec<SLine> = Vec::new();
    let mut starts = Vec::new();
    // Line-number widths: unified shows old numbers on removals and new numbers otherwise.
    let all = hunks.iter().flat_map(|h| h.lines.iter());
    let (left_digits, right_digits, unified_digits) = {
        let old = all.clone().filter_map(|l| if l.kind == Kind::Add { None } else { l.old }).max().unwrap_or(1);
        let new = all.clone().filter_map(|l| if l.kind == Kind::Remove { None } else { l.new }).max().unwrap_or(1);
        let uni = all.filter_map(|l| if l.kind == Kind::Remove { l.old } else { l.new }).max().unwrap_or(1);
        let min = header_digits(hunks);
        (digits(old).max(min), digits(new).max(min), digits(uni).max(min))
    };
    let nw = if split { left_digits.max(right_digits) } else { unified_digits };
    let gutter = |num: Option<usize>, kind: Option<&Kind>, extra: usize, first: bool| -> Vec<Seg> {
        let (gbg, sign_fg, sign) = match kind {
            Some(Kind::Add) => (d.line_number_bg_added, d.highlight_added, "+"),
            Some(Kind::Remove) => (d.line_number_bg_removed, d.highlight_removed, "-"),
            _ => (d.bg_context, d.line_number_text, " "),
        };
        let bg = gbg.tui();
        let n = match num.filter(|_| first) {
            Some(n) => format!("{}{n:>nw$}", " ".repeat(gutter_pad + 1)),
            None => " ".repeat(gutter_pad + 1 + nw),
        };
        let sign = if first && kind.is_some() { sign } else { " " };
        vec![
            Seg::new(n, Style::default().fg(d.line_number_text.tui()).bg(bg)),
            Seg::new(format!(" {sign}"), Style::default().fg(sign_fg.tui()).bg(bg)),
            Seg::new(" ".repeat(1 + extra), Style::default().bg(bg)),
        ]
    };
    let gutter_w = |extra: usize| gutter_pad + 1 + nw + 2 + 1 + extra;
    let code_bg = |kind: &Kind| match kind {
        Kind::Add => d.bg_added.tui(),
        Kind::Remove => d.bg_removed.tui(),
        Kind::Context => d.bg_context.tui(),
    };
    // One side of a row: gutter + wrapped code, padded to `w` cells per row.
    let side = |line: Option<(&DLine, Vec<Seg>)>, num: Option<usize>, w: usize, extra: usize| -> Vec<Vec<Seg>> {
        let Some((l, code)) = line else {
            return vec![vec![Seg::new(" ".repeat(w), Style::default().bg(d.bg_context.tui()))]];
        };
        let bg = code_bg(&l.kind);
        let code_w = w.saturating_sub(gutter_w(extra)).max(1);
        let code = with_bg(code, bg);
        let rows = wrap(&code, code_w, Wrap::Char);
        rows.into_iter()
            .enumerate()
            .map(|(i, r)| {
                let mut segs = gutter(num, Some(&l.kind), extra, i == 0);
                let used: usize = r.iter().map(|s| s.width()).sum();
                segs.extend(r);
                segs.push(Seg::new(" ".repeat(code_w.saturating_sub(used)), Style::default().bg(bg)));
                super::text::clip(&segs, w)
            })
            .collect()
    };
    for (hi, h) in hunks.iter().enumerate() {
        starts.push(out.len());
        if hi > 0 {
            let segs = vec![Seg::new(format!(" {}", h.header), Style::default().fg(d.text_hunk_header.tui()).bg(d.bg_context.tui()))];
            let mut segs = super::text::clip(&segs, width);
            let used: usize = segs.iter().map(|s| s.width()).sum();
            segs.push(Seg::new(" ".repeat(width.saturating_sub(used)), Style::default().bg(d.bg_context.tui())));
            out.push(SLine::new(segs));
        }
        if !split {
            let lines: Vec<&DLine> = h.lines.iter().collect();
            let hl = highlight_lines(&lines, o);
            for (i, l) in lines.iter().enumerate() {
                let num = if l.kind == Kind::Remove { l.old } else { l.new };
                for r in side(Some((l, hl.get(i).cloned().unwrap_or_default())), num, width, 0) {
                    out.push(SLine::new(r));
                }
            }
            continue;
        }
        // Pair removals with additions within each change run.
        let mut rows: Vec<(Option<&DLine>, Option<&DLine>)> = Vec::new();
        let ls = &h.lines;
        let mut i = 0;
        while i < ls.len() {
            if ls[i].kind == Kind::Context {
                rows.push((Some(&ls[i]), Some(&ls[i])));
                i += 1;
                continue;
            }
            let mut dels = Vec::new();
            let mut adds = Vec::new();
            while i < ls.len() && ls[i].kind == Kind::Remove {
                dels.push(&ls[i]);
                i += 1;
            }
            while i < ls.len() && ls[i].kind == Kind::Add {
                adds.push(&ls[i]);
                i += 1;
            }
            for k in 0..dels.len().max(adds.len()) {
                rows.push((dels.get(k).copied(), adds.get(k).copied()));
            }
        }
        let left_w = width.div_ceil(2);
        let right_w = width - left_w;
        let left_lines: Vec<&DLine> = rows.iter().filter_map(|r| r.0).collect();
        let right_lines: Vec<&DLine> = rows.iter().filter_map(|r| r.1).collect();
        let lh = highlight_lines(&left_lines, o);
        let rh = highlight_lines(&right_lines, o);
        let (mut li, mut ri) = (0, 0);
        for (l, r) in rows {
            let left = side(
                l.map(|x| {
                    li += 1;
                    (x, lh.get(li - 1).cloned().unwrap_or_default())
                }),
                l.and_then(|x| x.old),
                left_w,
                nw - left_digits,
            );
            let right = side(
                r.map(|x| {
                    ri += 1;
                    (x, rh.get(ri - 1).cloned().unwrap_or_default())
                }),
                r.and_then(|x| x.new),
                right_w,
                nw - right_digits,
            );
            for k in 0..left.len().max(right.len()) {
                let mut segs = left.get(k).cloned().unwrap_or_else(|| blank_side(left.first(), left_w, d.bg_context.tui()));
                segs.extend(right.get(k).cloned().unwrap_or_else(|| blank_side(right.first(), right_w, d.bg_context.tui())));
                out.push(SLine::new(segs));
            }
        }
    }
    (out, starts)
}

/// A continuation row for the shorter side of a wrapped split row: its gutter and code backgrounds.
fn blank_side(first: Option<&Vec<Seg>>, w: usize, fallback: ratatui::style::Color) -> Vec<Seg> {
    let gutter_bg = first.and_then(|s| s.first()).and_then(|s| s.style.bg).unwrap_or(fallback);
    let code_bg = first.and_then(|s| s.last()).and_then(|s| s.style.bg).unwrap_or(fallback);
    let gw: usize = first.map(|s| s.iter().take(3).map(|x| x.width()).sum()).unwrap_or(0).min(w);
    vec![Seg::new(" ".repeat(gw), Style::default().bg(gutter_bg)), Seg::new(" ".repeat(w - gw), Style::default().bg(code_bg))]
}

/// Additions/deletions counts of a patch.
pub fn counts(patch: &str) -> (usize, usize) {
    let mut a = 0;
    let mut d = 0;
    for l in patch.lines() {
        if l.starts_with("+++") || l.starts_with("---") {
            continue;
        }
        if l.starts_with('+') {
            a += 1;
        } else if l.starts_with('-') {
            d += 1;
        }
    }
    (a, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Mode, OPENCODE_V2, resolve};

    const PATCH: &str = "--- a/x.ts\n+++ b/x.ts\n@@ -1,3 +1,3 @@\n a\n-b\n+c\n d\n@@ -10,2 +10,3 @@\n x\n+y\n z\n";

    #[test]
    fn parses() {
        let h = parse(PATCH);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].lines.len(), 4);
        assert_eq!(h[0].lines[2], DLine { kind: Kind::Add, old: None, new: Some(2), text: "c".into() });
        assert_eq!(h[1].lines[1].new, Some(11));
        assert_eq!(counts(PATCH), (2, 1));
    }

    #[test]
    fn renders_unified_and_split() {
        let doc: serde_json::Value = serde_json::from_str(OPENCODE_V2).unwrap();
        let t = resolve::resolve_document(&doc, Mode::Dark).unwrap();
        let st = SyntaxStyle::from_theme(&t);
        let o = DiffOpts { theme: &t, syntax: &st, filetype: "typescript", split: false, wrap: true };
        let lines = render(&parse(PATCH), 40, &o);
        let texts: Vec<String> = lines.iter().map(|l| l.text()).collect();
        assert_eq!(texts[1], " 2 - b");
        assert_eq!(texts[2], " 2 + c");
        assert!(texts[4].starts_with(" @@ -10,2"));
        let o = DiffOpts { split: true, ..o };
        let lines = render(&parse(PATCH), 40, &o);
        assert!(lines[1].text().contains("- b") && lines[1].text().contains("+ c"));
    }
}
