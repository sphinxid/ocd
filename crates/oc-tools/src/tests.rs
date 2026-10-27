//! End-to-end tool tests with a mock host.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use oc_schema::{Effect, PermissionRule, ToolContent};
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::formatter::Formatter;
use crate::host::{FormRequest, FormResponse, PermissionError, PermissionRequest, ToolHost};
use crate::registry::{NoHooks, Registry};
use crate::tool::{DefinitionContext, Tool, ToolConfig, ToolContext, ToolError, ToolErrorKind, ToolResult};
use crate::tools;

#[derive(Default)]
struct MockHost {
    requests: Mutex<Vec<PermissionRequest>>,
    deny: Mutex<Option<PermissionError>>,
    form: Mutex<Option<FormResponse>>,
}

#[async_trait]
impl ToolHost for MockHost {
    async fn ask_permission(&self, request: PermissionRequest) -> Result<(), PermissionError> {
        self.requests.lock().push(request);
        match self.deny.lock().clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn ask_form(&self, _request: FormRequest) -> Result<FormResponse, String> {
        Ok(self.form.lock().clone().unwrap_or(FormResponse::Cancelled { message: None }))
    }
}

fn context(dir: &Path, host: Arc<MockHost>) -> ToolContext {
    let mut global = oc_util::Global::resolve();
    global.data = dir.join(".data");
    global.bin = dir.join(".bin");
    global.home = dir.join(".home");
    ToolContext {
        session_id: "ses_test".into(),
        message_id: "msg_test".into(),
        call_id: "call_1".into(),
        agent: "build".into(),
        directory: dir.to_path_buf(),
        worktree: dir.to_path_buf(),
        project_id: "prj".into(),
        workspace_id: None,
        global: Arc::new(global),
        cancel: tokio_util::sync::CancellationToken::new(),
        progress: Arc::new(|_| {}),
        config: Arc::new(ToolConfig::default()),
        host,
    }
}

async fn run(tool: &dyn Tool, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
    crate::registry::execute_tool(tool, tool.name(), input, ctx, &NoHooks).await
}

fn text(result: &ToolResult) -> String {
    result.text_content()
}

#[tokio::test]
async fn edit_tool_flow() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let ctx = context(dir.path(), host.clone());
    std::fs::write(dir.path().join("a.txt"), "\u{FEFF}one\r\ntwo\r\none\r\n").unwrap();
    let tool = tools::edit::EditTool::new(Arc::new(Formatter::new()));
    let err = run(&tool, json!({"path": "a.txt", "oldString": "one", "newString": "uno"}), ctx.clone()).await.unwrap_err();
    assert_eq!(err.model_message(), "Found 2 matches for oldString, but expected exactly one. Add more surrounding context to make oldString unique, or set replaceAll to true to replace every occurrence.");
    // assert happened before the count error, without preview
    assert_eq!(host.requests.lock()[0].action, "edit");
    assert!(host.requests.lock()[0].metadata.is_none());
    let ok = run(&tool, json!({"path": "a.txt", "oldString": "two\n", "newString": "dos\n"}), ctx.clone()).await.unwrap();
    assert_eq!(text(&ok), "Edited a.txt (1 replacement)");
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "\u{FEFF}one\r\ndos\r\none\r\n");
    let files = &ok.metadata.as_ref().unwrap()["files"][0];
    assert_eq!(files["file"], json!("a.txt"));
    assert_eq!(files["additions"], json!(1));
    let all = run(&tool, json!({"path": "a.txt", "oldString": "one", "newString": "1", "replaceAll": true}), ctx.clone()).await.unwrap();
    assert_eq!(text(&all), "Edited a.txt (2 replacements)");
    let err = run(&tool, json!({"path": "a.txt", "oldString": "x", "newString": "x"}), ctx.clone()).await.unwrap_err();
    assert_eq!(err.message, "No changes to apply: oldString and newString are identical.");
    let err = run(&tool, json!({"path": "missing.txt", "oldString": "x", "newString": "y"}), ctx.clone()).await.unwrap_err();
    assert_eq!(err.message, "File not found: missing.txt");
    let err = run(&tool, json!({"path": "a.txt", "oldString": "zzz", "newString": "y"}), ctx.clone()).await.unwrap_err();
    assert_eq!(err.message, "Could not find oldString in a.txt. It must match exactly, including whitespace and indentation.");
    let err = run(&tool, json!({"path": "a.txt", "oldString": ""}), ctx.clone()).await.unwrap_err();
    assert_eq!(
        err.message,
        "Invalid arguments for tool \"edit\":\n- newString: Missing key\n\nArguments provided:\n{\n  \"path\": \"a.txt\",\n  \"oldString\": \"\"\n}\n\nUpdate the arguments and call the tool again."
    );
}

