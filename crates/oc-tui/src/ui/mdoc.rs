//! A Markdown document as OpenTUI 0.9.4's native engine shows it (`packages/native/src/markdown.zig`,
//! `markdown-inline.zig`; Core's `renderables/Markdown.ts` lays it out): CommonMark with the GFM tables, task list
//! items, strikethrough and extended autolinks, parsed by pulldown-cmark. Each prose leaf (paragraph, heading, table
//! cell) is its display text, line breaks kept, split into spans that carry the Markdown scopes Core styles them
//! with. The full TUI (`ui/markdown.rs`) and mini (`mini/markdown.rs`) lay the blocks out.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, LinkType, Options, Parser, Tag, TagEnd};
use std::borrow::Cow;
use std::ops::Range;

/// The scopes of one span (`Scope` in markdown.zig): which style groups compose its style.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Scope {
    /// A heading's level, 1 to 6, or 0.
    pub heading: u8,
    pub strong: bool,
    pub italic: bool,
    pub strike: bool,
    /// Inside a link, an image or an autolink.
    pub link: bool,
    /// A link's brackets and parentheses.
    pub gap: bool,
    /// A link's URL shown as text.
    pub url: bool,
    /// A code span.
    pub raw: bool,
    /// A backslash escape.
    pub escape: bool,
}

impl Scope {
    /// The style groups of the scope, later ones winning (`scopeSet` in context.zig).
    pub fn groups(&self) -> Vec<&'static str> {
        const HEADINGS: [&str; 6] = ["markup.heading.1", "markup.heading.2", "markup.heading.3", "markup.heading.4", "markup.heading.5", "markup.heading.6"];
        let mut out = Vec::new();
        if self.heading > 0 {
            out.push(HEADINGS[(self.heading.min(6) - 1) as usize]);
        }
        let label = self.link && !self.gap && !self.url;
        for (on, name) in [
            (self.strong, "markup.strong"),
            (self.italic, "markup.italic"),
            (self.strike, "markup.strikethrough"),
            ((self.link || self.gap) && !self.url, "markup.link"),
            (label, "markup.link.label"),
            (self.url, "markup.link.url"),
            (self.raw, "markup.raw"),
            (self.escape, "string.escape"),
        ] {
            if on {
                out.push(name);
            }
        }
        out
    }
}

/// A run of display text with one scope.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub text: String,
    pub scope: Scope,
    /// The hyperlink target, for links, images and autolinks.
    pub link: Option<String>,
    /// A code span (an inline hook leaves it alone unless it is exactly a session ID).
    pub code: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub header: bool,
    pub cells: Vec<Vec<Span>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// A task item's box.
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Paragraph(Vec<Span>),
    /// A setext heading's spans end with "\n" and its underline line.
    Heading { level: u8, spans: Vec<Span> },
    Code { lang: String, text: String },
    Html(String),
    Rule,
    /// The first row is the header; every row has as many cells as the header.
    Table { aligns: Vec<Align>, rows: Vec<Row> },
    Quote(Vec<Block>),
    List { ordered: bool, start: u64, tight: bool, items: Vec<Item> },
}

/// Parse `src`. `conceal` hides the syntax markers (`session.markdown` "rendered"); `streaming` shows the last line
/// only as far as it is decided, as Core's `streaming` does.
pub fn parse(src: &str, conceal: bool, streaming: bool) -> Vec<Block> {
    let src: Cow<str> = if streaming { hold(src) } else { Cow::Borrowed(src) };
    Builder::new(&src, conceal).run()
}

// ---- Streaming ----

/// What the engine shows of a document that is still streaming: a last line that may still become a fence's closer,
/// a setext underline or a table row waits for its end, a link destination waits for its `)`, and an emphasis or a
/// code span that has not closed yet draws as if it had.
pub fn hold(src: &str) -> Cow<'_, str> {
    let (mut text, again) = hold_step(src);
    // A pending line held for a table leaves the paragraph's last complete line last, which may wait too.
    if again {
        text = hold_step(&text).0;
    }
    if text == src { Cow::Borrowed(src) } else { Cow::Owned(text) }
}

