//! Small emulations of JavaScript string/number semantics used by the port.

/// JS `WhiteSpace` + `LineTerminator` (what `String.prototype.trim` and the
/// regex `\s` class match).
pub(super) fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{b}'
            | '\u{c}'
            | '\r'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// `String.prototype.trim`.
pub(super) fn trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

/// `String.prototype.toLowerCase` (ASCII fast path).
pub(super) fn to_lower_case(value: &str) -> String {
    if value.is_ascii() { value.to_ascii_lowercase() } else { value.to_lowercase() }
}

/// `Number.parseInt(value)` with no radix. Returns `NaN` when no digits parse.
pub(super) fn parse_int(value: &str) -> f64 {
    let trimmed = value.trim_start_matches(is_js_whitespace);
    let (negative, rest) = if let Some(rest) = trimmed.strip_prefix('-') {
        (true, rest)
    } else if let Some(rest) = trimmed.strip_prefix('+') {
        (false, rest)
    } else {
        (false, trimmed)
    };
    let result = if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        let digits: String = hex.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        if digits.is_empty() {
            return f64::NAN;
        }
        digits.chars().fold(0f64, |acc, c| acc * 16.0 + c.to_digit(16).unwrap_or(0) as f64)
    } else {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return f64::NAN;
        }
        digits.parse::<f64>().unwrap_or(f64::NAN)
    };
    if negative { -result } else { result }
}

/// `String(number)` for the integral values produced by `parseInt` and `++`.
pub(super) fn number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }
    if value.abs() >= 1e21 {
        let formatted = format!("{value:e}");
        return match formatted.split_once('e') {
            Some((mantissa, exponent)) if !exponent.starts_with('-') => format!("{mantissa}e+{exponent}"),
            _ => formatted,
        };
    }
    format!("{value}")
}

/// UTF-8 byte length of a string (`TextEncoder().encode(value).byteLength`).
pub(super) fn byte_len(value: &str) -> usize {
    value.len()
}

/// Longest prefix (by code point) whose UTF-8 encoding fits in `bytes`.
pub(super) fn slice_bytes(value: &str, bytes: i64) -> &str {
    if (value.len() as i64) <= bytes {
        return value;
    }
    if bytes <= 0 {
        return "";
    }
    let mut end = bytes as usize;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}
