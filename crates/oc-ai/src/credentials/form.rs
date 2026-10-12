//! Form field validation used by integration methods (`packages/core/src/form.ts` `validateFields` /
//! `validateAnswer`). Fields and answers are kept as raw JSON (`Form.Field[]`, `Form.Answer`) so their wire
//! shape is preserved exactly.

use regex::Regex;
use serde_json::{Map, Value};

/// `Form.Answer`: field key → string | number | boolean | string[].
pub type FormAnswer = Map<String, Value>;

fn key_of(field: &Value) -> &str {
    field.get("key").and_then(Value::as_str).unwrap_or("")
}

fn type_of(field: &Value) -> &str {
    field.get("type").and_then(Value::as_str).unwrap_or("")
}

fn is_true(field: &Value, name: &str) -> bool {
    field.get(name).and_then(Value::as_bool).unwrap_or(false)
}

fn options(field: &Value) -> Option<Vec<&str>> {
    field
        .get("options")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|o| o.get("value").and_then(Value::as_str)).collect())
}

/// Create-time validation (`validateFields`). Returns the first problem.
pub fn validate_fields(fields: &[Value]) -> Option<String> {
    if fields.is_empty() {
        return Some("Form must have at least one field".into());
    }
    let mut earlier: Vec<&Value> = Vec::new();
    let mut keys = std::collections::HashSet::new();
    for field in fields {
        let key = key_of(field);
        if !keys.insert(key.to_string()) {
            return Some(format!("Duplicate form field key: {key}"));
        }
        if type_of(field) == "external" {
            continue;
        }
        for when in field.get("when").and_then(Value::as_array).into_iter().flatten() {
            let target_key = when.get("key").and_then(Value::as_str).unwrap_or("");
            let Some(target) = earlier.iter().rev().find(|f| key_of(f) == target_key) else {
                return Some(format!(
                    "Form field condition must reference an earlier field: {key} -> {target_key}"
                ));
            };
            if let Some(invalid) = validate_when(when, target) {
                return Some(format!("{invalid}: {key} -> {target_key}"));
            }
        }
        earlier.push(field);
    }
    None
}

fn validate_when(when: &Value, target: &Value) -> Option<&'static str> {
    let value = when.get("value").unwrap_or(&Value::Null);
    match type_of(target) {
        "boolean" => (!value.is_boolean()).then_some("Form field condition value must be a boolean"),
        "number" | "integer" => (!value.is_number()).then_some("Form field condition value must be a number"),
        kind => {
            let Some(value) = value.as_str() else {
                return Some("Form field condition value must be a string");
            };
            let opts = options(target);
            let closed = if kind == "multiselect" {
                !is_true(target, "custom")
            } else {
                opts.is_some() && !is_true(target, "custom")
            };
            if closed && !opts.unwrap_or_default().contains(&value) {
                return Some("Form field condition value must be one of the field's options");
            }
            None
        }
    }
}

fn is_active(field: &Value, answer: &FormAnswer) -> bool {
    let Some(conditions) = field.get("when").and_then(Value::as_array) else { return true };
    conditions.iter().all(|when| {
        let key = when.get("key").and_then(Value::as_str).unwrap_or("");
        let expected = when.get("value").unwrap_or(&Value::Null);
        let Some(value) = answer.get(key) else { return false };
        let hit = match value {
            Value::Array(items) => items.iter().any(|item| item == expected),
            other => other == expected,
        };
        if when.get("op").and_then(Value::as_str) == Some("eq") { hit } else { !hit }
    })
}

/// Answer validation (`validateAnswer`). Returns the first problem.
pub fn validate_answer(fields: &[Value], answer: &FormAnswer) -> Option<String> {
    for key in answer.keys() {
        if !fields.iter().any(|f| key_of(f) == key) {
            return Some(format!("Unknown form field: {key}"));
        }
    }
    for field in fields {
        let key = key_of(field);
        let value = answer.get(key);
        if type_of(field) == "external" {
            if value != Some(&Value::Bool(true)) {
                return Some(format!("External form field must be acknowledged: {key}"));
            }
            continue;
        }
        let active = is_active(field, answer);
        let Some(value) = value else {
            if is_true(field, "required") && active {
                return Some(format!("Missing required form field: {key}"));
            }
            continue;
        };
        if !active {
            return Some(format!("Form field is not active: {key}"));
        }
        if let Some(invalid) = validate_field(field, value) {
            return Some(invalid);
        }
    }
    None
}

