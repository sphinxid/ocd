//! Payload validation with Effect Schema's error messages.
//!
//! Upstream decodes request bodies with Effect Schema (`effect@4` `SchemaParser`, errors `"first"`) and
//! formats the issue with `SchemaIssue.makeFormatterDefault` (`"<message>\n  at [\"key\"][0]"`). Serde
//! errors look nothing like that, and serde accepts strings that upstream rejects through brand checks
//! (`Session.ID` must start with `"ses"`). So every JSON body is first checked against the operation's
//! request schema in the embedded `openapi.json` (the encoded side of the Effect schema), reproducing:
//!
//! * `Missing key` for absent required properties, properties checked in declaration order;
//! * `Expected <expected>` for type mismatches, where `<expected>` is the schema identifier (the
//!   component name) or the structural description (`string`, `number`, `object`, `"a" | "b"`,
//!   `string | null`; `SchemaAST.getExpected`);
//! * union candidate selection by runtime type, literal value and discriminator sentinels
//!   (`SchemaAST.getCandidateIndex`): no candidate reports the whole union, one candidate reports that
//!   member's issue, several failing candidates report every member issue (one per line);
//! * check messages (`a string starting with "ses"`, `an integer`, `a value greater than 0`, ...).
//!
//! `{"not": {}}` is `Schema.Never` (an `optionalKey(Never)` property that must be absent): any value fails
//! with `Expected never`. Constructs the document does not express precisely (`allOf`, other `not`s,
//! untyped schemas) are accepted, so the checker never rejects a body upstream would accept.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::LazyLock;

use serde_json::Value;

struct Doc {
    components: serde_json::Map<String, Value>,
    bodies: HashMap<String, Value>,
}

static DOC: LazyLock<Doc> = LazyLock::new(|| {
    let doc: Value = serde_json::from_str(crate::OPENAPI_JSON).expect("embedded openapi.json parses");
    let components = doc.pointer("/components/schemas").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut bodies = HashMap::new();
    if let Some(paths) = doc.get("paths").and_then(Value::as_object) {
        for item in paths.values() {
            for op in item.as_object().into_iter().flat_map(|o| o.values()) {
                let (Some(id), Some(schema)) = (
                    op.get("operationId").and_then(Value::as_str),
                    op.pointer("/requestBody/content/application~1json/schema"),
                ) else {
                    continue;
                };
                bodies.insert(id.to_string(), schema.clone());
            }
        }
    }
    Doc { components, bodies }
});

/// One formatted issue line group (`message` + optional path).
type Issue = Vec<String>;

#[derive(Clone)]
enum Seg {
    Key(String),
    Index(usize),
}

fn render(path: &[Seg]) -> String {
    path.iter()
        .map(|s| match s {
            Seg::Key(k) => format!("[{}]", serde_json::to_string(k).unwrap_or_default()),
            Seg::Index(i) => format!("[{i}]"),
        })
        .collect()
}

fn issue(path: &[Seg], message: String) -> Issue {
    if path.is_empty() { vec![message] } else { vec![format!("{message}\n  at {}", render(path))] }
}

/// Validate `body` against the request schema of `operation`; `Some(message)` describes the first issue.
pub fn payload_issue(operation: &str, body: &Value) -> Option<String> {
    let schema = DOC.bodies.get(operation)?;
    let mut path = Vec::new();
    check(schema, None, body, &mut path).err().map(|lines| lines.join("\n"))
}

/// Validate each of `items` against the component schema `component`, reporting paths as `[key][index]…`.
/// For bodies whose document schema leaves the items untyped because the server upgrades them before
/// decoding (`experimental.session.import` `messages`).
pub fn items_issue(component: &str, key: &str, items: &[Value]) -> Option<String> {
    let schema = serde_json::json!({ "$ref": format!("#/components/schemas/{component}") });
    let mut path = vec![Seg::Key(key.into())];
    for (index, item) in items.iter().enumerate() {
        path.push(Seg::Index(index));
        if let Err(lines) = check(&schema, None, item, &mut path) {
            return Some(lines.join("\n"));
        }
        path.pop();
    }
    None
}

/// Definitions of schemas validated with [`schema_issue`] (`$defs`), by name, kept for the process.
static DEFINITIONS: LazyLock<parking_lot::RwLock<HashMap<String, &'static Value>>> = LazyLock::new(Default::default);

