//! `JSON.stringify` for JSON values, with ECMAScript number formatting (`Number.prototype.toString`).
//! Everything the Claude Agent provider writes to `claude` and every fingerprint it hashes goes
//! through it, so floats render like upstream's Bun runtime (`2.0` → `2`, `1e21` → `1e+21`).

use serde_json::Value;

/// ECMAScript `Number.prototype.toString` of a finite `f64`; non-finite values become `null`.
pub fn number(x: f64) -> String {
    if !x.is_finite() {
        return "null".into();
    }
    if x == 0.0 {
        return "0".into();
    }
    let mut buf = ryu::Buffer::new();
    let shortest = buf.format_finite(x.abs());
    let (mantissa, exp) = match shortest.find('e') {
        Some(i) => (&shortest[..i], shortest[i + 1..].parse::<i32>().unwrap_or(0)),
        None => (shortest, 0),
    };
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let frac_part = if frac_part == "0" { "" } else { frac_part };
    let mut digits: String = format!("{int_part}{frac_part}");
    let mut n = int_part.len() as i32 + exp;
    let lead = digits.len() - digits.trim_start_matches('0').len();
    digits.drain(..lead);
    n -= lead as i32;
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    let k = digits.len() as i32;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let sign = if e < 0 { '-' } else { '+' };
        if k == 1 { format!("{digits}e{sign}{}", e.abs()) } else { format!("{}.{}e{sign}{}", &digits[..1], &digits[1..], e.abs()) }
    };
    if x < 0.0 { format!("-{body}") } else { body }
}

fn number_value(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    number(n.as_f64().unwrap_or(0.0))
}

/// `JSON.stringify(string)`.
pub fn string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn write(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number_value(n)),
        Value::String(s) => out.push_str(&string(s)),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&string(k));
                out.push(':');
                write(v, out);
            }
            out.push('}');
        }
    }
}

/// `JSON.stringify(value)` (keys in insertion order).
pub fn stringify(value: &Value) -> String {
    let mut out = String::new();
    write(value, &mut out);
    out
}

/// The `canonical` encoding of `sync/fingerprint.ts`: object keys sorted (UTF-16 order, like
/// `Array.prototype.sort`), everything else as `JSON.stringify`.
pub fn canonical(value: &Value) -> String {
    match value {
        Value::Array(items) => format!("[{}]", items.iter().map(canonical).collect::<Vec<_>>().join(",")),
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            let fields: Vec<String> = keys.into_iter().map(|k| format!("{}:{}", string(k), canonical(&map[k]))).collect();
            format!("{{{}}}", fields.join(","))
        }
        other => stringify(other),
    }
}

/// UTF-16 length of a string (`String.prototype.length`).
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `s.slice(0, end)` in UTF-16 code units. A cut inside a surrogate pair drops the half pair.
pub fn utf16_prefix(s: &str, end: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    for c in s.chars() {
        let width = c.len_utf16();
        if count + width > end {
            break;
        }
        out.push(c);
        count += width;
    }
    out
}

/// `s.slice(start)` in UTF-16 code units.
pub fn utf16_suffix(s: &str, start: usize) -> String {
    let mut count = 0;
    for (index, c) in s.char_indices() {
        if count >= start {
            return s[index..].to_string();
        }
        count += c.len_utf16();
    }
    String::new()
}

/// `s.indexOf(needle)` in UTF-16 code units.
pub fn utf16_index_of(s: &str, needle: &str) -> Option<usize> {
    s.find(needle).map(|byte| utf16_len(&s[..byte]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_like_js() {
        assert_eq!(number(2.0), "2");
        assert_eq!(number(0.5), "0.5");
        assert_eq!(number(1e21), "1e+21");
        assert_eq!(number(1.5e-7), "1.5e-7");
        assert_eq!(number(-0.0), "0");
        assert_eq!(stringify(&json!({"a": 1.0, "b": [2.5, null, "x\n"]})), r#"{"a":1,"b":[2.5,null,"x\n"]}"#);
    }

    #[test]
    fn canonical_sorts_keys() {
        assert_eq!(canonical(&json!({"b": 1, "a": {"d": 2, "c": [3]}})), r#"{"a":{"c":[3],"d":2},"b":1}"#);
    }

    #[test]
    fn utf16_slices() {
        assert_eq!(utf16_len("a😀"), 3);
        assert_eq!(utf16_prefix("a😀b", 2), "a");
        assert_eq!(utf16_prefix("a😀b", 3), "a😀");
        assert_eq!(utf16_suffix("a😀b", 3), "b");
        assert_eq!(utf16_index_of("a😀bc", "c"), Some(4));
    }
}
