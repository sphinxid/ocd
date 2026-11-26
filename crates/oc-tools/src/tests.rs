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
    /// `(type, model)` of every enabled model.
    models: Mutex<Vec<(&'static str, crate::host::ModelSummary)>>,
    skills: Mutex<Vec<crate::host::SkillInfo>>,
    /// A resource whose requests are denied.
    denied_resource: Mutex<Option<String>>,
    /// Whether `permission_allows` answers true (for resources other than the denied one).
    allows: Mutex<bool>,
    /// The jobs cancelled, in order.
    cancelled_jobs: Mutex<Vec<String>>,
    /// The timeout of every shell command, as `shell.create.before` sees it.
    shell_timeouts: Mutex<Vec<u64>>,
    /// The agent's and session's configured rules.
    rules: Mutex<Vec<PermissionRule>>,
}

#[async_trait]
impl ToolHost for MockHost {
    async fn get_skill(&self, id: &str) -> Result<Option<crate::host::SkillInfo>, String> {
        Ok(self.skills.lock().iter().find(|s| s.id == id).cloned())
    }

    async fn permission_rules(&self, _session_id: &str, _agent: &str) -> Vec<PermissionRule> {
        self.rules.lock().clone()
    }

    async fn permission_allows(&self, request: PermissionRequest) -> bool {
        let denied = self.denied_resource.lock().clone();
        *self.allows.lock() && !request.resources.iter().any(|r| Some(r) == denied.as_ref())
    }

    async fn shell_create_before(&self, invocation: &mut crate::host::ShellInvocation) -> Result<(), String> {
        self.shell_timeouts.lock().push(invocation.timeout);
        Ok(())
    }

    async fn job_cancel(&self, id: &str) -> Result<(), String> {
        self.cancelled_jobs.lock().push(id.to_string());
        Ok(())
    }

    async fn job_background(&self, id: &str) -> Result<Option<crate::host::JobInfo>, String> {
        Ok(Some(crate::host::JobInfo {
            id: id.to_string(),
            kind: "shell".into(),
            title: None,
            status: crate::host::JobStatus::Running,
            started_at: 0,
            completed_at: None,
            output: None,
            error: None,
            notification_id: None,
        }))
    }

    async fn models_available(&self) -> Result<Vec<crate::host::ModelSummary>, String> {
        self.models_list(&[]).await
    }

    async fn models_list(&self, types: &[String]) -> Result<Vec<crate::host::ModelSummary>, String> {
        let types: Vec<&str> = if types.is_empty() { vec!["chat"] } else { types.iter().map(String::as_str).collect() };
        Ok(types
            .into_iter()
            .flat_map(|kind| {
                self.models.lock().iter().filter(|(t, _)| *t == kind).map(|(_, m)| m.clone()).collect::<Vec<_>>()
            })
            .collect())
    }

    async fn ask_permission(&self, request: PermissionRequest) -> Result<(), PermissionError> {
        let denied = self.denied_resource.lock().clone();
        if request.resources.iter().any(|r| Some(r) == denied.as_ref()) {
            let message = format!("Permission denied: {}", request.action);
            self.requests.lock().push(request);
            return Err(PermissionError::Blocked { message });
        }
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
        codemode: false,
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
        stop: Default::default(),
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
    // tool-edit.test.ts (2.3.0, #63635): the summary is the structured output; the diffs are metadata.
    assert_eq!(ok.output, Some(json!("Edited a.txt (1 replacement)")));
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "\u{FEFF}one\r\ndos\r\none\r\n");
    let files = &ok.metadata.as_ref().unwrap()["files"][0];
    assert_eq!(files["file"], json!("a.txt"));
    assert_eq!(files["additions"], json!(1));
    let all = run(&tool, json!({"path": "a.txt", "oldString": "one", "newString": "1", "replaceAll": true}), ctx.clone()).await.unwrap();
    assert_eq!(text(&all), "Edited a.txt (2 replacements)");
    assert_eq!(all.output, Some(json!("Edited a.txt (2 replacements)")));
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
    assert_eq!(ok.output, Some(json!("Edited overlap.txt (1 replacement)")));
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
    assert_eq!(ok.output, Some(json!("Edited strip.sh (2 replacements)")));
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

// tool-subagent.test.ts failures table (model resolution)
#[tokio::test]
async fn subagent_model_failures() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let model = |id: &str, variants: &[&str]| crate::host::ModelSummary {
        provider_id: "test".into(),
        id: id.into(),
        name: id.into(),
        family: None,
        released: 0.0,
        variants: variants.iter().map(|v| v.to_string()).collect(),
        cost: json!([]),
        status: "active".into(),
        input: vec![],
    };
    *host.models.lock() = vec![
        ("chat", model("override", &["fast"])),
        ("chat", model("plain", &[])),
        ("image", model("painter", &[])),
        ("decision", model("classifier", &[])),
    ];
    let ctx = context(dir.path(), host.clone());
    let tool = tools::subagent::SubagentTool;
    let failures = [
        ("test/missing", "Model \"test/missing\" is not available. Use the models tool to see what is available."),
        ("test/override#slow", "Variant \"slow\" is not available for \"test/override\". Available: fast."),
        ("test/plain#high", "Model \"test/plain\" has no variants. Omit the variant."),
        (
            "test/painter",
            "Model \"test/painter\" is an image model; subagents need a chat model. Use the models tool to find one.",
        ),
        (
            "test/classifier",
            "Model \"test/classifier\" is a decision model; subagents need a chat model. Use the models tool to find one.",
        ),
    ];
    for (input, message) in failures {
        assert_eq!(tool.resolve_model(&ctx, input).await.unwrap_err().message, message);
    }
    assert_eq!(tool.resolve_model(&ctx, "test/override#fast").await.unwrap().variant.as_deref(), Some("fast"));
    let background = tools::subagent::background_text("ses_child");
    assert!(background.ends_with(
        "\nTo send it follow-ups or change its instructions while it runs, call subagent again with this sessionID."
    ));
}

