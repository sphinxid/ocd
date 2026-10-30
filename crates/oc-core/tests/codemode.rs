//! CodeMode `execute` tool exposure: definition, catalog instructions, and nested tool execution.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use oc_core::codemode;
use oc_core::instructions::Read;
use oc_schema::{Metadata, ToolContent};
use oc_tools::host::{PermissionError, PermissionRequest, ToolHost};
use oc_tools::tool::{DefinitionContext, Namespace, ToolConfig, ToolOptions};
use oc_tools::{Registry, Tool, ToolContext, ToolDefinition, ToolError, ToolResult};
use serde_json::{json, Value};

struct Host;

#[async_trait]
impl ToolHost for Host {
    async fn ask_permission(&self, _request: PermissionRequest) -> Result<(), PermissionError> {
        Ok(())
    }
}

/// `lookup`: a CodeMode tool in namespace `orders` with a declared output.
struct Lookup;

#[async_trait]
impl Tool for Lookup {
    fn name(&self) -> &str {
        "lookup"
    }
    fn options(&self) -> ToolOptions {
        ToolOptions { pinned: true, ..ToolOptions::codemode("orders") }
    }
    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        ToolDefinition::new(
            "orders_lookup",
            "Look up an order by ID",
            json!({ "type": "object", "properties": { "id": { "type": "string", "description": "Order id" } }, "required": ["id"] }),
        )
        .with_output(Some(json!({ "type": "object", "properties": { "id": { "type": "string" }, "status": { "type": "string" } }, "required": ["id", "status"] })))
    }
    async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let id = input["id"].as_str().unwrap_or_default().to_string();
        if id == "missing" {
            return Err(ToolError::new("Order not found: missing"));
        }
        Ok(ToolResult::text("found").with_output(json!({ "id": id, "status": "open" })))
    }
}

/// `chart`: no declared output; returns text and an image file part.
struct Chart;

#[async_trait]
impl Tool for Chart {
    fn name(&self) -> &str {
        "chart"
    }
    fn options(&self) -> ToolOptions {
        ToolOptions::codemode("viz")
    }
    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        ToolDefinition::new("viz_chart", "Render a chart", json!({ "type": "object", "properties": {} }))
    }
    async fn execute(&self, _input: Value, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            output: None,
            content: vec![
                ToolContent::Text { text: "rendered".into() },
                ToolContent::File { uri: "data:text/plain;base64,aGk=".into(), mime: "text/plain".into(), name: Some("c.txt".into()) },
            ],
            metadata: None,
        })
    }
}

fn registry() -> Registry {
    let mut registry = Registry::new();
    registry.add(Arc::new(Lookup)).unwrap();
    registry.add(Arc::new(Chart)).unwrap();
    registry.namespace(Namespace { name: "orders".into(), description: "Purchases and shipments".into() }).unwrap();
    registry
}

fn context(progress: Arc<Mutex<Vec<Metadata>>>) -> ToolContext {
    let dir = std::env::temp_dir();
    ToolContext {
        session_id: "ses_test".into(),
        message_id: "msg_test".into(),
        call_id: "call_execute".into(),
        agent: "build".into(),
        directory: dir.clone(),
        worktree: dir,
        project_id: "prj".into(),
        workspace_id: None,
        global: Arc::new(oc_util::Global::resolve()),
        cancel: tokio_util::sync::CancellationToken::new(),
        progress: Arc::new(move |m| progress.lock().unwrap().push(m)),
        config: Arc::new(ToolConfig::default()),
        host: Arc::new(Host),
    }
}

#[test]
fn definition_is_verbatim_and_gated() {
    let snapshot = registry().snapshot(&[]);
    let definition = codemode::execute_definition(&snapshot).unwrap();
    assert_eq!(definition.name, "execute");
    assert!(definition.description.starts_with(
        "Run JavaScript in a confined Code Mode runtime to script tool calls and HTTP requests and compose their results.\n`fetch` is available"
    ));
    assert_eq!(
        definition.input_schema,
        json!({"type":"object","properties":{"code":{"type":"string"}},"required":["code"],"additionalProperties":false})
    );
    let disabled = registry().snapshot(&[oc_schema::PermissionRule {
        action: "execute".into(),
        resource: "*".into(),
        effect: oc_schema::Effect::Deny,
    }]);
    assert!(codemode::execute_definition(&disabled).is_none());
    assert!(matches!(codemode::instruction_source(&disabled).read, Read::Removed));
}

#[test]
fn instruction_source_renders_the_catalog() {
    let snapshot = registry().snapshot(&[]);
    let source = codemode::instruction_source(&snapshot);
    assert_eq!(source.key, "core/codemode");
    let Read::Value(summary) = &source.read else { panic!("expected a catalog") };
    let text = (source.initial)(summary).unwrap();
    assert!(text.starts_with("# Code Mode\n\nThe tools listed below cannot be called directly: they only work as JavaScript inside the `execute` tool."));
    assert!(text.contains("The catalog is complete. Do not guess tool names."));
    assert!(text.contains("- orders (1 tool) // Purchases and shipments"));
    assert!(text.contains(
        "  - tools.orders.lookup({\n  /** Order id */\n  id: string,\n}): Promise<{\n  id: string,\n  status: string,\n}> // Look up an order by ID"
    ));
    assert!(text.contains("- viz (1 tool)\n  - tools.viz.chart(): Promise<string | null> // Render a chart"));
    assert_eq!((source.removed)(summary).unwrap(), oc_codemode::catalog::REMOVED_CATALOG);
}

