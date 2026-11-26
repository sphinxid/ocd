//! OpenTUI 0.9.4's `MarkdownRenderable` as mini's scrollback uses it (`tableOptions: { style: "columns",
//! widthMode: "content" }`, `monoMarkdownTableOptions` in mono): the document `ui::mdoc` parses, laid out in
//! top-level blocks one row apart. Paragraphs and headings keep their line breaks and wrap by word, a setext heading
//! keeps its underline row; a quote is a `│` bar with its blocks a row apart; a list is a marker column (`-`, numbers
//! aligned on their right, a task box) and a content column; a table is borderless columns two cells apart, aligned
//! as its delimiter row says, its header in the heading style; a rule is a row of `─`. Mono (`monoMarkdownRenderable`)
//! writes every text through `monoText` (`☐` → `[ ]`, `☑` → `[x]`) and draws bars and rules in ASCII. While the
//! content streams, every top-level block but the last is final (`committedBlockCount`).

use super::cell::{Buffer, Color, Glyph, StyledChunk, attr};
use super::highlight::{code_highlights, to_chunks};
use super::mono::mono_text;
use super::text::{WrapMode, layout};
use super::theme::{MiniSyntax, StyleDef};
use crate::ui::mdoc::{self, Align, Block as Doc, Item, Row, Span};

pub struct MdStyle<'a> {
    pub syntax: &'a MiniSyntax,
    /// Renderable foreground (`entryColor`).
    pub fg: Color,
    pub mono: bool,
}

/// A rendered top-level block: its top margin and its rows.
pub struct Block {
    pub margin_top: usize,
    pub buf: Buffer,
}

pub struct Rendered {
    pub blocks: Vec<Block>,
    /// `committedBlockCount`: the top-level blocks that are final.
    pub stable: usize,
}

impl Rendered {
    /// `renderable.y` of each block within the markdown root.
    pub fn ys(&self) -> Vec<usize> {
        let mut y = 0;
        let mut out = Vec::new();
        for b in &self.blocks {
            y += b.margin_top;
            out.push(y);
            y += b.buf.height as usize;
        }
        out
    }
    pub fn height(&self) -> usize {
        self.blocks.iter().map(|b| b.margin_top + b.buf.height as usize).sum::<usize>().max(1)
    }
    /// The whole surface as one buffer.
    pub fn compose(&self, width: u16) -> Buffer {
        let mut out = Buffer::new(width, self.height() as u16);
        let ys = self.ys();
        for (b, y) in self.blocks.iter().zip(ys) {
            blit(&mut out, &b.buf, 0, y as i32);
        }
        out
    }
}

/// Copy `src` into `dst` at (x, y), skipping never-written cells.
pub fn blit(dst: &mut Buffer, src: &Buffer, x: i32, y: i32) {
    for sy in 0..src.height {
        for sx in 0..src.width {
            let c = src.get(sx, sy);
            if matches!(c.glyph, Glyph::Empty) {
                continue;
            }
            let (dx, dy) = (x + sx as i32, y + sy as i32);
            if dx < 0 || dy < 0 || dx >= dst.width as i32 || dy >= dst.height as i32 {
                continue;
            }
            let i = dy as usize * dst.width as usize + dx as usize;
            dst.cells[i] = c.clone();
        }
    }
}

/// Render text chunks word-wrapped into a buffer of `width` columns.
fn text_buffer(chunks: &[StyledChunk], width: usize, mode: WrapMode) -> Buffer {
    let lines = layout(chunks, width, mode, false);
    let mut buf = Buffer::new(width as u16, lines.len().max(1) as u16);
    for (i, l) in lines.iter().enumerate() {
        buf.draw_chunks(0, i as i32, &l.chunks, width);
    }
    buf
}

