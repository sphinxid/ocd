//! Line-by-line port of `packages/core/src/tool/html-markdown.ts`
//! (`convertHTMLToMarkdown`).

use std::cell::RefCell;
use std::rc::Rc;

use super::js;
use super::parser::{self, Attributes, Handler};

pub const MAX_MARKDOWN_BYTES: i64 = 5 * 1024 * 1024;
const CONTENT_BYTES: i64 = MAX_MARKDOWN_BYTES - 64 * 1024;
const CHUNK_UNITS: usize = 64 * 1024;

type Shared<T> = Rc<RefCell<T>>;

fn shared<T>(value: T) -> Shared<T> {
    Rc::new(RefCell::new(value))
}

fn is_omitted(name: &str) -> bool {
    matches!(name, "script" | "style" | "noscript" | "iframe" | "object" | "embed" | "meta" | "link" | "template")
}

fn is_block(name: &str) -> bool {
    matches!(
        name,
        "address"
            | "article"
            | "aside"
            | "details"
            | "dialog"
            | "div"
            | "dl"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "header"
            | "main"
            | "nav"
            | "p"
            | "section"
            | "summary"
    )
}

fn is_heading(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 2 && bytes[0] == b'h' && (b'1'..=b'6').contains(&bytes[1])
}

#[derive(Debug)]
struct Link {
    href: String,
    title: Option<String>,
}

struct Marker {
    index: isize,
    block: u64,
    leading_space: bool,
    previous: Option<Shared<Marker>>,
}

struct Code {
    inline: bool,
    text: String,
    language: Option<String>,
}

struct List {
    ordered: bool,
    next: f64,
    previous: Option<Shared<List>>,
}

struct Item {
    indent: String,
    previous: Option<Rc<Item>>,
}

struct Table {
    start: usize,
    rows: Vec<Vec<String>>,
    row: Option<Vec<String>>,
    caption: Option<String>,
    fallback: bool,
    previous: Option<Shared<Table>>,
}

struct Details {
    open: bool,
    summary: bool,
    previous: Option<Shared<Details>>,
}

#[derive(Default)]
struct Frame {
    suppressed: bool,
    link: Option<Rc<Link>>,
    suspended_link: Option<Rc<Link>>,
    marker: Option<Shared<Marker>>,
    code: Option<Shared<Code>>,
    link_code: Option<Shared<Code>>,
    resumed_code: Option<Shared<Code>>,
    list: Option<Shared<List>>,
    item: Option<Rc<Item>>,
    table: Option<Shared<Table>>,
    cell: Option<usize>,
    caption: Option<usize>,
    details: Option<Shared<Details>>,
}

enum Chunk {
    Text(String),
    Raw(String),
}

impl Chunk {
    fn value(&self) -> &str {
        match self {
            Chunk::Text(value) | Chunk::Raw(value) => value,
        }
    }
}

fn same_link(left: &Option<Rc<Link>>, right: &Option<Rc<Link>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(a), Some(b)) => Rc::ptr_eq(a, b),
        _ => false,
    }
}

fn same_code(left: &Option<Shared<Code>>, right: &Option<Shared<Code>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(a), Some(b)) => Rc::ptr_eq(a, b),
        _ => false,
    }
}

fn is_inline_code(code: &Option<Shared<Code>>) -> bool {
    code.as_ref().is_some_and(|code| code.borrow().inline)
}

/// `value.replace(/([\\()])/g, "\\$1").replace(/[\t\n\r ]+/g, "%20")`
fn destination(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '(' | ')') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    collapse(&escaped, &['\t', '\n', '\r', ' '], "%20")
}

/// Replace runs of any of `set` with `with`.
fn collapse(value: &str, set: &[char], with: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_run = false;
    for c in value.chars() {
        if set.contains(&c) {
            if !in_run {
                out.push_str(with);
                in_run = true;
            }
        } else {
            in_run = false;
            out.push(c);
        }
    }
    out
}

fn title(value: Option<&str>) -> String {
    match value {
        Some(value) if !value.is_empty() => {
            let collapsed = collapse(value, &['\t', '\n', '\r', ' '], " ");
            let trimmed = js::trim(&collapsed);
            let mut escaped = String::with_capacity(trimmed.len());
            for c in trimmed.chars() {
                if matches!(c, '\\' | '"') {
                    escaped.push('\\');
                }
                escaped.push(c);
            }
            format!(" \"{escaped}\"")
        }
        _ => String::new(),
    }
}

