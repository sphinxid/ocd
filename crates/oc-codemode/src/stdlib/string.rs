//! `String` and `String.prototype`. Port of `stdlib/string.ts`.
//!
//! Upstream runs every method on the host's (JavaScriptCore's) string implementation after converting the receiver
//! and arguments itself; here the host algorithms are implemented over UTF-16 code units, so lengths, indexes, and
//! lone surrogates behave as in JS. The regular-expression halves (`match`, `matchAll`, `replace`, `search`,
//! `split`) run on [`crate::stdlib::regexp`]. `localeCompare` approximates the ICU root collation JSC uses (see
//! [`locale_compare`]).

use std::rc::Rc;

use unicode_normalization::UnicodeNormalization;

use crate::data::quote_units;
use crate::interp::callback::{apply_collection_callback, is_supported_callback};
use crate::interp::coerce::{coerce_to_integer, coerce_to_number, coerce_to_string, needs_conversion, primitives, to_primitive_string, Hint};
use crate::interp::errors::{invalid_data, range_error, type_error, JsResult};
use crate::interp::function::{arg, constructor, fn_, method, methods, Method, NResult, NativeFn};
use crate::interp::iteration::{iterator_object, string_code_points, ListCursor};
use crate::interp::limits::{check_array_length, MAX_ARRAY_LENGTH, MAX_STRING_LENGTH};
use crate::interp::references::contains_opaque_reference;
use crate::interp::{Rt, Site};
use crate::number::{to_integer_or_infinity, to_uint32, trim_units};
use crate::object::{define, get, index_key, Obj, HIDDEN};
use crate::stdlib::regexp::{
    as_regexp, capture_values, get_substitution, match_to_value, replace_matches, replace_with_template, replacer_groups,
    symbol_match, symbol_match_all, symbol_search, symbol_split, to_host_regex, MatchOutcome, RegExpData,
};
use crate::value::{JsStr, PropKey, Sym, Value};

macro_rules! tri {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return NResult::err(e),
        }
    };
}

// ----- argument helpers -----

/// `checkStringLength` on a JS number (NaN passes, as `NaN > max` is false).
fn check_length(length: f64) -> JsResult<()> {
    if length > MAX_STRING_LENGTH as f64 {
        return Err(range_error("Invalid string length"));
    }
    Ok(())
}

fn require_data(name: &str, index: usize, value: &Value) -> JsResult<Value> {
    if contains_opaque_reference(value) {
        return Err(invalid_data(format!("String.{name} expects argument {} to be a data value.", index + 1)));
    }
    Ok(value.clone())
}

/// Argument `index` as a string; opaque runtime references reject.
fn str_arg(name: &str, args: &[Value], index: usize) -> JsResult<JsStr> {
    Ok(coerce_to_string(&require_data(name, index, &arg(args, index))?))
}

fn num_arg(name: &str, args: &[Value], index: usize) -> JsResult<f64> {
    Ok(coerce_to_number(&require_data(name, index, &arg(args, index))?))
}

fn opt_num(name: &str, args: &[Value], index: usize) -> JsResult<Option<f64>> {
    if arg(args, index).is_undefined() { Ok(None) } else { num_arg(name, args, index).map(Some) }
}

fn opt_str(name: &str, args: &[Value], index: usize) -> JsResult<Option<JsStr>> {
    if arg(args, index).is_undefined() { Ok(None) } else { str_arg(name, args, index).map(Some) }
}

/// `withPrimitives` whose body may itself suspend.
fn with_prims(
    rt: &Rt,
    site: Site,
    hints: Vec<Hint>,
    values: Vec<Value>,
    body: impl FnOnce(Vec<Value>) -> NResult + 'static,
) -> NResult {
    if !needs_conversion(&values) {
        return body(values);
    }
    let rt = rt.clone();
    NResult::future(async move {
        let converted = primitives(&rt, &hints, values, site).await?;
        body(converted).resolve().await
    })
}

fn ready<T: Into<Value>>(result: JsResult<T>) -> NResult {
    match result {
        Ok(v) => NResult::ok(v),
        Err(e) => NResult::err(e),
    }
}

fn units_value(units: Vec<u16>) -> Value {
    Value::String(JsStr::from_units(units))
}

type Op = Rc<dyn Fn(&Rt, Site, JsStr, Vec<Value>) -> NResult>;

/// A prototype method: ToPrimitive in spec order (the receiver as a string, then the arguments the method
/// consumes, one hint per position; the rest pass through as they are), then `op` on the receiver's string.
fn simple(
    name: &'static str,
    length: usize,
    hints: &[Hint],
    op: impl Fn(&Rt, Site, JsStr, Vec<Value>) -> NResult + 'static,
) -> Method {
    let hints: Vec<Hint> = hints.to_vec();
    let op: Op = Rc::new(op);
    method(name, length, move |rt, site, this, args| {
        if this.is_nullish() {
            return NResult::err(type_error(format!("String.prototype.{name} called on null or undefined.")));
        }
        let count = hints.len();
        let mut all_hints = Vec::with_capacity(count + 1);
        all_hints.push(Hint::String);
        all_hints.extend_from_slice(&hints);
        let mut values = vec![this];
        values.extend(args.iter().take(count).cloned());
        let rest: Vec<Value> = args.iter().skip(count).cloned().collect();
        let (op, rt2) = (op.clone(), rt.clone());
        with_prims(rt, site, all_hints, values, move |converted| {
            let mut items = converted.into_iter();
            let value = coerce_to_string(&items.next().unwrap_or(Value::Undefined));
            let mut primitives: Vec<Value> = items.collect();
            primitives.extend(rest);
            op(&rt2, site, value, primitives)
        })
    })
}

/// `simple` for a method whose result is computed synchronously.
fn simple_sync(
    name: &'static str,
    length: usize,
    hints: &[Hint],
    op: impl Fn(&JsStr, &[Value]) -> JsResult<Value> + 'static,
) -> Method {
    simple(name, length, hints, move |_, _, value, args| ready(op(&value, &args)))
}

// ----- UTF-16 algorithms -----

/// A relative index (`slice`, `at`): negative counts from the end; clamped to `0..=len`.
fn relative(n: f64, len: usize) -> usize {
    let length = len as f64;
    if n < 0.0 { (length + n).max(0.0) as usize } else { n.min(length) as usize }
}

/// An index clamped to `0..=len` (ToIntegerOrInfinity first).
fn clamp(n: f64, len: usize) -> usize {
    to_integer_or_infinity(n).max(0.0).min(len as f64) as usize
}

