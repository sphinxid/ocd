//! Text layout like OpenTUI's `TextBufferView` (native `text-buffer-view.zig`): logical lines split at
//! LF/CR/CRLF, then `word`, `char` or `none` wrapping into virtual lines, plus middle truncation.

use super::cell::{Buffer, StyledChunk, grapheme_width};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrapMode {
    None,
    Char,
    Word,
}

/// One laid-out row.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct VLine {
    pub chunks: Vec<StyledChunk>,
    pub width: usize,
    /// Byte range of the source text (all chunks concatenated) shown on this row.
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug)]
struct G {
    text: String,
    w: usize,
    style: usize,
    pos: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    None,
    Whitespace,
    Punct,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Ascii,
    Cjk,
    Other,
}

fn class(c: char) -> Class {
    if (c as u32) <= 0x7f {
        return if c.is_ascii_alphanumeric() || c == '_' { Class::Ascii } else { Class::Other };
    }
    let cp = c as u32;
    let cjk = (0x3400..=0x4DBF).contains(&cp)
        || (0x4E00..=0x9FFF).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0x20000..=0x2EE5D).contains(&cp)
        || (0x2F800..=0x2FA1F).contains(&cp)
        || (0x3040..=0x30FF).contains(&cp)
        || (0x31F0..=0x31FF).contains(&cp)
        || (0xFF66..=0xFF9D).contains(&cp)
        || (0x1100..=0x11FF).contains(&cp)
        || (0x3130..=0x318F).contains(&cp)
        || (0xA960..=0xA97F).contains(&cp)
        || (0xAC00..=0xD7FF).contains(&cp);
    if cjk { Class::Cjk } else { Class::Other }
}

fn kind(g: &str) -> Kind {
    let c = g.chars().next().unwrap_or(' ');
    match c {
        ' ' | '\t' => Kind::Whitespace,
        '-' | '/' | '\\' | '.' | ',' | ';' | ':' | '!' | '?' | '(' | ')' | '[' | ']' | '{' | '}' => Kind::Punct,
        '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{200b}' => Kind::Whitespace,
        '\u{ad}' | '\u{2010}' | '\u{3001}' | '\u{3002}' | '\u{ff01}' | '\u{ff0c}' | '\u{ff1a}' | '\u{ff1f}' => Kind::Punct,
        _ => Kind::None,
    }
}

struct Wrapper<'a> {
    styles: &'a [StyledChunk],
    out: Vec<VLine>,
    line: Vec<G>,
    line_w: usize,
    width: usize,
    full: usize,
    line_pos: usize,
}

impl Wrapper<'_> {
    fn commit(&mut self) {
        let line = std::mem::take(&mut self.line);
        let mut v = to_vline(&line, self.styles);
        if line.is_empty() {
            v.start = self.line_pos;
            v.end = self.line_pos;
        }
        self.out.push(v);
        self.line_w = 0;
        self.width = self.full;
    }
    fn add(&mut self, gs: &[G]) {
        for g in gs {
            self.line_w += g.w;
            self.line.push(g.clone());
        }
    }
    /// Longest prefix of `gs` within `cols` (whole graphemes; zero-width ones stay attached).
    fn fit(gs: &[G], cols: usize, force: bool) -> usize {
        let mut used = 0;
        let mut n = 0;
        for g in gs {
            if used + g.w > cols {
                break;
            }
            used += g.w;
            n += 1;
        }
        if n == 0 && force && !gs.is_empty() {
            n = 1;
            while n < gs.len() && gs[n].w == 0 {
                n += 1;
            }
        }
        n
    }
    /// `placeCompleteWordPiece`.
    fn place(&mut self, piece: &[G]) {
        let mut piece = piece.to_vec();
        loop {
            let pw: usize = piece.iter().map(|g| g.w).sum();
            if pw == 0 {
                return;
            }
            if pw <= self.width {
                if self.line_w > 0 && self.line_w + pw > self.width {
                    self.commit();
                }
                self.add(&piece);
                return;
            }
            if self.line_w > 0 {
                self.commit();
            }
            let n = Self::fit(&piece, self.width, true);
            if n == 0 {
                return;
            }
            let head: Vec<G> = piece.drain(..n).collect();
            self.add(&head);
            if piece.iter().map(|g| g.w).sum::<usize>() > 0 {
                self.commit();
            }
        }
    }
    /// `finalizePendingWord`.
    fn finalize(&mut self, pending: &mut Vec<G>) {
        loop {
            let pw: usize = pending.iter().map(|g| g.w).sum();
            if pw == 0 {
                break;
            }
            if self.line_w > 0 && self.line_w + pw > self.width {
                self.commit();
                continue;
            }
            if self.line_w == 0 && pw > self.width {
                let n = Self::fit(pending, self.width, true);
                if n == 0 {
                    break;
                }
                let head: Vec<G> = pending.drain(..n).collect();
                self.add(&head);
                if pending.iter().map(|g| g.w).sum::<usize>() > 0 {
                    self.commit();
                }
                continue;
            }
            let all = std::mem::take(pending);
            self.add(&all);
            return;
        }
        if !pending.is_empty() {
            let all = std::mem::take(pending);
            self.add(&all);
        }
    }
}

