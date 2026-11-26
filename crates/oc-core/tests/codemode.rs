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
        codemode: false,
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
        stop: Default::default(),
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
        "Run JavaScript in a confined Code Mode runtime to script tool calls and HTTP requests and compose their results.\nWrite plain JavaScript and send it exactly as written: no code fence, and no JSON or other wrapper around it.\n`fetch` is available"
    ));
    // Models that take freeform text send the program as raw text (2.3.0).
    assert_eq!(definition.freeform, Some(oc_schema::Freeform::Text));
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
        json!("{\"order\":{\"id\":\"o1\",\"status\":\"open\"},\"chart\":\"rendered\"}")
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
    // Progress is throttled to one update per 100 ms, and a success drops the pending one (#60120).
    assert_eq!(rows.len(), 1);
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

/// A Code Mode tool whose input is one required non-empty string (`Schema.Struct({ text: Schema.NonEmptyString })`).
struct SendText {
    name: &'static str,
    freeform: Option<oc_schema::Freeform>,
}

#[async_trait]
impl Tool for SendText {
    fn name(&self) -> &str {
        self.name
    }
    fn options(&self) -> ToolOptions {
        ToolOptions { codemode: true, ..ToolOptions::default() }
    }
    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        ToolDefinition::new(
            self.name,
            "Send text",
            json!({ "type": "object", "properties": { "text": { "type": "string", "minLength": 1 } }, "required": ["text"], "additionalProperties": false }),
        )
        .with_freeform(self.freeform.clone())
        .with_output(Some(json!({ "type": "string" })))
    }
    async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let text = input["text"].as_str().unwrap_or_default().to_string();
        Ok(ToolResult::text(text.clone()).with_output(json!(text)))
    }
}

// Upstream tool-execute.test.ts "execute takes raw code as freeform text, and Code Mode calls freeform tools with a
// string" (2.3.0) and codemode.test.ts "a freeform tool takes its one string field directly and still accepts the
// object".
#[tokio::test]
async fn freeform_tools_take_their_string_field_directly() {
    let mut registry = Registry::new();
    registry.add(Arc::new(SendText { name: "echo", freeform: None })).unwrap();
    registry.add(Arc::new(SendText { name: "send", freeform: Some(oc_schema::Freeform::Text) })).unwrap();
    let snapshot = registry.snapshot(&[]);
    assert_eq!(codemode::execute_definition(&snapshot).unwrap().freeform, Some(oc_schema::Freeform::Text));
    let signatures: Vec<String> = codemode::catalog_inventory(&snapshot)
        .into_iter()
        .filter_map(|entry| match entry {
            oc_codemode::catalog::CatalogEntry::Tool(tool) => Some(tool.signature),
            _ => None,
        })
        .collect();
    assert_eq!(
        signatures,
        vec![
            "tools.echo({\n  /** @minLength 1 */\n  text: string,\n}): Promise<string>".to_string(),
            "tools.send(/** @minLength 1 */ text: string): Promise<string>".to_string(),
        ]
    );
    let calls = codemode::execute(
        &snapshot,
        json!({ "code": "return [await tools.send(\"first\"), await tools.send({text: \"second\"})]" }),
        context(Arc::new(Mutex::new(Vec::new()))),
    )
    .await
    .unwrap()
    .output
    .unwrap();
    let output: Value = serde_json::from_str(calls["output"].as_str().unwrap()).unwrap();
    assert_eq!(output, json!(["first", "second"]));
    let inputs: Vec<Value> = calls["toolCalls"].as_array().unwrap().iter().map(|c| c["input"].clone()).collect();
    assert_eq!(inputs, vec![json!({ "text": "first" }), json!({ "text": "second" })]);
    // A tool that is not freeform keeps taking only the object.
    let plain = codemode::execute(&snapshot, json!({ "code": "return await tools.echo(\"a\")" }), context(Arc::new(Mutex::new(Vec::new()))))
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(plain["error"], json!(true));
}

/// A file tool for the diffs tests: `write` and `edit` registered without a namespace (as `full_codemode` registers
/// the real ones), or `notes.write` in a namespace. `edit` reports its diff in `metadata.files` like the real one.
struct FileTool {
    name: &'static str,
    namespace: Option<&'static str>,
}

#[async_trait]
impl Tool for FileTool {
    fn name(&self) -> &str {
        self.name
    }
    fn options(&self) -> ToolOptions {
        ToolOptions { namespace: self.namespace.map(String::from), codemode: true, ..ToolOptions::default() }
    }
    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        let id = oc_tools::tool::effective_name(self.name, self.namespace);
        ToolDefinition::new(id, "Looks like a file tool", json!({ "type": "object", "properties": { "path": { "type": "string" }, "content": { "type": "string" } } }))
    }
    async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let path = input["path"].as_str().unwrap_or_default().to_string();
        if self.name == "edit" {
            let diff = oc_tools::file_diff::file_diff(&path, "a\n", "b\n", "modified");
            let mut metadata = Metadata::new();
            metadata.insert("files".into(), json!([diff.to_json()]));
            return Ok(ToolResult { output: Some(json!(format!("Edited {path}"))), content: vec![], metadata: Some(metadata) });
        }
        let text = format!("Created file successfully: {path}");
        let mut result = ToolResult::text(text.clone()).with_output(json!(text));
        if self.namespace.is_some() {
            let mut metadata = Metadata::new();
            metadata.insert("files".into(), json!([{ "file": path, "status": "added", "additions": 1, "deletions": 0, "patch": "+x" }]));
            result.metadata = Some(metadata);
        }
        Ok(result)
    }
}