/// StringIndexOf: the first occurrence of `needle` at or after `from`.
fn index_of(hay: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return if from <= hay.len() { Some(from) } else { None };
    }
    if needle.len() > hay.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

fn is_lead(u: u16) -> bool {
    (0xD800..0xDC00).contains(&u)
}

fn is_trail(u: u16) -> bool {
    (0xDC00..0xE000).contains(&u)
}

/// The code point at `i` (a lone surrogate is its own value) and its width in code units.
fn code_point_at(units: &[u16], i: usize) -> (u32, usize) {
    let c = units[i];
    if is_lead(c) && i + 1 < units.len() && is_trail(units[i + 1]) {
        (0x10000 + (((c as u32) - 0xD800) << 10) + ((units[i + 1] as u32) - 0xDC00), 2)
    } else {
        (c as u32, 1)
    }
}

/// Splits UTF-16 into runs of well-formed text and lone surrogates.
enum Run {
    Text(String),
    Lone(u16),
}

fn runs(units: &[u16]) -> Vec<Run> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut i = 0;
    while i < units.len() {
        let (cp, width) = code_point_at(units, i);
        match char::from_u32(cp) {
            Some(c) => text.push(c),
            None => {
                if !text.is_empty() {
                    out.push(Run::Text(std::mem::take(&mut text)));
                }
                out.push(Run::Lone(units[i]));
            }
        }
        i += width;
    }
    if !text.is_empty() {
        out.push(Run::Text(text));
    }
    out
}

/// Applies a text transformation to the well-formed runs, keeping lone surrogates as they are.
fn map_text(units: &[u16], f: impl Fn(&str) -> String) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    for run in runs(units) {
        match run {
            Run::Text(t) => out.extend(f(&t).encode_utf16()),
            Run::Lone(u) => out.push(u),
        }
    }
    out
}

fn to_lower(units: &[u16]) -> Vec<u16> {
    if units.iter().all(|&u| u < 0x80) {
        return units.iter().map(|&u| if (b'A' as u16..=b'Z' as u16).contains(&u) { u + 32 } else { u }).collect();
    }
    map_text(units, |s| s.to_lowercase())
}

fn to_upper(units: &[u16]) -> Vec<u16> {
    if units.iter().all(|&u| u < 0x80) {
        return units.iter().map(|&u| if (b'a' as u16..=b'z' as u16).contains(&u) { u - 32 } else { u }).collect();
    }
    map_text(units, |s| s.to_uppercase())
}

fn is_well_formed(units: &[u16]) -> bool {
    let mut i = 0;
    while i < units.len() {
        let (cp, width) = code_point_at(units, i);
        if (0xD800..0xE000).contains(&cp) {
            return false;
        }
        i += width;
    }
    true
}

fn to_well_formed(units: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let (cp, width) = code_point_at(units, i);
        if (0xD800..0xE000).contains(&cp) {
            out.push(0xFFFD);
        } else {
            out.extend_from_slice(&units[i..i + width]);
        }
        i += width;
    }
    out
}

fn trim_start(units: &[u16]) -> &[u16] {
    let start = units.iter().position(|&c| !crate::number::is_js_whitespace(c)).unwrap_or(units.len());
    &units[start..]
}

fn trim_end(units: &[u16]) -> &[u16] {
    let end = units.iter().rposition(|&c| !crate::number::is_js_whitespace(c)).map(|i| i + 1).unwrap_or(0);
    &units[..end]
}

/// `padStart`/`padEnd` after the length checks.
fn pad(units: &[u16], max_length: f64, filler: Option<&JsStr>, at_start: bool) -> Vec<u16> {
    let int_max = if max_length.is_nan() || max_length <= 0.0 { 0.0 } else { max_length.floor() };
    if int_max <= units.len() as f64 {
        return units.to_vec();
    }
    let fill: Vec<u16> = match filler {
        None => vec![b' ' as u16],
        Some(f) => f.units().to_vec(),
    };
    if fill.is_empty() {
        return units.to_vec();
    }
    let fill_len = int_max as usize - units.len();
    let mut padding: Vec<u16> = Vec::with_capacity(fill_len);
    while padding.len() < fill_len {
        let take = (fill_len - padding.len()).min(fill.len());
        padding.extend_from_slice(&fill[..take]);
    }
    let mut out = Vec::with_capacity(int_max as usize);
    if at_start {
        out.extend_from_slice(&padding);
        out.extend_from_slice(units);
    } else {
        out.extend_from_slice(units);
        out.extend_from_slice(&padding);
    }
    out
}

/// `String.prototype.split` with a string separator (`lim` already ToUint32).
fn split_string(value: &JsStr, separator: &JsStr, lim: u32) -> Vec<Value> {
    let mut out = Vec::new();
    if lim == 0 {
        return out;
    }
    let s = value.units();
    let sep = separator.units();
    if sep.is_empty() {
        // Past the array limit the caller fails anyway; do not build more than that.
        let take = s.len().min(lim as usize).min(MAX_ARRAY_LENGTH + 1);
        return s[..take].iter().map(|&u| Value::String(JsStr::from_slice(&[u]))).collect();
    }
    if s.is_empty() {
        return vec![Value::String(value.clone())];
    }
    let mut p = 0;
    let mut q = index_of(s, sep, 0);
    while let Some(at) = q {
        out.push(Value::String(value.slice(p, at)));
        if out.len() as u32 == lim || out.len() > MAX_ARRAY_LENGTH {
            return out;
        }
        p = at + sep.len();
        q = index_of(s, sep, p);
    }
    out.push(Value::String(value.slice(p, s.len())));
    out
}

/// `replace`/`replaceAll` with a string pattern: the positions it replaces.
fn string_positions(value: &[u16], search: &[u16], all: bool) -> Vec<usize> {
    let mut out = Vec::new();
    let advance = search.len().max(1);
    let mut position = index_of(value, search, 0);
    while let Some(p) = position {
        out.push(p);
        if !all {
            break;
        }
        position = index_of(value, search, p + advance);
    }
    out
}

/// `string.replace(search, template)` / `replaceAll` with a string pattern and a substitution template.
fn replace_string_template(value: &JsStr, search: &JsStr, template: &JsStr, all: bool) -> JsStr {
    let units = value.units();
    let positions = string_positions(units, search.units(), all);
    if positions.is_empty() {
        return value.clone();
    }
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut end = 0;
    for p in positions {
        out.extend_from_slice(&units[end..p]);
        get_substitution(search.units(), units, p, &[], None, template.units(), &mut out);
        end = p + search.len();
    }
    out.extend_from_slice(&units[end.min(units.len())..]);
    JsStr::from_units(out)
}

