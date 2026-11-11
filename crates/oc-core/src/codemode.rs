//! CodeMode exposure as the `execute` tool, its catalog instructions, and the `fetch` extension
//! (`core/src/codemode/{tool,catalog,instructions,fetch}.ts`). The interpreter lives in `oc-codemode`.

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
use oc_tools::registry::{execute_tool as run_tool, ExecuteHooks, NoHooks};
use oc_tools::tool::{normalized_name, DefinitionContext};
use oc_tools::host::PermissionError;
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
pub(crate) fn qualified_name(tool: &dyn Tool) -> String {
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

fn runtime(
    snapshot: &Snapshot,
    host: Arc<dyn Host>,
    checks: Option<(oc_codemode::web::Authorize, oc_codemode::web::CheckRedirect)>,
) -> CodeMode {
    let (authorize, redirect) = checks.unzip();
    let client = Arc::new(oc_codemode::web::ReqwestClient::manual());
    let extension = oc_codemode::web::extension_checked(client, authorize, redirect);
    CodeMode::new(entries(snapshot), host, vec![extension], ExecutionLimits::default())
        .expect("CodeMode tool names are validated at registration")
}

/// `codemode/fetch.ts`: the same rules as the webfetch tool, so a URL denied there is denied here. A correction
/// reaches the program as the error's message; a decline ends the execution like a declined tool call.
fn fetch_authorize(ctx: ToolContext, stop: Stopper) -> oc_codemode::web::Authorize {
    Arc::new(move |url: String, args: Vec<HostValue>| {
        let ctx = ctx.clone();
        let stop = stop.clone();
        Box::pin(async move {
            let metadata: Metadata = execute_tool::fetch_display(&args).into_iter().collect();
            let request = ctx.permission("webfetch", vec![url.clone()], Some(vec!["*".into()]), Some(metadata));
            match ctx.host.ask_permission(request).await {
                Ok(()) => Ok(()),
                Err(PermissionError::Corrected { feedback }) => {
                    Err(HostValue::error("Error", format!("The user rejected fetching {url}: {feedback}")))
                }
                Err(PermissionError::Blocked { message }) | Err(PermissionError::Other { message }) => Err(HostValue::error("Error", message)),
                Err(defect) => {
                    stop.stop(defect.into_tool_error(""));
                    Err(HostValue::error("Error", ""))
                }
            }
        }) as BoxFuture<'static, Result<(), HostValue>>
    })
}

/// Redirects are followed by `fetch` so each new URL meets the same deny rules as the webfetch tool's hops
/// (`assertNotDenied`, which never asks), with the original call's method and the URL it redirected from.
fn fetch_redirect_check(ctx: ToolContext) -> oc_codemode::web::CheckRedirect {
    Arc::new(move |next: String, args: Vec<HostValue>, from: String| {
        let ctx = ctx.clone();
        Box::pin(async move {
            let mut call = args.clone();
            match call.first_mut() {
                Some(first) => *first = HostValue::String(next.clone()),
                None => call.push(HostValue::String(next.clone())),
            }
            let mut metadata: Metadata = execute_tool::fetch_display(&call).into_iter().collect();
            metadata.insert("redirectedFrom".into(), Value::String(from));
            let request = ctx.permission("webfetch", vec![next], None, Some(metadata));
            ctx.host.check_permission(request).await.map_err(|error| match error {
                PermissionError::Blocked { message } | PermissionError::Other { message } => HostValue::error("Error", message),
                PermissionError::Corrected { feedback } => HostValue::error("Error", feedback),
                other => HostValue::error("Error", other.into_tool_error("").model_message()),
            })
        }) as BoxFuture<'static, Result<(), HostValue>>
    })
}

/// Ends an execution with a defect (a declined permission) the way upstream's `Effect.die` does: the program
/// stops and the `execute` call fails with the defect instead of a result.
#[derive(Clone, Default)]
struct Stopper {
    defect: Arc<Mutex<Option<ToolError>>>,
    token: tokio_util::sync::CancellationToken,
}

impl Stopper {
    fn stop(&self, error: ToolError) {
        self.defect.lock().unwrap().get_or_insert(error);
        self.token.cancel();
    }
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
    let descriptions = runtime(snapshot, Arc::new(CatalogOnly), None).catalog();
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
    let inventory = snapshot.execute_enabled.then(|| catalog_inventory(snapshot));
    let read = match &inventory {
        Some(inventory) => Read::Value(serde_json::to_value(catalog::summarize(inventory, None)).unwrap_or(Value::Null)),
        None => Read::Removed,
    };
    // Rendered once, against the inventory that produced `current`, and stored with the update.
    let lookup: Box<dyn Fn(&str) -> Option<catalog::Listing> + Send + Sync> = match &inventory {
        Some(inventory) => Box::new(catalog::lookup(inventory)),
        None => Box::new(|_| None),
    };
    let parse = |v: &Value| serde_json::from_value::<Summary>(v.clone()).ok();
    Source {
        key: KEY.into(),
        read,
        initial: Box::new(move |v| Some(catalog::render(&parse(v)?))),
        // A previous value that no longer decodes (a 2.2.5 summary) restates the whole catalog; an update that names
        // nothing new is still committed, without a message.
        changed: Box::new(move |previous, current| {
            let after = parse(current)?;
            match parse(previous) {
                None => Some(catalog::render(&after)),
                Some(before) => catalog::update(&before, &after, &lookup),
            }
        }),
        removed: Box::new(|_| Some(catalog::REMOVED_CATALOG.into())),
    }
}

struct Collected {
    index: usize,
    files: Vec<ExecuteFile>,
}

/// Execute progress throttling: each update replaces the last, so at most one is sent per 100 ms, and a trailing
/// one carries the latest rows (`codemode/tool.ts` `record`/`publish`).
const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Default)]
struct Throttle {
    published_at: Option<tokio::time::Instant>,
    /// The pending trailing update.
    trailing: Option<tokio::task::JoinHandle<()>>,
}