/// Validate `value` against a JSON Schema document of an Effect schema (`Schema.toJsonSchemaDocument`, its
/// definitions under `$defs`); `Some(message)` describes the first issue like Effect's `SchemaError`. Used for
/// the inputs of plugin RPC methods, which are not part of the OpenAPI document.
pub fn schema_issue(schema: &Value, value: &Value) -> Option<String> {
    if let Some(defs) = schema.get("$defs").and_then(Value::as_object) {
        let missing: Vec<(&String, &Value)> = {
            let known = DEFINITIONS.read();
            defs.iter().filter(|(name, _)| !known.contains_key(*name)).collect()
        };
        if !missing.is_empty() {
            let mut known = DEFINITIONS.write();
            for (name, def) in missing {
                known.entry(name.clone()).or_insert_with(|| Box::leak(Box::new(def.clone())));
            }
        }
    }
    let mut path = Vec::new();
    BETWEEN.set(true);
    let issue = check(schema, None, value, &mut path).err().map(|lines| lines.join("\n"));
    BETWEEN.set(false);
    issue
}

thread_local! {
    /// Set while [`schema_issue`] checks: `minimum` with `maximum` is Effect's `isBetween`.
    static BETWEEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn deref(schema: &Value) -> (&Value, Option<&str>) {
    let mut current = schema;
    let mut name = None;
    for _ in 0..16 {
        let Some(r) = current.get("$ref").and_then(Value::as_str) else { break };
        let n = r.rsplit('/').next().unwrap_or(r);
        name.get_or_insert(n);
        let found = if r.starts_with("#/$defs/") { DEFINITIONS.read().get(n).copied() } else { DOC.components.get(n) };
        match found {
            Some(next) => current = next,
            None => break,
        }
    }
    (current, name)
}

fn literals(schema: &Value) -> Option<&Vec<Value>> {
    schema.get("enum").and_then(Value::as_array)
}

/// A single-literal schema (`enum: [x]` or `const: x`).
fn literal(schema: &Value) -> Option<&Value> {
    if let Some(c) = schema.get("const") {
        return Some(c);
    }
    literals(schema).filter(|l| l.len() == 1).map(|l| &l[0])
}

fn same_literal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

fn js(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

fn type_of(schema: &Value) -> Option<&str> {
    schema.get("type").and_then(Value::as_str)
}

fn runtime_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn is_empty_object(schema: &Value) -> bool {
    schema.get("properties").and_then(Value::as_object).is_none_or(|p| p.is_empty())
        && schema.get("additionalProperties").is_none_or(|a| a == &Value::Bool(false))
        && schema.get("patternProperties").is_none()
}

/// `SchemaAST.getExpected` (identifier first).
fn expected(schema: &Value, name: Option<&str>) -> String {
    let (schema, inner) = deref(schema);
    if let Some(n) = name.or(inner) {
        return n.to_string();
    }
    if let Some(members) = schema.get("anyOf").and_then(Value::as_array) {
        let mut seen = Vec::new();
        for m in members {
            let e = expected(m, None);
            if !seen.contains(&e) {
                seen.push(e);
            }
        }
        return seen.join(" | ");
    }
    if let Some(l) = literals(schema) {
        return l.iter().map(js).collect::<Vec<_>>().join(" | ");
    }
    if let Some(c) = schema.get("const") {
        return js(c);
    }
    match type_of(schema) {
        Some("string") => "string".into(),
        Some("number") | Some("integer") => "number".into(),
        Some("boolean") => "boolean".into(),
        Some("null") => "null".into(),
        Some("array") => "array".into(),
        Some("object") if is_empty_object(schema) && schema.get("additionalProperties").is_none() => "object".into(),
        Some("object") => "object".into(),
        _ => "unknown".into(),
    }
}

fn mismatch(path: &[Seg], schema: &Value, name: Option<&str>) -> Issue {
    issue(path, format!("Expected {}", expected(schema, name)))
}

fn check(schema: &Value, name: Option<&str>, v: &Value, path: &mut Vec<Seg>) -> Result<(), Issue> {
    let (schema, inner) = deref(schema);
    let name = name.or(inner);
    if schema.get("not").and_then(Value::as_object).is_some_and(|n| n.is_empty()) {
        return Err(issue(path, "Expected never".into()));
    }
    if let Some(members) = schema.get("anyOf").and_then(Value::as_array) {
        return check_union(schema, name, members, v, path);
    }
    if let Some(l) = literal(schema) {
        return if same_literal(l, v) { Ok(()) } else { Err(mismatch(path, schema, name)) };
    }
    if let Some(ls) = literals(schema) {
        return if ls.iter().any(|l| same_literal(l, v)) { Ok(()) } else { Err(mismatch(path, schema, name)) };
    }
    match type_of(schema) {
        Some("string") => {
            let Some(s) = v.as_str() else { return Err(mismatch(path, schema, name)) };
            if let Some(p) = schema.get("pattern").and_then(Value::as_str) {
                if let Some(prefix) = starts_with_prefix(p) {
                    if !s.starts_with(&prefix) {
                        return Err(issue(path, format!("Expected a string starting with {}", js(&Value::String(prefix)))));
                    }
                } else if let Ok(re) = regex::Regex::new(p)
                    && !re.is_match(s)
                {
                    return Err(issue(path, format!("Expected a string matching the RegExp {p}")));
                }
            }
            let len = s.encode_utf16().count() as u64;
            if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
                && len < min
            {
                return Err(issue(path, format!("Expected a value with a length of at least {min}")));
            }
            if let Some(max) = schema.get("maxLength").and_then(Value::as_u64)
                && len > max
            {
                return Err(issue(path, format!("Expected a value with a length of at most {max}")));
            }
            Ok(())
        }
        Some(t @ ("number" | "integer")) => {
            let Some(n) = v.as_f64() else { return Err(mismatch(path, schema, name)) };
            if t == "integer" && n.fract() != 0.0 {
                return Err(issue(path, "Expected an integer".into()));
            }
            let fmt = crate::support::js_number_string;
            if let Some(m) = schema.get("exclusiveMinimum").and_then(Value::as_f64)
                && n <= m
            {
                return Err(issue(path, format!("Expected a value greater than {}", fmt(m))));
            }
            // A plugin schema's bounded range is one `isBetween` check (the OpenAPI document's pairs are
            // separate checks, such as an Int's safe range).
            if BETWEEN.get()
                && let (Some(min), Some(max)) = (schema.get("minimum").and_then(Value::as_f64), schema.get("maximum").and_then(Value::as_f64))
                && !(n >= min && n <= max)
            {
                return Err(issue(path, format!("Expected a value between {} and {}", fmt(min), fmt(max))));
            }
            if let Some(m) = schema.get("minimum").and_then(Value::as_f64)
                && n < m
            {
                return Err(issue(path, format!("Expected a value greater than or equal to {}", fmt(m))));
            }
            if let Some(m) = schema.get("exclusiveMaximum").and_then(Value::as_f64)
                && n >= m
            {
                return Err(issue(path, format!("Expected a value less than {}", fmt(m))));
            }
            if let Some(m) = schema.get("maximum").and_then(Value::as_f64)
                && n > m
            {
                return Err(issue(path, format!("Expected a value less than or equal to {}", fmt(m))));
            }
            Ok(())
        }
        Some("boolean") => if v.is_boolean() { Ok(()) } else { Err(mismatch(path, schema, name)) },
        Some("null") => if v.is_null() { Ok(()) } else { Err(mismatch(path, schema, name)) },
        Some("array") => {
            let Some(items) = v.as_array() else { return Err(mismatch(path, schema, name)) };
            // `prefixItems` are tuple elements (`Schema.NonEmptyArray` exports `[X, ...X[]]` with
            // minItems): a required element that is absent is a `Missing key` at its index.
            let prefix = schema.get("prefixItems").and_then(Value::as_array);
            let min_items = schema.get("minItems").and_then(Value::as_u64).unwrap_or(0) as usize;
            for (i, element) in prefix.into_iter().flatten().enumerate() {
                path.push(Seg::Index(i));
                let r = match items.get(i) {
                    None if i < min_items => Err(issue(path, "Missing key".into())),
                    None => Ok(()),
                    Some(x) => check(element, None, x, path),
                };
                path.pop();
                r?;
            }
            let skip = prefix.map_or(0, Vec::len);
            if let Some(item) = schema.get("items") {
                for (i, x) in items.iter().enumerate().skip(skip) {
                    path.push(Seg::Index(i));
                    let r = check(item, None, x, path);
                    path.pop();
                    r?;
                }
            }
            if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
                && prefix.is_none()
                && (items.len() as u64) < min
            {
                return Err(issue(path, format!("Expected a value with a length of at least {min}")));
            }
            if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
                && (items.len() as u64) > max
            {
                return Err(issue(path, format!("Expected a value with a length of at most {max}")));
            }
            Ok(())
        }
        Some("object") => {
            let Some(obj) = v.as_object() else { return Err(mismatch(path, schema, name)) };
            let required: BTreeSet<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .map(|r| r.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let props = schema.get("properties").and_then(Value::as_object);
            for (key, prop) in props.into_iter().flatten() {
                match obj.get(key) {
                    None if required.contains(key.as_str()) => {
                        path.push(Seg::Key(key.clone()));
                        let e = issue(path, "Missing key".into());
                        path.pop();
                        return Err(e);
                    }
                    None => {}
                    Some(x) => {
                        path.push(Seg::Key(key.clone()));
                        let r = check(prop, None, x, path);
                        path.pop();
                        r?;
                    }
                }
            }
            if let Some(extra) = schema.get("additionalProperties").filter(|a| a.is_object()) {
                for (key, x) in obj {
                    if props.is_some_and(|p| p.contains_key(key)) {
                        continue;
                    }
                    path.push(Seg::Key(key.clone()));
                    let r = check(extra, None, x, path);
                    path.pop();
                    r?;
                }
            }
            Ok(())
        }
        // Untyped / allOf / other `not`s: accept.
        _ => Ok(()),
    }
}

/// `Schema.isStartingWith` exports `^<escaped prefix>`: recover the prefix when the pattern is exactly that.
fn starts_with_prefix(pattern: &str) -> Option<String> {
    let rest = pattern.strip_prefix('^')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?),
            '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' => return None,
            c => out.push(c),
        }
    }
    Some(out)
}

