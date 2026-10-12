//! Copied from `oc-config::jsonc_edit` (kept local so oc-tui does not depend on oc-config).
//!
//! Faithful port of the parts of `jsonc-parser` 3.3.1 that opencode uses for
//! comment-preserving JSONC edits: the scanner (`createScanner`), the tolerant
//! tree parser (`parseTree`, `findNodeAtLocation`, `getNodeValue`), the
//! value parser (`parse`), the range formatter (`format`) and the property
//! editor (`modify` = `setProperty` + `withFormatting`, `applyEdits`).
//!
//! Offsets are **byte** offsets into the UTF-8 text (upstream uses UTF-16 code
//! unit offsets); every offset produced lands on a token boundary, which is
//! always a char boundary, so the resulting text is identical to upstream.

use serde_json::{Map, Value};

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

/// `SyntaxKind` with the same numeric values as upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SyntaxKind {
    OpenBraceToken = 1,
    CloseBraceToken = 2,
    OpenBracketToken = 3,
    CloseBracketToken = 4,
    CommaToken = 5,
    ColonToken = 6,
    NullKeyword = 7,
    TrueKeyword = 8,
    FalseKeyword = 9,
    StringLiteral = 10,
    NumericLiteral = 11,
    LineCommentTrivia = 12,
    BlockCommentTrivia = 13,
    LineBreakTrivia = 14,
    Trivia = 15,
    Unknown = 16,
    EOF = 17,
}

/// `ScanError` with the same numeric values as upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ScanError {
    None = 0,
    UnexpectedEndOfComment = 1,
    UnexpectedEndOfString = 2,
    UnexpectedEndOfNumber = 3,
    InvalidUnicode = 4,
    InvalidEscapeCharacter = 5,
    InvalidCharacter = 6,
}

const EOF_CH: u32 = u32::MAX;

/// JSON scanner (`createScanner`). If `ignore_trivia` is set, whitespace and
/// comments are skipped by [`Scanner::scan`].
pub struct Scanner<'a> {
    text: &'a str,
    bytes: &'a [u8],
    ignore_trivia: bool,
    pos: usize,
    value: String,
    token_offset: usize,
    token: SyntaxKind,
    line_number: usize,
    line_start_offset: usize,
    token_line_start_offset: usize,
    prev_token_line_start_offset: usize,
    scan_error: ScanError,
}

/// Creates a JSON scanner on the given text.
pub fn create_scanner(text: &str, ignore_trivia: bool) -> Scanner<'_> {
    Scanner {
        text,
        bytes: text.as_bytes(),
        ignore_trivia,
        pos: 0,
        value: String::new(),
        token_offset: 0,
        token: SyntaxKind::Unknown,
        line_number: 0,
        line_start_offset: 0,
        token_line_start_offset: 0,
        prev_token_line_start_offset: 0,
        scan_error: ScanError::None,
    }
}

fn is_white_space(ch: u32) -> bool {
    ch == 32 || ch == 9
}
fn is_line_break(ch: u32) -> bool {
    ch == 10 || ch == 13
}
fn is_digit(ch: u32) -> bool {
    (48..=57).contains(&ch)
}

impl<'a> Scanner<'a> {
    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn ch(&self, pos: usize) -> u32 {
        self.bytes.get(pos).map(|b| *b as u32).unwrap_or(EOF_CH)
    }

    fn slice(&self, start: usize, end: usize) -> &'a str {
        let len = self.text.len();
        let start = start.min(len);
        let end = end.min(len).max(start);
        &self.text[start..end]
    }

    pub fn set_position(&mut self, new_position: usize) {
        self.pos = new_position;
        self.value.clear();
        self.token_offset = 0;
        self.token = SyntaxKind::Unknown;
        self.scan_error = ScanError::None;
    }

    pub fn get_position(&self) -> usize {
        self.pos
    }

    pub fn scan(&mut self) -> SyntaxKind {
        if self.ignore_trivia {
            loop {
                let result = self.scan_next();
                if !matches!(
                    result,
                    SyntaxKind::LineCommentTrivia
                        | SyntaxKind::BlockCommentTrivia
                        | SyntaxKind::LineBreakTrivia
                        | SyntaxKind::Trivia
                ) {
                    return result;
                }
            }
        }
        self.scan_next()
    }

    pub fn get_token(&self) -> SyntaxKind {
        self.token
    }
    pub fn get_token_value(&self) -> &str {
        &self.value
    }
    pub fn get_token_offset(&self) -> usize {
        self.token_offset
    }
    pub fn get_token_length(&self) -> usize {
        self.pos - self.token_offset
    }
    pub fn get_token_start_line(&self) -> usize {
        self.line_start_offset
    }
    pub fn get_token_start_character(&self) -> usize {
        self.token_offset.saturating_sub(self.prev_token_line_start_offset)
    }
    pub fn get_token_error(&self) -> ScanError {
        self.scan_error
    }

    fn scan_hex_digits(&mut self, count: usize, exact: bool) -> i64 {
        let mut digits = 0;
        let mut value: i64 = 0;
        while digits < count || !exact {
            let ch = self.ch(self.pos);
            if (48..=57).contains(&ch) {
                value = value * 16 + ch as i64 - 48;
            } else if (65..=70).contains(&ch) {
                value = value * 16 + ch as i64 - 65 + 10;
            } else if (97..=102).contains(&ch) {
                value = value * 16 + ch as i64 - 97 + 10;
            } else {
                break;
            }
            self.pos += 1;
            digits += 1;
        }
        if digits < count {
            value = -1;
        }
        value
    }

    fn scan_number(&mut self) -> String {
        let start = self.pos;
        let len = self.len();
        if self.ch(self.pos) == 48 {
            self.pos += 1;
        } else {
            self.pos += 1;
            while self.pos < len && is_digit(self.ch(self.pos)) {
                self.pos += 1;
            }
        }
        if self.pos < len && self.ch(self.pos) == 46 {
            self.pos += 1;
            if self.pos < len && is_digit(self.ch(self.pos)) {
                self.pos += 1;
                while self.pos < len && is_digit(self.ch(self.pos)) {
                    self.pos += 1;
                }
            } else {
                self.scan_error = ScanError::UnexpectedEndOfNumber;
                return self.slice(start, self.pos).to_string();
            }
        }
        let mut end = self.pos;
        if self.pos < len && (self.ch(self.pos) == 69 || self.ch(self.pos) == 101) {
            self.pos += 1;
            // upstream precedence: `pos < len && ch === '+' || ch === '-'`
            if (self.pos < len && self.ch(self.pos) == 43) || self.ch(self.pos) == 45 {
                self.pos += 1;
            }
            if self.pos < len && is_digit(self.ch(self.pos)) {
                self.pos += 1;
                while self.pos < len && is_digit(self.ch(self.pos)) {
                    self.pos += 1;
                }
                end = self.pos;
            } else {
                self.scan_error = ScanError::UnexpectedEndOfNumber;
            }
        }
        self.slice(start, end).to_string()
    }

    fn scan_string(&mut self) -> String {
        // Built as UTF-16 code units so `😀` pairs combine like JS.
        let mut result: Vec<u16> = Vec::new();
        let push = |result: &mut Vec<u16>, s: &str| result.extend(s.encode_utf16());
        let len = self.len();
        let mut start = self.pos;
        loop {
            if self.pos >= len {
                push(&mut result, self.slice(start, self.pos));
                self.scan_error = ScanError::UnexpectedEndOfString;
                break;
            }
            let ch = self.ch(self.pos);
            if ch == 34 {
                push(&mut result, self.slice(start, self.pos));
                self.pos += 1;
                break;
            }
            if ch == 92 {
                push(&mut result, self.slice(start, self.pos));
                self.pos += 1;
                if self.pos >= len {
                    self.scan_error = ScanError::UnexpectedEndOfString;
                    break;
                }
                let ch2 = self.ch(self.pos);
                self.pos += 1;
                match ch2 {
                    34 => result.push(b'"' as u16),
                    92 => result.push(b'\\' as u16),
                    47 => result.push(b'/' as u16),
                    98 => result.push(0x08),
                    102 => result.push(0x0c),
                    110 => result.push(b'\n' as u16),
                    114 => result.push(b'\r' as u16),
                    116 => result.push(b'\t' as u16),
                    117 => {
                        let ch3 = self.scan_hex_digits(4, true);
                        if ch3 >= 0 {
                            result.push(ch3 as u16);
                        } else {
                            self.scan_error = ScanError::InvalidUnicode;
                        }
                    }
                    _ => self.scan_error = ScanError::InvalidEscapeCharacter,
                }
                start = self.pos;
                continue;
            }
            if ch <= 0x1f {
                if is_line_break(ch) {
                    push(&mut result, self.slice(start, self.pos));
                    self.scan_error = ScanError::UnexpectedEndOfString;
                    break;
                } else {
                    self.scan_error = ScanError::InvalidCharacter;
                }
            }
            self.pos += 1;
        }
        String::from_utf16_lossy(&result)
    }

    fn scan_next(&mut self) -> SyntaxKind {
        self.value.clear();
        self.scan_error = ScanError::None;
        self.token_offset = self.pos;
        self.line_start_offset = self.line_number;
        self.prev_token_line_start_offset = self.token_line_start_offset;
        let len = self.len();
        if self.pos >= len {
            self.token_offset = len;
            self.token = SyntaxKind::EOF;
            return self.token;
        }
        let mut code = self.ch(self.pos);
        if is_white_space(code) {
            loop {
                self.pos += 1;
                self.value.push(char::from_u32(code).unwrap_or(' '));
                code = self.ch(self.pos);
                if !is_white_space(code) {
                    break;
                }
            }
            self.token = SyntaxKind::Trivia;
            return self.token;
        }
        if is_line_break(code) {
            self.pos += 1;
            self.value.push(char::from_u32(code).unwrap_or('\n'));
            if code == 13 && self.ch(self.pos) == 10 {
                self.pos += 1;
                self.value.push('\n');
            }
            self.line_number += 1;
            self.token_line_start_offset = self.pos;
            self.token = SyntaxKind::LineBreakTrivia;
            return self.token;
        }
        let token = match code {
            123 => {
                self.pos += 1;
                SyntaxKind::OpenBraceToken
            }
            125 => {
                self.pos += 1;
                SyntaxKind::CloseBraceToken
            }
            91 => {
                self.pos += 1;
                SyntaxKind::OpenBracketToken
            }
            93 => {
                self.pos += 1;
                SyntaxKind::CloseBracketToken
            }
            58 => {
                self.pos += 1;
                SyntaxKind::ColonToken
            }
            44 => {
                self.pos += 1;
                SyntaxKind::CommaToken
            }
            34 => {
                self.pos += 1;
                self.value = self.scan_string();
                SyntaxKind::StringLiteral
            }
            47 => {
                let start = self.pos;
                if self.ch(self.pos + 1) == 47 {
                    self.pos += 2;
                    while self.pos < len {
                        if is_line_break(self.ch(self.pos)) {
                            break;
                        }
                        self.pos += 1;
                    }
                    self.value = self.slice(start, self.pos).to_string();
                    SyntaxKind::LineCommentTrivia
                } else if self.ch(self.pos + 1) == 42 {
                    self.pos += 2;
                    let safe_length = len.saturating_sub(1);
                    let mut closed = false;
                    while self.pos < safe_length {
                        let ch = self.ch(self.pos);
                        if ch == 42 && self.ch(self.pos + 1) == 47 {
                            self.pos += 2;
                            closed = true;
                            break;
                        }
                        self.pos += 1;
                        if is_line_break(ch) {
                            if ch == 13 && self.ch(self.pos) == 10 {
                                self.pos += 1;
                            }
                            self.line_number += 1;
                            self.token_line_start_offset = self.pos;
                        }
                    }
                    if !closed {
                        self.pos += 1;
                        self.scan_error = ScanError::UnexpectedEndOfComment;
                    }
                    self.value = self.slice(start, self.pos).to_string();
                    SyntaxKind::BlockCommentTrivia
                } else {
                    self.value.push('/');
                    self.pos += 1;
                    SyntaxKind::Unknown
                }
            }
            45 | 48..=57 => {
                let mut number = true;
                if code == 45 {
                    self.value.push('-');
                    self.pos += 1;
                    if self.pos == len || !is_digit(self.ch(self.pos)) {
                        number = false;
                    }
                }
                if number {
                    let n = self.scan_number();
                    self.value.push_str(&n);
                    SyntaxKind::NumericLiteral
                } else {
                    SyntaxKind::Unknown
                }
            }
            _ => {
                while self.pos < len && is_unknown_content_character(code) {
                    self.pos += 1;
                    code = self.ch(self.pos);
                }
                if self.token_offset != self.pos {
                    self.value = self.slice(self.token_offset, self.pos).to_string();
                    match self.value.as_str() {
                        "true" => SyntaxKind::TrueKeyword,
                        "false" => SyntaxKind::FalseKeyword,
                        "null" => SyntaxKind::NullKeyword,
                        _ => SyntaxKind::Unknown,
                    }
                } else {
                    // Only reachable for ASCII specials; keep the char intact.
                    let c = self.text[self.pos..].chars().next().unwrap_or('\u{fffd}');
                    self.value.push(c);
                    self.pos += c.len_utf8().max(1);
                    SyntaxKind::Unknown
                }
            }
        };
        self.token = token;
        token
    }
}

