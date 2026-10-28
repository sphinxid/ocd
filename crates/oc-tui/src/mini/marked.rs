//! Block lexer port of marked 17.0.1 (`Lexer.blockTokens` + block tokenizers, GFM): the token boundaries
//! and `raw` text OpenTUI's `MarkdownRenderable` builds its render blocks from, plus
//! `parseMarkdownIncremental` (`renderables/markdown-parser.ts`).

use super::marked_rules as rules;
use fancy_regex::Regex;
use once_cell::sync::Lazy;

#[derive(Clone, Debug, PartialEq)]
pub struct ListItem {
    pub raw: String,
    pub task: bool,
    pub checked: Option<bool>,
    pub loose: bool,
    pub text: String,
    pub tokens: Vec<Token>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Space,
    Code { lang: Option<String>, text: String },
    Heading { depth: usize, text: String },
    Hr,
    Blockquote { text: String, tokens: Vec<Token> },
    List { ordered: bool, start: Option<u64>, loose: bool, items: Vec<ListItem> },
    Html { text: String },
    Def,
    Table { header: Vec<String>, align: Vec<Option<&'static str>>, rows: Vec<Vec<String>> },
    Paragraph { text: String },
    Text { text: String },
    Checkbox { checked: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: Kind,
    pub raw: String,
}

impl Token {
    pub fn type_name(&self) -> &'static str {
        match self.kind {
            Kind::Space => "space",
            Kind::Code { .. } => "code",
            Kind::Heading { .. } => "heading",
            Kind::Hr => "hr",
            Kind::Blockquote { .. } => "blockquote",
            Kind::List { .. } => "list",
            Kind::Html { .. } => "html",
            Kind::Def => "def",
            Kind::Table { .. } => "table",
            Kind::Paragraph { .. } => "paragraph",
            Kind::Text { .. } => "text",
            Kind::Checkbox { .. } => "checkbox",
        }
    }
    pub fn text(&self) -> Option<&str> {
        match &self.kind {
            Kind::Code { text, .. } | Kind::Heading { text, .. } | Kind::Blockquote { text, .. } | Kind::Html { text } | Kind::Paragraph { text } | Kind::Text { text } => Some(text),
            _ => None,
        }
    }
}

fn re(s: &str) -> Regex {
    Regex::new(s).unwrap_or_else(|e| panic!("bad regex {s}: {e}"))
}

static BLANK: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"^[ \t]*$").unwrap());
static NEWLINE: Lazy<Regex> = Lazy::new(|| re(rules::NEWLINE));
static CODE: Lazy<Regex> = Lazy::new(|| re(rules::CODE));
static FENCES: Lazy<Regex> = Lazy::new(|| re(rules::FENCES));
static HEADING: Lazy<Regex> = Lazy::new(|| re(rules::HEADING));
static HR: Lazy<Regex> = Lazy::new(|| re(rules::HR));
static BLOCKQUOTE: Lazy<Regex> = Lazy::new(|| re(rules::BLOCKQUOTE));
static LIST: Lazy<Regex> = Lazy::new(|| re(rules::LIST));
static HTML: Lazy<Regex> = Lazy::new(|| re(rules::HTML));
static DEF: Lazy<Regex> = Lazy::new(|| re(rules::DEF));
static TABLE: Lazy<Regex> = Lazy::new(|| re(rules::TABLE));
static LHEADING: Lazy<Regex> = Lazy::new(|| re(rules::LHEADING));
static PARAGRAPH: Lazy<Regex> = Lazy::new(|| re(rules::PARAGRAPH));
static TEXT: Lazy<Regex> = Lazy::new(|| re(rules::TEXT));
static ANY_PUNCT: Lazy<Regex> = Lazy::new(|| re(rules::ANY_PUNCTUATION));

