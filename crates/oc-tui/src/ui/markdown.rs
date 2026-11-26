//! Markdown → styled lines: OpenTUI 0.9.4's `<markdown>` (`renderables/Markdown.ts` over the native engine) as
//! opencode's `TextPart` and `ReasoningBody` use it (`tableOptions: { style: "grid", cellPaddingX: 1 }`).
//!
//! Top-level blocks are separated by one blank row. A paragraph or heading keeps its line breaks and wraps by word; a
//! setext heading keeps its underline row. A block quote is a `│` bar (the `default` style's color) with its blocks a
//! row apart, its text in `markup.quote`. A list's items are a marker column (`-`, or numbers aligned on their right,
//! then a task box) and a content column; a loose list puts a row between items and between an item's blocks. A
//! table fills the width (`widthMode: "full"`, proportional fitting), with `#888888` borders, a header in the heading
//! style and each column aligned as its delimiter row says. A rule is `─` in the Markdown color. Code blocks wrap by
//! word and never show their fences.
//!
//! `conceal` hides the syntax markers (`session.markdown = "rendered"`); without it each leaf shows its source.

use super::highlight::{highlight, token_style};
use super::mdoc::{self, Align, Block, Item, Row, Scope, Span};
use super::text::{Click, SLine, Seg, Wrap, wrap};
use crate::mini::cell::{Color as CellColor, StyledChunk};
use crate::mini::text::{WrapMode, layout};
use crate::theme::{Rgba, SyntaxStyle, Theme, TokenStyle};
use crate::util::locale::width as text_width;
use crate::util::session_mention::{parse_session_mention, session_mentions};
use ratatui::style::{Color, Modifier, Style};
use std::cell::RefCell;

pub struct MdOpts<'a> {
    pub theme: &'a Theme,
    /// The syntax style: the theme's, or the thinking style (every group in `text.muted`) for reasoning.
    pub syntax: &'a SyntaxStyle,
    pub conceal: bool,
    /// Base foreground (`markdown.text`, or `text.muted` for reasoning).
    pub fg: Rgba,
    /// Whether more content is still to come (`streaming`): the last line shows as far as it is decided.
    pub streaming: bool,
    /// Show session IDs as links (`useSessionLinks`).
    pub links: Option<&'a SessionLinks<'a>>,
}

/// `useSessionLinks` (`ui/session-link.ts`): session IDs in prose show as links titled after their
/// sessions (`@<label>` when markers are concealed, the ID itself otherwise) and open them on click.
/// `label` answers `None` for IDs that stay plain (the current session, archived sessions and ones
/// that are not loaded). Every ID a render mentions is recorded with the label it got, so a cache
/// can re-render only what mentions a session whose label changed.
pub struct SessionLinks<'a> {
    label: &'a dyn Fn(&str) -> Option<String>,
    used: RefCell<Vec<(String, Option<String>)>>,
}

impl<'a> SessionLinks<'a> {
    pub fn new(label: &'a dyn Fn(&str) -> Option<String>) -> Self {
        SessionLinks { label, used: RefCell::new(Vec::new()) }
    }

    pub fn label(&self, id: &str) -> Option<String> {
        let label = (self.label)(id);
        let mut used = self.used.borrow_mut();
        if !used.iter().any(|(u, _)| u == id) {
            used.push((id.to_string(), label.clone()));
        }
        label
    }

    /// The IDs mentioned so far, each with the label it showed (`None`: plain), in first-seen order.
    pub fn take(&self) -> Vec<(String, Option<String>)> {
        std::mem::take(&mut *self.used.borrow_mut())
    }
}

/// The table border color when the syntax has no `conceal` group (`resolveTableRenderableOptions`).
const TABLE_BORDER: Rgba = Rgba::rgb(0x88, 0x88, 0x88);

type Rows = Vec<Vec<Seg>>;

struct Lay<'a> {
    o: &'a MdOpts<'a>,
    table: TableOpts,
}

