//! `RegExp`: construction, `exec`/`test`/`toString`, the flag and `lastIndex` accessors, `RegExp.escape`, and the
//! regular-expression halves of the String methods (`match`, `matchAll`, `replace`, `search`, `split`). Port of
//! `stdlib/regexp.ts`. Upstream delegates matching to the host engine (JavaScriptCore); here `regress` matches over
//! UTF-16, early errors carry Yarr's texts (see [`syntax`]), and the `RegExp.prototype[Symbol.*]` algorithms that
//! the host String methods run are implemented after the spec, with one shared `lastIndex` per RegExp object.

pub mod sticky;
pub mod syntax;

use std::cell::Cell;
use std::rc::Rc;

use crate::data::quote_units;
use crate::interp::coerce::{coerce_to_number, coerce_to_string};
use crate::interp::errors::{syntax_error, type_error, JsResult};
use crate::interp::function::{arg, constructor, method, methods, prototype_from, receiver, NResult, NativeFn};
use crate::interp::Rt;
use crate::object::{define_accessor, define_data, Obj, ObjKind};
use crate::value::{JsStr, Value};

/// The flags of a regular expression.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    pub has_indices: bool,
    pub global: bool,
    pub ignore_case: bool,
    pub multiline: bool,
    pub dot_all: bool,
    pub unicode: bool,
    pub unicode_sets: bool,
    pub sticky: bool,
}

impl Flags {
    /// Parses a flags string: known letters, each at most once, never `u` with `v`.
    pub fn parse(units: &[u16]) -> Option<Flags> {
        let mut f = Flags::default();
        for &u in units {
            let slot = match char::from_u32(u as u32)? {
                'd' => &mut f.has_indices,
                'g' => &mut f.global,
                'i' => &mut f.ignore_case,
                'm' => &mut f.multiline,
                's' => &mut f.dot_all,
                'u' => &mut f.unicode,
                'v' => &mut f.unicode_sets,
                'y' => &mut f.sticky,
                _ => return None,
            };
            if *slot {
                return None;
            }
            *slot = true;
        }
        if f.unicode && f.unicode_sets {
            return None;
        }
        Some(f)
    }
    /// The canonical `flags` string (`"dgimsuvy"` order).
    pub fn text(&self) -> String {
        let mut s = String::new();
        for (on, c) in [
            (self.has_indices, 'd'),
            (self.global, 'g'),
            (self.ignore_case, 'i'),
            (self.multiline, 'm'),
            (self.dot_all, 's'),
            (self.unicode, 'u'),
            (self.unicode_sets, 'v'),
            (self.sticky, 'y'),
        ] {
            if on {
                s.push(c);
            }
        }
        s
    }
    /// Unicode-aware matching (`u` or `v`).
    pub fn full_unicode(&self) -> bool {
        self.unicode || self.unicode_sets
    }
    fn get(&self, name: &str) -> bool {
        match name {
            "hasIndices" => self.has_indices,
            "global" => self.global,
            "ignoreCase" => self.ignore_case,
            "multiline" => self.multiline,
            "sticky" => self.sticky,
            "unicode" => self.unicode,
            "unicodeSets" => self.unicode_sets,
            "dotAll" => self.dot_all,
            _ => false,
        }
    }
}

/// A compiled pattern, shared by copies of a RegExp (`matchAll`, `split`) and by RegExps compiled from the same
/// pattern and flags.
pub struct Compiled {
    regex: regress::Regex,
    /// The pattern as the matcher reads it and its options, for the anchored variant.
    chars: Rc<[u32]>,
    options: regress::Flags,
    /// `^(?:pattern)`, used for sticky matching over the text after `lastIndex` so a failed sticky match does not
    /// scan the rest of the input; `None` when the pattern looks behind its start (`^`, `\b`, `\B`, lookbehind).
    anchored: std::cell::OnceCell<Option<regress::Regex>>,
    /// `^(?:a|b|...)` over the atoms every match starts with ([`sticky::first_atoms`]): a failing sticky match is
    /// usually rejected by testing the one character at `lastIndex`.
    probe: std::cell::OnceCell<Option<regress::Regex>>,
    /// One entry per capture group: its name, if any.
    names: Vec<Option<String>>,
    flags: Flags,
    /// The pattern as `source` reports it (escaped like JSC's `source`).
    source: JsStr,
}

impl Compiled {
    fn has_named_groups(&self) -> bool {
        self.names.iter().any(|n| n.is_some())
    }
    /// Group names in first-appearance order, without duplicates.
    fn unique_names(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for name in self.names.iter().flatten() {
            if !out.contains(&name.as_str()) {
                out.push(name);
            }
        }
        out
    }
}

/// The state of a RegExp object: its compiled pattern and the one `lastIndex` that `exec`, `test`, and the String
/// methods share.
pub struct RegExpData {
    /// The escaped `source` text.
    pub source: String,
    /// The canonical `flags` text.
    pub flags: String,
    compiled: Rc<Compiled>,
    last_index: Cell<f64>,
}

