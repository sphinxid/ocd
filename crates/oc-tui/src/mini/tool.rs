//! Per-tool display rules (`src/mini/tool.ts`): visibility policy, inline summaries, scrollback text for
//! start/progress/final entries and structured snapshots.

use super::types::*;
use super::util::{canonical_tool_name, dict, finite_number, format_path, home_dir, primitive_input_summary, text, web_search_provider_label};
use crate::util::locale;
use crate::util::todo::{Todo, parse_todos, todo_marker};
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Snap {
    Code,
    Diff,
    Structured,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolView {
    pub output: bool,
    pub final_: bool,
    pub snap: Option<Snap>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiffItem {
    pub title: String,
    pub diff: String,
    pub file: Option<String>,
    pub deletions: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ToolSnapshot {
    Code { title: String, content: String, file: Option<String> },
    Diff { items: Vec<DiffItem> },
    Task { title: String, rows: Vec<String>, tail: String },
    Todo { items: Vec<Todo> },
    Question { items: Vec<(String, String)>, tail: String },
}

#[derive(Clone, Debug, PartialEq)]
pub enum EntryBody {
    None,
    Text(String),
    Code { content: String, filetype: Option<String> },
    Markdown(String),
    Structured(ToolSnapshot),
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ToolInline {
    pub icon: String,
    pub title: String,
    pub description: Option<String>,
    pub block: bool,
    pub body: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ToolFrame {
    pub directory: Option<String>,
    pub raw: String,
    pub name: String,
    pub input: Map<String, Value>,
    pub meta: Map<String, Value>,
    pub state: Map<String, Value>,
    pub status: String,
    pub error: String,
    pub output: String,
    pub start: Option<i64>,
    pub end: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolPhase {
    Start,
    Progress,
    Final,
}

impl From<Phase> for ToolPhase {
    fn from(p: Phase) -> Self {
        match p {
            Phase::Start => ToolPhase::Start,
            Phase::Progress => ToolPhase::Progress,
            Phase::Final => ToolPhase::Final,
        }
    }
}

/// `toolOutputText(name, content)`.
pub fn tool_output_text(name: &str, content: &[Value]) -> String {
    // V2 shell content appends model-only status after the user-visible command output.
    if canonical_tool_name(name) == "shell" {
        return content.iter().find(|c| c.get("type").and_then(|t| t.as_str()) == Some("text")).and_then(|c| c.get("text")).and_then(|t| t.as_str()).unwrap_or_default().to_string();
    }
    let joined = content
        .iter()
        .filter(|c| c.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|c| c.get("text").and_then(|t| t.as_str()).filter(|t| !t.is_empty()))
        .collect::<Vec<_>>()
        .join("\n");
    if canonical_tool_name(name) == "read" {
        return read_display_text(&joined).unwrap_or(joined);
    }
    joined
}

/// `readDisplayText`: unwrap Read's JSON page envelope.
pub fn read_display_text(text: &str) -> Option<String> {
    if !text.starts_with('{') {
        return None;
    }
    let parsed: Value = serde_json::from_str(text).ok()?;
    let envelope = dict(Some(&parsed));
    if let Some(Value::String(content)) = envelope.get("content")
        && (envelope.get("type").and_then(|t| t.as_str()) == Some("text-page") || envelope.get("encoding").and_then(|t| t.as_str()) == Some("utf8"))
    {
        return Some(content.clone());
    }
    let entries = envelope.get("entries")?.as_array()?;
    Some(
        entries
            .iter()
            .filter_map(|e| match e {
                Value::String(s) => Some(s.clone()),
                other => other.get("path").and_then(|p| p.as_str()).map(|s| s.to_string()),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn normalize_input(name: &str, value: &Value) -> Map<String, Value> {
    let mut input = dict(Some(value));
    let path = match input.get("path") {
        Some(Value::String(s)) => s.clone(),
        _ => text(input.get("filePath")),
    };
    let agent = match input.get("agent") {
        Some(Value::String(s)) => s.clone(),
        _ => text(input.get("subagent_type")),
    };
    if ["read", "write", "edit", "lsp"].contains(&name) && !path.is_empty() {
        input.insert("path".into(), Value::String(path));
    }
    if name == "subagent" && !agent.is_empty() {
        input.insert("agent".into(), Value::String(agent));
    }
    input
}

fn normalize_file(value: &Value) -> Option<Map<String, Value>> {
    let file = dict(Some(value));
    let name = [text(file.get("file")), text(file.get("relativePath")), text(file.get("filePath"))].into_iter().find(|s| !s.is_empty())?;
    let legacy = text(file.get("type"));
    let status = {
        let s = text(file.get("status"));
        if !s.is_empty() {
            s
        } else {
            match legacy.as_str() {
                "add" => "added".into(),
                "delete" => "deleted".into(),
                "update" => "modified".into(),
                "move" => "moved".into(),
                _ => legacy.clone(),
            }
        }
    };
    let patch = file.get("patch").and_then(|p| p.as_str()).map(|s| s.to_string());
    let deletions = finite_number(file.get("deletions"));
    let mut out = file.clone();
    out.insert("file".into(), Value::String(name));
    if status == "moved" && !text(file.get("filePath")).is_empty() {
        out.insert("from".into(), Value::String(text(file.get("filePath"))));
    }
    if !status.is_empty() {
        out.insert("status".into(), Value::String(status));
    }
    if let Some(p) = patch {
        out.insert("patch".into(), Value::String(p));
    }
    if let Some(d) = deletions {
        out.insert("deletions".into(), serde_json::json!(d));
    }
    Some(out)
}

fn normalize_metadata(name: &str, value: &Value) -> Map<String, Value> {
    let mut metadata = dict(Some(value));
    let files: Vec<Value> = metadata.get("files").and_then(|f| f.as_array()).map(|a| a.iter().filter_map(normalize_file).map(Value::Object).collect()).unwrap_or_default();
    let session_id = {
        let s = text(metadata.get("sessionID"));
        if s.is_empty() { text(metadata.get("sessionId")) } else { s }
    };
    if ["edit", "patch"].contains(&name) && metadata.get("files").is_some_and(|f| f.is_array()) {
        metadata.insert("files".into(), Value::Array(files));
    }
    if name == "subagent" && !session_id.is_empty() {
        metadata.insert("sessionID".into(), Value::String(session_id));
    }
    metadata
}

/// `toolDisplayMetadata(state)`.
fn tool_display_metadata(part: &ToolPart) -> Value {
    if part.status == ToolStatus::Streaming {
        return Value::Object(Map::new());
    }
    match &part.metadata {
        Value::Object(m) => Value::Object(m.clone()),
        _ => Value::Object(Map::new()),
    }
}

/// `toolDisplayContent(state)`.
pub fn tool_display_content(part: &ToolPart) -> &[Value] {
    match part.status {
        ToolStatus::Streaming | ToolStatus::Running => &[],
        _ => &part.content,
    }
}

/// `normalizeTool(tool)`.
pub fn normalize_tool(tool: &ToolPart) -> ToolPart {
    let name = canonical_tool_name(&tool.name).to_string();
    if tool.status == ToolStatus::Streaming {
        return ToolPart { name, ..tool.clone() };
    }
    ToolPart {
        input: Value::Object(normalize_input(&name, &tool.input)),
        metadata: Value::Object(normalize_metadata(&name, &tool_display_metadata(tool))),
        name,
        ..tool.clone()
    }
}

fn list(v: Option<&Value>) -> Vec<Value> {
    v.and_then(|v| v.as_array()).cloned().unwrap_or_default()
}

fn span(frame: &ToolFrame) -> String {
    match (frame.start, frame.end) {
        (Some(s), Some(e)) if e > s => locale::duration(e - s),
        _ => String::new(),
    }
}

fn tool_error(ctx: &ToolFrame) -> String {
    if !ctx.error.is_empty() {
        return ctx.error.clone();
    }
    let state = text(ctx.state.get("error")).trim().to_string();
    if !state.is_empty() {
        return state;
    }
    ctx.raw.trim().to_string()
}

fn fail(ctx: &ToolFrame) -> String {
    let error = tool_error(ctx);
    if !error.is_empty() { format!("✖ {} failed: {error}", ctx.name) } else { format!("✖ {} failed", ctx.name) }
}

fn fallback_start(ctx: &ToolFrame) -> String {
    let extra = primitive_input_summary(&ctx.input, &[]);
    if extra.is_empty() { format!("⚙ {}", ctx.name) } else { format!("⚙ {} {extra}", ctx.name) }
}

fn fallback_final(ctx: &ToolFrame) -> String {
    if ctx.status == "error" {
        return fail(ctx);
    }
    if !ctx.status.is_empty() && ctx.status != "completed" {
        return ctx.raw.trim().to_string();
    }
    let time = span(ctx);
    if time.is_empty() { format!("{} completed", ctx.name) } else { format!("{} completed · {time}", ctx.name) }
}

/// `toolPath(input, {home, directory})`.
pub fn tool_path(input: Option<&str>, home: bool, directory: Option<&str>) -> String {
    let base = directory.map(|d| d.to_string()).unwrap_or_else(super::util::cwd);
    let h = home.then(home_dir);
    format_path(input, &base, h.as_deref())
}

fn display_path(p: &ToolFrame, input: Option<&str>, home: bool) -> String {
    tool_path(input, home, p.directory.as_deref())
}

fn input_str(p: &ToolFrame, key: &str) -> Option<String> {
    p.input.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn fallback_inline(ctx: &ToolFrame) -> ToolInline {
    let title = if !ctx.input.is_empty() { serde_json::to_string(&Value::Object(ctx.input.clone())).unwrap_or_default() } else { "Unknown".into() };
    ToolInline { icon: "⚙".into(), title: format!("{} {title}", ctx.name), ..Default::default() }
}

fn count(n: f64, label: &str) -> String {
    format!("{} {label}{}", locale::js_number(n), if n == 1.0 { "" } else { "es" })
}

fn run_search(p: &ToolFrame, kind: &str, key: &str) -> ToolInline {
    let root = input_str(p, "path").unwrap_or_default();
    let title = format!("{kind} \"{}\"", input_str(p, "pattern").unwrap_or_default());
    let suffix = if root.is_empty() { String::new() } else { format!("in {}", display_path(p, Some(&root), false)) };
    let description = match p.meta.get(key).and_then(|v| v.as_f64()) {
        None => suffix,
        Some(m) => format!("{suffix}{}{}", if suffix.is_empty() { "" } else { " · " }, count(m, "match")),
    };
    ToolInline { icon: "✱".into(), title, description: Some(description).filter(|d| !d.is_empty()), ..Default::default() }
}

fn run_inline(name: &str, p: &ToolFrame) -> Option<ToolInline> {
    Some(match name {
        "invalid" => ToolInline { icon: "✗".into(), title: "Invalid Tool".into(), block: true, body: (p.status == "completed").then(|| p.output.clone()), ..Default::default() },
        "shell" => ToolInline {
            icon: "$".into(),
            title: input_str(p, "command").unwrap_or_default(),
            block: true,
            body: (p.status == "completed").then(|| p.output.trim().to_string()),
            ..Default::default()
        },
        "write" => ToolInline {
            icon: "←".into(),
            title: format!("Write {}", display_path(p, input_str(p, "path").as_deref(), false)),
            block: true,
            body: (p.status == "completed").then(|| p.output.clone()),
            ..Default::default()
        },
        "edit" => {
            let file = list(p.meta.get("files")).into_iter().next();
            let body = file.as_ref().and_then(|f| f.get("patch")).and_then(|v| v.as_str()).map(|s| s.to_string()).or_else(|| p.meta.get("diff").and_then(|d| d.as_str()).map(|s| s.to_string()));
            ToolInline { icon: "←".into(), title: format!("Edit {}", display_path(p, input_str(p, "path").as_deref(), false)), block: true, body, ..Default::default() }
        }
        "patch" => {
            let files = p.meta.get("files").and_then(|f| f.as_array()).map(|a| a.len()).unwrap_or(0);
            if files == 0 {
                ToolInline { icon: "%".into(), title: "Patch".into(), ..Default::default() }
            } else {
                ToolInline { icon: "%".into(), title: format!("Patch {files} file{}", if files == 1 { "" } else { "s" }), ..Default::default() }
            }
        }
        "batch" => {
            let calls = list(p.input.get("tool_calls")).len();
            ToolInline {
                icon: "#".into(),
                title: if calls > 0 { format!("Batch {calls} tool{}", if calls == 1 { "" } else { "s" }) } else { "Batch".into() },
                block: true,
                body: (p.status == "completed").then(|| p.output.clone()),
                ..Default::default()
            }
        }
        "subagent" => {
            let kind = locale::titlecase(&input_str(p, "agent").filter(|s| !s.is_empty()).unwrap_or_else(|| "unknown".into()));
            let desc = input_str(p, "description").filter(|s| !s.is_empty());
            let icon = if p.status == "error" { "✗" } else if p.status == "running" { "•" } else { "✓" };
            ToolInline {
                icon: icon.into(),
                title: desc.clone().unwrap_or_else(|| format!("{kind} Subagent")),
                description: desc.map(|_| format!("{kind} Agent")),
                ..Default::default()
            }
        }
        "todowrite" => ToolInline {
            icon: "#".into(),
            title: "Todos".into(),
            block: true,
            body: Some(parse_todos(p.input.get("todos").unwrap_or(&Value::Null)).iter().map(|t| format!("{} {}", todo_marker(t.status, false), t.content)).collect::<Vec<_>>().join("\n")),
            ..Default::default()
        },
        "question" => {
            let total = list(p.input.get("questions")).len();
            ToolInline { icon: "→".into(), title: format!("Asked {total} question{}", if total == 1 { "" } else { "s" }), ..Default::default() }
        }
        "read" => {
            let file = display_path(p, input_str(p, "path").as_deref(), false);
            let description = primitive_input_summary(&p.input, &["path"]);
            ToolInline { icon: "→".into(), title: format!("Read {file}"), description: Some(description).filter(|d| !d.is_empty()), ..Default::default() }
        }
        "glob" => run_search(p, "Glob", "count"),
        "grep" => run_search(p, "Grep", "matches"),
        "list" => {
            let dir = text(p.input.get("path"));
            ToolInline { icon: "→".into(), title: if dir.is_empty() { "List".into() } else { format!("List {}", display_path(p, Some(&dir), false)) }, ..Default::default() }
        }
        "lsp" => ToolInline { icon: "→".into(), title: lsp_title(&p.input, p.directory.as_deref()), ..Default::default() },
        "webfetch" => {
            let url = input_str(p, "url").unwrap_or_default();
            ToolInline { icon: "%".into(), title: if url.is_empty() { "WebFetch".into() } else { format!("WebFetch {url}") }, ..Default::default() }
        }
        "websearch" => {
            let title = web_search_provider_label(p.meta.get("provider"));
            let query = input_str(p, "query").filter(|q| !q.is_empty());
            ToolInline { icon: "◈".into(), title: match query { Some(q) => format!("{title} \"{q}\""), None => title }, ..Default::default() }
        }
        "skill" => {
            let name = p.meta.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()).or_else(|| input_str(p, "id")).unwrap_or_default();
            ToolInline { icon: "→".into(), title: format!("Skill \"{name}\""), ..Default::default() }
        }
        _ => return None,
    })
}

fn lsp_title(input: &Map<String, Value>, directory: Option<&str>) -> String {
    let op = input.get("operation").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).unwrap_or("request");
    let file = input.get("path").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(|p| tool_path(Some(p), false, directory)).unwrap_or_default();
    let line = input.get("line").and_then(|v| v.as_f64());
    let ch = input.get("character").and_then(|v| v.as_f64());
    let pos = match (line, ch) {
        (Some(l), Some(c)) => format!(":{}:{}", locale::js_number(l), locale::js_number(c)),
        _ => String::new(),
    };
    if file.is_empty() { format!("LSP {op}") } else { format!("LSP {op} {file}{pos}") }
}

fn patch_title(file: &Value, directory: Option<&str>) -> String {
    let f = |k: &str| file.get(k).and_then(|v| v.as_str());
    match f("status") {
        Some("added") => format!("# Created {}", tool_path(f("file"), false, directory)),
        Some("deleted") => format!("# Deleted {}", tool_path(f("file"), false, directory)),
        Some("moved") => format!("# Moved {} -> {}", tool_path(f("from"), false, directory), tool_path(f("file"), false, directory)),
        _ => format!("# Patched {}", tool_path(f("file"), false, directory)),
    }
}

fn patch_line(file: &Value, directory: Option<&str>) -> String {
    let f = |k: &str| file.get(k).and_then(|v| v.as_str());
    match f("status") {
        Some("added") => format!("+ Created {}", tool_path(f("file"), false, directory)),
        Some("deleted") => format!("- Deleted {}", tool_path(f("file"), false, directory)),
        Some("moved") => format!("→ Moved {} → {}", tool_path(f("from"), false, directory), tool_path(f("file"), false, directory)),
        _ => format!("~ Patched {}", tool_path(f("file"), false, directory)),
    }
}

fn snap(name: &str, p: &ToolFrame) -> Option<ToolSnapshot> {
    match name {
        "write" => {
            let file = input_str(p, "path").unwrap_or_default();
            let content = input_str(p, "content").unwrap_or_default();
            if file.is_empty() && content.is_empty() {
                return None;
            }
            Some(ToolSnapshot::Code { title: format!("# Wrote {}", display_path(p, Some(&file), false)), content, file: Some(file) })
        }
        "edit" => {
            let item = list(p.meta.get("files")).into_iter().next();
            let file = item.as_ref().map(|i| text(i.get("file"))).filter(|s| !s.is_empty()).or_else(|| input_str(p, "path").filter(|s| !s.is_empty())).unwrap_or_default();
            let diff = item.as_ref().map(|i| text(i.get("patch"))).filter(|s| !s.is_empty()).unwrap_or_else(|| text(p.meta.get("diff")));
            if file.is_empty() || diff.trim().is_empty() {
                return None;
            }
            Some(ToolSnapshot::Diff { items: vec![DiffItem { title: format!("# Edited {}", display_path(p, Some(&file), false)), diff, file: Some(file), deletions: None }] })
        }
        "patch" => {
            let files = list(p.meta.get("files"));
            if files.is_empty() {
                return None;
            }
            let items: Vec<DiffItem> = files
                .iter()
                .filter(|f| f.is_object())
                .filter_map(|f| {
                    let diff = f.get("patch").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                    if diff.trim().is_empty() {
                        return None;
                    }
                    Some(DiffItem {
                        title: patch_title(f, p.directory.as_deref()),
                        diff,
                        file: f.get("file").and_then(|v| v.as_str()).map(|s| s.to_string()),
                        deletions: Some(f.get("deletions").and_then(|v| v.as_f64()).map(|d| d as i64).unwrap_or(0)),
                    })
                })
                .collect();
            if items.len() != files.len() {
                return None;
            }
            Some(ToolSnapshot::Diff { items })
        }
        "subagent" => {
            let kind = locale::titlecase(&input_str(p, "agent").filter(|s| !s.is_empty()).unwrap_or_else(|| "general".into()));
            let rows = input_str(p, "description").filter(|s| !s.is_empty()).into_iter().collect();
            Some(ToolSnapshot::Task { title: format!("# {kind} Subagent"), rows, tail: String::new() })
        }
        "todowrite" => Some(ToolSnapshot::Todo { items: parse_todos(p.input.get("todos").unwrap_or(&Value::Null)) }),
        "question" => {
            let answers = list(p.meta.get("answers"));
            let items = list(p.input.get("questions"))
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let answer: Vec<String> = list(answers.get(i)).iter().filter_map(|a| a.as_str().map(|s| s.to_string())).collect();
                    let question = item.get("question").and_then(|q| q.as_str()).filter(|q| !q.is_empty()).map(|s| s.to_string()).unwrap_or_else(|| format!("Question {}", i + 1));
                    (question, if answer.is_empty() { "(no answer)".to_string() } else { answer.join(", ") })
                })
                .collect();
            Some(ToolSnapshot::Question { items, tail: String::new() })
        }
        _ => None,
    }
}

fn scroll_bash_start(p: &ToolFrame) -> String {
    let cmd = input_str(p, "command").unwrap_or_default();
    let wd = input_str(p, "workdir").unwrap_or_default();
    let formatted = if !wd.is_empty() && wd != "." { display_path(p, Some(&wd), true) } else { String::new() };
    let dir = if formatted == "." { String::new() } else { formatted };
    if !cmd.is_empty() && dir.is_empty() {
        return format!("$ {cmd}");
    }
    if cmd.is_empty() {
        return if dir.is_empty() { String::new() } else { format!("{dir}$") };
    }
    format!("{dir}$ {cmd}")
}

fn trim_newlines(s: &str) -> &str {
    s.trim_start_matches('\n').trim_end_matches('\n')
}

fn scroll_bash_progress(p: &ToolFrame) -> String {
    let out = locale::strip_ansi(&p.raw);
    let cmd = input_str(p, "command").unwrap_or_default().trim().to_string();
    let fmt = |text: &str| {
        let body = trim_newlines(text);
        if body.is_empty() { String::new() } else { format!("\n{body}") }
    };
    if cmd.is_empty() {
        return out.trim_end_matches('\n').to_string();
    }
    let wd_raw = input_str(p, "workdir").unwrap_or_default().trim().to_string();
    let wd = if wd_raw.is_empty() { String::new() } else { display_path(p, Some(&wd_raw), true) };
    let lines: Vec<&str> = out.split('\n').collect();
    let first = lines.first().copied().unwrap_or("").trim();
    let second = lines.get(1).copied().unwrap_or("").trim();
    if !wd.is_empty() && (first == wd || first == wd_raw) && second == cmd {
        return fmt(&lines.get(2..).map(|l| l.join("\n")).unwrap_or_default());
    }
    if first == cmd || first == format!("$ {cmd}") {
        return fmt(&lines[1..].join("\n"));
    }
    if !wd.is_empty() && (first == format!("{wd} {cmd}") || first == format!("{wd_raw} {cmd}")) {
        return fmt(&lines[1..].join("\n"));
    }
    fmt(&out)
}

fn scroll_shell_final(p: &ToolFrame) -> String {
    if p.status == "error" {
        return fail(p);
    }
    let code = p.meta.get("exit");
    let time = span(p);
    match code {
        None => {
            if time.is_empty() {
                "shell completed".into()
            } else {
                format!("shell completed · {time}")
            }
        }
        Some(code) => format!("shell completed (exit {}){}", super::util::js_string(code), if time.is_empty() { String::new() } else { format!(" · {time}") }),
    }
}

fn scroll_patch_final(p: &ToolFrame) -> String {
    if p.status == "error" {
        return fail(p);
    }
    let files = list(p.meta.get("files"));
    if files.is_empty() {
        let time = span(p);
        return if time.is_empty() { "patch".into() } else { format!("patch · {time}") };
    }
    let status = |f: &Value| f.get("status").and_then(|s| s.as_str()).map(|s| s.to_string());
    let show_modified = !files.iter().any(|f| status(f).is_some_and(|s| !s.is_empty() && s != "modified"));
    let shown: Vec<&Value> = files.iter().filter(|f| show_modified || status(f).as_deref() != Some("modified")).collect();
    let mut rows: Vec<String> = shown.iter().take(6).map(|f| patch_line(f, p.directory.as_deref())).collect();
    if shown.len() > 6 {
        rows.push(format!("… and {} more", shown.len() - 6));
    }
    if !rows.is_empty() {
        return rows.join("\n");
    }
    patch_line(&files[0], p.directory.as_deref())
}

fn scroll_task_final(p: &ToolFrame) -> String {
    if p.status == "error" {
        return fail(p);
    }
    let kind = locale::titlecase(&input_str(p, "agent").filter(|s| !s.is_empty()).unwrap_or_else(|| "general".into()));
    match input_str(p, "description").filter(|s| !s.is_empty()) {
        None => format!("# {kind} Subagent"),
        Some(row) => format!("# {kind} Subagent\n{row}"),
    }
}

fn scroll_question_final(p: &ToolFrame) -> String {
    let q = list(p.input.get("questions"));
    let a = list(p.meta.get("answers"));
    let time = span(p);
    if q.is_empty() {
        return if time.is_empty() { "0 questions".into() } else { format!("0 questions · {time}") };
    }
    let mut rows = Vec::new();
    for (i, item) in q.iter().take(4).enumerate() {
        let prompt = item.get("question").and_then(|q| q.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string()).unwrap_or_else(|| format!("Question {}", i + 1));
        let reply: Vec<String> = list(a.get(i)).iter().map(super::util::js_string).collect();
        rows.push(format!("? {prompt}"));
        rows.push(format!("  {}", if reply.is_empty() { "(no answer)".to_string() } else { reply.join(", ") }));
    }
    if q.len() > 4 {
        rows.push(format!("… and {} more", q.len() - 4));
    }
    rows.join("\n")
}

fn scroll_search_start(p: &ToolFrame, kind: &str) -> String {
    let pattern = input_str(p, "pattern").unwrap_or_default();
    let head = if pattern.is_empty() { format!("✱ {kind}") } else { format!("✱ {kind} \"{pattern}\"") };
    let dir = input_str(p, "path").unwrap_or_default();
    if dir.is_empty() { head } else { format!("{head} in {}", display_path(p, Some(&dir), false)) }
}

/// `TOOL_RULES[name].scroll[phase]`: `None` when the rule has no hook for the phase (or no rule exists).
fn scroll_hook(name: &str, phase: ToolPhase, p: &ToolFrame) -> Option<String> {
    use ToolPhase::*;
    Some(match (name, phase) {
        ("invalid" | "batch" | "write" | "edit" | "patch" | "subagent" | "todowrite" | "question", Start) => String::new(),
        ("shell", Start) => scroll_bash_start(p),
        ("shell", Progress) => scroll_bash_progress(p),
        ("shell", Final) => scroll_shell_final(p),
        ("patch", Final) => scroll_patch_final(p),
        ("subagent", Final) => scroll_task_final(p),
        ("question", Final) => scroll_question_final(p),
        ("read", Start) => {
            let file = display_path(p, input_str(p, "path").as_deref(), false);
            let extra = primitive_input_summary(&p.input, &["path"]);
            format!("→ Read {file}{}", if extra.is_empty() { String::new() } else { format!(" {extra}") }).trim().to_string()
        }
        ("glob", Start) => scroll_search_start(p, "Glob"),
        ("glob", Final) => {
            let e = tool_error(p);
            if e.is_empty() { fail(p) } else { e }
        }
        ("grep", Start) => scroll_search_start(p, "Grep"),
        ("list", Start) => {
            let dir = text(p.input.get("path"));
            if dir.is_empty() { "→ List".into() } else { format!("→ List {}", display_path(p, Some(&dir), false)) }
        }
        ("lsp", Start) => format!("→ {}", lsp_title(&p.input, p.directory.as_deref())),
        ("webfetch", Start) => {
            let url = input_str(p, "url").unwrap_or_default();
            if url.is_empty() { "% WebFetch".into() } else { format!("% WebFetch {url}") }
        }
        ("websearch", Start) => {
            let title = web_search_provider_label(p.meta.get("provider"));
            let query = input_str(p, "query").unwrap_or_default();
            if query.is_empty() { format!("◈ {title}") } else { format!("◈ {title} \"{query}\"") }
        }
        ("skill", Start) => format!("→ Skill \"{}\"", p.meta.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()).or_else(|| input_str(p, "id")).unwrap_or_default()),
        _ => return None,
    })
}

const KNOWN: [&str; 17] = ["invalid", "shell", "write", "edit", "patch", "batch", "subagent", "todowrite", "question", "read", "glob", "grep", "list", "lsp", "webfetch", "websearch", "skill"];

fn rule_exists(name: &str) -> bool {
    KNOWN.contains(&name)
}

/// `toolView(name)`.
pub fn tool_view(name: Option<&str>) -> ToolView {
    let name = name.map(canonical_tool_name);
    let v = |output, final_, snap| ToolView { output, final_, snap };
    match name {
        Some("invalid" | "shell" | "batch") => v(true, false, None),
        Some("write") => v(false, true, Some(Snap::Code)),
        Some("edit" | "patch") => v(false, true, Some(Snap::Diff)),
        Some("subagent" | "todowrite" | "question") => v(false, true, Some(Snap::Structured)),
        Some("read" | "glob" | "grep" | "list" | "lsp" | "webfetch" | "websearch" | "skill") => v(false, false, None),
        _ => v(true, true, None),
    }
}

/// `toolStructuredFinal(commit)`.
pub fn tool_structured_final(commit: &StreamCommit) -> bool {
    let state = commit.tool_state.map(|s| s.as_str()).or_else(|| commit.part.as_ref().map(|p| p.status.as_str()));
    commit.kind == EntryKind::Tool && commit.phase == Phase::Final && state == Some("completed") && tool_view(commit.tool.as_deref().or(commit.part.as_ref().map(|p| p.name.as_str()))).snap.is_some()
}

fn frame(part: &ToolPart, directory: Option<&str>) -> ToolFrame {
    let tool = normalize_tool(part);
    if tool.status == ToolStatus::Streaming {
        return ToolFrame {
            directory: directory.map(|s| s.to_string()),
            raw: tool.input.as_str().unwrap_or_default().to_string(),
            name: tool.name.clone(),
            input: Map::new(),
            meta: Map::new(),
            state: tool.state_dict(),
            status: tool.status.as_str().into(),
            error: String::new(),
            output: String::new(),
            start: Some(tool.created),
            end: None,
        };
    }
    let output = tool_output_text(&tool.name, tool_display_content(&tool));
    ToolFrame {
        directory: directory.map(|s| s.to_string()),
        raw: output.clone(),
        name: tool.name.clone(),
        input: normalize_input(&tool.name, &tool.input),
        meta: normalize_metadata(&tool.name, &tool.metadata),
        state: tool.state_dict(),
        status: tool.status.as_str().into(),
        error: if tool.status == ToolStatus::Error { tool.error_message() } else { String::new() },
        output,
        start: Some(tool.ran.unwrap_or(tool.created)),
        end: tool.completed,
    }
}

/// `toolFrame(commit, raw)`.
pub fn tool_frame(commit: &StreamCommit, raw: &str) -> ToolFrame {
    let current = commit.part.as_ref().map(|p| frame(p, commit.directory.as_deref()));
    ToolFrame {
        directory: commit.directory.clone(),
        raw: raw.to_string(),
        name: canonical_tool_name(commit.tool.as_deref().filter(|t| !t.is_empty()).or(current.as_ref().map(|c| c.name.as_str())).unwrap_or("tool")).to_string(),
        input: current.as_ref().map(|c| c.input.clone()).unwrap_or_default(),
        meta: current.as_ref().map(|c| c.meta.clone()).unwrap_or_default(),
        state: current.as_ref().map(|c| c.state.clone()).unwrap_or_default(),
        status: commit.tool_state.map(|s| s.as_str().to_string()).or(current.as_ref().map(|c| c.status.clone())).unwrap_or_default(),
        error: commit.tool_error.clone().or(current.as_ref().map(|c| c.error.clone())).unwrap_or_default().trim().to_string(),
        output: current.as_ref().map(|c| c.output.clone()).unwrap_or_else(|| raw.to_string()),
        start: current.as_ref().and_then(|c| c.start),
        end: current.as_ref().and_then(|c| c.end),
    }
}

/// `toolInlineInfo(part, directory)`.
pub fn tool_inline_info(part: &ToolPart, directory: Option<&str>) -> ToolInline {
    let ctx = frame(part, directory);
    run_inline(&ctx.name.clone(), &ctx).unwrap_or_else(|| fallback_inline(&ctx))
}

/// `toolScroll(phase, ctx)`.
pub fn tool_scroll(phase: ToolPhase, ctx: &ToolFrame) -> String {
    if let Some(v) = scroll_hook(&ctx.name, phase, ctx) {
        return v;
    }
    match phase {
        ToolPhase::Start => fallback_start(ctx),
        ToolPhase::Progress => ctx.raw.clone(),
        ToolPhase::Final => fallback_final(ctx),
    }
}

fn text_body(content: String) -> Option<EntryBody> {
    if content.is_empty() { None } else { Some(EntryBody::Text(content)) }
}

const STRUCTURED_FALLBACK_LENGTH: usize = 4096;

fn structured_fallback(value: &Map<String, Value>) -> Option<EntryBody> {
    if value.is_empty() {
        return None;
    }
    let content = serde_json::to_string_pretty(&Value::Object(value.clone())).ok()?;
    let suffix = "\n… [truncated]";
    let units: Vec<u16> = content.encode_utf16().collect();
    let content = if units.len() <= STRUCTURED_FALLBACK_LENGTH {
        content
    } else {
        String::from_utf16_lossy(&units[..STRUCTURED_FALLBACK_LENGTH - suffix.encode_utf16().count()]) + suffix
    };
    Some(EntryBody::Code { content, filetype: Some("json".into()) })
}

fn shell_output(command: &str, raw: &str) -> Option<String> {
    let stripped = locale::strip_ansi(raw);
    let body = trim_newlines(&stripped);
    if body.is_empty() {
        return None;
    }
    if command.is_empty() {
        return Some(body.to_string());
    }
    Some(format!("\n{body}"))
}

fn task_result(output: &str) -> Option<String> {
    if output.trim().is_empty() {
        return None;
    }
    let re = regex::Regex::new(r"<task_result>\s*([\s\S]*?)\s*</task_result>").unwrap();
    if let Some(m) = re.captures(output) {
        let v = m[1].trim().to_string();
        return if v.is_empty() { None } else { Some(v) };
    }
    let next = output.split('\n').filter(|l| !l.starts_with("task_id:")).collect::<Vec<_>>().join("\n").trim().to_string();
    if next.is_empty() { None } else { Some(next) }
}

/// `toolEntryBody(commit, raw, options)`.
pub fn tool_entry_body(commit: &StreamCommit, raw: &str, shell_output_opt: Option<bool>) -> Option<EntryBody> {
    if let Some(command) = &commit.shell {
        return match commit.phase {
            Phase::Start => text_body(format!("$ {command}")),
            Phase::Progress => text_body(shell_output(command, raw).unwrap_or_default()),
            Phase::Final => {
                if commit.tool_state == Some(ToolStateKind::Error) {
                    let ctx = tool_frame(commit, raw);
                    return text_body(tool_scroll(ToolPhase::Final, &ctx));
                }
                None
            }
        };
    }
    let ctx = tool_frame(commit, raw);
    let view = tool_view(Some(&ctx.name));
    if ctx.name == "shell" && commit.phase == Phase::Progress && shell_output_opt == Some(false) {
        return None;
    }
    if ctx.name == "subagent" {
        if commit.phase == Phase::Start {
            return None;
        }
        if commit.phase == Phase::Final
            && ctx.status == "completed"
            && let Some(result) = task_result(&ctx.output)
        {
            return Some(EntryBody::Markdown(result));
        }
    }
    if commit.phase == Phase::Progress && !view.output {
        return None;
    }
    if commit.phase == Phase::Final {
        if ctx.status == "error" {
            return text_body(tool_scroll(ToolPhase::Final, &ctx));
        }
        if !view.final_ {
            return None;
        }
        if !ctx.status.is_empty() && ctx.status != "completed" {
            return text_body(ctx.raw.trim().to_string());
        }
        if tool_structured_final(commit) {
            return snap(&ctx.name, &ctx).map(EntryBody::Structured).or_else(|| text_body(tool_scroll(ToolPhase::Final, &ctx)));
        }
        if !rule_exists(&ctx.name) && ctx.output.trim().is_empty() {
            return structured_fallback(&ctx.meta).or_else(|| text_body(tool_scroll(ToolPhase::Final, &ctx)));
        }
    }
    text_body(tool_scroll(commit.phase.into(), &ctx))
}

/// `toolFiletype(input)`.
pub fn tool_filetype(input: Option<&str>) -> Option<String> {
    let input = input.filter(|s| !s.is_empty())?;
    let lang = super::util::language_extension(super::util::extname(input))?;
    if ["typescriptreact", "javascriptreact", "javascript"].contains(&lang) {
        return Some("typescript".into());
    }
    Some(lang.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `canonicalToolPart(name, state, id)` from the upstream fixture.
    pub fn part(name: &str, state: Value, id: Option<&str>) -> ToolPart {
        let status = state.get("status").and_then(|s| s.as_str()).unwrap_or("").to_string();
        let time = match status.as_str() {
            "streaming" => json!({"created": 1}),
            "completed" | "error" => json!({"created": 1, "ran": 1, "completed": 2}),
            _ => json!({"created": 1, "ran": 1}),
        };
        ToolPart::from_value(&json!({"type": "tool", "id": id.map(|s| s.to_string()).unwrap_or(format!("{name}-1")), "name": name, "state": state, "time": time}))
    }

    #[test]
    fn uses_v2_shell_output_without_model_status() {
        assert_eq!(tool_output_text("shell", &[json!({"type": "text", "text": "mini-output\n"}), json!({"type": "text", "text": "Command exited with code 0."})]), "mini-output\n");
        assert_eq!(tool_output_text("shell", &[json!({"type": "text", "text": ""}), json!({"type": "text", "text": "Command exited with code 0."})]), "");
    }

    #[test]
    fn normalizes_persisted_aliases() {
        let t = normalize_tool(&ToolPart::from_value(&json!({
            "type": "tool", "id": "call-patch", "name": "apply_patch",
            "state": {"status": "completed", "input": {"patchText": "*** Begin Patch\n*** End Patch"},
                "metadata": {"files": [{"type": "update", "filePath": "/tmp/project/src/a.ts", "relativePath": "src/a.ts", "patch": "@@ -1 +1 @@\n-old\n+new"}]},
                "content": [{"type": "text", "text": "patched"}]},
            "time": {"created": 1, "ran": 1, "completed": 2}
        })));
        assert_eq!(t.name, "patch");
        let f = &t.metadata["files"][0];
        assert_eq!(f["status"], "modified");
        assert_eq!(f["file"], "src/a.ts");
        assert_eq!(f["patch"], "@@ -1 +1 @@\n-old\n+new");
        assert_eq!(t.content, vec![json!({"type": "text", "text": "patched"})]);
        let t = normalize_tool(&ToolPart::from_value(&json!({
            "type": "tool", "id": "call-subagent", "name": "task",
            "state": {"status": "running", "input": {"subagent_type": "explore", "description": "Inspect"}, "metadata": {}},
            "time": {"created": 1, "ran": 1}
        })));
        assert_eq!(t.name, "subagent");
        assert_eq!(t.input["agent"], "explore");
    }

    #[test]
    fn skill_name_from_metadata() {
        let skill = |metadata: Value| part("skill", json!({"status": "completed", "input": {"id": "tigerstyle"}, "metadata": metadata, "content": [{"type": "text", "text": ""}]}), Some("call-skill"));
        assert_eq!(tool_inline_info(&skill(json!({"name": "effect"})), None).title, "Skill \"effect\"");
        assert_eq!(tool_inline_info(&skill(json!({})), None).title, "Skill \"tigerstyle\"");
        let ctx = ToolFrame {
            directory: Some("/work/project".into()),
            name: "skill".into(),
            input: json!({"id": "tigerstyle"}).as_object().unwrap().clone(),
            meta: json!({"name": "effect"}).as_object().unwrap().clone(),
            status: "completed".into(),
            ..Default::default()
        };
        assert_eq!(tool_scroll(ToolPhase::Start, &ctx), "→ Skill \"effect\"");
    }

    #[test]
    fn compact_search_metadata() {
        let glob = part("glob", json!({"status": "completed", "input": {"pattern": "*.ts"}, "metadata": {"count": 3}, "content": [{"type": "text", "text": ""}]}), None);
        assert_eq!(tool_inline_info(&glob, None).description.as_deref(), Some("3 matches"));
        let grep = part("grep", json!({"status": "completed", "input": {"pattern": "needle"}, "metadata": {"matches": 1}, "content": [{"type": "text", "text": ""}]}), None);
        assert_eq!(tool_inline_info(&grep, None).description.as_deref(), Some("1 match"));
    }

    #[test]
    fn segment_safe_paths() {
        assert_eq!(tool_path(Some("..cache/result.txt"), false, Some("/work/project")), "..cache/result.txt");
        assert_eq!(tool_path(Some("../shared/result.txt"), false, Some("/work/project")), "/work/shared/result.txt");
    }
}
