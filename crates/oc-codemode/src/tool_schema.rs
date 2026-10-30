//! JSON Schema → TypeScript signature rendering for the model-visible tool catalog. Port of `tool-schema.ts`.

use std::collections::HashSet;

use serde_json::{Map, Value as Json};

use crate::data::quote;

/// How a tool's input or output is described.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolSchema {
    /// A render-only JSON Schema document (`$defs`/`definitions` inside it are honored).
    Json(Json),
    /// A pre-built schema document with top-level definitions, as Effect's `Schema.toJsonSchemaDocument` produces
    /// (`None` when it could not be built: renders as `unknown`). `empty_document` is the document used for the
    /// empty-input check of an input schema.
    Document { document: Option<(Json, Map<String, Json>)>, empty_document: Option<Json> },
}

impl ToolSchema {
    pub fn json(schema: Json) -> ToolSchema {
        ToolSchema::Json(schema)
    }
}

pub fn is_identifier_segment(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

fn render_literal(value: &Json) -> String {
    crate::data::stringify(value, "")
}

fn render_key(name: &str) -> String {
    if is_identifier_segment(name) { name.to_string() } else { quote(name) }
}

fn parse_uri_fragment(reference: &str) -> Option<Vec<String>> {
    let rest = reference.strip_prefix('#')?;
    let decoded = percent_decode(rest)?;
    if decoded.is_empty() {
        return Some(Vec::new());
    }
    let rest = decoded.strip_prefix('/')?;
    Some(rest.split('/').map(|t| t.replace("~1", "/").replace("~0", "~")).collect())
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn definition_name(reference: &str) -> Option<String> {
    let tokens = parse_uri_fragment(reference)?;
    if tokens.len() == 2 && (tokens[0] == "$defs" || tokens[0] == "definitions") { Some(tokens[1].clone()) } else { None }
}

thread_local! {
    /// Set when rendering hits a construct upstream's renderer throws on (a non-string `$ref`); the whole rendering
    /// then becomes `unknown`, as upstream's try/catch does.
    static RENDER_FAILED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn ref_name(value: &Json) -> Option<String> {
    match value {
        Json::String(r) => definition_name(r),
        _ => {
            RENDER_FAILED.with(|f| f.set(true));
            None
        }
    }
}

fn guarded(f: impl FnOnce() -> String) -> String {
    RENDER_FAILED.with(|flag| flag.set(false));
    let out = f();
    if RENDER_FAILED.with(|flag| flag.replace(false)) { "unknown".into() } else { out }
}

static EMPTY: std::sync::LazyLock<Map<String, Json>> = std::sync::LazyLock::new(Map::new);

fn obj(schema: &Json) -> &Map<String, Json> {
    match schema {
        Json::Object(m) => m,
        _ => &EMPTY,
    }
}

fn field<'a>(schema: &'a Map<String, Json>, key: &str) -> Option<&'a Json> {
    schema.get(key)
}

fn is_truthy(v: Option<&Json>) -> bool {
    match v {
        None | Some(Json::Null) => false,
        Some(Json::Bool(b)) => *b,
        Some(Json::Number(n)) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(false),
        Some(Json::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn type_str<'a>(schema: &'a Map<String, Json>) -> Option<&'a str> {
    schema.get("type").and_then(|t| t.as_str())
}

// Effect encodes an unchecked `number` as `number | "Infinity" | "-Infinity" | "NaN"`.
fn effect_number_sentinel(schema: &Map<String, Json>) -> bool {
    type_str(schema) == Some("string")
        && matches!(schema.get("enum"), Some(Json::Array(values)) if !values.is_empty()
            && values.iter().all(|v| matches!(v.as_str(), Some("NaN" | "Infinity" | "-Infinity"))))
}

const ANNOTATIONS: [&str; 6] = ["title", "description", "default", "examples", "readOnly", "writeOnly"];

/// Effect's `{ not: { type: "null" } }` marker for a struct with no properties: TypeScript's `{}`.
pub fn is_effect_empty_struct(schema: &Json) -> bool {
    let schema = obj(schema);
    let Some(not) = schema.get("not") else { return false };
    let not = obj(not);
    not.get("type").and_then(|t| t.as_str()) == Some("null")
        && not.len() == 1
        && schema.keys().all(|k| k == "not" || ANNOTATIONS.contains(&k.as_str()))
}

fn intersection(members: Vec<String>) -> String {
    let concrete: Vec<String> = members.into_iter().filter(|m| m != "unknown").collect();
    match concrete.len() {
        0 => "unknown".into(),
        1 => concrete.into_iter().next().unwrap(),
        _ => concrete.iter().map(|m| if m.contains(" | ") { format!("({m})") } else { m.clone() }).collect::<Vec<_>>().join(" & "),
    }
}

const MAX_RENDER_DEPTH: usize = 8;

#[derive(Clone)]
struct Ctx {
    definitions: Map<String, Json>,
    pretty: bool,
}

fn has_unresolved_ref(
    schema: &Json,
    definitions: &Map<String, Json>,
    seen: &HashSet<String>,
    visited: &HashSet<usize>,
) -> bool {
    let addr = schema as *const Json as usize;
    if visited.contains(&addr) {
        return false;
    }
    let mut next_visited = visited.clone();
    next_visited.insert(addr);
    let s = obj(schema);
    if let Some(Json::String(r)) = s.get("$ref") {
        match definition_name(r) {
            None => return true,
            Some(name) => {
                let Some(def) = definitions.get(&name) else { return true };
                if seen.contains(&name) {
                    return true;
                }
                let mut seen2 = seen.clone();
                seen2.insert(name);
                if has_unresolved_ref(def, definitions, &seen2, &next_visited) {
                    return true;
                }
            }
        }
    } else if let Some(r) = s.get("$ref")
        && !r.is_null()
    {
        ref_name(r);
        return true;
    }
    let mut children: Vec<&Json> = Vec::new();
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(Json::Array(items)) = s.get(key) {
            children.extend(items.iter());
        }
    }
    if let Some(Json::Object(props)) = s.get("properties") {
        children.extend(props.values());
    }
    if let Some(items) = s.get("items") {
        children.push(items);
    }
    if let Some(ap @ Json::Object(_)) = s.get("additionalProperties") {
        children.push(ap);
    }
    children.into_iter().any(|c| has_unresolved_ref(c, definitions, seen, &next_visited))
}

fn js_number(v: &Json) -> Option<String> {
    v.as_f64().map(crate::number::to_string)
}

fn doc_tags(schema: &Map<String, Json>) -> Vec<String> {
    let mut tags = Vec::new();
    if schema.get("deprecated") == Some(&Json::Bool(true)) {
        tags.push("@deprecated".to_string());
    }
    if let Some(d) = schema.get("default") {
        tags.push(format!("@default {}", render_literal(d)));
    }
    if let Some(Json::String(f)) = schema.get("format") {
        tags.push(format!("@format {f}"));
    }
    if type_str(schema) == Some("integer") {
        tags.push("@integer".into());
    }
    // Effect 4.0.6 bounds every integer by the safe-integer range; those bounds say nothing.
    let integer = type_str(schema) == Some("integer");
    for key in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum", "multipleOf", "minLength", "maxLength"] {
        let bound = schema.get(key).and_then(|v| v.as_f64());
        if integer
            && ((key == "minimum" && bound == Some(-9007199254740991.0))
                || (key == "maximum" && bound == Some(9007199254740991.0)))
        {
            continue;
        }
        if let Some(n) = schema.get(key).and_then(js_number) {
            tags.push(format!("@{key} {n}"));
        }
    }
    if let Some(Json::String(p)) = schema.get("pattern") {
        tags.push(format!("@pattern {p}"));
    }
    for key in ["minItems", "maxItems"] {
        if let Some(n) = schema.get(key).and_then(js_number) {
            tags.push(format!("@{key} {n}"));
        }
    }
    if schema.get("uniqueItems") == Some(&Json::Bool(true)) {
        tags.push("@uniqueItems true".into());
    }
    tags
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn is_js_space(c: char) -> bool {
    (c as u32) < 0x10000 && crate::number::is_js_whitespace(c as u16)
}

fn doc_lines(schema: &Map<String, Json>, width: usize) -> Vec<String> {
    let summary = doc_tags(schema).join(" ");
    let description = schema.get("description").and_then(|d| d.as_str()).unwrap_or("");
    let mut lines: Vec<String> = description.split('\n').map(|l| l.trim_end_matches(is_js_space).to_string()).collect();
    while !lines.is_empty() && lines[0].trim_matches(is_js_space).is_empty() {
        lines.remove(0);
    }
    while !lines.is_empty() && lines[lines.len() - 1].trim_matches(is_js_space).is_empty() {
        lines.pop();
    }
    if lines.len() == 1 {
        let inline = format!("{}{} {}", lines[0], if lines[0].ends_with('.') { "" } else { "." }, summary);
        if !summary.is_empty() && !summary.contains('\n') && width + utf16_len(&inline) + 7 <= 120 {
            return vec![inline];
        }
    }
    let mut out = lines;
    if !summary.is_empty() {
        out.extend(summary.split('\n').map(|s| s.to_string()));
    }
    out
}

fn types_of(schema: &Map<String, Json>) -> Vec<String> {
    match schema.get("type") {
        Some(Json::String(s)) => vec![s.clone()],
        Some(Json::Array(a)) => a.iter().filter_map(|t| t.as_str().map(String::from)).collect(),
        _ => Vec::new(),
    }
}

// Neutralize `*/` so model-provided schema text cannot terminate generated documentation.
fn jsdoc(schema: &Json, pad: &str) -> String {
    let s = obj(schema);
    let mut content = doc_lines(s, pad.len());
    let types = types_of(s);
    let mut append = |label: &str, child: &Json| {
        for (index, line) in doc_lines(obj(child), pad.len() + label.len() + 2).into_iter().enumerate() {
            if index == 0 {
                content.push(format!("{label}: {line}"));
            } else {
                content.push(if line.is_empty() { String::new() } else { format!("  {line}") });
            }
        }
    };
    if types.iter().any(|t| t == "array")
        && let Some(items) = s.get("items")
        && is_truthy(Some(items))
    {
        append("Each item", items);
    }
    if (types.iter().any(|t| t == "object") || is_truthy(s.get("properties")))
        && let Some(ap @ Json::Object(_)) = s.get("additionalProperties")
    {
        let has_props = matches!(s.get("properties"), Some(Json::Object(p)) if !p.is_empty());
        append(if has_props { "Each additional value" } else { "Each value" }, ap);
    }
    if content.is_empty() {
        return String::new();
    }
    let escaped: Vec<String> = content.iter().map(|l| l.replace("*/", "* /")).collect();
    if escaped.len() == 1 && pad.len() + utf16_len(&escaped[0]) + 7 <= 120 {
        return format!("{pad}/** {} */\n", escaped[0]);
    }
    let body: Vec<String> =
        escaped.iter().map(|l| if l.is_empty() { format!("{pad} *") } else { format!("{pad} * {l}") }).collect();
    format!("{pad}/**\n{}\n{pad} */\n", body.join("\n"))
}

fn without(schema: &Map<String, Json>, keys: &[&str]) -> Json {
    let mut m = schema.clone();
    for k in keys {
        m.shift_remove(*k);
    }
    Json::Object(m)
}

fn with_type(schema: &Map<String, Json>, t: Json) -> Json {
    let mut m = schema.clone();
    m.insert("type".into(), t);
    Json::Object(m)
}

/// Entries of a JSON object in JavaScript property order (integer-like keys first).
fn js_entries(map: &Map<String, Json>) -> Vec<(&String, &Json)> {
    let mut ints: Vec<(usize, &String, &Json)> = Vec::new();
    let mut rest: Vec<(&String, &Json)> = Vec::new();
    for (k, v) in map {
        match crate::object::parse_array_index(&crate::value::JsStr::from(k.as_str())) {
            Some(i) => ints.push((i, k, v)),
            None => rest.push((k, v)),
        }
    }
    ints.sort_by_key(|(i, _, _)| *i);
    ints.into_iter().map(|(_, k, v)| (k, v)).chain(rest).collect()
}

fn render(schema: &Json, ctx: &Ctx, depth: usize, seen: &HashSet<String>) -> String {
    if depth > MAX_RENDER_DEPTH {
        return "unknown".into();
    }
    let s = obj(schema);
    let nested_owned;
    let nested = if s.contains_key("definitions") || s.contains_key("$defs") {
        let mut defs = ctx.definitions.clone();
        for key in ["definitions", "$defs"] {
            if let Some(Json::Object(d)) = s.get(key) {
                for (k, v) in d {
                    defs.insert(k.clone(), v.clone());
                }
            }
        }
        nested_owned = Ctx { definitions: defs, pretty: ctx.pretty };
        &nested_owned
    } else {
        ctx
    };
    if is_truthy(s.get("$ref")) {
        let name = s.get("$ref").and_then(ref_name);
        let Some(name) = name else { return "unknown".into() };
        let Some(def) = nested.definitions.get(&name).cloned() else { return "unknown".into() };
        if seen.contains(&name) {
            return "unknown".into();
        }
        let mut seen2 = seen.clone();
        seen2.insert(name);
        return intersection(vec![render(&def, nested, depth, &seen2), render(&without(s, &["$ref"]), nested, depth + 1, seen)]);
    }
    if let Some(c) = s.get("const") {
        return render_literal(c);
    }
    if let Some(e) = s.get("enum")
        && is_truthy(Some(e))
    {
        return match e {
            Json::Array(values) => values.iter().map(render_literal).collect::<Vec<_>>().join(" | "),
            _ => String::new(),
        };
    }
    let alternatives = match s.get("anyOf") {
        Some(Json::Null) | None => s.get("oneOf").filter(|v| !v.is_null()),
        some => some,
    };
    if let Some(alternatives) = alternatives {
        let mut items: Vec<Json> = match alternatives {
            Json::Array(a) => a.clone(),
            _ => Vec::new(),
        };
        if items.iter().any(|i| type_str(obj(i)) == Some("number")) {
            items.retain(|i| !effect_number_sentinel(obj(i)));
        }
        if items.iter().any(|i| type_str(obj(i)) == Some("number"))
            && items.iter().all(|i| type_str(obj(i)) == Some("number") || effect_number_sentinel(obj(i)))
        {
            return "number".into();
        }
        if items.len() == 2
            && type_str(obj(&items[0])) == Some("object")
            && !obj(&items[0]).contains_key("properties")
            && type_str(obj(&items[1])) == Some("array")
            && !obj(&items[1]).contains_key("items")
        {
            return "{}".into();
        }
        let members: Vec<String> = items.iter().map(|i| render(i, nested, depth + 1, seen)).collect();
        if members.iter().any(|m| m == "unknown") {
            return "unknown".into();
        }
        return intersection(vec![members.join(" | "), render(&without(s, &["anyOf", "oneOf"]), nested, depth + 1, seen)]);
    }
    if let Some(all) = s.get("allOf")
        && is_truthy(Some(all))
    {
        let items: Vec<Json> = match all {
            Json::Array(a) => a.clone(),
            _ => Vec::new(),
        };
        if items.iter().any(|i| has_unresolved_ref(i, &nested.definitions, &HashSet::new(), &HashSet::new())) {
            return "unknown".into();
        }
        let mut members = vec![render(&without(s, &["allOf"]), nested, depth + 1, seen)];
        members.extend(items.iter().map(|i| render(i, nested, depth + 1, seen)));
        return intersection(members);
    }
    if let Some(Json::Array(types)) = s.get("type") {
        return types.iter().map(|t| render(&with_type(s, t.clone()), nested, depth + 1, seen)).collect::<Vec<_>>().join(" | ");
    }
    match type_str(s) {
        Some("string") => return "string".into(),
        Some("number" | "integer") => return "number".into(),
        Some("boolean") => return "boolean".into(),
        Some("null") => return "null".into(),
        Some("array") => {
            let empty = Json::Object(Map::new());
            let items = s.get("items").filter(|v| !v.is_null()).unwrap_or(&empty);
            return format!("Array<{}>", render(items, nested, depth + 1, seen));
        }
        _ => {}
    }
    if type_str(s) == Some("object") || is_truthy(s.get("properties")) {
        let required: HashSet<String> = match s.get("required") {
            Some(Json::Array(r)) => r.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            _ => HashSet::new(),
        };
        let empty = Map::new();
        let props = match s.get("properties") {
            Some(Json::Object(p)) => p,
            _ => &empty,
        };
        let properties = js_entries(props);
        let index_type = match s.get("additionalProperties") {
            Some(ap @ Json::Object(_)) => Some(render(ap, nested, depth + 1, seen)),
            _ => None,
        };
        let field = |name: &String, value: &Json| {
            format!(
                "{}{}: {}",
                render_key(name),
                if required.contains(name) { "" } else { "?" },
                render(value, nested, depth + 1, seen)
            )
        };
        if !ctx.pretty {
            let mut fields: Vec<String> = properties.iter().map(|(n, v)| field(n, v)).collect();
            if let Some(t) = &index_type {
                fields.push(format!("[key: string]: {t}"));
            }
            return if fields.is_empty() { "{}".into() } else { format!("{{ {} }}", fields.join("; ")) };
        }
        if properties.is_empty() && index_type.is_none() {
            return "{}".into();
        }
        let pad = "  ".repeat(depth + 1);
        let mut lines: Vec<String> =
            properties.iter().map(|(n, v)| format!("{}{}{},", jsdoc(v, &pad), pad, field(n, v))).collect();
        if let Some(t) = &index_type {
            lines.push(format!("{pad}[key: string]: {t},"));
        }
        return format!("{{\n{}\n{}}}", lines.join("\n"), "  ".repeat(depth));
    }
    if is_effect_empty_struct(schema) {
        return "{}".into();
    }
    "unknown".into()
}

/// `jsonSchemaToTypeScript`
pub fn json_schema_to_typescript(schema: &Json, pretty: bool) -> String {
    guarded(|| render(schema, &Ctx { definitions: Map::new(), pretty }, 0, &HashSet::new()))
}

/// Renders a schema record.
pub fn schema_to_typescript(schema: &ToolSchema, pretty: bool) -> String {
    match schema {
        ToolSchema::Json(s) => json_schema_to_typescript(s, pretty),
        ToolSchema::Document { document: Some((s, defs)), .. } => {
            guarded(|| render(s, &Ctx { definitions: defs.clone(), pretty }, 0, &HashSet::new()))
        }
        ToolSchema::Document { document: None, .. } => "unknown".into(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputProperty {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
}

pub fn input_properties(input: &ToolSchema) -> Vec<InputProperty> {
    let (schema, definitions) = match input {
        ToolSchema::Json(s) => {
            let mut defs = Map::new();
            for key in ["definitions", "$defs"] {
                if let Some(Json::Object(d)) = obj(s).get(key) {
                    for (k, v) in d {
                        defs.insert(k.clone(), v.clone());
                    }
                }
            }
            (s.clone(), defs)
        }
        ToolSchema::Document { document: Some((s, d)), .. } => (s.clone(), d.clone()),
        ToolSchema::Document { document: None, .. } => return Vec::new(),
    };
    let mut schema = schema;
    if let Some(r) = obj(&schema).get("$ref").cloned() {
        if !r.is_string() {
            return Vec::new();
        }
        let resolved = r.as_str().and_then(definition_name).and_then(|n| definitions.get(&n).cloned());
        match resolved {
            Some(s) => schema = s,
            None => return Vec::new(),
        }
    }
    let s = obj(&schema);
    let required: HashSet<String> = match s.get("required") {
        Some(Json::Array(r)) => r.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        _ => HashSet::new(),
    };
    match field(s, "properties") {
        Some(Json::Object(props)) => js_entries(props)
            .into_iter()
            .map(|(name, value)| InputProperty {
                name: name.clone(),
                description: obj(value).get("description").and_then(|d| d.as_str()).map(String::from),
                required: required.contains(name),
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub fn is_empty_input(input: &ToolSchema) -> bool {
    match input {
        ToolSchema::Json(_) => schema_to_typescript(input, false) == "{}",
        ToolSchema::Document { empty_document, .. } => {
            empty_document.as_ref().map(is_effect_empty_struct).unwrap_or(false)
        }
    }
}

pub fn output_typescript(output: Option<&ToolSchema>, pretty: bool) -> String {
    match output {
        None => "void".into(),
        Some(s) => schema_to_typescript(s, pretty),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_objects_and_docs() {
        let schema = json!({
            "type": "object",
            "properties": {
                "server": { "type": "string", "description": "MCP server name as configured. Omit to list every server." },
                "limit": { "type": "integer", "minimum": 1 },
                "tags": { "type": "array", "items": { "type": "string" } },
                "kind": { "enum": ["a", "b"] }
            },
            "required": ["limit"]
        });
        assert_eq!(
            json_schema_to_typescript(&schema, false),
            "{ server?: string; limit: number; tags?: Array<string>; kind?: \"a\" | \"b\" }"
        );
        assert_eq!(
            json_schema_to_typescript(&schema, true),
            "{\n  /** MCP server name as configured. Omit to list every server. */\n  server?: string,\n  /** @integer @minimum 1 */\n  limit: number,\n  tags?: Array<string>,\n  kind?: \"a\" | \"b\",\n}"
        );
    }

    #[test]
    fn refs_and_unions() {
        let schema = json!({
            "$defs": { "Item": { "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"] } },
            "anyOf": [{ "$ref": "#/$defs/Item" }, { "type": "null" }]
        });
        assert_eq!(json_schema_to_typescript(&schema, false), "{ id: string } | null");
        assert_eq!(json_schema_to_typescript(&json!({"$ref": "#/$defs/Missing"}), false), "unknown");
        assert_eq!(json_schema_to_typescript(&json!({"type": ["string", "null"]}), false), "string | null");
    }
}