fn to_vline(gs: &[G], styles: &[StyledChunk]) -> VLine {
    let mut chunks: Vec<StyledChunk> = Vec::new();
    let mut last: Option<usize> = None;
    let mut width = 0;
    for g in gs {
        width += g.w;
        match chunks.last_mut() {
            Some(c) if last == Some(g.style) => c.text.push_str(&g.text),
            _ => {
                let s = &styles[g.style];
                chunks.push(StyledChunk { text: g.text.clone(), ..s.clone() });
                last = Some(g.style);
            }
        }
    }
    let start = gs.first().map(|g| g.pos).unwrap_or(0);
    let end = gs.last().map(|g| g.pos + g.text.len()).unwrap_or(start);
    VLine { chunks, width, start, end }
}

/// Split styled chunks into logical lines of graphemes.
fn logical_lines(chunks: &[StyledChunk]) -> Vec<(usize, Vec<G>)> {
    let mut lines: Vec<(usize, Vec<G>)> = vec![(0, Vec::new())];
    let mut base = 0;
    for (i, c) in chunks.iter().enumerate() {
        let mut prev_cr = false;
        for (o, g) in c.text.grapheme_indices(true) {
            let pos = base + o;
            if g == "\r\n" || g == "\n" {
                if !prev_cr || g == "\r\n" {
                    lines.push((pos + g.len(), Vec::new()));
                } else {
                    lines.last_mut().unwrap().0 = pos + g.len();
                }
                prev_cr = false;
                continue;
            }
            if g == "\r" {
                lines.push((pos + 1, Vec::new()));
                prev_cr = true;
                continue;
            }
            prev_cr = false;
            lines.last_mut().unwrap().1.push(G { text: g.to_string(), w: grapheme_width(g), style: i, pos });
        }
        base += c.text.len();
    }
    lines
}