fn normalize(units: &[u16], form: &str) -> Vec<u16> {
    map_text(units, |s| match form {
        "NFC" => s.nfc().collect(),
        "NFD" => s.nfd().collect(),
        "NFKC" => s.nfkc().collect(),
        _ => s.nfkd().collect(),
    })
}

// ----- localeCompare -----

/// Characters the root collation ignores entirely.
fn is_ignorable(cp: u32) -> bool {
    matches!(cp, 0x00..=0x08 | 0x0E..=0x1F | 0x7F..=0x84 | 0x86..=0x9F | 0xAD | 0x200B..=0x200F | 0x2060 | 0xFEFF)
}

/// Whitespace, punctuation, and symbols in their root-collation order (ahead of digits and letters), as
/// (code point, primary, tertiary): `‘` and `’` differ from `'` only at the tertiary level, for example.
const VARIABLE_ORDER: &[(u32, u32, u32)] = &[
    (0x9, 0, 0), (0xa, 1, 0), (0xb, 2, 0), (0xc, 3, 0), (0xd, 4, 0), (0x85, 5, 0), (0x2028, 6, 0), (0x2029, 7, 0),
    (0x20, 8, 0), (0x3000, 8, 1), (0x1680, 8, 2), (0x2000, 8, 2), (0x205f, 8, 2), (0xa0, 8, 3), (0x202f, 8, 3),
    (0x5f, 9, 0), (0x2d, 10, 0), (0x2013, 11, 0), (0x2014, 12, 0), (0x301c, 13, 0), (0x30fb, 14, 0), (0x2c, 15, 0),
    (0x3001, 16, 0), (0x3b, 17, 0), (0x3a, 18, 0), (0x21, 19, 0), (0xa1, 20, 0), (0x3f, 21, 0), (0xbf, 22, 0),
    (0x203d, 23, 0), (0x2e, 24, 0), (0x3002, 25, 0), (0xb7, 26, 0), (0x27, 27, 0), (0x2018, 27, 1), (0x2019, 27, 2),
    (0x2039, 28, 0), (0x203a, 29, 0), (0x22, 30, 0), (0x201c, 30, 1), (0x201d, 30, 2), (0xab, 31, 0), (0xbb, 32, 0),
    (0x28, 33, 0), (0x29, 34, 0), (0x5b, 35, 0), (0x5d, 36, 0), (0x7b, 37, 0), (0x7d, 38, 0), (0x3008, 39, 0),
    (0x3009, 40, 0), (0x300a, 41, 0), (0x300b, 42, 0), (0x300c, 43, 0), (0x300d, 44, 0), (0x300e, 45, 0), (0x300f,
    46, 0), (0x3010, 47, 0), (0x3011, 48, 0), (0xa7, 49, 0), (0xb6, 50, 0), (0x40, 51, 0), (0x2a, 52, 0), (0x2f, 53,
    0), (0x5c, 54, 0), (0x26, 55, 0), (0x23, 56, 0), (0x25, 57, 0), (0x2030, 58, 0), (0x2022, 59, 0), (0x2032, 60,
    0), (0x3003, 61, 0), (0x203b, 62, 0), (0x60, 63, 0), (0xb4, 64, 0), (0x2dc, 65, 0), (0x5e, 66, 0), (0xaf, 67,
    0), (0x2d8, 68, 0), (0x2d9, 69, 0), (0xa8, 70, 0), (0x2da, 71, 0), (0x2dd, 72, 0), (0xb8, 73, 0), (0x2db, 74,
    0), (0x2b9, 75, 0), (0x2ba, 76, 0), (0x2c8, 77, 0), (0x2cc, 78, 0), (0xb0, 79, 0), (0xa9, 80, 0), (0xae, 81, 0),
    (0x2190, 82, 0), (0x2192, 83, 0), (0x2191, 84, 0), (0x2193, 85, 0), (0x2211, 86, 0), (0x2b, 87, 0), (0xb1, 88,
    0), (0xf7, 89, 0), (0xd7, 90, 0), (0x3c, 91, 0), (0x3d, 92, 0), (0x3e, 93, 0), (0xac, 94, 0), (0x7c, 95, 0),
    (0xa6, 96, 0), (0x7e, 97, 0), (0x2212, 98, 0), (0x2044, 99, 0), (0x221a, 100, 0), (0x221e, 101, 0), (0x2264,
    102, 0), (0x2265, 103, 0), (0x2605, 104, 0), (0x2606, 105, 0), (0x2665, 106, 0), (0x2713, 107, 0), (0x3012, 108,
    0), (0x2d0, 109, 0), (0x3005, 110, 0), (0x30fc, 111, 0), (0xa4, 112, 0), (0xa2, 113, 0), (0x24, 114, 0), (0xa3,
    115, 0), (0xa5, 116, 0), (0x20a9, 117, 0), (0x20ac, 118, 0), (0x20b9, 119, 0), (0x20bd, 120, 0)
];

fn variable_entry(cp: u32) -> Option<(u32, u32)> {
    VARIABLE_ORDER.iter().find(|(c, _, _)| *c == cp).map(|(_, w, t)| (*w, *t))
}

fn variable_weight(cp: u32) -> Option<u32> {
    variable_entry(cp).map(|(w, _)| w)
}

/// The zero of each run of decimal digits outside ASCII (Arabic-Indic, Devanagari, ...): digits compare equal to
/// their ASCII counterparts.
const DIGIT_ZEROS: &[u32] = &[
    0x660, 0x6F0, 0x7C0, 0x966, 0x9E6, 0xA66, 0xAE6, 0xB66, 0xBE6, 0xC66, 0xCE6, 0xD66, 0xDE6, 0xE50, 0xED0, 0xF20,
    0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80, 0x1A90, 0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0,
    0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0, 0xFF10,
];

fn digit_value(cp: u32) -> Option<u32> {
    if (0x30..=0x39).contains(&cp) {
        return Some(cp - 0x30);
    }
    DIGIT_ZEROS.iter().find(|&&z| (z..z + 10).contains(&cp)).map(|z| cp - z)
}