impl RegExpData {
    /// `/source/flags`, as `String(regex)` shows it.
    pub fn display(&self) -> String {
        format!("/{}/{}", self.source, self.flags)
    }
    pub fn source(&self) -> String {
        self.source.clone()
    }
    /// `display` without losing lone surrogates.
    pub fn display_js(&self) -> JsStr {
        let mut units: Vec<u16> = vec![b'/' as u16];
        units.extend_from_slice(self.compiled.source.units());
        units.push(b'/' as u16);
        units.extend(self.flags.encode_utf16());
        JsStr::from_units(units)
    }
    pub fn source_js(&self) -> JsStr {
        self.compiled.source.clone()
    }
    pub fn flag_set(&self) -> Flags {
        self.compiled.flags
    }
    pub fn last_index(&self) -> f64 {
        self.last_index.get()
    }
    pub fn set_last_index(&self, value: f64) {
        self.last_index.set(value);
    }
    /// A fresh RegExp over the same pattern and flags (`lastIndex` 0), as `new RegExp(re.source, re.flags)`.
    pub fn copy(&self) -> RegExpData {
        RegExpData {
            source: self.source.clone(),
            flags: self.flags.clone(),
            compiled: self.compiled.clone(),
            last_index: Cell::new(0.0),
        }
    }
    fn with_flags(&self, flags: Flags) -> RegExpData {
        if flags == self.compiled.flags {
            return self.copy();
        }
        // Only flags that do not change compilation (g, y, d) are ever added this way.
        let compiled = Compiled {
            regex: self.compiled.regex.clone(),
            chars: self.compiled.chars.clone(),
            options: self.compiled.options,
            anchored: self.compiled.anchored.clone(),
            probe: self.compiled.probe.clone(),
            names: self.compiled.names.clone(),
            flags,
            source: self.compiled.source.clone(),
        };
        RegExpData {
            source: self.source.clone(),
            flags: flags.text(),
            compiled: Rc::new(compiled),
            last_index: Cell::new(0.0),
        }
    }

    /// Compiles `pattern` with `flags` like `new RegExp(pattern, flags)` on the host.
    pub fn new(pattern: &JsStr, flags: &[u16]) -> Result<RegExpData, CompileError> {
        let flags = Flags::parse(flags).ok_or(CompileError::Flags)?;
        let key = (pattern.units().to_vec(), flags.text());
        if let Some(compiled) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
            return Ok(RegExpData {
                source: String::from_utf16_lossy(compiled.source.units()),
                flags: key.1,
                compiled,
                last_index: Cell::new(0.0),
            });
        }
        let info = syntax::validate(pattern.units(), flags.unicode, flags.unicode_sets)
            .map_err(|code| CompileError::Pattern(code.message().to_string()))?;
        let full = flags.full_unicode();
        let chars = syntax::matcher_chars(syntax::pattern_chars(pattern.units(), full), &info);
        let options = regress::Flags {
            icase: flags.ignore_case,
            multiline: flags.multiline,
            dot_all: flags.dot_all,
            no_opt: false,
            unicode: full,
            unicode_sets: flags.unicode_sets,
        };
        let regex = regress::Regex::from_unicode(chars.iter().copied(), options)
            .map_err(|e| CompileError::Pattern(engine_reason(&e.text)))?;
        let source = escape_pattern(pattern.units());
        let compiled = Rc::new(Compiled {
            regex,
            chars: chars.into(),
            options,
            anchored: std::cell::OnceCell::new(),
            probe: std::cell::OnceCell::new(),
            names: info.group_names,
            flags,
            source,
        });
        CACHE.with(|c| {
            let mut cache = c.borrow_mut();
            if cache.len() >= 512 {
                cache.clear();
            }
            cache.insert(key.clone(), compiled.clone());
        });
        Ok(RegExpData {
            source: String::from_utf16_lossy(compiled.source.units()),
            flags: key.1,
            compiled,
            last_index: Cell::new(0.0),
        })
    }
}

type CompileCache = std::collections::HashMap<(Vec<u16>, String), Rc<Compiled>>;

thread_local! {
    /// Compiled patterns by (pattern, flags): regex literals in loops and pattern strings compile once.
    static CACHE: std::cell::RefCell<CompileCache> = std::cell::RefCell::new(CompileCache::new());
}

/// Whether matching can depend on the text before the match start: `^`, `\b`, `\B`, or a lookbehind.
fn looks_behind(chars: &[u32]) -> bool {
    let ch = |c: char| c as u32;
    let mut i = 0;
    let mut in_class = false;
    while i < chars.len() {
        let c = chars[i];
        if c == ch('\\') {
            if !in_class && matches!(chars.get(i + 1), Some(&n) if n == ch('b') || n == ch('B')) {
                return true;
            }
            i += 2;
            continue;
        }
        if in_class {
            if c == ch(']') {
                in_class = false;
            } else if c == ch('[') {
                // A nested v-mode class: stay conservative.
                return true;
            }
        } else if c == ch('[') {
            in_class = true;
            if chars.get(i + 1) == Some(&ch('^')) {
                i += 1;
            }
        } else if c == ch('^')
            || (c == ch('(')
                && chars.get(i + 1) == Some(&ch('?'))
                && chars.get(i + 2) == Some(&ch('<'))
                && matches!(chars.get(i + 3), Some(&n) if n == ch('=') || n == ch('!')))
        {
            return true;
        }
        i += 1;
    }
    false
}