fn word_wrap_line(line: &[G], w: &mut Wrapper) {
    let mut pending: Vec<G> = Vec::new();
    let mut has_non_ws = false;
    let mut start = 0usize;
    // Chunk boundaries: a trailing unbroken run of one chunk becomes a pending word piece.
    let mut i = 0;
    while i < line.len() {
        let g = &line[i];
        let k = kind(&g.text);
        let chunk_end = i + 1 == line.len() || line[i + 1].style != g.style;
        let next_class = line.get(i + 1).map(|n| class(n.text.chars().next().unwrap_or(' ')));
        let cls = class(g.text.chars().next().unwrap_or(' '));
        let transition = k == Kind::None && matches!((cls, next_class), (Class::Ascii, Some(Class::Cjk)) | (Class::Cjk, Some(Class::Ascii)));
        let cjk_inter = k == Kind::None && cls == Class::Cjk && next_class == Some(Class::Cjk) && g.w > 0;
        match k {
            Kind::Whitespace => {
                let word = &line[start..i];
                let word_w: usize = word.iter().map(|g| g.w).sum();
                if pending.is_empty() && g.w > 0 && word_w > 0 && w.line_w + word_w + g.w <= w.width {
                    w.add(word);
                    w.add(std::slice::from_ref(g));
                    has_non_ws = true;
                } else {
                    if word_w > 0 {
                        if !pending.is_empty() {
                            pending.extend(word.iter().cloned());
                            w.finalize(&mut pending);
                        } else {
                            w.place(word);
                        }
                        has_non_ws = true;
                    } else if !word.is_empty() || !pending.is_empty() {
                        pending.extend(word.iter().cloned());
                        w.finalize(&mut pending);
                    }
                    let preserve = !has_non_ws;
                    if !preserve && w.line_w + g.w > w.width {
                        if w.line_w > 0 {
                            w.commit();
                        }
                    } else if !preserve && w.line_w == 0 {
                        // dropped separator after a wrap
                    } else {
                        w.place(std::slice::from_ref(g));
                    }
                }
                start = i + 1;
            }
            Kind::Punct => {
                let word = &line[start..=i];
                if !pending.is_empty() {
                    pending.extend(word.iter().cloned());
                    w.finalize(&mut pending);
                } else {
                    w.place(word);
                }
                has_non_ws = true;
                start = i + 1;
            }
            Kind::None if transition || cjk_inter => {
                let word = &line[start..=i];
                if !pending.is_empty() {
                    pending.extend(word.iter().cloned());
                    w.finalize(&mut pending);
                } else {
                    w.place(word);
                }
                has_non_ws = true;
                start = i + 1;
            }
            Kind::None => {}
        }
        if chunk_end && start <= i {
            // The rest of this chunk is a pending piece of a word that may continue in the next chunk.
            pending.extend(line[start..=i].iter().cloned());
            if line[start..=i].iter().any(|g| g.w > 0) {
                has_non_ws = true;
            }
            start = i + 1;
        }
        i += 1;
    }
    w.finalize(&mut pending);
}

fn char_wrap_line(line: &[G], w: &mut Wrapper) {
    for g in line {
        if w.line_w + g.w > w.width && w.line_w > 0 {
            w.commit();
        }
        w.add(std::slice::from_ref(g));
    }
}

/// Lay out styled chunks at `width` with `mode`. `truncate` applies middle truncation (`...`) to
/// rows wider than `width`.
pub fn layout(chunks: &[StyledChunk], width: usize, mode: WrapMode, truncate: bool) -> Vec<VLine> {
    layout_inner(chunks, width, mode, truncate, 0)
}

/// Like [`layout`] without truncation, with the first row's budget reduced by `first_line_offset`
/// (OpenTUI's `firstLineOffset`: a scrollback commit continuing the terminal's tail row).
pub fn layout_first(chunks: &[StyledChunk], width: usize, mode: WrapMode, first_line_offset: usize) -> Vec<VLine> {
    layout_inner(chunks, width, mode, false, first_line_offset)
}

fn layout_inner(chunks: &[StyledChunk], width: usize, mode: WrapMode, truncate: bool, offset: usize) -> Vec<VLine> {
    let width = width.max(1);
    let first = if offset > 0 && offset < width && mode != WrapMode::None { width - offset } else { width };
    let mut w = Wrapper { styles: chunks, out: Vec::new(), line: Vec::new(), line_w: 0, width: first, full: width, line_pos: 0 };
    for (line_pos, line) in logical_lines(chunks) {
        w.line_pos = line_pos;
        let line_width: usize = line.iter().map(|g| g.w).sum();
        match mode {
            WrapMode::None => w.add(&line),
            WrapMode::Char => char_wrap_line(&line, &mut w),
            WrapMode::Word => word_wrap_line(&line, &mut w),
        }
        if !w.line.is_empty() || line_width == 0 {
            w.commit();
        }
    }
    let mut out = w.out;
    if truncate {
        for v in out.iter_mut() {
            if v.width > width {
                *v = truncate_middle(v, width);
            }
        }
    }
    out
}

