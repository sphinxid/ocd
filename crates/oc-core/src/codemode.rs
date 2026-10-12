//! CodeMode exposure as the `execute` tool, its catalog instructions, and the `fetch` extension
//! (`core/src/codemode/{tool,catalog,instructions,web}.ts`). The interpreter lives in `oc-codemode`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use oc_codemode::catalog::{self, Summary};
use oc_codemode::codemode::{CodeMode, ExecutionLimits};
use oc_codemode::execute_tool::{self, ExecuteFile, ProgressRows};
use oc_codemode::host::{CallResult, ExtensionInvocation, Host, HostValue, Json, ToolCallError, ToolInvocation};
use oc_codemode::tool_runtime::{ToolDef, ToolEntry};
use oc_codemode::tool_schema::ToolSchema;
use oc_schema::{Metadata, ToolContent};
use oc_tools::registry::{execute_tool as run_tool, NoHooks};
use oc_tools::tool::{normalized_name, DefinitionContext};
use oc_tools::{Snapshot, Tool, ToolContext, ToolDefinition, ToolError, ToolResult};
use serde_json::{json, Value};

use crate::instructions::{Read, Source};

pub const KEY: &str = "core/codemode";

/// The `execute` tool definition, when CodeMode is not wholly disabled.
pub fn execute_definition(snapshot: &Snapshot) -> Option<ToolDefinition> {
    if !snapshot.execute_enabled {
        return None;
    }
    Some(
        ToolDefinition::new(execute_tool::NAME, execute_tool::DESCRIPTION, execute_tool::input_schema())
            .with_output(Some(output_schema())),
    )
}

/// JSON Schema of `ExecuteOutput`.
fn output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "output": { "type": "string" },
            "toolCalls": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "tool": { "type": "string" },
                        "status": { "type": "string", "enum": ["running", "completed", "error"] },
                        "input": { "type": "object", "additionalProperties": { "$ref": "#/$defs/Json" } }
                    },
                    "required": ["tool", "status"],
                    "additionalProperties": false
                }
            },
            "error": { "type": "boolean", "enum": [true] },
            "files": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "data": { "type": "string" },
                        "mime": { "type": "string" },
                        "name": { "type": "string" }
                    },
                    "required": ["data", "mime"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["output", "toolCalls", "files"],
        "additionalProperties": false
    })
}

/// `qualifiedName(registration)`: `<namespace>.<normalized name>` or the normalized name.
fn qualified_name(tool: &dyn Tool) -> String {
    let normalized = normalized_name(tool.name());
    match tool.options().namespace {
        None => normalized,
        Some(ns) => format!("{ns}.{normalized}"),
    }
}

#[derive(Default)]
struct Node {
    tool: Option<(String, Arc<dyn Tool>)>,
    namespace: Option<String>,
    children: Vec<(String, Node)>,
}

fn node<'a>(root: &'a mut Node, path: &str) -> &'a mut Node {
    let mut current = root;
    for name in path.split('.') {
        let index = match current.children.iter().position(|(n, _)| n == name) {
            Some(i) => i,
            None => {
                current.children.push((name.to_string(), Node::default()));
                current.children.len() - 1
            }
        };
        current = &mut current.children[index].1;
    }
    current
}

/// The CodeMode tool tree of a snapshot (core's `runtime()`): namespaces first, then tools at their qualified paths.
fn entries(snapshot: &Snapshot) -> Vec<ToolEntry> {
    let mut root = Node::default();
    for namespace in &snapshot.namespaces {
        node(&mut root, &namespace.name).namespace = Some(namespace.description.clone());
    }
    for (name, tool) in &snapshot.codemode {
        node(&mut root, &qualified_name(tool.as_ref())).tool = Some((name.clone(), tool.clone()));
    }
    fn walk(node: &Node, path: &mut Vec<String>, out: &mut Vec<ToolEntry>) {
        if let Some(description) = &node.namespace {
            out.push(ToolEntry::Namespace { path: path.join("."), description: Some(description.clone()) });
        }
        if let Some((_, tool)) = &node.tool {
            let definition = tool.definition(&DefinitionContext::default());
            out.push(ToolEntry::Tool(ToolDef {
                path: path.join("."),
                description: definition.description,
                input: ToolSchema::Json(definition.input_schema),
                // Without a declared output the value is the joined text or null (`Schema.NullOr(Schema.String)`).
                output: Some(ToolSchema::Json(
                    definition
                        .output_schema
                        .unwrap_or_else(|| json!({ "anyOf": [{ "type": "string" }, { "type": "null" }] })),
                )),
            }));
        }
        for (name, child) in &node.children {
            path.push(name.clone());
            walk(child, path, out);
            path.pop();
        }
    }
    let mut out = Vec::new();
    walk(&root, &mut Vec::new(), &mut out);
    out
}

