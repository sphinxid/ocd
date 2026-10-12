//! Request compilation (`route/client.ts` prepareRequest, `tool-history.ts`, `effort-updates.ts`,
//! `cache-policy.ts`, `protocols/utils/tool-schema.ts`, `utils/gemini-json-schema.ts`).

use crate::json::Object;
use crate::types::*;
use indexmap::IndexMap;
use serde_json::Value;

// ---------------------------------------------------------------------------------------------
// Tool history normalization
// ---------------------------------------------------------------------------------------------

const EMPTY_TOOL_OUTPUT: &str = "(no tool output)";
const MISSING_TOOL_RESULT: &str = "Tool result missing";

/// `normalizeToolHistory`.
pub fn normalize_tool_history(messages: &[Message]) -> Vec<Message> {
    let mut normalized: Vec<Message> = vec![];
    let mut pending: IndexMap<String, ToolCallPart> = IndexMap::new();
    let mut held: Vec<Message> = vec![];

    fn missing(pending: &IndexMap<String, ToolCallPart>) -> Message {
        Message::new(
            Role::Tool,
            pending
                .values()
                .map(|call| {
                    ContentPart::ToolResult(ToolResultPart {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        namespace: call.namespace.clone(),
                        result: ToolResultValue::error(MISSING_TOOL_RESULT),
                        provider_executed: None,
                        cache: None,
                        metadata: None,
                        provider_metadata: None,
                    })
                })
                .collect(),
        )
    }

    for message in messages {
        if matches!(message.role, Role::User | Role::Assistant) {
            if !pending.is_empty() {
                normalized.push(missing(&pending));
            }
            pending.clear();
            normalized.append(&mut held);
        }
        if message.role == Role::Tool {
            let mut content = vec![];
            for part in &message.content {
                match part {
                    ContentPart::ToolResult(result) if result.provider_executed != Some(true) => {
                        let call = pending.shift_remove(&result.id);
                        content.push(ContentPart::ToolResult(normalize_tool_result(result, call.as_ref())));
                    }
                    other => content.push(other.clone()),
                }
            }
            if !content.is_empty() {
                let mut m = message.clone();
                m.content = content;
                normalized.push(m);
            }
            if pending.is_empty() {
                normalized.append(&mut held);
            }
            continue;
        }
        if message.role == Role::System && !pending.is_empty() {
            held.push(message.clone());
            continue;
        }
        normalized.push(message.clone());
        if message.role != Role::Assistant {
            continue;
        }
        for part in &message.content {
            if let ContentPart::ToolCall(call) = part {
                if call.provider_executed != Some(true) {
                    pending.insert(call.id.clone(), call.clone());
                }
            }
        }
    }
    if !pending.is_empty() {
        normalized.push(missing(&pending));
    }
    normalized.append(&mut held);
    normalized
}

fn normalize_tool_result(part: &ToolResultPart, call: Option<&ToolCallPart>) -> ToolResultPart {
    let mut named = part.clone();
    if let Some(call) = call {
        named.name = call.name.clone();
        named.namespace = call.namespace.clone();
    }
    match &named.result {
        ToolResultValue::Text(Value::String(s)) if s.is_empty() => {
            named.result = ToolResultValue::text(EMPTY_TOOL_OUTPUT);
        }
        ToolResultValue::Error(Value::String(s)) if s.is_empty() => {
            named.result = ToolResultValue::error(EMPTY_TOOL_OUTPUT);
        }
        ToolResultValue::Content(items) => {
            let value: Vec<ToolContent> =
                items.iter().filter(|i| !matches!(i, ToolContent::Text { text } if text.is_empty())).cloned().collect();
            if value.is_empty() {
                named.result = ToolResultValue::text(EMPTY_TOOL_OUTPUT);
            } else {
                named.result = ToolResultValue::Content(value);
            }
        }
        _ => {}
    }
    named
}

// ---------------------------------------------------------------------------------------------
// Effort updates
// ---------------------------------------------------------------------------------------------

/// `effortUpdate(message)`.
pub fn effort_update(message: &Message) -> Option<&EffortPart> {
    if message.role != Role::System || message.content.len() != 1 {
        return None;
    }
    match &message.content[0] {
        ContentPart::Effort(e) => Some(e),
        _ => None,
    }
}

pub fn strip_effort_updates(messages: &[Message]) -> Vec<Message> {
    messages.iter().filter(|m| effort_update(m).is_none()).cloned().collect()
}