/// Runtime types a member can accept (`getCandidateTypes`).
fn candidate_types(schema: &Value) -> Vec<&'static str> {
    const ALL: [&str; 6] = ["null", "string", "number", "boolean", "object", "array"];
    let (schema, _) = deref(schema);
    if let Some(members) = schema.get("anyOf").and_then(Value::as_array) {
        let mut out: Vec<&'static str> = Vec::new();
        for m in members {
            for t in candidate_types(m) {
                if !out.contains(&t) {
                    out.push(t);
                }
            }
        }
        return out;
    }
    if let Some(l) = literal(schema) {
        return vec![runtime_type(l)];
    }
    if let Some(ls) = literals(schema) {
        let mut out: Vec<&'static str> = Vec::new();
        for l in ls {
            let t = runtime_type(l);
            if !out.contains(&t) {
                out.push(t);
            }
        }
        return out;
    }
    match type_of(schema) {
        Some("string") => vec!["string"],
        Some("number") | Some("integer") => vec!["number"],
        Some("boolean") => vec!["boolean"],
        Some("null") => vec!["null"],
        Some("array") => vec!["array"],
        Some("object") if is_empty_object(schema) => vec!["string", "number", "boolean", "object", "array"],
        Some("object") => vec!["object"],
        _ => ALL.to_vec(),
    }
}