fn link_close(link: &Link) -> String {
    format!("]({}{})", destination(&link.href), title(link.title.as_deref()))
}

fn inline_marker(name: &str) -> Option<&'static str> {
    match name {
        "strong" | "b" => Some("**"),
        "em" | "i" => Some("*"),
        "s" | "strike" | "del" => Some("~~"),
        _ => None,
    }
}

/// `/(?:language-|lang-)([^\s]+)/` capture group 1.
fn code_language(class: &str) -> Option<String> {
    for (index, _) in class.char_indices() {
        let rest = &class[index..];
        let after = if let Some(after) = rest.strip_prefix("language-") {
            after
        } else if let Some(after) = rest.strip_prefix("lang-") {
            after
        } else {
            continue;
        };
        let captured: String = after.chars().take_while(|c| !js::is_js_whitespace(*c)).collect();
        if !captured.is_empty() {
            return Some(captured);
        }
    }
    None
}

/// Escape one non-whitespace text part.
fn escape_text(part: &str) -> String {
    let mut escaped = String::with_capacity(part.len() + 4);
    let bytes = part.as_bytes();
    let mut copied = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        let replacement = match byte {
            b'\\' => "\\\\",
            b'`' => "\\`",
            b'*' => "\\*",
            b'_' => "\\_",
            b'[' => "\\[",
            b']' => "\\]",
            b'<' => "\\<",
            b'>' => "\\>",
            b'|' => "\\|",
            b'~' => "\\~",
            _ => continue,
        };
        escaped.push_str(&part[copied..index]);
        escaped.push_str(replacement);
        copied = index + 1;
    }
    escaped.push_str(&part[copied..]);
    if escaped.starts_with(['#', '+', '-']) {
        escaped.insert(0, '\\');
    }
    let digits = escaped.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 && escaped.as_bytes().get(digits) == Some(&b'.') {
        escaped.insert(digits, '\\');
    }
    escaped
}

/// `.replace(/[\t\r\n ]+/g, " ").trim()`
fn squash(value: &str) -> String {
    js::trim(&collapse(value, &['\t', '\r', '\n', ' '], " ")).to_string()
}

/// `.replace(/(?<!\\)\|/g, "\\|")`
fn escape_pipes(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut previous: Option<char> = None;
    for c in value.chars() {
        if c == '|' && previous != Some('\\') {
            out.push('\\');
        }
        out.push(c);
        previous = Some(c);
    }
    out
}

/// `value.replace(/^/gm, prefix)`: JS multiline `^` matches at the start and
/// after every line terminator (`\n`, `\r`, U+2028, U+2029).
fn prefix_lines(value: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(value.len() + prefix.len() * 4);
    out.push_str(prefix);
    for c in value.chars() {
        out.push(c);
        if matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}') {
            out.push_str(prefix);
        }
    }
    out
}

struct Converter {
    output: Vec<Chunk>,
    stack: Vec<Frame>,
    pending_space: bool,
    pending_indent: String,
    last: Option<char>,
    quote_depth: i64,
    needs_quote_prefix: bool,
    block_count: u64,
    depth: i64,
    stopped: bool,
    output_bytes: i64,
    active_code: Option<Shared<Code>>,
    active_link: Option<Rc<Link>>,
    link_open: bool,
    active_marker: Option<Shared<Marker>>,
    active_list: Option<Shared<List>>,
    active_item: Option<Rc<Item>>,
    active_table: Option<Shared<Table>>,
    active_cell: Option<usize>,
    table_depth: i64,
    fallback_suppressed_depth: i64,
    fallback_omitted_depth: i64,
    active_details: Option<Shared<Details>>,
}

impl Converter {
    fn new() -> Self {
        Converter {
            output: Vec::new(),
            stack: Vec::new(),
            pending_space: false,
            pending_indent: String::new(),
            last: None,
            quote_depth: 0,
            needs_quote_prefix: false,
            block_count: 0,
            depth: 0,
            stopped: false,
            output_bytes: 0,
            active_code: None,
            active_link: None,
            link_open: false,
            active_marker: None,
            active_list: None,
            active_item: None,
            active_table: None,
            active_cell: None,
            table_depth: 0,
            fallback_suppressed_depth: 0,
            fallback_omitted_depth: 0,
            active_details: None,
        }
    }

    fn top(&mut self) -> &mut Frame {
        self.stack.last_mut().expect("frame pushed")
    }

