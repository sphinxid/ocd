//! Small emulations of JavaScript string/number semantics that leak into
//! model-visible output (whitespace trimming, UTF-16 lengths, number printing,
//! `localeCompare`).

use std::cmp::Ordering;

/// `String.prototype.trim` whitespace set (WhiteSpace + LineTerminator).
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

pub fn trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

pub fn trim_end(s: &str) -> &str {
    s.trim_end_matches(is_js_whitespace)
}

pub fn trim_start(s: &str) -> &str {
    s.trim_start_matches(is_js_whitespace)
}

/// `/\s/` test for a single char (same set as trim).
pub fn is_space(c: char) -> bool {
    is_js_whitespace(c)
}

/// JS `string.length` (UTF-16 code units).
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Byte index of the prefix holding at most `units` UTF-16 code units
/// (a split surrogate pair is excluded).
pub fn utf16_prefix_end(s: &str, units: usize) -> usize {
    let mut count = 0;
    for (index, c) in s.char_indices() {
        let next = count + c.len_utf16();
        if next > units {
            return index;
        }
        count = next;
    }
    s.len()
}

/// JS `string.slice(0, units)`.
pub fn utf16_prefix(s: &str, units: usize) -> &str {
    &s[..utf16_prefix_end(s, units)]
}

/// JS `Number.prototype.toString()` for the values that appear in messages.
pub fn number(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if value == value.trunc() && value.abs() < 1e21 {
        return format!("{}", value as i128);
    }
    let text = format!("{value}");
    if text.contains('e') {
        // Rust prints `1e21`, JS prints `1e+21`.
        if let Some((m, e)) = text.split_once('e') {
            if !e.starts_with('-') {
                return format!("{m}e+{e}");
            }
        }
    }
    text
}

/// JSON-ish number formatting for a serde_json Number.
pub fn json_number(value: &serde_json::Number) -> String {
    if let Some(i) = value.as_i64() {
        return i.to_string();
    }
    if let Some(u) = value.as_u64() {
        return u.to_string();
    }
    number(value.as_f64().unwrap_or(f64::NAN))
}

/// ICU root-collation `localeCompare` approximation (see [`oc_schema::collate`]).
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    oc_schema::collate::locale_compare(a, b)
}

/// `JSON.stringify(value, null, 2)`.
pub fn stringify_pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// `JSON.stringify(value)` for a string (quotes and escapes).
pub fn quote(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_like_js() {
        assert_eq!(trim("\u{FEFF} a \u{00A0}"), "a");
        assert_eq!(trim("\u{0085}a"), "\u{0085}a");
    }

    #[test]
    fn utf16() {
        assert_eq!(utf16_len("a😀"), 3);
        assert_eq!(utf16_prefix("a😀b", 2), "a");
        assert_eq!(utf16_prefix("a😀b", 3), "a😀");
    }

    #[test]
    fn numbers() {
        assert_eq!(number(0.0), "0");
        assert_eq!(number(1.5), "1.5");
        assert_eq!(number(120.0), "120");
        assert_eq!(number(-3.0), "-3");
    }

    #[test]
    fn collation() {
        let mut items = vec!["b", "A", "a", "_x", "1", "B", "é", "e"];
        items.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(items, vec!["_x", "1", "a", "A", "b", "B", "e", "é"]);
    }
}
