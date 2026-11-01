//! Tool input repair (`core/src/plugin/tool-input-repair.ts`, the built-in
//! `opencode.tool.input.repair` plugin's `tool.execute.before` hook).
//!
//! Repairs apply only when the input schema unambiguously supports them:
//! - Stringified root or nested object: `'{"limit":"20"}'` -> `{ limit: 20 }`
//! - Closed object: `{ limit: "20", extra: true }` -> `{ limit: 20 }`
//! - Optional null or empty-object placeholder: `{ limit: null }` -> `{}`
//! - Numeric or boolean string: `{ limit: "20", enabled: "false" }` -> `{ limit: 20, enabled: false }`
//! - Nullable field: `{ count: "2" }` -> `{ count: 2 }`
//! - Stringified array or compatible item: `{ tags: '["a"]', count: "2" }` -> `{ tags: ["a"], count: [2] }`
//! - Positional tuple: `{ pair: ["2", "false"] }` -> `{ pair: [2, false] }`
//! - Typed dictionary: `{ counts: { first: "2" } }` -> `{ counts: { first: 2 } }`
//! - Nested fields and local references: `{ items: [{ count: "2" }] }` -> `{ items: [{ count: 2 }] }`
//!
//! The schema is the tool's registered definition `inputSchema` (`definition(tool).inputSchema`).
//! JavaScript semantics that matter here are reproduced: `typeof` (null and arrays are
//! `"object"`), `Predicate.isObject` (plain objects only), `Number(string)` and `JSON.parse`.

use serde_json::{Map, Number, Value};

use crate::util::js;

/// Built-in plugin id.
pub const PLUGIN_ID: &str = "opencode.tool.input.repair";

const MAX_DEPTH: usize = 6;

/// The hook body for one call: `input` repaired against the tool's registered input schema.
/// Roots that are not `type: "object"` schemas are left alone.
pub fn repair_input(input: &Value, schema: &Value) -> Value {
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return input.clone();
    }
    repair(input, schema, schema, 0)
}

/// `Predicate.isObject`: a non-null, non-array object.
fn object(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

/// JavaScript `typeof` of a JSON value.
fn type_of(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        Value::Null | Value::Array(_) | Value::Object(_) => "object",
    }
}

/// `schema.type === <string>`.
fn type_is(schema: &Map<String, Value>, ty: &str) -> bool {
    schema.get("type").and_then(Value::as_str) == Some(ty)
}

fn repair(value: &Value, schema: &Value, root: &Value, depth: usize) -> Value {
    if depth > MAX_DEPTH {
        return value.clone();
    }
    let Some(schema_map) = object(schema) else { return value.clone() };

    if let Some(reference) = schema_map.get("$ref").and_then(Value::as_str) {
        let definitions = if local_ref(reference, "#/$defs/") {
            root.get("$defs")
        } else if local_ref(reference, "#/definitions/") {
            root.get("definitions")
        } else {
            None
        };
        let Some(definitions) = definitions.and_then(object) else { return value.clone() };
        let name = reference[reference.rfind('/').map_or(0, |i| i + 1)..].replace("~1", "/").replace("~0", "~");
        return match definitions.get(&name).filter(|target| target.is_object()) {
            Some(target) => repair(value, target, root, depth + 1),
            None => value.clone(),
        };
    }

    match schema_map.get("type") {
        Some(Value::Array(types)) => {
            if value.is_null() && types.iter().any(|t| t == "null") {
                return value.clone();
            }
            if types.iter().any(|t| t.as_str() == Some(type_of(value))) {
                return value.clone();
            }
            let rest: Vec<&Value> = types.iter().filter(|t| *t != "null").collect();
            if rest.len() != 1 {
                return value.clone();
            }
            let mut narrowed = schema_map.clone();
            narrowed.insert("type".into(), rest[0].clone());
            repair(value, &Value::Object(narrowed), root, depth + 1)
        }
        None => {
            let any_of = schema_map.get("anyOf").and_then(Value::as_array);
            let one_of = schema_map.get("oneOf").and_then(Value::as_array);
            if any_of.is_some() && one_of.is_some() {
                return value.clone();
            }
            let Some(branches) = any_of.or(one_of) else { return value.clone() };
            if value.is_null() {
                return value.clone();
            }
            let typeof_value = type_of(value);
            if branches.iter().any(|branch| match object(branch) {
                None => true,
                Some(branch) => type_is(branch, typeof_value),
            }) {
                return value.clone();
            }
            let candidates: Vec<&Value> = branches.iter().filter(|branch| !type_is(branch.as_object().unwrap(), "null")).collect();
            if candidates.len() == 1 { repair(value, candidates[0], root, depth + 1) } else { value.clone() }
        }
        Some(Value::String(ty)) => match ty.as_str() {
            "number" | "integer" => {
                let Value::String(text) = value else { return value.clone() };
                if js::trim(text).is_empty() {
                    return value.clone();
                }
                let parsed = js_number(text);
                if parsed.is_finite() && (ty != "integer" || safe_integer(parsed)) { number(parsed) } else { value.clone() }
            }
            "boolean" => match value.as_str() {
                Some("true") => Value::Bool(true),
                Some("false") => Value::Bool(false),
                _ => value.clone(),
            },
            "object" => repair_object(value, schema_map, root, depth),
            "array" => repair_array(value, schema_map, root, depth),
            _ => value.clone(),
        },
        Some(_) => value.clone(),
    }
}

