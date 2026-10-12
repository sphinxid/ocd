//! Early errors for regular expression patterns, reported with the texts JavaScriptCore's Yarr parser uses (the
//! upstream host engine): `new RegExp("(")` fails with "missing )", `"[b-a]"` with "range out of order in character
//! class", and so on. The walk follows Yarr's parse order so the first error found is the one JSC reports; matching
//! itself is left to `regress`. Also collects the capture-group names the match results need.

use std::collections::HashMap;

/// Yarr's error texts (`YarrErrorCode.cpp`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    QuantifierOutOfOrder,
    QuantifierWithoutAtom,
    QuantifierIncomplete,
    CantQuantifyAtom,
    MissingParentheses,
    BracketUnmatched,
    ParenthesesUnmatched,
    ParenthesesTypeInvalid,
    InvalidGroupName,
    DuplicateGroupName,
    CharacterClassUnmatched,
    CharacterClassRangeOutOfOrder,
    CharacterClassRangeInvalid,
    ClassStringDisjunctionUnmatched,
    EscapeUnterminated,
    InvalidUnicodeEscape,
    InvalidUnicodeCodePointEscape,
    InvalidBackreference,
    InvalidNamedBackReference,
    InvalidIdentityEscape,
    InvalidOctalEscape,
    InvalidControlLetterEscape,
    InvalidUnicodePropertyExpression,
    InvalidClassSetOperation,
    NegatedClassSetMayContainStrings,
    InvalidClassSetCharacter,
    InvalidRegularExpressionModifier,
    TooManyDisjunctions,
}

impl ErrorCode {
    pub fn message(self) -> &'static str {
        match self {
            ErrorCode::QuantifierOutOfOrder => "numbers out of order in {} quantifier",
            ErrorCode::QuantifierWithoutAtom => "nothing to repeat",
            ErrorCode::QuantifierIncomplete => "incomplete {} quantifier for Unicode pattern",
            ErrorCode::CantQuantifyAtom => "invalid quantifier",
            ErrorCode::MissingParentheses => "missing )",
            ErrorCode::BracketUnmatched => "unmatched ] or } bracket for Unicode pattern",
            ErrorCode::ParenthesesUnmatched => "unmatched parentheses",
            ErrorCode::ParenthesesTypeInvalid => "unrecognized character after (?",
            ErrorCode::InvalidGroupName => "invalid group specifier name",
            ErrorCode::DuplicateGroupName => "duplicate group specifier name",
            ErrorCode::CharacterClassUnmatched => "missing terminating ] for character class",
            ErrorCode::CharacterClassRangeOutOfOrder => "range out of order in character class",
            ErrorCode::CharacterClassRangeInvalid => "invalid range in character class for Unicode pattern",
            ErrorCode::ClassStringDisjunctionUnmatched => "missing terminating } for class string disjunction",
            ErrorCode::EscapeUnterminated => "\\ at end of pattern",
            ErrorCode::InvalidUnicodeEscape => "invalid Unicode \\u escape",
            ErrorCode::InvalidUnicodeCodePointEscape => "invalid Unicode code point \\u{} escape",
            ErrorCode::InvalidBackreference => "invalid backreference for Unicode pattern",
            ErrorCode::InvalidNamedBackReference => "invalid \\k<> named backreference",
            ErrorCode::InvalidIdentityEscape => "invalid escaped character for Unicode pattern",
            ErrorCode::InvalidOctalEscape => "invalid octal escape for Unicode pattern",
            ErrorCode::InvalidControlLetterEscape => "invalid \\c escape for Unicode pattern",
            ErrorCode::InvalidUnicodePropertyExpression => "invalid property expression",
            ErrorCode::InvalidClassSetOperation => "invalid operation in class set",
            ErrorCode::NegatedClassSetMayContainStrings => "negated class set may contain strings",
            ErrorCode::InvalidClassSetCharacter => "invalid class set character",
            ErrorCode::InvalidRegularExpressionModifier => "invalid regular expression modifier",
            ErrorCode::TooManyDisjunctions => "too many nested disjunctions",
        }
    }
}

type R<T> = Result<T, ErrorCode>;

/// What a successful validation learned about the pattern.
#[derive(Debug, Default, Clone)]
pub struct PatternInfo {
    /// One entry per capture group, in pattern order: its name, if any.
    pub group_names: Vec<Option<String>>,
    /// Each `\k<name>` backreference outside a class: its span in [`pattern_chars`] and the name.
    pub named_refs: Vec<(usize, usize, String)>,
    /// Positions of literal `&` operands in v-mode classes (the matcher reads them escaped).
    pub class_ampersands: Vec<usize>,
    /// Every group name (in `(?<name>` and `\k<name>`): its span through the closing `>`, and the name.
    pub name_spans: Vec<(usize, usize, String)>,
}

impl PatternInfo {
    pub fn has_named_groups(&self) -> bool {
        self.group_names.iter().any(|n| n.is_some())
    }
}