/// `resolveEffortUpdates(request, current)` → (messages, effort).
pub fn resolve_effort_updates(request: &LLMRequest, current: Option<String>) -> (Vec<Message>, Option<String>) {
    let updates: Vec<&EffortPart> = request.messages.iter().filter_map(effort_update).collect();
    if updates.is_empty() {
        return (request.messages.clone(), current);
    }
    if updates.last().and_then(|u| u.effort.clone()) != current {
        return (strip_effort_updates(&request.messages), current);
    }
    (request.messages.clone(), updates[0].previous.clone())
}

// ---------------------------------------------------------------------------------------------
// Cache policy
// ---------------------------------------------------------------------------------------------

const RESPECTS_INLINE_HINTS: &[&str] = &[
    "alibaba-chat",
    "alibaba-messages",
    "anthropic-messages",
    "anthropic-compatible-messages",
    "bedrock-mantle-messages",
    "cloudflare-ai-gateway-messages",
    "google-vertex-messages",
    "meta-messages",
    "minimax-messages",
    "moonshot-messages",
    "vercel-ai-gateway-messages",
    "zai-coding-messages",
    "bedrock-converse",
    "openrouter",
    "digitalocean",
];
const BREAKPOINT_CAP: i64 = 4;

fn auto_policy() -> CachePolicyObject {
    CachePolicyObject { tools: Some(true), system: Some(true), messages: Some(CacheMessages::Tail { tail: 1 }), ttl_seconds: None }
}
fn qwen_policy() -> CachePolicyObject {
    CachePolicyObject { tools: None, system: Some(true), messages: Some(CacheMessages::Tail { tail: 1 }), ttl_seconds: None }
}

fn resolve_policy(policy: Option<&CachePolicy>) -> CachePolicyObject {
    match policy {
        None => auto_policy(),
        Some(CachePolicy::Mode(m)) if m == "auto" => auto_policy(),
        Some(CachePolicy::Mode(_)) => CachePolicyObject::default(),
        Some(CachePolicy::Object(o)) => o.clone(),
    }
}

fn gateway_policy(model_id: &str) -> CachePolicyObject {
    let id = model_id.strip_prefix('~').unwrap_or(model_id);
    if id.starts_with("anthropic/") {
        auto_policy()
    } else if id.starts_with("qwen/") || id.starts_with("alibaba/qwen") {
        qwen_policy()
    } else {
        CachePolicyObject::default()
    }
}

fn count_tool_hints(tools: &[ToolEntry]) -> i64 {
    tools
        .iter()
        .map(|t| match t {
            ToolEntry::Tool(d) => d.cache.is_some() as i64,
            ToolEntry::Namespace(n) => count_tool_hints(&n.tools),
        })
        .sum()
}

fn mark_last_tool(tools: &mut [ToolEntry], hint: &CacheHint, budget: &mut i64) {
    let Some(target) = tools.last_mut() else { return };
    match target {
        ToolEntry::Namespace(ns) => mark_last_tool(&mut ns.tools, hint, budget),
        ToolEntry::Tool(def) => {
            if def.cache.is_some() || *budget == 0 {
                return;
            }
            *budget -= 1;
            def.cache = Some(hint.clone());
        }
    }
}

fn mark_message_at(messages: &mut [Message], index: Option<usize>, hint: &CacheHint, budget: &mut i64) {
    let Some(index) = index else { return };
    let Some(target) = messages.get_mut(index) else { return };
    if target.content.is_empty() {
        return;
    }
    let mark_at = target
        .content
        .iter()
        .rposition(|p| matches!(p, ContentPart::Text(_)))
        .unwrap_or(target.content.len() - 1);
    let existing = &target.content[mark_at];
    if existing.cache().is_some() || *budget == 0 {
        return;
    }
    *budget -= 1;
    target.content[mark_at].set_cache(hint.clone());
}

