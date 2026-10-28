//! OpenTUI `MarkdownRenderable` in `internalBlockMode: "top-level"` (`renderables/Markdown.ts`): top-level
//! marked tokens become render blocks (paragraph-like blocks are highlighted markdown code, lists are
//! marker + content rows, code blocks, block quotes, rules and borderless column tables), stacked with
//! one-row top margins between separated blocks. Streaming keeps the trailing tokens unstable.

use super::cell::{Buffer, Color, Glyph, StyledChunk, attr};
use super::highlight::{code_highlights, markdown_highlights, to_chunks};
use super::marked::{Kind, ListItem, ParseState, Token, parse_incremental};
use super::text::{WrapMode, layout};
use super::theme::MiniSyntax;

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
    /// `_stableBlockCount`.
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

fn is_separated(t: &Token) -> bool {
    matches!(t.kind, Kind::Code { .. } | Kind::Table { .. } | Kind::Blockquote { .. } | Kind::Hr)
}

fn is_separated_top_level(t: &Token) -> bool {
    matches!(t.kind, Kind::Heading { .. } | Kind::List { .. }) || is_separated(t)
}

fn trailing_breaks(s: &str) -> bool {
    let t = s.trim_end_matches('\n');
    s.len() - t.len() >= 2 || s.ends_with("\r\n\r\n") || s.ends_with("\n\r\n")
}

/// `normalizeScrollbackMarkdownBlockRaw`: drop trailing newlines.
fn strip_trailing_newlines(raw: &str) -> &str {
    raw.trim_end_matches(['\n', '\r'])
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

/// A `CodeRenderable` with `filetype: "markdown"` and conceal.
fn markdown_code(content: &str, width: usize, st: &MdStyle, base: Option<&str>) -> Buffer {
    let chunks = to_chunks(content, &markdown_highlights(content), st.syntax, true, base, st.fg);
    text_buffer(&chunks, width, WrapMode::Word)
}

static BLANK_END: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| regex::Regex::new(r"\n[ \t]*\n$").unwrap());

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

fn style_chunk(text: &str, group: &str, st: &MdStyle) -> StyledChunk {
    let s = st.syntax.get(group).or_else(|| group.split_once('.').and_then(|(b, _)| st.syntax.get(b))).or(st.syntax.default_style());
    StyledChunk { text: text.into(), fg: s.and_then(|s| s.fg).unwrap_or(st.fg), bg: s.and_then(|s| s.bg).unwrap_or(Color::TRANSPARENT), attrs: s.map(|s| s.attrs()).unwrap_or(0) }
}

fn border_color(st: &MdStyle) -> Color {
    st.syntax.get("conceal").and_then(|s| s.fg).or(st.syntax.default_style().and_then(|s| s.fg)).unwrap_or(st.fg)
}

fn blockquote(token: &Token, width: usize, st: &MdStyle) -> Buffer {
    let text = token.text().filter(|t| !t.is_empty()).unwrap_or(" ");
    let inner = markdown_code(text, width.saturating_sub(2).max(1), st, Some("markup.quote"));
    let mut buf = Buffer::new(width as u16, inner.height);
    let c = border_color(st);
    for y in 0..inner.height {
        buf.set_blend(0, y as i32, Glyph::Text("│".into()), c, Color::TRANSPARENT, 0);
    }
    blit(&mut buf, &inner, 2, 0);
    buf
}

fn hr(width: usize, st: &MdStyle) -> Buffer {
    let c = st.syntax.get("conceal").and_then(|s| s.fg).unwrap_or(st.fg);
    let mut buf = Buffer::new(width as u16, 1);
    for x in 0..width {
        buf.set_blend(x as i32, 0, Glyph::Text("─".into()), c, Color::TRANSPARENT, 0);
    }
    buf
}