impl Compiled {
    fn probe(&self) -> Option<&regress::Regex> {
        self.probe
            .get_or_init(|| {
                if self.options.unicode_sets {
                    return None;
                }
                let atoms = sticky::first_atoms(&self.chars, self.options.unicode)?;
                let mut source: Vec<u32> = "^(?:".chars().map(|c| c as u32).collect();
                for (i, atom) in atoms.iter().enumerate() {
                    if i > 0 {
                        source.push('|' as u32);
                    }
                    source.extend_from_slice(atom);
                }
                source.push(')' as u32);
                regress::Regex::from_unicode(source.into_iter(), self.options).ok()
            })
            .as_ref()
    }

    fn anchored(&self) -> Option<&regress::Regex> {
        self.anchored
            .get_or_init(|| {
                if self.options.multiline || looks_behind(&self.chars) {
                    return None;
                }
                let mut wrapped: Vec<u32> = "^(?:".chars().map(|c| c as u32).collect();
                wrapped.extend_from_slice(&self.chars);
                wrapped.push(')' as u32);
                regress::Regex::from_unicode(wrapped.into_iter(), self.options).ok()
            })
            .as_ref()
    }
}

/// Why a pattern did not compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    Flags,
    Pattern(String),
}

impl CompileError {
    /// The host engine's reason text (without its "Invalid regular expression: " prefix).
    pub fn reason(&self) -> String {
        match self {
            CompileError::Flags => "Invalid flags supplied to RegExp constructor.".into(),
            CompileError::Pattern(reason) => reason.clone(),
        }
    }
}

/// A pattern the validator accepted but the engine could not compile: closest Yarr text.
fn engine_reason(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if lower.contains("parenthes") {
        "missing )".into()
    } else if lower.contains("quantifier") {
        "nothing to repeat".into()
    } else if lower.contains("bracket") {
        "missing terminating ] for character class".into()
    } else if lower.contains("too deeply nested") {
        "too many nested disjunctions".into()
    } else if lower.contains("limit") {
        "regular expression too large".into()
    } else {
        lower
    }
}

/// JSC's `source` escaping: `/` outside a class and line terminators become escapes; the empty pattern is `(?:)`.
pub fn escape_pattern(pattern: &[u16]) -> JsStr {
    if pattern.is_empty() {
        return JsStr::from("(?:)");
    }
    let mut out: Vec<u16> = Vec::with_capacity(pattern.len());
    let mut backslash = false;
    let mut in_brackets = false;
    for &c in pattern {
        let mut escape = false;
        if !backslash {
            if in_brackets {
                if c == b']' as u16 {
                    in_brackets = false;
                }
            } else if c == b'/' as u16 {
                escape = true;
            } else if c == b'[' as u16 {
                in_brackets = true;
            }
        }
        let terminator = matches!(c, 0x0A | 0x0D | 0x2028 | 0x2029);
        if escape || terminator {
            if !backslash {
                out.push(b'\\' as u16);
            }
            match c {
                0x0A => out.push(b'n' as u16),
                0x0D => out.push(b'r' as u16),
                0x2028 => out.extend("u2028".encode_utf16()),
                0x2029 => out.extend("u2029".encode_utf16()),
                _ => out.push(c),
            }
        } else {
            out.push(c);
        }
        backslash = !backslash && c == b'\\' as u16;
    }
    JsStr::from_units(out)
}

fn quoted(units: &[u16]) -> String {
    let mut out = String::new();
    quote_units(units, &mut out);
    out
}

const ESCAPE_REGEX_HINT: &str = "To match special characters like ( ) [ ] { } + * ? . literally, escape them with a backslash (e.g. \"\\\\(\") or test for them with String.includes instead.";

pub fn regexp_data(o: &Obj) -> Option<&RegExpData> {
    match o.kind() {
        ObjKind::RegExp(r) => Some(r),
        _ => None,
    }
}

pub fn as_regexp(value: &Value) -> Option<&RegExpData> {
    match value {
        Value::Object(o) => regexp_data(o),
        _ => None,
    }
}

/// A RegExp object around `data`.
pub fn regexp_object(proto: &Obj, data: RegExpData) -> Obj {
    Obj::new(Some(proto.clone()), ObjKind::RegExp(data))
}

/// A regex the String methods run: the RegExp object's own (sharing its `lastIndex`) or a fresh one compiled from
/// a pattern string.
pub enum HostRegex {
    Object(Obj),
    Fresh(RegExpData),
}

impl HostRegex {
    pub fn data(&self) -> &RegExpData {
        match self {
            HostRegex::Object(o) => regexp_data(o).expect("RegExp object"),
            HostRegex::Fresh(d) => d,
        }
    }
}