/// One pass of [`hold`]; `true` when a line waited for a table and the one above it is to be looked at again.
fn hold_step(src: &str) -> (String, bool) {
    let mut text = src.to_string();
    // The fences the source opens and closes, line by line.
    let mut fence: Option<(char, usize)> = None;
    let mut para_start: Option<usize> = None;
    let mut at = 0;
    let lines: Vec<(usize, &str)> = src.split_inclusive('\n').map(|l| {
        let start = at;
        at += l.len();
        (start, l)
    }).collect();
    for (i, (start, raw)) in lines.iter().enumerate() {
        let last = i + 1 == lines.len();
        let line = raw.trim_end_matches(['\n', '\r']);
        let body = strip_containers(line);
        if let Some((ch, n)) = fence {
            let t = body.trim_start_matches(' ');
            let run = t.chars().take_while(|c| *c == ch).count();
            let closes = run >= n && t[run * ch.len_utf8()..].trim().is_empty() && body.len() - t.len() <= 3;
            if last && !raw.ends_with('\n') {
                // A pending line that may still become the closer waits.
                let trimmed = t.trim_end();
                if !trimmed.is_empty() && trimmed.chars().all(|c| c == ch) {
                    text.truncate(*start);
                    return (text, false);
                }
            }
            if closes {
                fence = None;
            }
            continue;
        }
        let t = body.trim_start_matches(' ');
        if body.len() - t.len() <= 3 && (t.starts_with("```") || t.starts_with("~~~")) {
            let ch = t.chars().next().unwrap();
            let n = t.chars().take_while(|c| *c == ch).count();
            if ch == '~' || !t[n..].contains('`') {
                fence = Some((ch, n));
                para_start = None;
                continue;
            }
        }
        if body.trim().is_empty() {
            para_start = None;
            continue;
        }
        let pending = last && !raw.ends_with('\n');
        // A pending line of `=` or `-` under a paragraph may be its setext underline.
        if pending && para_start.is_some() {
            let u = t.trim_end();
            if body.len() - t.len() <= 3 && !u.is_empty() && (u.chars().all(|c| c == '=') || u.chars().all(|c| c == '-')) {
                text.truncate(*start);
                return (close_inline(text), false);
            }
        }
        if para_start.is_none() {
            para_start = Some(*start);
        }
        // The last line of an open paragraph waits from its first pipe: with the next line it may be a table's header.
        let complete_last = !pending && i + 1 == lines.len();
        let pending_after = i + 2 == lines.len() && lines[i + 1].1.trim().is_empty() && !lines[i + 1].1.ends_with('\n');
        // A paragraph that may still be a link definition (`[label]: …`) is not shown until that is decided.
        if (pending || complete_last) && let Some(p) = para_start
            && src[p..].starts_with('[')
            && src[p..].split('\n').next().is_some_and(|l| l.contains("]:") || !l.contains(']'))
        {
            text.truncate(p);
            return (text, false);
        }
        // A complete delimiter row under a row of cells makes a table; a pending one waits like any line.
        let header = !pending && is_delimiter_row(line) && i > 0 && lines[i - 1].1.contains('|');
        if (pending || complete_last || pending_after) && line.len() <= HELD_LINE_MAX && !header && !is_table_row(&lines, i) {
            if let Some(p) = line.find('|') {
                text.truncate(start + p);
                // The line above may wait too, now that it is the last.
                return (text, true);
            }
        }
    }
    if fence.is_some() {
        return (text, false);
    }
    (close_inline(text), false)
}

/// `held_line_byte_count_max`.
const HELD_LINE_MAX: usize = 4096;

/// Whether line `i` is a row of a table that started above it (a header and its delimiter row).
fn is_table_row(lines: &[(usize, &str)], i: usize) -> bool {
    let mut j = i;
    while j > 0 {
        let prev = lines[j - 1].1.trim();
        if prev.is_empty() || !prev.contains('|') {
            return false;
        }
        if is_delimiter_row(prev) {
            return true;
        }
        j -= 1;
    }
    false
}

fn is_delimiter_row(line: &str) -> bool {
    let t = line.trim().trim_matches('|');
    !t.is_empty() && t.split('|').all(|c| {
        let c = c.trim();
        let c = c.strip_prefix(':').unwrap_or(c);
        let c = c.strip_suffix(':').unwrap_or(c);
        !c.is_empty() && c.chars().all(|ch| ch == '-')
    })
}

/// A line without its block quote markers and list indentation (approximately, for the streaming holds).
fn strip_containers(line: &str) -> &str {
    let mut s = line;
    loop {
        let t = s.trim_start_matches(' ');
        if let Some(rest) = t.strip_prefix('>') {
            s = rest.strip_prefix(' ').unwrap_or(rest);
            continue;
        }
        return s;
    }
}