fn is_unknown_content_character(code: u32) -> bool {
    if is_white_space(code) || is_line_break(code) {
        return false;
    }
    !matches!(code, 125 | 93 | 123 | 91 | 34 | 58 | 44 | 47)
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// `ParseErrorCode` with the same numeric values as upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ParseErrorCode {
    InvalidSymbol = 1,
    InvalidNumberFormat = 2,
    PropertyNameExpected = 3,
    ValueExpected = 4,
    ColonExpected = 5,
    CommaExpected = 6,
    CloseBraceExpected = 7,
    CloseBracketExpected = 8,
    EndOfFileExpected = 9,
    InvalidCommentToken = 10,
    UnexpectedEndOfComment = 11,
    UnexpectedEndOfString = 12,
    UnexpectedEndOfNumber = 13,
    InvalidUnicode = 14,
    InvalidEscapeCharacter = 15,
    InvalidCharacter = 16,
}

impl ParseErrorCode {
    /// `printParseErrorCode`.
    pub fn name(self) -> &'static str {
        match self {
            Self::InvalidSymbol => "InvalidSymbol",
            Self::InvalidNumberFormat => "InvalidNumberFormat",
            Self::PropertyNameExpected => "PropertyNameExpected",
            Self::ValueExpected => "ValueExpected",
            Self::ColonExpected => "ColonExpected",
            Self::CommaExpected => "CommaExpected",
            Self::CloseBraceExpected => "CloseBraceExpected",
            Self::CloseBracketExpected => "CloseBracketExpected",
            Self::EndOfFileExpected => "EndOfFileExpected",
            Self::InvalidCommentToken => "InvalidCommentToken",
            Self::UnexpectedEndOfComment => "UnexpectedEndOfComment",
            Self::UnexpectedEndOfString => "UnexpectedEndOfString",
            Self::UnexpectedEndOfNumber => "UnexpectedEndOfNumber",
            Self::InvalidUnicode => "InvalidUnicode",
            Self::InvalidEscapeCharacter => "InvalidEscapeCharacter",
            Self::InvalidCharacter => "InvalidCharacter",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError {
    pub error: ParseErrorCode,
    pub offset: usize,
    pub length: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ParseOptions {
    pub disallow_comments: bool,
    pub allow_trailing_comma: bool,
    pub allow_empty_content: bool,
}

/// Callbacks of `visit`. Offsets/lengths are those of the current token.
trait Visitor {
    fn on_object_begin(&mut self, _offset: usize, _length: usize) {}
    fn on_object_property(&mut self, _name: &str, _offset: usize, _length: usize) {}
    fn on_object_end(&mut self, _offset: usize, _length: usize) {}
    fn on_array_begin(&mut self, _offset: usize, _length: usize) {}
    fn on_array_end(&mut self, _offset: usize, _length: usize) {}
    fn on_literal_value(&mut self, _value: Value, _offset: usize, _length: usize) {}
    fn on_separator(&mut self, _sep: char, _offset: usize, _length: usize) {}
    fn on_error(&mut self, _error: ParseErrorCode, _offset: usize, _length: usize) {}
}

struct Visit<'a, 'v, V: Visitor> {
    scanner: Scanner<'a>,
    visitor: &'v mut V,
    options: ParseOptions,
}

/// JS `Number(text)` for a scanned numeric literal, as a JSON value.
fn number_value(text: &str) -> Option<Value> {
    let f: f64 = text.parse().ok()?;
    if !f.is_finite() {
        return None;
    }
    if f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
        if f == 0.0 {
            return Some(Value::from(0));
        }
        return Some(Value::from(f as i64));
    }
    serde_json::Number::from_f64(f).map(Value::Number)
}

impl<'a, 'v, V: Visitor> Visit<'a, 'v, V> {
    fn off(&self) -> usize {
        self.scanner.get_token_offset()
    }
    fn tlen(&self) -> usize {
        self.scanner.get_token_length()
    }

    fn scan_next(&mut self) -> SyntaxKind {
        loop {
            let token = self.scanner.scan();
            match self.scanner.get_token_error() {
                ScanError::InvalidUnicode => self.handle_error(ParseErrorCode::InvalidUnicode, &[], &[]),
                ScanError::InvalidEscapeCharacter => {
                    self.handle_error(ParseErrorCode::InvalidEscapeCharacter, &[], &[])
                }
                ScanError::UnexpectedEndOfNumber => {
                    self.handle_error(ParseErrorCode::UnexpectedEndOfNumber, &[], &[])
                }
                ScanError::UnexpectedEndOfComment => {
                    if !self.options.disallow_comments {
                        self.handle_error(ParseErrorCode::UnexpectedEndOfComment, &[], &[])
                    }
                }
                ScanError::UnexpectedEndOfString => {
                    self.handle_error(ParseErrorCode::UnexpectedEndOfString, &[], &[])
                }
                ScanError::InvalidCharacter => self.handle_error(ParseErrorCode::InvalidCharacter, &[], &[]),
                ScanError::None => {}
            }
            match token {
                SyntaxKind::LineCommentTrivia | SyntaxKind::BlockCommentTrivia => {
                    if self.options.disallow_comments {
                        self.handle_error(ParseErrorCode::InvalidCommentToken, &[], &[]);
                    }
                }
                SyntaxKind::Unknown => self.handle_error(ParseErrorCode::InvalidSymbol, &[], &[]),
                SyntaxKind::Trivia | SyntaxKind::LineBreakTrivia => {}
                _ => return token,
            }
        }
    }

    fn handle_error(&mut self, error: ParseErrorCode, skip_until_after: &[SyntaxKind], skip_until: &[SyntaxKind]) {
        let (o, l) = (self.off(), self.tlen());
        self.visitor.on_error(error, o, l);
        if skip_until_after.len() + skip_until.len() > 0 {
            let mut token = self.scanner.get_token();
            while token != SyntaxKind::EOF {
                if skip_until_after.contains(&token) {
                    self.scan_next();
                    break;
                } else if skip_until.contains(&token) {
                    break;
                }
                token = self.scan_next();
            }
        }
    }

    fn parse_string(&mut self, is_value: bool) -> bool {
        let value = self.scanner.get_token_value().to_string();
        let (o, l) = (self.off(), self.tlen());
        if is_value {
            self.visitor.on_literal_value(Value::String(value), o, l);
        } else {
            self.visitor.on_object_property(&value, o, l);
        }
        self.scan_next();
        true
    }

    fn parse_literal(&mut self) -> bool {
        let (o, l) = (self.off(), self.tlen());
        match self.scanner.get_token() {
            SyntaxKind::NumericLiteral => {
                let text = self.scanner.get_token_value().to_string();
                let value = match number_value(&text) {
                    Some(v) => v,
                    None => {
                        self.handle_error(ParseErrorCode::InvalidNumberFormat, &[], &[]);
                        Value::from(0)
                    }
                };
                self.visitor.on_literal_value(value, o, l);
            }
            SyntaxKind::NullKeyword => self.visitor.on_literal_value(Value::Null, o, l),
            SyntaxKind::TrueKeyword => self.visitor.on_literal_value(Value::Bool(true), o, l),
            SyntaxKind::FalseKeyword => self.visitor.on_literal_value(Value::Bool(false), o, l),
            _ => return false,
        }
        self.scan_next();
        true
    }

    fn parse_property(&mut self) -> bool {
        use SyntaxKind::*;
        if self.scanner.get_token() != StringLiteral {
            self.handle_error(ParseErrorCode::PropertyNameExpected, &[], &[CloseBraceToken, CommaToken]);
            return false;
        }
        self.parse_string(false);
        if self.scanner.get_token() == ColonToken {
            let (o, l) = (self.off(), self.tlen());
            self.visitor.on_separator(':', o, l);
            self.scan_next();
            if !self.parse_value() {
                self.handle_error(ParseErrorCode::ValueExpected, &[], &[CloseBraceToken, CommaToken]);
            }
        } else {
            self.handle_error(ParseErrorCode::ColonExpected, &[], &[CloseBraceToken, CommaToken]);
        }
        true
    }

    fn parse_object(&mut self) -> bool {
        use SyntaxKind::*;
        let (o, l) = (self.off(), self.tlen());
        self.visitor.on_object_begin(o, l);
        self.scan_next();
        let mut needs_comma = false;
        while self.scanner.get_token() != CloseBraceToken && self.scanner.get_token() != EOF {
            if self.scanner.get_token() == CommaToken {
                if !needs_comma {
                    self.handle_error(ParseErrorCode::ValueExpected, &[], &[]);
                }
                let (o, l) = (self.off(), self.tlen());
                self.visitor.on_separator(',', o, l);
                self.scan_next();
                if self.scanner.get_token() == CloseBraceToken && self.options.allow_trailing_comma {
                    break;
                }
            } else if needs_comma {
                self.handle_error(ParseErrorCode::CommaExpected, &[], &[]);
            }
            if !self.parse_property() {
                self.handle_error(ParseErrorCode::ValueExpected, &[], &[CloseBraceToken, CommaToken]);
            }
            needs_comma = true;
        }
        let (o, l) = (self.off(), self.tlen());
        self.visitor.on_object_end(o, l);
        if self.scanner.get_token() != CloseBraceToken {
            self.handle_error(ParseErrorCode::CloseBraceExpected, &[CloseBraceToken], &[]);
        } else {
            self.scan_next();
        }
        true
    }

    fn parse_array(&mut self) -> bool {
        use SyntaxKind::*;
        let (o, l) = (self.off(), self.tlen());
        self.visitor.on_array_begin(o, l);
        self.scan_next();
        let mut needs_comma = false;
        while self.scanner.get_token() != CloseBracketToken && self.scanner.get_token() != EOF {
            if self.scanner.get_token() == CommaToken {
                if !needs_comma {
                    self.handle_error(ParseErrorCode::ValueExpected, &[], &[]);
                }
                let (o, l) = (self.off(), self.tlen());
                self.visitor.on_separator(',', o, l);
                self.scan_next();
                if self.scanner.get_token() == CloseBracketToken && self.options.allow_trailing_comma {
                    break;
                }
            } else if needs_comma {
                self.handle_error(ParseErrorCode::CommaExpected, &[], &[]);
            }
            if !self.parse_value() {
                self.handle_error(ParseErrorCode::ValueExpected, &[], &[CloseBracketToken, CommaToken]);
            }
            needs_comma = true;
        }
        let (o, l) = (self.off(), self.tlen());
        self.visitor.on_array_end(o, l);
        if self.scanner.get_token() != CloseBracketToken {
            self.handle_error(ParseErrorCode::CloseBracketExpected, &[CloseBracketToken], &[]);
        } else {
            self.scan_next();
        }
        true
    }

    fn parse_value(&mut self) -> bool {
        match self.scanner.get_token() {
            SyntaxKind::OpenBracketToken => self.parse_array(),
            SyntaxKind::OpenBraceToken => self.parse_object(),
            SyntaxKind::StringLiteral => self.parse_string(true),
            _ => self.parse_literal(),
        }
    }

    fn run(&mut self) -> bool {
        self.scan_next();
        if self.scanner.get_token() == SyntaxKind::EOF {
            if self.options.allow_empty_content {
                return true;
            }
            self.handle_error(ParseErrorCode::ValueExpected, &[], &[]);
            return false;
        }
        if !self.parse_value() {
            self.handle_error(ParseErrorCode::ValueExpected, &[], &[]);
            return false;
        }
        if self.scanner.get_token() != SyntaxKind::EOF {
            self.handle_error(ParseErrorCode::EndOfFileExpected, &[], &[]);
        }
        true
    }
}

fn visit<V: Visitor>(text: &str, visitor: &mut V, options: ParseOptions) -> bool {
    let mut v = Visit { scanner: create_scanner(text, false), visitor, options };
    v.run()
}

struct ValueBuilder {
    // (container, property of this container inside its parent)
    stack: Vec<(Value, Option<String>)>,
    current_property: Option<String>,
    root: Option<Value>,
    errors: Vec<ParseError>,
}

impl ValueBuilder {
    fn on_value(&mut self, value: Value) {
        match self.stack.last_mut() {
            None => {
                if self.root.is_none() {
                    self.root = Some(value);
                }
            }
            Some((Value::Array(items), _)) => items.push(value),
            Some((Value::Object(map), _)) => {
                if let Some(name) = &self.current_property {
                    map.insert(name.clone(), value);
                }
            }
            Some(_) => {}
        }
    }
    fn end(&mut self) {
        if let Some((value, property)) = self.stack.pop() {
            self.current_property = property;
            self.on_value(value);
        }
    }
}

impl Visitor for ValueBuilder {
    fn on_object_begin(&mut self, _: usize, _: usize) {
        let property = self.current_property.take();
        self.stack.push((Value::Object(Map::new()), property));
    }
    fn on_object_property(&mut self, name: &str, _: usize, _: usize) {
        self.current_property = Some(name.to_string());
    }
    fn on_object_end(&mut self, _: usize, _: usize) {
        self.end();
    }
    fn on_array_begin(&mut self, _: usize, _: usize) {
        let property = self.current_property.take();
        self.stack.push((Value::Array(Vec::new()), property));
    }
    fn on_array_end(&mut self, _: usize, _: usize) {
        self.end();
    }
    fn on_literal_value(&mut self, value: Value, _: usize, _: usize) {
        self.on_value(value);
    }
    fn on_error(&mut self, error: ParseErrorCode, offset: usize, length: usize) {
        self.errors.push(ParseError { error, offset, length });
    }
}

/// `parse(text, errors, options)`: tolerant parse; always check the errors.
/// Returns `None` where upstream returns `undefined`.
pub fn parse(text: &str, options: ParseOptions) -> (Option<Value>, Vec<ParseError>) {
    let mut builder = ValueBuilder { stack: Vec::new(), current_property: None, root: None, errors: Vec::new() };
    visit(text, &mut builder, options);
    while !builder.stack.is_empty() {
        builder.end();
    }
    (builder.root, builder.errors)
}

/// Node kind of a parse tree node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeType {
    Object,
    Array,
    Property,
    String,
    Number,
    Boolean,
    Null,
}

impl NodeType {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeType::Object => "object",
            NodeType::Array => "array",
            NodeType::Property => "property",
            NodeType::String => "string",
            NodeType::Number => "number",
            NodeType::Boolean => "boolean",
            NodeType::Null => "null",
        }
    }
}

