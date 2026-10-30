//! Highlight ranges to styled chunks (`lib/tree-sitter-styled-text.ts` `treeSitterToTextChunks`), an
//! emulation of the tree-sitter `markdown` + `markdown_inline` captures OpenTUI's markdown queries make
//! (including conceals), and code highlighting for the filetypes OpenTUI ships parsers for.

use super::cell::{Color, StyledChunk};
use super::theme::{MiniSyntax, StyleDef};
use pulldown_cmark::{Event, LinkType, Options, Parser, Tag};

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Meta {
    pub conceal: Option<String>,
    pub conceal_lines: bool,
    pub is_injection: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Highlight {
    pub start: usize,
    pub end: usize,
    pub group: String,
    pub meta: Meta,
}

fn hl(start: usize, end: usize, group: &str) -> Highlight {
    Highlight { start, end, group: group.into(), meta: Meta::default() }
}

fn conceal(start: usize, end: usize, group: &str, with: &str, injection: bool) -> Highlight {
    Highlight { start, end, group: group.into(), meta: Meta { conceal: Some(with.into()), conceal_lines: false, is_injection: injection } }
}

fn chunk_of(text: &str, style: Option<&StyleDef>, fallback_fg: Color) -> StyledChunk {
    StyledChunk {
        text: text.to_string(),
        fg: style.and_then(|s| s.fg).unwrap_or(fallback_fg),
        bg: style.and_then(|s| s.bg).unwrap_or(Color::TRANSPARENT),
        attrs: style.map(|s| s.attrs()).unwrap_or(0),
    }
}

fn specificity(group: &str) -> usize {
    group.split('.').count()
}

/// `treeSitterToTextChunks(content, highlights, syntaxStyle, {enabled, baseHighlight})`.
/// `fg` is the renderable's default foreground for chunks without a style color.
pub fn to_chunks(content: &str, highlights: &[Highlight], syntax: &MiniSyntax, conceal_enabled: bool, base_highlight: Option<&str>, fg: Color) -> Vec<StyledChunk> {
    to_chunks_ranged(content, highlights, syntax, conceal_enabled, base_highlight, fg).0
}

/// [`to_chunks`] plus each chunk's source byte range (`sourceRanges`; a conceal replacement covers the text it hides).
pub fn to_chunks_ranged(
    content: &str,
    highlights: &[Highlight],
    syntax: &MiniSyntax,
    conceal_enabled: bool,
    base_highlight: Option<&str>,
    fg: Color,
) -> (Vec<StyledChunk>, Vec<(usize, usize)>) {
    let mut chunks = Vec::new();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let default = syntax.default_style().copied();
    let base = base_highlight.and_then(|b| syntax.get(b)).copied();
    #[derive(Clone, Copy)]
    struct B {
        offset: usize,
        start: bool,
        idx: usize,
    }
    let mut bounds: Vec<B> = Vec::new();
    for (i, h) in highlights.iter().enumerate() {
        if h.start == h.end {
            continue;
        }
        bounds.push(B { offset: h.start, start: true, idx: i });
        bounds.push(B { offset: h.end, start: false, idx: i });
    }
    bounds.sort_by(|a, b| a.offset.cmp(&b.offset).then_with(|| match (a.start, b.start) {
        (false, true) => std::cmp::Ordering::Less,
        (true, false) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    }));
    let mut active: Vec<usize> = Vec::new();
    let mut current = 0usize;
    let slice = |a: usize, b: usize| content.get(a..b).unwrap_or("");
    let mut i = 0;
    while i < bounds.len() {
        let bd = bounds[i];
        if current < bd.offset && !active.is_empty() {
            let text = slice(current, bd.offset);
            let concealer = if conceal_enabled { active.iter().map(|i| &highlights[*i]).find(|h| h.meta.conceal.is_some() || h.group == "conceal" || h.group.starts_with("conceal.")) } else { None };
            if let Some(c) = concealer {
                let replacement = match &c.meta.conceal {
                    Some(r) => r.clone(),
                    None if c.group == "conceal.with.space" => " ".into(),
                    None => String::new(),
                };
                if !replacement.is_empty() {
                    chunks.push(chunk_of(&replacement, default.as_ref(), fg));
                    ranges.push((current, bd.offset));
                }
            } else {
                let mut groups: Vec<(usize, &Highlight)> = active.iter().map(|i| (*i, &highlights[*i])).collect();
                groups.sort_by(|a, b| specificity(&a.1.group).cmp(&specificity(&b.1.group)).then(a.0.cmp(&b.0)));
                let mut merged: StyleDef = base.unwrap_or_default();
                let mut any = base.is_some();
                for (_, g) in groups {
                    if let Some(s) = syntax.resolve(&g.group) {
                        any = true;
                        if s.fg.is_some() {
                            merged.fg = s.fg;
                        }
                        if s.bg.is_some() {
                            merged.bg = s.bg;
                        }
                        if s.bold.is_some() {
                            merged.bold = s.bold;
                        }
                        if s.italic.is_some() {
                            merged.italic = s.italic;
                        }
                        if s.underline.is_some() {
                            merged.underline = s.underline;
                        }
                        if s.dim.is_some() {
                            merged.dim = s.dim;
                        }
                    }
                }
                let style = if any { Some(merged) } else { default };
                chunks.push(chunk_of(text, style.as_ref(), fg));
                ranges.push((current, bd.offset));
            }
        } else if current < bd.offset {
            let style = base.or(default);
            chunks.push(chunk_of(slice(current, bd.offset), style.as_ref(), fg));
            ranges.push((current, bd.offset));
        }
        if bd.start {
            active.push(bd.idx);
        } else {
            active.retain(|x| *x != bd.idx);
            if conceal_enabled {
                let h = &highlights[bd.idx];
                let next = content.as_bytes().get(bd.offset).copied();
                if h.meta.conceal_lines && next == Some(b'\n') {
                    current = bd.offset + 1;
                    i += 1;
                    continue;
                }
                if let Some(c) = &h.meta.conceal {
                    if c == " " {
                        if next == Some(b' ') {
                            current = bd.offset + 1;
                            i += 1;
                            continue;
                        }
                    } else if c.is_empty() && h.group == "conceal" && !h.meta.is_injection && next == Some(b' ') {
                        current = bd.offset + 1;
                        i += 1;
                        continue;
                    }
                }
            }
        }
        current = bd.offset;
        i += 1;
    }
    if current < content.len() {
        let style = base.or(default);
        chunks.push(chunk_of(&content[current..], style.as_ref(), fg));
        ranges.push((current, content.len()));
    }
    (chunks, ranges)
}

/// Byte offset of every line start.
fn line_starts(s: &str) -> Vec<usize> {
    let mut v = vec![0];
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}

/// Highlights for a markdown block rendered as `CodeRenderable(filetype: "markdown")`.
pub fn markdown_highlights(content: &str) -> Vec<Highlight> {
    let mut out: Vec<Highlight> = Vec::new();
    // Block level: ATX headings (whole heading + concealed marker) and the inline ranges.
    let mut inline_ranges: Vec<(usize, usize)> = Vec::new();
    let starts = line_starts(content);
    let mut in_fence: Option<(char, usize)> = None;
    for (li, &ls) in starts.iter().enumerate() {
        let le = starts.get(li + 1).map(|n| n - 1).unwrap_or(content.len());
        let line = &content[ls..le];
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        if let Some((ch, n)) = in_fence {
            let run = trimmed.chars().take_while(|c| *c == ch).count();
            if indent <= 3 && run >= n && trimmed[run..].trim().is_empty() {
                in_fence = None;
            }
            continue;
        }
        if indent <= 3 && (trimmed.starts_with("```") || trimmed.starts_with("~~~")) {
            let ch = trimmed.chars().next().unwrap();
            in_fence = Some((ch, trimmed.chars().take_while(|c| *c == ch).count()));
            continue;
        }
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if indent <= 3 && (1..=6).contains(&hashes) && (trimmed.len() == hashes || trimmed[hashes..].starts_with([' ', '\t'])) {
            let end = if li + 1 < starts.len() { le + 1 } else { le };
            out.push(Highlight { start: ls, end, group: format!("markup.heading.{hashes}"), meta: Meta::default() });
            out.push(conceal(ls + indent, ls + indent + hashes, "conceal", "", false));
            let content_start = ls + indent + hashes + trimmed[hashes..].len() - trimmed[hashes..].trim_start().len();
            if content_start < le {
                inline_ranges.push((content_start, le));
            }
            continue;
        }
        if !trimmed.is_empty() {
            inline_ranges.push((ls + indent, le));
        }
    }
    // Merge consecutive paragraph lines into one inline range (one `inline` node per paragraph).
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in inline_ranges {
        match merged.last_mut() {
            Some(last) if content[last.1..s].bytes().all(|b| b == b'\n' || b == b' ') && content[last.1..s].matches('\n').count() == 1 && !is_heading_line(content, s) && !is_heading_line(content, last.0) => last.1 = e,
            _ => merged.push((s, e)),
        }
    }
    for (s, e) in merged {
        out.push(hl(s, e, "spell"));
        inline_highlights(content, s, e, &mut out);
    }
    out
}

fn is_heading_line(content: &str, at: usize) -> bool {
    let ls = content[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    content[ls..].trim_start_matches(' ').starts_with('#')
}

/// `markdown_inline` captures for `content[s..e]` (all marked as injections).
fn inline_highlights(content: &str, s: usize, e: usize, out: &mut Vec<Highlight>) {
    let text = &content[s..e];
    let inj = |h: Highlight| Highlight { meta: Meta { is_injection: true, ..h.meta }, ..h };
    let mut push = |h: Highlight| out.push(inj(h));
    // Code spans follow tree-sitter-markdown-inline's scanner; the rest of the inline syntax is parsed
    // on a copy with the spans masked out.
    let spans = code_spans(text);
    let mut masked = text.as_bytes().to_vec();
    for (i, b) in masked.iter_mut().enumerate() {
        if spans.iter().any(|(x, y, _)| i >= *x && i < *y) {
            *b = b'x';
        } else if *b == b'`' {
            *b = b'\'';
        }
    }
    let masked = String::from_utf8(masked).unwrap_or_else(|_| text.to_string());
    let mut covered: Vec<(usize, usize)> = Vec::new();
    for (x, y, n) in &spans {
        let (a, b) = (s + x, s + y);
        push(hl(a, b, "markup.raw"));
        push(hl(a, b, "nospell"));
        push(conceal(a, a + n, "conceal", "", true));
        push(conceal(b - n, b, "conceal", "", true));
        covered.push((a, b));
    }
    // Reference links need no definition in tree-sitter's grammar.
    let mut unknown = |_: pulldown_cmark::BrokenLink<'_>| Some((pulldown_cmark::CowStr::from(""), pulldown_cmark::CowStr::from("")));
    let parser = Parser::new_with_broken_link_callback(&masked, Options::ENABLE_STRIKETHROUGH, Some(&mut unknown)).into_offset_iter();
    let events: Vec<(Event, std::ops::Range<usize>)> = parser.collect();
    let mut idx = 0;
    while idx < events.len() {
        let (ev, r) = &events[idx];
        let (a, b) = (s + r.start, s + r.end);
        match ev {
            Event::Start(Tag::Emphasis) | Event::Start(Tag::Strong) | Event::Start(Tag::Strikethrough) => {
                let (group, n) = match ev {
                    Event::Start(Tag::Emphasis) => ("markup.italic", 1),
                    Event::Start(Tag::Strong) => ("markup.strong", 2),
                    _ => ("markup.strikethrough", content[a..b].chars().take_while(|c| *c == '~').count()),
                };
                push(hl(a, b, group));
                push(conceal(a, a + n, "conceal", "", true));
                push(conceal(b - n, b, "conceal", "", true));
            }
            Event::Start(Tag::Link { link_type, dest_url, .. }) | Event::Start(Tag::Image { link_type, dest_url, .. }) => {
                let image = matches!(ev, Event::Start(Tag::Image { .. }));
                let raw = &content[a..b];
                // Find the end of the link text: the matching `]`.
                let open = if image { a + 1 } else { a };
                let close = find_close_bracket(content, open, b);
                match link_type {
                    LinkType::Autolink | LinkType::Email => {
                        push(hl(a, b, "markup.link.url"));
                        push(hl(a, b, "nospell"));
                    }
                    LinkType::Inline if !image => {
                        if let Some(close) = close {
                            push(conceal(open, open + 1, "conceal", "", true));
                            push(hl(open + 1, close, "markup.link.label"));
                            push(hl(close, close + 1, "markup.link.bracket.close"));
                            push(conceal(close, close + 1, "conceal", " ", true));
                            let paren = close + 1;
                            push(hl(paren, paren + 1, "markup.link"));
                            push(hl(b - 1, b, "markup.link"));
                            let inner = &content[paren + 1..b - 1];
                            let lead = inner.len() - inner.trim_start().len();
                            let dest_start = paren + 1 + lead;
                            let dest_len = raw_dest_len(&content[dest_start..b - 1], dest_url);
                            push(hl(dest_start, dest_start + dest_len, "markup.link.url"));
                            push(hl(dest_start, dest_start + dest_len, "nospell"));
                            let rest = &content[dest_start + dest_len..b - 1];
                            let tlead = rest.len() - rest.trim_start().len();
                            let title = rest.trim();
                            if !title.is_empty() {
                                let ts = dest_start + dest_len + tlead;
                                push(hl(ts, ts + title.len(), "markup.link.label"));
                            }
                        }
                    }
                    _ if image => {
                        if let Some(close) = close {
                            push(hl(open + 1, close, "markup.link.label"));
                            push(conceal(a, open + 1, "markup.link", "", true));
                            push(conceal(close, b, "markup.link", "", true));
                        }
                    }
                    LinkType::Reference | LinkType::ReferenceUnknown => {
                        if let Some(close) = close {
                            push(hl(open + 1, close, "markup.link.label"));
                            push(conceal(open, open + 1, "markup.link", "", true));
                            push(conceal(close, b, "markup.link", "", true));
                        }
                    }
                    LinkType::Collapsed | LinkType::CollapsedUnknown | LinkType::Shortcut | LinkType::ShortcutUnknown => {
                        if let Some(close) = close {
                            push(hl(open + 1, close, "markup.link.label"));
                            push(hl(open + 1, close, "nospell"));
                            push(conceal(open, open + 1, "markup.link", "", true));
                            push(conceal(close, b, "markup.link", "", true));
                        }
                    }
                    _ => {}
                }
                let _ = raw;
            }
            Event::HardBreak => {
                let raw = &content[a..b];
                let end = if raw.ends_with('\n') { b - 1 } else { b };
                push(hl(a, end, "string.escape"));
            }
            _ => {}
        }
        idx += 1;
    }
    // Backslash escapes and entity references (outside code spans).
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let abs = s + i;
        if covered.iter().any(|(x, y)| abs >= *x && abs < *y) {
            i += 1;
            continue;
        }
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_punctuation() {
            push(hl(abs, abs + 2, "string.escape"));
            i += 2;
            continue;
        }
        if bytes[i] == b'&'
            && let Some(semi) = text[i..].find(';')
        {
            let ent = &text[i..i + semi + 1];
            let replacement = match ent {
                "&nbsp;" => Some(""),
                "&lt;" => Some("<"),
                "&gt;" => Some(">"),
                "&amp;" => Some("&"),
                "&quot;" => Some("\""),
                "&ensp;" | "&emsp;" => Some(" "),
                _ => None,
            };
            if ent.len() > 2 && ent[1..ent.len() - 1].chars().all(|c| c.is_ascii_alphanumeric() || c == '#') {
                push(hl(abs, abs + ent.len(), "character.special"));
                if let Some(r) = replacement {
                    push(conceal(abs, abs + ent.len(), "character.special", r, true));
                }
                i += ent.len();
                continue;
            }
        }
        i += 1;
    }
    // Autolink `<...>` brackets are concealed by `addMarkdownLinkHighlights`.
    let urls: Vec<(usize, usize)> = out.iter().filter(|h| h.group == "markup.link.url").map(|h| (h.start, h.end)).collect();
    for (a, b) in urls {
        if content.as_bytes().get(a) == Some(&b'<') && content.as_bytes().get(b - 1) == Some(&b'>') {
            for h in out.iter_mut().filter(|h| h.group == "markup.link.url" && h.start == a && h.end == b) {
                h.start = a + 1;
                h.end = b - 1;
            }
            out.push(conceal(a, a + 1, "conceal", "", true));
            out.push(conceal(b - 1, b, "conceal", "", true));
        }
    }
}

/// Code spans as tree-sitter-markdown-inline's external scanner finds them: an opening run of `n`
/// backticks needs a later run of exactly `n`; inside the span, any run of at least `n` closes it with
/// its last `n` backticks. Returns `(start, end, n)` byte ranges.
fn code_spans(text: &str) -> Vec<(usize, usize, usize)> {
    let b = text.as_bytes();
    let run = |i: usize| b[i..].iter().take_while(|c| **c == b'`').count();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() && b[i + 1].is_ascii_punctuation() {
            i += 2;
            continue;
        }
        if b[i] != b'`' {
            i += 1;
            continue;
        }
        let level = run(i);
        let mut j = i + level;
        let mut close = 0;
        while j < b.len() {
            if b[j] == b'`' {
                close += 1;
            } else {
                if close == level {
                    break;
                }
                close = 0;
            }
            j += 1;
        }
        if close != level {
            i += 1;
            continue;
        }
        let mut k = i + level;
        let mut end = None;
        while k < b.len() {
            if b[k] == b'`' {
                let r = run(k);
                if r >= level {
                    end = Some(k + r);
                    break;
                }
                k += r;
            } else {
                k += 1;
            }
        }
        match end {
            Some(e) => {
                out.push((i, e, level));
                i = e;
            }
            None => i += 1,
        }
    }
    out
}

