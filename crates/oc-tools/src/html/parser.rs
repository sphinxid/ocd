//! Port of htmlparser2 8.0.2 `lib/esm/Parser.js` with default options
//! (`xmlMode: false`, `decodeEntities: true`, lowercased tag and attribute
//! names, self-closing tags only recognised in foreign (svg/math) context).

use super::js;
use super::tokenizer::{QuoteType, TokenSink, Tokenizer};

/// Attributes of an open tag, in source order; the first occurrence of a
/// name wins (htmlparser2 checks `hasOwnProperty` before assigning).
#[derive(Debug, Default, Clone)]
pub(super) struct Attributes {
    items: Vec<(String, String)>,
}

impl Attributes {
    pub(super) fn get(&self, name: &str) -> Option<&str> {
        self.items.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }
    pub(super) fn has(&self, name: &str) -> bool {
        self.items.iter().any(|(key, _)| key == name)
    }
    fn insert_if_absent(&mut self, name: String, value: String) {
        if !self.has(&name) {
            self.items.push((name, value));
        }
    }
}

/// The subset of htmlparser2 handler callbacks used by the converters.
pub(super) trait Handler {
    fn on_open_tag(&mut self, name: &str, attributes: &Attributes);
    fn on_text(&mut self, text: &str);
    fn on_close_tag(&mut self, name: &str);
}

fn implies_close(name: &str) -> Option<&'static [&'static str]> {
    const FORM_TAGS: &[&str] = &["input", "option", "optgroup", "select", "button", "datalist", "textarea"];
    const P_TAG: &[&str] = &["p"];
    const TABLE_SECTION_TAGS: &[&str] = &["thead", "tbody"];
    const DDT_TAGS: &[&str] = &["dd", "dt"];
    const RTP_TAGS: &[&str] = &["rt", "rp"];
    Some(match name {
        "tr" => &["tr", "th", "td"],
        "th" => &["th"],
        "td" => &["thead", "th", "td"],
        "body" => &["head", "link", "script"],
        "li" => &["li"],
        "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => P_TAG,
        "select" | "input" | "output" | "button" | "datalist" | "textarea" => FORM_TAGS,
        "option" => &["option"],
        "optgroup" => &["optgroup", "option"],
        "dd" | "dt" => DDT_TAGS,
        "address" | "article" | "aside" | "blockquote" | "details" | "div" | "dl" | "fieldset" | "figcaption"
        | "figure" | "footer" | "form" | "header" | "hr" | "main" | "nav" | "ol" | "pre" | "section" | "table"
        | "ul" => P_TAG,
        "rt" | "rp" => RTP_TAGS,
        "tbody" | "tfoot" => TABLE_SECTION_TAGS,
        _ => return None,
    })
}

fn is_void_element(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "basefont"
            | "br"
            | "col"
            | "command"
            | "embed"
            | "frame"
            | "hr"
            | "img"
            | "input"
            | "isindex"
            | "keygen"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn is_foreign_context_element(name: &str) -> bool {
    matches!(name, "math" | "svg")
}

fn is_html_integration_element(name: &str) -> bool {
    matches!(name, "mi" | "mo" | "mn" | "ms" | "mtext" | "annotation-xml" | "foreignobject" | "desc" | "title")
}

struct Sink<'a, H: Handler> {
    input: &'a [u8],
    handler: H,
    tagname: String,
    attribname: String,
    attribvalue: String,
    attribs: Option<Attributes>,
    stack: Vec<String>,
    foreign_context: Vec<bool>,
    /// A high surrogate from a named entity whose value is emitted as two
    /// UTF-16 code units; joined with the following low surrogate.
    surrogate: Option<u32>,
}

impl<H: Handler> Sink<'_, H> {
    fn slice(&self, start: isize, end: isize) -> String {
        let len = self.input.len() as isize;
        let start = start.clamp(0, len) as usize;
        let end = end.clamp(0, len) as usize;
        if end <= start {
            return String::new();
        }
        match std::str::from_utf8(&self.input[start..end]) {
            Ok(text) => text.to_string(),
            Err(_) => String::from_utf8_lossy(&self.input[start..end]).into_owned(),
        }
    }

    fn flush_surrogate(&mut self) {
        if self.surrogate.take().is_some() {
            self.handler.on_text("\u{fffd}");
        }
    }

    fn emit_open_tag(&mut self, name: String) {
        self.tagname = name.clone();
        if let Some(closes) = implies_close(&name) {
            while let Some(top) = self.stack.last() {
                if !closes.contains(&top.as_str()) {
                    break;
                }
                let element = self.stack.pop().unwrap_or_default();
                self.handler.on_close_tag(&element);
            }
        }
        if !is_void_element(&name) {
            if is_foreign_context_element(&name) {
                self.foreign_context.push(true);
            } else if is_html_integration_element(&name) {
                self.foreign_context.push(false);
            }
            self.stack.push(name);
        }
        self.attribs = Some(Attributes::default());
    }

    fn end_open_tag(&mut self) {
        if let Some(attribs) = self.attribs.take() {
            let name = self.tagname.clone();
            self.handler.on_open_tag(&name, &attribs);
        }
        if is_void_element(&self.tagname) {
            let name = self.tagname.clone();
            self.handler.on_close_tag(&name);
        }
        self.tagname.clear();
    }

    fn close_current_tag(&mut self) {
        let name = self.tagname.clone();
        self.end_open_tag();
        if self.stack.last() == Some(&name) {
            self.handler.on_close_tag(&name);
            self.stack.pop();
        }
    }
}