/// `toHostRegex`: a RegExp is used as is; anything else is a pattern string, as `new RegExp(arg)` would read it.
pub fn to_host_regex(value: &Value, method: &str, extra_flags: &str) -> JsResult<HostRegex> {
    if let Value::Object(o) = value
        && regexp_data(o).is_some()
    {
        return Ok(HostRegex::Object(o.clone()));
    }
    let source = if value.is_undefined() { JsStr::empty() } else { coerce_to_string(value) };
    let flags: Vec<u16> = extra_flags.encode_utf16().collect();
    RegExpData::new(&source, &flags).map(HostRegex::Fresh).map_err(|e| {
        syntax_error(format!(
            "String.{method} received the string {}, which is not a valid regular expression pattern ({}). {ESCAPE_REGEX_HINT}",
            quoted(source.units()),
            e.reason()
        ))
    })
}

/// `new RegExp(pattern, flags)` / `RegExp(pattern, flags)` and regex literals.
pub fn construct_regexp(args: &[Value], proto: &Obj) -> JsResult<Obj> {
    let first = arg(args, 0);
    let pattern = match &first {
        v if as_regexp(v).is_some() => as_regexp(v).unwrap().source_js(),
        Value::Undefined => JsStr::empty(),
        other => coerce_to_string(other),
    };
    let flags_arg = arg(args, 1);
    let flags: JsStr = match &flags_arg {
        Value::Undefined => match as_regexp(&first) {
            Some(r) => JsStr::from(r.flags.as_str()),
            None => JsStr::empty(),
        },
        Value::String(s) => s.clone(),
        other => {
            let shown = if matches!(other, Value::Null) { "null" } else { other.js_typeof() };
            return Err(syntax_error(format!(
                "RegExp flags must be a string of flag characters (e.g. \"g\", \"gi\"), not {shown}."
            )));
        }
    };
    match RegExpData::new(&pattern, flags.units()) {
        Ok(data) => Ok(regexp_object(proto, data)),
        Err(e) => {
            let reason = e.reason();
            Err(syntax_error(if reason.to_ascii_lowercase().contains("flag") {
                format!(
                    "new RegExp(...) received invalid flags {} ({reason}). Valid flags are d, g, i, m, s, u, v, and y.",
                    quoted(flags.units())
                )
            } else {
                format!(
                    "new RegExp(...) received {}, which is not a valid regular expression pattern ({reason}). {ESCAPE_REGEX_HINT}",
                    quoted(pattern.units())
                )
            }))
        }
    }
}

/// A regex literal, or a host RegExp crossing into the program.
pub fn construct_regexp_literal(rt: &Rt, pattern: &str, flags: &str) -> JsResult<Obj> {
    construct_regexp(&[Value::str(pattern), Value::str(flags)], &rt.builtins.regexp)
}

/// A fresh copy of a RegExp object (`structuredClone`): same pattern and flags, `lastIndex` 0.
pub fn clone_regexp(rt: &Rt, data: &RegExpData) -> Obj {
    regexp_object(&rt.builtins.regexp, data.copy())
}

// ----- matching -----

/// One match: the matched range and each capture's range (`None` when the group did not participate).
#[derive(Clone, Debug)]
pub struct RawMatch {
    pub start: usize,
    pub end: usize,
    pub captures: Vec<Option<(usize, usize)>>,
}

/// ToLength.
pub fn to_length(n: f64) -> f64 {
    if n.is_nan() || n <= 0.0 {
        return 0.0;
    }
    n.floor().min(9_007_199_254_740_991.0)
}

/// AdvanceStringIndex.
pub fn advance_index(text: &[u16], index: usize, unicode: bool) -> usize {
    if !unicode || index + 1 >= text.len() {
        return index + 1;
    }
    let (a, b) = (text[index], text[index + 1]);
    if (0xD800..0xDC00).contains(&a) && (0xDC00..0xE000).contains(&b) { index + 2 } else { index + 1 }
}