// tool-skill.test.ts (2.3.0, #63623): the skill tool returns `{directory, content}`; the name is metadata.
#[tokio::test]
async fn skill_tool_returns_directory_and_content() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let ctx = context(dir.path(), host.clone());
    let skill = crate::host::SkillInfo {
        id: "effect".into(),
        name: "Effect".into(),
        description: None,
        path: "skill://effect/SKILL.md".into(),
        content: "Body".into(),
        files: vec![("references/a.md".into(), "A".into())],
    };
    host.skills.lock().push(skill.clone());
    let ok = run(&tools::skill::SkillTool, json!({"id": "effect"}), ctx).await.unwrap();
    let content = tools::skill::to_model_output(&skill, &["skill://effect/references/a.md".into()]);
    assert_eq!(ok.output, Some(json!({"directory": "skill://effect", "content": content})));
    assert_eq!(text(&ok), content);
    assert_eq!(ok.metadata, Some(crate::tool::meta(vec![("name", json!("Effect")), ("directory", json!("skill://effect"))])));
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
    // tool-write.test.ts (2.3.0, #63616): the confirmation is the structured output.
    assert_eq!(ok.output, Some(json!("Created file successfully: sub/x.txt")));
    let ok = run(&tool, json!({"path": "sub/x.txt", "content": "ho"}), ctx.clone()).await.unwrap();
    assert_eq!(text(&ok), "Wrote file successfully: sub/x.txt");
    assert_eq!(ok.output, Some(json!("Wrote file successfully: sub/x.txt")));
    assert!(ok.metadata.is_none());
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
    // Permission resources always use `/`.
    assert_eq!(requests[0].resources, vec![format!("{}/*", outside.to_string_lossy().replace('\\', "/"))]);
    assert_eq!(requests[1].action, "read");
    assert_eq!(requests[1].resources, vec![path.replace('\\', "/")]);
    assert_eq!(ok.metadata.as_ref().unwrap()["truncated"], json!(false));
    // tool-read.test.ts (2.3.1, #64361): whole-file text is the `text` shape.
    assert_eq!(ok.output, Some(json!({"type": "text", "content": "data\n", "offset": 1, "lineCount": 1})));
}

// tool-read.test.ts "rejects a zero limit before reading" (2.3.1, #63808).
#[tokio::test]
async fn read_rejects_a_zero_limit_before_reading() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.md"), "hello").unwrap();
    let host = Arc::new(MockHost::default());
    let ctx = context(dir.path(), host.clone());
    let error = run(&tools::read::ReadTool, json!({"path": "README.md", "limit": 0}), ctx.clone()).await.unwrap_err();
    assert_eq!(error.model_message(), "Limit must be a positive integer");
    assert!(host.requests.lock().is_empty());
    // A bounded page reports itself truncated in the metadata.
    std::fs::write(dir.path().join("lines.txt"), "one\ntwo\nthree\n").unwrap();
    let ok = run(&tools::read::ReadTool, json!({"path": "lines.txt", "limit": 1}), ctx).await.unwrap();
    assert_eq!(ok.output, Some(json!({"type": "text", "content": "one", "offset": 1, "lineCount": 1, "next": 2})));
    assert_eq!(ok.metadata.unwrap()["truncated"], json!(true));
}

