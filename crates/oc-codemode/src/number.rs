//! ECMAScript number semantics: Number::toString, StringToNumber, parseInt/parseFloat, toFixed/toExponential/
//! toPrecision with exact decimal rounding, radix conversion, and the int32 conversions.

use num_bigint::BigUint;
use num_traits::{One, ToPrimitive, Zero};

/// Number::toString(x) (radix 10), exactly as ECMAScript specifies (shortest round-trip digits).
pub fn to_string(n: f64) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n == 0.0 {
        return "0".into();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let mut buffer = ryu_js::Buffer::new();
    buffer.format(n).to_string()
}

/// ECMAScript WhiteSpace and LineTerminator code units (what `trim` and StringToNumber strip).
pub fn is_js_whitespace(c: u16) -> bool {
    matches!(
        c,
        0x09 | 0x0A | 0x0B | 0x0C | 0x0D | 0x20 | 0xA0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF
    )
}

pub fn trim_units(units: &[u16]) -> &[u16] {
    let start = units.iter().position(|&c| !is_js_whitespace(c)).unwrap_or(units.len());
    let end = units.iter().rposition(|&c| !is_js_whitespace(c)).map(|i| i + 1).unwrap_or(start);
    &units[start..end.max(start)]
}

fn ascii(units: &[u16]) -> Option<String> {
    if units.iter().all(|&c| c < 128) { Some(units.iter().map(|&c| c as u8 as char).collect()) } else { None }
}

/// StringToNumber.
pub fn string_to_number(units: &[u16]) -> f64 {
    let trimmed = trim_units(units);
    if trimmed.is_empty() {
        return 0.0;
    }
    let Some(s) = ascii(trimmed) else { return f64::NAN };
    let bytes = s.as_bytes();
    if bytes.len() > 2 && bytes[0] == b'0' {
        let radix = match bytes[1] {
            b'x' | b'X' => 16,
            b'o' | b'O' => 8,
            b'b' | b'B' => 2,
            _ => 0,
        };
        if radix != 0 {
            return parse_radix_digits(&s[2..], radix).unwrap_or(f64::NAN);
        }
    }
    match s.as_str() {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    if !is_str_decimal_literal(&s) {
        return f64::NAN;
    }
    s.parse::<f64>().unwrap_or(f64::NAN)
}

fn parse_radix_digits(s: &str, radix: u32) -> Option<f64> {
    if s.is_empty() {
        return None;
    }
    let mut value = 0f64;
    let mut big: Option<BigUint> = None;
    for c in s.chars() {
        let d = c.to_digit(radix)?;
        match big.as_mut() {
            Some(b) => {
                *b = &*b * radix + d;
            }
            None => {
                value = value * radix as f64 + d as f64;
                if value >= 9007199254740992.0 {
                    big = Some(BigUint::zero());
                }
            }
        }
    }
    if big.is_some() {
        let mut b = BigUint::zero();
        for c in s.chars() {
            b = b * radix + c.to_digit(radix)?;
        }
        return Some(biguint_to_f64(&b));
    }
    Some(value)
}

/// Correctly rounded conversion of a big integer to f64.
pub fn biguint_to_f64(b: &BigUint) -> f64 {
    let text = b.to_str_radix(10);
    text.parse::<f64>().unwrap_or(f64::INFINITY)
}

/// StrUnsignedDecimalLiteral with optional sign: digits, optional fraction, optional exponent.
fn is_str_decimal_literal(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - start;
    let mut frac_digits = 0;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let fs = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - fs;
    }
    if int_digits == 0 && frac_digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let es = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == es {
            return false;
        }
    }
    i == b.len()
}

/// The longest prefix of `s` that is a StrDecimalLiteral (for parseFloat); returns its byte length.
fn decimal_prefix(s: &[u8]) -> usize {
    let mut i = 0;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        i += 1;
    }
    if s[i..].starts_with(b"Infinity") {
        return i + 8;
    }
    let start = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    let mut digits = i - start;
    if i < s.len() && s[i] == b'.' {
        let dot = i;
        i += 1;
        let fs = i;
        while i < s.len() && s[i].is_ascii_digit() {
            i += 1;
        }
        digits += i - fs;
        if digits == 0 {
            return 0;
        }
        if i - fs == 0 && dot == start {
            return 0;
        }
    }
    if digits == 0 {
        return 0;
    }
    if i < s.len() && (s[i] == b'e' || s[i] == b'E') {
        let mut j = i + 1;
        if j < s.len() && (s[j] == b'+' || s[j] == b'-') {
            j += 1;
        }
        let es = j;
        while j < s.len() && s[j].is_ascii_digit() {
            j += 1;
        }
        if j > es {
            i = j;
        }
    }
    i
}