/// `infoStringToFiletype(lang)`.
pub fn info_string_filetype(lang: &str) -> Option<String> {
    let first = lang.split_whitespace().next().unwrap_or("").to_lowercase();
    let ft = match first.as_str() {
        "" => return None,
        "js" | "javascript" | "mjs" | "cjs" => "javascript",
        "jsx" | "javascriptreact" => "javascriptreact",
        "ts" | "typescript" | "mts" | "cts" => "typescript",
        "tsx" | "typescriptreact" => "typescriptreact",
        "md" | "markdown" => "markdown",
        "zig" => "zig",
        other => return Some(other.to_string()),
    };
    Some(ft.into())
}

/// A code block (`CodeRenderable` without conceal).
pub fn code_block(text: &str, filetype: Option<&str>, width: usize, st: &MdStyle, conceal: bool) -> Buffer {
    let hl = filetype.map(|f| code_highlights(text, f)).unwrap_or_default();
    let chunks = if hl.is_empty() {
        let d = st.syntax.default_style();
        vec![StyledChunk { text: text.to_string(), fg: d.and_then(|s| s.fg).unwrap_or(st.fg), bg: d.and_then(|s| s.bg).unwrap_or(Color::TRANSPARENT), attrs: d.map(|s| s.attrs()).unwrap_or(0) }]
    } else {
        to_chunks(text, &hl, st.syntax, conceal, None, st.fg)
    };
    text_buffer(&chunks, width, WrapMode::Word)
}


/// The style of text in a block that no part of it styles (`baseStyle`): the entry color, under `markup.quote` for
/// each quote around it.
#[derive(Clone, Copy)]
struct Base {
    fg: Color,
    bg: Color,
    attrs: u8,
}

/// Composes `def` over a style the way `resolveGroupSet` does: colors replace, supplied attributes replace.
fn over(fg: &mut Color, bg: &mut Color, attrs: &mut u8, def: &StyleDef) {
    if let Some(c) = def.fg {
        *fg = c;
    }
    if let Some(c) = def.bg {
        *bg = c;
    }
    let mut supplied = 0u8;
    for (on, bit) in [(def.bold, attr::BOLD), (def.italic, attr::ITALIC), (def.underline, attr::UNDERLINE), (def.dim, attr::DIM)] {
        if on.is_some() {
            supplied |= bit;
        }
    }
    *attrs = (*attrs & !supplied) | def.attrs();
}

/// A group as Core's `getStyle` finds it: the name, else its first dotted segment.
fn group<'a>(st: &MdStyle<'a>, name: &str) -> Option<&'a StyleDef> {
    st.syntax.get(name).or_else(|| name.split_once('.').and_then(|(head, _)| st.syntax.get(head)))
}

struct Lay<'a, 'b> {
    st: &'b MdStyle<'a>,
}

impl Lay<'_, '_> {
    fn root(&self) -> Base {
        Base { fg: self.st.fg, bg: Color::TRANSPARENT, attrs: 0 }
    }

    fn quoted(&self, b: Base) -> Base {
        let Some(q) = group(self.st, "markup.quote") else { return b };
        let mut out = b;
        if let Some(c) = q.fg {
            out.fg = c;
        }
        if let Some(c) = q.bg {
            out.bg = c;
        }
        out.attrs |= q.attrs();
        out
    }

    fn text(&self, s: &str) -> String {
        if self.st.mono { mono_text(s) } else { s.to_string() }
    }

    /// A span's chunk: its scope's groups composed (`resolveGroupSet`), over the block's base style.
    fn chunk(&self, s: &Span, base: Base) -> StyledChunk {
        let (mut fg, mut bg, mut attrs) = (Color::TRANSPARENT, Color::TRANSPARENT, 0u8);
        let mut has_fg = false;
        let mut has_bg = false;
        for name in s.scope.groups() {
            if let Some(def) = group(self.st, name) {
                has_fg |= def.fg.is_some();
                has_bg |= def.bg.is_some();
                over(&mut fg, &mut bg, &mut attrs, def);
            }
            if name == "markup.strikethrough" {
                attrs |= attr::STRIKETHROUGH;
            }
        }
        StyledChunk { text: self.text(&s.text), fg: if has_fg { fg } else { base.fg }, bg: if has_bg { bg } else { base.bg }, attrs: attrs | base.attrs }
    }