pub type NodeId = usize;

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub kind: NodeType,
    pub offset: usize,
    pub length: usize,
    pub colon_offset: Option<usize>,
    /// Leaf value (strings, numbers, booleans, null and property keys).
    pub value: Option<Value>,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
}

/// Arena-backed parse tree.
#[derive(Debug, Clone, PartialEq)]
pub struct Tree {
    pub nodes: Vec<Node>,
    pub root: NodeId,
}

impl Tree {
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }
}

fn node_type(value: &Value) -> NodeType {
    match value {
        Value::Bool(_) => NodeType::Boolean,
        Value::Number(_) => NodeType::Number,
        Value::String(_) => NodeType::String,
        Value::Array(_) => NodeType::Array,
        Value::Object(_) => NodeType::Object,
        Value::Null => NodeType::Null,
    }
}

struct TreeBuilder {
    nodes: Vec<Node>,
    root_children: Vec<NodeId>,
    /// `None` is the artificial root.
    current: Option<NodeId>,
    errors: Vec<ParseError>,
}

impl TreeBuilder {
    fn ensure_property_complete(&mut self, end_offset: usize) {
        if let Some(id) = self.current
            && self.nodes[id].kind == NodeType::Property {
                self.nodes[id].length = end_offset - self.nodes[id].offset;
                self.current = self.nodes[id].parent;
            }
    }
    fn on_value(&mut self, node: Node) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(node);
        match self.current {
            Some(parent) => self.nodes[parent].children.push(id),
            None => self.root_children.push(id),
        }
        id
    }
    fn container(&mut self, kind: NodeType, offset: usize) {
        let node =
            Node { kind, offset, length: 0, colon_offset: None, value: None, parent: self.current, children: vec![] };
        self.current = Some(self.on_value(node));
    }
    fn close(&mut self, offset: usize, length: usize) {
        if let Some(id) = self.current {
            self.nodes[id].length = offset + length - self.nodes[id].offset;
            self.current = self.nodes[id].parent;
        }
    }
}