// tool-read.test.ts "reads virtual skill:// files and directories from Skill.Service without external_directory
// approval" (#60921).
#[tokio::test]
async fn read_skill_uris() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let skill = |id: &str, path: &str, content: &str, files: Vec<(String, String)>| crate::host::SkillInfo {
        id: id.into(),
        name: id.into(),
        description: None,
        path: path.into(),
        content: content.into(),
        files,
    };
    host.skills.lock().push(skill(
        "guide",
        "skill://guide/SKILL.md",
        "# Guide\nRead references/style.md",
        vec![("references/style.md".into(), "Line 1\nLine 2".into())],
    ));
    host.skills.lock().push(skill("disk", "/somewhere/disk/SKILL.md", "Disk", vec![]));
    let ctx = context(dir.path(), host.clone());
    let read = |input: Value| run(&tools::read::ReadTool, input, ctx.clone());

    let file = read(json!({"path": "skill://guide/references/style.md", "offset": 2, "limit": 1})).await.unwrap();
    assert_eq!(text(&file), "Read file skill://guide/references/style.md, lines 2-2\n2: Line 2");
    let root = read(json!({"path": "skill://guide/"})).await.unwrap();
    assert_eq!(text(&root), "Read directory skill://guide/, entries 1-2\nreferences/\nSKILL.md");
    for bad in ["skill://", "skill://guide/../SKILL.md", "skill://guide/missing.md", "skill://unknown", "skill://disk/SKILL.md"] {
        let error = read(json!({"path": bad})).await.unwrap_err();
        assert_eq!(error.model_message(), format!("File not found: {bad}"), "{bad}");
    }
    let requests: Vec<(String, Vec<String>, Option<Vec<String>>)> =
        host.requests.lock().iter().map(|r| (r.action.clone(), r.resources.clone(), r.save.clone())).collect();
    let pair = |uri: &str| {
        [
            ("skill".to_string(), vec!["guide".to_string()], Some(vec!["guide".to_string()])),
            ("read".to_string(), vec![uri.to_string()], Some(vec!["*".to_string()])),
        ]
    };
    assert_eq!(requests, [pair("skill://guide/references/style.md"), pair("skill://guide"), pair("skill://guide/missing.md")].concat());

    *host.denied_resource.lock() = Some("guide".into());
    let error = read(json!({"path": "skill://guide/SKILL.md"})).await.unwrap_err();
    assert_eq!((error.to_session_error().kind.as_str(), error.model_message().as_str()), ("permission.rejected", "Permission denied: skill"));
    *host.denied_resource.lock() = Some("skill://guide/SKILL.md".into());
    for path in ["skill://guide/SKILL.md", "skill://guide/SKILL.md/"] {
        let error = read(json!({"path": path})).await.unwrap_err();
        assert_eq!((error.to_session_error().kind.as_str(), error.model_message().as_str()), ("permission.rejected", "Permission denied: read"));
    }
}

// tool-webfetch.test.ts "matches rules against the URL as written without the fragment and rejects credentials"
// (#60502, #61701).
#[tokio::test]
async fn webfetch_drops_the_fragment_and_rejects_credentials() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = requests.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            seen.lock().push(request.lines().next().unwrap_or_default().to_string());
            let response = if request.starts_with("GET /hop ") {
                format!("HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:{port}/page#frag\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            } else if request.starts_with("GET /creds ") {
                "HTTP/1.1 302 Found\r\nlocation: http://user:pw@127.0.0.1/x\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string()
            } else {
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok".to_string()
            };
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let ctx = context(dir.path(), host.clone());
    let tool = tools::webfetch::WebFetchTool::new();
    let url = format!("http://127.0.0.1:{port}/#top");
    let ok = run(&tool, json!({"url": url, "format": "text"}), ctx.clone()).await.unwrap();
    assert_eq!(text(&ok), "ok");
    // The rule resource is the URL as written, without the fragment; the metadata keeps the input.
    let request = host.requests.lock()[0].clone();
    assert_eq!(request.resources, vec![format!("http://127.0.0.1:{port}/")]);
    assert_eq!(request.metadata.as_ref().unwrap()["url"], json!(url));
    // 2.3.2 (#64534): the structured output no longer repeats the `url` and `format` it was called with.
    assert_eq!(ok.output.as_ref().unwrap(), &json!({"contentType": "text/plain", "output": "ok"}));

    // A redirect target is checked without its fragment.
    run(&tool, json!({"url": format!("http://127.0.0.1:{port}/hop"), "format": "text"}), ctx.clone()).await.unwrap();
    assert_eq!(requests.lock().last().unwrap(), "GET /page HTTP/1.1");

    let sent = requests.lock().len();
    let error = run(&tool, json!({"url": "https://user@example.com/", "format": "text"}), ctx.clone()).await.unwrap_err();
    assert_eq!((error.message.as_str(), error.model_message().as_str()), ("Unable to fetch https://user@example.com/", "URL must not include credentials"));
    let error = run(&tool, json!({"url": format!("http://127.0.0.1:{port}/creds"), "format": "text"}), ctx).await.unwrap_err();
    assert_eq!(error.model_message(), "URL must not include credentials");
    assert_eq!(requests.lock().len(), sent + 1);
}