fn runtime(snapshot: &Snapshot, host: Arc<dyn Host>) -> CodeMode {
    let extension = oc_codemode::web::extension(Arc::new(oc_codemode::web::ReqwestClient::new()));
    CodeMode::new(entries(snapshot), host, vec![extension], ExecutionLimits::default())
        .expect("CodeMode tool names are validated at registration")
}

/// Host used only to render the catalog (no tool may run).
struct CatalogOnly;

impl Host for CatalogOnly {
    fn call(&self, _path: &str, _input: Option<Json>) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>> {
        Box::pin(async { Err(ToolCallError::failure("Execute context is unavailable")) })
    }
}

/// The catalog inventory of a snapshot (core's `CodeModeTool.catalog`).
pub fn catalog_inventory(snapshot: &Snapshot) -> Vec<catalog::CatalogEntry> {
    let pinned: HashSet<String> = snapshot
        .codemode
        .iter()
        .filter(|(_, tool)| tool.options().pinned)
        .map(|(_, tool)| qualified_name(tool.as_ref()))
        .collect();
    let namespaces: Vec<(String, Option<String>)> =
        snapshot.namespaces.iter().map(|n| (n.name.clone(), Some(n.description.clone()))).collect();
    let descriptions = runtime(snapshot, Arc::new(CatalogOnly)).catalog();
    catalog::inventory(&descriptions, &namespaces, &pinned)
}

/// The summarized catalog advertised for a snapshot, or `None` when `execute` is wholly disabled.
pub fn catalog_summary(snapshot: &Snapshot) -> Option<Summary> {
    if !snapshot.execute_enabled {
        return None;
    }
    Some(catalog::summarize(&catalog_inventory(snapshot), None))
}

/// The `core/codemode` instruction source (`CodeModeInstructions.make`).
pub fn instruction_source(snapshot: &Snapshot) -> Source {
    let read = match catalog_summary(snapshot) {
        Some(summary) => Read::Value(serde_json::to_value(summary).unwrap_or(Value::Null)),
        None => Read::Removed,
    };
    let parse = |v: &Value| serde_json::from_value::<Summary>(v.clone()).ok();
    Source {
        key: KEY.into(),
        read,
        initial: Box::new(move |v| Some(catalog::render(&parse(v)?))),
        changed: Box::new(move |previous, current| Some(catalog::update(&parse(previous)?, &parse(current)?))),
        removed: Box::new(|_| Some(catalog::REMOVED_CATALOG.into())),
    }
}

struct Collected {
    index: usize,
    files: Vec<ExecuteFile>,
}

/// The host bridging programs to the registry's CodeMode tools.
struct ExecuteHost {
    tools: Vec<(String, String, Arc<dyn Tool>)>,
    ctx: ToolContext,
    rows: ProgressRows,
    progress_lock: Mutex<()>,
    calls: Mutex<usize>,
    files: Arc<Mutex<Vec<Collected>>>,
}

impl ExecuteHost {
    fn publish(&self, rows: Option<Vec<execute_tool::ExecuteCall>>) {
        let Some(rows) = rows else { return };
        let _guard = self.progress_lock.lock().unwrap();
        let mut metadata = Metadata::new();
        metadata.insert("toolCalls".into(), serde_json::to_value(rows).unwrap_or(Value::Array(Vec::new())));
        self.ctx.progress(metadata);
    }
}

fn output_files(content: &[ToolContent]) -> Vec<ExecuteFile> {
    content
        .iter()
        .filter_map(|part| match part {
            ToolContent::File { uri, mime, name } => execute_tool::output_file(uri, mime, name.as_deref()),
            _ => None,
        })
        .collect()
}

/// The program-visible value of a nested result: declared output, else the joined text (null when empty).
fn result_value(result: &ToolResult) -> Value {
    if let Some(output) = &result.output {
        return output.clone();
    }
    let text = result.text_content();
    if text.is_empty() { Value::Null } else { Value::String(text) }
}