impl Visitor for TreeBuilder {
    fn on_object_begin(&mut self, offset: usize, _: usize) {
        self.container(NodeType::Object, offset);
    }
    fn on_object_property(&mut self, name: &str, offset: usize, length: usize) {
        self.container(NodeType::Property, offset);
        let prop = self.current.unwrap();
        let id = self.nodes.len();
        self.nodes.push(Node {
            kind: NodeType::String,
            offset,
            length,
            colon_offset: None,
            value: Some(Value::String(name.to_string())),
            parent: Some(prop),
            children: vec![],
        });
        self.nodes[prop].children.push(id);
    }
    fn on_object_end(&mut self, offset: usize, length: usize) {
        self.ensure_property_complete(offset + length);
        self.close(offset, length);
        self.ensure_property_complete(offset + length);
    }
    fn on_array_begin(&mut self, offset: usize, _: usize) {
        self.container(NodeType::Array, offset);
    }
    fn on_array_end(&mut self, offset: usize, length: usize) {
        self.close(offset, length);
        self.ensure_property_complete(offset + length);
    }
    fn on_literal_value(&mut self, value: Value, offset: usize, length: usize) {
        let node = Node {
            kind: node_type(&value),
            offset,
            length,
            colon_offset: None,
            value: Some(value),
            parent: self.current,
            children: vec![],
        };
        self.on_value(node);
        self.ensure_property_complete(offset + length);
    }
    fn on_separator(&mut self, sep: char, offset: usize, _: usize) {
        if let Some(id) = self.current
            && self.nodes[id].kind == NodeType::Property {
                if sep == ':' {
                    self.nodes[id].colon_offset = Some(offset);
                } else if sep == ',' {
                    self.ensure_property_complete(offset);
                }
            }
    }
    fn on_error(&mut self, error: ParseErrorCode, offset: usize, length: usize) {
        self.errors.push(ParseError { error, offset, length });
    }
}

/// `parseTree(text, errors, options)`.
pub fn parse_tree_with(text: &str, options: ParseOptions) -> (Option<Tree>, Vec<ParseError>) {
    let mut builder = TreeBuilder { nodes: Vec::new(), root_children: Vec::new(), current: None, errors: Vec::new() };
    visit(text, &mut builder, options);
    let root = builder.root_children.first().copied();
    let tree = root.map(|root| {
        builder.nodes[root].parent = None;
        Tree { nodes: builder.nodes, root }
    });
    (tree, builder.errors)
}

/// `parseTree(text)` with default options (as `setProperty` uses it).
pub fn parse_tree(text: &str) -> Option<Tree> {
    parse_tree_with(text, ParseOptions::default()).0
}

/// A path segment: property name or array index (`-1` means "append" for
/// [`modify`], like upstream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Key(String),
    Index(isize),
}

impl From<&str> for Segment {
    fn from(value: &str) -> Self {
        Segment::Key(value.to_string())
    }
}
impl From<String> for Segment {
    fn from(value: String) -> Self {
        Segment::Key(value)
    }
}
impl From<isize> for Segment {
    fn from(value: isize) -> Self {
        Segment::Index(value)
    }
}

/// `findNodeAtLocation(root, path)` starting at `node`.
pub fn find_node_at_location(tree: &Tree, node: NodeId, path: &[Segment]) -> Option<NodeId> {
    let mut node = node;
    for segment in path {
        let current = tree.node(node);
        match segment {
            Segment::Key(key) => {
                if current.kind != NodeType::Object {
                    return None;
                }
                let mut found = false;
                for &property in &current.children {
                    let p = tree.node(property);
                    let key_matches = p
                        .children
                        .first()
                        .and_then(|k| tree.node(*k).value.as_ref())
                        .is_some_and(|v| v.as_str() == Some(key.as_str()));
                    if key_matches && p.children.len() == 2 {
                        node = p.children[1];
                        found = true;
                        break;
                    }
                }
                if !found {
                    return None;
                }
            }
            Segment::Index(index) => {
                if current.kind != NodeType::Array || *index < 0 || *index as usize >= current.children.len() {
                    return None;
                }
                node = current.children[*index as usize];
            }
        }
    }
    Some(node)
}

/// `getNodePath(node)`.
pub fn get_node_path(tree: &Tree, node: NodeId) -> Vec<Segment> {
    let Some(parent) = tree.node(node).parent else { return vec![] };
    let mut path = get_node_path(tree, parent);
    let p = tree.node(parent);
    if p.kind == NodeType::Property {
        if let Some(Value::String(key)) = p.children.first().and_then(|k| tree.node(*k).value.clone()) {
            path.push(Segment::Key(key));
        }
    } else if p.kind == NodeType::Array
        && let Some(index) = p.children.iter().position(|c| *c == node) {
            path.push(Segment::Index(index as isize));
        }
    path
}

/// `getNodeValue(node)`; `None` for property nodes (upstream `undefined`).
pub fn get_node_value(tree: &Tree, node: NodeId) -> Option<Value> {
    let n = tree.node(node);
    match n.kind {
        NodeType::Array => {
            Some(Value::Array(n.children.iter().map(|c| get_node_value(tree, *c).unwrap_or(Value::Null)).collect()))
        }
        NodeType::Object => {
            let mut map = Map::new();
            for &prop in &n.children {
                let p = tree.node(prop);
                if let (Some(key), Some(value)) = (p.children.first(), p.children.get(1)) {
                    let key = match &tree.node(*key).value {
                        Some(Value::String(s)) => s.clone(),
                        _ => continue,
                    };
                    if let Some(v) = get_node_value(tree, *value) {
                        map.insert(key, v);
                    }
                }
            }
            Some(Value::Object(map))
        }
        NodeType::Null | NodeType::String | NodeType::Number | NodeType::Boolean => n.value.clone(),
        NodeType::Property => None,
    }
}

// ---------------------------------------------------------------------------
// Format
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub offset: usize,
    pub length: usize,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub offset: usize,
    pub length: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormattingOptions {
    /// `0` behaves like upstream's falsy `tabSize` (defaults to 4).
    pub tab_size: usize,
    pub insert_spaces: bool,
    pub eol: Option<String>,
    pub insert_final_newline: bool,
    pub keep_lines: bool,
}