fn truncate_middle(v: &VLine, width: usize) -> VLine {
    if width <= 3 {
        return VLine { chunks: vec![], width: 0, start: v.start, end: v.start };
    }
    let available = width - 3;
    let prefix_w = available / 2;
    let suffix_w = available - prefix_w;
    let mut gs: Vec<(String, usize, StyledChunk)> = Vec::new();
    for c in &v.chunks {
        for g in c.text.graphemes(true) {
            gs.push((g.to_string(), grapheme_width(g), c.clone()));
        }
    }
    let mut out: Vec<StyledChunk> = Vec::new();
    let push = |out: &mut Vec<StyledChunk>, g: &str, st: &StyledChunk| match out.last_mut() {
        Some(l) if l.fg == st.fg && l.bg == st.bg && l.attrs == st.attrs => l.text.push_str(g),
        _ => out.push(StyledChunk { text: g.to_string(), ..st.clone() }),
    };
    let mut used = 0;
    let mut last_style = gs.first().map(|g| g.2.clone());
    for (g, w, st) in &gs {
        if used + w > prefix_w {
            break;
        }
        used += w;
        push(&mut out, g, st);
        last_style = Some(st.clone());
    }
    let ell = last_style.unwrap_or_else(|| StyledChunk::plain("", super::cell::Color::WHITE));
    out.push(StyledChunk { text: "...".into(), ..ell });
    let suffix_start = v.width - suffix_w;
    let mut pos = 0;
    let mut suffix_used = 0;
    for (g, w, st) in &gs {
        if pos >= suffix_start {
            push(&mut out, g, st);
            suffix_used += w;
        }
        pos += w;
    }
    VLine { chunks: out, width: used + 3 + suffix_used, start: v.start, end: v.end }
}

/// Draw laid-out rows at (x, y), at most `max_rows` rows and `width` columns.
pub fn draw(buf: &mut Buffer, x: i32, y: i32, lines: &[VLine], width: usize, max_rows: usize) {
    for (i, l) in lines.iter().take(max_rows).enumerate() {
        buf.draw_chunks(x, y + i as i32, &l.chunks, width);
    }
}

/// Plain text of laid-out rows (for tests).
pub fn plain(lines: &[VLine]) -> Vec<String> {
    lines.iter().map(|l| l.chunks.iter().map(|c| c.text.as_str()).collect()).collect()
}

#[cfg(test)]
mod tests {
    use super::super::cell::Color;
    use super::*;

    fn wrap(text: &str, w: usize) -> Vec<String> {
        plain(&layout(&[StyledChunk::plain(text, Color::WHITE)], w, WrapMode::Word, false))
    }

    #[test]
    fn word_wrap_matches_opentui() {
        let text = "This is a long paragraph that should wrap across several lines at thirty columns, including a path like src/components/footer-view.tsx and a hyphenated-word-sequence.";
        assert_eq!(
            wrap(text, 30),
            vec![
                "This is a long paragraph that ",
                "should wrap across several ",
                "lines at thirty columns, ",
                "including a path like src/",
                "components/footer-view.tsx and",
                "a hyphenated-word-sequence."
            ]
        );
    }

    /// Expected rows captured from OpenTUI 0.6.8's `TextRenderable` (trailing padding trimmed).
    #[test]
    fn wraps_like_opentui_text_renderable() {
        let t = |text: &str, w: usize| wrap(text, w).into_iter().map(|s| s.trim_end().to_string()).collect::<Vec<_>>();
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
        assert_eq!(wrap("", 10), vec![""]);
        assert_eq!(t("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(t("  indented text", 8), vec!["", "indented", "text"]);
        assert_eq!(t("word   spaced    out", 7), vec!["word", "spaced", "out"]);
        assert_eq!(t("hello world foo", 11), vec!["hello world", "foo"]);
        assert_eq!(t("hello world foo", 12), vec!["hello world", "foo"]);
        assert_eq!(t("x a-very-long-hyphenated-thing y", 9), vec!["x a-very-", "long-", "hyphenate", "d-thing y"]);
        assert_eq!(t("trailing spaces here     ", 10), vec!["trailing", "spaces", "here"]);
        assert_eq!(t("日本語のテキストを折り返す", 9), vec!["日本語の", "テキスト", "を折り返", "す"]);
        assert_eq!(t("abc日本def", 5), vec!["abc日", "本def"]);
        let trunc = plain(&layout(&[StyledChunk::plain("a long line to be truncated in the middle", Color::WHITE)], 20, WrapMode::None, true));
        assert_eq!(trunc, vec!["a long l...he middle"]);
    }
}