fn validate_field(field: &Value, value: &Value) -> Option<String> {
    let key = key_of(field);
    let required = is_true(field, "required");
    let num = |name: &str| field.get(name).and_then(Value::as_f64);
    match type_of(field) {
        "string" => {
            let Some(text) = value.as_str() else { return Some(format!("Expected string for form field: {key}")) };
            let length = text.encode_utf16().count() as f64;
            if required && text.is_empty() {
                return Some(format!("Missing required form field: {key}"));
            }
            if num("minLength").is_some_and(|min| length < min) {
                return Some(format!("Form field is too short: {key}"));
            }
            if num("maxLength").is_some_and(|max| length > max) {
                return Some(format!("Form field is too long: {key}"));
            }
            if let Some(pattern) = field.get("pattern").and_then(Value::as_str) {
                match Regex::new(pattern) {
                    Ok(re) if !re.is_match(text) => return Some(format!("Form field does not match pattern: {key}")),
                    Err(_) => return Some(format!("Form field has invalid pattern: {key}")),
                    _ => {}
                }
            }
            match field.get("format").and_then(Value::as_str) {
                Some("email") => {
                    let re = Regex::new(r"^[^\s@]+@[^\s@]+\.[^\s@]+$").expect("valid regex");
                    if !re.is_match(text) {
                        return Some(format!("Expected email for form field: {key}"));
                    }
                }
                Some("uri") if url::Url::parse(text).is_err() => {
                    return Some(format!("Expected URI for form field: {key}"));
                }
                Some("date") if !is_date(text) => return Some(format!("Expected date for form field: {key}")),
                Some("date-time") if !is_date_time(text) => {
                    return Some(format!("Expected date-time for form field: {key}"));
                }
                _ => {}
            }
            if let Some(opts) = options(field)
                && !is_true(field, "custom")
                && !opts.contains(&text)
            {
                return Some(format!("Invalid option for form field: {key}"));
            }
            None
        }
        kind @ ("number" | "integer") => {
            let Some(n) = value.as_f64().filter(|n| n.is_finite()) else {
                return Some(format!("Expected number for form field: {key}"));
            };
            if kind == "integer" && n.fract() != 0.0 {
                return Some(format!("Expected integer for form field: {key}"));
            }
            if num("minimum").is_some_and(|min| n < min) {
                return Some(format!("Form field is too small: {key}"));
            }
            if num("maximum").is_some_and(|max| n > max) {
                return Some(format!("Form field is too large: {key}"));
            }
            None
        }
        "boolean" => (!value.is_boolean()).then(|| format!("Expected boolean for form field: {key}")),
        "multiselect" => {
            let Some(items) = value.as_array().filter(|items| items.iter().all(Value::is_string)) else {
                return Some(format!("Expected string array for form field: {key}"));
            };
            let count = items.len() as f64;
            if required && items.is_empty() {
                return Some(format!("Missing required form field: {key}"));
            }
            if num("minItems").is_some_and(|min| count < min) {
                return Some(format!("Too few selections for form field: {key}"));
            }
            if num("maxItems").is_some_and(|max| count > max) {
                return Some(format!("Too many selections for form field: {key}"));
            }
            let opts = options(field).unwrap_or_default();
            if !is_true(field, "custom") && items.iter().any(|item| !opts.contains(&item.as_str().unwrap_or(""))) {
                return Some(format!("Invalid option for form field: {key}"));
            }
            None
        }
        _ => None,
    }
}

fn is_date(value: &str) -> bool {
    let re = Regex::new(r"^\d{4}-\d{2}-\d{2}$").expect("valid regex");
    re.is_match(value) && chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok()
}

fn is_date_time(value: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(value).is_ok()
        || chrono::DateTime::parse_from_rfc2822(value).is_ok()
        || chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f").is_ok()
        || chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M").is_ok()
        || is_date(value)
}

/// Validates a method form + answer the way `oauth.connect` / `connection.key` do.
pub fn check(fields: Option<&[Value]>, answer: &FormAnswer) -> Option<String> {
    let fields = fields?;
    validate_fields(fields).or_else(|| validate_answer(fields, answer))
}