/// The table options (`MarkdownTableOptions`): `full` fills the width (`widthMode: "full"`, else `"content"`), `pad`
/// is `cellPaddingX`. opencode's answers and reasoning use full width and one cell of padding.
#[derive(Clone, Copy, Debug)]
pub struct TableOpts {
    pub full: bool,
    pub pad: usize,
}

/// The style of text no part of a block styles: the Markdown color, under `markup.quote` for each quote around it.
#[derive(Clone, Copy)]
struct Base {
    style: Style,
}

fn modifiers(t: &TokenStyle) -> Modifier {
    let mut m = Modifier::empty();
    if t.bold {
        m |= Modifier::BOLD;
    }
    if t.italic {
        m |= Modifier::ITALIC;
    }
    if t.underline {
        m |= Modifier::UNDERLINED;
    }
    m
}

impl<'a> Lay<'a> {
    fn root(&self) -> Base {
        Base { style: Style::default().fg(self.o.fg.tui()) }
    }

    fn quoted(&self, b: Base) -> Base {
        let Some(q) = self.o.syntax.lookup("markup.quote") else { return b };
        let mut style = b.style;
        if let Some(fg) = q.fg {
            style = style.fg(fg.tui());
        }
        if let Some(bg) = q.bg {
            style = style.bg(bg.tui());
        }
        Base { style: style.add_modifier(modifiers(&q)) }
    }

    /// A span's style: the scope's groups composed over the base (`resolveGroupSet`).
    fn scope(&self, base: Base, scope: &Scope) -> Style {
        let mut style = base.style;
        for name in scope.groups() {
            let Some(t) = self.o.syntax.lookup(name) else {
                if name == "markup.strikethrough" {
                    style = style.add_modifier(Modifier::CROSSED_OUT);
                }
                continue;
            };
            if let Some(fg) = t.fg {
                style = style.fg(fg.tui());
            }
            if let Some(bg) = t.bg {
                style = style.bg(bg.tui());
            }
            style = style.add_modifier(modifiers(&t));
            if name == "markup.strikethrough" {
                style = style.add_modifier(Modifier::CROSSED_OUT);
            }
        }
        style
    }

    /// A prose leaf's spans as segments, session IDs linked (the inline hook; table cells never get it).
    fn segs(&self, spans: &[Span], base: Base, prose: bool) -> Vec<Seg> {
        let mut out = Vec::new();
        for s in spans {
            let style = self.scope(base, &s.scope);
            match self.o.links.filter(|_| prose) {
                Some(links) => self.link_ids(&mut out, s, style, links),
                None => out.push(Seg::new(s.text.clone(), style)),
            }
        }
        out
    }

    /// `linkSpans`: a span's session IDs become links. A link's text stays, and a code span links only when it is
    /// exactly a session ID, so a command such as `opencode --session ses_…` stays copyable.
    fn link_ids(&self, out: &mut Vec<Seg>, s: &Span, style: Style, links: &SessionLinks) {
        let exact = || parse_session_mention(s.text.trim_start_matches('`').trim_end_matches('`').trim()).is_some();
        if s.link.is_some() || (s.code && !exact()) {
            out.push(Seg::new(s.text.clone(), style));
            return;
        }
        let mut at = 0;
        for m in session_mentions(&s.text) {
            if m.start > at {
                out.push(Seg::new(s.text[at..m.start].to_string(), style));
            }
            let raw = &s.text[m.start..m.end];
            match links.label(&m.id) {
                Some(label) => {
                    let shown = if self.o.conceal { format!("@{label}") } else { raw.to_string() };
                    let url = token_style(self.o.syntax.get("markup.link.url"));
                    let mut link = style;
                    link.fg = url.fg.or(style.fg);
                    link.add_modifier = url.add_modifier | Modifier::UNDERLINED;
                    link.sub_modifier = Modifier::empty();
                    out.push(Seg::new(shown, link).on_click(Click::SessionLink(m.id)));
                }
                None => out.push(Seg::new(raw.to_string(), style)),
            }
            at = m.end;
        }
        if at < s.text.len() {
            out.push(Seg::new(s.text[at..].to_string(), style));
        }
    }