impl Default for FormattingOptions {
    /// `{ tabSize: 2, insertSpaces: true }`, the options opencode passes.
    fn default() -> Self {
        FormattingOptions { tab_size: 2, insert_spaces: true, eol: None, insert_final_newline: false, keep_lines: false }
    }
}

fn is_eol(text: &str, offset: usize) -> bool {
    matches!(text.as_bytes().get(offset), Some(b'\r') | Some(b'\n'))
}

fn compute_indent_level(content: &str, options: &FormattingOptions) -> usize {
    let tab_size = if options.tab_size == 0 { 4 } else { options.tab_size };
    let mut n = 0;
    for b in content.bytes() {
        match b {
            b' ' => n += 1,
            b'\t' => n += tab_size,
            _ => break,
        }
    }
    n / tab_size
}

fn get_eol(options: &FormattingOptions, text: &str) -> String {
    let bytes = text.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\r' {
            if bytes.get(i + 1) == Some(&b'\n') {
                return "\r\n".into();
            }
            return "\r".into();
        } else if *b == b'\n' {
            return "\n".into();
        }
    }
    options.eol.clone().filter(|e| !e.is_empty()).unwrap_or_else(|| "\n".into())
}

/// `format(documentText, range, options)`: edit operations to format the
/// document (or the lines touching `range`).
pub fn format(document_text: &str, range: Option<Range>, options: &FormattingOptions) -> Vec<Edit> {
    use SyntaxKind::*;
    let (initial_indent_level, format_text, format_text_start, range_start, range_end);
    if let Some(range) = range {
        range_start = range.offset;
        range_end = range_start + range.length;
        let mut fts = range_start;
        while fts > 0 && !is_eol(document_text, fts - 1) {
            fts -= 1;
        }
        let mut end_offset = range_end;
        while end_offset < document_text.len() && !is_eol(document_text, end_offset) {
            end_offset += 1;
        }
        format_text_start = fts;
        format_text = &document_text[fts.min(document_text.len())..end_offset.min(document_text.len())];
        initial_indent_level = compute_indent_level(format_text, options) as isize;
    } else {
        format_text = document_text;
        initial_indent_level = 0;
        format_text_start = 0;
        range_start = 0;
        range_end = document_text.len();
    }
    let has_range = range.is_some();
    let eol = get_eol(options, document_text);
    let tab_size = if options.tab_size == 0 { 4 } else { options.tab_size };
    let indent_value: String = if options.insert_spaces { " ".repeat(tab_size) } else { "\t".into() };

    let mut number_line_breaks: usize = 0;
    let mut indent_level: isize = 0;
    let mut scanner = create_scanner(format_text, false);
    let mut has_error = false;

    let new_lines_and_indent = |number_line_breaks: usize, indent_level: isize| -> String {
        let level = (initial_indent_level + indent_level).max(0) as usize;
        if number_line_breaks > 1 {
            return eol.repeat(number_line_breaks) + &indent_value.repeat(level);
        }
        eol.clone() + &indent_value.repeat(level)
    };

    let scan_next = |scanner: &mut Scanner, number_line_breaks: &mut usize, has_error: &mut bool| {
        let mut token = scanner.scan();
        *number_line_breaks = 0;
        while token == Trivia || token == LineBreakTrivia {
            if token == LineBreakTrivia && options.keep_lines {
                *number_line_breaks += 1;
            } else if token == LineBreakTrivia {
                *number_line_breaks = 1;
            }
            token = scanner.scan();
        }
        *has_error = token == Unknown || scanner.get_token_error() != ScanError::None;
        token
    };

    let mut edits: Vec<Edit> = Vec::new();
    let add_edit = |edits: &mut Vec<Edit>, has_error: bool, text: String, start: usize, end: usize| {
        let in_range = !has_range || (start < range_end && end > range_start);
        let current = document_text.get(start..end).unwrap_or("");
        if !has_error && in_range && current != text {
            edits.push(Edit { offset: start, length: end - start, content: text });
        }
    };

    let mut first_token = scan_next(&mut scanner, &mut number_line_breaks, &mut has_error);
    if options.keep_lines && number_line_breaks > 0 {
        add_edit(&mut edits, has_error, eol.repeat(number_line_breaks), 0, 0);
    }
    if first_token != EOF {
        let first_token_start = scanner.get_token_offset() + format_text_start;
        let initial_indent = indent_value.repeat(initial_indent_level.max(0) as usize);
        add_edit(&mut edits, has_error, initial_indent, format_text_start, first_token_start);
    }
    while first_token != EOF {
        let mut first_token_end = scanner.get_token_offset() + scanner.get_token_length() + format_text_start;
        let mut second_token = scan_next(&mut scanner, &mut number_line_breaks, &mut has_error);
        let mut replace_content = String::new();
        let mut needs_line_break = false;
        while number_line_breaks == 0 && (second_token == LineCommentTrivia || second_token == BlockCommentTrivia) {
            let comment_token_start = scanner.get_token_offset() + format_text_start;
            add_edit(&mut edits, has_error, " ".into(), first_token_end, comment_token_start);
            first_token_end = scanner.get_token_offset() + scanner.get_token_length() + format_text_start;
            needs_line_break = second_token == LineCommentTrivia;
            replace_content =
                if needs_line_break { new_lines_and_indent(number_line_breaks, indent_level) } else { String::new() };
            second_token = scan_next(&mut scanner, &mut number_line_breaks, &mut has_error);
        }
        if second_token == CloseBraceToken {
            if first_token != OpenBraceToken {
                indent_level -= 1;
            }
            if (options.keep_lines && number_line_breaks > 0) || (!options.keep_lines && first_token != OpenBraceToken)
            {
                replace_content = new_lines_and_indent(number_line_breaks, indent_level);
            } else if options.keep_lines {
                replace_content = " ".into();
            }
        } else if second_token == CloseBracketToken {
            if first_token != OpenBracketToken {
                indent_level -= 1;
            }
            if (options.keep_lines && number_line_breaks > 0)
                || (!options.keep_lines && first_token != OpenBracketToken)
            {
                replace_content = new_lines_and_indent(number_line_breaks, indent_level);
            } else if options.keep_lines {
                replace_content = " ".into();
            }
        } else {
            match first_token {
                OpenBracketToken | OpenBraceToken => {
                    indent_level += 1;
                    if (options.keep_lines && number_line_breaks > 0) || !options.keep_lines {
                        replace_content = new_lines_and_indent(number_line_breaks, indent_level);
                    } else {
                        replace_content = " ".into();
                    }
                }
                CommaToken => {
                    if (options.keep_lines && number_line_breaks > 0) || !options.keep_lines {
                        replace_content = new_lines_and_indent(number_line_breaks, indent_level);
                    } else {
                        replace_content = " ".into();
                    }
                }
                LineCommentTrivia => {
                    replace_content = new_lines_and_indent(number_line_breaks, indent_level);
                }
                BlockCommentTrivia => {
                    if number_line_breaks > 0 {
                        replace_content = new_lines_and_indent(number_line_breaks, indent_level);
                    } else if !needs_line_break {
                        replace_content = " ".into();
                    }
                }
                ColonToken => {
                    if options.keep_lines && number_line_breaks > 0 {
                        replace_content = new_lines_and_indent(number_line_breaks, indent_level);
                    } else if !needs_line_break {
                        replace_content = " ".into();
                    }
                }
                StringLiteral => {
                    if options.keep_lines && number_line_breaks > 0 {
                        replace_content = new_lines_and_indent(number_line_breaks, indent_level);
                    } else if second_token == ColonToken && !needs_line_break {
                        replace_content = String::new();
                    }
                }
                NullKeyword | TrueKeyword | FalseKeyword | NumericLiteral | CloseBraceToken | CloseBracketToken => {
                    if options.keep_lines && number_line_breaks > 0 {
                        replace_content = new_lines_and_indent(number_line_breaks, indent_level);
                    } else if (second_token == LineCommentTrivia || second_token == BlockCommentTrivia)
                        && !needs_line_break
                    {
                        replace_content = " ".into();
                    } else if second_token != CommaToken && second_token != EOF {
                        has_error = true;
                    }
                }
                Unknown => has_error = true,
                _ => {}
            }
            if number_line_breaks > 0 && (second_token == LineCommentTrivia || second_token == BlockCommentTrivia) {
                replace_content = new_lines_and_indent(number_line_breaks, indent_level);
            }
        }
        if second_token == EOF {
            if options.keep_lines && number_line_breaks > 0 {
                replace_content = new_lines_and_indent(number_line_breaks, indent_level);
            } else {
                replace_content = if options.insert_final_newline { eol.clone() } else { String::new() };
            }
        }
        let second_token_start = scanner.get_token_offset() + format_text_start;
        add_edit(&mut edits, has_error, replace_content, first_token_end, second_token_start);
        first_token = second_token;
    }
    edits
}

// ---------------------------------------------------------------------------
// Edit
// ---------------------------------------------------------------------------