    fn append(&mut self, value: &str, content: bool) {
        let limit = if content { CONTENT_BYTES } else { MAX_MARKDOWN_BYTES };
        if value.is_empty() || self.output_bytes >= limit {
            return;
        }
        let bytes = js::byte_len(value) as i64;
        let remaining = limit - self.output_bytes;
        let next = if bytes <= remaining { value } else { js::slice_bytes(value, remaining) };
        self.output_bytes += if bytes <= remaining { bytes } else { js::byte_len(next) as i64 };
        if let Some(c) = next.chars().next_back() {
            self.last = Some(c);
        }
        self.output.push(Chunk::Text(next.to_string()));
    }

    fn append_raw(&mut self, value: &str) {
        let before = self.output.len();
        self.append(value, false);
        if self.output.len() > before
            && let Some(Chunk::Text(text)) = self.output.pop()
        {
            self.output.push(Chunk::Raw(text));
        }
    }

    fn take(&mut self, start: usize) -> String {
        if start >= self.output.len() {
            return String::new();
        }
        let value: String = self.output.drain(start..).map(|chunk| chunk.value().to_string()).collect();
        self.output_bytes -= js::byte_len(&value) as i64;
        value
    }

    fn quote_prefix(&self) -> String {
        "> ".repeat(self.quote_depth.clamp(0, 8) as usize)
    }

    fn prefix_quote(&mut self) {
        if !self.needs_quote_prefix || self.quote_depth == 0 || self.active_cell.is_some() {
            return;
        }
        let prefix = self.quote_prefix();
        self.append(&prefix, false);
        self.needs_quote_prefix = false;
    }

    fn flush_space(&mut self) {
        if !self.pending_space {
            return;
        }
        if let Some(marker) = self.active_marker.clone() {
            let mut marker = marker.borrow_mut();
            if self.output.len() as isize == marker.index + 1 && self.last != Some(' ') && self.last != Some('\n') {
                if marker.index >= 0
                    && let Some(Chunk::Text(value)) = self.output.get_mut(marker.index as usize)
                {
                    value.insert(0, ' ');
                }
                self.output_bytes += 1;
                marker.leading_space = true;
                self.pending_space = false;
                return;
            }
        }
        if let Some(last) = self.last
            && last != '\n'
            && last != ' '
        {
            self.append(" ", false);
        }
        self.pending_space = false;
    }

    fn inline(&mut self, value: &str, open: bool) {
        if open {
            self.flush_space();
        }
        self.prefix_quote();
        if !self.pending_indent.is_empty() {
            let indent = std::mem::take(&mut self.pending_indent);
            self.append(&indent, false);
        }
        if self.active_link.is_some() && !self.link_open {
            self.append("[", false);
            self.link_open = true;
        }
        self.append(value, false);
    }

    fn block(&mut self) {
        if self.link_open
            && let Some(link) = self.active_link.clone()
        {
            self.append(&link_close(&link), false);
            self.link_open = false;
        }
        self.pending_space = false;
        self.append("\n\n", false);
        self.block_count += 1;
        self.needs_quote_prefix = self.quote_depth > 0;
        self.pending_indent = self.active_item.as_ref().map(|item| item.indent.clone()).unwrap_or_default();
    }

    /// `suspendLink(frame)` for the frame on top of the stack.
    fn suspend_link(&mut self) {
        let Some(link) = self.active_link.clone() else { return };
        self.top().suspended_link = Some(link);
        self.block();
        self.active_link = None;
        self.link_open = false;
    }

    fn text(&mut self, value: &str) {
        if let Some(code) = &self.active_code {
            code.borrow_mut().text.push_str(value);
            return;
        }
        let is_space = |b: u8| matches!(b, b'\t' | b'\n' | 0x0c | b'\r' | b' ');
        let mut rest = value;
        while !rest.is_empty() {
            let bytes = rest.as_bytes();
            let space = is_space(bytes[0]);
            let mut end = 1;
            while end < bytes.len() && is_space(bytes[end]) == space {
                end += 1;
            }
            let part = &rest[..end];
            rest = &rest[end..];
            if space {
                self.pending_space = true;
                continue;
            }
            self.flush_space();
            self.prefix_quote();
            if !self.pending_indent.is_empty() {
                let indent = std::mem::take(&mut self.pending_indent);
                self.append(&indent, false);
            }
            if self.active_link.is_some() && !self.link_open {
                self.append("[", false);
                self.link_open = true;
            }
            let escaped = escape_text(part);
            self.append(&escaped, true);
        }
    }