// tool-read.test.ts "suggests a path written from the parent of the location first" (#62147).
#[tokio::test]
async fn read_suggests_the_rerooted_path_first() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path();
    let repo = tmp.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src").join("a.ts"), "x\n").unwrap();
    let host = Arc::new(MockHost::default());
    *host.allows.lock() = true;
    let ctx = context(&repo, host.clone());
    let requested = tmp.join("src").join("a.ts").to_string_lossy().into_owned();
    let error = run(&tools::read::ReadTool, json!({"path": requested}), ctx.clone()).await.unwrap_err();
    assert_eq!(
        error.model_message(),
        format!(
            "File not found: {requested}\n\nThe folder {} does not exist. The nearest folder that does is {}.\n\nDid you mean one of these?\n{}",
            tmp.join("src").display(),
            tmp.display(),
            repo.join("src").join("a.ts").display()
        )
    );
    std::fs::create_dir_all(tmp.join("src")).unwrap();
    std::fs::write(tmp.join("src").join("a.tsx"), "x\n").unwrap();
    let error = run(&tools::read::ReadTool, json!({"path": requested}), ctx.clone()).await.unwrap_err();
    assert_eq!(
        error.model_message(),
        format!(
            "File not found: {requested}\n\nDid you mean one of these?\n{}\n{}",
            repo.join("src").join("a.ts").display(),
            tmp.join("src").join("a.tsx").display()
        )
    );
    // Without read approval for the rerooted path, it is not suggested.
    *host.allows.lock() = false;
    let error = run(&tools::read::ReadTool, json!({"path": requested}), ctx).await.unwrap_err();
    assert_eq!(error.model_message(), format!("File not found: {requested}\n\nDid you mean one of these?\n{}", tmp.join("src").join("a.tsx").display()));
}

// tool-patch.test.ts "preserves files across self-moves and multi-hunk moves in one patch"
#[tokio::test]
async fn patch_self_moves_and_multi_hunk_moves() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let ctx = context(dir.path(), host.clone());
    std::fs::write(dir.path().join("same.txt"), "before\n").unwrap();
    std::fs::write(dir.path().join("old.txt"), "a\nb\nc\n").unwrap();
    let tool = tools::patch::PatchTool::new(Arc::new(Formatter::new()));
    let patch = [
        "*** Begin Patch",
        "*** Update File: same.txt",
        "*** Move to: ./same.txt",
        "@@",
        "-before",
        "+after",
        "*** Update File: old.txt",
        "@@",
        "-a",
        "+A",
        "*** Update File: old.txt",
        "*** Move to: moved.txt",
        "@@",
        "-b",
        "+B",
        "*** Update File: moved.txt",
        "@@",
        "-c",
        "+C",
        "*** End Patch",
    ]
    .join("\n");
    run(&tool, json!({"patchText": patch}), ctx.clone()).await.unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("same.txt")).unwrap(), "after\n");
    assert!(!dir.path().join("old.txt").exists());
    assert_eq!(std::fs::read_to_string(dir.path().join("moved.txt")).unwrap(), "A\nB\nC\n");

    // Later hunks see files an earlier hunk deleted or added.
    let gone = ["*** Begin Patch", "*** Delete File: same.txt", "*** Delete File: same.txt", "*** End Patch"].join("\n");
    let error = run(&tool, json!({"patchText": gone}), ctx.clone()).await.unwrap_err();
    assert_eq!(error.message, "patch verification failed: Failed to delete same.txt: file does not exist");
    let update = ["*** Begin Patch", "*** Delete File: same.txt", "*** Update File: same.txt", "@@", "-after", "+x", "*** End Patch"].join("\n");
    let error = run(&tool, json!({"patchText": update}), ctx.clone()).await.unwrap_err();
    let absolute = dir.path().join("same.txt");
    assert_eq!(
        error.message,
        format!("patch verification failed: Failed to read file to update {}: file does not exist", absolute.display())
    );
    let added = ["*** Begin Patch", "*** Add File: new.txt", "+one", "*** Update File: new.txt", "@@", "-one", "+two", "*** End Patch"].join("\n");
    run(&tool, json!({"patchText": added}), ctx.clone()).await.unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("new.txt")).unwrap(), "two\n");
    assert_eq!(std::fs::read_to_string(dir.path().join("same.txt")).unwrap(), "after\n");
}

/// "// José" in Latin-1, far from the edited line.
fn latin1_source() -> Vec<u8> {
    [b"// Jos\xe9\n".as_slice(), b"int MAX = 10;\n"].concat()
}

// tool-edit.test.ts (2.4.1, #64868): a file that is not valid UTF-8 is not rewritten.
#[tokio::test]
async fn edit_refuses_a_file_that_is_not_valid_utf8() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let ctx = context(dir.path(), host.clone());
    let tool = tools::edit::EditTool::new(Arc::new(Formatter::new()));
    let target = dir.path().join("Invoice.java");
    std::fs::write(&target, latin1_source()).unwrap();
    let error = run(&tool, json!({"path": "Invoice.java", "oldString": "MAX = 10", "newString": "MAX = 25"}), ctx).await.unwrap_err();
    assert!(error.model_message().contains("Unable to edit Invoice.java: file is not valid UTF-8."), "{}", error.model_message());
    assert_eq!(std::fs::read(&target).unwrap(), latin1_source());
}