/// Inline tokens of a table cell as chunks (`renderInlineContent` over marked inline tokens).
fn inline_chunks(text: &str, st: &MdStyle, header: bool) -> Vec<StyledChunk> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    let mut out: Vec<StyledChunk> = Vec::new();
    let mut groups: Vec<&str> = Vec::new();
    for ev in Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH) {
        match ev {
            Event::Start(Tag::Strong) => groups.push("markup.strong"),
            Event::Start(Tag::Emphasis) => groups.push("markup.italic"),
            Event::Start(Tag::Strikethrough) => groups.push("markup.strikethrough"),
            Event::Start(Tag::Link { .. }) => groups.push("markup.link.label"),
            Event::End(TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::Link) => {
                groups.pop();
            }
            Event::Text(t) => out.push(style_chunk(&t, groups.last().copied().unwrap_or("default"), st)),
            Event::Code(t) => out.push(style_chunk(&t, "markup.raw", st)),
            Event::SoftBreak | Event::HardBreak => out.push(style_chunk("\n", "default", st)),
            _ => {}
        }
    }
    if out.is_empty() {
        out.push(style_chunk(" ", "default", st));
    }
    if header {
        let h = st.syntax.get("markup.heading").or(st.syntax.default_style()).copied();
        if let Some(h) = h {
            for c in out.iter_mut() {
                if let Some(fg) = h.fg {
                    c.fg = fg;
                }
                if let Some(bg) = h.bg {
                    c.bg = bg;
                }
                c.attrs = h.attrs();
            }
        }
    }
    out
}

fn allocate_proportional(widths: &[usize], target: usize, min: usize) -> Vec<usize> {
    let base: Vec<usize> = widths.iter().map(|w| (*w).max(min)).collect();
    let total: usize = base.iter().sum();
    let capacity: Vec<usize> = base.iter().map(|w| w - min).collect();
    let n = base.len();
    let mut growth = vec![0usize; n];
    let available = (target.saturating_sub(min * n)).min(total - min * n);
    if available == 0 {
        return vec![min; n];
    }
    if available == capacity.iter().sum::<usize>() {
        return base;
    }
    let weights: Vec<f64> = capacity.iter().map(|c| (*c as f64).sqrt()).collect();
    let mut active: Vec<(usize, usize, f64)> = capacity.iter().enumerate().filter(|(_, w)| **w > 0).map(|(i, w)| (i, *w, weights[i])).collect();
    active.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap());
    if active.len() == n && capacity.iter().all(|c| *c == capacity[0]) {
        let shared = available / n;
        let rem = available % n;
        return (0..n).map(|i| min + shared + usize::from(i < rem)).collect();
    }
    let mut remaining = available as f64;
    let mut total_weight: f64 = active.iter().map(|a| a.2).sum();
    for (idx, w, weight) in &active {
        if remaining / total_weight <= *weight {
            break;
        }
        growth[*idx] = *w;
        remaining -= *w as f64;
        total_weight -= weight;
    }
    let level = remaining / total_weight;
    for (idx, w, weight) in &active {
        if growth[*idx] == *w {
            continue;
        }
        growth[*idx] = (*w).min((level * weight).floor() as usize);
    }
    let cmp = |lg: usize, lc: usize, rg: usize, rc: usize| ((lg * lg * rc) as u128).cmp(&((rg * rg * lc) as u128));
    let mut allocated: usize = growth.iter().sum();
    while allocated > available {
        let mut worst: Option<usize> = None;
        for i in 0..n {
            if growth[i] == 0 {
                continue;
            }
            let better = match worst {
                None => true,
                Some(w) => {
                    let c = cmp(growth[i], capacity[i], growth[w], capacity[w]);
                    c == std::cmp::Ordering::Greater || (c == std::cmp::Ordering::Equal && i > w)
                }
            };
            if better {
                worst = Some(i);
            }
        }
        let Some(w) = worst else { break };
        growth[w] -= 1;
        allocated -= 1;
    }
    while allocated < available {
        let mut best: Option<usize> = None;
        for i in 0..n {
            if growth[i] >= capacity[i] {
                continue;
            }
            let better = match best {
                None => true,
                Some(b) => cmp(growth[i] + 1, capacity[i], growth[b] + 1, capacity[b]) == std::cmp::Ordering::Less,
            };
            if better {
                best = Some(i);
            }
        }
        let Some(b) = best else { break };
        growth[b] += 1;
        allocated += 1;
    }
    growth.iter().map(|g| g + min).collect()
}