    fn finish_code(&mut self, code: &Shared<Code>) {
        let (inline, text, language) = {
            let code = code.borrow();
            (code.inline, code.text.clone(), code.language.clone())
        };
        if inline && text.is_empty() {
            return;
        }
        let mut backticks = 0usize;
        let mut tildes = 0usize;
        let mut current_backticks = 0usize;
        let mut current_tildes = 0usize;
        for c in text.chars() {
            current_backticks = if c == '`' { current_backticks + 1 } else { 0 };
            current_tildes = if c == '~' { current_tildes + 1 } else { 0 };
            backticks = backticks.max(current_backticks);
            tildes = tildes.max(current_tildes);
        }
        if inline {
            let fence = "`".repeat((backticks + 1).max(1));
            self.flush_space();
            self.prefix_quote();
            let available = (CONTENT_BYTES - self.output_bytes - fence.len() as i64 * 2).max(0);
            let mut payload = js::slice_bytes(&text, available).to_string();
            while !payload.is_empty() {
                let edge = payload.starts_with([' ', '`']) || payload.ends_with([' ', '`']);
                let all_spaces = payload.chars().all(|c| c == ' ');
                let padding = if edge && !all_spaces { " " } else { "" };
                let bytes = js::byte_len(&payload) as i64;
                if bytes + padding.len() as i64 * 2 <= available {
                    self.append_raw(&format!("{fence}{padding}{payload}{padding}{fence}"));
                    return;
                }
                payload = js::slice_bytes(&payload, bytes - 1).to_string();
            }
            return;
        }
        if self.active_cell.is_some() {
            self.text(&text);
            return;
        }
        let marker = if backticks <= tildes { '`' } else { '~' };
        let length = 3.max(if marker == '`' { backticks } else { tildes } + 1);
        let fence: String = std::iter::repeat_n(marker, length).collect();
        self.block();
        let prefix = format!("{fence}{}\n", language.as_deref().unwrap_or(""));
        let quote = if self.quote_depth > 0 { self.quote_prefix() } else { String::new() };
        let mut payload = text;
        loop {
            let newline = if payload.ends_with('\n') { "" } else { "\n" };
            let candidate = format!("{prefix}{payload}{newline}{fence}");
            let value = if quote.is_empty() { candidate } else { prefix_lines(&candidate, &quote) };
            let value_bytes = js::byte_len(&value) as i64;
            if self.output_bytes + value_bytes <= CONTENT_BYTES {
                self.append_raw(&value);
                self.block();
                return;
            }
            if payload.is_empty() {
                return;
            }
            let excess = value_bytes - (CONTENT_BYTES - self.output_bytes).max(0);
            let keep = (js::byte_len(&payload) as i64 - excess).max(0);
            payload = js::slice_bytes(&payload, keep).to_string();
        }
    }

