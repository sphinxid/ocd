//! `opencode run` (`handlers/run.ts`, `run/run.ts`, `run/noninteractive.ts`, `session-target.ts`):
//! a non-interactive prompt against the server, streaming SSE events to stdout/stderr.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use base64::Engine;
use futures::FutureExt;
use oc_util::Global;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::cli::parse::Input;
use crate::client::{Client, ClientError, EventStream, Req};
use crate::connection::{self, Args};
use crate::term::{EOL, write_stderr, write_stdout};

const GLOBAL_FORM_SESSION_ID: &str = "global";
const QUESTION_CANCELLED_FEEDBACK: &str = "This non-interactive run cannot ask the user questions, so the question was cancelled. Continue without an answer; make reasonable assumptions and state them.";
const PERMISSION_REJECTED_FEEDBACK: &str =
    "This non-interactive run cannot ask the user for permission, so the request was rejected. Continue without this action.";
const ATTACH_FILE_MAX_BYTES: u64 = 10 * 1024 * 1024;

// ---------------------------------------------------------------------------------------------
// UI (`run/ui.ts`) — everything on stderr

pub mod ui {
    use super::*;
    pub const TEXT_DIM: &str = "\x1b[90m";
    pub const TEXT_NORMAL: &str = "\x1b[0m";
    pub const TEXT_WARNING_BOLD: &str = "\x1b[93m\x1b[1m";
    pub const TEXT_DANGER_BOLD: &str = "\x1b[91m\x1b[1m";

    static BLANK: AtomicBool = AtomicBool::new(false);

    pub fn println(parts: &[&str]) {
        write_stderr(&format!("{}{EOL}", parts.join(" ")));
    }

    /// Prints `\x1b[0m` + EOL only on the first call in the process.
    pub fn empty() {
        if BLANK.swap(true, Ordering::SeqCst) {
            return;
        }
        println(&[TEXT_NORMAL]);
    }

    pub fn error(message: &str) {
        let message = message.strip_prefix("Error: ").unwrap_or(message);
        println(&[&format!("{TEXT_DANGER_BOLD}Error: {TEXT_NORMAL}{message}")]);
    }
}

// ---------------------------------------------------------------------------------------------
// model refs (`Model.Ref.parse`)

pub struct ModelRef {
    pub provider_id: String,
    pub id: String,
    pub variant: Option<String>,
}

pub fn parse_model(input: &str) -> anyhow::Result<ModelRef> {
    let invalid = || anyhow::anyhow!("Invalid model reference: {input}");
    let slash = input.find('/').filter(|i| *i > 0).ok_or_else(invalid)?;
    let provider = &input[..slash];
    let rest = &input[slash + 1..];
    let (id, variant) = match rest.find('#') {
        Some(h) => (&rest[..h], Some(&rest[h + 1..])),
        None => (rest, None),
    };
    if id.is_empty() || provider.contains('#') {
        return Err(invalid());
    }
    if let Some(v) = variant
        && (v.is_empty() || v.contains('#'))
    {
        return Err(invalid());
    }
    Ok(ModelRef { provider_id: provider.into(), id: id.into(), variant: variant.map(String::from) })
}

// ---------------------------------------------------------------------------------------------
// input preparation

/// `formatMessage(args)`
pub fn format_message(message: &[String]) -> Option<String> {
    let v = message
        .iter()
        .map(|p| if p.contains(' ') { format!("\"{}\"", p.replace('"', "\\\"")) } else { p.clone() })
        .collect::<Vec<_>>()
        .join(" ");
    (!v.is_empty()).then_some(v)
}

/// `mergeInput(message, piped)`
pub fn merge_input(message: Option<String>, piped: Option<String>) -> Option<String> {
    match (message, piped) {
        (None, p) => p.filter(|p| !p.is_empty()),
        (Some(m), None) => Some(m),
        (Some(m), Some(p)) if p.is_empty() => Some(m),
        (Some(m), Some(p)) => Some(format!("{m}\n{p}")),
    }
}

pub fn is_binary(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    if bytes.contains(&0) {
        return true;
    }
    let odd = bytes.iter().filter(|b| **b < 9 || (**b > 13 && **b < 32)).count();
    odd as f64 / bytes.len() as f64 > 0.3
}

#[derive(Clone, Debug)]
pub struct FilePart {
    pub url: String,
    pub filename: String,
    pub mime: String,
}

pub fn prepare_file(input: &str, root: &std::path::Path) -> anyhow::Result<FilePart> {
    let file = root.join(input);
    let meta = std::fs::metadata(&file).map_err(|_| anyhow::anyhow!("File not found: {input}"))?;
    if !meta.is_file() || meta.len() > ATTACH_FILE_MAX_BYTES {
        anyhow::bail!("Cannot attach a directory, special file, or file larger than 10 MiB: {input}");
    }
    let bytes = std::fs::read(&file).map_err(|_| anyhow::anyhow!("File not found: {input}"))?;
    let detected = mime_guess::from_path(&file).first().map(|m| m.essence_str().to_string()).unwrap_or_else(|| "application/octet-stream".into());
    let mime = if detected.starts_with("image/") || detected == "application/pdf" {
        detected
    } else if !is_binary(&bytes) && std::str::from_utf8(&bytes).is_ok() {
        "text/plain".into()
    } else {
        detected
    };
    Ok(FilePart {
        url: format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes)),
        filename: file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        mime,
    })
}

// ---------------------------------------------------------------------------------------------
// tool display (`tui/src/mini/tool.ts` toolInlineInfo)

pub fn canonical_tool_name(name: &str) -> &str {
    match name {
        "bash" => "shell",
        "task" => "subagent",
        "apply_patch" => "patch",
        n => n,
    }
}

pub fn read_display_text(text: &str) -> Option<String> {
    if !text.starts_with('{') {
        return None;
    }
    let v: Value = serde_json::from_str(text).ok()?;
    if let Some(c) = v.get("content").and_then(Value::as_str)
        && (v.get("type").and_then(Value::as_str) == Some("text-page") || v.get("encoding").and_then(Value::as_str) == Some("utf8"))
    {
        return Some(c.to_string());
    }
    let entries = v.get("entries")?.as_array()?;
    Some(
        entries
            .iter()
            .filter_map(|e| e.as_str().map(String::from).or_else(|| e.get("path").and_then(Value::as_str).map(String::from)))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// `toolOutputText(name, content)`
pub fn tool_output_text(name: &str, content: Option<&Value>) -> String {
    let Some(items) = content.and_then(Value::as_array) else { return String::new() };
    let canonical = canonical_tool_name(name);
    if canonical == "shell" {
        return items.iter().find(|i| i.get("type").and_then(Value::as_str) == Some("text")).and_then(|i| i.get("text")).and_then(Value::as_str).unwrap_or("").to_string();
    }
    let joined = items
        .iter()
        .filter(|i| i.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|i| i.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()))
        .collect::<Vec<_>>()
        .join("\n");
    if canonical == "read" {
        return read_display_text(&joined).unwrap_or(joined);
    }
    joined
}

/// `formatPath(input, {base, forwardSlashes})` (POSIX).
pub fn tool_path(input: &str, directory: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let base = std::path::Path::new(directory);
    let abs = if std::path::Path::new(input).is_absolute() { normalize(std::path::Path::new(input)) } else { normalize(&base.join(input)) };
    let base = normalize(base);
    let formatted = match abs.strip_prefix(&base) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
        Ok(rel) => rel.display().to_string(),
        Err(_) => abs.display().to_string(),
    };
    formatted.replace('\\', "/")
}