// tool-patch.test.ts (2.4.1, #64868): a patch touching a file that is not valid UTF-8 applies none of it; deleting
// such a file does not rewrite its bytes, so it stays allowed.
#[tokio::test]
async fn patch_refuses_a_file_that_is_not_valid_utf8_and_applies_none_of_the_patch() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let ctx = context(dir.path(), host.clone());
    let tool = tools::patch::PatchTool::new(Arc::new(Formatter::new()));
    let (utf8, latin1, doomed) = (dir.path().join("a.txt"), dir.path().join("Invoice.java"), dir.path().join("Old.java"));
    std::fs::write(&utf8, "before\n").unwrap();
    std::fs::write(&latin1, latin1_source()).unwrap();
    std::fs::write(&doomed, latin1_source()).unwrap();
    let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-before\n+after\n*** Update File: Invoice.java\n@@\n-int MAX = 10;\n+int MAX = 25;\n*** End Patch";
    let error = run(&tool, json!({"patchText": patch}), ctx.clone()).await.unwrap_err();
    let expected = format!("patch verification failed: Failed to read file to update {}: file is not valid UTF-8.", latin1.display());
    assert!(error.model_message().contains(&expected), "{}", error.model_message());
    assert_eq!(std::fs::read_to_string(&utf8).unwrap(), "before\n");
    assert_eq!(std::fs::read(&latin1).unwrap(), latin1_source());
    run(&tool, json!({"patchText": "*** Begin Patch\n*** Delete File: Old.java\n*** End Patch"}), ctx).await.unwrap();
    assert!(!doomed.exists());
}

// tool-patch.test.ts (2.3.0, #63655): a move that only changes case renames the file, and a move into a directory
// whose name differs only in case writes there.
#[tokio::test]
async fn patch_case_only_moves() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let ctx = context(dir.path(), host.clone());
    let tool = tools::patch::PatchTool::new(Arc::new(Formatter::new()));
    std::fs::write(dir.path().join("same.txt"), "before\n").unwrap();
    let patch = "*** Begin Patch\n*** Update File: same.txt\n*** Move to: Same.txt\n@@\n-before\n+after\n*** End Patch";
    let ok = run(&tool, json!({"patchText": patch}), ctx.clone()).await.unwrap();
    assert_eq!(text(&ok), "Success. Updated the following files:\nM Same.txt");
    let names: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["Same.txt"]);
    assert_eq!(std::fs::read_to_string(dir.path().join("Same.txt")).unwrap(), "after\n");

    std::fs::create_dir(dir.path().join("Foo")).unwrap();
    std::fs::write(dir.path().join("Foo").join("a.txt"), "before\n").unwrap();
    let patch = "*** Begin Patch\n*** Update File: Foo/a.txt\n*** Move to: foo/a.txt\n@@\n-before\n+after\n*** End Patch";
    run(&tool, json!({"patchText": patch}), ctx.clone()).await.unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("foo").join("a.txt")).unwrap(), "after\n");
}

// A case-only move onto a name that already exists (as on a case-insensitive file system) writes in place and
// renames instead of writing the new name and deleting the old one. A case-sensitive file system stands in for one
// with a second file under the new name; on a case-insensitive one (the macOS and Windows default) the new name is
// the file itself.
#[tokio::test]
async fn patch_case_only_move_onto_an_existing_name_renames() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let ctx = context(dir.path(), host.clone());
    let tool = tools::patch::PatchTool::new(Arc::new(Formatter::new()));
    std::fs::write(dir.path().join("note.txt"), "before\n").unwrap();
    let case_insensitive = dir.path().join("Note.txt").exists();
    if !case_insensitive {
        std::fs::write(dir.path().join("Note.txt"), "other\n").unwrap();
    }
    let patch = "*** Begin Patch\n*** Update File: note.txt\n*** Move to: Note.txt\n@@\n-before\n+after\n*** End Patch";
    run(&tool, json!({"patchText": patch}), ctx.clone()).await.unwrap();
    let names: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["Note.txt"], "case-insensitive: {case_insensitive}");
    assert_eq!(std::fs::read_to_string(dir.path().join("Note.txt")).unwrap(), "after\n");
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
    // tool-patch.test.ts (2.3.0, #63618): the structured output is empty; the files are metadata.
    assert_eq!(ok.output, Some(json!({})));
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