fn edit_ctx() -> (tempfile::TempDir, Arc<MockHost>, ToolContext, tools::edit::EditTool) {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let ctx = context(dir.path(), host.clone());
    (dir, host, ctx, tools::edit::EditTool::new(Arc::new(Formatter::new())))
}

// tool-edit.test.ts "uses non-overlapping trailing-whitespace matches and preserves CRLF"
#[tokio::test]
async fn edit_trailing_whitespace_matches_preserve_crlf() {
    let (dir, _host, ctx, tool) = edit_ctx();
    std::fs::write(dir.path().join("overlap.txt"), "a  \na  \na  \n").unwrap();
    std::fs::write(dir.path().join("windows.txt"), "a  \r\nb\t\r\n").unwrap();
    let ok = run(&tool, json!({"path": "overlap.txt", "oldString": "a\na", "newString": "x", "replaceAll": true}), ctx.clone()).await.unwrap();
    assert_eq!(ok.output.as_ref().unwrap()["replacements"], json!(1));
    run(&tool, json!({"path": "windows.txt", "oldString": "a\nb", "newString": "x"}), ctx.clone()).await.unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("overlap.txt")).unwrap(), "x\na  \n");
    assert_eq!(std::fs::read_to_string(dir.path().join("windows.txt")).unwrap(), "x\r\n");
}

// "matches across mixed line endings and keeps each region's endings"
#[tokio::test]
async fn edit_mixed_line_endings_keep_region_endings() {
    let (dir, _host, ctx, tool) = edit_ctx();
    std::fs::write(dir.path().join("mostly-lf.txt"), "head\r\nAAA OLD BBB\ntail\n").unwrap();
    std::fs::write(dir.path().join("mostly-crlf.txt"), "one\r\ntwo\r\nthree\nfour\r\n").unwrap();
    for input in [
        json!({"path": "mostly-lf.txt", "oldString": "AAA OLD BBB", "newString": "AAA NEW\nLINE2"}),
        json!({"path": "mostly-lf.txt", "oldString": "LINE2\ntail", "newString": "LINE2\nTAIL"}),
        json!({"path": "mostly-crlf.txt", "oldString": "o\nthree\nfo", "newString": "O\nTHREE\nFO"}),
        json!({"path": "mostly-crlf.txt", "oldString": "one", "newString": "ONE\nUNO"}),
    ] {
        run(&tool, input, ctx.clone()).await.unwrap();
    }
    assert_eq!(std::fs::read_to_string(dir.path().join("mostly-lf.txt")).unwrap(), "head\r\nAAA NEW\nLINE2\nTAIL\n");
    assert_eq!(std::fs::read_to_string(dir.path().join("mostly-crlf.txt")).unwrap(), "ONE\r\nUNO\r\ntwO\r\nTHREE\nFOur\r\n");
}

// "keeps the endings of unchanged and in-place lines inside a multi-line match"
#[tokio::test]
async fn edit_keeps_endings_inside_multiline_match() {
    let (dir, _host, ctx, tool) = edit_ctx();
    let target = dir.path().join("store.cs");
    std::fs::write(&target, "All(item.Sku);\r\n\r\nFind(sku);\r\n\r\nvoid Add()\n{\n  _items[item.Sku] = item;\n}\r\n").unwrap();
    run(
        &tool,
        json!({
            "path": "store.cs",
            "oldString": "All(item.Sku);\n\nFind(sku);\n\nvoid Add()\n{\n  _items[item.Sku] = item;\n}",
            "newString": "All(item.Code);\n\nFind(code);\n\nvoid Add()\n{\n  _items[item.Code] = item;\n}\n\nbool Remove(string code)\n{\n}",
        }),
        ctx.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "All(item.Code);\r\n\r\nFind(code);\r\n\r\nvoid Add()\n{\n  _items[item.Code] = item;\n}\r\n\r\nbool Remove(string code)\r\n{\r\n}\r\n"
    );
}

