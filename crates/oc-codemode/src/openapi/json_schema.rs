//! The parts of Effect's `JsonSchema` module (effect 4.0.1) that `normalizeSchema` relies on:
//! `fromSchemaOpenApi3_0` / `fromSchemaOpenApi3_1` (to Draft 2020-12 documents) and the JSON Pointer URI-fragment
//! helpers they use.

use std::collections::HashSet;

use serde_json::{Map, Value as Json};

const META_SCHEMA_URI_DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";
const META_SCHEMA_URI_OPEN_API_3_1: &str = "https://spec.openapis.org/oas/3.1/dialect/base";

/// A Draft 2020-12 document: the schema without `$defs`, and the definitions.
pub(crate) type Document = (Map<String, Json>, Map<String, Json>);

// ---------------------------------------------------------------------------------------------------------------
// JSON Pointer URI fragments (`JsonPointer.ts`)

fn is_uri_unescaped(c: char) -> bool {
    c.is_ascii_alphanumeric() || ";,/?:@&=+$-_.!~*'()#".contains(c)
}

/// `encodeURI` (Rust strings cannot hold lone surrogates, so it never throws).
pub(crate) fn encode_uri(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if is_uri_unescaped(c) {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

/// `decodeURIComponent`; `None` where it throws a `URIError`.
pub(crate) fn decode_uri_component(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn escape_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

fn unescape_token(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

fn decode_uri_fragment(fragment: &str) -> Option<String> {
    if fragment.is_empty() || fragment == "#" {
        return Some(String::new());
    }
    let encoded = fragment.strip_prefix('#')?;
    if encode_uri(encoded).replace("%25", "%").replace('#', "%23") != encoded {
        return None;
    }
    let pointer = decode_uri_component(encoded)?;
    let chars: Vec<char> = pointer.chars().collect();
    let bad_escape =
        chars.iter().enumerate().any(|(i, c)| *c == '~' && !matches!(chars.get(i + 1), Some('0') | Some('1')));
    if pointer.starts_with('/') && !bad_escape { Some(pointer) } else { None }
}

pub(crate) fn parse_uri_fragment(fragment: &str) -> Option<Vec<String>> {
    let pointer = decode_uri_fragment(fragment)?;
    if pointer.is_empty() {
        return Some(Vec::new());
    }
    Some(pointer[1..].split('/').map(unescape_token).collect())
}

pub(crate) fn format_uri_fragment(path: &[String]) -> String {
    let mut out = String::from("#");
    for token in path {
        out.push('/');
        out.push_str(&encode_uri(&escape_token(token)).replace('#', "%23"));
    }
    out
}

fn rewrite_open_api_components_reference(reference: &str) -> String {
    if reference.starts_with('#')
        && let Some(path) = parse_uri_fragment(reference)
        && path.len() >= 2
        && path[0] == "components"
        && path[1] == "schemas"
    {
        let mut next = vec!["$defs".to_string()];
        next.extend(path[2..].iter().cloned());
        return format_uri_fragment(&next);
    }
    reference.to_string()
}

fn is_meta_schema_uri(value: Option<&Json>, uri: &str) -> bool {
    let alternative = match uri.strip_suffix('#') {
        Some(stripped) => stripped.to_string(),
        None => format!("{uri}#"),
    };
    matches!(value, Some(Json::String(s)) if s == uri || *s == alternative)
}

fn creates_resource(id: Option<&Json>) -> bool {
    matches!(id, Some(Json::String(s)) if !s.is_empty() && !s.starts_with('#'))
}

/// `fromSchemaDraft2020_12`: splits `$defs` off the schema.
fn from_draft_2020_12(mut schema: Map<String, Json>) -> Document {
    let definitions = match schema.shift_remove("$defs") {
        Some(Json::Object(defs)) => defs,
        _ => Map::new(),
    };
    (schema, definitions)
}

// ---------------------------------------------------------------------------------------------------------------
// OpenAPI 3.1

pub(crate) fn from_schema_open_api_3_1(js: &Map<String, Json>) -> Document {
    let root_resource = creates_resource(js.get("$id"));
    let walked = walk_3_1(js, false, true, root_resource);
    from_draft_2020_12(walked)
}

fn walk_3_1_value(node: &Json, inherited: bool, root_resource: bool) -> Json {
    match node {
        Json::Object(map) => Json::Object(walk_3_1(map, inherited, false, root_resource)),
        other => other.clone(),
    }
}

fn walk_3_1(node: &Map<String, Json>, inherited: bool, is_root: bool, root_resource: bool) -> Map<String, Json> {
    let embedded = inherited || (!is_root && creates_resource(node.get("$id")));
    let mut out = Map::new();
    for (key, value) in node {
        let transformed = match key.as_str() {
            "$defs" | "properties" | "patternProperties" | "dependentSchemas" => match value {
                Json::Object(map) => Json::Object(
                    map.iter().map(|(k, v)| (k.clone(), walk_3_1_value(v, embedded, root_resource))).collect(),
                ),
                other => other.clone(),
            },
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => match value {
                Json::Array(items) => {
                    Json::Array(items.iter().map(|v| walk_3_1_value(v, embedded, root_resource)).collect())
                }
                other => other.clone(),
            },
            "not"
            | "additionalProperties"
            | "propertyNames"
            | "unevaluatedProperties"
            | "items"
            | "contains"
            | "unevaluatedItems"
            | "if"
            | "then"
            | "else"
            | "contentSchema" => walk_3_1_value(value, embedded, root_resource),
            _ => value.clone(),
        };
        out.insert(key.clone(), transformed);
    }
    if !root_resource && !embedded {
        for keyword in ["$ref", "$dynamicRef"] {
            if let Some(Json::String(reference)) = out.get(keyword) {
                let rewritten = rewrite_open_api_components_reference(reference);
                out.insert(keyword.to_string(), Json::String(rewritten));
            }
        }
    }
    if is_meta_schema_uri(out.get("$schema"), META_SCHEMA_URI_OPEN_API_3_1) {
        out.insert("$schema".into(), Json::String(META_SCHEMA_URI_DRAFT_2020_12.into()));
    }
    if let Some(example) = out.get("example").cloned() {
        match out.get("examples") {
            None => {
                out.insert("examples".into(), Json::Array(vec![example]));
                out.shift_remove("example");
            }
            Some(Json::Array(existing)) => {
                let mut examples = vec![example];
                examples.extend(existing.iter().cloned());
                out.insert("examples".into(), Json::Array(examples));
                out.shift_remove("example");
            }
            Some(_) => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------------------------
// OpenAPI 3.0

const PRE_2020_TO_2020_COLLISIONS: [&str; 13] = [
    "$anchor",
    "$defs",
    "$dynamicAnchor",
    "$dynamicRef",
    "$vocabulary",
    "contentSchema",
    "dependentRequired",
    "dependentSchemas",
    "maxContains",
    "minContains",
    "prefixItems",
    "unevaluatedItems",
    "unevaluatedProperties",
];
const OPEN_API_30_EXTRA_COLLISIONS: [&str; 13] = [
    "$comment",
    "$id",
    "$schema",
    "const",
    "contains",
    "contentEncoding",
    "contentMediaType",
    "else",
    "examples",
    "if",
    "patternProperties",
    "propertyNames",
    "then",
];

/// `fromSchemaOpenApi3_0`; `Err` carries the message of the `Error` Effect throws.
pub(crate) fn from_schema_open_api_3_0(schema: &Map<String, Json>) -> Result<Document, String> {
    let root = Json::Object(schema.clone());
    let mut locations = HashSet::new();
    collect_locations_3_0(&root, &mut Vec::new(), &mut locations);
    match convert_3_0(&root, &mut Vec::new(), &locations)? {
        Json::Object(map) => Ok(from_draft_2020_12(map)),
        _ => Ok((Map::new(), Map::new())),
    }
}

// The converter kernel records every visited schema location; with OpenAPI 3.0 source and target paths coincide.
fn collect_locations_3_0(node: &Json, path: &mut Vec<String>, out: &mut HashSet<Vec<String>>) {
    match node {
        Json::Bool(_) => {
            out.insert(path.clone());
        }
        Json::Object(map) => {
            out.insert(path.clone());
            if matches!(map.get("$ref"), Some(Json::String(_))) {
                return;
            }
            for (key, value) in map {
                match key.as_str() {
                    "properties" => {
                        if let Json::Object(properties) = value {
                            for (name, item) in properties {
                                path.push(key.clone());
                                path.push(name.clone());
                                collect_locations_3_0(item, path, out);
                                path.truncate(path.len() - 2);
                            }
                        }
                    }
                    "allOf" | "anyOf" | "oneOf" => {
                        if let Json::Array(items) = value {
                            for (index, item) in items.iter().enumerate() {
                                path.push(key.clone());
                                path.push(index.to_string());
                                collect_locations_3_0(item, path, out);
                                path.truncate(path.len() - 2);
                            }
                        }
                    }
                    "not" | "items" | "additionalProperties" => {
                        path.push(key.clone());
                        collect_locations_3_0(value, path, out);
                        path.pop();
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn is_fragment_encoded(c: char) -> bool {
    (c as u32) < 0x20 || (c as u32) > 0x7E || matches!(c, ' ' | '"' | '<' | '>' | '`')
}

/// The pending-reference fix-up of the converter kernel: a reference into the converted schema is re-pointed at the
/// target location (re-formatted as a URI fragment).
fn relocate_reference(value: &str, locations: &HashSet<Vec<String>>) -> String {
    // Resolved against the converter's root URI, only fragment-only (or empty) references stay in the root resource.
    let trimmed: String =
        value.trim_matches(|c: char| (c as u32) <= 0x20).chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
    if !(trimmed.is_empty() || trimmed.starts_with('#')) {
        return value.to_string();
    }
    let fragment = trimmed.strip_prefix('#').unwrap_or("");
    let mut hash = String::new();
    if !fragment.is_empty() {
        hash.push('#');
        for c in fragment.chars() {
            if is_fragment_encoded(c) {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    hash.push_str(&format!("%{b:02X}"));
                }
            } else {
                hash.push(c);
            }
        }
    }
    let Some(pointer) = parse_uri_fragment(&hash) else { return value.to_string() };
    if !locations.contains(&pointer) {
        return value.to_string();
    }
    match value.find('#') {
        None if pointer.is_empty() => value.to_string(),
        None => format!("{value}{}", format_uri_fragment(&pointer)),
        Some(index) => format!("{}{}", &value[..index], format_uri_fragment(&pointer)),
    }
}

fn convert_3_0(node: &Json, path: &mut Vec<String>, locations: &HashSet<Vec<String>>) -> Result<Json, String> {
    let Json::Object(source) = node else { return Ok(node.clone()) };
    if let Some(Json::String(reference)) = source.get("$ref") {
        let rewritten = rewrite_open_api_components_reference(reference);
        let mut out = Map::new();
        out.insert("$ref".into(), Json::String(relocate_reference(&rewritten, locations)));
        return Ok(Json::Object(out));
    }
    for keyword in PRE_2020_TO_2020_COLLISIONS.iter().chain(OPEN_API_30_EXTRA_COLLISIONS.iter()) {
        if source.contains_key(*keyword) {
            return Err(format!(
                "Cannot convert JSON Schema keyword \"{keyword}\" to Draft 2020-12: it is not active in OpenAPI 3.0 but would become active in the target"
            ));
        }
    }
    let mut out = Map::new();
    for (key, value) in source {
        match key.as_str() {
            "properties" => {
                let converted = match value {
                    Json::Object(properties) => {
                        let mut mapped = Map::new();
                        for (name, item) in properties {
                            path.push(key.clone());
                            path.push(name.clone());
                            let converted = convert_3_0(item, path, locations);
                            path.truncate(path.len() - 2);
                            mapped.insert(name.clone(), converted?);
                        }
                        Json::Object(mapped)
                    }
                    other => other.clone(),
                };
                out.insert(key.clone(), converted);
            }
            "allOf" | "anyOf" | "oneOf" => {
                let converted = match value {
                    Json::Array(items) => {
                        let mut mapped = Vec::with_capacity(items.len());
                        for (index, item) in items.iter().enumerate() {
                            path.push(key.clone());
                            path.push(index.to_string());
                            let converted = convert_3_0(item, path, locations);
                            path.truncate(path.len() - 2);
                            mapped.push(converted?);
                        }
                        Json::Array(mapped)
                    }
                    other => other.clone(),
                };
                out.insert(key.clone(), converted);
            }
            "not" | "items" | "additionalProperties" => {
                path.push(key.clone());
                let converted = convert_3_0(value, path, locations);
                path.pop();
                out.insert(key.clone(), converted?);
            }
            "example" => {
                out.insert("examples".into(), Json::Array(vec![value.clone()]));
            }
            "nullable" | "exclusiveMinimum" | "exclusiveMaximum" => {}
            _ => {
                out.insert(key.clone(), value.clone());
            }
        }
    }
    exclusive_bound(source, &mut out, "minimum", "exclusiveMinimum");
    exclusive_bound(source, &mut out, "maximum", "exclusiveMaximum");
    if source.get("nullable") == Some(&Json::Bool(true))
        && let Some(Json::String(kind)) = source.get("type")
    {
        out.insert("type".into(), Json::Array(vec![Json::String(kind.clone()), Json::String("null".into())]));
    }
    Ok(Json::Object(out))
}

fn exclusive_bound(source: &Map<String, Json>, out: &mut Map<String, Json>, bound: &str, exclusive_key: &str) {
    match source.get(exclusive_key) {
        None => {}
        Some(Json::Bool(exclusive)) => {
            if *exclusive && let Some(value @ Json::Number(_)) = source.get(bound) {
                out.insert(exclusive_key.into(), value.clone());
                out.shift_remove(bound);
            }
        }
        Some(other) => {
            out.insert(exclusive_key.into(), other.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fragments_round_trip() {
        assert_eq!(
            parse_uri_fragment("#/components/schemas/A%20B"),
            Some(vec!["components".into(), "schemas".into(), "A B".into()])
        );
        assert_eq!(parse_uri_fragment("#/a b"), None);
        assert_eq!(format_uri_fragment(&["$defs".into(), "a/b~c#".into()]), "#/$defs/a~1b~0c%23");
        assert_eq!(rewrite_open_api_components_reference("#/components/schemas/Foo"), "#/$defs/Foo");
        assert_eq!(rewrite_open_api_components_reference("#/components/responses/Foo"), "#/components/responses/Foo");
    }

    #[test]
    fn open_api_3_0_conversion() {
        let source = json!({"type": "integer", "nullable": true, "minimum": 1, "exclusiveMinimum": true, "example": 3});
        let (schema, defs) = from_schema_open_api_3_0(source.as_object().unwrap()).unwrap();
        assert!(defs.is_empty());
        assert_eq!(Json::Object(schema), json!({"type": ["integer", "null"], "examples": [3], "exclusiveMinimum": 1}));
        let err = from_schema_open_api_3_0(json!({"const": 1}).as_object().unwrap()).unwrap_err();
        assert_eq!(
            err,
            "Cannot convert JSON Schema keyword \"const\" to Draft 2020-12: it is not active in OpenAPI 3.0 but would become active in the target"
        );
    }

    #[test]
    fn open_api_3_1_conversion() {
        let source = json!({"$ref": "#/components/schemas/X", "example": 1, "$defs": {"L": {"type": "string"}}});
        let (schema, defs) = from_schema_open_api_3_1(source.as_object().unwrap());
        assert_eq!(Json::Object(schema), json!({"$ref": "#/$defs/X", "examples": [1]}));
        assert_eq!(Json::Object(defs), json!({"L": {"type": "string"}}));
    }
}