/// Global `parseFloat` on an already-converted string.
pub fn parse_float(units: &[u16]) -> f64 {
    let trimmed = {
        let start = units.iter().position(|&c| !is_js_whitespace(c)).unwrap_or(units.len());
        &units[start..]
    };
    let prefix: Vec<u8> = trimmed.iter().take_while(|&&c| c < 128).map(|&c| c as u8).collect();
    let len = decimal_prefix(&prefix);
    if len == 0 {
        return f64::NAN;
    }
    let text = std::str::from_utf8(&prefix[..len]).unwrap();
    match text {
        "Infinity" | "+Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        _ => text.parse::<f64>().unwrap_or(f64::NAN),
    }
}

/// Global `parseInt(string, radix)` with `radix` already converted to a number.
pub fn parse_int(units: &[u16], radix: f64) -> f64 {
    let start = units.iter().position(|&c| !is_js_whitespace(c)).unwrap_or(units.len());
    let mut s = &units[start..];
    let mut sign = 1.0;
    if let Some(&c) = s.first()
        && (c == b'-' as u16 || c == b'+' as u16)
    {
        if c == b'-' as u16 {
            sign = -1.0;
        }
        s = &s[1..];
    }
    let mut r = to_int32(radix);
    let mut strip_prefix = true;
    if r != 0 {
        if !(2..=36).contains(&r) {
            return f64::NAN;
        }
        if r != 16 {
            strip_prefix = false;
        }
    } else {
        r = 10;
    }
    if strip_prefix && s.len() >= 2 && s[0] == b'0' as u16 && (s[1] == b'x' as u16 || s[1] == b'X' as u16) {
        s = &s[2..];
        r = 16;
    }
    let digits: Vec<u32> = s
        .iter()
        .map_while(|&c| char::from_u32(c as u32).and_then(|ch| ch.to_digit(r as u32)))
        .collect();
    if digits.is_empty() {
        return f64::NAN;
    }
    let value = if r == 10 {
        let text: String = digits.iter().map(|d| char::from_digit(*d, 10).unwrap()).collect();
        text.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        let mut v = 0f64;
        let mut exact = true;
        for &d in &digits {
            v = v * r as f64 + d as f64;
            if v >= 9007199254740992.0 {
                exact = false;
            }
        }
        if exact {
            v
        } else {
            let mut b = BigUint::zero();
            for &d in &digits {
                b = b * (r as u32) + d;
            }
            biguint_to_f64(&b)
        }
    };
    sign * value
}

/// ToInt32.
pub fn to_int32(n: f64) -> i32 {
    to_uint32(n) as i32
}

/// ToUint32.
pub fn to_uint32(n: f64) -> u32 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    let t = n.trunc();
    let m = t.rem_euclid(4294967296.0);
    m as u32
}

/// ToIntegerOrInfinity on a number.
pub fn to_integer_or_infinity(n: f64) -> f64 {
    if n.is_nan() {
        0.0
    } else if n.is_infinite() {
        n
    } else {
        let t = n.trunc();
        if t == 0.0 { 0.0 } else { t }
    }
}

/// Decomposes a finite positive double into `mantissa * 2^exponent`.
fn decompose(x: f64) -> (u64, i32) {
    let bits = x.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    if exp == 0 { (frac, -1074) } else { (frac | (1u64 << 52), exp - 1075) }
}

/// round-half-up(x * 10^k) for finite x >= 0 as a big integer (k may be negative).
fn scaled_round(x: f64, k: i32) -> BigUint {
    let (m, e) = decompose(x);
    let mut num = BigUint::from(m);
    let mut den = BigUint::one();
    if e >= 0 {
        num <<= e as usize;
    } else {
        den <<= (-e) as usize;
    }
    if k >= 0 {
        num *= BigUint::from(10u32).pow(k as u32);
    } else {
        den *= BigUint::from(10u32).pow((-k) as u32);
    }
    // floor((2*num + den) / (2*den))
    let two = BigUint::from(2u32);
    (&num * &two + &den) / (&den * &two)
}