    fn chunks(&self, spans: &[Span], base: Base) -> Vec<StyledChunk> {
        spans.iter().map(|s| self.chunk(s, base)).collect()
    }

    fn stack(&self, bufs: Vec<Buffer>, width: usize, gap: usize) -> Buffer {
        let total: usize = bufs.iter().map(|b| b.height as usize).sum::<usize>() + gap * bufs.len().saturating_sub(1);
        let mut out = Buffer::new(width as u16, total.max(1) as u16);
        let mut y = 0;
        for (i, b) in bufs.iter().enumerate() {
            if i > 0 {
                y += gap;
            }
            blit(&mut out, b, 0, y as i32);
            y += b.height as usize;
        }
        out
    }

    fn blocks(&self, blocks: &[Doc], width: usize, base: Base, gap: usize) -> Option<Buffer> {
        if blocks.is_empty() {
            return None;
        }
        Some(self.stack(blocks.iter().map(|b| self.block(b, width, base)).collect(), width, gap))
    }

    fn block(&self, b: &Doc, width: usize, base: Base) -> Buffer {
        let width = width.max(1);
        match b {
            Doc::Paragraph(spans) | Doc::Heading { spans, .. } => text_buffer(&self.chunks(spans, base), width, WrapMode::Word),
            Doc::Html(text) => text_buffer(&[StyledChunk::new(self.text(text), base.fg, base.bg, base.attrs)], width, WrapMode::Word),
            Doc::Code { lang, text } => {
                let text = if self.st.mono { mono_text(text) } else { text.clone() };
                code_block(&text, info_string_filetype(lang).as_deref(), width, self.st, false)
            }
            Doc::Rule => {
                let c = group(self.st, "conceal").and_then(|s| s.fg).unwrap_or(self.st.fg);
                let mut buf = Buffer::new(width as u16, 1);
                let g = if self.st.mono { "-" } else { "─" };
                for x in 0..width {
                    buf.set_blend(x as i32, 0, Glyph::Text(g.into()), c, Color::TRANSPARENT, 0);
                }
                buf
            }
            Doc::Quote(children) => {
                let c = group(self.st, "conceal").and_then(|s| s.fg).or(self.st.syntax.default_style().and_then(|s| s.fg)).unwrap_or(self.st.fg);
                let inner = self.blocks(children, width.saturating_sub(2).max(1), self.quoted(base), 1);
                let h = inner.as_ref().map(|b| b.height).unwrap_or(1).max(1);
                let mut buf = Buffer::new(width as u16, h);
                let bar = if self.st.mono { "|" } else { "│" };
                for y in 0..h {
                    buf.set_blend(0, y as i32, Glyph::Text(bar.into()), c, Color::TRANSPARENT, 0);
                }
                if let Some(inner) = inner {
                    blit(&mut buf, &inner, 2, 0);
                }
                buf
            }
            Doc::List { ordered, start, tight, items } => self.list(*ordered, *start, *tight, items, width, base),
            Doc::Table { aligns, rows } => self.table(aligns, rows, width),
        }
    }