/// Combining marks in their root-collation (secondary) order; marks sharing a weight are listed together.
const MARK_ORDER: &[&[u32]] = &[
    &[0x34F], &[0x332], &[0x313, 0x343], &[0x314], &[0x301, 0x341], &[0x300, 0x340], &[0x306], &[0x302],
    &[0x30C], &[0x30A], &[0x342], &[0x308], &[0x344], &[0x30B], &[0x303], &[0x307], &[0x338], &[0x327],
    &[0x328], &[0x304], &[0x30D, 0x30E],
];

/// Letters with a primary weight of their own right after a Latin letter (`ə` and `ɛ` after `e`, ...).
fn latin_extra(c: char) -> Option<(char, u32)> {
    Some(match c {
        'ə' => ('e', 1),
        'ɛ' => ('e', 2),
        'ı' => ('i', 1),
        'ŋ' => ('n', 1),
        'ɔ' => ('o', 1),
        'ĸ' => ('q', 1),
        'ʃ' => ('s', 1),
        'ʒ' => ('z', 1),
        'þ' => ('z', 2),
        'ʼ' => ('z', 3),
        _ => return None,
    })
}

/// Katakana sort with the matching hiragana (a tertiary difference).
fn is_katakana(cp: u32) -> bool {
    (0x30A1..=0x30F6).contains(&cp)
}

/// Latin letters that are a base letter with a stroke (a secondary difference).
fn stroked(c: char) -> Option<char> {
    Some(match c {
        'ø' => 'o',
        'Ø' => 'O',
        'đ' => 'd',
        'Đ' => 'D',
        'ħ' => 'h',
        'Ħ' => 'H',
        'ł' => 'l',
        'Ł' => 'L',
        'ŧ' => 't',
        'Ŧ' => 'T',
        'ƀ' => 'b',
        'ɨ' => 'i',
        _ => return None,
    })
}

/// Latin ligatures and letters the root collation expands.
fn expansion(c: char) -> Option<&'static str> {
    Some(match c {
        'æ' => "ae",
        'Æ' => "AE",
        'œ' => "oe",
        'Œ' => "OE",
        'ß' => "ss",
        'ẞ' => "SS",
        _ => return None,
    })
}

#[derive(Default)]
struct SortKey {
    primary: Vec<u32>,
    secondary: Vec<u32>,
    tertiary: Vec<u32>,
}

fn mark_weight(cp: u32) -> u32 {
    for (i, group) in MARK_ORDER.iter().enumerate() {
        if group.contains(&cp) {
            return 2 + i as u32;
        }
    }
    0x1000 + cp
}

/// Primary weights: each symbol-table entry has a region of its own; digits, Latin letters, other letters, and
/// everything else follow in regions after the table.
const REGION: u32 = 1 << 24;

fn table_end() -> u32 {
    VARIABLE_ORDER.last().map_or(0, |(_, w, _)| *w) + 1
}

/// Cyrillic letters with a primary weight of their own right after another (`і` and `й` after `и`, ...).
fn cyrillic_extra(c: char) -> Option<(char, u32)> {
    Some(match c {
        'ә' => ('а', 1),
        'ғ' => ('г', 1),
        'ђ' => ('д', 1),
        'є' => ('е', 1),
        'ѕ' => ('з', 1),
        'і' => ('и', 1),
        'й' => ('и', 2),
        'ј' => ('и', 3),
        'қ' => ('к', 1),
        'љ' => ('л', 1),
        'ң' => ('н', 1),
        'њ' => ('н', 2),
        'ө' => ('о', 1),
        'ћ' => ('т', 1),
        'ү' => ('у', 1),
        'ұ' => ('у', 2),
        'һ' => ('х', 1),
        'џ' => ('ч', 1),
        _ => return None,
    })
}

fn primary_weight(c: char) -> u32 {
    let cp = c as u32;
    let end = table_end();
    if let Some(w) = variable_weight(cp) {
        return w * REGION;
    }
    if let Some(d) = digit_value(cp) {
        return end * REGION + d;
    }
    let letters = (end + 2) * REGION;
    if is_katakana(cp) {
        return letters + (cp - 0x60) * 8;
    }
    let lower = c.to_lowercase().next().unwrap_or(c);
    if lower.is_ascii_lowercase() {
        return (end + 1) * REGION + (lower as u32 - 'a' as u32) * 4;
    }
    if let Some((base, step)) = latin_extra(lower) {
        return (end + 1) * REGION + (base as u32 - 'a' as u32) * 4 + step;
    }
    if let Some((base, step)) = cyrillic_extra(lower) {
        return letters + base as u32 * 8 + step;
    }
    if c.is_alphabetic() || c.is_numeric() {
        return letters + lower as u32 * 8;
    }
    if !c.is_alphanumeric() && is_assigned(c) {
        // Other symbols sort by code point among the table's symbols, before the CJK marks and currency signs.
        let check = variable_weight(0x2713).unwrap_or(0);
        return check * REGION + 1 + cp;
    }
    (end + 3) * REGION + cp
}

fn is_assigned(c: char) -> bool {
    unicode_normalization::char::is_public_assigned(c)
}

fn push_base(key: &mut SortKey, c: char, compat: bool) {
    key.primary.push(primary_weight(c));
    key.secondary.push(0);
    let upper = c.is_uppercase() || (c.to_lowercase().next() != Some(c) && !c.is_lowercase());
    let variant = compat || is_katakana(c as u32);
    key.tertiary.push(u32::from(upper) + if variant { 2 } else { 0 });
}

fn push_char(key: &mut SortKey, c: char) {
    let cp = c as u32;
    if is_ignorable(cp) {
        return;
    }
    if let Some((w, t)) = variable_entry(cp) {
        key.primary.push(w * REGION);
        key.secondary.push(0);
        key.tertiary.push(t * 4);
        return;
    }
    if let Some(text) = expansion(c) {
        for e in text.chars() {
            push_base(key, e, true);
        }
        return;
    }
    if let Some(base) = stroked(c) {
        push_base(key, base, false);
        key.secondary.push(mark_weight(0x338));
        key.tertiary.push(0);
        return;
    }
    if let Some(base) = match c {
        'ð' => Some('d'),
        'Ð' => Some('D'),
        'ґ' => Some('г'),
        'Ґ' => Some('Г'),
        _ => None,
    } {
        // A variant of the base letter after its other accented forms.
        push_base(key, base, false);
        key.secondary.push(0xFFF);
        key.tertiary.push(0);
        return;
    }
    if cyrillic_extra(c.to_lowercase().next().unwrap_or(c)).is_some() {
        push_base(key, c, false);
        return;
    }
    let mut canonical = Vec::new();
    unicode_normalization::char::decompose_canonical(c, |d| canonical.push(d));
    let mut compatible = Vec::new();
    unicode_normalization::char::decompose_compatible(c, |d| compatible.push(d));
    let compat = canonical != compatible;
    for d in compatible {
        if unicode_normalization::char::is_combining_mark(d) {
            key.secondary.push(mark_weight(d as u32));
            key.tertiary.push(0);
        } else if d == c || latin_extra(d).is_some() || d.is_ascii() || !needs_split(d) {
            push_base(key, d, compat);
        } else {
            push_char(key, d);
        }
    }
}