    /// Blocks stacked with `gap` blank rows between them.
    fn blocks(&self, blocks: &[Block], width: usize, base: Base, gap: usize) -> Rows {
        let mut out: Rows = Vec::new();
        for (i, b) in blocks.iter().enumerate() {
            if i > 0 {
                for _ in 0..gap {
                    out.push(vec![]);
                }
            }
            out.extend(self.block(b, width, base));
        }
        out
    }

    fn block(&self, b: &Block, width: usize, base: Base) -> Rows {
        let width = width.max(1);
        match b {
            Block::Paragraph(spans) | Block::Heading { spans, .. } => word(&self.segs(spans, base, true), width),
            Block::Html(text) => word(&[Seg::new(text.clone(), base.style)], width),
            Block::Code { lang, text } => self.code(lang, text, width),
            Block::Rule => vec![vec![Seg::new("─".repeat(width), Style::default().fg(self.o.fg.tui()))]],
            Block::Table { aligns, rows } => self.table(aligns, rows, width),
            Block::Quote(children) => {
                let bar = Style::default().fg(self.o.syntax.lookup("default").and_then(|d| d.fg).unwrap_or(self.o.fg).tui());
                let inner = self.blocks(children, width.saturating_sub(2), self.quoted(base), 1);
                // An empty quote still shows its bar.
                let rows = if inner.is_empty() { vec![vec![]] } else { inner };
                rows.into_iter()
                    .map(|r| {
                        let mut row = vec![Seg::new("│", bar)];
                        if !r.is_empty() {
                            row.push(Seg::new(" ", Style::default()));
                            row.extend(r);
                        }
                        row
                    })
                    .collect()
            }
            Block::List { ordered, start, tight, items } => self.list(*ordered, *start, *tight, items, width, base),
        }
    }

    /// A code block (`CodeRenderable`, word wrap): highlighted, its plain text in the Markdown color.
    fn code(&self, lang: &str, text: &str, width: usize) -> Rows {
        let plain = Style::default().fg(self.o.fg.tui());
        let default = token_style(self.o.syntax.default);
        let lines: Rows = if lang.is_empty() {
            text.split('\n').map(|l| vec![Seg::new(l.to_string(), plain)]).collect()
        } else {
            highlight(text, lang, self.o.syntax)
                .into_iter()
                .map(|l| l.into_iter().map(|s| if s.style == default { Seg::new(s.text, plain) } else { s }).collect())
                .collect()
        };
        let mut out = Vec::new();
        for l in lines {
            out.extend(word(&l, width));
        }
        out
    }