/// The host bridging programs to the registry's CodeMode tools.
struct ExecuteHost {
    tools: Vec<(String, String, Arc<dyn Tool>)>,
    ctx: ToolContext,
    /// `tool.execute.before/after` for nested calls (`beforeExecute` → `executeTool`).
    hooks: Arc<dyn ExecuteHooks>,
    rows: ProgressRows,
    throttle: Mutex<Throttle>,
    calls: Mutex<usize>,
    files: Arc<Mutex<Vec<Collected>>>,
    stop: Stopper,
    this: std::sync::Weak<ExecuteHost>,
}

impl ExecuteHost {
    /// Sends the current rows (the caller holds the throttle lock).
    fn publish_locked(&self, throttle: &mut Throttle) {
        throttle.trailing = None;
        throttle.published_at = Some(tokio::time::Instant::now());
        let mut metadata = Metadata::new();
        metadata.insert("toolCalls".into(), serde_json::to_value(self.rows.rows()).unwrap_or(Value::Array(Vec::new())));
        self.ctx.progress(metadata);
    }

    /// `record`: the rows already changed; publish now, or schedule the trailing update unless one is pending.
    fn record(&self, changed: bool) {
        if !changed {
            return;
        }
        let mut throttle = self.throttle.lock().unwrap();
        if throttle.trailing.is_some() {
            return;
        }
        let now = tokio::time::Instant::now();
        let due = throttle.published_at.map(|at| at + PROGRESS_INTERVAL).filter(|due| *due > now);
        let Some(due) = due else { return self.publish_locked(&mut throttle) };
        let this = self.this.clone();
        throttle.trailing = Some(tokio::spawn(async move {
            tokio::time::sleep_until(due).await;
            if let Some(host) = this.upgrade() {
                let mut throttle = host.throttle.lock().unwrap();
                if throttle.trailing.is_some() {
                    host.publish_locked(&mut throttle);
                }
            }
        }));
    }