/// Options of [`modify`] (`ModificationOptions`).
#[derive(Default)]
pub struct ModifyOptions<'a> {
    /// `None` returns the raw edit without formatting.
    pub formatting_options: Option<FormattingOptions>,
    pub is_array_insertion: bool,
    pub get_insertion_index: Option<&'a dyn Fn(&[String]) -> usize>,
}

impl ModifyOptions<'_> {
    /// `{ formattingOptions: { tabSize: 2, insertSpaces: true } }`.
    pub fn formatted() -> Self {
        ModifyOptions { formatting_options: Some(FormattingOptions::default()), ..Default::default() }
    }
}

/// `applyEdit(text, edit)` (single edit).
pub fn apply_edit(text: &str, edit: &Edit) -> String {
    let mut out = String::with_capacity(text.len() + edit.content.len());
    out.push_str(&text[..edit.offset]);
    out.push_str(&edit.content);
    out.push_str(&text[edit.offset + edit.length..]);
    out
}

/// `applyEdits(text, edits)`; errors with `"Overlapping edit"`.
pub fn apply_edits(text: &str, edits: &[Edit]) -> Result<String, String> {
    let mut sorted: Vec<&Edit> = edits.iter().collect();
    sorted.sort_by(|a, b| a.offset.cmp(&b.offset).then(a.length.cmp(&b.length)));
    let mut text = text.to_string();
    let mut last_modified_offset = text.len();
    for e in sorted.iter().rev() {
        if e.offset + e.length <= last_modified_offset {
            text = apply_edit(&text, e);
        } else {
            return Err("Overlapping edit".into());
        }
        last_modified_offset = e.offset;
    }
    Ok(text)
}

fn stringify(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".into())
}

/// `modify(text, path, value, options)` (`setProperty`). `value: None`
/// removes the property / array item.
pub fn modify(
    text: &str,
    path: &[Segment],
    value: Option<&Value>,
    options: &ModifyOptions,
) -> Result<Vec<Edit>, String> {
    let mut path: Vec<Segment> = path.to_vec();
    let tree = parse_tree(text);
    let mut value: Option<Value> = value.cloned();
    let mut parent: Option<NodeId> = None;
    let mut last_segment: Option<Segment> = None;
    while let Some(segment) = path.pop() {
        parent = tree.as_ref().and_then(|t| find_node_at_location(t, t.root, &path));
        let missing = parent.is_none() && value.is_some();
        last_segment = Some(segment.clone());
        if missing {
            let inner = value.take().unwrap();
            value = Some(match segment {
                Segment::Key(key) => {
                    let mut map = Map::new();
                    map.insert(key, inner);
                    Value::Object(map)
                }
                Segment::Index(_) => Value::Array(vec![inner]),
            });
        } else {
            break;
        }
    }

    let Some(parent) = parent else {
        // empty document
        let Some(value) = value else { return Err("Can not delete in empty document".into()) };
        let (offset, length) = match &tree {
            Some(t) => (t.node(t.root).offset, t.node(t.root).length),
            None => (0, 0),
        };
        return Ok(with_formatting(text, Edit { offset, length, content: stringify(&value) }, options));
    };
    let tree = tree.expect("parent implies tree");
    let p = tree.node(parent).clone();
    match (&p.kind, &last_segment) {
        (NodeType::Object, Some(Segment::Key(key))) => {
            let existing = find_node_at_location(&tree, parent, &[Segment::Key(key.clone())]);
            if let Some(existing) = existing {
                match value {
                    None => {
                        let Some(prop) = tree.node(existing).parent else { return Err("Malformed AST".into()) };
                        let property_index = p.children.iter().position(|c| *c == prop).unwrap_or(0);
                        let prop_node = tree.node(prop);
                        let mut remove_end = prop_node.offset + prop_node.length;
                        let remove_begin;
                        if property_index > 0 {
                            let previous = tree.node(p.children[property_index - 1]);
                            remove_begin = previous.offset + previous.length;
                        } else {
                            remove_begin = p.offset + 1;
                            if p.children.len() > 1 {
                                remove_end = tree.node(p.children[1]).offset;
                            }
                        }
                        Ok(with_formatting(
                            text,
                            Edit { offset: remove_begin, length: remove_end - remove_begin, content: String::new() },
                            options,
                        ))
                    }
                    Some(value) => {
                        let e = tree.node(existing);
                        Ok(with_formatting(
                            text,
                            Edit { offset: e.offset, length: e.length, content: stringify(&value) },
                            options,
                        ))
                    }
                }
            } else {
                let Some(value) = value else { return Ok(vec![]) };
                let new_property = format!("{}: {}", stringify(&Value::String(key.clone())), stringify(&value));
                let index = match options.get_insertion_index {
                    Some(f) => {
                        let keys: Vec<String> = p
                            .children
                            .iter()
                            .map(|c| {
                                tree.node(*c)
                                    .children
                                    .first()
                                    .and_then(|k| tree.node(*k).value.as_ref())
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string()
                            })
                            .collect();
                        f(&keys)
                    }
                    None => p.children.len(),
                };
                let edit = if index > 0 {
                    let previous = tree.node(p.children[(index - 1).min(p.children.len().saturating_sub(1))]);
                    Edit { offset: previous.offset + previous.length, length: 0, content: format!(",{new_property}") }
                } else if p.children.is_empty() {
                    Edit { offset: p.offset + 1, length: 0, content: new_property }
                } else {
                    Edit { offset: p.offset + 1, length: 0, content: format!("{new_property},") }
                };
                Ok(with_formatting(text, edit, options))
            }
        }
        (NodeType::Array, Some(Segment::Index(index))) => {
            let insert_index = *index;
            let children = &p.children;
            if insert_index == -1 {
                // `${JSON.stringify(undefined)}` is the text "undefined" upstream.
                let new_property = value.as_ref().map(stringify).unwrap_or_else(|| "undefined".into());
                let edit = if children.is_empty() {
                    Edit { offset: p.offset + 1, length: 0, content: new_property }
                } else {
                    let previous = tree.node(*children.last().unwrap());
                    Edit { offset: previous.offset + previous.length, length: 0, content: format!(",{new_property}") }
                };
                Ok(with_formatting(text, edit, options))
            } else if value.is_none() {
                let removal_index = insert_index;
                let out_of_range = || {
                    format!("Can not remove Array index {insert_index} as length is not sufficient")
                };
                if removal_index < 0 || removal_index as usize >= children.len() {
                    // upstream dereferences an undefined node (TypeError)
                    return Err(out_of_range());
                }
                let removal_index = removal_index as usize;
                let to_remove = tree.node(children[removal_index]);
                let edit = if children.len() == 1 {
                    Edit { offset: p.offset + 1, length: p.length - 2, content: String::new() }
                } else if children.len() - 1 == removal_index {
                    let previous = tree.node(children[removal_index - 1]);
                    let offset = previous.offset + previous.length;
                    let parent_end_offset = p.offset + p.length;
                    Edit { offset, length: parent_end_offset - 2 - offset, content: String::new() }
                } else {
                    Edit {
                        offset: to_remove.offset,
                        length: tree.node(children[removal_index + 1]).offset - to_remove.offset,
                        content: String::new(),
                    }
                };
                Ok(with_formatting(text, edit, options))
            } else {
                let value = value.unwrap();
                let new_property = stringify(&value);
                let last = insert_index.max(0) as usize;
                let edit = if !options.is_array_insertion && children.len() > last && insert_index >= 0 {
                    let to_modify = tree.node(children[last]);
                    Edit { offset: to_modify.offset, length: to_modify.length, content: new_property }
                } else if children.is_empty() || insert_index == 0 {
                    Edit {
                        offset: p.offset + 1,
                        length: 0,
                        content: if children.is_empty() { new_property } else { format!("{new_property},") },
                    }
                } else {
                    let index = if last > children.len() { children.len() } else { last };
                    let previous = tree.node(children[index - 1]);
                    Edit { offset: previous.offset + previous.length, length: 0, content: format!(",{new_property}") }
                };
                Ok(with_formatting(text, edit, options))
            }
        }
        _ => {
            let what = if matches!(last_segment, Some(Segment::Index(_))) { "property" } else { "index" };
            Err(format!("Can not add {what} to parent of type {}", p.kind.as_str()))
        }
    }
}

/// `removeProperty(text, path, options)`.
pub fn remove_property(text: &str, path: &[Segment], options: &ModifyOptions) -> Result<Vec<Edit>, String> {
    modify(text, path, None, options)
}

