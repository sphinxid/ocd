//! Effect `Schema.decodeUnknown` over the browser schemas' JSON Schema export: validates a value and
//! returns it projected onto the schema the way Effect builds a decoded struct (declared properties in
//! declaration order, undeclared ones dropped, a union decoded by its first matching member).
//!
//! Only the keywords the exported browser schemas use are interpreted (`type`, `properties`, `required`,
//! `items`, `anyOf`, `enum`, `pattern`, `minLength`/`maxLength` in UTF-16 units, `minItems`/`maxItems`,
//! `minimum`/`maximum`); an empty schema is `Schema.Json` and keeps the value as it is.

use serde_json::{Map, Value};

/// `Schema.decodeUnknown(schema)(value)`: the decoded value, or `None` when it does not match.
pub fn decode(schema: &Value, value: &Value) -> Option<Value> {
    let map = schema.as_object()?;
    if let Some(Value::Array(members)) = map.get("anyOf") {
        return members.iter().find_map(|member| decode(member, value));
    }
    if let Some(Value::Array(values)) = map.get("enum") {
        return values.iter().any(|v| same(v, value)).then(|| value.clone());
    }
    let Some(kind) = map.get("type").and_then(Value::as_str) else {
        return Some(value.clone());
    };
    match kind {
        "object" => {
            let object = value.as_object()?;
            let required: Vec<&str> =
                map.get("required").and_then(Value::as_array).map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            let mut out = Map::new();
            if let Some(properties) = map.get("properties").and_then(Value::as_object) {
                for (key, property) in properties {
                    match object.get(key) {
                        Some(item) => {
                            out.insert(key.clone(), decode(property, item)?);
                        }
                        None if required.contains(&key.as_str()) => return None,
                        None => {}
                    }
                }
            }
            Some(Value::Object(out))
        }
        "array" => {
            let items = value.as_array()?;
            let count = items.len() as u64;
            if map.get("minItems").and_then(Value::as_u64).is_some_and(|min| count < min)
                || map.get("maxItems").and_then(Value::as_u64).is_some_and(|max| count > max)
            {
                return None;
            }
            let out = match map.get("items") {
                Some(item) => items.iter().map(|x| decode(item, x)).collect::<Option<Vec<_>>>()?,
                None => items.clone(),
            };
            Some(Value::Array(out))
        }
        "string" => {
            let text = value.as_str()?;
            let units = text.encode_utf16().count() as u64;
            if map.get("minLength").and_then(Value::as_u64).is_some_and(|min| units < min)
                || map.get("maxLength").and_then(Value::as_u64).is_some_and(|max| units > max)
            {
                return None;
            }
            if let Some(pattern) = map.get("pattern").and_then(Value::as_str)
                && !regex::Regex::new(pattern).is_ok_and(|re| re.is_match(text))
            {
                return None;
            }
            Some(value.clone())
        }
        "integer" | "number" => {
            let number = value.as_f64()?;
            if !number.is_finite() || (kind == "integer" && number.fract() != 0.0) {
                return None;
            }
            if map.get("minimum").and_then(Value::as_f64).is_some_and(|min| number < min)
                || map.get("maximum").and_then(Value::as_f64).is_some_and(|max| number > max)
            {
                return None;
            }
            Some(js_number(value))
        }
        "boolean" => value.is_boolean().then(|| value.clone()),
        "null" => value.is_null().then(|| value.clone()),
        _ => Some(value.clone()),
    }
}

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

/// A JSON number as JavaScript holds it: an integral float is an integer (`1.0` prints as `1`).
pub fn js_number(value: &Value) -> Value {
    match value.as_f64() {
        Some(n) if value.is_f64() && n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0 => {
            if n == 0.0 {
                Value::from(0)
            } else {
                Value::from(n as i64)
            }
        }
        _ => value.clone(),
    }
}

/// Every number in a JSON value as JavaScript holds it (see [`js_number`]).
pub fn js_numbers(value: Value) -> Value {
    match value {
        Value::Number(_) => js_number(&value),
        Value::Array(items) => Value::Array(items.into_iter().map(js_numbers).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, js_numbers(v))).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projects_structs_in_declaration_order_and_drops_extras() {
        let schema = json!({"type":"object","properties":{"a":{"type":"string"},"b":{"type":"integer","minimum":0},"c":{"type":"boolean"}},"required":["a","b"]});
        let out = decode(&schema, &json!({"z": 1, "b": 2.0, "a": "x"})).unwrap();
        assert_eq!(serde_json::to_string(&out).unwrap(), r#"{"a":"x","b":2}"#);
        assert!(decode(&schema, &json!({"a": "x"})).is_none());
        assert!(decode(&schema, &json!({"a": "x", "b": -1})).is_none());
        assert!(decode(&schema, &json!({"a": "x", "b": 1.5})).is_none());
        assert!(decode(&schema, &json!({"a": "x", "b": 1, "c": null})).is_none());
    }

    #[test]
    fn unions_take_the_first_matching_member() {
        let schema = json!({"anyOf":[
            {"type":"object","properties":{"state":{"type":"string","enum":["notRequested","pending","empty"]}},"required":["state"]},
            {"type":"object","properties":{"state":{"type":"string","enum":["text"]},"text":{"type":"string"},"truncated":{"type":"boolean"}},"required":["state","text","truncated"]}
        ]});
        assert_eq!(decode(&schema, &json!({"state":"empty","text":"x"})).unwrap(), json!({"state":"empty"}));
        assert_eq!(
            decode(&schema, &json!({"truncated":false,"text":"t","state":"text"})).unwrap(),
            json!({"state":"text","text":"t","truncated":false})
        );
        assert!(decode(&schema, &json!({"state":"other"})).is_none());
    }

    #[test]
    fn strings_count_utf16_units_and_match_patterns() {
        let schema = json!({"type":"string","maxLength":2,"pattern":"^@?e[1-9][0-9]*$"});
        assert!(decode(&schema, &json!("e1")).is_some());
        assert!(decode(&schema, &json!("e10")).is_none());
        assert!(decode(&schema, &json!("x1")).is_none());
        assert!(decode(&json!({"type":"string","maxLength":1}), &json!("😀")).is_none());
        assert_eq!(decode(&json!({}), &json!({"any": [1, null]})).unwrap(), json!({"any": [1, null]}));
    }
}
