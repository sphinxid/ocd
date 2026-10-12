//! Minimal decoding combinators reproducing Effect Schema `decodeUnknown` semantics over
//! `serde_json::Value` with `{ errors: "all", onExcessProperty: "ignore" }`:
//!
//! * structs require a non-array object, excess keys are dropped, declared optional keys may be
//!   absent but never `null`;
//! * records/arrays fail as a whole when any value fails;
//! * unions try members in declaration order.
//!
//! `None` means "decode failed" (the issue details are never surfaced by the original either).

use indexmap::IndexMap;
use serde_json::{Map, Number, Value};

pub type Object = Map<String, Value>;

/// A type decodable from JSON with Effect Schema semantics.
pub trait Decode: Sized {
    fn decode(value: &Value) -> Option<Self>;
}

impl Decode for String {
    fn decode(value: &Value) -> Option<Self> {
        value.as_str().map(str::to_string)
    }
}

impl Decode for bool {
    fn decode(value: &Value) -> Option<Self> {
        value.as_bool()
    }
}

/// `Schema.Unknown` / `Schema.Any` / `Schema.Json` (every parsed JSON value is valid JSON).
impl Decode for Value {
    fn decode(value: &Value) -> Option<Self> {
        Some(value.clone())
    }
}

impl<T: Decode> Decode for Vec<T> {
    fn decode(value: &Value) -> Option<Self> {
        value.as_array()?.iter().map(T::decode).collect()
    }
}

impl<T: Decode> Decode for IndexMap<String, T> {
    fn decode(value: &Value) -> Option<Self> {
        record(value)?.iter().map(|(k, v)| Some((k.clone(), T::decode(v)?))).collect()
    }
}

/// `isRecord`: a non-null, non-array object.
pub fn record(value: &Value) -> Option<&Object> {
    value.as_object()
}

/// Optional struct key: absent → `Some(None)`; present and valid → `Some(Some(v))`; invalid → `None`.
pub fn opt<T: Decode>(o: &Object, key: &str) -> Option<Option<T>> {
    match o.get(key) {
        None => Some(None),
        Some(v) => T::decode(v).map(Some),
    }
}

/// Optional struct key decoded with a custom function.
pub fn opt_with<T>(o: &Object, key: &str, f: impl Fn(&Value) -> Option<T>) -> Option<Option<T>> {
    match o.get(key) {
        None => Some(None),
        Some(v) => f(v).map(Some),
    }
}

/// Required struct key.
pub fn req<T: Decode>(o: &Object, key: &str) -> Option<T> {
    T::decode(o.get(key)?)
}

pub fn req_with<T>(o: &Object, key: &str, f: impl Fn(&Value) -> Option<T>) -> Option<T> {
    f(o.get(key)?)
}

/// `Schema.Finite` (JSON numbers are always finite).
pub fn finite(value: &Value) -> Option<Number> {
    match value {
        Value::Number(n) => Some(js_number(n)),
        _ => None,
    }
}

/// `Schema.Int` (`Number.isInteger`).
pub fn int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return Some(i);
            }
            if n.as_u64().is_some() {
                return Some(i64::MAX);
            }
            let f = n.as_f64()?;
            if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.3e18 { Some(f as i64) } else { None }
        }
        _ => None,
    }
}

/// `PositiveInt` = integer > 0.
pub fn positive_int(value: &Value) -> Option<u64> {
    let i = int(value)?;
    (i > 0).then_some(i as u64)
}

/// `NonNegativeInt` = integer ≥ 0.
pub fn non_negative_int(value: &Value) -> Option<u64> {
    let i = int(value)?;
    (i >= 0).then_some(i as u64)
}

/// Literal string union.
pub fn literal<'a>(value: &Value, allowed: &[&'a str]) -> Option<&'a str> {
    let s = value.as_str()?;
    allowed.iter().copied().find(|a| *a == s)
}

/// Normalize a number the way JavaScript would print it: integral floats become integers
/// (`3.0` → `3`, `-0` → `0`).
pub fn js_number(n: &Number) -> Number {
    if n.is_f64() {
        let f = n.as_f64().unwrap_or(0.0);
        if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
            if f >= 0.0 {
                return Number::from(f as u64);
            }
            return Number::from(f as i64);
        }
    }
    n.clone()
}

/// Recursively apply [`js_number`] so parsed documents behave like JavaScript values.
pub fn js_numbers(value: &mut Value) {
    match value {
        Value::Number(n) => *n = js_number(n),
        Value::Array(items) => items.iter_mut().for_each(js_numbers),
        Value::Object(map) => map.values_mut().for_each(js_numbers),
        _ => {}
    }
}

/// Encode a serializable value to JSON (`Schema.encode` + `plain`).
pub fn encode<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// `isDeepStrictEqual` for JSON values (object key order is ignored, as in Node).
pub fn deep_equal(a: &Value, b: &Value) -> bool {
    a == b
}

/// Number → f64.
pub fn num_f64(n: &Number) -> f64 {
    n.as_f64().unwrap_or(0.0)
}

/// Format a finite f64 the way `String(number)` does for the common (non-exponent) range.
pub fn js_number_string(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if f.fract() == 0.0 && f.abs() < 1e21 {
        return format!("{}", f as i128);
    }
    let s = format!("{}", f);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ints() {
        assert_eq!(positive_int(&json!(5)), Some(5));
        assert_eq!(positive_int(&json!(0)), None);
        assert_eq!(positive_int(&json!(5.5)), None);
        assert_eq!(positive_int(&json!(5.0)), Some(5));
        assert_eq!(non_negative_int(&json!(0)), Some(0));
        assert_eq!(non_negative_int(&json!(-1)), None);
        assert_eq!(positive_int(&json!("5")), None);
    }

    #[test]
    fn numbers_normalize_like_js() {
        let mut v = json!({"a": 3.0, "b": [1.5, -0.0], "c": 7});
        js_numbers(&mut v);
        assert_eq!(serde_json::to_string(&v).unwrap(), r#"{"a":3,"b":[1.5,0],"c":7}"#);
    }

    #[test]
    fn opt_rejects_null() {
        let o = json!({"a": null});
        let o = o.as_object().unwrap();
        assert_eq!(opt::<String>(o, "a"), None);
        assert_eq!(opt::<String>(o, "b"), Some(None));
    }
}