    /// `updateList`.
    fn list(&self, ordered: bool, start: u64, tight: bool, items: &[Item], width: usize, base: Base) -> Buffer {
        let gap = usize::from(!tight);
        let count = items.len() as u64;
        let mw = if ordered { (start + count.saturating_sub(1)).to_string().len() + 1 } else { 1 };
        let marker = group(self.st, "markup.list");
        let mut rows = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let task = match item.task {
                None => "",
                Some(true) => " ☑",
                Some(false) => " ☐",
            };
            let number = if ordered { format!("{:>mw$}", format!("{}.", start + i as u64)) } else { "-".to_string() };
            let label = self.text(&format!("{number}{task} "));
            let lw = super::cell::str_width(&label);
            let content = self.blocks(&item.blocks, width.saturating_sub(lw).max(1), base, gap);
            let h = content.as_ref().map(|b| b.height).unwrap_or(1).max(1);
            let mut row = Buffer::new(width as u16, h);
            let chunk = StyledChunk::new(
                label,
                marker.and_then(|m| m.fg).unwrap_or(base.fg),
                marker.and_then(|m| m.bg).unwrap_or(base.bg),
                marker.map(|m| m.attrs()).unwrap_or(base.attrs),
            );
            row.draw_chunks(0, 0, &[chunk], lw);
            if let Some(c) = content {
                blit(&mut row, &c, lw as i32, 0);
            }
            rows.push(row);
        }
        self.stack(rows, width, gap)
    }

    /// A `columns` table: no borders, columns two cells apart and as wide as their content (shrunk
    /// proportionally to fit), word wrap, each column aligned as its delimiter row says.
    fn table(&self, aligns: &[Align], rows: &[Row], width: usize) -> Buffer {
        let cols = aligns.len().max(1);
        let base = self.root();
        let heading = group(self.st, "markup.heading").or(self.st.syntax.default_style());
        let cells: Vec<Vec<Vec<StyledChunk>>> = rows
            .iter()
            .map(|r| {
                (0..cols)
                    .map(|c| {
                        let spans = r.cells.get(c).map(Vec::as_slice).unwrap_or(&[]);
                        let mut chunks = self.chunks(spans, base);
                        if r.header
                            && let Some(h) = heading
                        {
                            for ch in &mut chunks {
                                let strike = ch.attrs & attr::STRIKETHROUGH;
                                if let Some(fg) = h.fg {
                                    ch.fg = fg;
                                }
                                if let Some(bg) = h.bg {
                                    ch.bg = bg;
                                }
                                ch.attrs = h.attrs() | strike;
                            }
                        }
                        if chunks.is_empty() { vec![StyledChunk::new(" ", base.fg, base.bg, base.attrs)] } else { chunks }
                    })
                    .collect()
            })
            .collect();
        let measure = |c: &[StyledChunk]| layout(c, usize::MAX / 4, WrapMode::None, false).iter().map(|l| l.width).max().unwrap_or(0);
        let mut intrinsic = vec![1usize; cols];
        for row in &cells {
            for (i, c) in row.iter().enumerate() {
                intrinsic[i] = intrinsic[i].max(measure(c).max(1));
            }
        }
        let gap = 2;
        let max_content = (width.saturating_sub(gap * cols.saturating_sub(1))).max(1);
        let current: usize = intrinsic.iter().sum();
        let widths = if current <= max_content { intrinsic } else { crate::ui::markdown::allocate_proportional(&intrinsic, max_content, 1) };
        let laid: Vec<Vec<Vec<super::text::VLine>>> = cells.iter().map(|row| row.iter().enumerate().map(|(i, c)| layout(c, widths[i].max(1), WrapMode::Word, false)).collect()).collect();
        let heights: Vec<usize> = laid.iter().map(|r| r.iter().map(Vec::len).max().unwrap_or(1).max(1)).collect();
        let mut buf = Buffer::new(width as u16, heights.iter().sum::<usize>().max(1) as u16);
        let mut y = 0;
        for (ri, row) in laid.iter().enumerate() {
            let mut x = 0;
            for (ci, lines) in row.iter().enumerate() {
                for (li, l) in lines.iter().enumerate().take(heights[ri]) {
                    let free = widths[ci].saturating_sub(l.width);
                    let left = match aligns.get(ci).copied().unwrap_or(Align::Left) {
                        Align::Left => 0,
                        Align::Right => free,
                        Align::Center => free / 2,
                    };
                    buf.draw_chunks((x + left) as i32, (y + li) as i32, &l.chunks, widths[ci] - left);
                }
                x += widths[ci] + gap;
            }
            y += heights[ri];
        }
        buf
    }
}

/// Render `content` (`streaming` while more may come) as top-level blocks.
pub fn render(content: &str, streaming: bool, width: usize, st: &MdStyle) -> Rendered {
    let doc = mdoc::parse(content, true, streaming);
    let lay = Lay { st };
    let width = width.max(1);
    let blocks: Vec<Block> = doc.iter().enumerate().map(|(i, b)| Block { margin_top: usize::from(i > 0), buf: lay.block(b, width, lay.root()) }).collect();
    let stable = if streaming { committed(content) } else { blocks.len() };
    Rendered { blocks, stable }
}