impl Host for ExecuteHost {
    fn call(&self, path: &str, input: Option<Json>) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>> {
        let found = self.tools.iter().find(|(qualified, _, _)| qualified == path).cloned();
        let index = {
            let mut calls = self.calls.lock().unwrap();
            let index = *calls;
            *calls += 1;
            index
        };
        let mut ctx = self.ctx.clone();
        ctx.call_id = oc_schema::id::ascending("call");
        let files = self.files.clone();
        Box::pin(async move {
            let Some((_, name, tool)) = found else {
                return Err(ToolCallError::failure("Execute context is unavailable"));
            };
            let executed = run_tool(tool.as_ref(), &name, input.unwrap_or(Value::Null), ctx, &NoHooks)
                .await
                .map_err(|e| ToolCallError::Failure(e.model_message()))?;
            let collected = output_files(&executed.content);
            if !collected.is_empty() {
                files.lock().unwrap().push(Collected { index, files: collected });
            }
            Ok(Some(result_value(&executed)))
        })
    }

    fn tool_before(&self, call: &ToolInvocation) -> BoxFuture<'static, Result<(), ToolCallError>> {
        let rows = self.rows.tool_started(call);
        self.publish(Some(rows));
        Box::pin(async { Ok(()) })
    }

    fn tool_after(&self, call: &ToolInvocation, result: &CallResult) {
        self.publish(self.rows.settled(call.id, result));
    }

    fn extension_before(&self, call: &ExtensionInvocation) -> BoxFuture<'static, Result<(), HostValue>> {
        self.publish(self.rows.extension_started(call));
        Box::pin(async { Ok(()) })
    }

    fn extension_after(&self, call: &ExtensionInvocation, result: &CallResult) {
        self.publish(self.rows.settled(call.id, result));
    }
}

/// Runs the `execute` tool: `{ code }` → `{ output, content, metadata }` like upstream `CodeModeTool.create`.
pub async fn execute(snapshot: &Snapshot, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
    let Some(code) = input.get("code").and_then(|c| c.as_str()).map(String::from) else {
        return Err(ToolError::new(
            "The execute tool was called with invalid arguments: expected { code: string }.".to_string(),
        ));
    };
    let files = Arc::new(Mutex::new(Vec::new()));
    let tools = snapshot
        .codemode
        .iter()
        .map(|(name, tool)| (qualified_name(tool.as_ref()), name.clone(), tool.clone()))
        .collect();
    let host = Arc::new(ExecuteHost {
        tools,
        ctx: ctx.clone(),
        rows: ProgressRows::new(),
        progress_lock: Mutex::new(()),
        calls: Mutex::new(0),
        files: files.clone(),
    });
    let runtime = runtime(snapshot, host.clone());
    let cancel = ctx.cancel.clone();
    let result = tokio::select! {
        result = runtime.execute(&code) => result,
        _ = cancel.cancelled() => return Err(ToolError::interrupted()),
    };
    let tool_calls = host.rows.rows();
    let mut collected = std::mem::take(&mut *files.lock().unwrap());
    collected.sort_by_key(|c| c.index);
    let files: Vec<ExecuteFile> = collected.into_iter().flat_map(|c| c.files).collect();
    let value = execute_tool::output(&result, tool_calls, files);
    let mut content = vec![ToolContent::Text { text: value.output.clone() }];
    for file in &value.files {
        content.push(ToolContent::File {
            uri: format!("data:{};base64,{}", file.mime, file.data),
            mime: file.mime.clone(),
            name: file.name.clone(),
        });
    }
    let mut metadata = Metadata::new();
    metadata.insert("toolCalls".into(), serde_json::to_value(&value.tool_calls).unwrap_or(Value::Array(Vec::new())));
    if value.error == Some(true) {
        metadata.insert("error".into(), Value::Bool(true));
    }
    let output = serde_json::to_value(&value).unwrap_or(Value::Null);
    Ok(ToolResult { output: Some(output), content, metadata: Some(metadata) })
}


/// The `execute` tool over a request snapshot, so it runs through the same hooks and normalization as direct tools.
pub struct ExecuteTool(pub Snapshot);

#[async_trait::async_trait]
impl Tool for ExecuteTool {
    fn name(&self) -> &str {
        execute_tool::NAME
    }
    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        execute_definition(&self.0).unwrap_or_else(|| {
            ToolDefinition::new(execute_tool::NAME, execute_tool::DESCRIPTION, execute_tool::input_schema())
        })
    }
    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        execute(&self.0, input, ctx).await
    }
}