fn find_close_bracket(content: &str, open: usize, end: usize) -> Option<usize> {
    let bytes = content.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < end {
        match bytes[i] {
            b'\\' => i += 1,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn raw_dest_len(after: &str, dest: &str) -> usize {
    if after.starts_with('<') {
        return after.find('>').map(|i| i + 1).unwrap_or(after.len());
    }
    if after.starts_with(dest) {
        return dest.len();
    }
    after.find(char::is_whitespace).unwrap_or(after.len())
}

/// Filetypes OpenTUI ships tree-sitter parsers for (`default-parsers.ts`); others render unstyled.
pub fn has_parser(filetype: &str) -> bool {
    matches!(filetype, "javascript" | "javascriptreact" | "typescript" | "typescriptreact" | "markdown" | "markdown_inline" | "zig")
}

/// Code highlighting for `filetype` (approximated with syntect scopes mapped to tree-sitter groups).
pub fn code_highlights(content: &str, filetype: &str) -> Vec<Highlight> {
    if filetype == "markdown" {
        return markdown_highlights(content);
    }
    if !has_parser(filetype) {
        return vec![];
    }
    let lang = match filetype {
        "typescriptreact" | "javascriptreact" | "typescript" | "javascript" => "typescript",
        other => other,
    };
    let Some(lines) = crate::ui::highlight::scopes(content, lang) else { return vec![] };
    let starts = line_starts(content);
    let mut out = Vec::new();
    for (li, toks) in lines.into_iter().enumerate() {
        let base = starts.get(li).copied().unwrap_or(0);
        for (a, b, scope) in toks {
            if let Some(scope) = scope {
                out.push(hl(base + a as usize, base + b as usize, scope));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conceals_inline_markers() {
        let syntax = super::super::theme::fallback(crate::theme::Mode::Dark);
        let st = MiniSyntax::generate(
            &crate::theme::resolve::resolve_document(&serde_json::from_str(crate::theme::OPENCODE_V2).unwrap(), crate::theme::Mode::Dark).unwrap(),
            &|c| super::super::theme::nearest_indexed(&super::super::theme::ansi_palette(), c),
        );
        let _ = syntax;
        let text = "Hello **world** and `code` and *em*.";
        let chunks = to_chunks(text, &markdown_highlights(text), &st, true, None, Color::WHITE);
        let plain: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(plain, "Hello world and code and em.");
        let heading = "# Title";
        let chunks = to_chunks(heading, &markdown_highlights(heading), &st, true, None, Color::WHITE);
        assert_eq!(chunks.iter().map(|c| c.text.as_str()).collect::<String>(), "Title");
        let link = "See [link](https://example.com) now.";
        let chunks = to_chunks(link, &markdown_highlights(link), &st, true, None, Color::WHITE);
        assert_eq!(chunks.iter().map(|c| c.text.as_str()).collect::<String>(), "See link (https://example.com) now.");
    }
}
