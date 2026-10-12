//! JSON helpers that reproduce JavaScript `JSON.stringify` / object-spread semantics on top of
//! `serde_json::Value` (with `preserve_order`, so key insertion order is the wire order).

use serde_json::{Map, Number, Value};

pub type Object = Map<String, Value>;

/// A JSON number from an `f64`, rendered like JavaScript (`1` not `1.0`).
pub fn num(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        Value::Number(Number::from(value as i64))
    } else {
        Number::from_f64(value).map(Value::Number).unwrap_or(Value::Null)
    }
}

/// Normalize a JSON value so integral floats render without a fractional part, like JS.
pub fn normalize_numbers(value: Value) -> Value {
    match value {
        Value::Number(n) => {
            if n.is_f64() {
                num(n.as_f64().unwrap_or(0.0))
            } else {
                Value::Number(n)
            }
        }
        Value::Array(items) => Value::Array(items.into_iter().map(normalize_numbers).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, normalize_numbers(v))).collect()),
        other => other,
    }
}

/// `JSON.stringify(value)`.
pub fn encode(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".into())
}

/// Plain-record check (`typeof value === "object" && value !== null && !Array.isArray(value)`).
pub fn is_record(value: &Value) -> bool {
    value.is_object()
}

/// JS truthiness of a JSON value.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// `mergeJsonRecords`: later wins, nested records merged recursively, `null`-as-undefined is NOT skipped
/// (JSON has no `undefined`); returns `None` if all inputs are `None` or the result is empty.
pub fn merge_records(items: &[Option<&Object>]) -> Option<Object> {
    let defined: Vec<&Object> = items.iter().filter_map(|item| *item).collect();
    if defined.is_empty() {
        return None;
    }
    if defined.len() == 1 {
        return if defined[0].is_empty() { Some(defined[0].clone()) } else { Some(defined[0].clone()) };
    }
    let mut result = Object::new();
    for item in defined {
        merge_into(&mut result, item);
    }
    if result.is_empty() { None } else { Some(result) }
}

fn merge_into(target: &mut Object, source: &Object) {
    for (key, value) in source {
        match (target.get_mut(key), value) {
            (Some(Value::Object(existing)), Value::Object(incoming)) => {
                let mut merged = existing.clone();
                merge_into(&mut merged, incoming);
                *existing = merged;
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Deep-merge `overlay` over `base` (objects merged recursively, other values replaced).
pub fn deep_merge(base: &Value, overlay: &Value) -> Value {
    match (base, overlay) {
        (Value::Object(a), Value::Object(b)) => {
            let mut out = a.clone();
            merge_into(&mut out, b);
            Value::Object(out)
        }
        _ => overlay.clone(),
    }
}

/// Builder for ordered JSON objects that skips `None` values (JS `undefined`).
#[derive(Default, Debug, Clone)]
pub struct ObjBuilder(pub Object);

impl ObjBuilder {
    pub fn new() -> Self {
        Self(Object::new())
    }
    pub fn set(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.0.insert(key.to_string(), value.into());
        self
    }
    pub fn opt<V: Into<Value>>(mut self, key: &str, value: Option<V>) -> Self {
        if let Some(v) = value {
            self.0.insert(key.to_string(), v.into());
        }
        self
    }
    /// Set only when `value` is not `Value::Null` (treat null as JS undefined).
    pub fn val(mut self, key: &str, value: Value) -> Self {
        if !value.is_null() {
            self.0.insert(key.to_string(), value);
        }
        self
    }
    pub fn num(self, key: &str, value: Option<f64>) -> Self {
        self.opt(key, value.map(num))
    }
    pub fn build(self) -> Value {
        Value::Object(self.0)
    }
    pub fn object(self) -> Object {
        self.0
    }
}

/// `obj!{ "a" => 1, "b" => opt_value }` builds an ordered object; values are anything `Into<Value>`.
#[macro_export]
macro_rules! obj {
    () => { $crate::json::ObjBuilder::new().build() };
    ($($key:expr => $value:expr),+ $(,)?) => {{
        let mut map = $crate::json::Object::new();
        $( map.insert(($key).to_string(), ::serde_json::Value::from($value)); )+
        ::serde_json::Value::Object(map)
    }};
}

pub fn str_of(value: &Value) -> Option<&str> {
    value.as_str()
}

pub fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_object().and_then(|o| o.get(key)).filter(|v| !v.is_null())
}

pub fn get_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    get(value, key).and_then(Value::as_str)
}

pub fn get_f64(value: &Value, key: &str) -> Option<f64> {
    get(value, key).and_then(Value::as_f64)
}

pub fn get_u64(value: &Value, key: &str) -> Option<u64> {
    get(value, key).and_then(|v| v.as_u64().or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)))
}

pub fn get_bool(value: &Value, key: &str) -> Option<bool> {
    get(value, key).and_then(Value::as_bool)
}

pub fn get_obj<'a>(value: &'a Value, key: &str) -> Option<&'a Object> {
    get(value, key).and_then(Value::as_object)
}

pub fn get_arr<'a>(value: &'a Value, key: &str) -> Option<&'a Vec<Value>> {
    get(value, key).and_then(Value::as_array)
}

/// Path lookup: `path(v, &["a","b"])`.
pub fn path<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in keys {
        current = get(current, key)?;
    }
    Some(current)
}

/// `String(value)` for a JSON value (JS semantics).
pub fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => js_number_string(n),
        Value::Array(items) => items
            .iter()
            .map(|v| if v.is_null() { String::new() } else { js_string(v) })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

pub fn js_number_string(n: &Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    if f.fract() == 0.0 && f.abs() < 1e21 { format!("{}", f as i128) } else { format!("{f}") }
}

/// Replace lone surrogate escapes is unnecessary in Rust (`String` is valid UTF-8); kept for parity.
pub fn sanitize(value: Value) -> Value {
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn num_renders_integers() {
        assert_eq!(encode(&num(1.0)), "1");
        assert_eq!(encode(&num(0.5)), "0.5");
        assert_eq!(encode(&num(0.0)), "0");
    }

    #[test]
    fn merge_records_deep() {
        let a = json!({"a": {"x": 1, "y": 2}, "b": 1}).as_object().unwrap().clone();
        let b = json!({"a": {"y": 3}, "c": 4}).as_object().unwrap().clone();
        let merged = merge_records(&[Some(&a), Some(&b)]).unwrap();
        assert_eq!(Value::Object(merged), json!({"a": {"x": 1, "y": 3}, "b": 1, "c": 4}));
    }
}