/// `/^#\/\$defs\/[^/]+$/` and `/^#\/definitions\/[^/]+$/`.
fn local_ref(reference: &str, prefix: &str) -> bool {
    reference.strip_prefix(prefix).is_some_and(|name| !name.is_empty() && !name.contains('/'))
}

/// `Schema.decodeUnknownOption(Schema.fromJsonString(Schema.Unknown))` for strings, else the value.
fn parse(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) => serde_json::from_str::<Value>(text).ok().map(js_numbers),
        other => Some(other.clone()),
    }
}

const COMPOSITIONS: [&str; 3] = ["allOf", "anyOf", "oneOf"];

fn repair_object(value: &Value, schema: &Map<String, Value>, root: &Value, depth: usize) -> Value {
    let Some(Value::Object(parsed)) = parse(value) else { return value.clone() };

    let empty = Map::new();
    let properties = schema.get("properties").and_then(object).unwrap_or(&empty);
    let required: &[Value] = schema.get("required").and_then(Value::as_array).map_or(&[], Vec::as_slice);
    let patterned = schema.get("patternProperties").is_some_and(Value::is_object);
    let composed = COMPOSITIONS.iter().any(|keyword| schema.get(*keyword).is_some_and(Value::is_array));
    let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));

    let mut result = Map::new();
    for (key, current) in parsed {
        let declared = properties.contains_key(&key);
        let property = if declared {
            properties.get(&key)
        } else if !patterned {
            schema.get("additionalProperties")
        } else {
            None
        };

        if !declared && closed && !patterned && !composed {
            continue;
        }
        let Some((property_schema, property)) = property.and_then(|p| object(p).map(|map| (p, map))) else {
            result.insert(key, current);
            continue;
        };

        // Only a bare single-type property provably rejects null and `{}`. Compositions, enums,
        // constants, references and nullable flags may accept them, so those are left alone.
        let plain = property.get("type").and_then(Value::as_str).is_some_and(|t| t != "null")
            && !composed
            && ["anyOf", "oneOf", "allOf", "enum", "const", "$ref", "nullable"].iter().all(|keyword| !property.contains_key(*keyword));
        let placeholder = current.as_object().is_some_and(Map::is_empty) && !type_is(property, "object");
        if declared && !required.iter().any(|r| r.as_str() == Some(key.as_str())) && plain && (current.is_null() || placeholder) {
            continue;
        }

        let repaired = repair(&current, property_schema, root, depth + 1);
        result.insert(key, repaired);
    }
    Value::Object(result)
}