// "applies edits that differ only in \r to the exact bytes"
#[tokio::test]
async fn edit_endings_only_applies_exact_bytes() {
    let (dir, _host, ctx, tool) = edit_ctx();
    std::fs::write(dir.path().join("strip.sh"), "a\r\nb\r\n").unwrap();
    std::fs::write(dir.path().join("whole.sh"), "x\r\ny\r\n").unwrap();
    std::fs::write(dir.path().join("add.bat"), "p\nq\n").unwrap();
    let ok = run(&tool, json!({"path": "strip.sh", "oldString": "\r", "newString": "", "replaceAll": true}), ctx.clone()).await.unwrap();
    assert_eq!(ok.output.as_ref().unwrap()["replacements"], json!(2));
    run(&tool, json!({"path": "whole.sh", "oldString": "x\r\ny\r\n", "newString": "x\ny\n"}), ctx.clone()).await.unwrap();
    run(&tool, json!({"path": "add.bat", "oldString": "p\nq", "newString": "p\r\nq"}), ctx.clone()).await.unwrap();
    // The exact bytes are gone now, so the same edit finds nothing.
    let err = run(&tool, json!({"path": "add.bat", "oldString": "p\nq", "newString": "p\r\nq"}), ctx.clone()).await.unwrap_err();
    assert_eq!(err.message, "Could not find oldString in add.bat. It must match exactly, including whitespace and indentation.");
    assert_eq!(std::fs::read_to_string(dir.path().join("strip.sh")).unwrap(), "a\nb\n");
    assert_eq!(std::fs::read_to_string(dir.path().join("whole.sh")).unwrap(), "x\ny\n");
    assert_eq!(std::fs::read_to_string(dir.path().join("add.bat")).unwrap(), "p\r\nq\n");
}

// "fails instead of reporting success when the edit leaves the file unchanged"
#[tokio::test]
async fn edit_unchanged_result_fails() {
    let (dir, _host, ctx, tool) = edit_ctx();
    let target = dir.path().join("quote.txt");
    std::fs::write(&target, "it\u{2019}s here\n").unwrap();
    let before = std::fs::metadata(&target).unwrap().modified().unwrap();
    // The typography tier matches the curly quote, and the replacement writes the same bytes back.
    let err = run(&tool, json!({"path": "quote.txt", "oldString": "it's here", "newString": "it\u{2019}s here"}), ctx.clone()).await.unwrap_err();
    assert_eq!(
        err.message,
        "No changes to apply: the edit leaves quote.txt unchanged. Line endings follow the file; to change them, write \\r explicitly in oldString or newString."
    );
    assert_eq!(err.to_session_error().kind, "tool.execution");
    assert_eq!(std::fs::metadata(&target).unwrap().modified().unwrap(), before);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "it\u{2019}s here\n");
}

#[tokio::test]
async fn permission_outcomes() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let ctx = context(dir.path(), host.clone());
    let tool = tools::write::WriteTool::new(Arc::new(Formatter::new()));
    *host.deny.lock() = Some(PermissionError::Corrected { feedback: "use a different name".into() });
    let err = run(&tool, json!({"path": "x.txt", "content": "hi"}), ctx.clone()).await.unwrap_err();
    assert_eq!(err.message, "Unable to write x.txt");
    assert_eq!(err.to_session_error().kind, "permission.rejected");
    assert_eq!(err.model_message(), "use a different name");
    *host.deny.lock() = Some(PermissionError::Declined);
    let err = run(&tool, json!({"path": "x.txt", "content": "hi"}), ctx.clone()).await.unwrap_err();
    assert_eq!(err.kind, ToolErrorKind::Declined);
    *host.deny.lock() = None;
    let ok = run(&tool, json!({"path": "sub/x.txt", "content": "hi"}), ctx.clone()).await.unwrap();
    assert_eq!(text(&ok), "Created file successfully: sub/x.txt");
    let ok = run(&tool, json!({"path": "sub/x.txt", "content": "ho"}), ctx.clone()).await.unwrap();
    assert_eq!(text(&ok), "Wrote file successfully: sub/x.txt");
    let request = host.requests.lock().last().cloned().unwrap();
    assert_eq!(request.metadata.unwrap()["files"][0]["status"], json!("modified"));
}

