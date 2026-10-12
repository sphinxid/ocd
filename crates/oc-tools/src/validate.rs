//! Input validation with Effect-compatible issue messages (`core/src/tool/runtime.ts`).
//!
//! Built-in tools are Effect schemas (`Flavor::Effect`); MCP/plugin tools carry raw JSON
//! Schema that Effect converts into a codec (`Flavor::Json`). Messages were captured from the
//! real runtime (`SchemaIssue.makeFormatterStandardSchemaV1`).

use serde_json::{Map, Value};

use crate::util::js;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegment {
    Key(String),
    Index(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub path: Vec<PathSegment>,
    pub message: String,
}

impl Issue {
    pub fn new(path: Vec<PathSegment>, message: impl Into<String>) -> Self {
        Self { path, message: message.into() }
    }

    /// Render the path like `formatInputIssues`.
    pub fn path_text(&self) -> String {
        let mut out = String::new();
        for segment in &self.path {
            match segment {
                PathSegment::Index(i) => out.push_str(&format!("[{i}]")),
                PathSegment::Key(k) => {
                    if out.is_empty() {
                        out.push_str(k);
                    } else {
                        out.push('.');
                        out.push_str(k);
                    }
                }
            }
        }
        if out.is_empty() { "root".into() } else { out }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// Effect `Schema` declared in code (built-in tools).
    Effect,
    /// Raw JSON Schema converted by `SchemaRepresentation.fromJsonSchemaDocument`.
    Json,
}

/// `formatInputIssues`.
pub fn format_input_issues(tool: &str, issues: &[Issue], value: &Value) -> String {
    let mut details: Vec<String> =
        issues.iter().take(5).map(|issue| format!("- {}: {}", issue.path_text(), issue.message)).collect();
    if issues.len() > 5 {
        details.push(format!(
            "- ...and {} more {}",
            issues.len() - 5,
            if issues.len() == 6 { "issue" } else { "issues" }
        ));
    }
    format!(
        "Invalid arguments for tool \"{tool}\":\n{}\n\nArguments provided:\n{}\n\nUpdate the arguments and call the tool again.",
        details.join("\n"),
        js::stringify_pretty(value)
    )
}

fn has_key(schema: &Value, key: &str) -> bool {
    match schema {
        Value::Object(map) => map.contains_key(key) || map.values().any(|v| has_key(v, key)),
        Value::Array(items) => items.iter().any(|v| has_key(v, key)),
        _ => false,
    }
}

/// Validate `value` against `schema`. For `Flavor::Json`, schemas containing `pattern`
/// fail codec construction upstream and are therefore not validated at all.
pub fn validate(schema: &Value, value: &Value, flavor: Flavor) -> Vec<Issue> {
    if flavor == Flavor::Json && has_key(schema, "pattern") {
        return Vec::new();
    }
    let mut issues = Vec::new();
    let root = schema;
    check(root, schema, value, &mut Vec::new(), flavor, &mut issues);
    issues
}

fn resolve_ref<'a>(root: &'a Value, reference: &str) -> Option<(&'a Value, String)> {
    let rest = reference.strip_prefix("#/")?;
    let mut parts = rest.split('/');
    let container = parts.next()?;
    let name = parts.next()?;
    let target = root.get(container)?.get(name)?;
    Some((target, name.to_string()))
}

fn type_names(schema: &Value) -> Vec<String> {
    match schema.get("type") {
        Some(Value::String(t)) => vec![t.clone()],
        Some(Value::Array(items)) => items.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        _ => Vec::new(),
    }
}

fn literal_text(value: &Value) -> String {
    match value {
        Value::String(s) => js::quote(s),
        Value::Number(n) => js::json_number(n),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

/// Text describing what a schema expects (`Expected <describe>`).
fn describe(root: &Value, schema: &Value) -> String {
    if let Some(Value::String(reference)) = schema.get("$ref") {
        if let Some((_, name)) = resolve_ref(root, reference) {
            return name;
        }
    }
    if let Some(Value::Array(values)) = schema.get("enum") {
        return values.iter().map(literal_text).collect::<Vec<_>>().join(" | ");
    }
    if let Some(value) = schema.get("const") {
        return literal_text(value);
    }
    if let Some(Value::Array(members)) = schema.get("anyOf").or_else(|| schema.get("oneOf")) {
        return members.iter().map(|m| describe(root, m)).collect::<Vec<_>>().join(" | ");
    }
    let names = type_names(schema);
    if !names.is_empty() {
        return names.iter().map(|n| if n == "integer" { "number".to_string() } else { n.clone() }).collect::<Vec<_>>().join(" | ");
    }
    "unknown".into()
}

fn matches_type(name: &str, value: &Value) -> bool {
    match name {
        "string" => value.is_string(),
        "number" | "integer" => value.is_number(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn is_safe_integer(value: f64) -> bool {
    value.is_finite() && value.trunc() == value && value.abs() <= 9_007_199_254_740_991.0
}

fn check(root: &Value, schema: &Value, value: &Value, path: &mut Vec<PathSegment>, flavor: Flavor, issues: &mut Vec<Issue>) {
    let Some(map) = schema.as_object() else { return };
    if let Some(Value::String(reference)) = map.get("$ref") {
        if let Some((target, _)) = resolve_ref(root, reference) {
            let before = issues.len();
            let mut nested = Vec::new();
            check(root, target, value, path, flavor, &mut nested);
            // A failing type check on a referenced schema reports the reference name.
            if nested.len() == 1 && nested[0].path == *path && nested[0].message.starts_with("Expected ") && !matches_base_type(target, value) {
                issues.push(Issue::new(path.clone(), format!("Expected {}", describe(root, schema))));
            } else {
                issues.extend(nested);
            }
            let _ = before;
            return;
        }
    }
    if let Some(Value::Array(members)) = map.get("anyOf").or_else(|| map.get("oneOf")) {
        check_union(root, schema, members, value, path, flavor, issues);
        return;
    }
    if let Some(Value::Array(values)) = map.get("enum") {
        if !values.iter().any(|v| v == value) {
            issues.push(Issue::new(path.clone(), format!("Expected {}", describe(root, schema))));
        }
        return;
    }
    if let Some(constant) = map.get("const") {
        if constant != value {
            issues.push(Issue::new(path.clone(), format!("Expected {}", describe(root, schema))));
        }
        return;
    }
    let names = type_names(schema);
    if names.len() > 1 {
        if !names.iter().any(|n| matches_type(n, value)) {
            issues.push(Issue::new(path.clone(), format!("Expected {}", describe(root, schema))));
            return;
        }
    } else if let Some(name) = names.first() {
        if !matches_type(name, value) {
            let expected = if name == "integer" { "number" } else { name.as_str() };
            issues.push(Issue::new(path.clone(), format!("Expected {expected}")));
            return;
        }
    }
    match value {
        Value::Object(object) => check_object(root, map, object, path, flavor, issues),
        Value::Array(items) => check_array(root, map, items, path, flavor, issues),
        Value::String(text) => check_string(map, text, path, flavor, issues),
        Value::Number(number) => check_number(map, names.iter().any(|n| n == "integer"), number.as_f64().unwrap_or(f64::NAN), path, flavor, issues),
        _ => {}
    }
}

fn matches_base_type(schema: &Value, value: &Value) -> bool {
    let names = type_names(schema);
    names.is_empty() || names.iter().any(|n| matches_type(n, value))
}

fn check_union(root: &Value, schema: &Value, members: &[Value], value: &Value, path: &mut Vec<PathSegment>, flavor: Flavor, issues: &mut Vec<Issue>) {
    let mut candidates = Vec::new();
    for member in members {
        let mut nested = Vec::new();
        check(root, member, value, path, flavor, &mut nested);
        if nested.is_empty() {
            return;
        }
        candidates.push((member, nested));
    }
    // Members whose base type matches report their own issues.
    let typed: Vec<_> = candidates.iter().filter(|(member, _)| !type_names(member).is_empty() && matches_base_type(member, value)).collect();
    if typed.len() == 1 {
        issues.extend(typed[0].1.iter().cloned());
        return;
    }
    issues.push(Issue::new(path.clone(), format!("Expected {}", describe(root, schema))));
}

fn check_object(root: &Value, schema: &Map<String, Value>, object: &Map<String, Value>, path: &mut Vec<PathSegment>, flavor: Flavor, issues: &mut Vec<Issue>) {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let empty = Map::new();
    let properties = schema.get("properties").and_then(Value::as_object).unwrap_or(&empty);
    for (key, property) in properties {
        path.push(PathSegment::Key(key.clone()));
        match object.get(key) {
            Some(item) => check(root, property, item, path, flavor, issues),
            None => {
                if required.contains(&key.as_str()) {
                    issues.push(Issue::new(path.clone(), "Missing key"));
                }
            }
        }
        path.pop();
    }
    for key in &required {
        if properties.contains_key(*key) {
            continue;
        }
        if !object.contains_key(*key) {
            path.push(PathSegment::Key(key.to_string()));
            issues.push(Issue::new(path.clone(), "Missing key"));
            path.pop();
        }
    }
    if let Some(additional) = schema.get("additionalProperties").filter(|v| v.is_object()) {
        for (key, item) in object {
            if properties.contains_key(key) {
                continue;
            }
            path.push(PathSegment::Key(key.clone()));
            check(root, additional, item, path, flavor, issues);
            path.pop();
        }
    }
}

fn check_array(root: &Value, schema: &Map<String, Value>, items: &[Value], path: &mut Vec<PathSegment>, flavor: Flavor, issues: &mut Vec<Issue>) {
    let before = issues.len();
    if let Some(item_schema) = schema.get("items").filter(|v| v.is_object()) {
        for (index, item) in items.iter().enumerate() {
            path.push(PathSegment::Index(index));
            check(root, item_schema, item, path, flavor, issues);
            path.pop();
        }
    }
    if issues.len() > before {
        return;
    }
    if let Some(min) = schema.get("minItems").and_then(Value::as_u64) {
        if (items.len() as u64) < min {
            issues.push(Issue::new(path.clone(), format!("Expected a value with a length of at least {min}")));
        }
    }
    if let Some(max) = schema.get("maxItems").and_then(Value::as_u64) {
        if (items.len() as u64) > max {
            issues.push(Issue::new(path.clone(), format!("Expected a value with a length of at most {max}")));
        }
    }
    if schema.get("uniqueItems").and_then(Value::as_bool) == Some(true) {
        let mut seen: Vec<&Value> = Vec::new();
        for item in items {
            if seen.contains(&item) {
                issues.push(Issue::new(path.clone(), "Expected an array with unique items"));
                break;
            }
            seen.push(item);
        }
    }
}

fn check_string(schema: &Map<String, Value>, text: &str, path: &mut [PathSegment], flavor: Flavor, issues: &mut Vec<Issue>) {
    let points = text.chars().count() as u64;
    if let Some(min) = schema.get("minLength").and_then(Value::as_u64) {
        let failed = if min == 1 { js::utf16_len(text) == 0 } else { points < min };
        if failed {
            if min == 1 || flavor == Flavor::Effect {
                issues.push(Issue::new(path.to_vec(), format!("Expected a value with a length of at least {min}")));
            } else {
                issues.push(Issue::new(path.to_vec(), format!("Expected a string with at least {min} code points")));
            }
        }
    }
    if let Some(max) = schema.get("maxLength").and_then(Value::as_u64) {
        if points > max {
            issues.push(Issue::new(path.to_vec(), format!("Expected a string with at most {max} code points")));
        }
    }
    if flavor == Flavor::Effect {
        if let Some(Value::String(pattern)) = schema.get("pattern") {
            let literal = pattern.strip_prefix('^').filter(|rest| !rest.chars().any(|c| "\\.^$|?*+()[]{}".contains(c)));
            match literal {
                Some(prefix) => {
                    if !text.starts_with(prefix) {
                        issues.push(Issue::new(path.to_vec(), format!("Expected a string starting with {}", js::quote(prefix))));
                    }
                }
                None => {
                    if let Ok(re) = regex::Regex::new(pattern) {
                        if !re.is_match(text) {
                            issues.push(Issue::new(path.to_vec(), format!("Expected a string matching the RegExp {pattern}")));
                        }
                    }
                }
            }
        }
    }
}

fn check_number(schema: &Map<String, Value>, integer: bool, value: f64, path: &mut [PathSegment], flavor: Flavor, issues: &mut Vec<Issue>) {
    let num = |key: &str| schema.get(key).and_then(Value::as_f64);
    if integer && !is_safe_integer(value) {
        issues.push(Issue::new(path.to_vec(), "Expected an integer"));
    }
    let minimum = num("minimum");
    let maximum = num("maximum");
    if flavor == Flavor::Effect {
        if let (Some(min), Some(max)) = (minimum, maximum) {
            if !(value >= min && value <= max) {
                issues.push(Issue::new(
                    path.to_vec(),
                    format!("Expected a value between {} and {}", js::number(min), js::number(max)),
                ));
            }
            check_exclusive(schema, value, path, issues);
            check_multiple(schema, value, path, issues);
            return;
        }
    }
    if let Some(min) = minimum {
        if !(value >= min) {
            issues.push(Issue::new(path.to_vec(), format!("Expected a value greater than or equal to {}", js::number(min))));
        }
    }
    if let Some(max) = maximum {
        if !(value <= max) {
            issues.push(Issue::new(path.to_vec(), format!("Expected a value less than or equal to {}", js::number(max))));
        }
    }
    check_exclusive(schema, value, path, issues);
    check_multiple(schema, value, path, issues);
}

fn check_exclusive(schema: &Map<String, Value>, value: f64, path: &mut [PathSegment], issues: &mut Vec<Issue>) {
    if let Some(min) = schema.get("exclusiveMinimum").and_then(Value::as_f64) {
        if !(value > min) {
            issues.push(Issue::new(path.to_vec(), format!("Expected a value greater than {}", js::number(min))));
        }
    }
    if let Some(max) = schema.get("exclusiveMaximum").and_then(Value::as_f64) {
        if !(value < max) {
            issues.push(Issue::new(path.to_vec(), format!("Expected a value less than {}", js::number(max))));
        }
    }
}

fn check_multiple(schema: &Map<String, Value>, value: f64, path: &mut [PathSegment], issues: &mut Vec<Issue>) {
    if let Some(step) = schema.get("multipleOf").and_then(Value::as_f64) {
        if step != 0.0 && (value / step).fract() != 0.0 {
            issues.push(Issue::new(path.to_vec(), format!("Expected a value that is a multiple of {}", js::number(step))));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read_schema() -> Value {
        json!({"type":"object","properties":{"path":{"type":"string","description":"File or directory to read"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":0}},"required":["path"],"additionalProperties":false})
    }

    fn messages(schema: &Value, value: Value, flavor: Flavor) -> Vec<(String, String)> {
        validate(schema, &value, flavor).into_iter().map(|i| (i.path_text(), i.message)).collect()
    }

    #[test]
    fn effect_messages_match_runtime() {
        let schema = read_schema();
        assert_eq!(messages(&schema, json!({}), Flavor::Effect), vec![("path".into(), "Missing key".into())]);
        assert_eq!(
            messages(&schema, json!({"path": 1, "offset": -1, "limit": 1.5, "extra": true}), Flavor::Effect),
            vec![
                ("path".into(), "Expected string".into()),
                ("offset".into(), "Expected a value greater than or equal to 0".into()),
                ("limit".into(), "Expected an integer".into()),
            ]
        );
        assert_eq!(messages(&schema, json!("str"), Flavor::Effect), vec![("root".into(), "Expected object".into())]);
        assert_eq!(messages(&schema, json!(null), Flavor::Effect), vec![("root".into(), "Expected object".into())]);
        assert_eq!(messages(&schema, json!({"path":"a","offset":"x"}), Flavor::Effect), vec![("offset".into(), "Expected number".into())]);
        assert_eq!(
            messages(&schema, json!({"path":"x","limit":1.5,"offset":-1.5}), Flavor::Effect),
            vec![
                ("offset".into(), "Expected an integer".into()),
                ("offset".into(), "Expected a value greater than or equal to 0".into()),
                ("limit".into(), "Expected an integer".into()),
            ]
        );
    }

    #[test]
    fn nested_and_enums() {
        let q = json!({"type":"object","properties":{"questions":{"type":"array","items":{"type":"object","properties":{"question":{"type":"string"},"header":{"type":"string"},"options":{"type":"array","items":{"type":"object","properties":{"label":{"type":"string"},"description":{"type":"string"}},"required":["label","description"],"additionalProperties":false}},"multiple":{"type":"boolean"}},"required":["question","header","options"],"additionalProperties":false},"minItems":1}},"required":["questions"],"additionalProperties":false});
        assert_eq!(messages(&q, json!({"questions": []}), Flavor::Effect), vec![("questions".into(), "Expected a value with a length of at least 1".into())]);
        assert_eq!(
            messages(&q, json!({"questions": [{"question": 1}]}), Flavor::Effect),
            vec![
                ("questions[0].question".into(), "Expected string".into()),
                ("questions[0].header".into(), "Missing key".into()),
                ("questions[0].options".into(), "Missing key".into()),
            ]
        );
        assert_eq!(
            messages(&q, json!({"questions":[{"question":"q","header":"h","options":[{"label":1}],"multiple":null}]}), Flavor::Effect),
            vec![
                ("questions[0].options[0].label".into(), "Expected string".into()),
                ("questions[0].options[0].description".into(), "Missing key".into()),
                ("questions[0].multiple".into(), "Expected boolean".into()),
            ]
        );
        let w = json!({"type":"object","properties":{"url":{"type":"string"},"format":{"type":"string","enum":["text","markdown","html"]},"timeout":{"type":"number","exclusiveMinimum":0,"maximum":120}},"required":["url"],"additionalProperties":false});
        assert_eq!(
            messages(&w, json!({"url":"x","format":"pdf","timeout":0}), Flavor::Effect),
            vec![
                ("format".into(), "Expected \"text\" | \"markdown\" | \"html\"".into()),
                ("timeout".into(), "Expected a value greater than 0".into()),
            ]
        );
        assert_eq!(messages(&w, json!({"url":"x","timeout":121}), Flavor::Effect), vec![("timeout".into(), "Expected a value less than or equal to 120".into())]);
    }

    #[test]
    fn between_and_pattern() {
        let m = json!({"type":"object","properties":{"all":{"type":"boolean"},"limit":{"type":"integer","minimum":1,"maximum":100},"offset":{"type":"integer","minimum":0}},"additionalProperties":false});
        assert_eq!(
            messages(&m, json!({"limit":0,"offset":-1,"all":"x"}), Flavor::Effect),
            vec![
                ("all".into(), "Expected boolean".into()),
                ("limit".into(), "Expected a value between 1 and 100".into()),
                ("offset".into(), "Expected a value greater than or equal to 0".into()),
            ]
        );
        let s = json!({"type":"object","properties":{"sessionID":{"type":"string","pattern":"^ses"},"title":{"type":"string","minLength":1}},"required":["title"]});
        assert_eq!(
            messages(&s, json!({"title":"","sessionID":"abc"}), Flavor::Effect),
            vec![
                ("sessionID".into(), "Expected a string starting with \"ses\"".into()),
                ("title".into(), "Expected a value with a length of at least 1".into()),
            ]
        );
        // Raw JSON schemas with patterns are not validated at all.
        assert!(messages(&s, json!({"title": 5}), Flavor::Json).is_empty());
    }

    #[test]
    fn json_flavor_messages() {
        let s = |p: Value| json!({"type":"object","properties":{"a": p}});
        assert_eq!(messages(&s(json!({"type":"string","maxLength":3})), json!({"a":"xxxxx"}), Flavor::Json), vec![("a".into(), "Expected a string with at most 3 code points".into())]);
        assert_eq!(messages(&s(json!({"type":"string","minLength":2})), json!({"a":"x"}), Flavor::Json), vec![("a".into(), "Expected a string with at least 2 code points".into())]);
        assert_eq!(messages(&s(json!({"type":"number","minimum":1,"maximum":3})), json!({"a":5}), Flavor::Json), vec![("a".into(), "Expected a value less than or equal to 3".into())]);
        assert_eq!(messages(&s(json!({"type":"array","items":{"type":"string"},"minItems":2})), json!({"a":[1]}), Flavor::Json), vec![("a[0]".into(), "Expected string".into())]);
        assert_eq!(messages(&s(json!({"type":"array","maxItems":1})), json!({"a":[1,2]}), Flavor::Json), vec![("a".into(), "Expected a value with a length of at most 1".into())]);
        assert_eq!(messages(&s(json!({"enum":["x",1]})), json!({"a":"y"}), Flavor::Json), vec![("a".into(), "Expected \"x\" | 1".into())]);
        assert_eq!(messages(&s(json!({"const":"x"})), json!({"a":"y"}), Flavor::Json), vec![("a".into(), "Expected \"x\"".into())]);
        assert_eq!(messages(&s(json!({"type":["string","null"]})), json!({"a":1}), Flavor::Json), vec![("a".into(), "Expected string | null".into())]);
        assert_eq!(messages(&s(json!({"anyOf":[{"type":"string"},{"type":"number"}]})), json!({"a":true}), Flavor::Json), vec![("a".into(), "Expected string | number".into())]);
        assert_eq!(messages(&s(json!({"type":"number","multipleOf":2})), json!({"a":3}), Flavor::Json), vec![("a".into(), "Expected a value that is a multiple of 2".into())]);
        assert_eq!(messages(&s(json!({"type":"array","uniqueItems":true})), json!({"a":[1,1]}), Flavor::Json), vec![("a".into(), "Expected an array with unique items".into())]);
        assert_eq!(messages(&s(json!({"type":"object","additionalProperties":{"type":"number"}})), json!({"a":{"x":"s"}}), Flavor::Json), vec![("a.x".into(), "Expected number".into())]);
        assert_eq!(
            messages(&json!({"type":"object","properties":{"a":{"type":"number"},"b":{"type":"string"}},"required":["b"]}), json!({"a":"1"}), Flavor::Json),
            vec![("a".into(), "Expected number".into()), ("b".into(), "Missing key".into())]
        );
        assert_eq!(messages(&json!({"type":"object","required":["a"]}), json!({}), Flavor::Json), vec![("a".into(), "Missing key".into())]);
        assert_eq!(
            messages(&s(json!({"anyOf":[{"type":"object","properties":{"x":{"type":"string"}},"required":["x"]},{"type":"number"}]})), json!({"a":{}}), Flavor::Json),
            vec![("a.x".into(), "Missing key".into())]
        );
        assert_eq!(
            messages(&json!({"type":"object","properties":{"a":{"$ref":"#/definitions/X"}},"definitions":{"X":{"type":"number"}}}), json!({"a":"s"}), Flavor::Json),
            vec![("a".into(), "Expected X".into())]
        );
        assert!(messages(&json!({}), json!(5), Flavor::Json).is_empty());
    }

    #[test]
    fn formats_issue_list() {
        let issues: Vec<Issue> = (0..7).map(|i| Issue::new(vec![PathSegment::Key(format!("k{i}"))], "Missing key")).collect();
        let text = format_input_issues("read", &issues, &json!({"a": 1}));
        assert_eq!(
            text,
            "Invalid arguments for tool \"read\":\n- k0: Missing key\n- k1: Missing key\n- k2: Missing key\n- k3: Missing key\n- k4: Missing key\n- ...and 2 more issues\n\nArguments provided:\n{\n  \"a\": 1\n}\n\nUpdate the arguments and call the tool again."
        );
        let six: Vec<Issue> = issues[..6].to_vec();
        assert!(format_input_issues("x", &six, &json!({})).contains("- ...and 1 more issue\n"));
        let root = vec![Issue::new(vec![], "Expected object")];
        assert!(format_input_issues("x", &root, &json!("s")).starts_with("Invalid arguments for tool \"x\":\n- root: Expected object"));
    }
}