// tool-shell.test.ts "ShellTool stopped" (2.3.0, #62977): a call the user stops keeps the command's output.
#[cfg(unix)]
#[tokio::test]
async fn shell_tool_stopped_by_the_user() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let mut ctx = context(dir.path(), host.clone());
    ctx.config = Arc::new(ToolConfig { shell: Some("/bin/sh".into()), ..ToolConfig::default() });
    let service = crate::shell::service::ShellService::new(dir.path().join(".data"), None);
    let tool = tools::shell::ShellTool::new(service.clone());

    // Stopped once the command printed.
    let stop = crate::Stop::new();
    ctx.stop = stop.clone();
    let call = tokio::spawn({
        let ctx = ctx.clone();
        let tool = tools::shell::ShellTool::new(service.clone());
        async move { run(&tool, json!({"command": "printf 'first\\nsecond\\n'; sleep 60"}), ctx).await }
    });
    let info = loop {
        if let Some(info) = service.list().into_iter().next().filter(|i| service.output(&i.id, None, None).unwrap().size > 0) {
            break info;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert!(stop.trigger(), "the shell tool listens for its stop");
    let error = call.await.unwrap().unwrap_err();
    let notice = format!("[full output saved to {}]\n\nCommand interrupted by the user (shell ID: {}).", info.file, info.id);
    assert_eq!(error.to_session_error().kind, "tool.execution");
    assert_eq!(error.model_message(), format!("first\nsecond\n\n{notice}"));
    assert_eq!(
        serde_json::Value::Object(error.metadata.clone().unwrap()),
        json!({"interrupted": true, "truncated": false, "shellID": info.id, "notice": notice})
    );
    assert_eq!(service.wait(&info.id).await.unwrap().status, oc_schema::message::ShellStatus::Killed);
    assert_eq!(std::fs::read_to_string(&info.file).unwrap(), "first\nsecond\n");

    // Already stopped before the command started: it never starts.
    let stop = crate::Stop::new();
    let early = stop.listen().unwrap();
    stop.trigger();
    drop(early);
    ctx.stop = stop;
    let error = run(&tool, json!({"command": "printf started > started"}), ctx.clone()).await.unwrap_err();
    assert_eq!(error.model_message(), "Command interrupted by the user before it started.");
    assert!(!dir.path().join("started").exists());
    assert!(service.list().is_empty());
}

// tool-shell.test.ts "stops a command whose call an interrupt cut off, with no handoff to take it over" (2.3.0): an
// interrupted call is dropped wherever it waits, and the drop stops its command (it ends killed, not when it would
// have exited) and cancels its job.
#[cfg(unix)]
#[tokio::test]
async fn shell_tool_interrupted_stops_its_command() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let mut ctx = context(dir.path(), host.clone());
    ctx.config = Arc::new(ToolConfig { shell: Some("/bin/sh".into()), ..ToolConfig::default() });
    let service = crate::shell::service::ShellService::new(dir.path().join(".data"), None);
    let call = tokio::spawn({
        let tool = tools::shell::ShellTool::new(service.clone());
        async move { run(&tool, json!({"command": "printf 'server starting'; sleep 60"}), ctx).await }
    });
    let info = loop {
        if let Some(info) = service.list().into_iter().next().filter(|i| service.output(&i.id, None, None).unwrap().size > 0) {
            break info;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    let ended = tokio::time::timeout(std::time::Duration::from_secs(10), service.wait(&info.id)).await.expect("the command is stopped");
    assert_eq!(ended.unwrap().status, oc_schema::message::ShellStatus::Killed);
    assert_eq!(std::fs::read_to_string(&info.file).unwrap(), "server starting");
    for _ in 0..500 {
        if !host.cancelled_jobs.lock().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(*host.cancelled_jobs.lock(), vec![info.id.clone()]);
}

#[cfg(unix)]
#[tokio::test]
async fn shell_tool_flow() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let mut ctx = context(dir.path(), host.clone());
    ctx.config = Arc::new(ToolConfig { shell: Some("/bin/sh".into()), ..ToolConfig::default() });
    let service = crate::shell::service::ShellService::new(dir.path().join(".data"), None);
    let tool = tools::shell::ShellTool::new(service.clone());
    let ok = run(&tool, json!({"command": "echo hello && exit 2"}), ctx.clone()).await.unwrap();
    assert_eq!(ok.content, vec![ToolContent::Text { text: "hello\n".into() }, ToolContent::Text { text: "Exit code: 2".into() }]);
    // tool-shell.test.ts (2.3.0, #63647): the structured output has the exit code and the elapsed time.
    let output = ok.output.clone().unwrap();
    assert_eq!(output.as_object().unwrap().keys().collect::<Vec<_>>(), ["exitCode", "wallTimeSeconds", "output"]);
    assert_eq!((output["output"].clone(), output["exitCode"].clone()), (json!("hello\n"), json!(2)));
    assert!(output["wallTimeSeconds"].as_f64().unwrap() >= 0.0);
    let request = host.requests.lock()[0].clone();
    assert_eq!(request.action, "shell");
    assert_eq!(request.resources, vec!["echo hello", "exit 2"]);
    assert_eq!(request.save, Some(vec!["echo *".to_string(), "exit *".to_string()]));
    // tool-shell.test.ts (2.3.1, #63699): a foreground timeout moves the command to the background instead of
    // stopping it, and says so.
    let ok = run(&tool, json!({"command": "printf started; sleep 1; printf ' done'", "timeout": 10}), ctx.clone()).await.unwrap();
    let metadata = ok.metadata.clone().unwrap();
    let id = metadata["shellID"].as_str().unwrap().to_string();
    let pid = metadata["pid"].as_u64().unwrap();
    assert_eq!((metadata["status"].clone(), metadata["truncated"].clone()), (json!("running"), json!(false)));
    assert!(metadata.get("timeout").is_none());
    let output = ok.output.clone().unwrap();
    let file = output["outputPath"].as_str().unwrap().to_string();
    assert_eq!(output.as_object().unwrap().keys().collect::<Vec<_>>(), ["shellID", "exitCode", "outputPath", "pid", "wallTimeSeconds", "output"]);
    assert!(output["wallTimeSeconds"].as_f64().unwrap() >= 0.01);
    assert_eq!(
        ok.content,
        vec![
            ToolContent::Text {
                text: format!(
                    "Command moved to the background (shell ID: {id}).\nPID: {pid}\nOutput is streaming to: {file}\n\nThe command did not finish within its timeout of 0.01 seconds and was moved to the background. Do not start another copy while it is running."
                )
            },
            ToolContent::Text { text: tools::shell::BACKGROUND_INSTRUCTION.into() },
        ]
    );
    let finished = tokio::time::timeout(std::time::Duration::from_secs(60), service.wait(&id)).await.unwrap().unwrap();
    assert_eq!((finished.status, finished.exit), (oc_schema::message::ShellStatus::Exited, Some(0)));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "started done");
    // A fast foreground command still returns directly, and a timeout of 0 is the default one.
    let ok = run(&tool, json!({"command": "printf hello", "timeout": 0}), ctx.clone()).await.unwrap();
    assert_eq!(ok.content, vec![ToolContent::Text { text: "hello".into() }]);
    // An explicit timeout still stops a background command.
    let ok = run(&tool, json!({"command": "sleep 30", "background": true, "timeout": 50}), ctx.clone()).await.unwrap();
    let id = ok.metadata.unwrap()["shellID"].as_str().unwrap().to_string();
    let stopped = tokio::time::timeout(std::time::Duration::from_secs(60), service.wait(&id)).await.unwrap().unwrap();
    assert_eq!(stopped.status, oc_schema::message::ShellStatus::Timeout);

    // A background command reports its PID, output file and elapsed time (#63647).
    let ok = run(&tool, json!({"command": "sleep 0.2", "background": true}), ctx.clone()).await.unwrap();
    let metadata = ok.metadata.clone().unwrap();
    let pid = metadata["pid"].as_u64().unwrap();
    assert!(pid > 0);
    assert_eq!((metadata["status"].clone(), metadata["truncated"].clone()), (json!("running"), json!(false)));
    let output = ok.output.clone().unwrap();
    let file = output["outputPath"].as_str().unwrap().to_string();
    assert_eq!(output.as_object().unwrap().keys().collect::<Vec<_>>(), ["shellID", "exitCode", "outputPath", "pid", "wallTimeSeconds", "output"]);
    assert_eq!((output["exitCode"].clone(), output["pid"].clone(), output["shellID"].clone()), (json!(null), json!(pid), metadata["shellID"].clone()));
    assert_eq!(
        ok.content[0],
        ToolContent::Text {
            text: format!(
                "Command moved to the background (shell ID: {}).\nPID: {pid}\nOutput is streaming to: {file}",
                metadata["shellID"].as_str().unwrap()
            )
        }
    );
    let err = run(&tool, json!({"command": "ls", "workdir": "nope"}), ctx.clone()).await.unwrap_err();
    assert_eq!(err.message, "Unable to execute command: ls");
    assert_eq!(err.model_message(), format!("Working directory does not exist: {}", dir.path().join("nope").to_string_lossy()));
}

/// shell-daemon.test.ts (2.4.1, #64889): the output directory is created per command, so commands keep working
/// after it is removed while the location stays open.
#[cfg(unix)]
#[tokio::test]
async fn shell_tool_recreates_a_removed_output_directory() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let mut ctx = context(dir.path(), host.clone());
    ctx.config = Arc::new(ToolConfig { shell: Some("/bin/sh".into()), ..ToolConfig::default() });
    let service = crate::shell::service::ShellService::new(dir.path().join(".data"), None);
    let tool = tools::shell::ShellTool::new(service.clone());
    run(&tool, json!({"command": "printf one"}), ctx.clone()).await.unwrap();
    let output = service.output_dir(&ctx.project_id);
    assert!(output.is_dir());
    std::fs::remove_dir_all(&output).unwrap();
    let ok = run(&tool, json!({"command": "printf two"}), ctx.clone()).await.unwrap();
    assert_eq!(ok.content, vec![ToolContent::Text { text: "two".into() }]);
    assert!(output.is_dir());
}