/// The first match starting at or after `start` (exactly at `start` when `sticky`).
fn find_at(c: &Compiled, text: &[u16], start: usize, sticky: bool) -> Option<RawMatch> {
    if start > text.len() {
        return None;
    }
    let search = |from: usize| {
        let m = if c.flags.full_unicode() {
            c.regex.find_from_utf16(text, from).next()
        } else {
            c.regex.find_from_ucs2(text, from).next()
        }?;
        Some(RawMatch {
            start: m.range.start,
            end: m.range.end,
            captures: m.captures.iter().map(|r| r.as_ref().map(|r| (r.start, r.end))).collect(),
        })
    };
    // Like JSC, a Unicode-mode search that starts inside a surrogate pair matches nothing but an empty match there,
    // then continues after the pair.
    let mid_pair = c.flags.full_unicode()
        && start > 0
        && start < text.len()
        && (0xDC00..0xE000).contains(&text[start])
        && (0xD800..0xDC00).contains(&text[start - 1]);
    if mid_pair {
        if let Some(m) = search(start)
            && m.start == start
            && m.end == start
        {
            return Some(m);
        }
        if sticky {
            return None;
        }
        return search(start + 1);
    }
    if sticky && let Some(probe) = c.probe() {
        // The pattern cannot match empty, so a match at `start` begins with the character there.
        if start >= text.len() {
            return None;
        }
        let pair = c.flags.full_unicode()
            && start + 1 < text.len()
            && (0xD800..0xDC00).contains(&text[start])
            && (0xDC00..0xE000).contains(&text[start + 1]);
        let unit = &text[start..start + if pair { 2 } else { 1 }];
        let hit = if c.flags.full_unicode() {
            probe.find_from_utf16(unit, 0).next().is_some()
        } else {
            probe.find_from_ucs2(unit, 0).next().is_some()
        };
        if !hit {
            return None;
        }
    }
    if sticky && let Some(anchored) = c.anchored() {
        let rest = &text[start..];
        let m = if c.flags.full_unicode() {
            anchored.find_from_utf16(rest, 0).next()
        } else {
            anchored.find_from_ucs2(rest, 0).next()
        }?;
        if m.range.start != 0 {
            return None;
        }
        return Some(RawMatch {
            start: start + m.range.start,
            end: start + m.range.end,
            captures: m.captures.iter().map(|r| r.as_ref().map(|r| (start + r.start, start + r.end))).collect(),
        });
    }
    let m = search(start)?;
    if sticky && m.start != start {
        return None;
    }
    Some(m)
}

/// RegExpBuiltinExec: reads and updates `lastIndex` for global and sticky regexes.
pub fn exec_raw(rx: &RegExpData, text: &[u16]) -> Option<RawMatch> {
    let flags = rx.compiled.flags;
    let stateful = flags.global || flags.sticky;
    let mut last_index = to_length(rx.last_index());
    if !stateful {
        last_index = 0.0;
    }
    if last_index > text.len() as f64 {
        if stateful {
            rx.set_last_index(0.0);
        }
        return None;
    }
    match find_at(&rx.compiled, text, last_index as usize, flags.sticky) {
        None => {
            if stateful {
                rx.set_last_index(0.0);
            }
            None
        }
        Some(m) => {
            if stateful {
                rx.set_last_index(m.end as f64);
            }
            Some(m)
        }
    }
}

fn slice_value(text: &JsStr, range: Option<(usize, usize)>) -> Value {
    match range {
        Some((s, e)) => Value::String(text.slice(s, e)),
        None => Value::Undefined,
    }
}

/// The `groups` object of a match (named groups only), keyed in first-appearance order.
fn groups_object(rt: &Rt, c: &Compiled, text: &JsStr, m: &RawMatch, range_value: &dyn Fn(Option<(usize, usize)>) -> Value) -> Obj {
    let groups = Obj::ordinary(&rt.builtins.object);
    for name in c.unique_names() {
        let range = c
            .names
            .iter()
            .enumerate()
            .filter(|(_, n)| n.as_deref() == Some(name))
            .find_map(|(i, _)| m.captures.get(i).cloned().flatten());
        let _ = text;
        define_data(&groups, name, range_value(range));
    }
    groups
}

/// The array `exec` and `match` return: captures plus `index`, `input`, `groups` (named groups only), and
/// `indices` (`d` flag).
pub fn match_to_value(rt: &Rt, rx: &RegExpData, text: &JsStr, m: &RawMatch) -> Obj {
    let c = &rx.compiled;
    let mut items = vec![Value::String(text.slice(m.start, m.end))];
    items.extend(m.captures.iter().map(|r| slice_value(text, *r)));
    let result = Obj::array(&rt.builtins.array, items);
    define_data(&result, "index", Value::Number(m.start as f64));
    define_data(&result, "input", Value::String(text.clone()));
    if c.has_named_groups() {
        let groups = groups_object(rt, c, text, m, &|r| slice_value(text, r));
        define_data(&result, "groups", Value::Object(groups));
    }
    if c.flags.has_indices {
        let pair = |r: Option<(usize, usize)>| match r {
            Some((s, e)) => Value::Object(Obj::array(
                &rt.builtins.array,
                vec![Value::Number(s as f64), Value::Number(e as f64)],
            )),
            None => Value::Undefined,
        };
        let mut ranges = vec![pair(Some((m.start, m.end)))];
        ranges.extend(m.captures.iter().map(|r| pair(*r)));
        let indices = Obj::array(&rt.builtins.array, ranges);
        let groups = if c.has_named_groups() {
            Value::Object(groups_object(rt, c, text, m, &pair))
        } else {
            Value::Undefined
        };
        define_data(&indices, "groups", groups);
        define_data(&result, "indices", Value::Object(indices));
    }
    result
}

/// `String.prototype.match`: every match text for a global regex, else the first match.
pub enum MatchOutcome {
    None,
    All(Vec<JsStr>),
    One(RawMatch),
}