/// `bash`: a direct tool (kept on the provider's native list).
struct Bash;

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        "bash"
    }
    fn options(&self) -> ToolOptions {
        ToolOptions::direct()
    }
    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        ToolDefinition::new("bash", "Run a command", json!({ "type": "object", "properties": {} }))
    }
    async fn execute(&self, _input: Value, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::text("ran"))
    }
}

// Upstream tool-registry.test.ts "exposes Code Mode tools directly when execute is denied".
#[tokio::test]
async fn codemode_tools_are_direct_when_execute_is_denied() {
    let mut registry = registry();
    registry.add(Arc::new(Bash)).unwrap();
    let deny = |action: &str| oc_schema::PermissionRule {
        action: action.into(),
        resource: "*".into(),
        effect: oc_schema::Effect::Deny,
    };
    let snapshot = registry.snapshot(&[deny("execute")]);
    let names: Vec<String> =
        snapshot.definitions(&DefinitionContext::default(), codemode::execute_definition(&snapshot)).into_iter().map(|d| d.name).collect();
    assert_eq!(names, vec!["bash", "orders_lookup", "viz_chart"]);
    assert!(matches!(codemode::instruction_source(&snapshot).read, Read::Removed));
    let progress = Arc::new(Mutex::new(Vec::new()));
    let result = snapshot
        .execute("orders_lookup", json!({ "id": "o1" }), context(progress.clone()), None, &oc_tools::registry::NoHooks)
        .await
        .unwrap();
    assert_eq!(result.output, Some(json!({ "id": "o1", "status": "open" })));

    let hidden = registry.snapshot(&[deny("execute"), deny("orders_lookup")]);
    let names: Vec<String> = hidden.registered_definitions().into_iter().map(|d| d.name).collect();
    assert_eq!(names, vec!["bash", "viz_chart"]);
    assert!(hidden.execute("orders_lookup", json!({ "id": "o1" }), context(progress), None, &oc_tools::registry::NoHooks).await.is_err());
}

#[tokio::test]
async fn execute_runs_nested_tools_with_progress_and_files() {
    let snapshot = registry().snapshot(&[]);
    let progress = Arc::new(Mutex::new(Vec::new()));
    let result = codemode::execute(
        &snapshot,
        json!({ "code": "const order = await tools.orders.lookup({ id: 'o1' }); const chart = await tools.viz.chart(); return { order, chart }" }),
        context(progress.clone()),
    )
    .await
    .unwrap();
    let output = result.output.unwrap();
    assert_eq!(
        output["output"],
        json!("{\n  \"order\": {\n    \"id\": \"o1\",\n    \"status\": \"open\"\n  },\n  \"chart\": \"rendered\"\n}")
    );
    assert_eq!(
        output["toolCalls"],
        json!([
            { "tool": "orders.lookup", "status": "completed", "input": { "id": "o1" } },
            { "tool": "viz.chart", "status": "completed" }
        ])
    );
    assert_eq!(output["files"], json!([{ "data": "aGk=", "mime": "text/plain", "name": "c.txt" }]));
    assert!(output.get("error").is_none());
    assert_eq!(result.content.len(), 2);
    assert_eq!(
        result.content[1],
        ToolContent::File { uri: "data:text/plain;base64,aGk=".into(), mime: "text/plain".into(), name: Some("c.txt".into()) }
    );
    let metadata = result.metadata.unwrap();
    assert_eq!(metadata["toolCalls"], output["toolCalls"]);
    let rows: Vec<Value> = progress.lock().unwrap().iter().map(|m| m["toolCalls"].clone()).collect();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0], json!([{ "tool": "orders.lookup", "status": "running", "input": { "id": "o1" } }]));
}

#[tokio::test]
async fn failures_are_results_not_tool_errors() {
    let snapshot = registry().snapshot(&[]);
    let progress = Arc::new(Mutex::new(Vec::new()));
    let result = codemode::execute(
        &snapshot,
        json!({ "code": "console.log('checking'); return await tools.orders.lookup({ id: 'missing' })" }),
        context(progress),
    )
    .await
    .unwrap();
    let output = result.output.unwrap();
    assert_eq!(output["output"], json!("Order not found: missing\n\nLogs:\nchecking"));
    assert_eq!(output["error"], json!(true));
    assert_eq!(output["toolCalls"][0]["status"], json!("error"));
    assert_eq!(result.metadata.unwrap()["error"], json!(true));

    let unknown = codemode::execute(&snapshot, json!({ "code": "return tools.orders.look({})" }), context(Arc::new(Mutex::new(Vec::new()))))
        .await
        .unwrap();
    assert_eq!(
        unknown.output.unwrap()["output"],
        json!("Unknown tool 'orders.look'. Did you mean tools.orders.lookup?\nUse search to find available tools.")
    );
}