/// tool-shell.test.ts (2.4.1, #65269): commands the scanner can't analyze.
#[cfg(unix)]
#[tokio::test]
async fn shell_tool_unanalyzable_commands() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let mut ctx = context(dir.path(), host.clone());
    ctx.config = Arc::new(ToolConfig { shell: Some("/bin/sh".into()), ..ToolConfig::default() });
    let service = crate::shell::service::ShellService::new(dir.path().join(".data"), None);
    let tool = tools::shell::ShellTool::new(service);
    let marker = dir.path().join("marker");
    let command = "alias p=true; printf hello > marker";
    let rule = |action: &str, resource: &str, effect: Effect| PermissionRule { action: action.into(), resource: resource.into(), effect };

    // Denied when a shell deny rule could be bypassed.
    *host.rules.lock() = vec![rule("*", "*", Effect::Allow), rule("shell", "rm *", Effect::Deny)];
    let err = run(&tool, json!({"command": command}), ctx.clone()).await.unwrap_err();
    assert!(
        err.model_message().contains("Portable shell scanner cannot analyze command: dynamic-execution. A shell deny rule is configured"),
        "{}",
        err.model_message()
    );
    assert!(host.requests.lock().is_empty());
    assert!(!marker.exists());

    // Otherwise asks for the exact command and any directory, without saving.
    *host.rules.lock() = vec![rule("shell", "alias *", Effect::Allow)];
    run(&tool, json!({"command": command}), ctx.clone()).await.unwrap();
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "hello");
    let requests: Vec<_> = host.requests.lock().drain(..).map(|r| (r.action, r.resources, r.save, r.metadata.map(Value::Object))).collect();
    assert_eq!(
        requests,
        vec![
            ("external_directory".to_string(), vec!["*".to_string()], None, None),
            ("shell".to_string(), vec![command.to_string()], None, Some(json!({"reason": "dynamic-execution"}))),
        ]
    );

    // Malformed syntax asks too, and a rejected approval runs nothing.
    std::fs::remove_file(&marker).unwrap();
    let malformed = "printf hello > marker\necho \"";
    *host.denied_resource.lock() = Some(malformed.to_string());
    let err = run(&tool, json!({"command": malformed}), ctx.clone()).await.unwrap_err();
    assert!(err.model_message().contains("Permission denied: shell"), "{}", err.model_message());
    let requests: Vec<_> = host.requests.lock().drain(..).map(|r| (r.action, r.resources, r.save, r.metadata.map(Value::Object))).collect();
    assert_eq!(
        requests,
        vec![
            ("external_directory".to_string(), vec!["*".to_string()], None, None),
            ("shell".to_string(), vec![malformed.to_string()], None, Some(json!({"reason": "unterminated-quote"}))),
        ]
    );
    assert!(!marker.exists());
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
    assert_eq!(names, vec!["edit", "experimental_image_generate", "glob", "grep", "patch", "question", "read", "shell", "skill", "subagent", "todowrite", "websearch", "write"]);
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