/// The last paragraph's unfinished inline syntax: a link destination that has not closed waits, and an open code
/// span or emphasis closes, so it draws as if it had.
fn close_inline(mut text: String) -> String {
    let start = text.rfind("\n\n").map(|p| p + 2).unwrap_or(0);
    // A link destination still streaming: show the label as text, without `](…`.
    if let Some(p) = text[start..].rfind("](") {
        let rest = &text[start + p + 2..];
        if !rest.contains(')') && !rest.contains('\n') {
            text.truncate(start + p);
        }
    }
    let para = &text[start..];
    // Code spans: an opener whose closer has not come yet.
    let bytes = para.as_bytes();
    let mut i = 0;
    let mut open_code: Option<usize> = None;
    let mut plain = String::new();
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            plain.push_str(&para[i..i + 2.min(para.len() - i)]);
            i += 2;
            continue;
        }
        if bytes[i] == b'`' {
            let n = bytes[i..].iter().take_while(|b| **b == b'`').count();
            match open_code {
                Some(m) if m == n => open_code = None,
                Some(_) => {}
                None => open_code = Some(n),
            }
            plain.push_str(&" ".repeat(n));
            i += n;
            continue;
        }
        let c = para[i..].chars().next().unwrap();
        if open_code.is_some() {
            plain.push(' ');
            for _ in 1..c.len_utf8() {
                plain.push(' ');
            }
        } else {
            plain.push(c);
        }
        i += c.len_utf8();
    }
    if let Some(n) = open_code {
        let trimmed = text.trim_end_matches([' ', '\t']).len();
        if trimmed > start && !text.ends_with('`') {
            text.truncate(trimmed);
            text.push_str(&"`".repeat(n));
        }
        return text;
    }
    // Emphasis runs that opened (left-flanking) and did not close.
    let mut stack: Vec<(char, usize)> = Vec::new();
    let chars: Vec<char> = plain.chars().collect();
    let mut k = 0;
    while k < chars.len() {
        let c = chars[k];
        if c == '\\' {
            k += 2;
            continue;
        }
        if c == '*' || c == '_' || c == '~' {
            let n = chars[k..].iter().take_while(|x| **x == c).count();
            let before = if k == 0 { ' ' } else { chars[k - 1] };
            let after = chars.get(k + n).copied().unwrap_or(' ');
            let left = !after.is_whitespace() && (!after.is_ascii_punctuation() || before.is_whitespace() || before.is_ascii_punctuation());
            let right = !before.is_whitespace() && (!before.is_ascii_punctuation() || after.is_whitespace() || after.is_ascii_punctuation());
            let intraword = c == '_' && before.is_alphanumeric() && after.is_alphanumeric();
            let valid = !(c == '~' && n > 2) && !intraword;
            if valid {
                if right && let Some(pos) = stack.iter().rposition(|(sc, sn)| *sc == c && (*sn == n || c != '~')) {
                    stack.truncate(pos);
                } else if left {
                    stack.push((c, n));
                }
            }
            k += n;
            continue;
        }
        k += 1;
    }
    if stack.is_empty() {
        return text;
    }
    let trimmed = text.trim_end_matches([' ', '\t']).len();
    let tail = &text[..trimmed];
    // A run at the very end has nothing to mark yet: it stays as it is.
    if tail.ends_with(['*', '_', '~']) {
        return text;
    }
    text.truncate(trimmed);
    for (c, n) in stack.iter().rev() {
        for _ in 0..*n {
            text.push(*c);
        }
    }
    text
}

// ---- Parsing ----

#[derive(Default)]
struct Inline {
    spans: Vec<Span>,
    strong: i32,
    italic: i32,
    strike: i32,
    heading: u8,
    /// The outermost link or image: its target, whether it is an image or an autolink, where its label starts.
    link: Option<LinkState>,
    /// Source mode: how far the source is shown.
    shown: usize,
    /// Source mode: the last thing shown was a line break, so the next gap starts with container markers.
    after_break: bool,
}

struct LinkState {
    url: String,
    image: bool,
    auto: bool,
    label_start: usize,
    depth: usize,
}

enum LeafKind {
    Paragraph,
    Heading { level: u8, setext_underline: Option<String> },
    Cell,
}

struct Leaf {
    kind: LeafKind,
    inline: Inline,
}

enum Frame {
    Root(Vec<Block>),
    Quote(Vec<Block>),
    List { ordered: bool, start: u64, items: Vec<Item>, loose: bool },
    /// `para`: pulldown-cmark wrapped the item's text in a paragraph, which it does in loose lists only.
    Item { task: Option<bool>, blocks: Vec<Block>, para: bool },
    Table { aligns: Vec<Align>, rows: Vec<Row>, row: Option<Row> },
    Code { lang: String, text: String },
    Html(String),
}

struct Builder<'a> {
    src: &'a str,
    conceal: bool,
    stack: Vec<Frame>,
    leaf: Option<Leaf>,
    /// Image nesting inside an outer link (only the outermost owns the target).
    link_depth: usize,
    /// For each open quote, list and item: whether it has a frame (containers nested deeper than `DEPTH_MAX` add none).
    opened: Vec<bool>,
}

/// `depth_max`: how deep containers nest; deeper ones lay their blocks out in the one around them.
const DEPTH_MAX: usize = 32;

impl<'a> Builder<'a> {
    fn new(src: &'a str, conceal: bool) -> Self {
        Builder { src, conceal, stack: vec![Frame::Root(vec![])], leaf: None, link_depth: 0, opened: vec![] }
    }