static CODE_REMOVE_INDENT: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"(?m)^(?: {1,4}| {0,3}\t)").unwrap());
static INDENT_CODE_COMP: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"^(\s+)(?:```)").unwrap());
static BLOCKQUOTE_START: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"^ {0,3}>").unwrap());
static BQ_SETEXT: Lazy<Regex> = Lazy::new(|| re(r"\n {0,3}((?:=+|-+) *)(?=\n|$)"));
static BQ_SETEXT2: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"(?m)^ {0,3}>[ \t]?").unwrap());
static LIST_IS_TASK: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"^\[[ xX]\] +\S").unwrap());
static LIST_REPLACE_TASK: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"^\[[ xX]\] +").unwrap());
static LIST_TASK_CHECKBOX: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"\[[ xX]\]").unwrap());
static DOUBLE_BLANK: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"\n[ \t]*\n[ \t]*$").unwrap());
static ANY_LINE: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"\n.*\n").unwrap());

fn exec<'a>(r: &Regex, src: &'a str) -> Option<fancy_regex::Captures<'a>> {
    r.captures(src).ok().flatten()
}

fn cap<'a>(c: &fancy_regex::Captures<'a>, i: usize) -> Option<&'a str> {
    c.get(i).map(|m| m.as_str())
}

fn rtrim(s: &str, c: char) -> &str {
    s.trim_end_matches(c)
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// Indentation of the first non-space character (`search(/[^ ]/)`), -1 when none.
fn non_space(s: &str) -> i64 {
    s.find(|c| c != ' ').map(|i| i as i64).unwrap_or(-1)
}

fn indent_code_compensation(raw: &str, text: &str) -> String {
    let Some(m) = INDENT_CODE_COMP.captures(raw) else { return text.to_string() };
    let indent = m[1].len();
    text.split('\n')
        .map(|node| {
            let lead = node.len() - node.trim_start().len();
            if lead == 0 {
                return node.to_string();
            }
            if lead >= indent { node[indent..].to_string() } else { node.to_string() }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `splitCells(row, count)`.
pub fn split_cells(row: &str, count: Option<usize>) -> Vec<String> {
    let mut out = String::new();
    let chars: Vec<char> = row.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c == '|' {
            let mut escaped = false;
            let mut j = i;
            while j > 0 && chars[j - 1] == '\\' {
                escaped = !escaped;
                j -= 1;
            }
            out.push_str(if escaped { "|" } else { " |" });
        } else {
            out.push(*c);
        }
    }
    let mut cells: Vec<String> = out.split(" |").map(|s| s.to_string()).collect();
    if cells.first().is_some_and(|c| js_trim(c).is_empty()) {
        cells.remove(0);
    }
    if cells.last().is_some_and(|c| js_trim(c).is_empty()) {
        cells.pop();
    }
    if let Some(n) = count {
        if cells.len() > n {
            cells.truncate(n);
        } else {
            while cells.len() < n {
                cells.push(String::new());
            }
        }
    }
    cells.into_iter().map(|c| js_trim(&c).replace("\\|", "|")).collect()
}

pub struct Lexer {
    top: bool,
}

impl Lexer {
    pub fn lex(src: &str) -> Vec<Token> {
        let src = src.replace("\r\n", "\n").replace('\r', "\n");
        let mut l = Lexer { top: true };
        let mut tokens = Vec::new();
        l.block_tokens(&src, &mut tokens, false);
        tokens
    }

    fn block_tokens(&mut self, src: &str, tokens: &mut Vec<Token>, mut last_paragraph_clipped: bool) {
        let mut src = src.to_string();
        while !src.is_empty() {
            if let Some(t) = self.space(&src) {
                src = src[t.raw.len()..].to_string();
                if t.raw.len() == 1
                    && let Some(last) = tokens.last_mut()
                {
                    last.raw.push('\n');
                } else {
                    tokens.push(t);
                }
                continue;
            }
            if let Some(t) = self.code(&src) {
                src = src[t.raw.len()..].to_string();
                match tokens.last_mut() {
                    Some(last) if matches!(last.kind, Kind::Paragraph { .. } | Kind::Text { .. }) => {
                        if !last.raw.ends_with('\n') {
                            last.raw.push('\n');
                        }
                        last.raw.push_str(&t.raw);
                        let add = t.text().unwrap_or_default().to_string();
                        if let Kind::Paragraph { text } | Kind::Text { text } = &mut last.kind {
                            text.push('\n');
                            text.push_str(&add);
                        }
                    }
                    _ => tokens.push(t),
                }
                continue;
            }
            if let Some(t) = self.fences(&src).or_else(|| self.heading(&src)).or_else(|| self.hr(&src)) {
                src = src[t.raw.len()..].to_string();
                tokens.push(t);
                continue;
            }
            if let Some(t) = self.blockquote(&src) {
                src = src[t.raw.len()..].to_string();
                tokens.push(t);
                continue;
            }
            if let Some(t) = self.list(&src) {
                src = src[t.raw.len()..].to_string();
                tokens.push(t);
                continue;
            }
            if let Some(t) = self.html(&src) {
                src = src[t.raw.len()..].to_string();
                tokens.push(t);
                continue;
            }
            if let Some(t) = self.def(&src) {
                src = src[t.raw.len()..].to_string();
                match tokens.last_mut() {
                    Some(last) if matches!(last.kind, Kind::Paragraph { .. } | Kind::Text { .. }) => {
                        if !last.raw.ends_with('\n') {
                            last.raw.push('\n');
                        }
                        last.raw.push_str(&t.raw);
                        if let Kind::Paragraph { text } | Kind::Text { text } = &mut last.kind {
                            text.push('\n');
                            text.push_str(&t.raw);
                        }
                    }
                    _ => tokens.push(t),
                }
                continue;
            }
            if let Some(t) = self.table(&src).or_else(|| self.lheading(&src)) {
                src = src[t.raw.len()..].to_string();
                tokens.push(t);
                continue;
            }
            if self.top
                && let Some(t) = self.paragraph(&src)
            {
                let len = t.raw.len();
                match tokens.last_mut() {
                    Some(last) if last_paragraph_clipped && matches!(last.kind, Kind::Paragraph { .. }) => {
                        if !last.raw.ends_with('\n') {
                            last.raw.push('\n');
                        }
                        last.raw.push_str(&t.raw);
                        let add = t.text().unwrap_or_default().to_string();
                        if let Kind::Paragraph { text } = &mut last.kind {
                            text.push('\n');
                            text.push_str(&add);
                        }
                    }
                    _ => tokens.push(t),
                }
                last_paragraph_clipped = false;
                src = src[len..].to_string();
                continue;
            }
            if let Some(t) = self.text(&src) {
                src = src[t.raw.len()..].to_string();
                match tokens.last_mut() {
                    Some(last) if matches!(last.kind, Kind::Text { .. }) => {
                        if !last.raw.ends_with('\n') {
                            last.raw.push('\n');
                        }
                        last.raw.push_str(&t.raw);
                        let add = t.text().unwrap_or_default().to_string();
                        if let Kind::Text { text } = &mut last.kind {
                            text.push('\n');
                            text.push_str(&add);
                        }
                    }
                    _ => tokens.push(t),
                }
                continue;
            }
            // Infinite-loop guard: consume one character as text.
            let c = src.chars().next().unwrap();
            tokens.push(Token { kind: Kind::Text { text: c.to_string() }, raw: c.to_string() });
            src = src[c.len_utf8()..].to_string();
        }
        self.top = true;
    }

    fn space(&self, src: &str) -> Option<Token> {
        let c = exec(&NEWLINE, src)?;
        let m = cap(&c, 0)?;
        if m.is_empty() {
            return None;
        }
        Some(Token { kind: Kind::Space, raw: m.to_string() })
    }

    fn code(&self, src: &str) -> Option<Token> {
        let c = exec(&CODE, src)?;
        let raw = cap(&c, 0)?.to_string();
        let text = CODE_REMOVE_INDENT.replace_all(&raw, "").to_string();
        Some(Token { kind: Kind::Code { lang: None, text: rtrim(&text, '\n').to_string() }, raw })
    }

    fn fences(&self, src: &str) -> Option<Token> {
        let c = exec(&FENCES, src)?;
        let raw = cap(&c, 0)?.to_string();
        let text = indent_code_compensation(&raw, cap(&c, 3).unwrap_or(""));
        let lang = cap(&c, 2).map(|l| ANY_PUNCT.replace_all(js_trim(l), "$1").to_string());
        Some(Token { kind: Kind::Code { lang, text }, raw })
    }

    fn heading(&self, src: &str) -> Option<Token> {
        let c = exec(&HEADING, src)?;
        let raw = cap(&c, 0)?.to_string();
        let mut text = js_trim(cap(&c, 2).unwrap_or("")).to_string();
        if text.ends_with('#') {
            let trimmed = rtrim(&text, '#');
            if trimmed.is_empty() || trimmed.ends_with(' ') {
                text = js_trim(trimmed).to_string();
            }
        }
        Some(Token { kind: Kind::Heading { depth: cap(&c, 1)?.len(), text }, raw })
    }

    fn hr(&self, src: &str) -> Option<Token> {
        let c = exec(&HR, src)?;
        Some(Token { kind: Kind::Hr, raw: rtrim(cap(&c, 0)?, '\n').to_string() })
    }

    fn blockquote(&mut self, src: &str) -> Option<Token> {
        let c = exec(&BLOCKQUOTE, src)?;
        let whole = rtrim(cap(&c, 0)?, '\n').to_string();
        let mut lines: Vec<String> = whole.split('\n').map(|s| s.to_string()).collect();
        let mut raw = String::new();
        let mut text = String::new();
        let mut tokens: Vec<Token> = Vec::new();
        while !lines.is_empty() {
            let mut in_quote = false;
            let mut current: Vec<String> = Vec::new();
            let mut i = 0;
            while i < lines.len() {
                if BLOCKQUOTE_START.is_match(&lines[i]) {
                    current.push(lines[i].clone());
                    in_quote = true;
                } else if !in_quote {
                    current.push(lines[i].clone());
                } else {
                    break;
                }
                i += 1;
            }
            lines = lines[i..].to_vec();
            let current_raw = current.join("\n");
            let step = BQ_SETEXT.replace_all(&current_raw, "\n    $1").to_string();
            let current_text = BQ_SETEXT2.replace_all(&step, "").to_string();
            raw = if raw.is_empty() { current_raw.clone() } else { format!("{raw}\n{current_raw}") };
            text = if text.is_empty() { current_text.clone() } else { format!("{text}\n{current_text}") };
            let top = self.top;
            self.top = true;
            self.block_tokens(&current_text, &mut tokens, true);
            self.top = top;
            if lines.is_empty() {
                break;
            }
            match tokens.last().map(|t| t.kind.clone()) {
                Some(Kind::Code { .. }) => break,
                Some(Kind::Blockquote { .. }) => {
                    let old = tokens.last().unwrap().clone();
                    let new_text = format!("{}\n{}", old.raw, lines.join("\n"));
                    let new_token = self.blockquote(&new_text)?;
                    raw = format!("{}{}", &raw[..raw.len() - old.raw.len()], new_token.raw);
                    let old_text = old.text().unwrap_or_default().len();
                    text = format!("{}{}", &text[..text.len().saturating_sub(old_text)], new_token.text().unwrap_or_default());
                    *tokens.last_mut().unwrap() = new_token;
                    break;
                }
                Some(Kind::List { .. }) => {
                    let old = tokens.last().unwrap().clone();
                    let new_text = format!("{}\n{}", old.raw, lines.join("\n"));
                    let new_token = self.list(&new_text)?;
                    raw = format!("{}{}", &raw[..raw.len() - old.raw.len()], new_token.raw);
                    text = format!("{}{}", &text[..text.len().saturating_sub(old.raw.len())], new_token.raw);
                    lines = new_text[new_token.raw.len()..].split('\n').map(|s| s.to_string()).collect();
                    *tokens.last_mut().unwrap() = new_token;
                    continue;
                }
                _ => {}
            }
        }
        Some(Token { kind: Kind::Blockquote { text, tokens }, raw })
    }

    fn list(&mut self, src: &str) -> Option<Token> {
        let c = exec(&LIST, src)?;
        let bull = js_trim(cap(&c, 1)?).to_string();
        let ordered = bull.len() > 1;
        let start = if ordered { bull[..bull.len() - 1].parse::<u64>().ok() } else { None };
        let bull_re = if ordered { format!(r"\d{{1,9}}\{}", &bull[bull.len() - 1..]) } else { format!(r"\{bull}") };
        let item_re = re(&format!(r"^( {{0,3}}{bull_re})((?:[\t ][^\n]*)?(?:\n|$))"));
        let mut list_raw = String::new();
        let mut items: Vec<ListItem> = Vec::new();
        let mut loose = false;
        let mut ends_with_blank = false;
        let mut src = src.to_string();
        while !src.is_empty() {
            let mut end_early = false;
            let Some(c) = exec(&item_re, &src) else { break };
            if HR.is_match(&src).unwrap_or(false) {
                break;
            }
            let mut raw = cap(&c, 0).unwrap_or("").to_string();
            let c1 = cap(&c, 1).unwrap_or("").to_string();
            let c2 = cap(&c, 2).unwrap_or("").to_string();
            src = src[raw.len()..].to_string();
            let first_line = c2.split('\n').next().unwrap_or("");
            let lead_tabs = first_line.len() - first_line.trim_start_matches('\t').len();
            let mut line = format!("{}{}", " ".repeat(3 * lead_tabs), &first_line[lead_tabs..]);
            let mut next_line = src.split('\n').next().unwrap_or("").to_string();
            let mut blank_line = js_trim(&line).is_empty();
            let mut indent: i64;
            let mut contents: String;
            if blank_line {
                indent = c1.len() as i64 + 1;
                contents = String::new();
            } else {
                indent = non_space(&c2);
                if indent > 4 {
                    indent = 1;
                }
                contents = line.get(indent.max(0) as usize..).unwrap_or("").to_string();
                indent += c1.len() as i64;
            }
            if blank_line && BLANK.is_match(&next_line) {
                raw.push_str(&next_line);
                raw.push('\n');
                src = src.get(next_line.len() + 1..).unwrap_or("").to_string();
                end_early = true;
            }
            if !end_early {
                let n = indent.clamp(1, 4) - 1;
                let n = n.min(3);
                let next_bullet = re(&format!(r"^ {{0,{n}}}(?:[*+-]|\d{{1,9}}[.)])((?:[ \t][^\n]*)?(?:\n|$))"));
                let hr_re = re(&format!(r"^ {{0,{n}}}((?:- *){{3,}}|(?:_ *){{3,}}|(?:\* *){{3,}})(?:\n+|$)"));
                let fences_begin = re(&format!(r"^ {{0,{n}}}(?:```|~~~)"));
                let heading_begin = re(&format!(r"^ {{0,{n}}}#"));
                let html_begin = re(&format!(r"(?i)^ {{0,{n}}}<(?:[a-z].*>|!--)"));
                while !src.is_empty() {
                    let raw_line = src.split('\n').next().unwrap_or("").to_string();
                    next_line = raw_line.clone();
                    let without_tabs = next_line.replace('\t', "    ");
                    let m = |r: &Regex, s: &str| r.is_match(s).unwrap_or(false);
                    if m(&fences_begin, &next_line) || m(&heading_begin, &next_line) || m(&html_begin, &next_line) || m(&next_bullet, &next_line) || m(&hr_re, &next_line) {
                        break;
                    }
                    if non_space(&without_tabs) >= indent || js_trim(&next_line).is_empty() {
                        contents.push('\n');
                        contents.push_str(without_tabs.get(indent.max(0) as usize..).unwrap_or(""));
                    } else {
                        if blank_line {
                            break;
                        }
                        if non_space(&line.replace('\t', "    ")) >= 4 {
                            break;
                        }
                        if m(&fences_begin, &line) || m(&heading_begin, &line) || m(&hr_re, &line) {
                            break;
                        }
                        contents.push('\n');
                        contents.push_str(&next_line);
                    }
                    if !blank_line && js_trim(&next_line).is_empty() {
                        blank_line = true;
                    }
                    raw.push_str(&raw_line);
                    raw.push('\n');
                    src = src.get(raw_line.len() + 1..).unwrap_or("").to_string();
                    line = without_tabs.get(indent.max(0) as usize..).unwrap_or("").to_string();
                }
            }
            if !loose {
                if ends_with_blank {
                    loose = true;
                } else if DOUBLE_BLANK.is_match(&raw) {
                    ends_with_blank = true;
                }
            }
            let task = LIST_IS_TASK.is_match(&contents);
            list_raw.push_str(&raw);
            items.push(ListItem { raw, task, checked: None, loose: false, text: contents, tokens: vec![] });
        }
        let last = items.last_mut()?;
        last.raw = last.raw.trim_end().to_string();
        last.text = last.text.trim_end().to_string();
        let list_raw = list_raw.trim_end().to_string();
        for item in items.iter_mut() {
            self.top = false;
            let mut toks = Vec::new();
            self.block_tokens(&item.text.clone(), &mut toks, false);
            item.tokens = toks;
            if item.task {
                item.text = LIST_REPLACE_TASK.replace(&item.text, "").to_string();
                if let Some(first) = item.tokens.first_mut()
                    && matches!(first.kind, Kind::Text { .. } | Kind::Paragraph { .. })
                {
                    first.raw = LIST_REPLACE_TASK.replace(&first.raw, "").to_string();
                    if let Kind::Text { text } | Kind::Paragraph { text } = &mut first.kind {
                        *text = LIST_REPLACE_TASK.replace(text, "").to_string();
                    }
                }
                if let Some(m) = LIST_TASK_CHECKBOX.find(&item.raw) {
                    let checked = m.as_str() != "[ ]";
                    item.checked = Some(checked);
                    let cb = Token { kind: Kind::Checkbox { checked }, raw: format!("{} ", m.as_str()) };
                    if loose {
                        match item.tokens.first_mut() {
                            Some(first) if matches!(first.kind, Kind::Text { .. } | Kind::Paragraph { .. }) => {
                                first.raw = format!("{}{}", cb.raw, first.raw);
                                if let Kind::Text { text } | Kind::Paragraph { text } = &mut first.kind {
                                    *text = format!("{}{}", cb.raw, text);
                                }
                            }
                            _ => item.tokens.insert(0, Token { kind: Kind::Paragraph { text: cb.raw.clone() }, raw: cb.raw.clone() }),
                        }
                    } else {
                        item.tokens.insert(0, cb);
                    }
                }
            }
            if !loose {
                let spacers: Vec<&Token> = item.tokens.iter().filter(|t| t.kind == Kind::Space).collect();
                loose = !spacers.is_empty() && spacers.iter().any(|t| ANY_LINE.is_match(&t.raw));
            }
        }
        if loose {
            for item in items.iter_mut() {
                item.loose = true;
                for t in item.tokens.iter_mut() {
                    if let Kind::Text { text } = &t.kind {
                        t.kind = Kind::Paragraph { text: text.clone() };
                    }
                }
            }
        }
        Some(Token { kind: Kind::List { ordered, start, loose, items }, raw: list_raw })
    }

    fn html(&self, src: &str) -> Option<Token> {
        let c = exec(&HTML, src)?;
        let raw = cap(&c, 0)?.to_string();
        Some(Token { kind: Kind::Html { text: raw.clone() }, raw })
    }

    fn def(&self, src: &str) -> Option<Token> {
        let c = exec(&DEF, src)?;
        Some(Token { kind: Kind::Def, raw: cap(&c, 0)?.to_string() })
    }

    fn table(&self, src: &str) -> Option<Token> {
        let c = exec(&TABLE, src)?;
        let c2 = cap(&c, 2)?;
        if !c2.contains([':', '|']) {
            return None;
        }
        let headers = split_cells(cap(&c, 1)?, None);
        let aligns_src = regex::Regex::new(r"^\||\| *$").unwrap().replace_all(c2, "").to_string();
        let aligns: Vec<&str> = aligns_src.split('|').collect();
        let rows: Vec<String> = match cap(&c, 3) {
            Some(r) if !js_trim(r).is_empty() => regex::Regex::new(r"\n[ \t]*$").unwrap().replace(r, "").split('\n').map(|s| s.to_string()).collect(),
            _ => vec![],
        };
        if headers.len() != aligns.len() {
            return None;
        }
        let align = aligns
            .iter()
            .map(|a| {
                if regex::Regex::new(r"^ *-+: *$").unwrap().is_match(a) {
                    Some("right")
                } else if regex::Regex::new(r"^ *:-+: *$").unwrap().is_match(a) {
                    Some("center")
                } else if regex::Regex::new(r"^ *:-+ *$").unwrap().is_match(a) {
                    Some("left")
                } else {
                    None
                }
            })
            .collect();
        let n = headers.len();
        let rows = rows.iter().map(|r| split_cells(r, Some(n))).collect();
        Some(Token { kind: Kind::Table { header: headers, align, rows }, raw: cap(&c, 0)?.to_string() })
    }

    fn lheading(&self, src: &str) -> Option<Token> {
        let c = exec(&LHEADING, src)?;
        let depth = if cap(&c, 2)?.starts_with('=') { 1 } else { 2 };
        Some(Token { kind: Kind::Heading { depth, text: cap(&c, 1)?.to_string() }, raw: cap(&c, 0)?.to_string() })
    }

    fn paragraph(&self, src: &str) -> Option<Token> {
        let c = exec(&PARAGRAPH, src)?;
        let c1 = cap(&c, 1)?;
        let text = c1.strip_suffix('\n').unwrap_or(c1).to_string();
        Some(Token { kind: Kind::Paragraph { text }, raw: cap(&c, 0)?.to_string() })
    }

    fn text(&self, src: &str) -> Option<Token> {
        let c = exec(&TEXT, src)?;
        let raw = cap(&c, 0)?.to_string();
        Some(Token { kind: Kind::Text { text: raw.clone() }, raw })
    }
}

/// `ParseState`: the tokens of the current content and how many leading tokens are stable.
#[derive(Clone, Debug, Default)]
pub struct ParseState {
    pub content: String,
    pub tokens: Vec<Token>,
    pub stable: usize,
}

/// `parseMarkdownIncremental(content, prev, trailingUnstable)`.
pub fn parse_incremental(content: &str, prev: Option<&ParseState>, trailing_unstable: usize) -> ParseState {
    let Some(prev) = prev.filter(|p| !p.tokens.is_empty()) else {
        let tokens = Lexer::lex(content);
        let stable = tokens.len().saturating_sub(trailing_unstable);
        return ParseState { content: content.to_string(), tokens, stable };
    };
    let mut offset = 0;
    let mut reuse: usize = 0;
    for t in &prev.tokens {
        if offset + t.raw.len() <= content.len() && content[offset..].starts_with(&t.raw) {
            reuse += 1;
            offset += t.raw.len();
        } else {
            break;
        }
    }
    reuse = reuse.saturating_sub(trailing_unstable);
    let offset: usize = prev.tokens[..reuse].iter().map(|t| t.raw.len()).sum();
    let mut tokens: Vec<Token> = prev.tokens[..reuse].to_vec();
    let remaining = &content[offset..];
    if remaining.is_empty() {
        return ParseState { content: content.to_string(), stable: tokens.len(), tokens };
    }
    let new = Lexer::lex(remaining);
    let stable = if trailing_unstable == 0 { tokens.len() + new.len() } else { tokens.len() };
    tokens.extend(new);
    ParseState { content: content.to_string(), tokens, stable }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<(String, String)> {
        Lexer::lex(src).into_iter().map(|t| (t.type_name().to_string(), t.raw)).collect()
    }

    fn simplify(t: &Token) -> serde_json::Value {
        let mut v = serde_json::json!({"type": t.type_name(), "raw": t.raw});
        match &t.kind {
            Kind::List { ordered, start, loose, items } => {
                v["ordered"] = (*ordered).into();
                v["start"] = start.map(serde_json::Value::from).unwrap_or_else(|| "".into());
                v["loose"] = (*loose).into();
                v["items"] = items
                    .iter()
                    .map(|i| serde_json::json!({"raw": i.raw, "text": i.text, "task": i.task, "checked": i.checked, "tokens": i.tokens.iter().map(simplify).collect::<Vec<_>>()}))
                    .collect();
            }
            Kind::Code { lang, text } => {
                v["lang"] = lang.clone().into();
                v["text"] = text.clone().into();
            }
            Kind::Heading { depth, text } => {
                v["depth"] = (*depth).into();
                v["text"] = text.clone().into();
            }
            Kind::Table { header, align, rows } => {
                v["header"] = header.clone().into();
                v["rows"] = rows.clone().into();
                v["align"] = align.iter().map(|a| a.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null)).collect();
            }
            Kind::Blockquote { text, tokens } => {
                v["text"] = text.clone().into();
                v["tokens"] = tokens.iter().map(simplify).collect();
            }
            Kind::Paragraph { text } | Kind::Text { text } | Kind::Html { text } => {
                v["text"] = text.clone().into();
            }
            _ => {}
        }
        v
    }

    /// Token streams captured from marked 17.0.1 / OpenTUI's `parseMarkdownIncremental`.
    #[test]
    fn matches_marked_tokens() {
        let fixtures: Vec<serde_json::Value> = serde_json::from_str(include_str!("testdata/marked_tokens.json")).unwrap();
        for f in fixtures {
            if let Some(text) = f.get("text").and_then(|t| t.as_str()) {
                let got: Vec<serde_json::Value> = Lexer::lex(text).iter().map(simplify).collect();
                assert_eq!(serde_json::Value::Array(got), f["tokens"], "tokens of {text:?}");
                continue;
            }
            let chunks: Vec<String> = f["chunks"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_string()).collect();
            let mut state: Option<ParseState> = None;
            let mut content = String::new();
            for (i, chunk) in chunks.iter().enumerate() {
                content.push_str(chunk);
                let next = parse_incremental(&content, state.as_ref(), 2);
                let want = &f["steps"][i];
                let got: Vec<(String, String)> = next.tokens.iter().map(|t| (t.type_name().to_string(), t.raw.clone())).collect();
                let exp: Vec<(String, String)> = want["tokens"].as_array().unwrap().iter().map(|t| (t["type"].as_str().unwrap().to_string(), t["raw"].as_str().unwrap().to_string())).collect();
                assert_eq!(got, exp, "step {i} of {chunks:?}");
                assert_eq!(next.stable as u64, want["stable"].as_u64().unwrap(), "stable at step {i} of {chunks:?}");
                state = Some(next);
            }
            let fin = parse_incremental(&content, state.as_ref(), 0);
            assert_eq!(fin.stable as u64, f["final"]["stable"].as_u64().unwrap());
        }
        let _ = kinds("");
    }
}
