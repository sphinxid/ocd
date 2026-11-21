//! Code Mode changes of opencode 2.3.1 in core: a plugin's `tool.execute.before` rename applies to a call inside a
//! program (#63633), the renamed tool's structured output reaches the program (#64333), each call of a program is
//! marked as made by the `execute` call so its permission requests show on that call's row until the program ends
//! (#64324), and the `execute` text has no `null` line before the logs of a program that returns nothing (#64344).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use oc_core::codemode;
use oc_schema::Metadata;
use oc_tools::host::{PermissionError, PermissionRequest, ToolHost};
use oc_tools::registry::ExecuteHooks;
use oc_tools::tool::{DefinitionContext, ToolConfig, ToolOptions};
use oc_tools::{Registry, Tool, ToolContext, ToolDefinition, ToolError, ToolResult};
use serde_json::{json, Value};

struct Host;

#[async_trait]
impl ToolHost for Host {
    async fn ask_permission(&self, _request: PermissionRequest) -> Result<(), PermissionError> {
        Ok(())
    }
}

/// A Code Mode tool (or a direct one) answering with fixed text and, optionally, a structured output. Each call
/// records its id and the call whose row its permission requests would show on.
struct Fixed {
    name: &'static str,
    codemode: bool,
    output: Option<Value>,
    seen: Arc<Mutex<Vec<(String, Option<String>)>>>,
}

#[async_trait]
impl Tool for Fixed {
    fn name(&self) -> &str {
        self.name
    }
    fn options(&self) -> ToolOptions {
        ToolOptions { codemode: self.codemode, ..ToolOptions::default() }
    }
    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        let definition = ToolDefinition::new(
            self.name,
            "A tool",
            json!({ "type": "object", "properties": { "text": { "type": "string" } } }),
        );
        match &self.output {
            Some(_) => definition.with_output(Some(json!({ "type": "object", "properties": { "text": { "type": "string" } } }))),
            None => definition,
        }
    }
    async fn execute(&self, _input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        self.seen.lock().unwrap().push((ctx.call_id.clone(), oc_core::permission::nested_parent(&ctx.call_id)));
        let result = ToolResult::text(self.name);
        Ok(match &self.output {
            Some(output) => result.with_output(output.clone()),
            None => result,
        })
    }
}

/// A plugin renaming every `echo` call to the tool its `text` names (upstream's test hook), and every `plain` call
/// to `constant`; it records the tools `tool.execute.after` sees.
#[derive(Default)]
struct Rename {
    after: Mutex<Vec<String>>,
}

#[async_trait]
impl ExecuteHooks for Rename {
    async fn before(&self, tool: &mut String, input: &mut Value, _ctx: &ToolContext) -> Result<(), ToolError> {
        if tool == "echo"
            && let Some(text) = input["text"].as_str()
        {
            *tool = text.to_string();
        }
        if tool == "plain" {
            *tool = "constant".into();
        }
        Ok(())
    }
    async fn after(&self, tool: &str, _input: &Value, _result: &mut Result<ToolResult, ToolError>, _ctx: &ToolContext) {
        self.after.lock().unwrap().push(tool.to_string());
    }
}

fn context() -> ToolContext {
    let dir = std::env::temp_dir();
    ToolContext {
        session_id: "ses_231".into(),
        message_id: "msg_231".into(),
        call_id: "call_program_231".into(),
        agent: "build".into(),
        directory: dir.clone(),
        worktree: dir,
        project_id: "prj".into(),
        workspace_id: None,
        global: Arc::new(oc_util::Global::resolve()),
        cancel: tokio_util::sync::CancellationToken::new(),
        stop: Default::default(),
        progress: Arc::new(|_: Metadata| {}),
        config: Arc::new(ToolConfig::default()),
        host: Arc::new(Host),
    }
}

type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