    fn open_tag(&mut self, name: &str, attributes: &Attributes) {
        self.depth += 1;
        if self.depth > 10_000 {
            if self.stack.last().is_some_and(|frame| frame.suppressed) {
                let visible_parent =
                    self.stack.iter().rposition(|frame| !frame.suppressed).map(|index| index as i64).unwrap_or(-1);
                self.fallback_suppressed_depth = visible_parent + 2;
            }
            self.active_code = None;
            self.stopped = true;
        }
        if self.stopped {
            if is_omitted(name) {
                self.fallback_omitted_depth += 1;
            } else {
                self.pending_space = true;
            }
            return;
        }
        let suppressed = self.stack.last().is_some_and(|frame| frame.suppressed) || is_omitted(name);
        let mut frame = Frame { suppressed, ..Frame::default() };
        let hidden = attributes.has("hidden")
            || attributes.get("aria-hidden").is_some_and(|value| js::to_lower_case(value) == "true")
            || name == "head";
        let details = self.active_details.clone();
        let closed_details = details.as_ref().is_some_and(|details| {
            let details = details.borrow();
            !details.open && !details.summary
        });
        if hidden || (closed_details && name != "summary") {
            frame.suppressed = true;
        }
        let suppressed = frame.suppressed;
        self.stack.push(frame);
        if suppressed {
            return;
        }

        if let Some(code) = self.active_code.clone()
            && !code.borrow().inline
        {
            if name == "br" {
                code.borrow_mut().text.push('\n');
            }
            if name == "code"
                && let Some(class) = attributes.get("class")
                && !class.is_empty()
            {
                code.borrow_mut().language = code_language(class);
            }
            return;
        }
        if name == "details" {
            let details = shared(Details {
                open: attributes.has("open"),
                summary: false,
                previous: self.active_details.clone(),
            });
            self.top().details = Some(details.clone());
            self.active_details = Some(details);
            self.block();
            return;
        }
        if name == "summary" {
            if let Some(details) = &details {
                details.borrow_mut().summary = true;
            }
            self.block();
            return;
        }
        if name == "pre" {
            self.suspend_link();
            let code = shared(Code { inline: false, text: String::new(), language: None });
            self.top().code = Some(code.clone());
            self.active_code = Some(code);
            return;
        }
        if name == "code" {
            if is_inline_code(&self.active_code) {
                return;
            }
            let code = shared(Code { inline: true, text: String::new(), language: None });
            self.top().code = Some(code.clone());
            self.active_code = Some(code);
            return;
        }
        if is_heading(name) {
            self.block();
            let level = (name.as_bytes()[1] - b'0') as usize;
            self.inline(&format!("{} ", "#".repeat(level)), false);
            return;
        }
        if is_block(name) {
            self.suspend_link();
            if name == "p" && self.last == Some(' ') {
                return;
            }
            self.block();
            return;
        }
        if name == "br" {
            self.pending_space = false;
            self.inline("  \n", false);
            self.needs_quote_prefix = self.quote_depth > 0;
            return;
        }
        if name == "hr" {
            self.block();
            self.inline("---", false);
            self.block();
            return;
        }
        if let Some(marker) = inline_marker(name) {
            self.inline(marker, true);
            let marker = shared(Marker {
                index: self.output.len() as isize - 1,
                block: self.block_count,
                leading_space: false,
                previous: self.active_marker.clone(),
            });
            self.top().marker = Some(marker.clone());
            self.active_marker = Some(marker);
            return;
        }
        if name == "a" {
            if self.active_link.is_some() && is_inline_code(&self.active_code) {
                let active_link = self.active_link.clone();
                let active_code = self.active_code.clone();
                let parent = self.stack.iter().rposition(|candidate| {
                    candidate.link.is_some()
                        && same_link(&candidate.link, &active_link)
                        && same_code(&candidate.link_code, &active_code)
                });
                if let Some(parent) = parent {
                    if let Some(code) = &active_code {
                        self.finish_code(code);
                    }
                    if self.link_open
                        && let Some(link) = &active_link
                    {
                        self.append(&link_close(link), false);
                    }
                    self.active_code = self.stack[parent].resumed_code.clone();
                    self.stack[parent].link = None;
                    self.stack[parent].link_code = None;
                    self.active_link = None;
                    self.link_open = false;
                }
            }
            if is_inline_code(&self.active_code) {
                let code = self.active_code.clone().expect("inline code");
                self.top().resumed_code = Some(code.clone());
                if !code.borrow().text.is_empty() {
                    self.finish_code(&code);
                }
                code.borrow_mut().text.clear();
                self.active_code = None;
                let link = Rc::new(Link {
                    href: attributes.get("href").unwrap_or("").to_string(),
                    title: attributes.get("title").map(str::to_string),
                });
                self.top().link = Some(link.clone());
                self.active_link = Some(link);
                self.link_open = true;
                self.inline("[", true);
                let link_code = shared(Code { inline: true, text: String::new(), language: None });
                self.top().link_code = Some(link_code.clone());
                self.active_code = Some(link_code);
                return;
            }
            if let Some(active) = self.active_link.clone() {
                if self.link_open {
                    self.append(&link_close(&active), false);
                }
                let active = Some(active);
                if let Some(parent) =
                    self.stack.iter().rposition(|candidate| candidate.link.is_some() && same_link(&candidate.link, &active))
                {
                    self.stack[parent].link = None;
                }
                self.active_link = None;
                self.link_open = false;
            }
            let link = Rc::new(Link {
                href: attributes.get("href").unwrap_or("").to_string(),
                title: attributes.get("title").map(str::to_string),
            });
            self.top().link = Some(link.clone());
            self.active_link = Some(link);
            self.link_open = true;
            self.inline("[", true);
            return;
        }
        if name == "img" {
            let mut alt = String::new();
            for c in attributes.get("alt").unwrap_or("").chars() {
                if matches!(c, '\\' | ']') {
                    alt.push('\\');
                }
                alt.push(c);
            }
            let close = format!(
                "]({}{})",
                destination(attributes.get("src").unwrap_or("")),
                title(attributes.get("title"))
            );
            let open = "![";
            let available = CONTENT_BYTES - self.output_bytes - js::byte_len(&format!("{open}{close}")) as i64;
            let value = format!("{open}{}{close}", js::slice_bytes(&alt, available.max(0)));
            self.inline(&value, true);
            return;
        }
        if name == "blockquote" {
            self.suspend_link();
            self.block();
            self.quote_depth += 1;
            self.needs_quote_prefix = true;
            return;
        }
        if name == "ul" || name == "ol" {
            self.suspend_link();
            let start = js::parse_int(attributes.get("start").unwrap_or("1"));
            let list = shared(List {
                ordered: name == "ol",
                next: if start.is_nan() { 1.0 } else { start },
                previous: self.active_list.clone(),
            });
            self.top().list = Some(list.clone());
            self.active_list = Some(list);
            self.block();
            return;
        }
        if name == "li" {
            self.block();
            let value = js::parse_int(attributes.get("value").unwrap_or(""));
            let ordered = self.active_list.as_ref().is_some_and(|list| list.borrow().ordered);
            if ordered
                && !value.is_nan()
                && let Some(list) = &self.active_list
            {
                list.borrow_mut().next = value;
            }
            let marker = if ordered {
                let list = self.active_list.clone().expect("ordered list");
                let mut list = list.borrow_mut();
                let current = list.next;
                list.next = current + 1.0;
                format!("{}.", js::number_to_string(current))
            } else {
                "-".to_string()
            };
            let parent_indent = self.active_item.as_ref().map(|item| item.indent.clone()).unwrap_or_default();
            let parent_indent: String = parent_indent.chars().take(24).collect();
            let prefix = format!("{parent_indent}{marker} ");
            let item = Rc::new(Item {
                indent: " ".repeat(prefix.encode_utf16().count()),
                previous: self.active_item.clone(),
            });
            self.top().item = Some(item.clone());
            self.active_item = Some(item);
            self.pending_indent.clear();
            self.inline(&prefix, false);
            return;
        }
        if name == "table" {
            self.suspend_link();
            self.table_depth += 1;
            if self.table_depth == 1 {
                self.block();
                let table = shared(Table {
                    start: self.output.len(),
                    rows: Vec::new(),
                    row: None,
                    caption: None,
                    fallback: false,
                    previous: self.active_table.clone(),
                });
                self.top().table = Some(table.clone());
                self.active_table = Some(table);
            } else {
                self.pending_space = true;
            }
            return;
        }
        if name == "tr" {
            if self.table_depth != 1 {
                self.pending_space = true;
                return;
            }
            if let Some(table) = &self.active_table {
                table.borrow_mut().row = Some(Vec::new());
            }
            return;
        }
        if name == "th" || name == "td" {
            if self.table_depth != 1 {
                self.pending_space = true;
                return;
            }
            let span = attributes.get("colspan").is_some_and(|v| !v.is_empty())
                || attributes.get("rowspan").is_some_and(|v| !v.is_empty());
            if span && let Some(table) = &self.active_table {
                table.borrow_mut().fallback = true;
            }
            let start = self.output.len();
            self.top().cell = Some(start);
            self.active_cell = Some(start);
            return;
        }
        if name == "caption" {
            let start = self.output.len();
            self.top().caption = Some(start);
            return;
        }
        if name == "dt" {
            self.block();
            self.inline("**", false);
            return;
        }
        if name == "dd" {
            self.inline("\n: ", false);
        }
    }

