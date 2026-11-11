//! Per-tool checks of a `tools/list` result (opencode 2.2.7 `core/src/mcp/client.ts` `tools()`).
//!
//! Upstream's SDK patch leaves the listed tools unchecked (`tools: z.array(z.unknown())`), so one malformed tool
//! doesn't fail the whole list. Each tool is checked on its own and copied, keeping the server's fields except
//! `icons`, each only when it has the right type. A tool that is not an object, has no usable name or no object
//! input schema is skipped with the warning "skipping invalid MCP tool".

use serde_json::{Map, Value};

fn record(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

/// Checks and copies one listed tool; `Err(reason)` skips it.
pub fn check_tool(tool: &Value) -> Result<Map<String, Value>, &'static str> {
    let Some(tool) = tool.as_object() else { return Err("not an object") };
    let name = match tool.get("name") {
        Some(Value::String(name)) if !name.is_empty() => name.clone(),
        _ => return Err("name is not a non-empty string"),
    };
    let Some(input) = record(tool.get("inputSchema")) else { return Err("inputSchema is not an object") };
    if input.get("type").is_some_and(|t| t.as_str() != Some("object")) {
        return Err("inputSchema type is not \"object\"");
    }
    // Providers that check tool parameters against the JSON Schema metaschema reject the whole request over a
    // malformed top level. Nested property schemas are left as sent.
    let mut schema: Map<String, Value> =
        input.iter().filter(|(k, _)| *k != "properties" && *k != "required").map(|(k, v)| (k.clone(), v.clone())).collect();
    schema.insert("type".into(), Value::String("object".into()));
    if let Some(properties) = record(input.get("properties")) {
        schema.insert("properties".into(), Value::Object(properties.clone()));
    }
    if let Some(Value::Array(required)) = input.get("required") {
        if required.iter().all(Value::is_string) {
            schema.insert("required".into(), Value::Array(required.clone()));
        }
    }
    let mut out = Map::new();
    out.insert("name".into(), Value::String(name));
    for key in ["title", "description"] {
        if let Some(value @ Value::String(_)) = tool.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    out.insert("inputSchema".into(), Value::Object(schema));
    for key in ["outputSchema", "annotations", "execution", "_meta"] {
        if let Some(value @ Value::Object(_)) = tool.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    Ok(out)
}

/// Checks every listed tool, logging each skipped one (`{server, tool, reason}`) after the list is built.
pub fn check_tools(server: &str, tools: Vec<Value>) -> Vec<Map<String, Value>> {
    let mut skipped = Vec::new();
    let checked = tools
        .into_iter()
        .filter_map(|tool| match check_tool(&tool) {
            Ok(checked) => Some(checked),
            Err(reason) => {
                skipped.push((tool.as_object().and_then(|t| t.get("name")).cloned(), reason));
                None
            }
        })
        .collect();
    for (tool, reason) in skipped {
        let tool = tool.map(|t| t.to_string()).unwrap_or_else(|| "undefined".into());
        tracing::warn!(server = %server, tool = %tool, reason = %reason, "skipping invalid MCP tool");
    }
    checked
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn skips_tools_that_cannot_be_called() {
        assert_eq!(check_tool(&json!("x")), Err("not an object"));
        assert_eq!(check_tool(&json!({"name": "", "inputSchema": {}})), Err("name is not a non-empty string"));
        assert_eq!(check_tool(&json!({"name": 3, "inputSchema": {}})), Err("name is not a non-empty string"));
        assert_eq!(check_tool(&json!({"name": "a"})), Err("inputSchema is not an object"));
        assert_eq!(check_tool(&json!({"name": "a", "inputSchema": []})), Err("inputSchema is not an object"));
        assert_eq!(check_tool(&json!({"name": "a", "inputSchema": {"type": "string"}})), Err("inputSchema type is not \"object\""));
        assert_eq!(check_tool(&json!({"name": "a", "inputSchema": {"type": null}})), Err("inputSchema type is not \"object\""));
    }

    #[test]
    fn copies_fields_of_the_right_type() {
        let tool = check_tool(&json!({
            "name": "t",
            "title": 5,
            "description": "d",
            "icons": [{"src": "x"}],
            "inputSchema": {"required": ["a", 1], "properties": [], "$schema": "s", "additionalProperties": false},
            "outputSchema": {"type": "string"},
            "annotations": {"readOnlyHint": "yes"},
            "execution": true,
            "_meta": {"k": 1},
        }))
        .unwrap();
        assert_eq!(
            Value::Object(tool.clone()),
            json!({
                "name": "t",
                "description": "d",
                "inputSchema": {"$schema": "s", "additionalProperties": false, "type": "object"},
                "outputSchema": {"type": "string"},
                "annotations": {"readOnlyHint": "yes"},
                "_meta": {"k": 1},
            })
        );
        let keys: Vec<&String> = tool["inputSchema"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["$schema", "additionalProperties", "type"]);
        // `type` keeps its place; properties and required go last.
        let tool = check_tool(&json!({"name": "t", "inputSchema": {"required": ["q"], "type": "object", "properties": {"q": {}}, "x": 1}})).unwrap();
        let keys: Vec<&String> = tool["inputSchema"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["type", "x", "properties", "required"]);
    }

    #[test]
    fn keeps_valid_tools_and_drops_malformed_ones() {
        let tools = check_tools(
            "s",
            vec![
                json!({"name": "good_one", "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}}}),
                json!({"name": "bad_schema", "inputSchema": {"type": "string"}}),
                json!({"name": "bad_annotations", "inputSchema": {"type": "object"}, "annotations": {"readOnlyHint": "yes"}}),
                json!({"name": "good_two", "inputSchema": {}}),
            ],
        );
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["good_one", "bad_annotations", "good_two"]);
        assert_eq!(tools[2]["inputSchema"], json!({"type": "object"}));
    }
}