#[tokio::test]
async fn external_directory_request() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("f.txt"), "data\n").unwrap();
    let host = Arc::new(MockHost::default());
    let ctx = context(&project, host.clone());
    let path = outside.join("f.txt").to_string_lossy().into_owned();
    let ok = run(&tools::read::ReadTool, json!({"path": path}), ctx).await.unwrap();
    assert_eq!(text(&ok), format!("Read file {path}, lines 1-1\n1: data"));
    let requests = host.requests.lock();
    assert_eq!(requests[0].action, "external_directory");
    assert_eq!(requests[0].resources, vec![format!("{}/*", outside.to_string_lossy())]);
    assert_eq!(requests[1].action, "read");
    assert_eq!(requests[1].resources, vec![path.clone()]);
    assert_eq!(ok.metadata.as_ref().unwrap()["truncated"], json!(false));
}

#[tokio::test]
async fn patch_tool_flow() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let ctx = context(dir.path(), host.clone());
    std::fs::write(dir.path().join("app.py"), "def greet():\n    print(\"Hi\")\n").unwrap();
    std::fs::write(dir.path().join("old.txt"), "bye\n").unwrap();
    let tool = tools::patch::PatchTool::new(Arc::new(Formatter::new()));
    let patch = "*** Begin Patch\n*** Add File: hello.txt\n+Hello world\n*** Update File: app.py\n*** Move to: main.py\n@@ def greet():\n-    print(\"Hi\")\n+    print(\"Hello, world!\")\n*** Delete File: old.txt\n*** End Patch";
    let ok = run(&tool, json!({"patchText": patch}), ctx.clone()).await.unwrap();
    assert_eq!(text(&ok), "Success. Updated the following files:\nA hello.txt\nM main.py\nD old.txt");
    assert_eq!(std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(), "Hello world\n");
    assert_eq!(std::fs::read_to_string(dir.path().join("main.py")).unwrap(), "def greet():\n    print(\"Hello, world!\")\n");
    assert!(!dir.path().join("app.py").exists());
    assert!(!dir.path().join("old.txt").exists());
    let request = host.requests.lock().last().cloned().unwrap();
    assert_eq!(request.resources, vec!["hello.txt", "app.py", "main.py", "old.txt"]);
    let metadata = request.metadata.unwrap();
    assert_eq!(metadata["filepath"], json!("hello.txt, app.py, main.py, old.txt"));
    let files = &ok.metadata.as_ref().unwrap()["files"];
    assert_eq!(files[1]["file"], json!("main.py"));
    assert!(files[1]["patch"].as_str().unwrap().contains("\n-    print(\"Hi\")\n+    print(\"Hello, world!\")"));

    let err = run(&tool, json!({"patchText": "nope"}), ctx.clone()).await.unwrap_err();
    assert_eq!(err.message, "patch verification failed: The first line of the patch must be '*** Begin Patch'");
    let err = run(&tool, json!({"patchText": "*** Begin Patch\n*** End Patch"}), ctx.clone()).await.unwrap_err();
    assert_eq!(err.message, "patch rejected: empty patch");
    let err = run(&tool, json!({"patchText": ""}), ctx.clone()).await.unwrap_err();
    assert_eq!(err.message, "patchText is required");
    let err = run(&tool, json!({"patchText": "*** Begin Patch\n*** Delete File: nothere.txt\n*** End Patch"}), ctx.clone()).await.unwrap_err();
    assert_eq!(err.message, "patch verification failed: Failed to delete nothere.txt: file does not exist");
    let err = run(&tool, json!({"patchText": "*** Begin Patch\n*** Update File: hello.txt\n@@\n-nothing\n+x\n*** End Patch"}), ctx.clone()).await.unwrap_err();
    assert_eq!(err.message, "patch verification failed: Failed to find expected lines in hello.txt:\nnothing");
}