    fn on_text_event(&mut self, value: &str) {
        if self.stopped {
            if self.fallback_suppressed_depth == 0 && self.fallback_omitted_depth == 0 {
                self.text(value);
            }
            return;
        }
        if self.stack.last().is_some_and(|frame| frame.suppressed) {
            return;
        }
        self.text(value);
    }

    fn close_tag(&mut self, name: &str) {
        self.depth -= 1;
        if self.stopped {
            if self.fallback_omitted_depth > 0 && is_omitted(name) {
                self.fallback_omitted_depth -= 1;
            }
            if self.fallback_suppressed_depth > 0 && self.depth < self.fallback_suppressed_depth {
                self.fallback_suppressed_depth = 0;
            }
            return;
        }
        let Some(frame) = self.stack.pop() else { return };
        if frame.suppressed {
            return;
        }
        if let Some(link_code) = &frame.link_code {
            if !same_link(&self.active_link, &frame.link) || !self.link_open {
                if let Some(resumed) = &frame.resumed_code {
                    let text = link_code.borrow().text.clone();
                    resumed.borrow_mut().text.push_str(&text);
                }
                self.active_code = frame.resumed_code.clone();
                if same_link(&self.active_link, &frame.link) {
                    self.active_link = None;
                }
                self.link_open = false;
                return;
            }
            self.active_code = None;
            self.finish_code(link_code);
            if self.link_open
                && let Some(link) = &frame.link
            {
                self.append(&link_close(link), false);
            }
            self.active_link = None;
            self.link_open = false;
            self.active_code = frame.resumed_code.clone();
            return;
        }
        if let Some(code) = &self.active_code
            && !code.borrow().inline
            && frame.code.is_none()
        {
            return;
        }
        if let Some(code) = &frame.code {
            self.active_code = None;
            self.finish_code(code);
            if let Some(link) = &frame.suspended_link {
                self.active_link = Some(link.clone());
            }
            return;
        }
        if name == "summary" {
            if let Some(details) = &self.active_details {
                details.borrow_mut().summary = false;
            }
            self.block();
            return;
        }
        if name == "details" {
            self.active_details = frame.details.as_ref().and_then(|details| details.borrow().previous.clone());
            self.block();
            return;
        }
        if name == "dt" {
            self.inline("**", false);
            return;
        }
        if name == "dd" {
            self.block();
            return;
        }
        if let Some(marker) = inline_marker(name) {
            let trailing_space = self.pending_space;
            self.pending_space = false;
            if let Some(frame_marker) = &frame.marker {
                self.active_marker = frame_marker.borrow().previous.clone();
                let (index, block, leading_space) = {
                    let m = frame_marker.borrow();
                    (m.index, m.block, m.leading_space)
                };
                if block != self.block_count || self.output.len() as isize == index + 1 {
                    if index >= 0
                        && let Some(chunk) = self.output.get_mut(index as usize)
                    {
                        *chunk = Chunk::Text(String::new());
                    }
                    self.pending_space = trailing_space || leading_space;
                    return;
                }
            }
            self.inline(marker, false);
            self.pending_space =
                trailing_space || frame.marker.as_ref().is_some_and(|marker| marker.borrow().leading_space);
            return;
        }
        if name == "a" {
            if let Some(link) = &frame.link
                && (same_link(&self.active_link, &frame.link) || self.active_link.is_none())
            {
                self.active_link = Some(link.clone());
                if self.link_open || self.last.is_some_and(|last| last != '\n') {
                    self.append(&link_close(link), false);
                }
                self.link_open = false;
                self.active_link = None;
            }
            return;
        }
        if is_heading(name) || is_block(name) {
            self.block();
            if let Some(link) = &frame.suspended_link {
                self.active_link = Some(link.clone());
            }
            return;
        }
        if name == "blockquote" {
            self.quote_depth -= 1;
            self.block();
            if let Some(link) = &frame.suspended_link {
                self.active_link = Some(link.clone());
            }
            return;
        }
        if name == "li" {
            self.active_item = frame.item.as_ref().and_then(|item| item.previous.clone());
            self.block();
            return;
        }
        if name == "ul" || name == "ol" {
            self.active_list = frame.list.as_ref().and_then(|list| list.borrow().previous.clone());
            self.block();
            if let Some(link) = &frame.suspended_link {
                self.active_link = Some(link.clone());
            }
            return;
        }
        if (name == "th" || name == "td") && self.table_depth == 1 {
            self.active_cell = None;
            if let Some(start) = frame.cell {
                let value = escape_pipes(&squash(&self.take(start)));
                if let Some(table) = &self.active_table
                    && let Some(row) = table.borrow_mut().row.as_mut()
                {
                    row.push(value);
                }
            }
            return;
        }
        if name == "tr" {
            if self.table_depth != 1 {
                return;
            }
            if let Some(table) = &self.active_table {
                let mut table = table.borrow_mut();
                if let Some(row) = table.row.take() {
                    table.rows.push(row);
                }
            }
            self.pending_space = true;
            return;
        }
        if name == "caption"
            && let Some(start) = frame.caption
            && self.active_table.is_some()
        {
            let caption = squash(&self.take(start));
            if let Some(table) = &self.active_table {
                table.borrow_mut().caption = Some(caption);
            }
            return;
        }
        if name == "table" {
            self.table_depth -= 1;
            if self.table_depth == 0 {
                let table = frame.table.clone();
                self.active_table = table.as_ref().and_then(|table| table.borrow().previous.clone());
                if let Some(table) = table {
                    let start = table.borrow().start;
                    let loose = squash(&self.take(start));
                    let (rows, caption, fallback) = {
                        let table = table.borrow();
                        (table.rows.clone(), table.caption.clone(), table.fallback)
                    };
                    let width = rows.first().map(Vec::len).unwrap_or(0);
                    let rectangular = width > 0 && rows.iter().all(|row| row.len() == width);
                    if !loose.is_empty() {
                        self.append(&loose, false);
                        self.block();
                    }
                    if let Some(caption) = caption.filter(|caption| !caption.is_empty()) {
                        self.append(&caption, false);
                        self.block();
                    }
                    if !fallback && rectangular {
                        let quote = if self.quote_depth > 0 { self.quote_prefix() } else { String::new() };
                        let prefix = format!("{quote}{}", self.pending_indent);
                        self.pending_indent.clear();
                        let header = format!(
                            "{prefix}| {} |\n{prefix}|{}",
                            rows[0].join(" | "),
                            " --- |".repeat(width)
                        );
                        self.append(&header, false);
                        for row in &rows[1..] {
                            let line = format!("\n{prefix}| {} |", row.join(" | "));
                            self.append(&line, false);
                        }
                    } else {
                        for (index, row) in rows.iter().enumerate() {
                            if index > 0 {
                                self.block();
                            }
                            self.append(&row.join(" | "), false);
                        }
                    }
                }
                self.block();
                if let Some(link) = &frame.suspended_link {
                    self.active_link = Some(link.clone());
                }
                return;
            }
            self.pending_space = true;
        }
    }