/// tool-shell.test.ts (2.3.1, #64284/#64335): with `full_codemode`, `tools.shell` waits 10 minutes by default, takes
/// no `background`, and a shorter timeout still moves the command to the background.
#[cfg(unix)]
#[tokio::test]
async fn code_mode_shell_has_the_long_default_and_no_background() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(MockHost::default());
    let mut ctx = context(dir.path(), host.clone());
    ctx.config = Arc::new(ToolConfig { shell: Some("/bin/sh".into()), ..ToolConfig::default() });
    let service = crate::shell::service::ShellService::new(dir.path().join(".data"), None);
    let shell: Arc<dyn Tool> = Arc::new(tools::shell::ShellTool::new(service.clone()));
    let tool = tools::code_mode::variant(&shell).unwrap();
    let options = tool.options();
    assert!(options.codemode && options.pinned);
    let ok = run(tool.as_ref(), json!({"command": "printf body; exit 7"}), ctx.clone()).await.unwrap();
    let output = ok.output.unwrap();
    assert_eq!((output["output"].clone(), output["exitCode"].clone()), (json!("body"), json!(7)));
    let ok = run(tool.as_ref(), json!({"command": "printf started; sleep 1; printf ' done'", "timeout": 10}), ctx.clone()).await.unwrap();
    let output = ok.output.unwrap();
    assert!(output["output"].as_str().unwrap().contains("did not finish within its timeout of 0.01 seconds"), "{output}");
    assert!(output["shellID"].as_str().unwrap().starts_with("sh_"), "{output}");
    assert_eq!(output["exitCode"], json!(null));
    let ok = run(tool.as_ref(), json!({"command": "printf zero", "timeout": 0}), ctx.clone()).await.unwrap();
    assert_eq!(ok.output.unwrap()["output"], json!("zero"));
    assert_eq!(*host.shell_timeouts.lock(), [600_000, 10, 600_000]);
    // A `background` key is not part of the input: the command runs in the foreground.
    let ok = run(tool.as_ref(), json!({"command": "printf fore", "background": true}), ctx.clone()).await.unwrap();
    let output = ok.output.unwrap();
    assert_eq!(output.as_object().unwrap().keys().collect::<Vec<_>>(), ["exitCode", "wallTimeSeconds", "output"]);
    assert_eq!(output["output"], json!("fore"));
    assert_eq!(host.shell_timeouts.lock().last(), Some(&600_000));
    // Without the flag the direct tool keeps its 2-minute default.
    run(shell.as_ref(), json!({"command": "printf direct"}), ctx.clone()).await.unwrap();
    assert_eq!(host.shell_timeouts.lock().last(), Some(&120_000));
}