#[tokio::test]
async fn question_flow() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let ctx = context(dir.path(), host.clone());
    let input = json!({"questions": [{"question": "Color?", "header": "Color", "options": [{"label": "Red", "description": "r"}]}]});
    let err = run(&tools::question::QuestionTool, input.clone(), ctx.clone()).await.unwrap_err();
    assert_eq!(err.kind, ToolErrorKind::QuestionDismissed);
    *host.form.lock() = Some(FormResponse::Cancelled { message: Some("not now".into()) });
    let err = run(&tools::question::QuestionTool, input.clone(), ctx.clone()).await.unwrap_err();
    assert_eq!(err.model_message(), "not now");
    let mut answer = oc_schema::Metadata::new();
    answer.insert("q0".into(), json!("Red"));
    *host.form.lock() = Some(FormResponse::Answered(answer));
    let ok = run(&tools::question::QuestionTool, input, ctx).await.unwrap();
    assert_eq!(text(&ok), "User has answered your questions: \"Color?\"=\"Red\". You can now continue with the user's answers in mind.");
    let request = host.requests.lock()[0].clone();
    assert_eq!(request.action, "question");
    assert!(request.save.is_none());
}

/// `tool-todo.test.ts`: "registers todowrite on the native tool list and summarizes the update".
#[tokio::test]
async fn todo_flow() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let ctx = context(dir.path(), host.clone());
    let todos = json!([
        { "content": "Write tests", "status": "in_progress" },
        { "content": "Ship it", "status": "pending" },
        { "content": "Read the code", "status": "completed" },
        { "content": "Rewrite everything", "status": "cancelled" },
    ]);
    let ok = run(&tools::todo::TodoTool, json!({ "todos": todos }), ctx.clone()).await.unwrap();
    assert_eq!(ok.output, Some(json!({ "todos": todos })));
    assert_eq!(text(&ok), "Todos updated: 1/4 completed, 1 in progress, 1 pending, 1 cancelled. In progress: \"Write tests\"");
    assert_eq!(ok.metadata.unwrap()["todos"], todos);
    let request = host.requests.lock()[0].clone();
    assert_eq!((request.action.as_str(), request.resources.clone(), request.save.clone()), ("todowrite", vec!["*".to_string()], Some(vec!["*".to_string()])));
    let ok = run(&tools::todo::TodoTool, json!({ "todos": [] }), ctx.clone()).await.unwrap();
    assert_eq!(text(&ok), "Todos cleared");
    let err = run(&tools::todo::TodoTool, json!({ "todos": [{ "content": "x", "status": "done" }] }), ctx.clone()).await.unwrap_err();
    assert!(err.message.starts_with("Invalid arguments for tool \"todowrite\":\n- todos[0].status: "), "{}", err.message);
    *host.deny.lock() = Some(PermissionError::Corrected { feedback: "not now".into() });
    let err = run(&tools::todo::TodoTool, json!({ "todos": [] }), ctx.clone()).await.unwrap_err();
    assert_eq!(err.message, "Unable to update todos");
    assert_eq!(err.model_message(), "not now");
}

#[cfg(unix)]
#[tokio::test]
async fn shell_tool_flow() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let mut ctx = context(dir.path(), host.clone());
    ctx.config = Arc::new(ToolConfig { shell: Some("/bin/sh".into()), ..ToolConfig::default() });
    let service = crate::shell::service::ShellService::new(dir.path().join(".data"), None);
    let tool = tools::shell::ShellTool::new(service);
    let ok = run(&tool, json!({"command": "echo hello && exit 2"}), ctx.clone()).await.unwrap();
    assert_eq!(ok.content, vec![ToolContent::Text { text: "hello\n".into() }, ToolContent::Text { text: "Exited with code 2".into() }]);
    let request = host.requests.lock()[0].clone();
    assert_eq!(request.action, "shell");
    assert_eq!(request.resources, vec!["echo hello", "exit 2"]);
    assert_eq!(request.save, Some(vec!["echo *".to_string(), "exit *".to_string()]));
    let ok = run(&tool, json!({"command": "sleep 3", "timeout": 100}), ctx.clone()).await.unwrap();
    assert_eq!(
        ok.content,
        vec![
            ToolContent::Text {
                text: "(no output)\n\nCommand exceeded timeout of 100 ms. Retry with a larger timeout if the command is expected to take longer.".into()
            },
            ToolContent::Text { text: "Timed out before completion".into() },
        ]
    );
    let err = run(&tool, json!({"command": "ls", "workdir": "nope"}), ctx.clone()).await.unwrap_err();
    assert_eq!(err.message, "Unable to execute command: ls");
    assert_eq!(err.model_message(), format!("Working directory does not exist: {}", dir.path().join("nope").to_string_lossy()));
}