/// Whether a decomposition result still needs its own decomposition (only for expansions and stroked letters).
fn needs_split(c: char) -> bool {
    expansion(c).is_some() || stroked(c).is_some()
}

fn sort_key(units: &[u16]) -> SortKey {
    // Canonically equivalent strings compare equal: decompose (and reorder marks) first.
    let mut key = SortKey::default();
    for run in runs(units) {
        match run {
            Run::Text(text) => {
                let chars: Vec<char> = text.nfd().collect();
                let mut i = 0;
                while i < chars.len() {
                    // `й` is a letter of its own, not `и` with a breve.
                    if matches!(chars[i], 'и' | 'И') && chars.get(i + 1) == Some(&'\u{306}') {
                        push_char(&mut key, if chars[i] == 'и' { 'й' } else { 'Й' });
                        i += 2;
                        continue;
                    }
                    push_char(&mut key, chars[i]);
                    i += 1;
                }
            }
            Run::Lone(u) => {
                key.primary.push((table_end() + 3) * REGION + u as u32);
                key.secondary.push(0);
                key.tertiary.push(0);
            }
        }
    }
    key
}

/// `localeCompare` without locale or options: an approximation of the ICU root collation JavaScriptCore uses
/// (tertiary strength, punctuation not ignored): whitespace and punctuation, then digits, then letters
/// alphabetically; accents break ties before case, and lowercase sorts before uppercase. Returns -1, 0, or 1.
pub fn locale_compare(a: &[u16], b: &[u16]) -> f64 {
    if a == b {
        return 0.0;
    }
    let (ka, kb) = (sort_key(a), sort_key(b));
    let order = ka
        .primary
        .cmp(&kb.primary)
        .then_with(|| ka.secondary.cmp(&kb.secondary))
        .then_with(|| ka.tertiary.cmp(&kb.tertiary));
    match order {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Equal => 0.0,
        std::cmp::Ordering::Greater => 1.0,
    }
}

// ----- regular-expression methods -----

fn replace_all_needs_global(rx: &RegExpData) -> JsResult<()> {
    if !rx.flag_set().global {
        return Err(type_error(format!(
            "String.replaceAll requires a regular expression with the global (g) flag: write /{}/{}g, or use String.replace to replace only the first match.",
            rx.source, rx.flags
        )));
    }
    Ok(())
}

/// `withPattern`: a RegExp is read as is; anything else converts to its pattern text.
fn with_pattern(rt: &Rt, site: Site, args: &[Value], op: impl FnOnce(Value) -> JsResult<Value> + 'static) -> NResult {
    let first = arg(args, 0);
    if as_regexp(&first).is_some() {
        return ready(op(first));
    }
    with_prims(rt, site, vec![Hint::String], vec![first], move |p| ready(op(p.into_iter().next().unwrap_or_default())))
}

fn string_match(rt: &Rt, value: &JsStr, pattern: &Value) -> JsResult<Value> {
    let host = to_host_regex(pattern, "match", "")?;
    let rx = host.data();
    Ok(match symbol_match(rx, value) {
        MatchOutcome::None => Value::Null,
        MatchOutcome::All(list) => {
            Value::Object(Obj::array(&rt.builtins.array, list.into_iter().map(Value::String).collect()))
        }
        MatchOutcome::One(m) => Value::Object(match_to_value(rt, rx, value, &m)),
    })
}

fn string_match_all(rt: &Rt, value: &JsStr, pattern: &Value) -> JsResult<Value> {
    let host = to_host_regex(pattern, "matchAll", "g")?;
    let rx = host.data();
    if !rx.flag_set().global {
        return Err(type_error(format!(
            "String.matchAll requires a regular expression with the global (g) flag: write /{}/{}g, or use String.match for a single match.",
            rx.source, rx.flags
        )));
    }
    let mut matches: Vec<Value> = Vec::new();
    symbol_match_all(rx, value, |matcher, m| {
        check_array_length((matches.len() + 1) as f64)?;
        matches.push(Value::Object(match_to_value(rt, matcher, value, &m)));
        Ok(())
    })?;
    Ok(Value::Object(Obj::array(&rt.builtins.array, matches)))
}

/// One match a replacer function is called for.
struct Replacement {
    offset: usize,
    length: usize,
    args: Vec<Value>,
}

fn replace_with_callback(rt: &Rt, site: Site, value: JsStr, name: &'static str, pattern: Value, replacer: Value) -> NResult {
    let apply = tri!(apply_collection_callback(&replacer, &format!("String.{name}")));
    let mut found: Vec<Replacement> = Vec::new();
    if let Some(rx) = as_regexp(&pattern) {
        if name == "replaceAll" {
            tri!(replace_all_needs_global(rx));
        }
        for m in replace_matches(rx, &value) {
            let mut args = vec![Value::String(value.slice(m.start, m.end))];
            args.extend(capture_values(&value, &m));
            args.push(Value::Number(m.start as f64));
            args.push(Value::String(value.clone()));
            if let Some(groups) = replacer_groups(rt, rx, &value, &m) {
                args.push(Value::Object(groups));
            }
            found.push(Replacement { offset: m.start, length: m.end - m.start, args });
        }
    } else {
        let search = tri!(require_data(name, 0, &pattern).map(|p| coerce_to_string(&p)));
        for p in string_positions(value.units(), search.units(), name == "replaceAll") {
            found.push(Replacement {
                offset: p,
                length: search.len(),
                args: vec![Value::String(search.clone()), Value::Number(p as f64), Value::String(value.clone())],
            });
        }
    }
    let rt = rt.clone();
    NResult::future(async move {
        let units = value.units();
        let mut out: Vec<u16> = Vec::with_capacity(units.len());
        let mut end = 0usize;
        for r in found {
            let replacement = apply(&rt, r.args, Value::Undefined, site).await?;
            if r.offset > end {
                out.extend_from_slice(&units[end..r.offset]);
            }
            out.extend_from_slice(coerce_to_string(&replacement).units());
            end = r.offset + r.length;
        }
        if end < units.len() {
            out.extend_from_slice(&units[end..]);
        }
        Ok(units_value(out))
    })
}