    fn finish(self) -> String {
        let mut normalized: Vec<String> = Vec::new();
        let mut pending_text = String::new();
        for chunk in &self.output {
            match chunk {
                Chunk::Raw(raw) => {
                    flush_text(&mut pending_text, &mut normalized);
                    normalized.push(raw.clone());
                }
                Chunk::Text(text) => pending_text.push_str(text),
            }
        }
        flush_text(&mut pending_text, &mut normalized);
        let joined = normalized.concat();
        js::slice_bytes(js::trim(&joined), MAX_MARKDOWN_BYTES).to_string()
    }
}

/// `.replace(/[ \t]+\n/g, (space) => (space.startsWith("  ") ? "  \n" : "\n"))`
fn strip_trailing_space(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut copied = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b' ' && bytes[index] != b'\t' {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && (bytes[index] == b' ' || bytes[index] == b'\t') {
            index += 1;
        }
        if index < bytes.len() && bytes[index] == b'\n' {
            out.push_str(&value[copied..start]);
            out.push_str(if value[start..].starts_with("  ") { "  \n" } else { "\n" });
            index += 1;
            copied = index;
        }
    }
    out.push_str(&value[copied..]);
    out
}

/// `.replace(/\n{3,}/g, "\n\n")`
fn collapse_newlines(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut copied = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\n' {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && bytes[index] == b'\n' {
            index += 1;
        }
        if index - start >= 3 {
            out.push_str(&value[copied..start]);
            out.push_str("\n\n");
            copied = index;
        }
    }
    out.push_str(&value[copied..]);
    out
}