/// Discriminator sentinels of a member (`collectSentinels`): required single-literal properties.
fn sentinels(schema: &Value) -> Vec<(String, Value)> {
    let (schema, _) = deref(schema);
    if let Some(members) = schema.get("anyOf").and_then(Value::as_array) {
        let all: Vec<Vec<(String, Value)>> = members.iter().map(sentinels).collect();
        let Some(first) = all.first() else { return vec![] };
        return first
            .iter()
            .filter(|(k, l)| all.iter().all(|m| m.iter().any(|(k2, l2)| k2 == k && same_literal(l, l2))))
            .cloned()
            .collect();
    }
    if type_of(schema) != Some("object") {
        return vec![];
    }
    let required: BTreeSet<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    for (key, prop) in schema.get("properties").and_then(Value::as_object).into_iter().flatten() {
        if !required.contains(key.as_str()) {
            continue;
        }
        let (prop, _) = deref(prop);
        if prop.get("anyOf").is_none()
            && let Some(l) = literal(prop)
        {
            out.push((key.clone(), l.clone()));
        }
    }
    out
}

/// `getCandidateIndex(types)(input, false)`.
fn candidates(members: &[Value], v: &Value) -> Vec<usize> {
    let rt = runtime_type(v);
    let mut literal_of: Vec<Option<&Value>> = Vec::with_capacity(members.len());
    let mut only_literals = true;
    // key -> (literal -> members, all members owning the key)
    let mut by_sentinel: BTreeMap<String, (Vec<(Value, BTreeSet<usize>)>, BTreeSet<usize>)> = BTreeMap::new();
    let mut sentinel_count = 0usize;
    let mut otherwise: HashMap<&'static str, Vec<usize>> = HashMap::new();
    let mut has_otherwise = false;
    for (i, m) in members.iter().enumerate() {
        let (resolved, _) = deref(m);
        let lit = if resolved.get("anyOf").is_none() { literal(resolved) } else { None };
        literal_of.push(lit);
        if lit.is_none() {
            only_literals = false;
        }
        let s = sentinels(m);
        if !s.is_empty() {
            sentinel_count += 1;
            for (key, l) in s {
                let entry = by_sentinel.entry(key).or_default();
                entry.1.insert(i);
                match entry.0.iter_mut().find(|(x, _)| same_literal(x, &l)) {
                    Some((_, set)) => {
                        set.insert(i);
                    }
                    None => entry.0.push((l, BTreeSet::from([i]))),
                }
            }
        } else {
            has_otherwise = true;
            for t in candidate_types(m) {
                otherwise.entry(t).or_default().push(i);
            }
        }
    }
    let fallback = |t: &str| otherwise.get(t).cloned().unwrap_or_default();
    if only_literals && !members.is_empty() {
        return (0..members.len()).filter(|&i| literal_of[i].is_some_and(|l| same_literal(l, v))).collect();
    }
    let lookup = |values: &Vec<(Value, BTreeSet<usize>)>, x: &Value| -> Option<BTreeSet<usize>> {
        values.iter().find(|(l, _)| same_literal(l, x)).map(|(_, s)| s.clone())
    };
    if by_sentinel.len() == 1 && !has_otherwise {
        let (key, (values, _)) = by_sentinel.iter().next().unwrap();
        if let Some(obj) = v.as_object()
            && let Some(x) = obj.get(key)
        {
            return lookup(values, x).map(|s| s.into_iter().collect()).unwrap_or_default();
        }
        return vec![];
    }
    if !by_sentinel.is_empty() {
        let Some(obj) = v.as_object() else { return fallback(rt) };
        let common = by_sentinel
            .iter()
            .filter(|(_, (_, all))| all.len() == sentinel_count)
            .max_by_key(|(_, (values, _))| values.len());
        let mut selected: BTreeSet<usize> = fallback(rt).into_iter().collect();
        let mut direct: Option<&String> = None;
        if let Some((key, (values, _))) = common
            && let Some(x) = obj.get(key)
        {
            match lookup(values, x) {
                None => return fallback(rt),
                Some(m) => selected.extend(m),
            }
            direct = Some(key);
        }
        if direct.is_none() {
            for (key, (values, _)) in &by_sentinel {
                if let Some(x) = obj.get(key)
                    && let Some(m) = lookup(values, x)
                {
                    selected.extend(m);
                }
            }
        }
        for (key, (values, all)) in &by_sentinel {
            if Some(key) == direct {
                continue;
            }
            if let Some(x) = obj.get(key) {
                let m = lookup(values, x).unwrap_or_default();
                selected.retain(|i| !all.contains(i) || m.contains(i));
            }
        }
        return selected.into_iter().collect();
    }
    fallback(rt).into_iter().filter(|&i| literal_of[i].is_none_or(|l| same_literal(l, v))).collect()
}