fn replace_method(name: &'static str) -> Method {
    simple(name, 2, &[], move |rt, site, value, args| {
        let pattern = arg(&args, 0);
        let replacer = arg(&args, 1);
        let is_regexp = as_regexp(&pattern).is_some();
        let callback = is_supported_callback(&replacer);
        // A RegExp pattern is used as is; a plain one converts to its search string, then a non-callable
        // replacement.
        let values = vec![
            if is_regexp { Value::Undefined } else { pattern.clone() },
            if callback { Value::Undefined } else { replacer.clone() },
        ];
        let rt2 = rt.clone();
        with_prims(rt, site, vec![Hint::String], values, move |primitives| {
            if callback {
                let target = if is_regexp { pattern } else { arg(&primitives, 0) };
                return replace_with_callback(&rt2, site, value, name, target, replacer);
            }
            if replacer.js_typeof() == "function" {
                return NResult::err(type_error(format!(
                    "String.{name} cannot use this callable as a replacer; wrap it in an arrow function, e.g. (match) => tools.ns.tool(match)."
                )));
            }
            if let Some(rx) = as_regexp(&pattern) {
                let text = tri!(str_arg(name, &primitives, 1));
                if name == "replaceAll" {
                    tri!(replace_all_needs_global(rx));
                }
                return NResult::ok(replace_with_template(rx, &value, &text));
            }
            let search = tri!(str_arg(name, &primitives, 0));
            let template = tri!(str_arg(name, &primitives, 1));
            NResult::ok(replace_string_template(&value, &search, &template, name == "replaceAll"))
        })
    })
}

fn split_method() -> Method {
    simple("split", 2, &[], |rt, site, value, args| {
        let separator = arg(&args, 0);
        let limit_arg = arg(&args, 1);
        let is_regexp = as_regexp(&separator).is_some();
        // A RegExp separator is used as is; the limit converts before a plain separator does, as in the spec.
        let values = vec![limit_arg.clone(), if is_regexp { Value::Undefined } else { separator.clone() }];
        let rt2 = rt.clone();
        with_prims(rt, site, vec![Hint::Number, Hint::String], values, move |primitives| {
            let wrap = |parts: Vec<Value>| -> NResult {
                tri!(check_array_length(parts.len() as f64));
                NResult::ok(Value::Object(Obj::array(&rt2.builtins.array, parts)))
            };
            let pattern = arg(&primitives, 1);
            let requested = if limit_arg.is_undefined() {
                None
            } else {
                Some(tri!(num_arg("split", &[pattern.clone(), arg(&primitives, 0)], 1)))
            };
            // Native: an undefined separator returns the whole string, not a split on "undefined", unless the
            // limit truncates to zero.
            if separator.is_undefined() {
                return wrap(if requested.is_some_and(|n| to_uint32(n) == 0) { vec![] } else { vec![Value::String(value)] });
            }
            if let Some(rx) = as_regexp(&separator) {
                return wrap(symbol_split(rx, &value, requested));
            }
            let sep = tri!(str_arg("split", &[pattern], 0));
            wrap(split_string(&value, &sep, requested.map(to_uint32).unwrap_or(u32::MAX)))
        })
    })
}

/// `includes`, `startsWith`, and `endsWith` reject a RegExp before converting their search string and position.
fn searching(name: &'static str, op: fn(&[u16], &[u16], Option<f64>) -> bool) -> Method {
    simple(name, 1, &[], move |rt, site, value, args| {
        if as_regexp(&arg(&args, 0)).is_some() {
            return NResult::err(type_error(format!(
                "String.{name} cannot take a regular expression; use regex.test(string) or String.search instead."
            )));
        }
        let values: Vec<Value> = args.iter().take(2).cloned().collect();
        with_prims(rt, site, vec![Hint::String, Hint::Number], values, move |p| {
            let search = tri!(str_arg(name, &p, 0));
            let position = tri!(opt_num(name, &p, 1));
            NResult::ok(op(value.units(), search.units(), position))
        })
    })
}

fn includes(value: &[u16], search: &[u16], position: Option<f64>) -> bool {
    let start = clamp(position.unwrap_or(0.0), value.len());
    index_of(value, search, start).is_some()
}

fn starts_with(value: &[u16], search: &[u16], position: Option<f64>) -> bool {
    let start = clamp(position.unwrap_or(0.0), value.len());
    start + search.len() <= value.len() && &value[start..start + search.len()] == search
}

fn ends_with(value: &[u16], search: &[u16], position: Option<f64>) -> bool {
    let end = match position {
        None => value.len(),
        Some(p) => clamp(p, value.len()),
    };
    end >= search.len() && &value[end - search.len()..end] == search
}

fn last_index_of(value: &[u16], search: &[u16], position: Option<f64>) -> f64 {
    let pos = match position {
        Some(n) if !n.is_nan() => to_integer_or_infinity(n),
        _ => f64::INFINITY,
    };
    let len = value.len();
    if search.len() > len {
        return -1.0;
    }
    let start = (pos.max(0.0).min(len as f64) as usize).min(len - search.len());
    (0..=start).rev().find(|&i| &value[i..i + search.len()] == search).map_or(-1.0, |i| i as f64)
}

fn substr(value: &JsStr, start: f64, length: Option<f64>) -> JsStr {
    let size = value.len() as f64;
    let start = to_integer_or_infinity(start);
    let start = if start == f64::NEG_INFINITY {
        0.0
    } else if start < 0.0 {
        (size + start).max(0.0)
    } else {
        start.min(size)
    };
    let length = match length {
        None => size,
        Some(n) => to_integer_or_infinity(n),
    };
    let end = (start + length).min(size);
    if start >= end { JsStr::empty() } else { value.slice(start as usize, end as usize) }
}

// ----- String.raw / fromCharCode / fromCodePoint -----