    fn run(mut self) -> Vec<Block> {
        let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
        let src = self.src;
        for (ev, range) in Parser::new_ext(src, opts).into_offset_iter() {
            self.event(ev, range);
        }
        self.flush_leaf();
        while self.stack.len() > 1 {
            self.close_frame();
        }
        match self.stack.pop() {
            Some(Frame::Root(b)) => b,
            _ => vec![],
        }
    }

    fn push_block(&mut self, b: Block) {
        match self.stack.last_mut() {
            Some(Frame::Root(v)) | Some(Frame::Quote(v)) | Some(Frame::Item { blocks: v, .. }) => v.push(b),
            _ => {}
        }
    }

    fn close_frame(&mut self) {
        self.flush_leaf();
        let Some(f) = self.stack.pop() else { return };
        let block = match f {
            Frame::Root(_) => return,
            Frame::Quote(v) => Block::Quote(v),
            Frame::List { ordered, start, items, loose } => Block::List { ordered, start, tight: !loose, items },
            Frame::Item { task, blocks, para } => {
                if let Some(Frame::List { items, loose, .. }) = self.stack.last_mut() {
                    items.push(Item { task, blocks });
                    *loose |= para;
                }
                return;
            }
            Frame::Table { aligns, mut rows, row } => {
                if let Some(r) = row {
                    rows.push(r);
                }
                let n = aligns.len();
                for r in &mut rows {
                    r.cells.resize(n, vec![]);
                }
                Block::Table { aligns, rows }
            }
            Frame::Code { lang, text } => Block::Code { lang, text: text.strip_suffix('\n').unwrap_or(&text).to_string() },
            Frame::Html(t) => Block::Html(t.trim_end_matches('\n').to_string()),
        };
        self.push_block(block);
    }

    fn flush_leaf(&mut self) {
        let Some(mut leaf) = self.leaf.take() else { return };
        let spans = std::mem::take(&mut leaf.inline.spans);
        match leaf.kind {
            LeafKind::Paragraph => {
                if !spans.is_empty() {
                    self.push_block(Block::Paragraph(spans));
                }
            }
            LeafKind::Heading { level, setext_underline } => {
                let mut spans = spans;
                if let Some(u) = setext_underline {
                    push_span(&mut spans, Span { text: format!("\n{u}"), scope: Scope { heading: level, ..Scope::default() }, link: None, code: false });
                }
                self.push_block(Block::Heading { level, spans });
            }
            LeafKind::Cell => {
                if let Some(Frame::Table { row: Some(r), .. }) = self.stack.last_mut() {
                    r.cells.push(spans);
                }
            }
        }
    }

    /// The leaf inline events go to: an item's text in a tight list is a paragraph of its own.
    fn leaf(&mut self, at: usize) -> &mut Leaf {
        if self.leaf.is_none() {
            self.leaf = Some(Leaf { kind: LeafKind::Paragraph, inline: Inline { shown: at, ..Inline::default() } });
        }
        self.leaf.as_mut().unwrap()
    }