/// `committedBlockCount` of a streaming document: the top-level blocks its complete lines (the source up to its last
/// line ending) close. The last of them is still open unless a blank line ends it and a blank line can end it: a
/// list and an indented or unclosed fenced code block can go on after one. A heading (ATX or setext), a rule and a
/// closed fence end with their line.
pub fn committed(content: &str) -> usize {
    let Some(end) = content.rfind('\n') else { return 0 };
    let prefix = &content[..=end];
    let doc = mdoc::parse(prefix, true, true);
    let Some(last) = doc.last() else { return 0 };
    let lines: Vec<&str> = prefix.trim_end_matches('\n').split('\n').collect();
    let blank_after = prefix.ends_with("\n\n") || lines.last().is_some_and(|l| l.trim().is_empty());
    let last_line = lines.iter().rev().find(|l| !l.trim().is_empty()).map(|l| l.trim()).unwrap_or("");
    let closed_fence = || {
        let t = last_line.trim_start_matches(['>', ' ']);
        (t.starts_with("```") || t.starts_with("~~~")) && t.chars().skip(3).all(|c| c == '`' || c == '~') && lines.iter().filter(|l| { let t = l.trim_start_matches(['>', ' ']); t.starts_with("```") || t.starts_with("~~~") }).count() % 2 == 0
    };
    let open = match last {
        Doc::Heading { .. } | Doc::Rule => false,
        Doc::Code { .. } => !closed_fence(),
        Doc::List { .. } => true,
        _ => !blank_after,
    };
    doc.len() - usize::from(open)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Mode, OPENCODE_V2, resolve};

    fn rows(content: &str, mono: bool, width: usize) -> Vec<String> {
        let doc: serde_json::Value = serde_json::from_str(OPENCODE_V2).unwrap();
        let t = resolve::resolve_document(&doc, Mode::Dark).unwrap();
        let theme = super::super::theme::resolve(&t, None, false);
        let syntax = theme.block.syntax.clone().unwrap_or_default();
        let st = MdStyle { syntax: &syntax, fg: Color::WHITE, mono };
        let buf = render(content, false, width, &st).compose(width as u16);
        (0..buf.height).map(|y| buf.row_text(y)).collect()
    }

    /// The scrollback rows 2.4.1's mini printed on a pty (`opencode mini`, mono off and on).
    #[test]
    fn matches_opencode_241_mini_rows() {
        let lists = "- [ ] todo item\n- [x] done item\n- plain\n  - nested a\n    - deeper\n  1. ordered nested\n  2. second\n3. starts at three\n4. four\n* star bullet\n+ plus bullet\n\n> quote line one\n> quote **bold**\n\nLoose list:\n\n- a\n\n- b";
        let want = [
            "- ☐ todo item", "- ☑ done item", "- plain", "  - nested a", "    - deeper", "  1. ordered nested", "  2. second", "", "3. starts at three", "4. four", "",
            "- star bullet", "", "- plus bullet", "", "│ quote line one", "│ quote bold", "", "Loose list:", "", "- a", "", "- b",
        ];
        assert_eq!(rows(lists, false, 100), want);
        let mono: Vec<String> = want.iter().map(|r| r.replace('☐', "[ ]").replace('☑', "[x]").replace('│', "|")).collect();
        assert_eq!(rows(lists, true, 100), mono);
        let table = "| Name | Value |\n|:-----|------:|\n| alpha | 1 |\n| `beta` | **2** |";
        assert_eq!(rows(table, false, 100), ["Name   Value", "alpha      1", "beta       2"]);
        assert_eq!(rows("Text\n___\nEnd.", true, 10), ["Text", "", "----------", "", "End."]);
        let thought = "## Plan\n\n| step | risk |\n| --- | --- |\n| check | low |\n\n```ts\nconst probe = 1\n```\nDone thinking.";
        assert_eq!(rows(thought, false, 100), ["Plan", "", "step   risk", "check  low", "", "const probe = 1", "", "Done thinking."]);
    }
}