/// Number.prototype.toFixed with digits already validated (0..=100).
pub fn to_fixed(x: f64, digits: usize) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.abs() >= 1e21 || x.is_infinite() {
        return to_string(x);
    }
    let negative = x < 0.0;
    let x = x.abs();
    let n = scaled_round(x, digits as i32);
    let mut m = if n.is_zero() { "0".to_string() } else { n.to_str_radix(10) };
    if digits != 0 {
        if m.len() <= digits {
            m = format!("{}{}", "0".repeat(digits + 1 - m.len()), m);
        }
        let split = m.len() - digits;
        m = format!("{}.{}", &m[..split], &m[split..]);
    }
    if negative { format!("-{m}") } else { m }
}

/// Exact decimal digits of x (finite, > 0) rounded half-up to `precision` significant digits; returns
/// (digits, exponent e) with value ≈ digits * 10^(e - precision + 1).
fn significant(x: f64, precision: usize) -> (String, i32) {
    // Estimate e = floor(log10(x)).
    let mut e = x.log10().floor() as i32;
    loop {
        let n = scaled_round(x, precision as i32 - 1 - e);
        let s = n.to_str_radix(10);
        if s.len() > precision {
            e += 1;
            continue;
        }
        if s.len() < precision {
            e -= 1;
            continue;
        }
        return (s, e);
    }
}

/// Number.prototype.toExponential; `digits` None means "as many as necessary".
pub fn to_exponential(x: f64, digits: Option<usize>) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return to_string(x);
    }
    let negative = x < 0.0;
    let x = x.abs();
    let (m, e) = if x == 0.0 {
        let f = digits.unwrap_or(0);
        ("0".repeat(f + 1), 0)
    } else {
        match digits {
            Some(f) => significant(x, f + 1),
            None => shortest_digits(x),
        }
    };
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&m[..1]);
    if m.len() > 1 {
        out.push('.');
        out.push_str(&m[1..]);
    }
    out.push('e');
    out.push(if e >= 0 { '+' } else { '-' });
    out.push_str(&e.abs().to_string());
    out
}

/// Shortest round-trip digits and exponent of a finite positive double, with ECMAScript's tie-breaking (the digits
/// Number::toString prints; Rust's `{:e}` breaks ties differently).
fn shortest_digits(x: f64) -> (String, i32) {
    let mut buffer = ryu_js::Buffer::new();
    let text = buffer.format(x);
    let (mantissa, exp) = match text.split_once('e') {
        Some((m, e)) => (m, e.parse::<i32>().unwrap()),
        None => (text, 0),
    };
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let all = format!("{int_part}{frac_part}");
    let significant = all.trim_start_matches('0');
    let leading = (all.len() - significant.len()) as i32;
    let digits = significant.trim_end_matches('0');
    if digits.is_empty() {
        return ("0".into(), 0);
    }
    (digits.to_string(), int_part.len() as i32 - 1 - leading + exp)
}

/// Number.prototype.toPrecision with precision validated (1..=100).
pub fn to_precision(x: f64, precision: usize) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return to_string(x);
    }
    let negative = x < 0.0;
    let x = x.abs();
    let (m, e) = if x == 0.0 { ("0".repeat(precision), 0) } else { significant(x, precision) };
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if x != 0.0 && (e < -6 || e >= precision as i32) {
        out.push_str(&m[..1]);
        if precision > 1 {
            out.push('.');
            out.push_str(&m[1..]);
        }
        out.push('e');
        out.push(if e >= 0 { '+' } else { '-' });
        out.push_str(&e.abs().to_string());
        return out;
    }
    if e == precision as i32 - 1 {
        out.push_str(&m);
        return out;
    }
    if e >= 0 {
        let split = (e + 1) as usize;
        out.push_str(&m[..split]);
        out.push('.');
        out.push_str(&m[split..]);
    } else {
        out.push_str("0.");
        out.push_str(&"0".repeat((-(e + 1)) as usize));
        out.push_str(&m);
    }
    out
}