    /// `updateList`: markers are `-` or numbers aligned on their right, then the task box; a loose list puts a row
    /// between its items and between an item's blocks.
    fn list(&self, ordered: bool, start: u64, tight: bool, items: &[Item], width: usize, base: Base) -> Rows {
        let gap = usize::from(!tight);
        let count = items.len() as u64;
        let mw = if ordered { (start + count.saturating_sub(1)).to_string().len() + 1 } else { 1 };
        let marker_style = match self.o.syntax.lookup("markup.list") {
            Some(t) => {
                let mut s = Style::default().fg(t.fg.map(|c| c.tui()).or(base.style.fg).unwrap_or(Color::Reset));
                if let Some(bg) = t.bg.map(|c| c.tui()).or(base.style.bg) {
                    s = s.bg(bg);
                }
                s.add_modifier(modifiers(&t))
            }
            None => base.style,
        };
        let mut out = Vec::new();
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                for _ in 0..gap {
                    out.push(vec![]);
                }
            }
            let task = match item.task {
                None => "",
                Some(true) if self.o.conceal => " ☑",
                Some(false) if self.o.conceal => " ☐",
                Some(true) => " [x]",
                Some(false) => " [ ]",
            };
            let number = if ordered { format!("{:>mw$}", format!("{}.", start + i as u64)) } else { "-".to_string() };
            let label = format!("{number}{task} ");
            let lw = text_width(&label);
            let content = self.blocks(&item.blocks, width.saturating_sub(lw).max(1), base, gap);
            let rows = if content.is_empty() { vec![vec![]] } else { content };
            for (k, r) in rows.into_iter().enumerate() {
                let mut row = if k == 0 { vec![Seg::new(label.clone(), marker_style)] } else if r.is_empty() { vec![] } else { vec![Seg::new(" ".repeat(lw), Style::default())] };
                row.extend(r);
                out.push(row);
            }
        }
        out
    }

    /// `TextTableRenderable` in the grid style: full width, proportional fitting, word wrap, one cell of padding on
    /// each side, a border on every side and between all rows and columns.
    fn table(&self, aligns: &[Align], rows: &[Row], width: usize) -> Rows {
        let cols = aligns.len();
        if cols == 0 || rows.is_empty() {
            return vec![];
        }
        let base = self.root();
        let heading = self.o.syntax.lookup("markup.heading").or_else(|| self.o.syntax.lookup("default"));
        let cells: Vec<Vec<Vec<Seg>>> = rows
            .iter()
            .map(|r| {
                (0..cols)
                    .map(|c| {
                        let spans = r.cells.get(c).map(Vec::as_slice).unwrap_or(&[]);
                        let mut segs = self.segs(spans, base, false);
                        if r.header
                            && let Some(h) = heading
                        {
                            for s in &mut segs {
                                let strike = s.style.add_modifier.contains(Modifier::CROSSED_OUT);
                                let mut st = Style::default().fg(h.fg.map(|c| c.tui()).or(s.style.fg).unwrap_or(Color::Reset)).add_modifier(modifiers(&h));
                                if let Some(bg) = h.bg.map(|c| c.tui()).or(s.style.bg) {
                                    st = st.bg(bg);
                                }
                                if strike {
                                    st = st.add_modifier(Modifier::CROSSED_OUT);
                                }
                                s.style = st;
                            }
                        }
                        if segs.is_empty() { vec![Seg::new(" ", base.style)] } else { segs }
                    })
                    .collect()
            })
            .collect();
        let px = self.table.pad;
        let pad = px * 2;
        let mut intrinsic = vec![1 + pad; cols];
        for row in &cells {
            for (c, cell) in row.iter().enumerate() {
                let w = wrap(cell, usize::MAX / 4, Wrap::None).iter().map(|l| l.iter().map(Seg::width).sum::<usize>()).max().unwrap_or(0);
                intrinsic[c] = intrinsic[c].max(w.max(1) + pad);
            }
        }
        let max_content = width.saturating_sub(cols + 1).max(1);
        let total: usize = intrinsic.iter().sum();
        let widths = if total == max_content {
            intrinsic
        } else if total < max_content && !self.table.full {
            intrinsic
        } else if total < max_content {
            let extra = max_content - total;
            let (share, rem) = (extra / cols, extra % cols);
            intrinsic.iter().enumerate().map(|(i, w)| w + share + usize::from(i < rem)).collect()
        } else {
            allocate_proportional(&intrinsic, max_content, 1 + pad)
        };
        let border = Style::default().fg(TABLE_BORDER.tui());
        let rule = |l: &str, m: &str, r: &str| -> Vec<Seg> {
            let mut s = l.to_string();
            for (i, w) in widths.iter().enumerate() {
                s.push_str(&"─".repeat(*w));
                s.push_str(if i + 1 == cols { r } else { m });
            }
            vec![Seg::new(s, border)]
        };
        let mut out = vec![rule("┌", "┬", "┐")];
        for (ri, row) in cells.iter().enumerate() {
            let laid: Vec<Rows> = row.iter().enumerate().map(|(c, cell)| word(cell, widths[c] - pad)).collect();
            let h = laid.iter().map(Vec::len).max().unwrap_or(1).max(1);
            for li in 0..h {
                let mut segs = vec![Seg::new("│", border)];
                for (c, w) in widths.iter().enumerate() {
                    let inner = w - pad;
                    let line = laid[c].get(li).cloned().unwrap_or_default();
                    let lw: usize = line.iter().map(Seg::width).sum();
                    let free = inner.saturating_sub(lw);
                    let left = match aligns[c] {
                        Align::Left => 0,
                        Align::Right => free,
                        Align::Center => free / 2,
                    };
                    if px + left > 0 {
                        segs.push(Seg::new(" ".repeat(px + left), Style::default()));
                    }
                    segs.extend(line);
                    if free - left + px > 0 {
                        segs.push(Seg::new(" ".repeat(free - left + px), Style::default()));
                    }
                    segs.push(Seg::new("│", border));
                }
                out.push(segs);
            }
            out.push(if ri + 1 < cells.len() { rule("├", "┼", "┤") } else { rule("└", "┴", "┘") });
        }
        out
    }
}