/// Length of the `^(?:> )+` match.
fn quote_run(line: &str) -> usize {
    let mut length = 0;
    while line[length..].starts_with("> ") {
        length += 2;
    }
    length
}

fn flush_text(pending: &mut String, normalized: &mut Vec<String>) {
    if pending.is_empty() {
        return;
    }
    let cleaned = collapse_newlines(&strip_trailing_space(pending));
    let lines: Vec<&str> = cleaned.split('\n').collect();
    let mut mapped: Vec<String> = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        if !line.is_empty() {
            mapped.push((*line).to_string());
            continue;
        }
        let before = if index > 0 { quote_run(lines[index - 1]) } else { 0 };
        let after = lines.get(index + 1).map(|line| quote_run(line)).unwrap_or(0);
        if before == 0 || after == 0 || before == after {
            mapped.push(String::new());
            continue;
        }
        mapped.push("> ".repeat(before.min(after) / 2).trim_end().to_string());
    }
    normalized.push(mapped.join("\n"));
    pending.clear();
}

impl Handler for Converter {
    fn on_open_tag(&mut self, name: &str, attributes: &Attributes) {
        self.open_tag(name, attributes);
    }
    fn on_text(&mut self, text: &str) {
        self.on_text_event(text);
    }
    fn on_close_tag(&mut self, name: &str) {
        self.close_tag(name);
    }
}

pub fn convert_html_to_markdown(html: &str) -> String {
    let converter = parser::parse(html, CHUNK_UNITS, Converter::new());
    converter.finish()
}