fn check_union(schema: &Value, name: Option<&str>, members: &[Value], v: &Value, path: &mut Vec<Seg>) -> Result<(), Issue> {
    let picked = candidates(members, v);
    match picked.len() {
        0 => Err(mismatch(path, schema, name)),
        1 => check(&members[picked[0]], None, v, path),
        _ => {
            let mut issues = Vec::new();
            for i in picked {
                match check(&members[i], None, v, path) {
                    Ok(()) => return Ok(()),
                    Err(e) => issues.extend(e),
                }
            }
            Err(issues)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(op: &str, body: Value) -> Option<String> {
        payload_issue(op, &body)
    }

    // Expected strings are the upstream server's actual responses (scripts/parity errors_validation).
    #[test]
    fn upstream_messages() {
        assert_eq!(msg("session.prompt", json!({})).unwrap(), "Missing key\n  at [\"text\"]");
        assert_eq!(msg("session.prompt", json!({"text": 5})).unwrap(), "Expected string\n  at [\"text\"]");
        assert_eq!(msg("session.prompt", json!({"text": null})).unwrap(), "Expected string\n  at [\"text\"]");
        assert_eq!(
            msg("session.prompt", json!({"text": "x", "delivery": "later"})).unwrap(),
            "Expected Session.Inbox.Delivery\n  at [\"delivery\"]"
        );
        assert_eq!(
            msg("session.prompt", json!({"text": "x", "files": [{"uri": 5}]})).unwrap(),
            "Expected string\n  at [\"files\"][0][\"uri\"]"
        );
        assert_eq!(msg("session.prompt", json!([])).unwrap(), "Expected object");
        assert_eq!(
            msg("session.prompt", json!({"text": "x", "id": "nope"})).unwrap(),
            "Expected a string starting with \"msg_\"\n  at [\"id\"]"
        );
        assert_eq!(
            msg("session.create", json!({"parentID": "nope"})).unwrap(),
            "Expected a string starting with \"ses\"\n  at [\"parentID\"]"
        );
        assert_eq!(msg("session.create", json!({"title": 1})).unwrap(), "Expected string | null\n  at [\"title\"]");
        assert_eq!(
            msg("session.create", json!({"model": {"id": "m1"}})).unwrap(),
            "Missing key\n  at [\"model\"][\"providerID\"]"
        );
        assert_eq!(
            msg("session.permission.reply", json!({"decision": "maybe"})).unwrap(),
            "Expected Permission.Reply\n  at [\"decision\"]"
        );
        assert_eq!(
            msg("session.fork", json!({"before": 3})).unwrap(),
            "Expected Session.Message.ID | null\n  at [\"before\"]"
        );
        assert_eq!(msg("session.switchModel", json!({"model": "mock/m1"})).unwrap(), "Expected Model.Ref\n  at [\"model\"]");
        assert_eq!(
            msg("session.form.create", json!({"title": "t", "fields": [{"key": "x", "type": "nope"}]})).unwrap(),
            "Expected Form.Field\n  at [\"fields\"][0]"
        );
        assert_eq!(msg("credential.create", json!({"x": 1})).unwrap(), "Missing key\n  at [\"integrationID\"]");
        assert_eq!(
            msg("session.form.create", json!({"title": "Empty", "fields": []})).unwrap(),
            "Missing key\n  at [\"fields\"][0]"
        );
    }

    // Schedule conditions: a `run` check or a Code Mode `code` check, the other member's keys `Never`
    // (strings captured from the 2.2.5 server).
    #[test]
    fn schedule_condition_members() {
        let op = "experimental.schedule.create";
        let body = |c: Value| json!({"sessionID": "ses_x", "prompt": "p", "if": c});
        assert_eq!(msg(op, body(json!({"code": "return true"}))), None);
        assert_eq!(msg(op, body(json!({"run": "x"}))), None);
        assert_eq!(
            msg(op, body(json!({"run": "true", "code": "x"}))).unwrap(),
            "Expected never\n  at [\"if\"][\"code\"]\nExpected never\n  at [\"if\"][\"run\"]"
        );
        assert_eq!(
            msg(op, body(json!({"code": "x", "is": "y"}))).unwrap(),
            "Missing key\n  at [\"if\"][\"run\"]\nExpected never\n  at [\"if\"][\"is\"]"
        );
        assert_eq!(
            msg(op, body(json!({"code": ""}))).unwrap(),
            "Missing key\n  at [\"if\"][\"run\"]\nExpected a value with a length of at least 1\n  at [\"if\"][\"code\"]"
        );
        assert_eq!(
            msg(op, body(json!({"is": "y"}))).unwrap(),
            "Missing key\n  at [\"if\"][\"run\"]\nMissing key\n  at [\"if\"][\"code\"]"
        );
        assert_eq!(
            msg(op, json!({"sessionID": "ses_x", "prompt": "p", "until": {"run": "x", "code": null}})).unwrap(),
            "Expected never\n  at [\"until\"][\"code\"]\nExpected string\n  at [\"until\"][\"code\"]"
        );
    }

    #[test]
    fn valid_bodies_pass() {
        assert_eq!(msg("session.prompt", json!({"text": "hi", "delivery": "queue", "resume": false})), None);
        assert_eq!(msg("session.create", json!({"title": null, "location": {"directory": "/tmp"}})), None);
        assert_eq!(
            msg(
                "session.form.create",
                json!({"title": "t", "fields": [{"key": "x", "type": "string"}, {"key": "n", "type": "integer", "minimum": 1}]})
            ),
            None
        );
        assert_eq!(msg("session.update", json!({"title": "ok", "bogus": 1})), None);
    }

    // The `experimental.browser` RPC's `result` input; expected strings are upstream's answers.
    #[test]
    fn plugin_rpc_inputs() {
        let schema = json!({
            "type": "object",
            "properties": {"sessionID": {"$ref": "#/$defs/Session.ID"}, "outcome": {"$ref": "#/$defs/Test.Outcome"}},
            "required": ["sessionID", "outcome"],
            "$defs": {
                "Session.ID": {"type": "string", "pattern": "^ses"},
                "Test.FileID": {"type": "string", "pattern": "^file_[a-f0-9-]{36}$"},
                "Test.Outcome": {"anyOf": [
                    {"type": "object", "properties": {"type": {"type": "string", "enum": ["success"]}, "result": {"type": "object", "properties": {"files": {"type": "array", "items": {"type": "object", "properties": {"id": {"$ref": "#/$defs/Test.FileID"}}, "required": ["id"]}}}, "required": ["files"]}}, "required": ["type", "result"]},
                    {"type": "object", "properties": {"type": {"type": "string", "enum": ["failure"]}, "code": {"type": "string"}}, "required": ["type", "code"]}
                ]}
            }
        });
        let issue = |v: Value| schema_issue(&schema, &v);
        assert_eq!(issue(json!({"sessionID": 1, "outcome": {}})).unwrap(), "Expected Session.ID\n  at [\"sessionID\"]");
        assert_eq!(issue(json!({"sessionID": "x", "outcome": {}})).unwrap(), "Expected a string starting with \"ses\"\n  at [\"sessionID\"]");
        assert_eq!(issue(json!({"sessionID": "ses", "outcome": {"type": "nope"}})).unwrap(), "Expected Test.Outcome\n  at [\"outcome\"]");
        assert_eq!(issue(json!({"sessionID": "ses", "outcome": {"type": "failure"}})).unwrap(), "Missing key\n  at [\"outcome\"][\"code\"]");
        assert_eq!(
            issue(json!({"sessionID": "ses", "outcome": {"type": "success", "result": {"files": [{"id": "file_1"}]}}})).unwrap(),
            "Expected a string matching the RegExp ^file_[a-f0-9-]{36}$\n  at [\"outcome\"][\"result\"][\"files\"][0][\"id\"]"
        );
        assert_eq!(issue(json!({"sessionID": "ses", "outcome": {"type": "failure", "code": "x"}})), None);
        let port = json!({"type": "object", "properties": {"port": {"type": "integer", "minimum": 1, "maximum": 65535}}, "required": ["port"]});
        assert_eq!(schema_issue(&port, &json!({"port": 0})).unwrap(), "Expected a value between 1 and 65535\n  at [\"port\"]");
    }

    #[test]
    fn prefix_patterns() {
        assert_eq!(starts_with_prefix("^ses").as_deref(), Some("ses"));
        assert_eq!(starts_with_prefix("^msg_").as_deref(), Some("msg_"));
        assert_eq!(starts_with_prefix("^[^#]+$"), None);
    }
}