/// `codemode/diffs` through `execute` (2.4.1, #65002/#65247): the files a program's write and edit calls changed are
/// listed in `metadata.files`, as they happen in progress and at the end, a failed program keeping them; a namespaced
/// tool named like a file tool records nothing.
#[tokio::test]
async fn execute_lists_the_files_a_program_changed() {
    let mut registry = Registry::new();
    registry.add(Arc::new(FileTool { name: "write", namespace: None })).unwrap();
    registry.add(Arc::new(FileTool { name: "edit", namespace: None })).unwrap();
    registry.add(Arc::new(FileTool { name: "write", namespace: Some("notes") })).unwrap();
    let snapshot = registry.snapshot(&[]);
    let progress = Arc::new(Mutex::new(Vec::new()));
    let code = "await tools.write({ path: \"n.md\", content: \"# N\\nx\\n\" })\nawait tools.edit({ path: \"a.ts\" })\nthrow new Error(\"boom\")";
    let result = codemode::execute(&snapshot, json!({ "code": code }), context(progress.clone())).await.unwrap();
    let metadata = result.metadata.unwrap();
    assert_eq!(metadata["error"], true);
    let files = metadata["files"].as_array().unwrap();
    let summary: Vec<(String, String, Value, Value)> =
        files.iter().map(|f| (f["file"].as_str().unwrap().into(), f["status"].as_str().unwrap().into(), f["additions"].clone(), f["deletions"].clone())).collect();
    assert_eq!(summary, vec![("n.md".into(), "added".into(), json!(2), json!(0)), ("a.ts".into(), "modified".into(), json!(1), json!(1))]);
    assert!(files.iter().all(|f| f["patch"].is_string()));
    // Progress never lists more changes than were made so far (updates within 100 ms of the last are coalesced, and a
    // program that ends drops its pending one: the result carries the final list).
    let counts: Vec<usize> = progress.lock().unwrap().iter().map(|m| m.get("files").and_then(Value::as_array).map_or(0, Vec::len)).collect();
    assert!(counts.windows(2).all(|w| w[0] <= w[1]) && counts.iter().all(|c| *c <= 2), "{counts:?}");

    let quiet = codemode::execute(&snapshot, json!({ "code": "await tools.notes.write({ path: \"n.md\", content: \"x\" })\nreturn []" }), context(Arc::default()))
        .await
        .unwrap();
    assert!(quiet.metadata.unwrap().get("files").is_none());
}

/// `opencode://` fetch (2.4.1, #65725): only `opencode:///api/...` is accepted, and a release build has the OpenCode
/// API off, so a valid one fails with how to enable it; neither asks for permission.
#[tokio::test]
async fn opencode_urls_are_validated_then_refused_in_a_release_build() {
    let snapshot = registry().snapshot(&[]);
    let run = |url: &'static str| {
        let snapshot = snapshot.clone();
        async move {
            let code = format!("try {{ await fetch({}) }} catch (e) {{ return e.name + \": \" + e.message }}", Value::from(url));
            let result = codemode::execute(&snapshot, json!({ "code": code }), context(Arc::default())).await.unwrap();
            result.output.unwrap()["output"].as_str().unwrap().to_string()
        }
    };
    for url in ["opencode://api/session", "opencode:x@evil.com/api/info", "opencode:///openapi.json"] {
        let message = run(url).await;
        assert_eq!(message, format!("TypeError: Invalid opencode URI \"{url}\": use opencode:///api/..., for example \"opencode:///api/session\"."));
    }
    assert_eq!(
        run("opencode:///api/info").await,
        "Error: opencode:// URLs are off. In a local or dev build, enable them with OPENCODE_FLAGS=opencode_api or \"experimental\": { \"opencode_api\": true }."
    );
}

/// The catalog shows a pinned tool's whole description, each further line as a `//` comment (2.4.1, #65493).
#[test]
fn pinned_tools_show_their_full_description() {
    struct Pinned;
    #[async_trait]
    impl Tool for Pinned {
        fn name(&self) -> &str {
            "move"
        }
        fn options(&self) -> ToolOptions {
            ToolOptions { pinned: true, ..ToolOptions::codemode("opencode.session") }
        }
        fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
            ToolDefinition::new("opencode_session_move", "Move a session.\nThe current session moves later.", json!({ "type": "object", "properties": {} }))
        }
        async fn execute(&self, _input: Value, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("moved"))
        }
    }
    let mut registry = Registry::new();
    registry.add(Arc::new(Pinned)).unwrap();
    let summary = codemode::catalog_summary(&registry.snapshot(&[])).unwrap();
    assert_eq!(summary.namespaces[0].entries[0].line, "  - tools.opencode.session.move(): Promise<string | null> // Move a session.\n    // The current session moves later.");
}
