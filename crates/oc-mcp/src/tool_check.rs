//! Per-tool validation of `tools/list` results (opencode 2.2 `core/src/mcp/client.ts`, `ToolCheckingClient`).
//!
//! The SDK (`@modelcontextprotocol/client` 2.3.1) validates a `tools/list` page as a whole, so one malformed tool
//! would fail every tool on the server. Upstream checks the page without its tools first and, when that passes,
//! checks each tool on its own against the negotiated era's `ListToolsResultSchema`, drops the ones it rejects with
//! the warning "skipping invalid MCP tool" and keeps the rest. A page that fails for another reason is left alone.

use serde_json::{Map, Value};

/// One validation issue: the JSON path and zod's `invalid_type`/`invalid_value` style message.
#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub path: Vec<Value>,
    pub message: String,
}

fn kind(value: Option<&Value>) -> &'static str {
    match value {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

struct Checker {
    path: Vec<Value>,
    issues: Vec<Issue>,
}

impl Checker {
    fn at<T>(&mut self, key: impl Into<Value>, f: impl FnOnce(&mut Self) -> T) -> T {
        self.path.push(key.into());
        let out = f(self);
        self.path.pop();
        out
    }

    fn issue(&mut self, message: String) {
        self.issues.push(Issue { path: self.path.clone(), message });
    }

    fn expect(&mut self, expected: &str, value: Option<&Value>) {
        self.issue(format!("Invalid input: expected {expected}, received {}", kind(value)));
    }

    fn string(&mut self, obj: &Map<String, Value>, key: &str, optional: bool) {
        let value = obj.get(key);
        if value.is_none() && optional {
            return;
        }
        if !matches!(value, Some(Value::String(_))) {
            self.at(key, |c| c.expect("string", value));
        }
    }

    fn boolean(&mut self, obj: &Map<String, Value>, key: &str) {
        let value = obj.get(key);
        if value.is_some() && !matches!(value, Some(Value::Bool(_))) {
            self.at(key, |c| c.expect("boolean", value));
        }
    }

    fn one_of(&mut self, obj: &Map<String, Value>, key: &str, values: &[&str]) {
        let Some(value) = obj.get(key) else { return };
        if !value.as_str().is_some_and(|v| values.contains(&v)) {
            let options = values.iter().map(|v| format!("\"{v}\"")).collect::<Vec<_>>().join("|");
            self.at(key, |c| c.issue(format!("Invalid option: expected one of {options}")));
        }
    }

    /// An optional object member; returns it when present and an object.
    fn object<'a>(&mut self, obj: &'a Map<String, Value>, key: &str, optional: bool) -> Option<&'a Map<String, Value>> {
        let value = obj.get(key);
        match value {
            None if optional => None,
            Some(Value::Object(o)) => Some(o),
            _ => {
                self.at(key, |c| c.expect("object", value));
                None
            }
        }
    }

    fn string_array(&mut self, obj: &Map<String, Value>, key: &str) {
        let value = obj.get(key);
        match value {
            None => {}
            Some(Value::Array(items)) => {
                self.at(key, |c| {
                    for (i, item) in items.iter().enumerate() {
                        if !item.is_string() {
                            c.at(i, |c| c.expect("string", Some(item)));
                        }
                    }
                });
            }
            _ => self.at(key, |c| c.expect("array", value)),
        }
    }

    fn tool(&mut self, tool: &Value, modern: bool) {
        let Some(tool) = tool.as_object() else {
            self.expect("object", Some(tool));
            return;
        };
        self.string(tool, "name", false);
        self.string(tool, "title", true);
        match tool.get("icons") {
            None => {}
            Some(Value::Array(icons)) => self.at("icons", |c| {
                for (i, icon) in icons.iter().enumerate() {
                    c.at(i, |c| match icon.as_object() {
                        None => c.expect("object", Some(icon)),
                        Some(icon) => {
                            c.string(icon, "src", false);
                            c.string(icon, "mimeType", true);
                            c.string_array(icon, "sizes");
                            c.one_of(icon, "theme", &["light", "dark"]);
                        }
                    });
                }
            }),
            other => self.at("icons", |c| c.expect("array", other)),
        }
        self.string(tool, "description", true);
        if let Some(input) = self.object(tool, "inputSchema", false) {
            self.at("inputSchema", |c| {
                if input.get("type").and_then(Value::as_str) != Some("object") {
                    c.at("type", |c| c.issue("Invalid input: expected \"object\"".into()));
                }
                if modern {
                    c.string(input, "$schema", true);
                } else {
                    let properties = input.get("properties");
                    if properties.is_some() && !matches!(properties, Some(Value::Object(_))) {
                        c.at("properties", |c| c.expect("record", properties));
                    }
                    c.string_array(input, "required");
                }
            });
        }
        if let Some(output) = self.object(tool, "outputSchema", true) {
            self.at("outputSchema", |c| c.string(output, "$schema", true));
        }
        if let Some(annotations) = self.object(tool, "annotations", true) {
            self.at("annotations", |c| {
                c.string(annotations, "title", true);
                for key in ["readOnlyHint", "destructiveHint", "idempotentHint", "openWorldHint"] {
                    c.boolean(annotations, key);
                }
            });
        }
        if !modern {
            if let Some(execution) = self.object(tool, "execution", true) {
                self.at("execution", |c| c.one_of(execution, "taskSupport", &["required", "optional", "forbidden"]));
            }
        }
        self.object(tool, "_meta", true);
    }

    /// The page without its tools: `PaginatedResult` (legacy) or the 2026-07-28 `ListToolsResult` envelope.
    fn page(&mut self, page: &Map<String, Value>, modern: bool) {
        if modern {
            self.object(page, "_meta", true);
            let ttl = page.get("ttlMs");
            let valid_ttl = ttl.and_then(Value::as_f64).is_some_and(|n| n >= 0.0 && n.fract() == 0.0 && n <= 9007199254740991.0);
            if !valid_ttl {
                self.at("ttlMs", |c| c.expect("number", ttl));
            }
            if !page.get("cacheScope").is_some_and(|s| matches!(s.as_str(), Some("public" | "private"))) {
                self.at("cacheScope", |c| c.issue("Invalid option: expected one of \"public\"|\"private\"".into()));
            }
        } else if let Some(meta) = self.object(page, "_meta", true) {
            self.at("_meta", |c| {
                if let Some(token) = meta.get("progressToken") {
                    let ok = token.is_string() || token.as_f64().is_some_and(|n| n.fract() == 0.0);
                    if !ok {
                        c.at("progressToken", |c| c.issue("Invalid input".into()));
                    }
                }
                if let Some(task) = c.object(meta, "io.modelcontextprotocol/related-task", true) {
                    c.at("io.modelcontextprotocol/related-task", |c| c.string(task, "taskId", false));
                }
            });
        }
        self.string(page, "nextCursor", true);
    }
}