fn from_code_point(args: &[Value]) -> JsResult<Value> {
    let mut out: Vec<u16> = Vec::with_capacity(args.len());
    for a in args {
        let n = coerce_to_number(a);
        if n.fract() != 0.0 || !n.is_finite() || !(0.0..=1_114_111.0).contains(&n) {
            return Err(range_error("Arguments contain a value that is out of range of code points"));
        }
        let cp = n as u32;
        if cp >= 0x10000 {
            let v = cp - 0x10000;
            out.push(0xD800 + (v >> 10) as u16);
            out.push(0xDC00 + (v & 0x3FF) as u16);
        } else {
            out.push(cp as u16);
        }
    }
    Ok(units_value(out))
}

fn string_raw(rt: &Rt, site: Site, args: Vec<Value>) -> NResult {
    let raw = match arg(&args, 0) {
        Value::Object(template) => tri!(get(&template, &PropKey::str("raw"))),
        _ => Value::Undefined,
    };
    let Value::Object(raw) = raw else {
        return NResult::err(type_error("String.raw expects a template object with a raw array."));
    };
    let count = tri!(get(&raw, &PropKey::str("length")).map(|l| coerce_to_integer(&l))).max(0.0);
    tri!(check_array_length(count));
    let count = count as usize;
    // Each literal is followed by its substitution, except the last literal, or when substitutions run out.
    let mut parts: Vec<Value> = Vec::new();
    for index in 0..count {
        parts.push(tri!(get(&raw, &PropKey::Str(index_key(index)))));
        if index + 1 < count && index + 1 < args.len() {
            parts.push(args[index + 1].clone());
        }
    }
    let rt = rt.clone();
    NResult::future(async move {
        let mut out: Vec<u16> = Vec::new();
        for part in parts {
            out.extend_from_slice(to_primitive_string(&rt, &part, site).await?.units());
        }
        check_length(out.len() as f64)?;
        Ok(units_value(out))
    })
}

// ----- the String global -----