pub fn symbol_match(rx: &RegExpData, text: &JsStr) -> MatchOutcome {
    let flags = rx.compiled.flags;
    if !flags.global {
        return match exec_raw(rx, text.units()) {
            Some(m) => MatchOutcome::One(m),
            None => MatchOutcome::None,
        };
    }
    rx.set_last_index(0.0);
    let mut out = Vec::new();
    while let Some(m) = exec_raw(rx, text.units()) {
        out.push(text.slice(m.start, m.end));
        if m.start == m.end {
            let at = to_length(rx.last_index()) as usize;
            rx.set_last_index(advance_index(text.units(), at, flags.full_unicode()) as f64);
        }
    }
    if out.is_empty() { MatchOutcome::None } else { MatchOutcome::All(out) }
}

/// The matches `String.prototype.matchAll` iterates: from a copy that starts at the regex's `lastIndex`.
pub fn symbol_match_all(rx: &RegExpData, text: &JsStr, mut each: impl FnMut(&RegExpData, RawMatch) -> JsResult<()>) -> JsResult<()> {
    let matcher = rx.copy();
    matcher.set_last_index(to_length(rx.last_index()));
    let unicode = matcher.compiled.flags.full_unicode();
    let global = matcher.compiled.flags.global;
    while let Some(m) = exec_raw(&matcher, text.units()) {
        let empty = m.start == m.end;
        each(&matcher, m)?;
        if !global {
            break;
        }
        if empty {
            let at = to_length(matcher.last_index()) as usize;
            matcher.set_last_index(advance_index(text.units(), at, unicode) as f64);
        }
    }
    Ok(())
}

/// The matches `String.prototype.replace` substitutes: one, or all for a global regex. Like JSC, a non-global
/// regex searches from the start (a sticky one is anchored there) and leaves `lastIndex` alone.
pub fn replace_matches(rx: &RegExpData, text: &JsStr) -> Vec<RawMatch> {
    let flags = rx.compiled.flags;
    if !flags.global {
        return find_at(&rx.compiled, text.units(), 0, flags.sticky).into_iter().collect();
    }
    rx.set_last_index(0.0);
    let mut out = Vec::new();
    while let Some(m) = exec_raw(rx, text.units()) {
        let empty = m.start == m.end;
        out.push(m);
        if empty {
            let at = to_length(rx.last_index()) as usize;
            rx.set_last_index(advance_index(text.units(), at, flags.full_unicode()) as f64);
        }
    }
    out
}

/// The capture texts of a match (`undefined` for groups that did not participate).
pub fn capture_values(text: &JsStr, m: &RawMatch) -> Vec<Value> {
    m.captures.iter().map(|r| slice_value(text, *r)).collect()
}

/// The `groups` object passed to replacer functions, when the regex has named groups.
pub fn replacer_groups(rt: &Rt, rx: &RegExpData, text: &JsStr, m: &RawMatch) -> Option<Obj> {
    let c = &rx.compiled;
    if !c.has_named_groups() {
        return None;
    }
    Some(groups_object(rt, c, text, m, &|r| slice_value(text, r)))
}

/// Named capture texts for GetSubstitution's `$<name>`: `None` when the regex has no named groups.
fn named_captures(c: &Compiled, text: &JsStr, m: &RawMatch) -> Option<Vec<(String, Option<JsStr>)>> {
    if !c.has_named_groups() {
        return None;
    }
    let mut out = Vec::new();
    for name in c.unique_names() {
        let range = c
            .names
            .iter()
            .enumerate()
            .filter(|(_, n)| n.as_deref() == Some(name))
            .find_map(|(i, _)| m.captures.get(i).cloned().flatten());
        out.push((name.to_string(), range.map(|(s, e)| text.slice(s, e))));
    }
    Some(out)
}

/// GetSubstitution: expands `$$ $& $\` $' $n $nn $<name>` in `template`.
pub fn get_substitution(
    matched: &[u16],
    text: &[u16],
    position: usize,
    captures: &[Option<JsStr>],
    named: Option<&[(String, Option<JsStr>)]>,
    template: &[u16],
    out: &mut Vec<u16>,
) {
    let dollar = b'$' as u16;
    let digit = |u: u16| (b'0' as u16..=b'9' as u16).contains(&u);
    let mut i = 0;
    while i < template.len() {
        let c = template[i];
        if c != dollar || i + 1 >= template.len() {
            out.push(c);
            i += 1;
            continue;
        }
        let next = template[i + 1];
        match next {
            0x24 => {
                out.push(dollar);
                i += 2;
            }
            0x26 => {
                out.extend_from_slice(matched);
                i += 2;
            }
            0x60 => {
                out.extend_from_slice(&text[..position.min(text.len())]);
                i += 2;
            }
            0x27 => {
                let tail = position + matched.len();
                if tail < text.len() {
                    out.extend_from_slice(&text[tail..]);
                }
                i += 2;
            }
            n if digit(n) => {
                let count = captures.len();
                let two = i + 2 < template.len() && digit(template[i + 2]);
                let mut digits = if two { 2 } else { 1 };
                let mut index = (n - b'0' as u16) as usize;
                if two {
                    index = index * 10 + (template[i + 2] - b'0' as u16) as usize;
                    if index > count {
                        digits = 1;
                        index = (n - b'0' as u16) as usize;
                    }
                }
                if index >= 1 && index <= count {
                    if let Some(s) = &captures[index - 1] {
                        out.extend_from_slice(s.units());
                    }
                } else {
                    out.extend_from_slice(&template[i..i + 1 + digits]);
                }
                i += 1 + digits;
            }
            0x3C => {
                let close = template[i + 2..].iter().position(|&u| u == b'>' as u16);
                match (close, named) {
                    (Some(rel), Some(named)) => {
                        let name = String::from_utf16_lossy(&template[i + 2..i + 2 + rel]);
                        if let Some((_, Some(s))) = named.iter().find(|(n, _)| *n == name) {
                            out.extend_from_slice(s.units());
                        }
                        i += 2 + rel + 1;
                    }
                    _ => {
                        out.extend_from_slice(&template[i..i + 2]);
                        i += 2;
                    }
                }
            }
            _ => {
                out.push(dollar);
                i += 1;
            }
        }
    }
}