/// zod's `String(error)`: the issues as pretty-printed JSON.
fn render(issues: &[Issue]) -> String {
    let list: Vec<Value> = issues
        .iter()
        .map(|i| serde_json::json!({"path": i.path, "message": i.message}))
        .collect();
    serde_json::to_string_pretty(&list).unwrap_or_default()
}

/// Validate one tool as `tools/list` would (`checkToolsList({...page, tools: [tool]})`); `Err` carries the message.
pub fn check_tool(tool: &Value, modern: bool) -> Result<(), String> {
    let mut c = Checker { path: vec!["tools".into(), 0.into()], issues: vec![] };
    c.tool(tool, modern);
    if c.issues.is_empty() { Ok(()) } else { Err(render(&c.issues)) }
}

/// Whether the page validates with its tools removed (`checkToolsList({...page, tools: []}).ok`).
pub fn page_valid(page: &Map<String, Value>, modern: bool) -> bool {
    let mut c = Checker { path: vec![], issues: vec![] };
    c.page(page, modern);
    c.issues.is_empty()
}

/// Drop the tools the SDK would reject from one `tools/list` page, logging each one.
pub fn filter_page(server: &str, page: &mut Value, modern: bool) {
    let Some(obj) = page.as_object_mut() else { return };
    if !obj.get("tools").is_some_and(Value::is_array) || !page_valid(obj, modern) {
        return;
    }
    let Some(Value::Array(tools)) = obj.get_mut("tools") else { return };
    tools.retain(|tool| match check_tool(tool, modern) {
        Ok(()) => true,
        Err(error) => {
            let name = tool.get("name").map(|n| match n {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            });
            tracing::warn!(server = %server, tool = ?name, error = %error, "skipping invalid MCP tool");
            false
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keeps_valid_tools_and_drops_malformed_ones() {
        let mut page = json!({
            "tools": [
                {"name": "ok", "inputSchema": {"type": "object"}},
                {"name": "bad-schema", "inputSchema": {"type": "string"}},
                {"name": 3, "inputSchema": {"type": "object"}},
                {"name": "bad-annotations", "inputSchema": {"type": "object"}, "annotations": {"readOnlyHint": "yes"}},
                {"name": "no-schema"},
                {"name": "also-ok", "description": "d", "inputSchema": {"type": "object", "properties": {"a": {}}, "required": ["a"]}},
            ]
        });
        filter_page("s", &mut page, false);
        let names: Vec<&str> = page["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["ok", "also-ok"]);
    }

    #[test]
    fn eras_differ_on_execution_and_input_schema_members() {
        let tool = json!({"name": "t", "inputSchema": {"type": "object", "required": "a"}, "execution": {"taskSupport": "never"}});
        assert!(check_tool(&tool, false).is_err());
        assert!(check_tool(&tool, true).is_ok());
    }

    #[test]
    fn leaves_an_invalid_page_to_fail_as_a_whole() {
        let mut page = json!({"nextCursor": 5, "tools": [{"name": 1}]});
        filter_page("s", &mut page, false);
        assert_eq!(page["tools"].as_array().unwrap().len(), 1);
        // The 2026-07-28 envelope requires ttlMs and cacheScope.
        let mut page = json!({"tools": [{"name": 1}]});
        filter_page("s", &mut page, true);
        assert_eq!(page["tools"].as_array().unwrap().len(), 1);
        let mut page = json!({"ttlMs": 0, "cacheScope": "private", "tools": [{"name": 1}, {"name": "a", "inputSchema": {"type": "object"}}]});
        filter_page("s", &mut page, true);
        assert_eq!(page["tools"].as_array().unwrap().len(), 1);
    }
}