pub fn string_global(rt: &Rt) -> Value {
    let b = &rt.builtins;
    let string = constructor(
        &b.function,
        &b.string,
        "String",
        1,
        NativeFn {
            call: Rc::new(|rt, site, _, args| crate::stdlib::value::coerce(rt, site, "String", args)),
            construct: None,
            callback: true,
        },
    );
    methods(
        &b.function,
        &string,
        vec![
            method("fromCharCode", 1, |_, _, _, args| {
                let units: Vec<u16> = args.iter().map(|a| to_uint32(coerce_to_number(a)) as u16).collect();
                NResult::ok(units_value(units))
            }),
            method("fromCodePoint", 1, |_, _, _, args| ready(from_code_point(&args))),
            method("raw", 1, |rt, site, _, args| string_raw(rt, site, args)),
        ],
    );

    use Hint::{Number as N, String as S};
    methods(
        &b.function,
        &b.string,
        vec![
            simple_sync("toString", 0, &[], |v, _| Ok(Value::String(v.clone()))),
            simple_sync("valueOf", 0, &[], |v, _| Ok(Value::String(v.clone()))),
            simple_sync("toLowerCase", 0, &[], |v, _| Ok(units_value(to_lower(v.units())))),
            simple_sync("toUpperCase", 0, &[], |v, _| Ok(units_value(to_upper(v.units())))),
            simple_sync("toLocaleLowerCase", 0, &[], |v, _| Ok(units_value(to_lower(v.units())))),
            simple_sync("toLocaleUpperCase", 0, &[], |v, _| Ok(units_value(to_upper(v.units())))),
            simple_sync("trim", 0, &[], |v, _| Ok(Value::String(JsStr::from_slice(trim_units(v.units()))))),
            simple_sync("trimStart", 0, &[], |v, _| Ok(Value::String(JsStr::from_slice(trim_start(v.units()))))),
            simple_sync("trimLeft", 0, &[], |v, _| Ok(Value::String(JsStr::from_slice(trim_start(v.units()))))),
            simple_sync("trimEnd", 0, &[], |v, _| Ok(Value::String(JsStr::from_slice(trim_end(v.units()))))),
            simple_sync("trimRight", 0, &[], |v, _| Ok(Value::String(JsStr::from_slice(trim_end(v.units()))))),
            // Locale/options are deliberately unsupported; comparison uses the root collation.
            simple_sync("localeCompare", 1, &[S], |v, args| {
                let that = str_arg("localeCompare", args, 0)?;
                Ok(Value::Number(locale_compare(v.units(), that.units())))
            }),
            simple_sync("normalize", 0, &[S], |v, args| {
                let form = opt_str("normalize", args, 0)?;
                let name = form.as_ref().map(|f| f.to_rust()).unwrap_or_else(|| "NFC".into());
                if !matches!(name.as_str(), "NFC" | "NFD" | "NFKC" | "NFKD")
                    || form.as_ref().is_some_and(|f| f.units().iter().any(|&u| u > 0x7F))
                {
                    let mut shown = String::new();
                    quote_units(form.as_ref().map(|f| f.units()).unwrap_or(&[]), &mut shown);
                    return Err(range_error(format!(
                        "String.normalize expects the form \"NFC\", \"NFD\", \"NFKC\", or \"NFKD\" (got {shown})."
                    )));
                }
                Ok(units_value(normalize(v.units(), &name)))
            }),
            split_method(),
            simple_sync("slice", 2, &[N, N], |v, args| {
                let len = v.len();
                let start = relative(to_integer_or_infinity(opt_num("slice", args, 0)?.unwrap_or(0.0)), len);
                let end = match opt_num("slice", args, 1)? {
                    None => len,
                    Some(n) => relative(to_integer_or_infinity(n), len),
                };
                Ok(Value::String(if start < end { v.slice(start, end) } else { JsStr::empty() }))
            }),
            searching("includes", includes),
            searching("startsWith", starts_with),
            searching("endsWith", ends_with),
            simple_sync("indexOf", 1, &[S, N], |v, args| {
                let search = str_arg("indexOf", args, 0)?;
                let start = clamp(opt_num("indexOf", args, 1)?.unwrap_or(0.0), v.len());
                Ok(Value::Number(index_of(v.units(), search.units(), start).map_or(-1.0, |i| i as f64)))
            }),
            simple_sync("lastIndexOf", 1, &[S, N], |v, args| {
                let search = str_arg("lastIndexOf", args, 0)?;
                let position = opt_num("lastIndexOf", args, 1)?;
                Ok(Value::Number(last_index_of(v.units(), search.units(), position)))
            }),
            replace_method("replace"),
            replace_method("replaceAll"),
            simple("match", 1, &[], |rt, site, value, args| {
                let rt2 = rt.clone();
                with_pattern(rt, site, &args, move |p| string_match(&rt2, &value, &p))
            }),
            simple("matchAll", 1, &[], |rt, site, value, args| {
                let rt2 = rt.clone();
                with_pattern(rt, site, &args, move |p| string_match_all(&rt2, &value, &p))
            }),
            simple("search", 1, &[], |rt, site, value, args| {
                with_pattern(rt, site, &args, move |p| {
                    let host = to_host_regex(&p, "search", "")?;
                    Ok(Value::Number(symbol_search(host.data(), &value)))
                })
            }),
            simple_sync("repeat", 1, &[N], |v, args| {
                let count = num_arg("repeat", args, 0)?;
                if !count.is_finite() || count < 0.0 {
                    return Err(range_error("String.repeat expects a finite non-negative count."));
                }
                check_length(v.len() as f64 * count)?;
                Ok(units_value(v.units().repeat(count as usize)))
            }),
            simple_sync("padStart", 1, &[N, S], |v, args| {
                let length = num_arg("padStart", args, 0)?;
                check_length(length)?;
                let filler = opt_str("padStart", args, 1)?;
                Ok(units_value(pad(v.units(), length, filler.as_ref(), true)))
            }),
            simple_sync("padEnd", 1, &[N, S], |v, args| {
                let length = num_arg("padEnd", args, 0)?;
                check_length(length)?;
                let filler = opt_str("padEnd", args, 1)?;
                Ok(units_value(pad(v.units(), length, filler.as_ref(), false)))
            }),
            simple_sync("charAt", 1, &[N], |v, args| {
                let pos = to_integer_or_infinity(opt_num("charAt", args, 0)?.unwrap_or(0.0));
                Ok(Value::String(if pos >= 0.0 && pos < v.len() as f64 {
                    v.slice(pos as usize, pos as usize + 1)
                } else {
                    JsStr::empty()
                }))
            }),
            simple_sync("at", 1, &[N], |v, args| {
                let n = to_integer_or_infinity(opt_num("at", args, 0)?.unwrap_or(0.0));
                let k = if n >= 0.0 { n } else { v.len() as f64 + n };
                Ok(if k < 0.0 || k >= v.len() as f64 {
                    Value::Undefined
                } else {
                    Value::String(v.slice(k as usize, k as usize + 1))
                })
            }),
            simple_sync("substring", 2, &[N, N], |v, args| {
                let len = v.len();
                let start = clamp(opt_num("substring", args, 0)?.unwrap_or(0.0), len);
                let end = match opt_num("substring", args, 1)? {
                    None => len,
                    Some(n) => clamp(n, len),
                };
                Ok(Value::String(v.slice(start.min(end), start.max(end))))
            }),
            simple_sync("substr", 2, &[N, N], |v, args| {
                let start = opt_num("substr", args, 0)?.unwrap_or(0.0);
                let length = opt_num("substr", args, 1)?;
                Ok(Value::String(substr(v, start, length)))
            }),
            simple_sync("isWellFormed", 0, &[], |v, _| Ok(Value::Bool(is_well_formed(v.units())))),
            simple_sync("toWellFormed", 0, &[], |v, _| Ok(units_value(to_well_formed(v.units())))),
            simple_sync("charCodeAt", 1, &[N], |v, args| {
                let pos = to_integer_or_infinity(opt_num("charCodeAt", args, 0)?.unwrap_or(0.0));
                Ok(Value::Number(if pos >= 0.0 && pos < v.len() as f64 {
                    v.units()[pos as usize] as f64
                } else {
                    f64::NAN
                }))
            }),
            simple_sync("codePointAt", 1, &[N], |v, args| {
                let pos = to_integer_or_infinity(opt_num("codePointAt", args, 0)?.unwrap_or(0.0));
                Ok(if pos >= 0.0 && pos < v.len() as f64 {
                    Value::Number(code_point_at(v.units(), pos as usize).0 as f64)
                } else {
                    Value::Undefined
                })
            }),
            simple("concat", 1, &[], |rt, site, value, args| {
                with_prims(rt, site, vec![Hint::String], args, move |parts| {
                    let mut joined: Vec<u16> = value.units().to_vec();
                    for index in 0..parts.len() {
                        joined.extend_from_slice(tri!(str_arg("concat", &parts, index)).units());
                    }
                    tri!(check_length(joined.len() as f64));
                    NResult::ok(units_value(joined))
                })
            }),
        ],
    );
    let iterate = fn_(&b.function, "[Symbol.iterator]", 0, |rt, _, this, _| {
        let value = match &this {
            Value::String(s) => s.clone(),
            Value::Undefined | Value::Null => {
                return NResult::err(type_error("String.prototype.[Symbol.iterator] called on null or undefined."));
            }
            other => coerce_to_string(other),
        };
        NResult::ok(iterator_object(rt, ListCursor::new(string_code_points(&value))))
    });
    define(&b.string, PropKey::Sym(Sym::Iterator), Value::Object(iterate), HIDDEN);
    Value::Object(string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn collation_orders_like_icu_root() {
        let mut words = vec!["résumé", "resume", "Resume", "résume", "a b", "ab", "a-b", "a_b", "a.b", "10", "9", "1"];
        words.sort_by(|a, b| locale_compare(&u(a), &u(b)).partial_cmp(&0.0).unwrap());
        assert_eq!(words, ["1", "10", "9", "a b", "a_b", "a-b", "a.b", "ab", "resume", "Resume", "résume", "résumé"]);
        let mut letters = vec!["a", "B", "b", "A", "é", "e", "f", "_", "1", " ", "Z", "z", "ä"];
        letters.sort_by(|a, b| locale_compare(&u(a), &u(b)).partial_cmp(&0.0).unwrap());
        assert_eq!(letters, [" ", "_", "1", "a", "A", "ä", "b", "B", "e", "é", "f", "z", "Z"]);
    }

    #[test]
    fn split_and_search_helpers() {
        assert_eq!(index_of(&u("abcabc"), &u("c"), 3), Some(5));
        assert_eq!(last_index_of(&u("abcabc"), &u("b"), None), 4.0);
        assert_eq!(last_index_of(&u("abcabc"), &u("b"), Some(3.0)), 1.0);
        assert_eq!(pad(&u("a"), 4.0, None, true), u("   a"));
        assert_eq!(pad(&u("a"), 6.0, Some(&JsStr::from("xy")), false), u("axyxyx"));
    }
}