/// OpenTUI's word wrap (`TextBufferView`, `wrapMode: "word"`): a row breaks after whitespace and after `-`, `/`,
/// `\`, `.`, `,`, `;`, `:`, `!`, `?` and brackets, and between CJK characters; a word wider than the row breaks by
/// character. `\n` starts a row. The break positions come from mini's port of it (`mini::text::layout`).
fn word(segs: &[Seg], width: usize) -> Rows {
    let chunks: Vec<StyledChunk> = segs.iter().map(|s| StyledChunk::new(s.text.clone(), CellColor::WHITE, CellColor::TRANSPARENT, 0)).collect();
    let mut starts = Vec::with_capacity(segs.len());
    let mut at = 0;
    for s in segs {
        starts.push(at);
        at += s.text.len();
    }
    layout(&chunks, width.max(1), WrapMode::Word, false)
        .into_iter()
        .map(|line| {
            let mut row = Vec::new();
            for (i, s) in segs.iter().enumerate() {
                let (a, b) = (starts[i], starts[i] + s.text.len());
                let (from, to) = (a.max(line.start), b.min(line.end));
                if from >= to {
                    continue;
                }
                let mut piece = s.clone();
                piece.text = s.text[from - a..to - a].replace('\t', "  ");
                row.push(piece);
            }
            row
        })
        .collect()
}

/// `allocateProportionalColumnWidths` (`text-table-width.ts`): shrinks columns toward `min`, the wide ones most.
pub fn allocate_proportional(widths: &[usize], target: usize, min: usize) -> Vec<usize> {
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

/// Render markdown text to lines of at most `width` cells.
pub fn render(text: &str, width: usize, o: &MdOpts) -> Vec<SLine> {
    render_with(text, width, o, TableOpts { full: true, pad: 1 })
}

/// Markdown shown as its source in a code view (`<code filetype="markdown" conceal={false}>`, reasoning in source
/// mode): the text as written, highlighted, its plain text in `o.fg`, wrapped by word.
pub fn source(text: &str, width: usize, o: &MdOpts) -> Vec<SLine> {
    Lay { o, table: TableOpts { full: true, pad: 1 } }.code("markdown", text, width.max(1)).into_iter().map(SLine::new).collect()
}

/// [`render`] with other table options.
pub fn render_with(text: &str, width: usize, o: &MdOpts, table: TableOpts) -> Vec<SLine> {
    let doc = mdoc::parse(text, o.conceal, o.streaming);
    let lay = Lay { o, table };
    lay.blocks(&doc, width.max(1), lay.root(), 1).into_iter().map(SLine::new).collect()
}

#[cfg(test)]
mod tests;