    /// A success drops the pending update, since the result carries the final rows.
    fn drop_trailing(&self) {
        if let Some(handle) = self.throttle.lock().unwrap().trailing.take() {
            handle.abort();
        }
    }

    /// The execution ends without a result: running calls settle as errors, and the pending rows are sent.
    fn interrupt(&self) {
        self.record(self.rows.interrupt_running());
        self.flush_trailing();
    }

    /// A failed or interrupted execution keeps its last progress, so pending rows are sent before it settles.
    fn flush_trailing(&self) {
        let mut throttle = self.throttle.lock().unwrap();
        if let Some(handle) = throttle.trailing.take() {
            handle.abort();
            self.publish_locked(&mut throttle);
        }
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
        // Progress replaces the execute call's own, so an inner tool's updates would hide the call rows (#60480).
        ctx.progress = Arc::new(|_| {});
        let files = self.files.clone();
        let hooks = self.hooks.clone();
        let stop = self.stop.clone();
        Box::pin(async move {
            let Some((_, name, tool)) = found else {
                return Err(ToolCallError::failure("Execute context is unavailable"));
            };
            // The program sees the same failure text the model gets for a direct call (#60109); a defect such as
            // a declined permission ends the execution instead.
            let fail = |e: ToolError| {
                if e.is_defect() {
                    let message = e.model_message();
                    stop.stop(e);
                    return ToolCallError::Failure(message);
                }
                ToolCallError::Failure(e.model_message())
            };
            let mut input = input.unwrap_or(Value::Null);
            let mut hooked = name.clone();
            hooks.before(&mut hooked, &mut input, &ctx).await.map_err(fail)?;
            let executed = run_tool(tool.as_ref(), &name, input, ctx, hooks.as_ref()).await.map_err(fail)?;
            let collected = output_files(&executed.content);
            if !collected.is_empty() {
                files.lock().unwrap().push(Collected { index, files: collected });
            }
            Ok(Some(result_value(&executed)))
        })
    }

    fn tool_before(&self, call: &ToolInvocation) -> BoxFuture<'static, Result<(), ToolCallError>> {
        self.rows.tool_started(call);
        self.record(true);
        Box::pin(async { Ok(()) })
    }

    fn tool_after(&self, call: &ToolInvocation, result: &CallResult) {
        self.record(self.rows.settled(call.id, result).is_some());
    }

    fn extension_before(&self, call: &ExtensionInvocation) -> BoxFuture<'static, Result<(), HostValue>> {
        self.record(self.rows.extension_started(call).is_some());
        Box::pin(async { Ok(()) })
    }

    fn extension_after(&self, call: &ExtensionInvocation, result: &CallResult) {
        self.record(self.rows.settled(call.id, result).is_some());
    }
}

/// Runs the `execute` tool: `{ code }` → `{ output, content, metadata }` like upstream `CodeModeTool.create`.
pub async fn execute(snapshot: &Snapshot, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
    execute_with(snapshot, input, ctx, Arc::new(NoHooks)).await
}