#[tokio::test]
async fn registry_snapshot_and_execute() {
    let dir = tempfile::tempdir().unwrap();
    let http = reqwest::Client::new();
    let store: Arc<dyn crate::websearch::WebSearchStore> = Arc::new(NullStore);
    let services = tools::Services {
        formatter: Arc::new(Formatter::new()),
        ripgrep: Arc::new(crate::ripgrep::Ripgrep::new(dir.path().join("bin"), http.clone())),
        shell: crate::shell::service::ShellService::new(dir.path().to_path_buf(), None),
        websearch: Arc::new(crate::websearch::WebSearch::new(store, http.clone())),
        http,
    };
    let mut registry = Registry::new();
    tools::register_builtins(&mut registry, &services);
    assert!(registry.errors().is_empty());
    let rules = vec![PermissionRule { action: "webfetch".into(), resource: "*".into(), effect: Effect::Deny }];
    let snapshot = registry.snapshot(&rules);
    let names: Vec<String> = snapshot.registered_definitions().into_iter().map(|d| d.name).collect();
    assert_eq!(names, vec!["edit", "glob", "grep", "patch", "question", "read", "shell", "skill", "subagent", "todowrite", "websearch", "write"]);
    assert_eq!(snapshot.codemode.len(), 8);
    let claude = DefinitionContext { model_id: Some("claude-sonnet".into()), shell: Some("/bin/bash".into()), ..Default::default() };
    let defs = snapshot.definitions(&claude, None);
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert!(!names.contains(&"patch"));
    assert!(defs.iter().find(|d| d.name == "shell").unwrap().description.contains("using bash."));
    let gpt = DefinitionContext { model_id: Some("gpt-5".into()), ..Default::default() };
    let names: Vec<String> = snapshot.definitions(&gpt, None).into_iter().map(|d| d.name).collect();
    assert!(names.contains(&"patch".to_string()) && !names.contains(&"edit".to_string()) && !names.contains(&"write".to_string()));

    let host = Arc::new(MockHost::default());
    let ctx = context(dir.path(), host);
    let advertised: HashSet<String> = names.into_iter().collect();
    let err = snapshot.execute("edit", json!({}), ctx.clone(), Some(&advertised), &NoHooks).await.unwrap_err();
    assert_eq!(err.message, "Tool is not available for this request: edit");
    let err = snapshot.execute("nope", json!({}), ctx.clone(), None, &NoHooks).await.unwrap_err();
    assert_eq!(err.message, "No tool named \"nope\" is currently available. Please use a tool from the available tool list.");
    let err = snapshot.execute("webfetch", json!({}), ctx.clone(), None, &NoHooks).await.unwrap_err();
    assert!(err.message.starts_with("No tool named \"webfetch\""));
    let disabled = registry.snapshot(&[PermissionRule { action: "*".into(), resource: "*".into(), effect: Effect::Deny }]);
    assert!(disabled.direct.is_empty());
    assert!(!disabled.execute_enabled);
}

struct NullStore;

#[async_trait]
impl crate::websearch::WebSearchStore for NullStore {
    async fn kv_get(&self, _key: &str) -> Option<Value> {
        None
    }
    async fn kv_set(&self, _key: &str, _value: Value) {}
    async fn kv_remove(&self, _key: &str) {}
    async fn api_key(&self, _integration_id: &str) -> Option<String> {
        None
    }
}