fn repair_array(value: &Value, schema: &Map<String, Value>, root: &Value, depth: usize) -> Value {
    let parsed = parse(value);
    let prefix_items = schema.get("prefixItems").and_then(Value::as_array);
    let tuple = prefix_items.or_else(|| schema.get("items").and_then(Value::as_array));

    if let Some(Value::Array(items)) = parsed {
        return Value::Array(
            items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let member = match tuple {
                        // `tuple[index] ?? rest`: a JSON null position falls through too.
                        Some(tuple) => match tuple.get(index).filter(|m| !m.is_null()) {
                            Some(member) => Some(member),
                            None if prefix_items.is_some() => schema.get("items"),
                            None => schema.get("additionalItems"),
                        },
                        None => schema.get("items"),
                    };
                    match member {
                        Some(member) if member.is_object() => repair(item, member, root, depth + 1),
                        _ => item.clone(),
                    }
                })
                .collect(),
        );
    }

    let Some(items) = schema.get("items").filter(|items| tuple.is_none() && items.is_object()) else { return value.clone() };
    let repaired = repair(value, items, root, depth + 1);
    let compatible = match items.get("type").and_then(Value::as_str) {
        Some("object") => repaired.is_object(),
        Some("array") => repaired.is_array(),
        Some("integer") => repaired.as_f64().is_some_and(safe_integer),
        Some("number") => repaired.as_f64().is_some_and(f64::is_finite),
        Some("string") => repaired.is_string(),
        Some("boolean") => repaired.is_boolean(),
        _ => false,
    };
    if compatible { Value::Array(vec![repaired]) } else { value.clone() }
}

/// `Number.isSafeInteger`.
fn safe_integer(value: f64) -> bool {
    value.is_finite() && value.trunc() == value && value.abs() <= 9_007_199_254_740_991.0
}

/// A JavaScript number as JSON: integral values without a fraction (`-0` is `0`).
fn number(value: f64) -> Value {
    if value.trunc() == value && value.abs() < 9.2e18 {
        Value::Number(Number::from(value as i64))
    } else {
        Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

/// `JSON.parse` numbers are doubles: integral floats (`1.0`) read as integers.
fn js_numbers(value: Value) -> Value {
    match value {
        Value::Number(n) if n.is_f64() => number(n.as_f64().unwrap_or(0.0)),
        Value::Array(items) => Value::Array(items.into_iter().map(js_numbers).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, js_numbers(v))).collect()),
        other => other,
    }
}

/// JavaScript `Number(string)` (`StringToNumber`); NaN when the text is not a numeric literal.
pub fn js_number(text: &str) -> f64 {
    let text = js::trim(text);
    if text.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = text.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return match u128::from_str_radix(digits, radix) {
                Ok(value) => value as f64,
                Err(_) => digits.chars().fold(0.0, |acc, c| acc * radix as f64 + c.to_digit(radix).unwrap_or(0) as f64),
            };
        }
    }
    let (sign, unsigned) = match text.as_bytes()[0] {
        b'+' => (1.0, &text[1..]),
        b'-' => (-1.0, &text[1..]),
        _ => (1.0, text),
    };
    if unsigned == "Infinity" {
        return sign * f64::INFINITY;
    }
    if !decimal_literal(unsigned) {
        return f64::NAN;
    }
    unsigned.parse::<f64>().map_or(f64::NAN, |value| sign * value)
}