/// Borderless `columns` table (`TextTableRenderable`, `widthMode: "content"`, gap 2, word wrap).
fn table(header: &[String], rows: &[Vec<String>], width: usize, st: &MdStyle) -> Buffer {
    let cols = header.len();
    let mut cells: Vec<Vec<Vec<StyledChunk>>> = vec![header.iter().map(|h| inline_chunks(h, st, true)).collect()];
    for r in rows {
        cells.push((0..cols).map(|i| inline_chunks(r.get(i).map(|s| s.as_str()).unwrap_or(""), st, false)).collect());
    }
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
    let widths = if current <= max_content { intrinsic } else { allocate_proportional(&intrinsic, max_content, 1) };
    let mut row_heights = Vec::new();
    let mut laid: Vec<Vec<Vec<super::text::VLine>>> = Vec::new();
    for row in &cells {
        let mut h = 1;
        let mut lrow = Vec::new();
        for (i, c) in row.iter().enumerate() {
            let l = layout(c, widths[i].max(1), WrapMode::Word, false);
            h = h.max(l.len());
            lrow.push(l);
        }
        row_heights.push(h);
        laid.push(lrow);
    }
    let total_h: usize = row_heights.iter().sum();
    let mut buf = Buffer::new(width as u16, total_h.max(1) as u16);
    for y in 0..buf.height {
        for x in 0..buf.width {
            buf.set_blend(x as i32, y as i32, Glyph::Text(" ".into()), Color::WHITE, Color::TRANSPARENT, 0);
        }
    }
    let mut y = 0;
    for (ri, lrow) in laid.iter().enumerate() {
        let mut x = 0;
        for (ci, lines) in lrow.iter().enumerate() {
            for (li, l) in lines.iter().enumerate().take(row_heights[ri]) {
                buf.draw_chunks(x as i32, (y + li) as i32, &l.chunks, widths[ci]);
            }
            x += widths[ci] + gap;
        }
        y += row_heights[ri];
    }
    buf
}

fn list(token: &Token, width: usize, st: &MdStyle) -> Buffer {
    let Kind::List { ordered, start, items, .. } = &token.kind else { return Buffer::new(width as u16, 1) };
    let start = start.unwrap_or(1);
    let marker_width = items.iter().enumerate().map(|(i, _)| if *ordered { format!("{}.", start + i as u64).len() } else { 1 }).max().unwrap_or(1).max(1);
    let mut rows: Vec<(Buffer, usize)> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let marker = if *ordered { format!("{}.", start + i as u64) } else { "-".to_string() };
        let content_w = width.saturating_sub(marker_width + 1).max(1);
        let content = list_item_content(item, content_w, st);
        let h = content.height.max(1);
        let mut row = Buffer::new(width as u16, h);
        let m = style_chunk(&format!("{:>w$} ", marker, w = marker_width), "markup.list", st);
        row.draw_chunks(0, 0, &[m], marker_width + 1);
        blit(&mut row, &content, (marker_width + 1) as i32, 0);
        let margin_bottom = usize::from(BLANK_END.is_match(&item.raw));
        rows.push((row, margin_bottom));
    }
    let total: usize = rows.iter().map(|(b, m)| b.height as usize + m).sum();
    let mut buf = Buffer::new(width as u16, total.max(1) as u16);
    let mut y = 0;
    for (row, mb) in rows {
        blit(&mut buf, &row, 0, y as i32);
        y += row.height as usize + mb;
    }
    buf
}