/// The pattern as the matcher reads it: code points in Unicode mode, code units otherwise.
pub fn pattern_chars(units: &[u16], unicode: bool) -> Vec<u32> {
    if !unicode {
        return units.iter().map(|&u| u as u32).collect();
    }
    let mut out = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let c = units[i];
        if (0xD800..0xDC00).contains(&c) && i + 1 < units.len() && (0xDC00..0xE000).contains(&units[i + 1]) {
            out.push(0x10000 + (((c as u32) - 0xD800) << 10) + ((units[i + 1] as u32) - 0xDC00));
            i += 2;
        } else {
            out.push(c as u32);
            i += 1;
        }
    }
    out
}

const INFINITE: u64 = u32::MAX as u64;
const MAX_NESTING: usize = 1000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    Capture,
    NonCapture,
    Lookahead,
    Lookbehind,
    Modifier,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Token {
    None,
    Atom,
    Lookbehind,
}

/// Duplicate-name bookkeeping for one disjunction: names may repeat only across alternatives.
#[derive(Default)]
struct NameFrame {
    current: Vec<String>,
    done: Vec<String>,
}

/// A class atom outside v-mode: one character or a built-in class such as `\d`.
enum ClassAtom {
    Char(u32),
    Builtin,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClassState {
    Empty,
    CachedCharacter(u32),
    CachedCharacterHyphen(u32),
    AfterCharacterClass,
    AfterCharacterClassHyphen,
}

struct Parser<'a> {
    s: &'a [u32],
    i: usize,
    unicode: bool,
    sets: bool,
    has_named: bool,
    total_groups: usize,
    names: Vec<Option<String>>,
    stack: Vec<Group>,
    frames: Vec<NameFrame>,
    max_backref: u64,
    named_refs: Vec<String>,
    ref_spans: Vec<(usize, usize, String)>,
    ampersands: Vec<usize>,
    name_spans: Vec<(usize, usize, String)>,
}

fn is_digit(c: u32) -> bool {
    (b'0' as u32..=b'9' as u32).contains(&c)
}

fn hex_value(c: u32) -> Option<u32> {
    char::from_u32(c).and_then(|ch| ch.to_digit(16))
}