/// `StrUnsignedDecimalLiteral` without `Infinity`: `1`, `1.`, `1.5`, `.5`, each with an optional exponent.
fn decimal_literal(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < bytes.len() && bytes[*i].is_ascii_digit() {
            *i += 1;
        }
        *i - start
    };
    let mut mantissa = digits(&mut i);
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        mantissa += digits(&mut i);
    }
    if mantissa == 0 {
        return false;
    }
    if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
        i += 1;
        if i < bytes.len() && matches!(bytes[i], b'+' | b'-') {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return false;
        }
    }
    i == bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Ported from upstream `packages/core/test/plugin/tool-input-repair.test.ts`.

    fn object(properties: Value, required: Option<Value>) -> Value {
        let mut schema = json!({ "type": "object", "properties": properties });
        if let Some(required) = required {
            schema["required"] = required;
        }
        schema
    }

    fn with(mut schema: Value, extra: Value) -> Value {
        for (key, value) in extra.as_object().unwrap() {
            schema[key] = value.clone();
        }
        schema
    }

    #[test]
    fn preserves_valid_input_nested_containers_and_unknown_properties() {
        let input = json!({ "count": 2, "nested": { "enabled": true }, "items": [2, 3], "extra": "keep" });
        let schema = object(
            json!({
                "count": { "type": "integer" },
                "nested": object(json!({ "enabled": { "type": "boolean" } }), None),
                "items": { "type": "array", "items": { "type": "number" } },
            }),
            None,
        );
        assert_eq!(repair_input(&input, &schema), input);
    }

    #[test]
    fn parses_root_objects_and_repairs_nested_stringified_containers() {
        let schema = object(
            json!({
                "count": { "type": "integer" },
                "item": object(json!({ "enabled": { "type": "boolean" } }), None),
                "list": { "type": "array", "items": { "type": "integer" } },
            }),
            None,
        );
        assert_eq!(
            repair_input(&json!(r#"{"count":"2","item":"{\"enabled\":\"false\"}","list":"[\"3\"]"}"#), &schema),
            json!({ "count": 2, "item": { "enabled": false }, "list": [3] })
        );
        assert_eq!(repair_input(&json!("{broken"), &schema), json!("{broken"));
        assert_eq!(repair_input(&json!("[]"), &schema), json!("[]"));
        assert_eq!(repair_input(&Value::Null, &schema), Value::Null);
    }

    #[test]
    fn removes_extras_only_from_explicitly_closed_objects() {
        let input = json!({
            "known": "2",
            "extra": true,
            "closed": { "keep": "3", "extra": true },
            "open": { "keep": "4", "extra": true },
            "items": [{ "keep": "5", "extra": true }],
        });
        let schema = with(
            object(
                json!({
                    "known": { "type": "integer" },
                    "closed": with(object(json!({ "keep": { "type": "integer" } }), None), json!({ "additionalProperties": false })),
                    "open": object(json!({ "keep": { "type": "integer" } }), None),
                    "items": {
                        "type": "array",
                        "items": with(object(json!({ "keep": { "type": "integer" } }), None), json!({ "additionalProperties": false })),
                    },
                }),
                None,
            ),
            json!({ "additionalProperties": false }),
        );
        assert_eq!(
            repair_input(&input, &schema),
            json!({ "known": 2, "closed": { "keep": 3 }, "open": { "keep": 4, "extra": true }, "items": [{ "keep": 5 }] })
        );
        assert_eq!(
            repair_input(&json!({ "extra": true }), &with(object(json!({}), None), json!({ "additionalProperties": false }))),
            json!({})
        );
    }

    #[test]
    fn preserves_unknown_keys_when_patterned_ownership_cannot_be_determined() {
        let input = json!({ "known": 1, "match": "2", "extra": true });
        let schema = with(
            object(json!({ "known": { "type": "integer" } }), None),
            json!({ "additionalProperties": false, "patternProperties": { "^match$": { "type": "integer" } } }),
        );
        assert_eq!(repair_input(&input, &schema), input);
    }

    #[test]
    fn preserves_properties_that_may_belong_to_composed_object_schemas() {
        let input = json!({ "name": "example", "extra": true });
        for keyword in ["allOf", "anyOf", "oneOf"] {
            let mut schema = json!({ "type": "object", "additionalProperties": false });
            schema[keyword] = json!([object(json!({ "name": { "type": "string" } }), None)]);
            assert_eq!(repair_input(&input, &schema), input, "{keyword}");
        }
    }

    #[test]
    fn removes_only_optional_nonnullable_nulls_and_non_object_empty_placeholders() {
        let input = json!({
            "optional": null,
            "required": null,
            "nullable": null,
            "union": null,
            "constant": null,
            "permissive": null,
            "referenced": null,
            "placeholder": {},
            "array": {},
            "requiredPlaceholder": {},
            "object": {},
            "unknown": null,
        });
        let schema = object(
            json!({
                "optional": { "type": "string" },
                "required": { "type": "string" },
                "nullable": { "type": "string", "nullable": true },
                "union": { "anyOf": [{ "type": "integer" }, { "type": "null" }] },
                "constant": { "anyOf": [{ "type": "integer" }, { "const": null }] },
                "permissive": { "anyOf": [{ "type": "integer" }, true] },
                "referenced": { "anyOf": [{ "type": "integer" }, { "$ref": "#/$defs/nullable" }] },
                "placeholder": { "type": "integer" },
                "array": { "type": "array", "items": { "type": "string" } },
                "requiredPlaceholder": { "type": "boolean" },
                "object": { "type": "object" },
                "unknown": {},
            }),
            Some(json!(["required", "requiredPlaceholder"])),
        );
        let output = repair_input(&input, &schema);
        assert_eq!(
            output,
            json!({
                "required": null,
                "nullable": null,
                "union": null,
                "constant": null,
                "permissive": null,
                "referenced": null,
                "requiredPlaceholder": {},
                "object": {},
                "unknown": null,
            })
        );
        // Key order follows the input.
        assert_eq!(
            output.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["required", "nullable", "union", "constant", "permissive", "referenced", "requiredPlaceholder", "object", "unknown"]
        );
    }

    #[test]
    fn preserves_nulls_whose_validity_is_hidden_inside_compositions() {
        let input = json!({ "wrapped": null, "enumerated": null, "permissive": null, "constant": null });
        let schema = object(
            json!({
                "wrapped": { "anyOf": [{ "type": ["string", "null"] }] },
                "enumerated": { "anyOf": [{ "enum": [null, "keep"] }] },
                "permissive": { "anyOf": [{}] },
                "constant": { "type": "string", "const": "keep" },
            }),
            None,
        );
        assert_eq!(repair_input(&input, &schema), input);
    }

    #[test]
    fn preserves_optional_looking_nulls_when_the_parent_composes_requirements() {
        let input = json!({ "value": null });
        let schema = with(object(json!({ "value": { "type": "string" } }), None), json!({ "allOf": [{ "required": ["value"] }] }));
        assert_eq!(repair_input(&input, &schema), input);
    }

    #[test]
    fn coerces_numeric_and_boolean_strings_while_preserving_invalid_and_existing_values() {
        let input = json!({
            "number": "1.5",
            "integer": "42",
            "enabled": "true",
            "disabled": "false",
            "valid": 3,
            "empty": " ",
            "infinite": "Infinity",
            "fractional": "1.5",
            "unsafe": "9007199254740992",
            "uppercase": "TRUE",
        });
        let schema = object(
            json!({
                "number": { "type": "number" },
                "integer": { "type": "integer" },
                "enabled": { "type": "boolean" },
                "disabled": { "type": "boolean" },
                "valid": { "type": "integer" },
                "empty": { "type": "number" },
                "infinite": { "type": "number" },
                "fractional": { "type": "integer" },
                "unsafe": { "type": "integer" },
                "uppercase": { "type": "boolean" },
            }),
            None,
        );
        let mut expected = input.clone();
        expected["number"] = json!(1.5);
        expected["integer"] = json!(42);
        expected["enabled"] = json!(true);
        expected["disabled"] = json!(false);
        assert_eq!(repair_input(&input, &schema), expected);
    }

    #[test]
    fn wraps_compatible_scalars_after_repairing_array_items() {
        let input = json!({
            "text": "one",
            "integer": "42",
            "boolean": "false",
            "item": "{\"count\":\"2\"}",
            "incompatible": 2,
            "fractional": 1.5,
            "unconstrained": "4",
        });
        let schema = object(
            json!({
                "text": { "type": "array", "items": { "type": "string" } },
                "integer": { "type": "array", "items": { "type": "integer" } },
                "boolean": { "type": "array", "items": { "type": "boolean" } },
                "item": { "type": "array", "items": object(json!({ "count": { "type": "integer" } }), None) },
                "incompatible": { "type": "array", "items": { "type": "string" } },
                "fractional": { "type": "array", "items": { "type": "integer" } },
                "unconstrained": { "type": "array" },
            }),
            None,
        );
        assert_eq!(
            repair_input(&input, &schema),
            json!({
                "text": ["one"],
                "integer": [42],
                "boolean": [false],
                "item": [{ "count": 2 }],
                "incompatible": 2,
                "fractional": 1.5,
                "unconstrained": "4",
            })
        );
    }

    #[test]
    fn repairs_nested_question_like_inputs() {
        let input = json!({
            "questions": [{ "question": "Pick one", "multiple": "false", "options": { "label": "First", "description": null } }],
        });
        let schema = object(
            json!({
                "questions": {
                    "type": "array",
                    "items": object(
                        json!({
                            "question": { "type": "string" },
                            "multiple": { "type": "boolean" },
                            "options": {
                                "type": "array",
                                "items": object(json!({ "label": { "type": "string" }, "description": { "type": "string" } }), Some(json!(["label"]))),
                            },
                        }),
                        None,
                    ),
                },
            }),
            None,
        );
        assert_eq!(
            repair_input(&input, &schema),
            json!({ "questions": [{ "question": "Pick one", "multiple": false, "options": [{ "label": "First" }] }] })
        );
    }

    #[test]
    fn repairs_unique_nullable_alternatives_while_preserving_accepted_union_values() {
        let input = json!({
            "number": "2",
            "boolean": "false",
            "nullable": null,
            "typed": "3",
            "typedBoolean": "true",
            "accepted": "4",
            "valid": 5,
        });
        let schema = object(
            json!({
                "number": { "anyOf": [{ "type": "number" }, { "type": "null" }] },
                "boolean": { "oneOf": [{ "type": "boolean" }, { "type": "null" }] },
                "nullable": { "anyOf": [{ "type": "number" }, { "type": "null" }] },
                "typed": { "type": ["integer", "null"] },
                "typedBoolean": { "type": ["boolean", "null"] },
                "accepted": { "anyOf": [{ "type": "string" }, { "type": "number" }] },
                "valid": { "type": ["number", "null"] },
            }),
            None,
        );
        let mut expected = input.clone();
        expected["number"] = json!(2);
        expected["boolean"] = json!(false);
        expected["typed"] = json!(3);
        expected["typedBoolean"] = json!(true);
        assert_eq!(repair_input(&input, &schema), expected);
    }

    #[test]
    fn repairs_tuple_positions_and_rest_items() {
        let input = json!({ "prefix": ["2", "false", "3"], "draft": "[\"4\",\"true\"]", "valid": [2, false], "scalar": "5" });
        let schema = object(
            json!({
                "prefix": { "type": "array", "prefixItems": [{ "type": "integer" }, { "type": "boolean" }], "items": { "type": "number" } },
                "draft": { "type": "array", "items": [{ "type": "integer" }, { "type": "boolean" }] },
                "valid": { "type": "array", "prefixItems": [{ "type": "integer" }, { "type": "boolean" }] },
                "scalar": { "type": "array", "prefixItems": [{ "type": "integer" }] },
            }),
            None,
        );
        assert_eq!(
            repair_input(&input, &schema),
            json!({ "prefix": [2, false, 3], "draft": [4, true], "valid": [2, false], "scalar": "5" })
        );
    }

    #[test]
    fn repairs_typed_dictionaries_and_straightforward_local_references() {
        let input = json!({
            "modern": "2",
            "legacy": "false",
            "nested": { "count": "3" },
            "dictionary": { "first": "4" },
            "missing": "5",
            "pointer": "6",
            "escaped": "7",
        });
        let schema = with(
            object(
                json!({
                    "modern": { "$ref": "#/$defs/integer" },
                    "legacy": { "$ref": "#/definitions/boolean" },
                    "nested": { "$ref": "#/$defs/nested" },
                    "dictionary": { "type": "object", "additionalProperties": { "$ref": "#/$defs/integer" } },
                    "missing": { "$ref": "#/$defs/missing" },
                    "pointer": { "$ref": "#/$defs/nested/properties/count" },
                    "escaped": { "$ref": "#/$defs/a~1b~0c" },
                }),
                None,
            ),
            json!({
                "$defs": {
                    "integer": { "type": "integer" },
                    "a/b~c": { "type": "integer" },
                    "nested": object(json!({ "count": { "$ref": "#/$defs/integer" } }), None),
                },
                "definitions": { "boolean": { "type": "boolean" } },
            }),
        );
        assert_eq!(
            repair_input(&input, &schema),
            json!({
                "modern": 2,
                "legacy": false,
                "nested": { "count": 3 },
                "dictionary": { "first": 4 },
                "missing": "5",
                "pointer": "6",
                "escaped": 7,
            })
        );
    }

    #[test]
    fn leaves_ambiguous_unions_compositions_and_unsupported_roots_unchanged() {
        let input = json!({ "numeric": "2", "objects": { "value": "3" }, "both": "4", "composed": "5", "unknown": "6" });
        let schema = object(
            json!({
                "numeric": { "anyOf": [{ "type": "number" }, { "type": "integer" }] },
                "objects": {
                    "oneOf": [
                        object(json!({ "value": { "type": "integer" } }), Some(json!(["value"]))),
                        object(json!({ "value": { "type": "number" } }), Some(json!(["value"]))),
                    ],
                },
                "both": { "anyOf": [{ "type": "integer" }], "oneOf": [{ "type": "integer" }] },
                "composed": { "allOf": [{ "type": "integer" }] },
                "unknown": {},
            }),
            None,
        );
        assert_eq!(repair_input(&input, &schema), input);
        assert_eq!(repair_input(&input, &json!({ "properties": { "numeric": { "type": "integer" } } })), input);
        assert_eq!(
            repair_input(&input, &json!({ "allOf": [object(json!({ "numeric": { "type": "integer" } }), None)] })),
            input
        );
    }

    // Rust-only cases.

    #[test]
    fn stops_below_the_depth_limit() {
        // depth 0 root, each nested object property adds one; past depth 6 values are kept.
        let mut schema = json!({ "type": "integer" });
        for _ in 0..7 {
            schema = object(json!({ "n": schema }), None);
        }
        let mut input = json!("1");
        for _ in 0..7 {
            input = json!({ "n": input });
        }
        assert_eq!(repair_input(&input, &schema), input);
        let mut schema = json!({ "type": "integer" });
        for _ in 0..6 {
            schema = object(json!({ "n": schema }), None);
        }
        let mut input = json!("1");
        let mut expected = json!(1);
        for _ in 0..6 {
            input = json!({ "n": input });
            expected = json!({ "n": expected });
        }
        assert_eq!(repair_input(&input, &schema), expected);
    }

    #[test]
    fn json_strings_read_numbers_like_javascript() {
        let schema = object(json!({ "count": { "type": "integer" }, "size": { "type": "number" } }), None);
        assert_eq!(repair_input(&json!("{\"count\":2.0,\"size\":1e2}"), &schema), json!({ "count": 2, "size": 100 }));
        assert_eq!(repair_input(&json!(" {\"count\":\"0x10\"} "), &schema), json!({ "count": 16 }));
        assert_eq!(repair_input(&json!({ "count": "-0" }), &schema), json!({ "count": 0 }));
    }

    #[test]
    fn number_conversion_matches_javascript() {
        let cases: [(&str, f64); 18] = [
            ("42", 42.0),
            (" 1.5\n", 1.5),
            ("+5", 5.0),
            ("-0", -0.0),
            (".5", 0.5),
            ("5.", 5.0),
            ("1e3", 1000.0),
            ("1E-2", 0.01),
            ("0x1A", 26.0),
            ("0b101", 5.0),
            ("0o17", 15.0),
            ("\u{FEFF}7\u{3000}", 7.0),
            ("Infinity", f64::INFINITY),
            ("-Infinity", f64::NEG_INFINITY),
            ("00012", 12.0),
            ("1e400", f64::INFINITY),
            ("", 0.0),
            ("9007199254740993", 9007199254740992.0),
        ];
        for (text, expected) in cases {
            assert_eq!(js_number(text), expected, "{text:?}");
        }
        for text in ["abc", "1_000", "-0x10", "0x", "1e", "e5", ".", "inf", "NaN", "infinity", "1.2.3", "TRUE", "0x1G"] {
            assert!(js_number(text).is_nan(), "{text:?}");
        }
        assert!(js_number("-0").is_sign_negative());
        assert_eq!(number(-0.0), Value::from(0));
        assert_eq!(number(1.5), Value::from(1.5));
    }
}