impl<H: Handler> TokenSink for Sink<'_, H> {
    fn input(&self) -> &[u8] {
        self.input
    }

    fn ontext(&mut self, start: isize, end: isize) {
        self.flush_surrogate();
        let text = self.slice(start, end);
        if !text.is_empty() {
            self.handler.on_text(&text);
        }
    }

    fn ontextentity(&mut self, cp: u32) {
        if (0xd800..0xdc00).contains(&cp) {
            self.flush_surrogate();
            self.surrogate = Some(cp);
            return;
        }
        let value = match (self.surrogate.take(), cp) {
            (Some(high), 0xdc00..=0xdfff) => 0x10000 + ((high - 0xd800) << 10) + (cp - 0xdc00),
            (Some(_), _) => {
                self.handler.on_text("\u{fffd}");
                cp
            }
            (None, _) => cp,
        };
        let text = char::from_u32(value).unwrap_or('\u{fffd}').to_string();
        self.handler.on_text(&text);
    }

    fn onopentagname(&mut self, start: isize, end: isize) {
        let name = js::to_lower_case(&self.slice(start, end));
        self.emit_open_tag(name);
    }

    fn onopentagend(&mut self, _end: isize) {
        self.end_open_tag();
    }

    fn onselfclosingtag(&mut self, end: isize) {
        if self.foreign_context.last() == Some(&true) {
            self.close_current_tag();
        } else {
            self.onopentagend(end);
        }
    }

    fn onclosetag(&mut self, start: isize, end: isize) {
        let name = js::to_lower_case(&self.slice(start, end));
        if is_foreign_context_element(&name) || is_html_integration_element(&name) {
            self.foreign_context.pop();
        }
        if !is_void_element(&name) {
            if let Some(pos) = self.stack.iter().rposition(|item| *item == name) {
                let mut count = self.stack.len() - pos;
                while count > 0 {
                    count -= 1;
                    let element = self.stack.pop().unwrap_or_default();
                    self.handler.on_close_tag(&element);
                }
            } else if name == "p" {
                self.emit_open_tag("p".to_string());
                self.close_current_tag();
            }
        } else if name == "br" {
            self.handler.on_open_tag("br", &Attributes::default());
            self.handler.on_close_tag("br");
        }
    }

    fn onattribname(&mut self, start: isize, end: isize) {
        self.attribname = js::to_lower_case(&self.slice(start, end));
    }

    fn onattribdata(&mut self, start: isize, end: isize) {
        let text = self.slice(start, end);
        self.attribvalue.push_str(&text);
    }

    fn onattribentity(&mut self, cp: u32) {
        self.attribvalue.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
    }

    fn onattribend(&mut self, _quote: QuoteType, _end: isize) {
        let value = std::mem::take(&mut self.attribvalue);
        if let Some(attribs) = self.attribs.as_mut() {
            attribs.insert_if_absent(self.attribname.clone(), value);
        }
    }

    fn oncomment(&mut self, _start: isize, _end: isize, _offset: isize) {}
    fn oncdata(&mut self, _start: isize, _end: isize, _offset: isize) {}
    fn ondeclaration(&mut self, _start: isize, _end: isize) {}
    fn onprocessinginstruction(&mut self, _start: isize, _end: isize) {}

    fn onend(&mut self) {
        self.flush_surrogate();
        let mut index = self.stack.len();
        while index > 0 {
            index -= 1;
            let name = self.stack[index].clone();
            self.handler.on_close_tag(&name);
        }
    }
}

/// Parse `input` by writing it in chunks of `chunk` UTF-16 code units
/// followed by `end()`, as `parser.write(...)`/`parser.end()` would. A chunk
/// boundary that would split a surrogate pair is moved after the pair.
pub(super) fn parse<H: Handler>(input: &str, chunk: usize, handler: H) -> H {
    let bytes = input.as_bytes();
    let mut sink = Sink {
        input: bytes,
        handler,
        tagname: String::new(),
        attribname: String::new(),
        attribvalue: String::new(),
        attribs: None,
        stack: Vec::new(),
        foreign_context: Vec::new(),
        surrogate: None,
    };
    let mut tokenizer = Tokenizer::new();
    for end in chunk_ends(input, chunk) {
        tokenizer.write(end as isize, &mut sink);
    }
    tokenizer.end(&mut sink);
    sink.handler
}

/// Byte offsets ending each chunk of `chunk` UTF-16 code units.
fn chunk_ends(input: &str, chunk: usize) -> Vec<usize> {
    let mut ends = Vec::new();
    if input.is_ascii() {
        let mut end = 0;
        while end < input.len() {
            end = end.saturating_add(chunk).min(input.len());
            ends.push(end);
        }
        return ends;
    }
    let mut units = 0usize;
    for (index, c) in input.char_indices() {
        if units >= chunk {
            ends.push(index);
            units -= chunk;
        }
        units += c.len_utf16();
    }
    if !input.is_empty() {
        ends.push(input.len());
    }
    ends
}