fn with_formatting(text: &str, edit: Edit, options: &ModifyOptions) -> Vec<Edit> {
    let Some(formatting) = &options.formatting_options else { return vec![edit] };
    let mut new_text = apply_edit(text, &edit);
    let mut begin = edit.offset as isize;
    let mut end = (edit.offset + edit.content.len()) as isize;
    if edit.length == 0 || edit.content.is_empty() {
        while begin > 0 && !is_eol(&new_text, (begin - 1) as usize) {
            begin -= 1;
        }
        while (end as usize) < new_text.len() && !is_eol(&new_text, end as usize) {
            end += 1;
        }
    }
    let formatting = FormattingOptions { keep_lines: false, ..formatting.clone() };
    let edits = format(
        &new_text,
        Some(Range { offset: begin as usize, length: (end - begin) as usize }),
        &formatting,
    );
    for e in edits.iter().rev() {
        new_text = apply_edit(&new_text, e);
        begin = begin.min(e.offset as isize);
        end = end.max((e.offset + e.length) as isize);
        end += e.content.len() as isize - e.length as isize;
    }
    let edit_length = text.len() as isize - (new_text.len() as isize - end) - begin;
    vec![Edit {
        offset: begin as usize,
        length: edit_length as usize,
        content: new_text[begin as usize..end as usize].to_string(),
    }]
}

/// Convenience: `applyEdits(text, modify(text, path, value, options))`.
pub fn apply_modify(
    text: &str,
    path: &[Segment],
    value: Option<&Value>,
    options: &ModifyOptions,
) -> Result<String, String> {
    let edits = modify(text, path, value, options)?;
    apply_edits(text, &edits)
}

#[cfg(test)]
mod tests {
    //! Expected outputs were produced by running jsonc-parser 3.3.1 itself.
    use super::*;
    use serde_json::json;

    fn check(
        text: &str,
        path: &[Segment],
        value: Option<&Value>,
        formatting: Option<FormattingOptions>,
        expected: Result<String, String>,
    ) {
        let options = ModifyOptions { formatting_options: formatting, ..Default::default() };
        assert_eq!(apply_modify(text, path, value, &options), expected);
    }

    #[test]
    fn set_existing_property() {
        check("{\n  \"x\": \"y\"\n}", &[Segment::Key("x".into())], Some(&json!("bar")), Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("{\n  \"x\": \"bar\"\n}".to_string()));
    }

    #[test]
    fn set_root_value() {
        check("true", &[], Some(&json!("bar")), Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("\"bar\"".to_string()));
    }

    #[test]
    fn insert_into_empty_object() {
        check("{}", &[Segment::Key("foo".into())], Some(&json!("bar")), Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("{\n  \"foo\": \"bar\"\n}".to_string()));
    }

    #[test]
    fn insert_into_object_with_newline() {
        check("{\n}", &[Segment::Key("foo".into())], Some(&json!("bar")), Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("{\n  \"foo\": \"bar\"\n}".to_string()));
    }

    #[test]
    fn insert_after_existing() {
        check("{\n  \"x\": \"y\"\n}", &[Segment::Key("foo".into())], Some(&json!("bar")), Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("{\n  \"x\": \"y\",\n  \"foo\": \"bar\"\n}".to_string()));
    }

    #[test]
    fn insert_nested_path_creation() {
        check("{\n  \"x\": \"y\"\n}", &[Segment::Key("a".into()), Segment::Key("b".into()), Segment::Key("c".into())], Some(&json!(1)), Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("{\n  \"x\": \"y\",\n  \"a\": {\n    \"b\": {\n      \"c\": 1\n    }\n  }\n}".to_string()));
    }

    #[test]
    fn insert_into_empty_document() {
        check("", &[Segment::Key("shell".into())], Some(&json!("/bin/zsh")), Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("{\n  \"shell\": \"/bin/zsh\"\n}".to_string()));
    }

    #[test]
    fn insert_object_value() {
        check("{\n  \"x\": \"y\"\n}", &[Segment::Key("obj".into())], Some(&json!({"a":[1,true],"b":null})), Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("{\n  \"x\": \"y\",\n  \"obj\": {\n    \"a\": [\n      1,\n      true\n    ],\n    \"b\": null\n  }\n}".to_string()));
    }

    #[test]
    fn remove_first_property() {
        check("{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}", &[Segment::Key("a".into())], None, Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("{\n  \"b\": 2,\n  \"c\": 3\n}".to_string()));
    }

    #[test]
    fn remove_middle_property() {
        check("{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}", &[Segment::Key("b".into())], None, Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("{\n  \"a\": 1,\n  \"c\": 3\n}".to_string()));
    }

    #[test]
    fn remove_last_property() {
        check("{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}", &[Segment::Key("c".into())], None, Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("{\n  \"a\": 1,\n  \"b\": 2\n}".to_string()));
    }

    #[test]
    fn remove_only_property() {
        check("{\n  \"a\": 1\n}", &[Segment::Key("a".into())], None, Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("{\n}".to_string()));
    }

    #[test]
    fn remove_missing_property_is_noop() {
        check("{\n  \"a\": 1\n}", &[Segment::Key("zz".into())], None, Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("{\n  \"a\": 1\n}".to_string()));
    }

    #[test]
    fn remove_nested_with_comments() {
        check("{\n  // keep\n  \"terminal\": {\n    \"copy_on_select\": true // gone\n  },\n  /* tail */\n  \"x\": 1\n}", &[Segment::Key("terminal".into()), Segment::Key("copy_on_select".into())], None, Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("{\n  // keep\n  \"terminal\": { // gone\n  },\n  /* tail */\n  \"x\": 1\n}".to_string()));
    }

    #[test]
    fn set_preserves_comments() {
        check("// head\n{\n  // c\n  \"a\": 1, // tail\n  /* b */ \"b\": [1, 2, 3]\n}\n", &[Segment::Key("a".into())], Some(&json!(2)), Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("// head\n{\n  // c\n  \"a\": 2, // tail\n  /* b */ \"b\": [1, 2, 3]\n}\n".to_string()));
    }

    #[test]
    fn insert_preserves_comments() {
        check("// head\n{\n  // c\n  \"a\": 1 // tail\n}\n", &[Segment::Key("shell".into())], Some(&json!("fish")), Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("// head\n{\n  // c\n  \"a\": 1,\n  \"shell\": \"fish\" // tail\n}\n".to_string()));
    }

    #[test]
    fn array_append() {
        check("{\n  \"x\": [1, 2]\n}", &[Segment::Key("x".into()), Segment::Index(-1)], Some(&json!(3)), Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("{\n  \"x\": [\n    1,\n    2,\n    3\n  ]\n}".to_string()));
    }

    #[test]
    fn array_append_empty() {
        check("{\n  \"x\": []\n}", &[Segment::Key("x".into()), Segment::Index(-1)], Some(&json!(3)), Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("{\n  \"x\": [\n    3\n  ]\n}".to_string()));
    }

    #[test]
    fn array_set_index() {
        check("[1, 2, 3]", &[Segment::Index(1)], Some(&json!("two")), Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("[1, \"two\", 3]".to_string()));
    }

    #[test]
    fn array_remove_first() {
        check("[1, 2, 3]", &[Segment::Index(0)], None, Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("[\n  2,\n  3\n]".to_string()));
    }

    #[test]
    fn array_remove_middle() {
        check("[1, 2, 3]", &[Segment::Index(1)], None, Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("[\n  1,\n  3\n]".to_string()));
    }

    #[test]
    fn array_remove_last() {
        check("[1, 2, 3]", &[Segment::Index(2)], None, Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("[\n  1,\n  23\n]".to_string()));
    }

    #[test]
    fn array_remove_only() {
        check("[1]", &[Segment::Index(0)], None, Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("[]".to_string()));
    }

    #[test]
    fn tabs_formatting() {
        check("{\n\t\"a\": {\n\t\t\"b\": true\n\t}\n}", &[Segment::Key("a".into()), Segment::Key("c".into())], Some(&json!(false)), Some(FormattingOptions { tab_size: 4, insert_spaces: false, ..Default::default() }), Ok("{\n\t\"a\": {\n\t\t\"b\": true,\n\t\t\"c\": false\n\t}\n}".to_string()));
    }

    #[test]
    fn crlf_eol() {
        check("{\r\n  \"a\": 1\r\n}", &[Segment::Key("b".into())], Some(&json!(2)), Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("{\r\n  \"a\": 1,\r\n  \"b\": 2\r\n}".to_string()));
    }

    #[test]
    fn trailing_comma_object() {
        check("{\"keybinds\": {\"app_exit\": \"ctrl+c\", \"leader\": \"ctrl+x\",}}", &[Segment::Key("keybinds".into()), Segment::Key("leader".into())], None, Some(FormattingOptions { tab_size: 2, insert_spaces: true, ..Default::default() }), Ok("{\n  \"keybinds\": {\n    \"app_exit\": \"ctrl+c\",\n  }\n}".to_string()));
    }

    #[test]
    fn unformatted_raw_edit() {
        check("{\"a\":1}", &[Segment::Key("b".into())], Some(&json!(2)), None, Ok("{\"a\":1,\"b\": 2}".to_string()));
    }

    #[test]
    fn format_minified() {
        let options = FormattingOptions { tab_size: 2, insert_spaces: true, insert_final_newline: false, keep_lines: false, eol: None };
        let text = "{\"a\":1,\"b\":[1,2,{\"c\":3}],\"d\":{}}";
        assert_eq!(apply_edits(text, &format(text, None, &options)).unwrap(), "{\n  \"a\": 1,\n  \"b\": [\n    1,\n    2,\n    {\n      \"c\": 3\n    }\n  ],\n  \"d\": {}\n}");
    }