/// Number.prototype.toString(radix) for radix != 10, as JavaScriptCore prints it: the integer part exactly, then
/// fraction digits generated with exact arithmetic until the remainder is within half an ulp of the neighbouring
/// doubles, rounding the last digit to nearest (ties to the even number; in odd radixes parity is the digit sum).
pub fn to_radix_string(value: f64, radix: u32) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if value == 0.0 {
        return "0".into();
    }
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    // Every finite double is an integer multiple of 2^-1074; scale by 2^K so fractions and half-ulps are integers.
    const K: usize = 1076;
    let exact = |x: f64| -> BigUint {
        let (m, e) = decompose(x);
        BigUint::from(m) << ((e + K as i32) as usize)
    };
    let negative = value < 0.0;
    let value = value.abs();
    let integer = value.floor();
    let mut int_big = if integer == 0.0 { BigUint::zero() } else { exact(integer) >> K };
    let one = BigUint::one() << K;
    let half = BigUint::one() << (K - 1);
    let mask = &one - 1u32;
    let mut fraction = exact(value) - exact(integer);
    let mut half_next = (exact(next_up(value)) - exact(value)) >> 1usize;
    let mut half_last = (exact(value) - exact(f64::from_bits(value.to_bits() - 1))) >> 1usize;
    let mut digits: Vec<u8> = Vec::new();
    if !fraction.is_zero() {
        // In an odd radix a number's parity is the parity of its digit sum.
        let mut odd = false;
        if radix & 1 == 1 {
            for c in int_big.to_str_radix(radix).bytes() {
                odd ^= ((c as char).to_digit(36).unwrap() & 1) == 1;
            }
        }
        loop {
            fraction *= radix;
            half_next *= radix;
            half_last *= radix;
            let digit = (&fraction >> K).to_u32().unwrap_or(0);
            fraction &= &mask;
            digits.push(digit as u8);
            let is_odd = if radix & 1 == 1 {
                odd ^= digit & 1 == 1;
                odd
            } else {
                digit & 1 == 1
            };
            if (fraction > half || (fraction == half && is_odd)) && &fraction + &half_next > one {
                // Round up, propagating the carry into the integer part.
                loop {
                    match digits.pop() {
                        None => {
                            int_big += 1u32;
                            break;
                        }
                        Some(d) if (d as u32) + 1 < radix => {
                            digits.push(d + 1);
                            break;
                        }
                        Some(_) => {}
                    }
                }
                break;
            }
            if fraction < half_last {
                break;
            }
        }
    }
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&int_big.to_str_radix(radix));
    if !digits.is_empty() {
        out.push('.');
        out.extend(digits.iter().map(|&d| CHARS[d as usize] as char));
    }
    out
}

fn next_up(x: f64) -> f64 {
    if x.is_nan() || x == f64::INFINITY {
        return x;
    }
    if x == 0.0 {
        return f64::from_bits(1);
    }
    let bits = x.to_bits();
    if x > 0.0 { f64::from_bits(bits + 1) } else { f64::from_bits(bits - 1) }
}