fn list_item_content(item: &ListItem, width: usize, st: &MdStyle) -> Buffer {
    let mut children: Vec<(Buffer, usize)> = Vec::new();
    let mut pending = 0;
    for t in &item.tokens {
        match t.kind {
            Kind::Checkbox { .. } => continue,
            Kind::Space => {
                pending = pending.max(1);
                continue;
            }
            _ => {}
        }
        let b = match &t.kind {
            Kind::Text { .. } | Kind::Paragraph { .. } => markdown_code(strip_trailing_newlines(&t.raw), width, st, None),
            Kind::List { .. } => list(t, width, st),
            Kind::Code { lang, text } => code_block(text, lang.as_deref().and_then(info_string_filetype).as_deref(), width, st, false),
            Kind::Blockquote { .. } => blockquote(t, width, st),
            Kind::Hr => hr(width, st),
            Kind::Table { header, rows, .. } => table(header, rows, width, st),
            _ if !t.raw.is_empty() => markdown_code(&t.raw, width, st, None),
            _ => continue,
        };
        let margin = if matches!(t.kind, Kind::List { .. }) { 0 } else { pending };
        pending = 0;
        children.push((b, margin));
    }
    let total: usize = children.iter().map(|(b, m)| b.height as usize + m).sum();
    let mut buf = Buffer::new(width as u16, total.max(1) as u16);
    let mut y = 0;
    for (b, m) in children {
        y += m;
        blit(&mut buf, &b, 0, y as i32);
        y += b.height as usize;
    }
    buf
}

fn render_block(token: &Token, width: usize, st: &MdStyle) -> Option<Buffer> {
    Some(match &token.kind {
        Kind::Code { lang, text } => code_block(text, lang.as_deref().and_then(info_string_filetype).as_deref(), width, st, false),
        Kind::Table { header, rows, .. } => {
            if header.is_empty() || rows.is_empty() {
                markdown_code(&token.raw, width, st, None)
            } else {
                table(header, rows, width, st)
            }
        }
        Kind::Blockquote { .. } => blockquote(token, width, st),
        Kind::List { .. } => list(token, width, st),
        Kind::Hr => hr(width, st),
        _ => {
            let raw = strip_trailing_newlines(&token.raw);
            if raw.is_empty() {
                return None;
            }
            markdown_code(raw, width, st, None)
        }
    })
}

/// Parse `content` incrementally (keeping the previous state) and render its top-level blocks.
pub fn render(content: &str, prev: Option<&ParseState>, streaming: bool, width: usize, st: &MdStyle) -> (ParseState, Rendered) {
    let state = parse_incremental(content, prev, if streaming { 2 } else { 0 });
    let rendered = render_state(&state, width, st);
    (state, rendered)
}

/// Render already parsed tokens.
pub fn render_state(state: &ParseState, width: usize, st: &MdStyle) -> Rendered {
    let mut blocks: Vec<Block> = Vec::new();
    let mut tokens_of: Vec<(&Token, usize)> = Vec::new();
    let mut gap = String::new();
    for (i, t) in state.tokens.iter().enumerate() {
        if t.kind == Kind::Space {
            gap.push_str(&t.raw);
            continue;
        }
        let margin = match tokens_of.last() {
            Some((prev, _)) => {
                let paragraphs = matches!(prev.kind, Kind::Paragraph { .. }) && matches!(t.kind, Kind::Paragraph { .. }) && trailing_breaks(&format!("{}{}", prev.raw, gap));
                usize::from(is_separated_top_level(prev) || is_separated_top_level(t) || paragraphs)
            }
            None => 0,
        };
        tokens_of.push((t, i + 1));
        gap.clear();
        if let Some(buf) = render_block(t, width, st) {
            blocks.push(Block { margin_top: margin, buf });
        }
    }
    let stable = tokens_of.iter().take_while(|(_, end)| *end <= state.stable).count();
    let _ = attr::BOLD;
    Rendered { blocks, stable }
}