    #[test]
    fn format_keeps_comments() {
        let options = FormattingOptions { tab_size: 2, insert_spaces: true, insert_final_newline: false, keep_lines: false, eol: None };
        let text = "{\"a\":1, // x\n/* y */ \"b\":2}";
        assert_eq!(apply_edits(text, &format(text, None, &options)).unwrap(), "{\n  \"a\": 1, // x\n  /* y */ \"b\": 2\n}");
    }

    #[test]
    fn format_range_only() {
        let options = FormattingOptions { tab_size: 2, insert_spaces: true, insert_final_newline: false, keep_lines: false, eol: None };
        let text = "{\"a\":1,\n\"b\":{\"c\":2}}";
        assert_eq!(apply_edits(text, &format(text, Some(Range { offset: 7, length: 5 }), &options)).unwrap(), "{\"a\":1,\n  \"b\":{\"c\":2}}");
    }

    #[test]
    fn format_insert_final_newline() {
        let options = FormattingOptions { tab_size: 2, insert_spaces: true, insert_final_newline: true, keep_lines: false, eol: None };
        let text = "{\"a\":1}";
        assert_eq!(apply_edits(text, &format(text, None, &options)).unwrap(), "{\n  \"a\": 1\n}\n");
    }

    #[test]
    fn format_keep_lines() {
        let options = FormattingOptions { tab_size: 2, insert_spaces: true, insert_final_newline: false, keep_lines: true, eol: None };
        let text = "{\n\"a\":1,\n\n\n\"b\":2}";
        assert_eq!(apply_edits(text, &format(text, None, &options)).unwrap(), "{\n  \"a\": 1,\n\n\n  \"b\": 2 }");
    }

    #[test]
    fn format_tabs() {
        let options = FormattingOptions { tab_size: 4, insert_spaces: false, insert_final_newline: false, keep_lines: false, eol: None };
        let text = "{\"a\":{\"b\":[1]}}";
        assert_eq!(apply_edits(text, &format(text, None, &options)).unwrap(), "{\n\t\"a\": {\n\t\t\"b\": [\n\t\t\t1\n\t\t]\n\t}\n}");
    }

    #[test]
    fn format_error_stops_edits() {
        let options = FormattingOptions { tab_size: 2, insert_spaces: true, insert_final_newline: false, keep_lines: false, eol: None };
        let text = "{\"a\": tru, \"b\":1}";
        assert_eq!(apply_edits(text, &format(text, None, &options)).unwrap(), "{\n  \"a\": tru,\n  \"b\": 1\n}");
    }

    #[test]
    fn delete_in_empty_document_errors() {
        let err = modify("", &[Segment::Key("a".into())], None, &ModifyOptions::formatted()).unwrap_err();
        assert_eq!(err, "Can not delete in empty document");
    }

    #[test]
    fn add_index_to_object_errors() {
        let err = modify("{\"a\": 1}", &[Segment::Index(0)], Some(&json!(1)), &ModifyOptions::formatted()).unwrap_err();
        assert_eq!(err, "Can not add property to parent of type object");
        let err = modify("[1]", &[Segment::Key("x".into())], Some(&json!(1)), &ModifyOptions::formatted()).unwrap_err();
        assert_eq!(err, "Can not add index to parent of type array");
    }

    #[test]
    fn insertion_index_controls_position() {
        let first = |_: &[String]| 0usize;
        let options = ModifyOptions {
            formatting_options: Some(FormattingOptions::default()),
            get_insertion_index: Some(&first),
            ..Default::default()
        };
        let out = apply_modify("{\n  \"b\": 1\n}", &[Segment::Key("a".into())], Some(&json!(0)), &options).unwrap();
        assert_eq!(out, "{\n  \"a\": 0,\n  \"b\": 1\n}");
    }

    #[test]
    fn array_insertion_mode_inserts() {
        let options = ModifyOptions {
            formatting_options: Some(FormattingOptions::default()),
            is_array_insertion: true,
            ..Default::default()
        };
        let out = apply_modify("[1, 2]", &[Segment::Index(1)], Some(&json!(9)), &options).unwrap();
        assert_eq!(out, "[\n  1,\n  9,\n  2\n]");
    }

    #[test]
    fn apply_edits_rejects_overlap() {
        let edits = vec![
            Edit { offset: 0, length: 3, content: "x".into() },
            Edit { offset: 1, length: 1, content: "y".into() },
        ];
        assert_eq!(apply_edits("abcd", &edits).unwrap_err(), "Overlapping edit");
    }

    #[test]
    fn scanner_finds_comma_between_offsets() {
        // Mirrors cli/src/config/migrate.ts findComma.
        let text = "{\"a\": 1 /* , */ , \"b\": 2}";
        let mut scanner = create_scanner(text, false);
        scanner.set_position(7);
        let mut found = None;
        loop {
            scanner.scan();
            let offset = scanner.get_token_offset();
            if scanner.get_token_length() == 0 || offset >= 18 {
                break;
            }
            if text.as_bytes()[offset] == b',' {
                found = Some(offset);
                break;
            }
        }
        assert_eq!(found, Some(16));
    }

    #[test]
    fn scanner_tokens_and_values() {
        let mut scanner = create_scanner("{\"a\\u00e9\": -1.5e3, // c\n true}", true);
        let mut tokens = vec![];
        loop {
            let token = scanner.scan();
            tokens.push((token, scanner.get_token_value().to_string()));
            if token == SyntaxKind::EOF {
                break;
            }
        }
        assert_eq!(
            tokens,
            vec![
                (SyntaxKind::OpenBraceToken, "".into()),
                (SyntaxKind::StringLiteral, "a\u{e9}".into()),
                (SyntaxKind::ColonToken, "".into()),
                (SyntaxKind::NumericLiteral, "-1.5e3".into()),
                (SyntaxKind::CommaToken, "".into()),
                (SyntaxKind::TrueKeyword, "true".into()),
                (SyntaxKind::CloseBraceToken, "".into()),
                (SyntaxKind::EOF, "".into()),
            ]
        );
    }

    #[test]
    fn surrogate_pair_escapes_combine() {
        let (value, errors) = parse("\"\\ud83d\\ude00\"", ParseOptions::default());
        assert!(errors.is_empty());
        assert_eq!(value, Some(json!("\u{1F600}")));
    }

    #[test]
    fn parse_tolerates_comments_and_trailing_commas() {
        let options = ParseOptions { allow_trailing_comma: true, ..Default::default() };
        let (value, errors) = parse("// c\n{\"a\": [1, 2,], /* x */ \"b\": {\"c\": null,},}", options);
        assert!(errors.is_empty());
        assert_eq!(value, Some(json!({"a": [1, 2], "b": {"c": null}})));
        let (_, errors) = parse("{\"a\": 1,}", ParseOptions::default());
        assert_eq!(errors.iter().map(|e| e.error).collect::<Vec<_>>(), vec![ParseErrorCode::PropertyNameExpected, ParseErrorCode::ValueExpected]);
        let (value, errors) = parse("", ParseOptions::default());
        assert_eq!(value, None);
        assert_eq!(errors[0].error, ParseErrorCode::ValueExpected);
    }

    #[test]
    fn duplicate_keys_keep_first_position_last_value() {
        let (value, _) = parse("{\"a\": 1, \"b\": 2, \"a\": 3}", ParseOptions::default());
        assert_eq!(serde_json::to_string(&value.unwrap()).unwrap(), "{\"a\":3,\"b\":2}");
    }

    #[test]
    fn tree_offsets_and_lookup() {
        let text = "{\n  \"keybinds\": {\"x\": 1, \"y\": [true]}\n}";
        let tree = parse_tree(text).unwrap();
        let root = tree.node(tree.root);
        assert_eq!((root.kind, root.offset, root.length), (NodeType::Object, 0, text.len()));
        let y = find_node_at_location(&tree, tree.root, &["keybinds".into(), "y".into(), Segment::Index(0)]).unwrap();
        assert_eq!(tree.node(y).value, Some(json!(true)));
        assert_eq!(get_node_path(&tree, y), vec![Segment::Key("keybinds".into()), "y".into(), Segment::Index(0)]);
        let property = tree.node(tree.node(tree.root).children[0]);
        assert_eq!(property.kind, NodeType::Property);
        assert_eq!(property.colon_offset, Some(14));
        assert_eq!(get_node_value(&tree, tree.root), Some(json!({"keybinds": {"x": 1, "y": [true]}})));
        assert!(find_node_at_location(&tree, tree.root, &["missing".into()]).is_none());
    }

    #[test]
    fn multibyte_text_uses_byte_offsets() {
        let text = "{\n  \"a\": \"é漢\"\n}";
        let out = apply_modify(text, &["b".into()], Some(&json!("ü")), &ModifyOptions::formatted()).unwrap();
        assert_eq!(out, "{\n  \"a\": \"é漢\",\n  \"b\": \"ü\"\n}");
        let out = apply_modify(text, &["a".into()], None, &ModifyOptions::formatted()).unwrap();
        assert_eq!(out, "{\n}");
    }
}