    fn event(&mut self, ev: Event<'a>, r: Range<usize>) {
        // Inside a code or HTML block every event is its text.
        if let Some(Frame::Code { text, .. }) = self.stack.last_mut() {
            match ev {
                Event::Text(t) => text.push_str(&t),
                Event::End(TagEnd::CodeBlock) => self.close_frame(),
                _ => {}
            }
            return;
        }
        if let Some(Frame::Html(text)) = self.stack.last_mut() {
            match ev {
                Event::Html(t) | Event::Text(t) => text.push_str(&t),
                Event::End(TagEnd::HtmlBlock) => self.close_frame(),
                _ => {}
            }
            return;
        }
        match ev {
            Event::Start(tag) => self.start(tag, r),
            Event::End(tag) => self.end(tag, r),
            Event::Text(t) => self.text(&t, r),
            Event::Code(t) => {
                let raw = if self.conceal { t.to_string() } else { self.src[r.clone()].to_string() };
                self.gap(r.start);
                let leaf = self.leaf(r.start);
                let scope = Scope { raw: true, ..scope_of(&leaf.inline) };
                let link = leaf.inline.link.as_ref().map(|l| l.url.clone());
                push_span(&mut leaf.inline.spans, Span { text: raw, scope, link, code: true });
                self.shown(r.end);
            }
            Event::InlineHtml(t) | Event::Html(t) => {
                self.gap(r.start);
                self.plain(&t, r.end);
            }
            Event::SoftBreak => {
                self.gap(r.start);
                self.plain("\n", r.end);
                self.leaf(r.end).inline.after_break = true;
            }
            Event::HardBreak => {
                self.gap(r.start);
                if !self.conceal && self.src[r.clone()].starts_with('\\') {
                    self.plain("\\", r.start + 1);
                }
                self.plain("\n", r.end);
                self.leaf(r.end).inline.after_break = true;
            }
            Event::Rule => {
                self.flush_leaf();
                self.push_block(Block::Rule);
            }
            Event::TaskListMarker(done) => {
                if let Some(Frame::Item { task, .. }) = self.stack.last_mut() {
                    *task = Some(done);
                }
                // The box and the space after it are the item's marker, not its text.
                let after = (r.end + 1).min(self.src.len());
                self.leaf(after).inline.shown = after;
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'a>, r: Range<usize>) {
        match tag {
            Tag::Paragraph => {
                self.flush_leaf();
                if let Some(Frame::Item { para, .. }) = self.stack.last_mut() {
                    *para = true;
                }
                self.leaf = Some(Leaf { kind: LeafKind::Paragraph, inline: Inline { shown: r.start, ..Inline::default() } });
            }
            Tag::Heading { level, .. } => {
                self.flush_leaf();
                let level = level as u8;
                let source = &self.src[r.clone()];
                let atx = source.trim_start_matches([' ', '\t']).starts_with('#');
                let setext_underline = (!atx).then(|| {
                    let body = source.trim_end_matches(['\n', '\r']);
                    let last = body.rsplit('\n').next().unwrap_or("").trim_matches([' ', '\t']);
                    let at = last.find(['=', '-']).unwrap_or(0);
                    last[at..].to_string()
                });
                let mut inline = Inline { shown: r.start, heading: level, ..Inline::default() };
                if atx {
                    // The content starts after the `#` marker.
                    let t = source.trim_start_matches([' ', '\t']);
                    let hashes = t.chars().take_while(|c| *c == '#').count();
                    let after = &t[hashes..];
                    let spaces = after.len() - after.trim_start_matches([' ', '\t']).len();
                    inline.shown = r.start + (source.len() - t.len()) + hashes + spaces;
                    if !self.conceal {
                        push_span(&mut inline.spans, Span { text: format!("{} ", "#".repeat(level as usize)), scope: Scope { heading: level, ..Scope::default() }, link: None, code: false });
                    }
                }
                self.leaf = Some(Leaf { kind: LeafKind::Heading { level, setext_underline }, inline });
            }
            Tag::BlockQuote(_) => {
                self.flush_leaf();
                self.open(Frame::Quote(vec![]));
            }
            Tag::CodeBlock(kind) => {
                self.flush_leaf();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or("").to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.stack.push(Frame::Code { lang, text: String::new() });
            }
            Tag::HtmlBlock => {
                self.flush_leaf();
                self.stack.push(Frame::Html(String::new()));
            }
            Tag::List(start) => {
                self.flush_leaf();
                self.open(Frame::List { ordered: start.is_some(), start: start.unwrap_or(1), items: vec![], loose: false });
            }
            Tag::Item => {
                self.flush_leaf();
                self.open(Frame::Item { task: None, blocks: vec![], para: false });
            }
            Tag::Table(aligns) => {
                self.flush_leaf();
                let aligns = aligns
                    .into_iter()
                    .map(|a| match a {
                        Alignment::Right => Align::Right,
                        Alignment::Center => Align::Center,
                        _ => Align::Left,
                    })
                    .collect();
                self.stack.push(Frame::Table { aligns, rows: vec![], row: None });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(Frame::Table { row, .. }) = self.stack.last_mut() {
                    *row = Some(Row { header: matches!(tag, Tag::TableHead), cells: vec![] });
                }
            }
            Tag::TableCell => {
                self.leaf = Some(Leaf { kind: LeafKind::Cell, inline: Inline { shown: r.start, ..Inline::default() } });
                // A cell's source starts with its padding.
                let cell = &self.src[r.clone()];
                let lead = cell.len() - cell.trim_start().len();
                self.leaf.as_mut().unwrap().inline.shown = r.start + lead;
            }
            Tag::Emphasis | Tag::Strong | Tag::Strikethrough => {
                self.gap(r.start);
                let l = &mut self.leaf(r.start).inline;
                match tag {
                    Tag::Emphasis => l.italic += 1,
                    Tag::Strong => l.strong += 1,
                    _ => l.strike += 1,
                }
            }
            Tag::Link { link_type, dest_url, .. } => {
                self.gap(r.start);
                self.link_depth += 1;
                let auto = matches!(link_type, LinkType::Autolink | LinkType::Email);
                let url = if matches!(link_type, LinkType::Email) && !dest_url.starts_with("mailto:") { format!("mailto:{dest_url}") } else { dest_url.to_string() };
                self.open_link(url, false, auto, r.start);
            }
            Tag::Image { dest_url, .. } => {
                self.gap(r.start);
                self.link_depth += 1;
                self.open_link(dest_url.to_string(), true, false, r.start);
            }
            _ => {}
        }
    }

    /// Opens a quote, list or item frame, unless containers already nest `DEPTH_MAX` deep.
    fn open(&mut self, frame: Frame) {
        let depth = self.opened.iter().filter(|o| **o).count();
        let open = depth < DEPTH_MAX;
        self.opened.push(open);
        if open {
            self.stack.push(frame);
        }
    }

    fn open_link(&mut self, url: String, image: bool, auto: bool, at: usize) {
        let conceal = self.conceal;
        let src = self.src;
        let depth = self.link_depth;
        let leaf = self.leaf(at);
        let label_start = leaf.inline.spans.iter().map(|s| s.text.len()).sum();
        if leaf.inline.link.is_none() {
            leaf.inline.link = Some(LinkState { url, image, auto, label_start, depth });
        }
        if !conceal {
            // The opening `[`, `![` or `<` in the gap style.
            let open = if src[at..].starts_with("![") {
                "!["
            } else if src[at..].starts_with('[') {
                "["
            } else if src[at..].starts_with('<') {
                "<"
            } else {
                ""
            };
            if !open.is_empty() {
                let scope = Scope { gap: true, ..scope_of(&leaf.inline) };
                let link = leaf.inline.link.as_ref().map(|l| l.url.clone());
                push_span(&mut leaf.inline.spans, Span { text: open.to_string(), scope, link, code: false });
                leaf.inline.shown = at + open.len();
            }
        }
    }

    fn end(&mut self, tag: TagEnd, r: Range<usize>) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) => self.flush_leaf(),
            TagEnd::TableCell => {
                self.flush_leaf();
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                self.flush_leaf();
                if let Some(Frame::Table { rows, row, .. }) = self.stack.last_mut()
                    && let Some(r) = row.take()
                {
                    rows.push(r);
                }
            }
            TagEnd::Table => self.close_frame(),
            TagEnd::BlockQuote(_) | TagEnd::List(_) | TagEnd::Item => {
                if self.opened.pop().unwrap_or(true) {
                    self.close_frame();
                } else {
                    self.flush_leaf();
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.gap(r.end);
                if let Some(l) = self.leaf.as_mut() {
                    match tag {
                        TagEnd::Emphasis => l.inline.italic -= 1,
                        TagEnd::Strong => l.inline.strong -= 1,
                        _ => l.inline.strike -= 1,
                    }
                }
            }
            TagEnd::Link | TagEnd::Image => {
                self.close_link(r);
                self.link_depth = self.link_depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    fn close_link(&mut self, r: Range<usize>) {
        let conceal = self.conceal;
        let src = self.src;
        let depth = self.link_depth;
        let Some(leaf) = self.leaf.as_mut() else { return };
        let owner = leaf.inline.link.as_ref().is_some_and(|l| l.depth == depth);
        if !conceal {
            // `](url)`, `][ref]`, `]` or `>`.
            let shown = leaf.inline.shown.min(r.end);
            let rest = &src[shown..r.end];
            let base = scope_of(&leaf.inline);
            let link = leaf.inline.link.as_ref().map(|l| l.url.clone());
            let mut put = |text: &str, scope: Scope, link: Option<String>| push_span(&mut leaf.inline.spans, Span { text: text.to_string(), scope, link, code: false });
            if rest.starts_with("](") && rest.ends_with(')') && rest.len() >= 3 {
                put("]", Scope { link: false, ..base }, None);
                put("(", Scope { gap: true, ..base }, link.clone());
                put(&rest[2..rest.len() - 1], Scope { url: true, ..base }, link.clone());
                put(")", Scope { gap: true, ..base }, link);
            } else if let Some(tail) = rest.strip_prefix(']') {
                put("]", Scope { link: false, ..base }, None);
                if !tail.is_empty() {
                    put(tail, Scope { gap: true, ..base }, link);
                }
            } else if !rest.is_empty() {
                put(rest, Scope { gap: true, ..base }, link);
            }
            leaf.inline.shown = r.end;
        }
        if !owner {
            return;
        }
        let state = leaf.inline.link.take().unwrap();
        if conceal && !state.image && !state.auto {
            let label: String = leaf.inline.spans.iter().map(|s| s.text.as_str()).collect::<String>();
            let label = label.get(state.label_start..).unwrap_or("");
            if label != state.url {
                let base = scope_of(&leaf.inline);
                let spans = &mut leaf.inline.spans;
                push_span(spans, Span { text: " ".into(), scope: base, link: None, code: false });
                push_span(spans, Span { text: "(".into(), scope: Scope { gap: true, ..base }, link: Some(state.url.clone()), code: false });
                push_span(spans, Span { text: state.url.clone(), scope: Scope { url: true, ..base }, link: Some(state.url.clone()), code: false });
                push_span(spans, Span { text: ")".into(), scope: Scope { gap: true, ..base }, link: Some(state.url), code: false });
            }
        }
        leaf.inline.shown = r.end;
    }

    // ---- Source mode ----

    /// Source mode: shows the source before `to` that no event showed (markers), in the current scope.
    fn gap(&mut self, to: usize) {
        if self.conceal {
            return;
        }
        let src = self.src;
        let Some(leaf) = self.leaf.as_mut() else { return };
        let from = leaf.inline.shown;
        if to <= from {
            return;
        }
        let mut text = &src[from..to];
        if leaf.inline.after_break {
            text = strip_line_start(text);
        }
        if !text.is_empty() {
            leaf.inline.after_break = false;
            let scope = scope_of(&leaf.inline);
            let link = leaf.inline.link.as_ref().map(|l| l.url.clone());
            let gap = leaf.inline.link.is_some();
            push_span(&mut leaf.inline.spans, Span { text: text.to_string(), scope: Scope { gap: gap || scope.gap, ..scope }, link, code: false });
        }
        leaf.inline.shown = to;
    }

    fn shown(&mut self, to: usize) {
        if let Some(l) = self.leaf.as_mut() {
            l.inline.shown = to;
            l.inline.after_break = false;
        }
    }

    fn plain(&mut self, text: &str, to: usize) {
        let leaf = self.leaf(to);
        let scope = scope_of(&leaf.inline);
        let link = leaf.inline.link.as_ref().map(|l| l.url.clone());
        push_span(&mut leaf.inline.spans, Span { text: text.to_string(), scope, link, code: false });
        leaf.inline.shown = to;
        leaf.inline.after_break = false;
    }

    fn text(&mut self, t: &str, r: Range<usize>) {
        let src = self.src;
        let conceal = self.conceal;
        // A backslash escape: pulldown-cmark starts the text at the escaped character.
        let escaped = r.start > 0 && src.as_bytes()[r.start - 1] == b'\\' && t.chars().next().is_some_and(|c| c.is_ascii_punctuation()) && src[r.clone()].starts_with(t.chars().next().unwrap());
        // A table cell's `\|` is unescaped before its content is parsed: a plain `|`, in source mode too.
        let cell_pipe = escaped && t.starts_with('|') && matches!(self.leaf.as_ref().map(|l| &l.kind), Some(LeafKind::Cell));
        let escaped = escaped && !cell_pipe;
        let gap_to = if (escaped || cell_pipe) && !conceal { r.start - 1 } else { r.start };
        self.gap(gap_to);
        let leaf = self.leaf(r.start);
        if cell_pipe {
            leaf.inline.shown = r.start;
        }
        let base = scope_of(&leaf.inline);
        let link = leaf.inline.link.as_ref().map(|l| l.url.clone());
        let mut rest = if conceal { t.to_string() } else { src[r.clone()].to_string() };
        if escaped {
            let c = rest.chars().next().unwrap();
            let shown = if conceal { c.to_string() } else { format!("\\{c}") };
            push_span(&mut leaf.inline.spans, Span { text: shown, scope: Scope { escape: true, ..base }, link: link.clone(), code: false });
            rest = rest[c.len_utf8()..].to_string();
        }
        if leaf.inline.link.is_some() {
            push_span(&mut leaf.inline.spans, Span { text: rest, scope: base, link, code: false });
        } else {
            // GFM extended autolinks: bare URLs, `www.` hosts and emails.
            let mut at = 0;
            for (a, b, target) in autolinks(&rest) {
                if a > at {
                    push_span(&mut leaf.inline.spans, Span { text: rest[at..a].to_string(), scope: base, link: None, code: false });
                }
                push_span(&mut leaf.inline.spans, Span { text: rest[a..b].to_string(), scope: Scope { link: true, ..base }, link: Some(target), code: false });
                at = b;
            }
            if at < rest.len() {
                push_span(&mut leaf.inline.spans, Span { text: rest[at..].to_string(), scope: base, link: None, code: false });
            }
        }
        leaf.inline.shown = r.end;
        leaf.inline.after_break = false;
    }
}

/// A continuation line's start without its container markers (block quote `>` and indentation).
fn strip_line_start(text: &str) -> &str {
    let mut s = text.trim_start_matches([' ', '\t']);
    while let Some(rest) = s.strip_prefix('>') {
        s = rest.trim_start_matches([' ', '\t']);
    }
    s
}

fn scope_of(i: &Inline) -> Scope {
    Scope { heading: i.heading, strong: i.strong > 0, italic: i.italic > 0, strike: i.strike > 0, link: i.link.is_some(), ..Scope::default() }
}

/// Appends a span, merging it into the last one when they look the same.
fn push_span(spans: &mut Vec<Span>, s: Span) {
    if s.text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.scope == s.scope
        && last.link == s.link
        && !last.code
        && !s.code
    {
        last.text.push_str(&s.text);
        return;
    }
    spans.push(s);
}

/// GFM extended autolinks in plain text (byte ranges and targets): `http(s)://` URLs, `www.` hosts (`http://` added)
/// and email addresses (`mailto:` added). A link ends at whitespace or `<`; trailing `?!.,:*_~` and an unbalanced `)`
/// are not part of it.
pub fn autolinks(text: &str) -> Vec<(usize, usize, String)> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?i)(?:https?://|www\.)[^\s<]+|[A-Za-z0-9._+-]+@[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)+"#).unwrap()
    });
    let mut out = Vec::new();
    for m in RE.find_iter(text) {
        let before = text[..m.start()].chars().next_back();
        // An extended autolink starts a word (after whitespace or `*`, `_`, `~`, `(`).
        if before.is_some_and(|c| !(c.is_whitespace() || "*_~(\"'".contains(c))) {
            continue;
        }
        let mut s = m.as_str();
        let email = !s.to_ascii_lowercase().starts_with("http") && !s.to_ascii_lowercase().starts_with("www.");
        if email {
            if s.ends_with(['-', '_']) {
                continue;
            }
            let s = s.trim_end_matches('.');
            out.push((m.start(), m.start() + s.len(), format!("mailto:{s}")));
            continue;
        }
        loop {
            let Some(last) = s.chars().last() else { break };
            if "?!.,:*_~'\"".contains(last) {
                s = &s[..s.len() - 1];
                continue;
            }
            if last == ')' && s.matches(')').count() > s.matches('(').count() {
                s = &s[..s.len() - 1];
                continue;
            }
            if last == ';' {
                // An entity-like `&name;` at the end is not part of it.
                if let Some(amp) = s.rfind('&')
                    && s[amp + 1..s.len() - 1].chars().all(|c| c.is_ascii_alphanumeric())
                {
                    s = &s[..amp];
                    continue;
                }
            }
            break;
        }
        if s.to_ascii_lowercase().starts_with("www.") && s.len() <= 4 {
            continue;
        }
        if s.ends_with("://") {
            continue;
        }
        let target = if s.to_ascii_lowercase().starts_with("www.") { format!("http://{s}") } else { s.to_string() };
        out.push((m.start(), m.start() + s.len(), target));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn holds_undecided_streaming_lines() {
        assert_eq!(hold("Text **bold part"), "Text **bold part**");
        assert_eq!(hold("Run `cmd --fl"), "Run `cmd --fl`");
        assert_eq!(hold("Intro line\n\n| a | b"), "Intro line\n\n");
        assert_eq!(hold("Start\n\n```py\nx = 1\n``"), "Start\n\n```py\nx = 1\n");
        assert_eq!(hold("Start\n\n```py\nx = 1\ny"), "Start\n\n```py\nx = 1\ny");
        assert_eq!(hold("Title\n=="), "Title\n");
        assert_eq!(hold("See [the docs](https://exa"), "See [the docs");
        assert_eq!(hold("Items:\n\n- one\n- tw"), "Items:\n\n- one\n- tw");
        assert_eq!(hold("a | b\n--- | ---\n1 | 2"), "a | b\n--- | ---\n1 | 2");
        assert_eq!(hold("snake_case and *a"), "snake_case and *a*");
    }

    #[test]
    fn autolinks_like_gfm() {
        let t = "see https://bare.url/path. and www.example.com, a@b.co (https://x.y/a_(b))";
        let found: Vec<(&str, String)> = autolinks(t).into_iter().map(|(a, b, u)| (&t[a..b], u)).collect();
        assert_eq!(
            found,
            [
                ("https://bare.url/path", "https://bare.url/path".to_string()),
                ("www.example.com", "http://www.example.com".to_string()),
                ("a@b.co", "mailto:a@b.co".to_string()),
                ("https://x.y/a_(b)", "https://x.y/a_(b)".to_string()),
            ]
        );
    }

    #[test]
    fn setext_headings_keep_their_underline() {
        let doc = parse("Intro\nTitle\n===", true, false);
        let Block::Heading { level, spans } = &doc[0] else { panic!("{doc:?}") };
        assert_eq!(*level, 1);
        assert_eq!(texts(spans), "Intro\nTitle\n===");
    }

    #[test]
    fn source_mode_shows_markers() {
        let doc = parse("Some **bold** and *em* and `code` and [link](https://x.y) and <https://a.b> and \\* esc.", false, false);
        let Block::Paragraph(spans) = &doc[0] else { panic!() };
        assert_eq!(texts(spans), "Some **bold** and *em* and `code` and [link](https://x.y) and <https://a.b> and \\* esc.");
    }
}