/// `string.replace(regex, template)` / `replaceAll` with a substitution template.
pub fn replace_with_template(rx: &RegExpData, text: &JsStr, template: &JsStr) -> JsStr {
    let matches = replace_matches(rx, text);
    let units = text.units();
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut next = 0usize;
    for m in &matches {
        let position = m.start.min(units.len());
        let captures: Vec<Option<JsStr>> = m.captures.iter().map(|r| r.map(|(s, e)| text.slice(s, e))).collect();
        let named = named_captures(&rx.compiled, text, m);
        let mut replacement = Vec::new();
        get_substitution(
            &units[m.start..m.end],
            units,
            position,
            &captures,
            named.as_deref(),
            template.units(),
            &mut replacement,
        );
        if position >= next {
            out.extend_from_slice(&units[next..position]);
            out.extend_from_slice(&replacement);
            next = position + (m.end - m.start);
        }
    }
    if next < units.len() {
        out.extend_from_slice(&units[next..]);
    }
    JsStr::from_units(out)
}

/// `String.prototype.search`: the index of the first match from 0, leaving `lastIndex` as it was.
pub fn symbol_search(rx: &RegExpData, text: &JsStr) -> f64 {
    let previous = rx.last_index();
    rx.set_last_index(0.0);
    let result = exec_raw(rx, text.units());
    rx.set_last_index(previous);
    match result {
        Some(m) => m.start as f64,
        None => -1.0,
    }
}

/// `String.prototype.split` with a RegExp separator, on a sticky copy so the original's `lastIndex` is untouched.
pub fn symbol_split(rx: &RegExpData, text: &JsStr, limit: Option<f64>) -> Vec<Value> {
    let lim = match limit {
        None => u32::MAX,
        Some(n) => crate::number::to_uint32(n),
    };
    let mut out: Vec<Value> = Vec::new();
    if lim == 0 {
        return out;
    }
    let splitter = rx.with_flags(Flags { sticky: true, ..rx.compiled.flags });
    let c = &splitter.compiled;
    let units = text.units();
    let size = units.len();
    let unicode = c.flags.full_unicode();
    if size == 0 {
        if find_at(c, units, 0, true).is_none() {
            out.push(Value::String(text.clone()));
        }
        return out;
    }
    let mut p = 0usize;
    let mut q = p;
    while q < size {
        // The next position at or after q where the sticky splitter matches.
        let Some(m) = find_at(c, units, q, false) else { break };
        if m.start >= size {
            break;
        }
        let e = m.end.min(size);
        if e == p {
            q = advance_index(units, m.start, unicode);
            continue;
        }
        out.push(Value::String(text.slice(p, m.start)));
        // Past the array limit the caller fails anyway; do not build more than that.
        if out.len() as u32 == lim || out.len() > crate::interp::limits::MAX_ARRAY_LENGTH {
            return out;
        }
        p = e;
        for r in &m.captures {
            out.push(slice_value(text, *r));
            if out.len() as u32 == lim {
                return out;
            }
        }
        q = p;
    }
    out.push(Value::String(text.slice(p, size)));
    out
}

// ----- RegExp.escape -----

fn is_escape_whitespace(c: u32) -> bool {
    matches!(
        c,
        0x09 | 0x0A | 0x0B | 0x0C | 0x0D | 0x20 | 0xA0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF
    )
}