/// [`execute`] with plugin hooks around every nested tool call.
pub async fn execute_with(snapshot: &Snapshot, input: Value, ctx: ToolContext, hooks: Arc<dyn ExecuteHooks>) -> Result<ToolResult, ToolError> {
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
    let stop = Stopper::default();
    let host = Arc::new_cyclic(|this| ExecuteHost {
        tools,
        ctx: ctx.clone(),
        hooks,
        rows: ProgressRows::new(),
        throttle: Mutex::new(Throttle::default()),
        calls: Mutex::new(0),
        files: files.clone(),
        stop: stop.clone(),
        this: this.clone(),
    });
    let runtime = runtime(snapshot, host.clone(), Some((fetch_authorize(ctx.clone(), stop.clone()), fetch_redirect_check(ctx.clone()))));
    let cancel = ctx.cancel.clone();
    let result = tokio::select! {
        result = runtime.execute(&code) => result,
        _ = cancel.cancelled() => {
            host.interrupt();
            return Err(ToolError::interrupted());
        }
        _ = stop.token.cancelled() => {
            host.interrupt();
            let defect = stop.defect.lock().unwrap().take();
            return Err(defect.unwrap_or_else(ToolError::declined));
        }
    };
    if let Some(defect) = stop.defect.lock().unwrap().take() {
        host.interrupt();
        return Err(defect);
    }
    host.drop_trailing();
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


/// The host of a schedule check (`CodeModeTool.check`): nested calls get the ids `call_<messageID>_<n>`, run
/// through the plugin hooks, and see the declared output (or the joined text) like an `execute` call.
struct CheckHost {
    tools: Vec<(String, String, Arc<dyn Tool>)>,
    ctx: ToolContext,
    hooks: Arc<dyn ExecuteHooks>,
    calls: Mutex<usize>,
    /// The `Unattended` rules in effect: nested calls run on the interpreter thread, outside the caller's task.
    unattended: Option<oc_schema::types::Ruleset>,
}

impl Host for CheckHost {
    fn call(&self, path: &str, input: Option<Json>) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>> {
        let found = self.tools.iter().find(|(qualified, _, _)| qualified == path).cloned();
        let index = {
            let mut calls = self.calls.lock().unwrap();
            let index = *calls;
            *calls += 1;
            index
        };
        let mut ctx = self.ctx.clone();
        ctx.call_id = format!("call_{}_{index}", ctx.message_id);
        let hooks = self.hooks.clone();
        let unattended = self.unattended.clone();
        let run = async move {
            let Some((_, name, tool)) = found else {
                return Err(ToolCallError::failure("Execute context is unavailable"));
            };
            let mut input = input.unwrap_or(Value::Null);
            let mut hooked = name.clone();
            let fail = |e: ToolError| ToolCallError::Failure(e.model_message());
            hooks.before(&mut hooked, &mut input, &ctx).await.map_err(fail)?;
            let executed = run_tool(tool.as_ref(), &name, input, ctx, hooks.as_ref()).await.map_err(fail)?;
            Ok(Some(result_value(&executed)))
        };
        match unattended {
            Some(rules) => Box::pin(crate::permission::unattended(rules, run)),
            None => Box::pin(run),
        }
    }
}

/// `CodeModeTool.check`: runs one program over the snapshot's Code Mode tools alone, with no model step and no
/// `fetch`, binding `bindings` as outer globals.
pub async fn check(
    snapshot: &Snapshot,
    code: &str,
    bindings: Vec<(String, Value)>,
    limits: ExecutionLimits,
    ctx: ToolContext,
    hooks: Arc<dyn ExecuteHooks>,
) -> oc_codemode::codemode::ExecResult {
    let tools = snapshot
        .codemode
        .iter()
        .map(|(name, tool)| (qualified_name(tool.as_ref()), name.clone(), tool.clone()))
        .collect();
    let host = Arc::new(CheckHost {
        tools,
        ctx,
        hooks,
        calls: Mutex::new(0),
        unattended: crate::permission::current_unattended(),
    });
    let runtime = match CodeMode::new(entries(snapshot), host, Vec::new(), limits) {
        Ok(runtime) => runtime.with_bindings(bindings),
        Err(e) => {
            return oc_codemode::codemode::ExecResult::Failure {
                error: oc_codemode::interp::errors::Diagnostic::new(oc_codemode::interp::errors::DiagnosticKind::ExecutionFailure, e.to_string()),
                logs: None,
                truncated: false,
                tool_calls: Vec::new(),
            };
        }
    };
    runtime.execute(code).await
}

/// The `execute` tool over a request snapshot, so it runs through the same hooks and normalization as direct tools.
pub struct ExecuteTool(pub Snapshot, pub Arc<dyn ExecuteHooks>);

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
        execute_with(&self.0, input, ctx, self.1.clone()).await
    }
}