/// `applyCachePolicy`.
pub fn apply_cache_policy(request: &mut LLMRequest) {
    let route = request.model.route.id.clone();
    if !RESPECTS_INLINE_HINTS.contains(&route.as_str()) {
        return;
    }
    let auto_or_unset = request.cache.as_ref().map(|c| c.is_auto()).unwrap_or(true);
    let policy = if (route == "openrouter" || route == "vercel-ai-gateway-messages") && auto_or_unset {
        gateway_policy(&request.model.id)
    } else if route == "alibaba-chat" && auto_or_unset {
        if request.model.id.to_lowercase().starts_with("qwen") { qwen_policy() } else { CachePolicyObject::default() }
    } else {
        resolve_policy(request.cache.as_ref())
    };
    if !policy.tools.unwrap_or(false) && !policy.system.unwrap_or(false) && policy.messages.is_none() {
        return;
    }
    let hint = CacheHint::with_ttl(policy.ttl_seconds);
    let existing = count_tool_hints(&request.tools)
        + request.system.iter().filter(|p| p.cache.is_some()).count() as i64
        + request.messages.iter().flat_map(|m| m.content.iter()).filter(|p| p.cache().is_some()).count() as i64;
    let mut budget = (BREAKPOINT_CAP - existing).max(0);
    if policy.tools.unwrap_or(false) {
        mark_last_tool(&mut request.tools, &hint, &mut budget);
    }
    if policy.system.unwrap_or(false) && !request.system.is_empty() {
        let last = request.system.len() - 1;
        for (index, part) in request.system.iter_mut().enumerate() {
            if (index != 0 && index != last) || part.cache.is_some() || budget == 0 {
                continue;
            }
            budget -= 1;
            part.cache = Some(hint.clone());
        }
    }
    if let Some(strategy) = &policy.messages {
        let messages = &mut request.messages;
        if messages.is_empty() {
            return;
        }
        match strategy {
            CacheMessages::Named(name) if name == "latest-user-message" => {
                let idx = messages.iter().rposition(|m| m.role == Role::User);
                mark_message_at(messages, idx, &hint, &mut budget);
            }
            CacheMessages::Named(name) if name == "latest-assistant" => {
                let idx = messages.iter().rposition(|m| m.role == Role::Assistant);
                mark_message_at(messages, idx, &hint, &mut budget);
            }
            CacheMessages::Named(_) => {}
            CacheMessages::Tail { tail } => {
                let mut start = messages.len();
                let mut remaining = *tail;
                while remaining > 0 && start > 0 {
                    start -= 1;
                    if effort_update(&messages[start]).is_none() {
                        remaining -= 1;
                    }
                }
                for i in start..messages.len() {
                    if effort_update(&messages[i]).is_none() {
                        mark_message_at(messages, Some(i), &hint, &mut budget);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tool schema projection
// ---------------------------------------------------------------------------------------------

fn object_root(schema: &Object) -> Object {
    let type_ = schema.get("type");
    let not_null = schema
        .get("not")
        .and_then(Value::as_object)
        .map(|n| n.len() == 1 && n.get("type").and_then(Value::as_str) == Some("null"))
        .unwrap_or(false);
    let is_object_or_undefined = type_.is_none() || type_.and_then(Value::as_str) == Some("object");
    let with_type_first = |skip: &str| {
        let mut out = Object::new();
        out.insert("type".into(), Value::String("object".into()));
        for (k, v) in schema {
            if k == skip {
                continue;
            }
            if k == "type" {
                // spread later overwrites the first `type` key in place (same position)
                out.insert(k.clone(), v.clone());
                continue;
            }
            out.insert(k.clone(), v.clone());
        }
        out
    };
    if is_object_or_undefined && not_null {
        return with_type_first("not");
    }
    if type_.is_some() {
        return schema.clone();
    }
    let any_of_object = schema
        .get("anyOf")
        .and_then(Value::as_array)
        .map(|branches| {
            branches.iter().any(|b| {
                b.as_object().map(|o| o.len() == 1 && o.get("type").and_then(Value::as_str) == Some("object")).unwrap_or(false)
            })
        })
        .unwrap_or(false);
    if any_of_object {
        return with_type_first("anyOf");
    }
    with_type_first("")
}

fn enum_type(values: &[Value]) -> Option<Value> {
    let mut types: Vec<&str> = vec![];
    for v in values {
        let t = match v {
            Value::Null => "null",
            Value::Array(_) => "array",
            Value::String(_) => "string",
            Value::Number(_) => "number",
            Value::Bool(_) => "boolean",
            Value::Object(_) => "object",
        };
        if !types.contains(&t) {
            types.push(t);
        }
    }
    if types.len() == 1 {
        return Some(Value::String(types[0].into()));
    }
    if types.len() == 2 && types.contains(&"null") {
        let non_null = types.iter().find(|t| **t != "null").unwrap();
        return Some(Value::Array(vec![Value::String((*non_null).into()), Value::String("null".into())]));
    }
    None
}

fn tuple_items_schema(items: &[Value]) -> Value {
    let projected: Vec<Value> = items.iter().map(moonshot_node).collect();
    match projected.len() {
        0 => Value::Object(Object::new()),
        1 => projected.into_iter().next().unwrap(),
        _ => {
            let mut o = Object::new();
            o.insert("anyOf".into(), Value::Array(projected));
            Value::Object(o)
        }
    }
}

fn moonshot_node(schema: &Value) -> Value {
    match schema {
        Value::Array(items) => Value::Array(items.iter().map(moonshot_node).collect()),
        Value::Object(obj) => {
            if let Some(Value::String(r)) = obj.get("$ref") {
                let mut o = Object::new();
                o.insert("$ref".into(), Value::String(r.clone()));
                return Value::Object(o);
            }
            let inferred = if obj.get("type").is_none() {
                obj.get("enum").and_then(Value::as_array).and_then(|e| enum_type(e))
            } else {
                None
            };
            let mut out = Object::new();
            if let Some(t) = inferred {
                out.insert("type".into(), t);
            }
            for (key, value) in obj {
                if key == "items" {
                    if let Value::Array(items) = value {
                        out.insert(key.clone(), tuple_items_schema(items));
                        continue;
                    }
                }
                if key == "prefixItems" {
                    if obj.contains_key("items") {
                        continue;
                    }
                    let items = value.as_array().cloned().unwrap_or_default();
                    out.insert("items".into(), tuple_items_schema(&items));
                    continue;
                }
                if key == "unevaluatedItems" {
                    continue;
                }
                out.insert(key.clone(), moonshot_node(value));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Moonshot/Kimi schema sanitizer.
pub fn moonshot_schema(schema: &Object) -> Object {
    match moonshot_node(&Value::Object(schema.clone())) {
        Value::Object(o) => o,
        _ => Object::new(),
    }
}

const SCHEMA_MAPS: &[&str] = &["properties", "patternProperties", "$defs", "definitions", "dependentSchemas", "dependencies"];
const VALUES: &[&str] = &["const", "default", "enum", "examples", "dependentRequired"];

fn gemini_normalize_node(schema: &Value) -> Value {
    match schema {
        Value::Array(items) => Value::Array(items.iter().map(gemini_normalize_node).collect()),
        Value::Object(obj) => {
            let properties = obj.get("properties").and_then(Value::as_object);
            let mut out = Object::new();
            for (key, value) in obj {
                let k = key.as_str();
                if VALUES.contains(&k) {
                    out.insert(key.clone(), value.clone());
                    continue;
                }
                if SCHEMA_MAPS.contains(&k) {
                    if let Value::Object(map) = value {
                        out.insert(key.clone(), Value::Object(map.iter().map(|(n, v)| (n.clone(), gemini_normalize_node(v))).collect()));
                        continue;
                    }
                }
                if k == "required" {
                    if let (Some(props), Value::Array(names)) = (properties, value) {
                        out.insert(
                            key.clone(),
                            Value::Array(
                                names.iter().filter(|n| n.as_str().map(|s| props.contains_key(s)).unwrap_or(false)).cloned().collect(),
                            ),
                        );
                        continue;
                    }
                }
                if k == "exclusiveMinimum" {
                    if let Value::Bool(b) = value {
                        if *b {
                            if let Some(min) = obj.get("minimum").filter(|m| m.is_number()) {
                                out.insert(key.clone(), min.clone());
                            }
                        }
                        continue;
                    }
                }
                if k == "exclusiveMaximum" {
                    if let Value::Bool(b) = value {
                        if *b {
                            if let Some(max) = obj.get("maximum").filter(|m| m.is_number()) {
                                out.insert(key.clone(), max.clone());
                            }
                        }
                        continue;
                    }
                }
                if k == "minimum" && obj.get("exclusiveMinimum") == Some(&Value::Bool(true)) {
                    continue;
                }
                if k == "maximum" && obj.get("exclusiveMaximum") == Some(&Value::Bool(true)) {
                    continue;
                }
                if k == "items" {
                    if let Value::Array(items) = value {
                        out.insert("prefixItems".into(), Value::Array(items.iter().map(gemini_normalize_node).collect()));
                        continue;
                    }
                }
                if k == "additionalItems" && obj.get("items").map(Value::is_array).unwrap_or(false) {
                    out.insert("items".into(), gemini_normalize_node(value));
                    continue;
                }
                out.insert(key.clone(), gemini_normalize_node(value));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn cut_loops(schema: &Value, target: &str, safe: bool) -> Value {
    match schema {
        Value::Array(items) => Value::Array(items.iter().map(|i| cut_loops(i, target, safe)).collect()),
        Value::Object(obj) => {
            if obj.get("$ref").and_then(Value::as_str) == Some(target) && !safe {
                return Value::Object(Object::new());
            }
            let required: Vec<&str> =
                obj.get("required").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            let min_items_positive = obj
                .get("minItems")
                .map(|v| match v {
                    Value::Number(n) => n.as_f64().map(|f| f > 0.0).unwrap_or(false),
                    Value::String(s) => s.trim().parse::<f64>().map(|f| f > 0.0).unwrap_or(false),
                    Value::Bool(b) => *b,
                    _ => false,
                })
                .unwrap_or(false);
            let mut out = Object::new();
            for (key, value) in obj {
                let k = key.as_str();
                let mapped = if VALUES.contains(&k) {
                    value.clone()
                } else if k == "items" {
                    cut_loops(value, target, safe || !min_items_positive)
                } else if k == "properties" && value.is_object() {
                    Value::Object(
                        value
                            .as_object()
                            .unwrap()
                            .iter()
                            .map(|(name, child)| (name.clone(), cut_loops(child, target, safe || !required.contains(&name.as_str()))))
                            .collect(),
                    )
                } else if SCHEMA_MAPS.contains(&k) && value.is_object() {
                    Value::Object(value.as_object().unwrap().iter().map(|(n, c)| (n.clone(), cut_loops(c, target, safe))).collect())
                } else {
                    cut_loops(value, target, safe)
                };
                out.insert(key.clone(), mapped);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// `GeminiJsonSchema.normalize`.
pub fn gemini_schema(schema: &Object) -> Object {
    let normalized = gemini_normalize_node(&Value::Object(schema.clone()));
    let Value::Object(normalized) = normalized else { return Object::new() };
    let mut mapped = Object::new();
    for (key, value) in &normalized {
        if (key == "$defs" || key == "definitions") && value.is_object() {
            mapped.insert(
                key.clone(),
                Value::Object(
                    value
                        .as_object()
                        .unwrap()
                        .iter()
                        .map(|(name, def)| (name.clone(), cut_loops(def, &format!("#/{key}/{name}"), false)))
                        .collect(),
                ),
            );
        } else {
            mapped.insert(key.clone(), value.clone());
        }
    }
    match cut_loops(&Value::Object(mapped), "#", false) {
        Value::Object(o) => o,
        _ => Object::new(),
    }
}

fn sanitizer_for(request: &LLMRequest) -> Option<String> {
    if let Some(s) = request.model.compatibility.as_ref().and_then(|c| c.sanitizer.clone()) {
        return Some(s);
    }
    if let Some(s) = request.model.route.protocol.sanitizer() {
        return Some(s.into());
    }
    let lower = request.model.id.to_lowercase();
    if lower.contains("gemini") {
        Some("gemini".into())
    } else if lower.contains("kimi") {
        Some("moonshot".into())
    } else {
        None
    }
}

fn project_schema(schema: &Object, sanitizer: Option<&str>) -> Object {
    let root = object_root(schema);
    match sanitizer {
        Some("gemini") => gemini_schema(&root),
        Some("moonshot") => moonshot_schema(&root),
        _ => root,
    }
}

fn project_tools(tools: &[ToolEntry], sanitizer: Option<&str>) -> Vec<ToolEntry> {
    tools
        .iter()
        .map(|t| match t {
            ToolEntry::Tool(def) => {
                let mut def = def.clone();
                def.input_schema = project_schema(&def.input_schema, sanitizer);
                ToolEntry::Tool(def)
            }
            ToolEntry::Namespace(ns) => {
                let mut ns = ns.clone();
                ns.tools = project_tools(&ns.tools, sanitizer);
                ToolEntry::Namespace(ns)
            }
        })
        .collect()
}

/// Dedupe per sibling level by `"<type>:<name>"` (last definition wins, first position).
fn dedupe_tools(tools: &[ToolEntry]) -> Vec<ToolEntry> {
    let mut map: IndexMap<String, ToolEntry> = IndexMap::new();
    for tool in tools {
        map.insert(format!("{}:{}", tool.type_name(), tool.name()), tool.clone());
    }
    map.into_values()
        .map(|t| match t {
            ToolEntry::Namespace(mut ns) => {
                ns.tools = dedupe_tools(&ns.tools);
                ToolEntry::Namespace(ns)
            }
            other => other,
        })
        .collect()
}

/// `resolveRequestOptions`: tool history normalization + defaults merging.
pub fn resolve_request_options(request: &LLMRequest) -> LLMRequest {
    let mut req = request.clone();
    req.messages = normalize_tool_history(&request.messages);
    let route = &req.model.route.defaults;
    let model = req.model.defaults.clone().unwrap_or_default();
    req.generation = Some(
        GenerationOptions::merge(&[route.generation.as_ref(), model.generation.as_ref(), request.generation.as_ref()])
            .unwrap_or_default(),
    );
    req.provider_options = crate::json::merge_records(&[
        route.provider_options.as_ref(),
        model.provider_options.as_ref(),
        request.provider_options.as_ref(),
    ]);
    req.http = HttpOptions::merge(&[route.http.as_ref(), model.http.as_ref(), request.http.as_ref()]);
    req
}

/// `prepareRequest`: full compile pipeline before protocol lowering.
pub fn prepare_request(request: &LLMRequest) -> LLMRequest {
    let mut req = resolve_request_options(request);
    let sanitizer = sanitizer_for(&req);
    req.tools = project_tools(&dedupe_tools(&req.tools), sanitizer.as_deref());
    if !req.model.route.protocol.supports_effort_updates(&req) {
        req.messages = strip_effort_updates(&req.messages);
    }
    apply_cache_policy(&mut req);
    if let Some(headers_fn) = req.model.route.headers.clone() {
        let headers = headers_fn(&req);
        req.http = HttpOptions::merge(&[Some(&HttpOptions::headers(headers)), req.http.as_ref()]);
    }
    req
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_call(id: &str) -> ContentPart {
        ContentPart::ToolCall(ToolCallPart {
            id: id.into(),
            name: "read".into(),
            namespace: None,
            input: json!({}),
            provider_executed: None,
            cache: None,
            metadata: None,
            provider_metadata: None,
        })
    }

    #[test]
    fn missing_tool_results_inserted() {
        let messages = vec![
            Message::user_text("hi"),
            Message::assistant(vec![tool_call("a"), tool_call("b")]),
            Message::system("update"),
            Message::user_text("next"),
        ];
        let out = normalize_tool_history(&messages);
        assert_eq!(out.len(), 5);
        assert_eq!(out[2].role, Role::Tool);
        assert_eq!(out[2].content.len(), 2);
        assert_eq!(out[3].role, Role::System);
        match &out[2].content[0] {
            ContentPart::ToolResult(r) => assert_eq!(r.result, ToolResultValue::error("Tool result missing")),
            _ => panic!(),
        }
    }

    #[test]
    fn object_root_type_first() {
        let schema = json!({"properties": {"a": {"type": "string"}}}).as_object().unwrap().clone();
        let root = object_root(&schema);
        assert_eq!(serde_json::to_string(&root).unwrap(), r#"{"type":"object","properties":{"a":{"type":"string"}}}"#);
        let schema = json!({"type":"object","not":{"type":"null"},"properties":{}}).as_object().unwrap().clone();
        assert_eq!(serde_json::to_string(&object_root(&schema)).unwrap(), r#"{"type":"object","properties":{}}"#);
    }

    #[test]
    fn gemini_schema_normalization() {
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "number", "minimum": 1, "exclusiveMinimum": true}, "t": {"type":"array","items":[{"type":"string"}]}},
            "required": ["a", "missing"]
        });
        let out = gemini_schema(schema.as_object().unwrap());
        assert_eq!(
            Value::Object(out),
            json!({"type":"object","properties":{"a":{"type":"number","exclusiveMinimum":1},"t":{"type":"array","prefixItems":[{"type":"string"}]}},"required":["a"]})
        );
        let recursive = json!({"type":"object","properties":{"child":{"$ref":"#"}},"required":["child"]});
        let out = gemini_schema(recursive.as_object().unwrap());
        assert_eq!(Value::Object(out), json!({"type":"object","properties":{"child":{}},"required":["child"]}));
    }

    #[test]
    fn moonshot_enum_type() {
        let schema = json!({"type":"object","properties":{"mode":{"enum":["a",null]},"t":{"prefixItems":[{"type":"string"}]}}});
        let out = moonshot_schema(schema.as_object().unwrap());
        assert_eq!(
            Value::Object(out),
            json!({"type":"object","properties":{"mode":{"type":["string","null"],"enum":["a",null]},"t":{"items":{"type":"string"}}}})
        );
    }
}