fn registry(seen: &Seen) -> Registry {
    let tool = |name, codemode, output: Option<Value>| -> Arc<dyn Tool> {
        Arc::new(Fixed { name, codemode, output, seen: seen.clone() })
    };
    let mut registry = Registry::new();
    registry.add(tool("echo", true, Some(json!({ "text": "echoed" })))).unwrap();
    registry.add(tool("constant", true, Some(json!({ "text": "renamed" })))).unwrap();
    registry.add(tool("plain", true, None)).unwrap();
    registry.add(tool("direct", false, None)).unwrap();
    registry
}

async fn output(snapshot: &oc_tools::Snapshot, code: &str, hooks: Arc<Rename>) -> String {
    let result = codemode::execute_with(snapshot, json!({ "code": code }), context(), hooks).await.unwrap();
    result.output.unwrap()["output"].as_str().unwrap().to_string()
}

// tool-registry.test.ts "a hook can rename a call inside a Code Mode program to another of its tools" (#63633)
#[tokio::test]
async fn a_hook_can_rename_a_call_inside_a_program_to_another_of_its_tools() {
    let seen = Seen::default();
    let snapshot = registry(&seen).snapshot(&[]);
    let hooks = Arc::new(Rename::default());
    assert_eq!(output(&snapshot, r#"return await tools.echo({ text: "constant" })"#, hooks.clone()).await, r#"{"text":"renamed"}"#);
    assert!(hooks.after.lock().unwrap().contains(&"constant".to_string()));
    // A program can only reach its own tools, however the call is renamed.
    for name in ["direct", "missing"] {
        assert_eq!(
            output(&snapshot, &format!(r#"try {{ await tools.echo({{ text: "{name}" }}) }} catch (e) {{ return e.message }}"#), hooks.clone()).await,
            format!("Tool is not available for this request: {name}")
        );
    }
}

// An automation's code renames the same way, among the tools it was given.
#[tokio::test]
async fn a_check_renames_among_its_own_tools() {
    let seen = Seen::default();
    let full = registry(&seen).snapshot(&[]);
    let only = |names: &[&str]| oc_tools::Snapshot {
        codemode: full.codemode.iter().filter(|(name, _)| names.contains(&name.as_str())).cloned().collect(),
        ..full.clone()
    };
    let run = |snapshot: oc_tools::Snapshot| async move {
        codemode::check(
            &snapshot,
            r#"return await tools.echo({ text: "constant" })"#,
            Vec::new(),
            Default::default(),
            context(),
            Arc::new(Rename::default()),
        )
        .await
        .to_json()
    };
    assert_eq!(run(only(&["echo"])).await["ok"], json!(false));
    let renamed = run(only(&["echo", "constant"])).await;
    assert_eq!((renamed["ok"].clone(), renamed["value"].clone()), (json!(true), json!({ "text": "renamed" })));
}

// tool-registry.test.ts "a call renamed inside a Code Mode program returns the renamed tool's structured output"
// (#64333): `plain` declares no output, `constant` does.
#[tokio::test]
async fn a_renamed_call_returns_the_renamed_tools_structured_output() {
    let seen = Seen::default();
    let snapshot = registry(&seen).snapshot(&[]);
    assert_eq!(output(&snapshot, r#"return await tools.plain({ text: "x" })"#, Arc::new(Rename::default())).await, r#"{"text":"renamed"}"#);
    // The signature of a tool without an output still reads `string | null`.
    let signatures: Vec<String> = codemode::catalog_inventory(&snapshot)
        .into_iter()
        .filter_map(|entry| match entry {
            oc_codemode::catalog::CatalogEntry::Tool(tool) if tool.name == "plain" => Some(tool.signature),
            _ => None,
        })
        .collect();
    assert_eq!(signatures, vec!["tools.plain({\n  text?: string,\n}): Promise<string | null>".to_string()]);
}

// tool-execute.test.ts "each call in a program gets its own ID and is marked as nested in execute" (#64324): the
// calls' permission requests show on the execute call's row until the program ends.
#[tokio::test]
async fn each_call_gets_its_own_id_and_is_marked_as_nested_in_execute() {
    let seen = Seen::default();
    let snapshot = registry(&seen).snapshot(&[]);
    output(&snapshot, r#"await Promise.all([tools.constant({}), tools.constant({})])"#, Arc::new(Rename::default())).await;
    let calls = seen.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0].0, calls[1].0);
    for (id, parent) in &calls {
        assert_ne!(id, "call_program_231");
        assert_eq!(parent.as_deref(), Some("call_program_231"));
        assert_eq!(oc_core::permission::nested_parent(id), None, "still nested after the program ended");
    }
}

// tool-execute.test.ts (#64344): a program that only logs returns undefined, sent as null; no "null" line precedes
// its logs, while one that returns a value keeps it.
#[tokio::test]
async fn a_program_that_only_logs_has_no_null_line() {
    let seen = Seen::default();
    let snapshot = registry(&seen).snapshot(&[]);
    let hooks = Arc::new(Rename::default());
    assert_eq!(output(&snapshot, r#"console.log("found 12 files")"#, hooks.clone()).await, "Logs:\nfound 12 files");
    assert_eq!(output(&snapshot, "return null", hooks.clone()).await, "null");
    assert_eq!(output(&snapshot, r#"console.log("x"); return 1"#, hooks).await, "1\n\nLogs:\nx");
}

/// The output of `code` run as an `execute` call whose `path` global (or none) resolves from `directory`.
async fn path_output(code: &str, directory: Option<&std::path::Path>) -> Value {
    let snapshot = Registry::new().snapshot(&[]);
    let mut ctx = context();
    if let Some(directory) = directory {
        ctx.directory = directory.to_path_buf();
    }
    let path = directory.map(|d| d.to_string_lossy().into_owned());
    let result = codemode::execute_in(&snapshot, json!({ "code": code }), ctx, Arc::new(oc_tools::registry::NoHooks), path).await.unwrap();
    let text = result.output.unwrap()["output"].as_str().unwrap().to_string();
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

// tool-execute.test.ts "path" (2.3.1, #64476): only with `full_codemode` (core passes the session's directory)
// programs get Node's `path`, the host's flavour resolving from the session's directory.
#[tokio::test]
async fn path_is_the_hosts_path_module_resolving_against_the_sessions_directory() {
    assert_eq!(path_output("return [typeof path]", None).await, json!(["undefined"]));
    let directory = if cfg!(windows) { std::path::PathBuf::from(r"C:\session\dir") } else { std::path::PathBuf::from("/session/dir") };
    let sep = std::path::MAIN_SEPARATOR_STR;
    let out = path_output(
        r#"return [typeof path, path.resolve("a", "b"), path.relative("a", "c"), path.join("out", "..", "x.txt"), path.basename("/a/b.ts", ".ts"), path.posix.join("a", "b"), path.win32.join("a", "b"), path.sep]"#,
        Some(&directory),
    )
    .await;
    let resolved = directory.join("a").join("b").to_string_lossy().into_owned();
    assert_eq!(out, json!(["object", resolved, format!("..{sep}c"), "x.txt", "b", "a/b", "a\\b", sep]));
    // The other platform's flavour resolves from its own root, not the server's working directory.
    let (other, root) = if cfg!(windows) { ("posix", "/a") } else { ("win32", "\\a") };
    assert_eq!(path_output(&format!(r#"return [path.{other}.resolve("a")]"#), Some(&directory)).await, json!([root]));
    // Importable by its Node module names.
    let imported = path_output(
        "import { join } from \"node:path\"\nconst posix = require(\"path\").posix\nreturn [join(\"a\", \"b\"), posix.dirname(\"a/b\")]",
        Some(&directory),
    )
    .await;
    assert_eq!(imported, json!([format!("a{sep}b"), "a"]));
}