/// `RegExp.escape(string)` (EncodeForRegExpEscape per code point).
pub fn regexp_escape(s: &JsStr) -> JsStr {
    let units = s.units();
    let mut out: Vec<u16> = Vec::with_capacity(units.len() * 2);
    let mut i = 0;
    let push_hex = |out: &mut Vec<u16>, text: String| out.extend(text.encode_utf16());
    while i < units.len() {
        let c = units[i];
        let (cp, width) = if (0xD800..0xDC00).contains(&c) && i + 1 < units.len() && (0xDC00..0xE000).contains(&units[i + 1]) {
            (0x10000 + (((c as u32) - 0xD800) << 10) + ((units[i + 1] as u32) - 0xDC00), 2)
        } else {
            (c as u32, 1)
        };
        if i == 0 && char::from_u32(cp).is_some_and(|ch| ch.is_ascii_alphanumeric()) {
            push_hex(&mut out, format!("\\x{cp:02x}"));
        } else if char::from_u32(cp).is_some_and(|ch| "^$\\.*+?()[]{}|/".contains(ch)) {
            out.push(b'\\' as u16);
            out.push(cp as u16);
        } else if let Some(letter) = match cp {
            0x09 => Some('t'),
            0x0A => Some('n'),
            0x0B => Some('v'),
            0x0C => Some('f'),
            0x0D => Some('r'),
            _ => None,
        } {
            out.push(b'\\' as u16);
            out.push(letter as u16);
        } else if char::from_u32(cp).is_some_and(|ch| ",-=<>#&!%:;@~'`\"".contains(ch))
            || is_escape_whitespace(cp)
            || (0xD800..0xE000).contains(&cp)
        {
            if cp <= 0xFF {
                push_hex(&mut out, format!("\\x{cp:02x}"));
            } else {
                push_hex(&mut out, format!("\\u{cp:04x}"));
            }
        } else {
            out.extend_from_slice(&units[i..i + width]);
        }
        i += width;
    }
    JsStr::from_units(out)
}

// ----- the RegExp global -----

const FLAG_PROPERTIES: [&str; 8] =
    ["hasIndices", "global", "ignoreCase", "multiline", "sticky", "unicode", "unicodeSets", "dotAll"];

fn this_regexp(this: &Value, name: &str) -> JsResult<Obj> {
    receiver(this, &format!("RegExp.prototype.{name}"), |o| matches!(o.kind(), ObjKind::RegExp(_)))
}

fn regexp_of(o: &Obj) -> &RegExpData {
    regexp_data(o).expect("RegExp receiver")
}

/// RegExp constructs identically with or without new, like JS.
pub fn regexp_global(rt: &Rt) -> Value {
    let b = &rt.builtins;
    let proto = b.regexp.clone();
    let ctor = constructor(
        &b.function,
        &proto,
        "RegExp",
        2,
        NativeFn {
            call: Rc::new(|rt, _, _, args| NResult::Ready(construct_regexp(&args, &rt.builtins.regexp).map(Value::Object))),
            construct: Some(Rc::new(|rt, _, args, new_target| {
                let proto = prototype_from(&new_target, &rt.builtins.regexp);
                NResult::Ready(construct_regexp(&args, &proto).map(Value::Object))
            })),
            callback: true,
        },
    );
    methods(
        &b.function,
        &ctor,
        vec![method("escape", 1, |_, _, _, args| match arg(&args, 0) {
            Value::String(s) => NResult::ok(regexp_escape(&s)),
            _ => NResult::err(type_error("RegExp.escape expects a string.")),
        })],
    );

    define_accessor(
        &proto,
        "source",
        Some(Rc::new(|this: &Value| Ok(Value::String(regexp_of(&this_regexp(this, "source")?).source_js())))),
        None,
    );
    define_accessor(
        &proto,
        "flags",
        Some(Rc::new(|this: &Value| Ok(Value::str(&regexp_of(&this_regexp(this, "flags")?).flags)))),
        None,
    );
    // The RegExp holds the only lastIndex, so exec/test and the String methods share one counter.
    define_accessor(
        &proto,
        "lastIndex",
        Some(Rc::new(|this: &Value| Ok(Value::Number(regexp_of(&this_regexp(this, "lastIndex")?).last_index())))),
        Some(Rc::new(|this: &Value, value: Value| {
            let o = this_regexp(this, "lastIndex")?;
            regexp_of(&o).set_last_index(coerce_to_number(&value));
            Ok(())
        })),
    );
    for name in FLAG_PROPERTIES {
        define_accessor(
            &proto,
            name,
            Some(Rc::new(move |this: &Value| Ok(Value::Bool(regexp_of(&this_regexp(this, name)?).flag_set().get(name))))),
            None,
        );
    }
    let run = |name: &'static str| {
        method(name, 1, move |rt, _, this, args| {
            let o = match this_regexp(&this, name) {
                Ok(o) => o,
                Err(e) => return NResult::err(e),
            };
            let rx = regexp_of(&o);
            let text = coerce_to_string(&arg(&args, 0));
            let matched = exec_raw(rx, text.units());
            if name == "test" {
                return NResult::ok(matched.is_some());
            }
            NResult::ok(match matched {
                Some(m) => Value::Object(match_to_value(rt, rx, &text, &m)),
                None => Value::Null,
            })
        })
    };
    methods(
        &b.function,
        &proto,
        vec![
            run("exec"),
            run("test"),
            method("toString", 0, |_, _, this, _| match this_regexp(&this, "toString") {
                Ok(o) => NResult::ok(regexp_of(&o).display_js()),
                Err(e) => NResult::err(e),
            }),
        ],
    );
    Value::Object(ctor)
}