/// en-US `toLocaleString` for numbers: grouping with commas, at most 3 fraction digits (ICU's default half-expand
/// rounding applied to the shortest decimal digits).
pub fn to_locale_string(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "∞".into() } else { "-∞".into() };
    }
    let negative = x < 0.0 || (x == 0.0 && x.is_sign_negative());
    let fixed = round_fraction_half_expand(x.abs(), 3);
    let (int_part, frac_part) = match fixed.split_once('.') {
        Some((i, f)) => (i.to_string(), f.trim_end_matches('0').to_string()),
        None => (fixed.clone(), String::new()),
    };
    let mut grouped = String::new();
    for (i, c) in int_part.chars().enumerate() {
        if i > 0 && (int_part.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let mut out = String::new();
    if negative && (int_part != "0" || !frac_part.is_empty()) {
        out.push('-');
    } else if negative {
        out.push('-');
    }
    out.push_str(&grouped);
    if !frac_part.is_empty() {
        out.push('.');
        out.push_str(&frac_part);
    }
    out
}

/// Rounds the shortest decimal representation of `x` to `digits` fraction digits, ties away from zero.
fn round_fraction_half_expand(x: f64, digits: usize) -> String {
    let (m, e) = if x == 0.0 { ("0".to_string(), 0) } else { shortest_digits(x) };
    // value = 0.m * 10^(e+1)
    let point = e + 1; // digits before the decimal point
    let mut all: Vec<u8> = m.bytes().map(|b| b - b'0').collect();
    let mut int_len = point;
    if int_len <= 0 {
        let pad = (-int_len) as usize;
        let mut v = vec![0u8; pad];
        v.extend(all);
        all = v;
        int_len = 0;
    }
    let int_len = int_len as usize;
    while all.len() < int_len + digits {
        all.push(0);
    }
    let keep = int_len + digits;
    let rest = &all[keep..];
    let mut kept: Vec<u8> = all[..keep].to_vec();
    let round_up = rest.first().is_some_and(|&d| d >= 5);
    let mut int_len = int_len;
    if round_up {
        let mut i = kept.len();
        loop {
            if i == 0 {
                kept.insert(0, 1);
                int_len += 1;
                break;
            }
            i -= 1;
            if kept[i] == 9 {
                kept[i] = 0;
            } else {
                kept[i] += 1;
                break;
            }
        }
    }
    let int_digits: String = if int_len == 0 {
        "0".into()
    } else {
        let s: String = kept[..int_len].iter().map(|d| (b'0' + d) as char).collect();
        let t = s.trim_start_matches('0');
        if t.is_empty() { "0".into() } else { t.to_string() }
    };
    let frac: String = kept[int_len..].iter().map(|d| (b'0' + d) as char).collect();
    if frac.is_empty() { int_digits } else { format!("{int_digits}.{frac}") }
}

pub fn big_to_f64(b: &BigUint) -> f64 {
    b.to_f64().unwrap_or(f64::INFINITY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_to_string() {
        assert_eq!(to_string(1.0), "1");
        assert_eq!(to_string(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(to_string(1e21), "1e+21");
        assert_eq!(to_string(1e-7), "1e-7");
        assert_eq!(to_string(123456789012345680000.0), "123456789012345680000");
        assert_eq!(to_string(-0.0), "0");
        assert_eq!(to_string(f64::NAN), "NaN");
    }

    #[test]
    fn fixed_and_precision() {
        assert_eq!(to_fixed(1.005, 2), "1.00");
        assert_eq!(to_fixed(0.5, 0), "1");
        assert_eq!(to_fixed(2.5, 0), "3");
        assert_eq!(to_fixed(1.45, 1), "1.4");
        assert_eq!(to_fixed(-1.5, 0), "-2");
        assert_eq!(to_fixed(123.456, 2), "123.46");
        assert_eq!(to_fixed(0.000001, 2), "0.00");
        assert_eq!(to_fixed(1e21, 2), "1e+21");
        assert_eq!(to_precision(123.456, 4), "123.5");
        assert_eq!(to_precision(0.000123, 2), "0.00012");
        assert_eq!(to_precision(123456.0, 2), "1.2e+5");
        assert_eq!(to_precision(1e-7, 1), "1e-7");
        assert_eq!(to_exponential(123456.0, Some(2)), "1.23e+5");
        assert_eq!(to_exponential(123456.0, None), "1.23456e+5");
        assert_eq!(to_exponential(0.0, Some(2)), "0.00e+0");
    }

    #[test]
    fn parsing() {
        let u = |s: &str| s.encode_utf16().collect::<Vec<_>>();
        assert_eq!(string_to_number(&u("  12  ")), 12.0);
        assert_eq!(string_to_number(&u("0x1F")), 31.0);
        assert!(string_to_number(&u("1_000")).is_nan());
        assert_eq!(string_to_number(&u("")), 0.0);
        assert_eq!(string_to_number(&u(".5")), 0.5);
        assert!(string_to_number(&u(".")).is_nan());
        assert_eq!(parse_int(&u("  0x1f"), 0.0), 31.0);
        assert_eq!(parse_int(&u("12px"), 0.0), 12.0);
        assert_eq!(parse_int(&u("11"), 2.0), 3.0);
        assert!(parse_int(&u("z"), 10.0).is_nan());
        assert_eq!(parse_float(&u("3.14abc")), 3.14);
        assert_eq!(parse_float(&u("-.5e1x")), -5.0);
        assert_eq!(parse_float(&u("Infinityx")), f64::INFINITY);
        assert!(parse_float(&u("e5")).is_nan());
    }

    #[test]
    fn radix() {
        assert_eq!(to_radix_string(255.0, 16), "ff");
        assert_eq!(to_radix_string(-255.0, 2), "-11111111");
        assert_eq!(to_radix_string(0.5, 2), "0.1");
        assert_eq!(to_radix_string(3.75, 2), "11.11");
    }

    #[test]
    fn locale() {
        assert_eq!(to_locale_string(1234.5), "1,234.5");
        assert_eq!(to_locale_string(1234567.891), "1,234,567.891");
        assert_eq!(to_locale_string(0.12345), "0.123");
        assert_eq!(to_locale_string(-1000.0), "-1,000");
    }
}