fn normalize(p: &std::path::Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn primitive_summary(input: &Map<String, Value>, omit: &[&str]) -> String {
    let entries: Vec<String> = input
        .iter()
        .filter(|(k, v)| !omit.contains(&k.as_str()) && (v.is_string() || v.is_number() || v.is_boolean()))
        .map(|(k, v)| format!("{k}={}", v.as_str().map(String::from).unwrap_or_else(|| v.to_string())))
        .collect();
    if entries.is_empty() { String::new() } else { format!("[{}]", entries.join(", ")) }
}

fn titlecase(s: &str) -> String {
    s.split(' ')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub struct Inline {
    pub icon: String,
    pub title: String,
    pub description: Option<String>,
    pub block: bool,
    pub body: Option<String>,
}

/// `toolInlineInfo(part, directory)` for a completed/error tool (`SessionMessageAssistantTool`).
pub fn tool_inline_info(tool: &Value, directory: &str) -> Inline {
    let raw_name = tool.get("name").and_then(Value::as_str).unwrap_or("tool");
    let name = canonical_tool_name(raw_name);
    let state = tool.get("state").cloned().unwrap_or(Value::Null);
    let status = state.get("status").and_then(Value::as_str).unwrap_or("");
    let mut input = state.get("input").and_then(Value::as_object).cloned().unwrap_or_default();
    let raw_input = input.clone();
    let meta = state.get("metadata").and_then(Value::as_object).cloned().unwrap_or_default();
    let output = tool_output_text(raw_name, if status == "completed" || status == "error" { state.get("content") } else { None });
    let path = input.get("path").and_then(Value::as_str).map(String::from).or_else(|| input.get("filePath").and_then(Value::as_str).map(String::from)).unwrap_or_default();
    if ["read", "write", "edit", "lsp"].contains(&name) && !path.is_empty() {
        input.insert("path".into(), json!(path));
    }
    let agent = input.get("agent").and_then(Value::as_str).map(String::from).or_else(|| input.get("subagent_type").and_then(Value::as_str).map(String::from)).unwrap_or_default();
    if name == "subagent" && !agent.is_empty() {
        input.insert("agent".into(), json!(agent));
    }
    let str_of = |m: &Map<String, Value>, k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let inline = |icon: &str, title: String| Inline { icon: icon.into(), title, description: None, block: false, body: None };
    let completed = status == "completed";
    let count = |n: i64| format!("{n} match{}", if n == 1 { "" } else { "es" });
    match name {
        "shell" => Inline { icon: "$".into(), title: str_of(&input, "command"), description: None, block: true, body: completed.then(|| output.trim().to_string()) },
        "write" => Inline { icon: "←".into(), title: format!("Write {}", tool_path(&str_of(&input, "path"), directory)), description: None, block: true, body: completed.then(|| output.clone()) },
        "edit" => {
            let file_patch = meta.get("files").and_then(Value::as_array).and_then(|f| f.first()).and_then(|f| f.get("patch")).and_then(Value::as_str).map(String::from);
            Inline {
                icon: "←".into(),
                title: format!("Edit {}", tool_path(&str_of(&input, "path"), directory)),
                description: None,
                block: true,
                body: file_patch.or_else(|| meta.get("diff").and_then(Value::as_str).map(String::from)),
            }
        }
        "patch" => {
            let n = meta.get("files").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
            inline("%", if n == 0 { "Patch".into() } else { format!("Patch {n} file{}", if n == 1 { "" } else { "s" }) })
        }
        "batch" => {
            let n = input.get("tool_calls").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
            Inline {
                icon: "#".into(),
                title: if n > 0 { format!("Batch {n} tool{}", if n == 1 { "" } else { "s" }) } else { "Batch".into() },
                description: None,
                block: true,
                body: completed.then(|| output.clone()),
            }
        }
        "subagent" => {
            let kind = titlecase(if agent.is_empty() { "unknown" } else { &agent });
            let desc = str_of(&input, "description");
            let icon = if status == "error" { "✗" } else if status == "running" { "•" } else { "✓" };
            Inline {
                icon: icon.into(),
                title: if desc.is_empty() { format!("{kind} Subagent") } else { desc.clone() },
                description: (!desc.is_empty()).then(|| format!("{kind} Agent")),
                block: false,
                body: None,
            }
        }
        "question" => {
            let n = raw_input.get("questions").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
            inline("→", format!("Asked {n} question{}", if n == 1 { "" } else { "s" }))
        }
        "read" => {
            let mut i = inline("→", format!("Read {}", tool_path(&str_of(&input, "path"), directory)));
            let d = primitive_summary(&raw_input, &["path"]);
            i.description = (!d.is_empty()).then_some(d);
            i
        }
        "glob" | "grep" => {
            let root = str_of(&input, "path");
            let title = format!("{} \"{}\"", if name == "glob" { "Glob" } else { "Grep" }, str_of(&input, "pattern"));
            let suffix = if root.is_empty() { String::new() } else { format!("in {}", tool_path(&root, directory)) };
            let matches = meta.get(if name == "glob" { "count" } else { "matches" }).and_then(Value::as_i64);
            let description = match matches {
                None => suffix,
                Some(m) => format!("{suffix}{}{}", if suffix.is_empty() { "" } else { " · " }, count(m)),
            };
            Inline { icon: "✱".into(), title, description: (!description.is_empty()).then_some(description), block: false, body: None }
        }
        "list" => {
            let dir = str_of(&input, "path");
            inline("→", if dir.is_empty() { "List".into() } else { format!("List {}", tool_path(&dir, directory)) })
        }
        "lsp" => {
            let op = Some(str_of(&input, "operation")).filter(|o| !o.is_empty()).unwrap_or_else(|| "request".into());
            let file = tool_path(&str_of(&input, "path"), directory);
            let pos = match (input.get("line").and_then(Value::as_i64), input.get("character").and_then(Value::as_i64)) {
                (Some(l), Some(c)) => format!(":{l}:{c}"),
                _ => String::new(),
            };
            inline("→", if file.is_empty() { format!("LSP {op}") } else { format!("LSP {op} {file}{pos}") })
        }
        "webfetch" => {
            let url = str_of(&input, "url");
            inline("%", if url.is_empty() { "WebFetch".into() } else { format!("WebFetch {url}") })
        }
        "websearch" => {
            let provider = meta.get("provider").and_then(Value::as_str).unwrap_or("");
            let label = match provider {
                "" => "Web Search".to_string(),
                "opencode" => "Web Search via OpenCode".into(),
                p => format!("Web Search via {}{}", p[..1].to_uppercase(), &p[1..]),
            };
            let q = str_of(&input, "query");
            inline("◈", if q.is_empty() { label } else { format!("{label} \"{q}\"") })
        }
        "skill" => {
            let n = meta.get("name").and_then(Value::as_str).map(String::from).unwrap_or_else(|| str_of(&input, "id"));
            inline("→", format!("Skill \"{n}\""))
        }
        "invalid" => Inline { icon: "✗".into(), title: "Invalid Tool".into(), description: None, block: true, body: completed.then(|| output.clone()) },
        _ => {
            let title = if input.is_empty() { "Unknown".to_string() } else { Value::Object(input.clone()).to_string() };
            inline("⚙", format!("{name} {title}"))
        }
    }
}

fn render_tool(tool: &Value, directory: &str) {
    let info = tool_inline_info(tool, directory);
    if info.block {
        ui::empty();
        ui::println(&[&format!("{}{}", ui::TEXT_NORMAL, info.icon), &format!("{}{}", ui::TEXT_NORMAL, info.title)]);
        if let Some(body) = info.body.filter(|b| !b.trim().is_empty()) {
            ui::println(&[&body]);
        }
        ui::empty();
        return;
    }
    let desc = info.description.map(|d| format!("{}{d}{}", ui::TEXT_DIM, ui::TEXT_NORMAL)).unwrap_or_default();
    ui::println(&[&format!("{}{}", ui::TEXT_NORMAL, info.icon), &format!("{}{}", ui::TEXT_NORMAL, info.title), &desc]);
}

fn render_tool_error(tool: &Value, directory: &str) {
    let info = tool_inline_info(tool, directory);
    ui::println(&[&format!("{}✗", ui::TEXT_NORMAL), &format!("{}{} failed", ui::TEXT_NORMAL, info.title)]);
}

// ---------------------------------------------------------------------------------------------
// entry

#[derive(Clone, Copy, PartialEq)]
enum Format {
    Default,
    Json,
}

/// `reportRunError(input, message, sessionID)`
fn report_run_error(format: Format, exit: &AtomicI32, message: &str, session: Option<&str>) {
    exit.store(1, Ordering::SeqCst);
    if format == Format::Json {
        let line = json!({
            "type": "error",
            "timestamp": chrono::Utc::now().timestamp_millis(),
            "sessionID": session.unwrap_or(""),
            "error": {"type": "unknown", "message": message},
        });
        write_stdout(&format!("{line}\n"));
        return;
    }
    ui::error(message);
}

pub async fn run(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut message = input.list("message");
    if let Some(sep) = argv.iter().position(|a| a == "--") {
        message.extend(argv[sep + 1..].iter().cloned());
    }
    let server = connection::resolve(global, Args { server: input.str("server"), standalone: input.flag("standalone"), mismatch: None, on_start: None }).await?;
    let format = if input.str("format").as_deref() == Some("json") { Format::Json } else { Format::Default };
    let exit = Arc::new(AtomicI32::new(-1));
    let opts = RunInput {
        message,
        continue_last: input.flag("continue"),
        session: input.str("session"),
        fork: input.flag("fork"),
        model: input.str("model"),
        agent: input.str("agent"),
        format,
        files: input.list("file"),
        title: input.str("title"),
        thinking: input.flag("thinking"),
        auto: input.flag("auto") || input.flag("yolo") || input.flag("dangerouslySkipPermissions"),
    };
    if let Err(e) = execute(&server, &opts, exit.clone()).await {
        report_run_error(format, &exit, &e.to_string(), None);
    }
    drop(server);
    let code = exit.load(Ordering::SeqCst);
    Ok(if code < 0 { 0 } else { code })
}

struct RunInput {
    message: Vec<String>,
    continue_last: bool,
    session: Option<String>,
    fork: bool,
    model: Option<String>,
    agent: Option<String>,
    format: Format,
    files: Vec<String>,
    title: Option<String>,
    thinking: bool,
    auto: bool,
}

/// An error carrying the session it happened in (`RunTargetError`).
#[derive(Debug)]
struct TargetError(String, Option<String>);

impl std::fmt::Display for TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TargetError {}

async fn execute(server: &connection::Resolved, input: &RunInput, exit: Arc<AtomicI32>) -> anyhow::Result<()> {
    if input.fork && !input.continue_last && input.session.is_none() {
        anyhow::bail!("--fork requires --continue or --session");
    }
    let root = std::env::var("PWD").ok().filter(|p| !p.is_empty()).map(std::path::PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    if std::env::set_current_dir(&root).is_err() {
        anyhow::bail!("Failed to change directory to {}", root.display());
    }
    let local = std::env::current_dir()?.display().to_string();
    let piped = if crate::term::stdin_is_tty() {
        None
    } else {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut s);
        Some(s)
    };
    let message = merge_input(format_message(&input.message), piped);
    let Some(message) = message.filter(|m| !m.trim().is_empty()) else { anyhow::bail!("You must provide a message") };
    let files = input.files.iter().map(|f| prepare_file(f, &root)).collect::<anyhow::Result<Vec<_>>>()?;

    let client = server.client();
    let explicit = input.model.as_deref().map(parse_model).transpose()?;
    let target = match resolve_target(
        &client,
        &local,
        input,
        explicit.as_ref(),
        server.service.is_some().then(connection::session_env),
    )
    .await
    {
        Ok(t) => t,
        Err(e) => match e.downcast::<TargetError>() {
            Ok(t) => {
                report_run_error(input.format, &exit, &t.0, t.1.as_deref());
                return Ok(());
            }
            Err(e) => return Err(e),
        },
    };
    if !target.resume
        && let Some(title) = &input.title
    {
        let title = if title.is_empty() {
            let units: Vec<u16> = message.encode_utf16().collect();
            let head = String::from_utf16_lossy(&units[..units.len().min(50)]);
            format!("{head}{}", if units.len() > 50 { "..." } else { "" })
        } else {
            title.clone()
        };
        client.call("session.update", Req::new().path("sessionID", &target.session_id).body(json!({"title": title}))).await?;
    }
    let prompt = Prompt {
        client: client.clone(),
        session_id: target.session_id.clone(),
        directory: target.directory.clone(),
        message,
        files,
        agent: target.agent.clone(),
        model: target.model.clone(),
        thinking: input.thinking,
        format: input.format,
        auto: input.auto,
        exit: exit.clone(),
    };
    if let Err(e) = prompt.run().await {
        report_run_error(input.format, &exit, &e.to_string(), Some(&target.session_id));
    }
    Ok(())
}

struct Target {
    session_id: String,
    directory: String,
    model: Option<Value>,
    agent: Option<String>,
    resume: bool,
}

fn mutation(e: ClientError) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

async fn find_session(client: &Client, id: &str) -> anyhow::Result<Option<Value>> {
    match client.call("session.get", Req::new().path("sessionID", id)).await {
        Ok(v) => Ok(v.get("data").cloned()),
        Err(e) if e.tag() == Some("SessionNotFoundError") => Ok(None),
        Err(e) => Err(e.into()),
    }
}

async fn location_get(client: &Client, directory: Option<&str>) -> anyhow::Result<Value> {
    let req = match directory {
        Some(d) => Req::new().location(d),
        None => Req::new(),
    };
    Ok(client.call("location.get", req).await?)
}

async fn latest_session(client: &Client, location: &Value) -> anyhow::Result<Option<Value>> {
    let dir = location.get("directory").cloned().unwrap_or(Value::Null);
    let mut cursor: Option<String> = None;
    loop {
        let mut req = Req::new()
            .query("directory", dir.clone())
            .query("parentID", Value::Null)
            .query("limit", json!(50))
            .query("order", json!("desc"));
        if let Some(c) = &cursor {
            req = req.query("cursor", json!(c));
        }
        let page = client.call("session.list", req).await?;
        let data = page.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
        if let Some(s) = data.iter().find(|s| s.pointer("/location/directory") == Some(&dir)) {
            return Ok(Some(s.clone()));
        }
        match page.pointer("/cursor/next").and_then(Value::as_str) {
            Some(n) if !data.is_empty() => cursor = Some(n.to_string()),
            _ => return Ok(None),
        }
    }
}

async fn fork(client: &Client, id: &str) -> anyhow::Result<Value> {
    Ok(client.call("session.fork", Req::new().path("sessionID", id)).await.map_err(mutation)?.get("data").cloned().unwrap_or(Value::Null))
}

/// `resolveSessionTarget`
async fn resolve_target(
    client: &Client,
    directory: &str,
    input: &RunInput,
    explicit: Option<&ModelRef>,
    environment: Option<indexmap::IndexMap<String, String>>,
) -> anyhow::Result<Target> {
    // selectSession
    let mut selected: Option<Value> = None;
    let mut selection_location: Option<Value> = None;
    if let Some(id) = &input.session {
        match find_session(client, id).await? {
            None if input.fork => anyhow::bail!("Session not found"),
            None => {}
            Some(found) => selected = Some(if input.fork { fork(client, id).await? } else { found }),
        }
    } else if input.continue_last {
        let location = location_get(client, Some(directory)).await?;
        match latest_session(client, &location).await? {
            Some(s) => {
                let id = s.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                selected = Some(if input.fork { fork(client, &id).await? } else { s });
            }
            None => selection_location = Some(location),
        }
    }
    let location = match selection_location {
        Some(l) => l,
        None => {
            let dir = selected.as_ref().and_then(|s| s.pointer("/location/directory").and_then(Value::as_str).map(String::from)).unwrap_or_else(|| directory.to_string());
            location_get(client, Some(&dir)).await?
        }
    };
    // prepare
    let model = match explicit {
        Some(m) => {
            let mut v = json!({"providerID": m.provider_id, "id": m.id});
            if let Some(var) = &m.variant {
                v["variant"] = json!(var);
            }
            Some(v)
        }
        None => selected.as_ref().and_then(|s| s.get("model").filter(|m| !m.is_null()).cloned()),
    };
    let model = model.map(|m| {
        let mut out = json!({"providerID": m.get("providerID"), "id": m.get("id")});
        if let Some(v) = m.get("variant").filter(|v| !v.is_null()) {
            out["variant"] = v.clone();
        }
        out
    });
    if explicit.and_then(|m| m.variant.as_ref()).is_some() && model.is_none() {
        return Err(TargetError(
            "Cannot select a variant before selecting a model".into(),
            selected.as_ref().and_then(|s| s.get("id").and_then(Value::as_str).map(String::from)),
        )
        .into());
    }
    let agent = input.agent.clone().or_else(|| selected.as_ref().and_then(|s| s.get("agent").and_then(Value::as_str).map(String::from)));
    let resume = selected.is_some();
    let session = match selected {
        Some(s) => s,
        None => {
            let mut body = Map::new();
            if let Some(id) = &input.session {
                body.insert("id".into(), json!(id));
            }
            if let Some(a) = &agent {
                body.insert("agent".into(), json!(a));
            }
            if let Some(m) = &model {
                body.insert("model".into(), m.clone());
            }
            body.insert("location".into(), json!({"directory": location.get("directory")}));
            client.call("session.create", Req::new().body(Value::Object(body))).await.map_err(mutation)?.get("data").cloned().unwrap_or(Value::Null)
        }
    };
    let session_id = session.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    if let Some(env) = environment {
        client
            .call("session.environment", Req::new().path("sessionID", &session_id).body(json!({"variables": env})))
            .await
            .map_err(mutation)?;
    }
    Ok(Target {
        directory: location.get("directory").and_then(Value::as_str).unwrap_or(directory).to_string(),
        agent: agent.or_else(|| session.get("agent").and_then(Value::as_str).map(String::from)),
        session_id,
        model,
        resume,
    })
}

// ---------------------------------------------------------------------------------------------
// prompt execution (`runNonInteractivePrompt`)

struct Prompt {
    client: Client,
    session_id: String,
    directory: String,
    message: String,
    files: Vec<FilePart>,
    agent: Option<String>,
    model: Option<Value>,
    thinking: bool,
    format: Format,
    auto: bool,
    exit: Arc<AtomicI32>,
}

#[derive(Default, Clone)]
struct ToolState {
    id: String,
    timestamp: i64,
    tool: String,
    input: Value,
    raw: Option<String>,
    provider: Option<Value>,
    metadata: Value,
}

#[derive(Default)]
struct State {
    starts: HashMap<String, (String, i64)>,
    tools: HashMap<String, ToolState>,
    rendered_text: HashMap<String, String>,
    rendered_reasoning: HashMap<String, String>,
    rendered_tools: std::collections::HashSet<String>,
    owned: HashMap<String, bool>,
    submitted: bool,
    promoted: bool,
    emitted_error: bool,
    form_cancelled: bool,
    pre_promotion_error: Option<Value>,
}

struct Shared {
    p: Prompt,
    message_id: String,
    state: Mutex<State>,
    interrupted: AtomicBool,
    finalizing: AtomicBool,
}

fn part_id(event_id: &str) -> String {
    format!("prt_{}", event_id.strip_prefix("evt_").unwrap_or(event_id))
}

fn projected_part_id(message_id: &str, part: &str) -> String {
    format!("prt_{}_{part}", message_id.strip_prefix("msg_").unwrap_or(message_id))
}

fn key(a: &str, b: impl std::fmt::Display) -> String {
    format!("{a}\u{0}{b}")
}

fn to_millis(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().map(|f| f as i64).unwrap_or(0),
        Some(Value::String(s)) => chrono::DateTime::parse_from_rfc3339(s).map(|d| d.timestamp_millis()).unwrap_or_else(|_| chrono::Utc::now().timestamp_millis()),
        _ => chrono::Utc::now().timestamp_millis(),
    }
}

fn put(m: &mut Map<String, Value>, k: &str, v: Option<&Value>) {
    if let Some(v) = v {
        m.insert(k.into(), v.clone());
    }
}

impl Shared {
    fn emit(&self, ty: &str, timestamp: i64, data: Vec<(&str, Value)>) -> bool {
        if self.p.format != Format::Json {
            return false;
        }
        let mut o = Map::new();
        o.insert("type".into(), json!(ty));
        o.insert("timestamp".into(), json!(timestamp));
        o.insert("sessionID".into(), json!(self.p.session_id));
        for (k, v) in data {
            o.insert(k.into(), v);
        }
        write_stdout(&format!("{}{EOL}", Value::Object(o)));
        true
    }

    fn set_exit(&self, code: i32) {
        self.p.exit.store(code, Ordering::SeqCst);
    }

    fn write_text(&self, part: Value, timestamp: i64) {
        if self.emit("text", timestamp, vec![("part", part.clone())]) {
            return;
        }
        let text = part.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if text.is_empty() {
            return;
        }
        if !crate::term::stdout_is_tty() {
            write_stdout(&format!("{text}{EOL}"));
            return;
        }
        ui::empty();
        ui::println(&[&text]);
        ui::empty();
    }

    fn write_reasoning(&self, part: Value, timestamp: i64) {
        if self.emit("reasoning", timestamp, vec![("part", part.clone())]) {
            return;
        }
        let text = part.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if text.is_empty() {
            return;
        }
        let line = format!("Thinking: {text}");
        if !crate::term::stdout_is_tty() {
            write_stdout(&format!("{line}{EOL}"));
            return;
        }
        ui::empty();
        ui::println(&[&format!("{}\x1b[3m{line}\x1b[0m{}", ui::TEXT_DIM, ui::TEXT_NORMAL)]);
        ui::empty();
    }

    fn error(&self, timestamp: i64, error: Value) {
        if !self.emit("error", timestamp, vec![("error", error.clone())]) {
            ui::error(error.get("message").and_then(Value::as_str).unwrap_or(""));
        }
    }

    async fn owns(&self, session_id: &str) -> bool {
        if session_id == self.p.session_id {
            return true;
        }
        let mut chain = vec![];
        let mut current = session_id.to_string();
        let result = loop {
            if let Some(v) = self.state.lock().owned.get(&current).copied() {
                break v;
            }
            if current == self.p.session_id {
                break true;
            }
            if current == GLOBAL_FORM_SESSION_ID {
                break false;
            }
            chain.push(current.clone());
            match self.p.client.call("session.get", Req::new().path("sessionID", &current)).await {
                Ok(v) => match v.pointer("/data/parentID").and_then(Value::as_str) {
                    Some(parent) => current = parent.to_string(),
                    None => break false,
                },
                Err(_) => break false,
            }
        };
        let mut st = self.state.lock();
        for c in chain {
            st.owned.insert(c, result);
        }
        result
    }

    async fn reply_permission(&self, request: &Value) {
        let action = request.get("action").and_then(Value::as_str).unwrap_or("");
        let resources: Vec<&str> = request.get("resources").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        if !self.p.auto {
            ui::println(&[
                &format!("{}!", ui::TEXT_WARNING_BOLD),
                &format!("{}permission requested: {action} ({}); auto-rejecting", ui::TEXT_NORMAL, resources.join(", ")),
            ]);
        }
        let body = if self.p.auto { json!({"decision": "once"}) } else { json!({"decision": "reject", "message": PERMISSION_REJECTED_FEEDBACK}) };
        let _ = self
            .p
            .client
            .call(
                "session.permission.reply",
                Req::new()
                    .path("sessionID", request.get("sessionID").and_then(Value::as_str).unwrap_or(""))
                    .path("requestID", request.get("id").and_then(Value::as_str).unwrap_or(""))
                    .body(body),
            )
            .await;
    }

    async fn cancel_form(&self, form: &Value) -> anyhow::Result<()> {
        let continuing = form.pointer("/metadata/kind").and_then(Value::as_str) == Some("question");
        let session = form.get("sessionID").and_then(Value::as_str).unwrap_or("");
        let mut req = Req::new().path("sessionID", session).path("formID", form.get("id").and_then(Value::as_str).unwrap_or(""));
        if continuing {
            req = req.query("message", json!(QUESTION_CANCELLED_FEEDBACK));
        }
        if session == GLOBAL_FORM_SESSION_ID {
            req = req.header("x-opencode-directory", crate::client::encode_component(&self.p.directory));
        }
        if let Err(e) = self.p.client.call("session.form.cancel", req).await
            && e.tag() != Some("FormAlreadySettledError")
        {
            return Err(e.into());
        }
        if continuing {
            return Ok(());
        }
        self.state.lock().form_cancelled = true;
        self.set_exit(1);
        Ok(())
    }

    async fn consume(&self, stream: &mut EventStream) -> Result<(), String> {
        loop {
            let next = stream.next().await;
            let event = match next {
                None => {
                    if self.state.lock().emitted_error {
                        return Ok(());
                    }
                    return Err("Event stream disconnected during prompt execution".into());
                }
                Some(Err(e)) => {
                    if self.state.lock().emitted_error {
                        return Ok(());
                    }
                    return Err(e.to_string());
                }
                Some(Ok(e)) => e,
            };
            let ty = event.get("type").and_then(Value::as_str).unwrap_or("").to_string();
            let data = event.get("data").cloned().unwrap_or(Value::Null);
            let submitted = self.state.lock().submitted;
            if ty == "permission.asked" && submitted && self.owns(data.get("sessionID").and_then(Value::as_str).unwrap_or("")).await {
                self.reply_permission(&data).await;
                continue;
            }
            if ty == "form.created" && submitted {
                let form = data.get("form").cloned().unwrap_or(Value::Null);
                if self.owns(form.get("sessionID").and_then(Value::as_str).unwrap_or("")).await {
                    self.cancel_form(&form).await.map_err(|e| e.to_string())?;
                    continue;
                }
            }
            if data.get("sessionID").and_then(Value::as_str) != Some(self.p.session_id.as_str()) {
                continue;
            }
            let time = to_millis(event.get("created"));
            let event_id = event.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let interrupted = self.interrupted.load(Ordering::SeqCst);
            let finalizing = self.finalizing.load(Ordering::SeqCst);
            {
                let mut st = self.state.lock();
                if ty == "session.inbox.delivered" && data.get("inboxID").and_then(Value::as_str) == Some(self.message_id.as_str()) {
                    st.promoted = true;
                    st.pre_promotion_error = None;
                    continue;
                }
                if ty == "session.execution.interrupted"
                    && data.get("reason").and_then(Value::as_str) == Some("user")
                    && (interrupted || st.form_cancelled)
                {
                    return Ok(());
                }
                if !st.promoted && ty == "session.execution.failed" {
                    st.pre_promotion_error = data.get("error").cloned();
                    if finalizing {
                        return Ok(());
                    }
                    continue;
                }
                if !st.promoted && finalizing && (ty == "session.execution.succeeded" || ty == "session.execution.interrupted") {
                    return Ok(());
                }
                if !st.promoted {
                    continue;
                }
                if finalizing && !ty.starts_with("session.execution.") {
                    continue;
                }
            }
            let amid = data.get("assistantMessageID").and_then(Value::as_str).unwrap_or("").to_string();
            match ty.as_str() {
                "session.step.started" => {
                    let mut part = Map::new();
                    part.insert("id".into(), json!(part_id(&event_id)));
                    part.insert("sessionID".into(), json!(self.p.session_id));
                    part.insert("messageID".into(), json!(amid));
                    part.insert("type".into(), json!("step-start"));
                    put(&mut part, "snapshot", data.get("snapshot"));
                    if !self.emit("step_start", time, vec![("part", Value::Object(part))]) && self.p.format != Format::Json {
                        ui::empty();
                        ui::println(&[&format!(
                            "> {} · {}",
                            data.get("agent").and_then(Value::as_str).unwrap_or(""),
                            data.pointer("/model/id").and_then(Value::as_str).unwrap_or("")
                        )]);
                        ui::empty();
                    }
                }
                "session.text.started" | "session.reasoning.started" => {
                    let kind = if ty == "session.text.started" { "text" } else { "reasoning" };
                    let k = key(kind, key(&amid, data.get("ordinal").cloned().unwrap_or(Value::Null)));
                    self.state.lock().starts.insert(k, (part_id(&event_id), time));
                }
                "session.text.ended" => {
                    let ck = key(&amid, data.get("ordinal").cloned().unwrap_or(Value::Null));
                    let started = self.state.lock().starts.remove(&key("text", &ck));
                    let text = data.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                    let part = json!({
                        "id": started.as_ref().map(|s| s.0.clone()).unwrap_or_else(|| part_id(&event_id)),
                        "sessionID": self.p.session_id, "messageID": amid, "type": "text", "text": text,
                        "time": {"start": started.as_ref().map(|s| s.1).unwrap_or(time), "end": time},
                    });
                    self.state.lock().rendered_text.insert(ck, text);
                    self.write_text(part, time);
                }
                "session.reasoning.ended" if self.p.thinking => {
                    let ck = key(&amid, data.get("ordinal").cloned().unwrap_or(Value::Null));
                    let started = self.state.lock().starts.remove(&key("reasoning", &ck));
                    let text = data.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                    let mut part = Map::new();
                    part.insert("id".into(), json!(started.as_ref().map(|s| s.0.clone()).unwrap_or_else(|| part_id(&event_id))));
                    part.insert("sessionID".into(), json!(self.p.session_id));
                    part.insert("messageID".into(), json!(amid));
                    part.insert("type".into(), json!("reasoning"));
                    part.insert("text".into(), json!(text));
                    put(&mut part, "metadata", data.get("native"));
                    part.insert("time".into(), json!({"start": started.as_ref().map(|s| s.1).unwrap_or(time), "end": time}));
                    self.state.lock().rendered_reasoning.insert(ck, text);
                    self.write_reasoning(Value::Object(part), time);
                }
                "session.tool.input.started" => {
                    let k = key(&amid, data.get("id").and_then(Value::as_str).unwrap_or(""));
                    self.state.lock().tools.insert(
                        k,
                        ToolState {
                            id: part_id(&event_id),
                            timestamp: time,
                            tool: data.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                            input: json!({}),
                            metadata: json!({}),
                            ..Default::default()
                        },
                    );
                }
                "session.tool.input.ended" | "session.tool.input.delta" => {
                    let k = key(&amid, data.get("id").and_then(Value::as_str).unwrap_or(""));
                    let mut st = self.state.lock();
                    if let Some(t) = st.tools.get_mut(&k) {
                        if ty == "session.tool.input.ended" {
                            t.raw = data.get("text").and_then(Value::as_str).map(String::from);
                        } else {
                            t.raw = Some(format!("{}{}", t.raw.clone().unwrap_or_default(), data.get("delta").and_then(Value::as_str).unwrap_or("")));
                        }
                    }
                }
                "session.tool.called" => {
                    let k = key(&amid, data.get("id").and_then(Value::as_str).unwrap_or(""));
                    let mut st = self.state.lock();
                    let current = st.tools.get(&k).cloned();
                    let mut provider = Map::new();
                    put(&mut provider, "executed", data.get("executed"));
                    put(&mut provider, "state", data.get("native"));
                    st.tools.insert(
                        k,
                        ToolState {
                            id: current.as_ref().map(|c| c.id.clone()).unwrap_or_else(|| part_id(&event_id)),
                            timestamp: current.as_ref().map(|c| c.timestamp).unwrap_or(time),
                            tool: current.as_ref().map(|c| c.tool.clone()).unwrap_or_else(|| "tool".into()),
                            input: data.get("input").cloned().unwrap_or(json!({})),
                            raw: current.and_then(|c| c.raw),
                            provider: Some(Value::Object(provider)),
                            metadata: json!({}),
                        },
                    );
                }
                "session.tool.progress" => {
                    let k = key(&amid, data.get("id").and_then(Value::as_str).unwrap_or(""));
                    if let Some(t) = self.state.lock().tools.get_mut(&k) {
                        t.metadata = data.get("metadata").cloned().unwrap_or(json!({}));
                    }
                }
                "session.tool.success" | "session.tool.failed" => {
                    let success = ty == "session.tool.success";
                    let tid = data.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                    let k = key(&amid, &tid);
                    let current = {
                        let mut st = self.state.lock();
                        let c = st.tools.remove(&k).unwrap_or_else(|| ToolState {
                            id: part_id(&event_id),
                            timestamp: time,
                            tool: "tool".into(),
                            input: json!({}),
                            metadata: json!({}),
                            ..Default::default()
                        });
                        st.rendered_tools.insert(k.clone());
                        c
                    };
                    let mut provider_result = Map::new();
                    put(&mut provider_result, "executed", data.get("executed"));
                    put(&mut provider_result, "state", data.get("native"));
                    let mut meta = Map::new();
                    let content = if success {
                        data.get("content").cloned()
                    } else {
                        data.get("content").cloned().filter(|c| !c.is_null())
                    };
                    let metadata = if success {
                        data.get("metadata").cloned()
                    } else {
                        Some(data.get("metadata").cloned().filter(|m| !m.is_null()).unwrap_or_else(|| current.metadata.clone()))
                    };
                    if success {
                        put(&mut meta, "metadata", metadata.as_ref());
                        put(&mut meta, "content", content.as_ref());
                    }
                    put(&mut meta, "providerCall", current.provider.as_ref());
                    meta.insert("providerResult".into(), Value::Object(provider_result));
                    put(&mut meta, "rawInput", current.raw.as_ref().map(|r| json!(r)).as_ref());
                    let error = data.pointer("/error/message").and_then(Value::as_str).unwrap_or("").to_string();
                    let state = if success {
                        json!({"status": "completed", "input": current.input, "output": tool_output_text(&current.tool, content.as_ref()), "title": current.tool, "metadata": meta, "time": {"start": current.timestamp, "end": time}})
                    } else {
                        json!({"status": "error", "input": current.input, "error": error, "metadata": meta, "time": {"start": current.timestamp, "end": time}})
                    };
                    let part = json!({
                        "partID": current.id, "sessionID": self.p.session_id, "messageID": amid, "type": "tool",
                        "id": tid, "tool": current.tool, "state": state,
                    });
                    if !self.emit("tool_use", time, vec![("part", part)]) {
                        let mut tool_state = json!({"status": if success { "completed" } else { "error" }, "input": current.input});
                        if let Some(m) = &metadata {
                            tool_state["metadata"] = m.clone();
                        }
                        if let Some(c) = &content {
                            tool_state["content"] = c.clone();
                        }
                        let tool = json!({"type": "tool", "id": tid, "name": current.tool, "state": tool_state, "time": {"created": current.timestamp, "ran": current.timestamp, "completed": time}});
                        if success {
                            render_tool(&tool, &self.p.directory);
                        } else {
                            if content.is_some() && !tool_output_text(&current.tool, content.as_ref()).trim().is_empty() {
                                let mut completed = tool.clone();
                                completed["state"]["status"] = json!("completed");
                                render_tool(&completed, &self.p.directory);
                            }
                            render_tool_error(&tool, &self.p.directory);
                            ui::error(&error);
                        }
                    }
                }
                "session.step.ended" => {
                    let mut part = Map::new();
                    part.insert("id".into(), json!(part_id(&event_id)));
                    part.insert("sessionID".into(), json!(self.p.session_id));
                    part.insert("messageID".into(), json!(amid));
                    part.insert("type".into(), json!("step-finish"));
                    put(&mut part, "reason", data.get("finish"));
                    put(&mut part, "snapshot", data.get("snapshot"));
                    put(&mut part, "cost", data.get("cost"));
                    put(&mut part, "tokens", data.get("tokens"));
                    self.emit("step_finish", time, vec![("part", Value::Object(part))]);
                }
                "session.step.failed" => {
                    if self.interrupted.load(Ordering::SeqCst) || self.state.lock().form_cancelled {
                        continue;
                    }
                    self.state.lock().emitted_error = true;
                    self.set_exit(1);
                    self.error(time, data.get("error").cloned().unwrap_or(Value::Null));
                }
                "session.execution.failed" => {
                    let emit = {
                        let mut st = self.state.lock();
                        let e = !st.emitted_error && !st.form_cancelled;
                        if e {
                            st.emitted_error = true;
                        }
                        e
                    };
                    if emit {
                        self.set_exit(1);
                        self.error(time, data.get("error").cloned().unwrap_or(Value::Null));
                    }
                    return Ok(());
                }
                "session.execution.interrupted" => {
                    let reason = data.get("reason").and_then(Value::as_str).unwrap_or("");
                    if reason == "user" && self.interrupted.load(Ordering::SeqCst) {
                        self.set_exit(130);
                    }
                    let emit = {
                        let mut st = self.state.lock();
                        let e = reason != "user" && !st.emitted_error && !st.form_cancelled;
                        if e {
                            st.emitted_error = true;
                        }
                        e
                    };
                    if emit {
                        self.set_exit(1);
                        self.error(time, json!({"type": "aborted", "message": format!("Session interrupted: {reason}")}));
                    }
                    return Ok(());
                }
                "session.execution.succeeded" => return Ok(()),
                _ => {}
            }
        }
    }

    async fn projected(&self) -> anyhow::Result<(bool, Vec<Value>)> {
        let mut messages = vec![];
        let mut cursor: Option<String> = None;
        loop {
            let mut req = Req::new().path("sessionID", &self.p.session_id).query("limit", json!(200));
            req = match &cursor {
                Some(c) => req.query("cursor", json!(c)),
                None => req.query("order", json!("desc")),
            };
            let page = self.p.client.call("session.message.list", req).await?;
            for m in page.get("data").and_then(Value::as_array).cloned().unwrap_or_default() {
                if m.get("id").and_then(Value::as_str) == Some(self.message_id.as_str()) {
                    messages.reverse();
                    return Ok((true, messages));
                }
                messages.push(m);
            }
            match page.pointer("/cursor/next").and_then(Value::as_str) {
                Some(n) => cursor = Some(n.to_string()),
                None => return Ok((false, vec![])),
            }
        }
    }

    async fn reconcile(&self) -> anyhow::Result<(bool, bool)> {
        let (found, messages) = self.projected().await?;
        for message in &messages {
            if message.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let mid = message.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let created = message.pointer("/time/created").cloned().unwrap_or(Value::Null);
            let timestamp = message.pointer("/time/completed").filter(|v| !v.is_null()).cloned().unwrap_or(created.clone());
            let ts = timestamp.as_i64().unwrap_or(0);
            let (mut text_ordinal, mut reasoning_ordinal) = (0, 0);
            for item in message.get("content").and_then(Value::as_array).cloned().unwrap_or_default() {
                let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
                if ty == "text" || ty == "reasoning" {
                    let text_kind = ty == "text";
                    let ordinal = if text_kind { &mut text_ordinal } else { &mut reasoning_ordinal };
                    let n = *ordinal;
                    *ordinal += 1;
                    if !text_kind && !self.p.thinking {
                        continue;
                    }
                    let k = key(&mid, n);
                    let full = item.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                    let rendered = {
                        let st = self.state.lock();
                        if text_kind { st.rendered_text.get(&k).cloned() } else { st.rendered_reasoning.get(&k).cloned() }.unwrap_or_default()
                    };
                    if rendered == full || !full.starts_with(&rendered) {
                        continue;
                    }
                    let text = full[rendered.len()..].to_string();
                    let mut part = Map::new();
                    part.insert("id".into(), json!(projected_part_id(&mid, &format!("{ty}-{n}"))));
                    part.insert("sessionID".into(), json!(self.p.session_id));
                    part.insert("messageID".into(), json!(mid));
                    part.insert("type".into(), json!(ty));
                    part.insert("text".into(), json!(text));
                    if !text_kind {
                        put(&mut part, "metadata", item.get("native"));
                    }
                    part.insert("time".into(), json!({"start": created, "end": timestamp}));
                    if text_kind {
                        self.state.lock().rendered_text.insert(k, full);
                        self.write_text(Value::Object(part), ts);
                    } else {
                        self.state.lock().rendered_reasoning.insert(k, full);
                        self.write_reasoning(Value::Object(part), ts);
                    }
                    continue;
                }
                let tid = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                let k = key(&mid, &tid);
                let status = item.pointer("/state/status").and_then(Value::as_str).unwrap_or("");
                if self.state.lock().rendered_tools.contains(&k) || status == "streaming" || status == "running" {
                    continue;
                }
                let name = item.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                let state = item.get("state").cloned().unwrap_or(Value::Null);
                let start = item.pointer("/time/ran").filter(|v| !v.is_null()).or_else(|| item.pointer("/time/created")).cloned();
                let end = item.pointer("/time/completed").filter(|v| !v.is_null()).cloned().unwrap_or(timestamp.clone());
                let mut meta = Map::new();
                put(&mut meta, "metadata", state.get("metadata"));
                put(&mut meta, "content", state.get("content"));
                let pstate = if status == "completed" {
                    json!({"status": "completed", "input": state.get("input"), "output": tool_output_text(&name, state.get("content")), "title": name, "metadata": meta, "time": {"start": start, "end": end}})
                } else {
                    json!({"status": "error", "input": state.get("input"), "error": state.pointer("/error/message"), "metadata": meta, "time": {"start": start, "end": end}})
                };
                let part = json!({
                    "partID": projected_part_id(&mid, &format!("tool-{tid}")), "sessionID": self.p.session_id, "messageID": mid,
                    "type": "tool", "id": tid, "tool": name, "state": pstate,
                });
                self.state.lock().rendered_tools.insert(k);
                if self.emit("tool_use", ts, vec![("part", part)]) {
                    continue;
                }
                if status == "completed" {
                    render_tool(&item, &self.p.directory);
                    continue;
                }
                if state.get("content").is_some() && !tool_output_text(&name, state.get("content")).trim().is_empty() {
                    let mut completed = item.clone();
                    completed["state"]["status"] = json!("completed");
                    render_tool(&completed, &self.p.directory);
                }
                render_tool_error(&item, &self.p.directory);
                ui::error(state.pointer("/error/message").and_then(Value::as_str).unwrap_or(""));
            }
            if let Some(err) = message.get("error").filter(|e| !e.is_null()) {
                let emit = {
                    let mut st = self.state.lock();
                    let e = !st.emitted_error && !st.form_cancelled;
                    if e {
                        st.emitted_error = true;
                    }
                    e
                };
                if emit {
                    self.set_exit(1);
                    self.error(ts, err.clone());
                }
            }
        }
        let responded = messages.iter().any(|m| m.get("type").and_then(Value::as_str) == Some("assistant"));
        Ok((found, responded))
    }
}

impl Prompt {
    async fn run(self) -> anyhow::Result<()> {
        let mut stream = self.client.subscribe().await?;
        match stream.next().await {
            None => anyhow::bail!("Event stream disconnected before prompt admission"),
            Some(Err(e)) => return Err(e.into()),
            Some(Ok(_)) => {}
        }
        let shared = Arc::new(Shared {
            message_id: oc_schema::id::message(),
            p: self,
            state: Mutex::new(State::default()),
            interrupted: AtomicBool::new(false),
            finalizing: AtomicBool::new(false),
        });
        let admission = Arc::new(tokio::sync::Notify::new());
        // SIGINT: first interrupts the session, second exits immediately with 130.
        let sigint = {
            let shared = shared.clone();
            let admission = admission.clone();
            tokio::spawn(async move {
                loop {
                    if tokio::signal::ctrl_c().await.is_err() {
                        return;
                    }
                    if shared.interrupted.swap(true, Ordering::SeqCst) {
                        std::process::exit(130);
                    }
                    shared.set_exit(130);
                    admission.notify_waiters();
                    let _ = shared.p.client.call("session.interrupt", Req::new().path("sessionID", &shared.p.session_id)).await;
                }
            })
        };
        let result = shared.clone().execute(stream, admission).await;
        sigint.abort();
        result
    }
}

impl Shared {
    async fn execute(self: Arc<Self>, stream: EventStream, admission: Arc<tokio::sync::Notify>) -> anyhow::Result<()> {
        let client = self.p.client.clone();
        let sid = self.p.session_id.clone();
        if let Some(agent) = &self.p.agent {
            client.call("session.switchAgent", Req::new().path("sessionID", &sid).body(json!({"agent": agent}))).await?;
        }
        if let Some(model) = &self.p.model {
            client.call("session.switchModel", Req::new().path("sessionID", &sid).body(json!({"model": model}))).await?;
        }
        let mut texts = vec![self.p.message.clone()];
        let mut attachments = vec![];
        for f in &self.p.files {
            if f.mime != "text/plain" {
                attachments.push(json!({"uri": f.url, "name": f.filename}));
            } else {
                let b64 = &f.url[f.url.find(',').map(|i| i + 1).unwrap_or(0)..];
                let content = base64::engine::general_purpose::STANDARD.decode(b64).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
                texts.push(format!("<file name=\"{}\">\n{content}\n</file>", f.filename));
            }
        }
        if self.interrupted.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.state.lock().submitted = true;
        let consumer = {
            let me = self.clone();
            let mut stream = stream;
            tokio::spawn(async move { me.consume(&mut stream).await }).map(|r| r.unwrap_or_else(|e| Err(e.to_string()))).boxed().shared()
        };
        let body = json!({"id": self.message_id, "text": texts.join("\n\n"), "files": attachments, "delivery": "steer"});
        let prompt = client.call("session.prompt", Req::new().path("sessionID", &sid).body(body));
        let response = tokio::select! {
            r = prompt => r.map_err(anyhow::Error::from),
            _ = admission.notified() => Err(anyhow::anyhow!("aborted")),
        };
        if let Err(e) = response {
            if self.interrupted.load(Ordering::SeqCst) {
                let _ = client.call("session.interrupt", Req::new().path("sessionID", &sid)).await;
            }
            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), consumer.clone()).await;
            if self.interrupted.load(Ordering::SeqCst) || self.state.lock().emitted_error {
                return Ok(());
            }
            return Err(e);
        }
        if self.interrupted.load(Ordering::SeqCst) {
            let _ = client.call("session.interrupt", Req::new().path("sessionID", &sid)).await;
        }
        let (permissions, forms) = tokio::join!(
            client.call("session.permission.list", Req::new().path("sessionID", &sid)),
            client.call("session.form.list", Req::new().path("sessionID", &sid)),
        );
        for p in permissions.ok().and_then(|v| v.get("data").and_then(Value::as_array).cloned()).unwrap_or_default() {
            self.reply_permission(&p).await;
        }
        for f in forms.ok().and_then(|v| v.get("data").and_then(Value::as_array).cloned()).unwrap_or_default() {
            self.cancel_form(&f).await?;
        }
        let waiting = client.call("experimental.session.wait", Req::new().path("sessionID", &sid)).boxed().shared();
        tokio::select! {
            w = waiting.clone() => { w?; }
            c = consumer.clone() => { c.map_err(|e| anyhow::anyhow!(e))?; waiting.await?; }
        }
        self.finalizing.store(true, Ordering::SeqCst);
        let (found, responded) = self.reconcile().await?;
        let (interrupted, form_cancelled, emitted, pre) = {
            let st = self.state.lock();
            (self.interrupted.load(Ordering::SeqCst), st.form_cancelled, st.emitted_error, st.pre_promotion_error.clone())
        };
        if !responded && !interrupted && !form_cancelled && !emitted && pre.is_none() {
            consumer.clone().await.map_err(|e| anyhow::anyhow!(e))?;
        }
        let (form_cancelled, emitted, pre) = {
            let st = self.state.lock();
            (st.form_cancelled, st.emitted_error, st.pre_promotion_error.clone())
        };
        if !found && !self.interrupted.load(Ordering::SeqCst) && !form_cancelled && !emitted {
            let error = pre.unwrap_or_else(|| json!({"type": "unknown", "message": "Prompt was not promoted"}));
            self.state.lock().emitted_error = true;
            self.set_exit(1);
            self.error(chrono::Utc::now().timestamp_millis(), error);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_refs() {
        let m = parse_model("openrouter/openai/gpt-5#high").unwrap();
        assert_eq!((m.provider_id.as_str(), m.id.as_str(), m.variant.as_deref()), ("openrouter", "openai/gpt-5", Some("high")));
        for bad in ["gpt", "/x", "a/", "a/b#", "a#x/b", "a/b#c#d"] {
            assert_eq!(parse_model(bad).err().unwrap().to_string(), format!("Invalid model reference: {bad}"));
        }
    }

    #[test]
    fn message_merging() {
        assert_eq!(format_message(&["hello world".into(), "a\"b c".into(), "x".into()]).unwrap(), "\"hello world\" \"a\\\"b c\" x");
        assert_eq!(merge_input(Some("from args".into()), Some("from stdin".into())).unwrap(), "from args\nfrom stdin");
        assert_eq!(merge_input(None, Some(String::new())), None);
        assert!(is_binary(&[0, 1]));
        assert!(!is_binary(b"hello\n"));
    }

    #[test]
    fn inline_info() {
        let tool = json!({"name": "bash", "state": {"status": "completed", "input": {"command": "ls"}, "content": [{"type": "text", "text": "a\n"}, {"type": "text", "text": "status"}]}});
        let i = tool_inline_info(&tool, "/w");
        assert_eq!((i.icon.as_str(), i.title.as_str(), i.body.as_deref()), ("$", "ls", Some("a")));
        let read = json!({"name": "read", "state": {"status": "completed", "input": {"filePath": "/w/src/a.rs", "offset": 2}}});
        let i = tool_inline_info(&read, "/w");
        assert_eq!((i.title.as_str(), i.description.as_deref()), ("Read src/a.rs", Some("[filePath=/w/src/a.rs, offset=2]")));
        let grep = json!({"name": "grep", "state": {"status": "completed", "input": {"pattern": "x", "path": "/w/src"}, "metadata": {"matches": 1}}});
        assert_eq!(tool_inline_info(&grep, "/w").description.as_deref(), Some("in src · 1 match"));
        let other = json!({"name": "mcp_x", "state": {"status": "completed", "input": {}}});
        assert_eq!(tool_inline_info(&other, "/w").title, "mcp_x Unknown");
    }
}