fn is_syntax_char(c: u32) -> bool {
    matches!(char::from_u32(c), Some('^' | '$' | '\\' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'))
}

fn ch(c: char) -> u32 {
    c as u32
}

/// Whether `\p{...}`/`\P{...}` names a property; `Some(true)` for a property of strings (v-mode only).
fn property_kind(body: &[u32], sets: bool) -> Option<bool> {
    type Cache = HashMap<(Vec<u32>, bool), Option<bool>>;
    thread_local! {
        static CACHE: std::cell::RefCell<Cache> = std::cell::RefCell::new(HashMap::new());
    }
    let key = (body.to_vec(), sets);
    if let Some(hit) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return hit;
    }
    let build = |negated: bool, flags: &str| {
        let mut p: Vec<u32> = vec![ch('\\'), ch(if negated { 'P' } else { 'p' }), ch('{')];
        p.extend_from_slice(body);
        p.push(ch('}'));
        regress::Regex::from_unicode(p.into_iter(), flags).is_ok()
    };
    let result = if build(false, "u") {
        Some(false)
    } else if sets && build(false, "v") {
        Some(true)
    } else {
        None
    };
    CACHE.with(|c| c.borrow_mut().insert(key, result));
    result
}

impl Parser<'_> {
    fn at_end(&self) -> bool {
        self.i >= self.s.len()
    }
    fn peek(&self) -> Option<u32> {
        self.s.get(self.i).copied()
    }
    fn peek_at(&self, k: usize) -> Option<u32> {
        self.s.get(self.i + k).copied()
    }
    fn peek_is(&self, c: char) -> bool {
        self.peek() == Some(c as u32)
    }
    fn try_consume(&mut self, c: char) -> bool {
        if self.peek_is(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn peek_is_digit(&self) -> bool {
        self.peek().is_some_and(is_digit)
    }
    fn consume_number(&mut self) -> u64 {
        let mut n: u64 = 0;
        while let Some(c) = self.peek() {
            if !is_digit(c) {
                break;
            }
            n = (n * 10 + (c - ch('0')) as u64).min(INFINITE);
            self.i += 1;
        }
        n
    }

    /// Prescan: whether any `(?<name>` group exists, and how many capture groups there are.
    fn prescan(&mut self) {
        let s = self.s;
        let mut i = 0;
        let mut depth = 0usize;
        while i < s.len() {
            let c = s[i];
            if c == ch('\\') {
                i += 2;
                continue;
            }
            if depth > 0 {
                if c == ch(']') {
                    depth -= 1;
                } else if c == ch('[') && self.sets {
                    depth += 1;
                }
                i += 1;
                continue;
            }
            if c == ch('[') {
                depth = 1;
            } else if c == ch('(') {
                if s.get(i + 1) == Some(&ch('?')) {
                    if s.get(i + 2) == Some(&ch('<'))
                        && !matches!(s.get(i + 3).copied(), Some(x) if x == ch('=') || x == ch('!'))
                    {
                        self.has_named = true;
                        self.total_groups += 1;
                    }
                } else {
                    self.total_groups += 1;
                }
            }
            i += 1;
        }
    }

    fn run(&mut self) -> R<()> {
        self.prescan();
        self.frames.push(NameFrame::default());
        self.tokens()?;
        if !self.stack.is_empty() {
            return Err(ErrorCode::MissingParentheses);
        }
        if self.unicode && self.max_backref > self.names.len() as u64 {
            return Err(ErrorCode::InvalidBackreference);
        }
        for name in &self.named_refs {
            if !self.names.iter().any(|n| n.as_deref() == Some(name.as_str())) {
                return Err(ErrorCode::InvalidNamedBackReference);
            }
        }
        Ok(())
    }

    fn tokens(&mut self) -> R<()> {
        let mut last = Token::None;
        while let Some(c) = self.peek() {
            match char::from_u32(c) {
                Some('|') => {
                    self.i += 1;
                    let frame = self.frames.last_mut().unwrap();
                    let current = std::mem::take(&mut frame.current);
                    frame.done.extend(current);
                    last = Token::None;
                }
                Some('(') => {
                    self.paren_begin()?;
                    last = Token::None;
                }
                Some(')') => {
                    last = self.paren_end()?;
                }
                Some('^') | Some('$') => {
                    self.i += 1;
                    last = Token::None;
                }
                Some('.') => {
                    self.i += 1;
                    last = Token::Atom;
                }
                Some('[') => {
                    if self.sets {
                        self.class_set_top()?;
                    } else {
                        self.class()?;
                    }
                    last = Token::Atom;
                }
                Some(']') | Some('}') => {
                    if self.unicode {
                        return Err(ErrorCode::BracketUnmatched);
                    }
                    self.i += 1;
                    last = Token::Atom;
                }
                Some('\\') => {
                    last = self.atom_escape()?;
                }
                Some('*') | Some('+') | Some('?') => {
                    self.i += 1;
                    self.quantifier(last)?;
                    last = Token::None;
                }
                Some('{') => {
                    let save = self.i;
                    self.i += 1;
                    if self.peek_is_digit() {
                        let min = self.consume_number();
                        let mut max = min;
                        if self.try_consume(',') {
                            max = if self.peek_is_digit() { self.consume_number() } else { INFINITE };
                        }
                        if self.try_consume('}') {
                            if min > max {
                                return Err(ErrorCode::QuantifierOutOfOrder);
                            }
                            self.quantifier(last)?;
                            last = Token::None;
                            continue;
                        }
                    }
                    if self.unicode {
                        return Err(ErrorCode::QuantifierIncomplete);
                    }
                    self.i = save + 1;
                    last = Token::Atom;
                }
                _ => {
                    self.i += 1;
                    last = Token::Atom;
                }
            }
        }
        Ok(())
    }

    fn quantifier(&mut self, last: Token) -> R<()> {
        self.try_consume('?');
        match last {
            Token::Atom => Ok(()),
            Token::Lookbehind => Err(ErrorCode::CantQuantifyAtom),
            Token::None => Err(ErrorCode::QuantifierWithoutAtom),
        }
    }

    fn declare_name(&mut self, name: String) -> R<()> {
        if self.frames.iter().any(|f| f.current.contains(&name)) {
            return Err(ErrorCode::DuplicateGroupName);
        }
        self.frames.last_mut().unwrap().current.push(name.clone());
        self.names.push(Some(name));
        Ok(())
    }

    fn push_group(&mut self, group: Group) -> R<()> {
        if self.stack.len() >= MAX_NESTING {
            return Err(ErrorCode::TooManyDisjunctions);
        }
        self.stack.push(group);
        self.frames.push(NameFrame::default());
        Ok(())
    }

    fn paren_begin(&mut self) -> R<()> {
        self.i += 1;
        if !self.try_consume('?') {
            self.names.push(None);
            return self.push_group(Group::Capture);
        }
        let Some(c) = self.peek() else { return Err(ErrorCode::ParenthesesTypeInvalid) };
        match char::from_u32(c) {
            Some(':') => {
                self.i += 1;
                self.push_group(Group::NonCapture)
            }
            Some('=') | Some('!') => {
                self.i += 1;
                self.push_group(Group::Lookahead)
            }
            Some('<') => {
                self.i += 1;
                if self.try_consume('=') || self.try_consume('!') {
                    return self.push_group(Group::Lookbehind);
                }
                let name = self.group_name().ok_or(ErrorCode::InvalidGroupName)?;
                self.declare_name(name)?;
                self.push_group(Group::Capture)
            }
            Some('i') | Some('m') | Some('s') | Some('-') => self.modifiers(),
            _ => Err(ErrorCode::ParenthesesTypeInvalid),
        }
    }

    /// `(?ims-ims:...)`.
    fn modifiers(&mut self) -> R<()> {
        let flag = |c: Option<u32>| matches!(c.and_then(char::from_u32), Some('i' | 'm' | 's'));
        let mut add: Vec<u32> = Vec::new();
        while flag(self.peek()) {
            let c = self.peek().unwrap();
            if add.contains(&c) {
                return Err(ErrorCode::InvalidRegularExpressionModifier);
            }
            add.push(c);
            self.i += 1;
        }
        if self.try_consume('-') {
            let mut remove: Vec<u32> = Vec::new();
            while flag(self.peek()) {
                let c = self.peek().unwrap();
                if remove.contains(&c) || add.contains(&c) {
                    return Err(ErrorCode::InvalidRegularExpressionModifier);
                }
                remove.push(c);
                self.i += 1;
            }
            if add.is_empty() && remove.is_empty() {
                return Err(ErrorCode::InvalidRegularExpressionModifier);
            }
        }
        if self.at_end() {
            return Err(ErrorCode::MissingParentheses);
        }
        if self.try_consume(':') {
            return self.push_group(Group::Modifier);
        }
        Err(ErrorCode::ParenthesesTypeInvalid)
    }

    fn paren_end(&mut self) -> R<Token> {
        self.i += 1;
        let Some(group) = self.stack.pop() else { return Err(ErrorCode::ParenthesesUnmatched) };
        let frame = self.frames.pop().unwrap();
        let parent = self.frames.last_mut().unwrap();
        parent.current.extend(frame.done);
        parent.current.extend(frame.current);
        Ok(match group {
            Group::Lookbehind => Token::Lookbehind,
            Group::Lookahead if self.unicode => Token::None,
            _ => Token::Atom,
        })
    }

    /// One character of a group name, with `\u` escapes and (outside Unicode mode) surrogate pairs combined.
    fn name_char(&mut self) -> Option<u32> {
        let c = self.peek()?;
        if c == ch('\\') {
            self.i += 1;
            if !self.try_consume('u') {
                return None;
            }
            return self.unicode_escape_body(true).ok().flatten();
        }
        self.i += 1;
        if (0xD800..0xDC00).contains(&c)
            && let Some(t) = self.peek()
            && (0xDC00..0xE000).contains(&t)
        {
            self.i += 1;
            return Some(0x10000 + ((c - 0xD800) << 10) + (t - 0xDC00));
        }
        Some(c)
    }

    /// RegExpIdentifierName followed by `>`; the opening `<` is consumed.
    fn group_name(&mut self) -> Option<String> {
        let start = self.i;
        let name = self.group_name_body()?;
        self.name_spans.push((start, self.i, name.clone()));
        Some(name)
    }

    fn group_name_body(&mut self) -> Option<String> {
        let mut name = String::new();
        let first = self.name_char()?;
        let c = char::from_u32(first)?;
        if !(c == '$' || c == '_' || unicode_ident::is_xid_start(c)) {
            return None;
        }
        name.push(c);
        loop {
            if self.try_consume('>') {
                return Some(name);
            }
            let next = self.name_char()?;
            let c = char::from_u32(next)?;
            if !(c == '$' || c == '\u{200C}' || c == '\u{200D}' || unicode_ident::is_xid_continue(c)) {
                return None;
            }
            name.push(c);
        }
    }

    /// After `\u`: `{hex}` (when allowed) or four hex digits, joining an escaped surrogate pair in Unicode mode.
    /// `Ok(None)` when the text does not form an escape (the caller decides whether that is an error).
    fn unicode_escape_body(&mut self, braces_allowed: bool) -> R<Option<u32>> {
        if braces_allowed && self.peek_is('{') {
            let save = self.i;
            self.i += 1;
            let mut value: u64 = 0;
            let mut digits = 0;
            while let Some(d) = self.peek().and_then(hex_value) {
                value = (value * 16 + d as u64).min(0x110000);
                digits += 1;
                self.i += 1;
            }
            if digits == 0 || value > 0x10FFFF || !self.try_consume('}') {
                self.i = save;
                return Err(ErrorCode::InvalidUnicodeCodePointEscape);
            }
            return Ok(Some(value as u32));
        }
        let Some(value) = self.hex4() else { return Ok(None) };
        if (0xD800..0xDC00).contains(&value) && (self.unicode || braces_allowed) {
            let save = self.i;
            if self.try_consume('\\')
                && self.try_consume('u')
                && let Some(t) = self.hex4()
                && (0xDC00..0xE000).contains(&t)
            {
                return Ok(Some(0x10000 + ((value - 0xD800) << 10) + (t - 0xDC00)));
            }
            self.i = save;
        }
        Ok(Some(value))
    }

    fn hex4(&mut self) -> Option<u32> {
        let mut value = 0;
        for k in 0..4 {
            value = value * 16 + self.peek_at(k).and_then(hex_value)?;
        }
        self.i += 4;
        Some(value)
    }

    /// `\p{...}` / `\P{...}` after the letter; returns whether it is a property of strings.
    fn property(&mut self, negated: bool) -> R<bool> {
        if !self.try_consume('{') {
            return Err(ErrorCode::InvalidUnicodePropertyExpression);
        }
        let start = self.i;
        while let Some(c) = self.peek() {
            if c == ch('}') {
                break;
            }
            let ok = char::from_u32(c).is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '=');
            if !ok {
                return Err(ErrorCode::InvalidUnicodePropertyExpression);
            }
            self.i += 1;
        }
        if self.at_end() || self.i == start {
            return Err(ErrorCode::InvalidUnicodePropertyExpression);
        }
        let body = self.s[start..self.i].to_vec();
        self.i += 1;
        match property_kind(&body, self.sets) {
            None => Err(ErrorCode::InvalidUnicodePropertyExpression),
            Some(true) if negated => Err(ErrorCode::NegatedClassSetMayContainStrings),
            Some(strings) => Ok(strings),
        }
    }

    /// An escape outside a character class.
    fn atom_escape(&mut self) -> R<Token> {
        let start = self.i;
        self.i += 1;
        let Some(c) = self.peek() else { return Err(ErrorCode::EscapeUnterminated) };
        match char::from_u32(c) {
            Some('b') | Some('B') => {
                self.i += 1;
                Ok(Token::None)
            }
            Some('1'..='9') => {
                let n = self.consume_number();
                self.max_backref = self.max_backref.max(n);
                Ok(Token::Atom)
            }
            Some('k') => {
                if self.unicode || self.has_named {
                    self.i += 1;
                    if self.try_consume('<')
                        && let Some(name) = self.group_name()
                    {
                        self.ref_spans.push((start, self.i, name.clone()));
                        self.named_refs.push(name);
                        return Ok(Token::Atom);
                    }
                    return Err(if self.unicode {
                        ErrorCode::InvalidIdentityEscape
                    } else {
                        ErrorCode::InvalidNamedBackReference
                    });
                }
                self.i += 1;
                Ok(Token::Atom)
            }
            _ => {
                self.character_escape(false)?;
                Ok(Token::Atom)
            }
        }
    }

    /// Escapes shared by atoms and class atoms (after the backslash): built-in classes, control and hex escapes,
    /// octal (Annex B), properties, and identity escapes.
    fn character_escape(&mut self, in_class: bool) -> R<ClassAtom> {
        let c = self.peek().unwrap();
        let letter = char::from_u32(c);
        match letter {
            Some('d' | 'D' | 's' | 'S' | 'w' | 'W') => {
                self.i += 1;
                Ok(ClassAtom::Builtin)
            }
            Some('p' | 'P') if self.unicode => {
                self.i += 1;
                self.property(letter == Some('P'))?;
                Ok(ClassAtom::Builtin)
            }
            Some('f') => self.simple(0x0C),
            Some('n') => self.simple(0x0A),
            Some('r') => self.simple(0x0D),
            Some('t') => self.simple(0x09),
            Some('v') => self.simple(0x0B),
            Some('0') => {
                self.i += 1;
                if self.unicode {
                    if self.peek_is_digit() {
                        return Err(ErrorCode::InvalidOctalEscape);
                    }
                    return Ok(ClassAtom::Char(0));
                }
                Ok(ClassAtom::Char(self.octal_rest(0)))
            }
            Some('1'..='9') => {
                // Only reached inside a class: Annex B octal, or the digit itself for 8 and 9.
                if self.unicode {
                    return Err(ErrorCode::InvalidOctalEscape);
                }
                self.i += 1;
                if c >= ch('8') {
                    return Ok(ClassAtom::Char(c));
                }
                Ok(ClassAtom::Char(self.octal_rest(c - ch('0'))))
            }
            Some('c') => {
                self.i += 1;
                if let Some(next) = self.peek() {
                    let ok = char::from_u32(next)
                        .is_some_and(|n| n.is_ascii_alphabetic() || (in_class && !self.unicode && (n.is_ascii_digit() || n == '_')));
                    if ok {
                        self.i += 1;
                        return Ok(ClassAtom::Char(next & 31));
                    }
                    if self.unicode {
                        return Err(ErrorCode::InvalidControlLetterEscape);
                    }
                } else if self.unicode {
                    return Err(ErrorCode::InvalidIdentityEscape);
                }
                // Annex B: the backslash is literal and `c` is read again as a pattern character.
                self.i -= 1;
                Ok(ClassAtom::Char(ch('\\')))
            }
            Some('x') => {
                self.i += 1;
                if let (Some(a), Some(b)) = (self.peek().and_then(hex_value), self.peek_at(1).and_then(hex_value)) {
                    self.i += 2;
                    return Ok(ClassAtom::Char(a * 16 + b));
                }
                if self.unicode {
                    return Err(ErrorCode::InvalidIdentityEscape);
                }
                Ok(ClassAtom::Char(c))
            }
            Some('u') => {
                self.i += 1;
                match self.unicode_escape_body(self.unicode)? {
                    Some(v) => Ok(ClassAtom::Char(v)),
                    None if self.unicode => Err(ErrorCode::InvalidUnicodeEscape),
                    None => Ok(ClassAtom::Char(c)),
                }
            }
            _ => {
                if self.unicode {
                    let allowed = is_syntax_char(c) || c == ch('/') || (in_class && c == ch('-'));
                    if !allowed {
                        return Err(ErrorCode::InvalidIdentityEscape);
                    }
                }
                self.i += 1;
                Ok(ClassAtom::Char(c))
            }
        }
    }

    fn simple(&mut self, value: u32) -> R<ClassAtom> {
        self.i += 1;
        Ok(ClassAtom::Char(value))
    }

    fn octal_rest(&mut self, mut value: u32) -> u32 {
        // Up to three octal digits in all, while the value stays within \377.
        for _ in 0..2 {
            match self.peek() {
                Some(d) if (ch('0')..=ch('7')).contains(&d) && value * 8 + (d - ch('0')) <= 0o377 => {
                    value = value * 8 + (d - ch('0'));
                    self.i += 1;
                }
                _ => break,
            }
        }
        value
    }

    /// A class escape outside v-mode (after the backslash).
    fn class_escape(&mut self) -> R<ClassAtom> {
        let Some(c) = self.peek() else { return Err(ErrorCode::EscapeUnterminated) };
        match char::from_u32(c) {
            Some('b') => {
                self.i += 1;
                Ok(ClassAtom::Char(8))
            }
            Some('B') => {
                if self.unicode {
                    return Err(ErrorCode::InvalidIdentityEscape);
                }
                self.i += 1;
                Ok(ClassAtom::Char(c))
            }
            Some('-') if self.unicode => {
                self.i += 1;
                Ok(ClassAtom::Char(c))
            }
            Some('k') => {
                if self.unicode {
                    return Err(ErrorCode::InvalidIdentityEscape);
                }
                if self.has_named {
                    return Err(ErrorCode::InvalidNamedBackReference);
                }
                self.i += 1;
                Ok(ClassAtom::Char(c))
            }
            _ => self.character_escape(true),
        }
    }

    /// `[...]` outside v-mode, with Yarr's range state machine.
    fn class(&mut self) -> R<()> {
        self.i += 1;
        self.try_consume('^');
        let mut state = ClassState::Empty;
        loop {
            let Some(c) = self.peek() else { return Err(ErrorCode::CharacterClassUnmatched) };
            let (atom, hyphen_is_range) = if c == ch(']') {
                self.i += 1;
                return Ok(());
            } else if c == ch('\\') {
                self.i += 1;
                (self.class_escape()?, false)
            } else {
                self.i += 1;
                if self.unicode
                    && (0xD800..0xDC00).contains(&c)
                    && let Some(t) = self.peek()
                    && (0xDC00..0xE000).contains(&t)
                {
                    self.i += 1;
                    (ClassAtom::Char(0x10000 + ((c - 0xD800) << 10) + (t - 0xDC00)), true)
                } else {
                    (ClassAtom::Char(c), true)
                }
            };
            state = match atom {
                ClassAtom::Char(cp) => self.class_char(state, cp, hyphen_is_range)?,
                ClassAtom::Builtin => self.class_builtin(state)?,
            };
        }
    }

    fn class_char(&self, state: ClassState, cp: u32, hyphen_is_range: bool) -> R<ClassState> {
        let hyphen = hyphen_is_range && cp == ch('-');
        Ok(match state {
            ClassState::AfterCharacterClass if hyphen => ClassState::AfterCharacterClassHyphen,
            ClassState::AfterCharacterClass | ClassState::Empty => ClassState::CachedCharacter(cp),
            ClassState::CachedCharacter(prev) => {
                if hyphen {
                    ClassState::CachedCharacterHyphen(prev)
                } else {
                    ClassState::CachedCharacter(cp)
                }
            }
            ClassState::CachedCharacterHyphen(prev) => {
                if cp < prev {
                    return Err(ErrorCode::CharacterClassRangeOutOfOrder);
                }
                ClassState::Empty
            }
            ClassState::AfterCharacterClassHyphen => {
                if self.unicode {
                    return Err(ErrorCode::CharacterClassRangeInvalid);
                }
                ClassState::Empty
            }
        })
    }

    fn class_builtin(&self, state: ClassState) -> R<ClassState> {
        match state {
            ClassState::CachedCharacterHyphen(_) | ClassState::AfterCharacterClassHyphen if self.unicode => {
                Err(ErrorCode::CharacterClassRangeInvalid)
            }
            ClassState::CachedCharacterHyphen(_) | ClassState::AfterCharacterClassHyphen => Ok(ClassState::Empty),
            _ => Ok(ClassState::AfterCharacterClass),
        }
    }

    // ----- v-mode class sets -----

    fn class_set_top(&mut self) -> R<()> {
        self.class_set().map(|_| ())
    }

    /// `[...]` in v-mode; returns whether the class may contain strings.
    fn class_set(&mut self) -> R<bool> {
        self.i += 1;
        let negated = self.try_consume('^');
        #[derive(PartialEq, Clone, Copy)]
        enum Op {
            None,
            Union,
            And,
            Sub,
        }
        let mut op = Op::None;
        let mut operands = 0usize;
        // After `&&`/`--` an operand is required; after an operand of an intersection or subtraction an operator.
        let mut need_operand = false;
        let mut strings_union = false;
        let mut strings_first = false;
        let mut strings_all = true;
        loop {
            let Some(c) = self.peek() else { return Err(ErrorCode::CharacterClassUnmatched) };
            if c == ch(']') && !need_operand {
                self.i += 1;
                break;
            }
            let next = self.peek_at(1);
            let double = |x: char| c == ch(x) && next == Some(ch(x));
            if double('&') || double('-') {
                let this = if c == ch('&') { Op::And } else { Op::Sub };
                if operands == 0 || need_operand {
                    return Err(if need_operand {
                        ErrorCode::InvalidClassSetCharacter
                    } else {
                        ErrorCode::InvalidClassSetOperation
                    });
                }
                if op == Op::Union || (op != Op::None && op != this) {
                    return Err(ErrorCode::InvalidClassSetOperation);
                }
                op = this;
                self.i += 2;
                if self.peek() == Some(c) || self.peek_is(']') {
                    return Err(ErrorCode::InvalidClassSetCharacter);
                }
                need_operand = true;
                continue;
            }
            if matches!(op, Op::And | Op::Sub) && !need_operand {
                return Err(ErrorCode::InvalidClassSetOperation);
            }
            if need_operand && c == ch(']') {
                return Err(ErrorCode::InvalidClassSetCharacter);
            }
            if next == Some(c) && char::from_u32(c).is_some_and(|x| "&!#$%*+,.:;<=>?@^`~".contains(x)) {
                return Err(ErrorCode::InvalidClassSetOperation);
            }
            // One operand, possibly the start of a range.
            let (single, strings) = self.class_set_operand()?;
            let mut was_range = false;
            if self.peek_is('-') && self.peek_at(1) != Some(ch('-')) {
                let Some(start) = single else {
                    return Err(if matches!(op, Op::And | Op::Sub) {
                        ErrorCode::InvalidClassSetOperation
                    } else {
                        ErrorCode::CharacterClassRangeInvalid
                    });
                };
                if matches!(op, Op::And | Op::Sub) {
                    return Err(ErrorCode::InvalidClassSetOperation);
                }
                self.i += 1;
                let Some(next) = self.peek() else { return Err(ErrorCode::CharacterClassUnmatched) };
                if next == ch('\\') && matches!(self.peek_at(1).and_then(char::from_u32), Some('d' | 'D' | 's' | 'S' | 'w' | 'W' | 'p' | 'P')) {
                    return Err(ErrorCode::CharacterClassRangeInvalid);
                }
                if next == ch('[') || (next == ch('\\') && self.peek_at(1) == Some(ch('q'))) {
                    return Err(ErrorCode::InvalidClassSetCharacter);
                }
                let (end, _) = self.class_set_operand()?;
                let Some(end) = end else { return Err(ErrorCode::CharacterClassRangeInvalid) };
                if end < start {
                    return Err(ErrorCode::CharacterClassRangeOutOfOrder);
                }
                was_range = true;
            }
            operands += 1;
            if operands == 1 {
                strings_first = strings;
            }
            strings_union |= strings;
            strings_all &= strings;
            match op {
                Op::None if operands > 1 || was_range => op = Op::Union,
                _ => {}
            }
            if matches!(op, Op::And | Op::Sub) {
                need_operand = false;
            }
            if was_range && self.peek_is('-') && self.peek_at(1) != Some(ch('-')) {
                return Err(ErrorCode::InvalidClassSetCharacter);
            }
        }
        let strings = match op {
            Op::And => strings_all,
            Op::Sub => strings_first,
            _ => strings_union,
        };
        if negated && strings {
            return Err(ErrorCode::NegatedClassSetMayContainStrings);
        }
        Ok(strings)
    }

    /// One v-mode operand: `(Some(cp), false)` for a single character, `(None, strings)` otherwise.
    fn class_set_operand(&mut self) -> R<(Option<u32>, bool)> {
        let Some(c) = self.peek() else { return Err(ErrorCode::CharacterClassUnmatched) };
        if c == ch('[') {
            if self.stack.len() + 1 >= MAX_NESTING {
                return Err(ErrorCode::TooManyDisjunctions);
            }
            let strings = self.class_set()?;
            return Ok((None, strings));
        }
        if c == ch('\\') {
            self.i += 1;
            let Some(e) = self.peek() else { return Err(ErrorCode::EscapeUnterminated) };
            match char::from_u32(e) {
                Some('q') => {
                    self.i += 1;
                    if !self.try_consume('{') {
                        return Err(ErrorCode::InvalidIdentityEscape);
                    }
                    return Ok((None, self.class_string_disjunction()?));
                }
                Some('b') => {
                    self.i += 1;
                    return Ok((Some(8), false));
                }
                Some('p' | 'P') => {
                    self.i += 1;
                    let strings = self.property(e == ch('P'))?;
                    return Ok((None, strings));
                }
                Some(x) if "&-!#%,:;<=>@`~".contains(x) => {
                    self.i += 1;
                    return Ok((Some(e), false));
                }
                Some('k' | 'B') => return Err(ErrorCode::InvalidIdentityEscape),
                _ => {
                    return Ok(match self.character_escape(true)? {
                        ClassAtom::Char(cp) => (Some(cp), false),
                        ClassAtom::Builtin => (None, false),
                    });
                }
            }
        }
        if char::from_u32(c).is_some_and(|x| "()[]{}/-|".contains(x)) {
            return Err(ErrorCode::InvalidClassSetCharacter);
        }
        if c == ch('&') {
            self.ampersands.push(self.i);
        }
        self.i += 1;
        Ok((Some(c), false))
    }

    /// `\q{...}` after the brace: whether any alternative is not exactly one character.
    fn class_string_disjunction(&mut self) -> R<bool> {
        let mut strings = false;
        let mut length = 0usize;
        loop {
            let Some(c) = self.peek() else { return Err(ErrorCode::ClassStringDisjunctionUnmatched) };
            if c == ch('}') {
                self.i += 1;
                return Ok(strings || length != 1);
            }
            if c == ch('|') {
                self.i += 1;
                strings |= length != 1;
                length = 0;
                continue;
            }
            if c == ch('\\') {
                self.i += 1;
                if self.at_end() {
                    return Err(ErrorCode::EscapeUnterminated);
                }
                let e = self.peek().unwrap();
                if e == ch('b') || char::from_u32(e).is_some_and(|x| "&-!#%,:;<=>@`~".contains(x)) {
                    self.i += 1;
                } else {
                    match self.character_escape(true)? {
                        ClassAtom::Char(_) => {}
                        ClassAtom::Builtin => return Err(ErrorCode::InvalidClassSetCharacter),
                    }
                }
                length += 1;
                continue;
            }
            if char::from_u32(c).is_some_and(|x| "()[]{}/-|".contains(x)) {
                return Err(ErrorCode::InvalidClassSetCharacter);
            }
            self.i += 1;
            length += 1;
        }
    }
}

/// Validates `pattern` (UTF-16) for the given mode, reporting the first error JSC would.
pub fn validate(pattern: &[u16], unicode: bool, sets: bool) -> Result<PatternInfo, ErrorCode> {
    let chars = pattern_chars(pattern, unicode || sets);
    let mut p = Parser {
        s: &chars,
        i: 0,
        unicode: unicode || sets,
        sets,
        has_named: false,
        total_groups: 0,
        names: Vec::new(),
        stack: Vec::new(),
        frames: Vec::new(),
        max_backref: 0,
        named_refs: Vec::new(),
        ref_spans: Vec::new(),
        ampersands: Vec::new(),
        name_spans: Vec::new(),
    };
    p.run()?;
    Ok(PatternInfo {
        group_names: p.names,
        named_refs: p.ref_spans,
        class_ampersands: p.ampersands,
        name_spans: p.name_spans,
    })
}

/// The pattern the matcher compiles, with two rewrites `regress` needs to match like JS:
/// - a `\k<name>` naming several groups (duplicate names in different alternatives) becomes `(?:\i\j...)` over all
///   of them: at most one of those groups participates and the others match empty, so the backreference reads
///   whichever one did;
/// - a literal `&` operand in a v-mode class is escaped (`[a&b]`);
/// - group names become plain ASCII (`n0`, `n1`, ...; results are keyed by our own name table), so names written
///   with escapes or as surrogate pairs in a non-Unicode pattern compile.
pub fn matcher_chars(chars: Vec<u32>, info: &PatternInfo) -> Vec<u32> {
    let duplicated = |name: &str| info.group_names.iter().filter(|n| n.as_deref() == Some(name)).count() > 1;
    let mut edits: Vec<(usize, usize, Vec<u32>)> = Vec::new();
    for (start, end, name) in &info.named_refs {
        if !duplicated(name) {
            continue;
        }
        let mut text = String::from("(?:");
        for (index, group) in info.group_names.iter().enumerate() {
            if group.as_deref() == Some(name.as_str()) {
                text.push_str(&format!("\\{}", index + 1));
            }
        }
        text.push(')');
        edits.push((*start, *end, text.chars().map(|c| c as u32).collect()));
    }
    for &at in &info.class_ampersands {
        edits.push((at, at + 1, vec![ch('\\'), ch('&')]));
    }
    let mut distinct: Vec<&str> = Vec::new();
    for (start, end, name) in &info.name_spans {
        let inside_ref = info.named_refs.iter().any(|(s, e, n)| duplicated(n) && *s <= *start && *end <= *e);
        if inside_ref {
            continue;
        }
        let index = distinct.iter().position(|d| *d == name.as_str()).unwrap_or_else(|| {
            distinct.push(name);
            distinct.len() - 1
        });
        edits.push((*start, *end, format!("n{index}>").chars().map(|c| c as u32).collect()));
    }
    if edits.is_empty() {
        return chars;
    }
    edits.sort_by_key(|e| e.0);
    let mut out = Vec::with_capacity(chars.len() + 16);
    let mut at = 0;
    for (start, end, replacement) in edits {
        out.extend_from_slice(&chars[at..start]);
        out.extend(replacement);
        at = end;
    }
    out.extend_from_slice(&chars[at..]);
    out
}
