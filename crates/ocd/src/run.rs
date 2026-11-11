//! `opencode run` (`handlers/run.ts`, `run/run.ts`, `run/noninteractive.ts`, `session-target.ts`, `util/io.ts`):
//! a non-interactive prompt against the server, streaming SSE events to stdout/stderr, waiting for the background
//! jobs it started, and cancelling its Session and jobs on SIGINT/SIGTERM.

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
// entry (`handlers/run.ts`, `run/run.ts`)

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

/// `opencode run`. A stop signal (SIGINT/SIGTERM) cancels the run's Session and background jobs; the run then
/// returns and `main` exits 130 or 143.
pub async fn run(global: &Global, input: &Input) -> anyhow::Result<i32> {
    let format = if input.str("format").as_deref() == Some("json") { Format::Json } else { Format::Default };
    let exit = Arc::new(AtomicI32::new(-1));
    // A one-off run uses the background service when it is already up, but never leaves a new one running.
    let args = Args {
        server: input.str("server"),
        standalone: input.flag("standalone"),
        mismatch: None,
        autostart: Some(false),
        on_start: None,
    };
    let server = match connection::resolve(global, args).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(cause = %e, "run could not resolve its server");
            report_run_error(format, &exit, &e.to_string(), None);
            return Ok(1);
        }
    };
    let opts = RunInput {
        message: input.list("message"),
        continue_last: input.flag("continue"),
        session: input.str("session"),
        fork: input.flag("fork"),
        worktree: input.optional_value("worktree"),
        model: input.str("model"),
        agent: input.str("agent"),
        format,
        files: input.list("file"),
        title: input.str("title"),
        archived: input.flag("archived"),
        thinking: input.flag("thinking"),
        auto: input.flag("auto") || input.flag("yolo") || input.flag("dangerouslySkipPermissions"),
    };
    // `runNonInteractive`: any failure is reported as a run error.
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
    /// `--worktree` (`Some(None)`) or `--worktree=<name>`.
    worktree: Option<Option<String>>,
    model: Option<String>,
    agent: Option<String>,
    format: Format,
    files: Vec<String>,
    title: Option<String>,
    archived: bool,
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

/// How long a message argument waits for other stdin (`PIPED_INPUT_GRACE_MS`).
const PIPED_INPUT_GRACE: std::time::Duration = std::time::Duration::from_millis(1000);

/// `readStdin()`: all of stdin.
async fn read_stdin() -> String {
    tokio::task::spawn_blocking(|| {
        let mut buf = vec![];
        let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    })
    .await
    .unwrap_or_default()
}

/// `readStdinIfReady(grace)` (`util/io.ts`): a shell pipe or redirected file is read in full, however slow its
/// producer; any other stdin (the socket spawn's default stdio gives a child) only if it has data or ends within
/// `grace`, otherwise it is left unread.
async fn read_stdin_if_ready(grace: std::time::Duration) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        use std::os::fd::AsFd;
        let stat = std::io::stdin().as_fd().try_clone_to_owned().ok().map(std::fs::File::from).and_then(|f| f.metadata().ok());
        if stat.as_ref().is_some_and(|m| m.file_type().is_fifo() || m.is_file()) {
            return Some(read_stdin().await);
        }
        let ready = tokio::task::spawn_blocking(move || {
            let mut fds = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd for the duration of the call.
            unsafe { libc::poll(&mut fds, 1, grace.as_millis() as i32) > 0 }
        })
        .await
        .unwrap_or(false);
        if ready { Some(read_stdin().await) } else { None }
    }
    #[cfg(not(unix))]
    {
        let _ = grace;
        Some(read_stdin().await)
    }
}

async fn execute(server: &connection::Resolved, input: &RunInput, exit: Arc<AtomicI32>) -> anyhow::Result<()> {
    if input.fork && !input.continue_last && input.session.is_none() {
        anyhow::bail!("--fork requires --continue or --session");
    }
    if input.worktree.is_some() && (input.continue_last || input.session.is_some()) {
        anyhow::bail!("--worktree starts a new session and cannot be combined with --continue or --session");
    }
    let root = std::env::var("PWD").ok().filter(|p| !p.is_empty()).map(std::path::PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    if std::env::set_current_dir(&root).is_err() {
        anyhow::bail!("Failed to change directory to {}", root.display());
    }
    let local = std::env::current_dir()?.display().to_string();
    let argument = Some(input.message.join(" ")).filter(|m| !m.is_empty());
    // With a message argument, stdin is extra context: a shell pipe is read in full, but the open, silent stdin
    // that spawn's default stdio gives a child must not hold the run.
    let piped = if crate::term::stdin_is_tty() {
        None
    } else if argument.is_some() {
        read_stdin_if_ready(PIPED_INPUT_GRACE).await
    } else {
        Some(read_stdin().await)
    };
    let message = merge_input(argument, piped);
    let Some(message) = message.filter(|m| !m.trim().is_empty()) else { anyhow::bail!("You must provide a message") };
    let files = input.files.iter().map(|f| prepare_file(f, &root)).collect::<anyhow::Result<Vec<_>>>()?;

    let client = server.client();
    let explicit = input.model.as_deref().map(parse_model).transpose()?;
    let worktree = match &input.worktree {
        None => None,
        Some(name) => Some(create_worktree_location(&client, &local, name.as_deref()).await?),
    };
    if let Some((root, _)) = &worktree {
        ui::println(&[&format!("{}worktree {root}{}", ui::TEXT_DIM, ui::TEXT_NORMAL)]);
    }
    let directory = worktree.as_ref().map(|(_, d)| d.clone()).unwrap_or(local);
    let target = match resolve_target(&client, &directory, input, explicit.as_ref(), server.service.is_some().then(connection::session_env)).await {
        Ok(t) => t,
        Err(e) => match e.downcast::<TargetError>() {
            Ok(t) => {
                report_run_error(input.format, &exit, &t.0, t.1.as_deref());
                return Ok(());
            }
            Err(e) => return Err(e),
        },
    };
    let title = match &input.title {
        Some(title) if !target.resume => Some(if title.is_empty() {
            let units: Vec<u16> = message.encode_utf16().collect();
            let head = String::from_utf16_lossy(&units[..units.len().min(50)]);
            format!("{head}{}", if units.len() > 50 { "..." } else { "" })
        } else {
            title.clone()
        }),
        _ => None,
    };
    // Archive before the first prompt is admitted, so the run never reaches an inbox as working or finished.
    let archived = input.archived && (!target.resume || input.fork);
    if title.is_some() || archived {
        let mut body = Map::new();
        if let Some(title) = title {
            body.insert("title".into(), json!(title));
        }
        if archived {
            body.insert("archived".into(), json!(true));
        }
        client.call("session.update", Req::new().path("sessionID", &target.session_id).body(Value::Object(body))).await?;
    }
    let mut texts = vec![message];
    let mut attachments = vec![];
    for f in &files {
        if f.mime == "text/plain" {
            let b64 = &f.url[f.url.find(',').map(|i| i + 1).unwrap_or(0)..];
            let content = base64::engine::general_purpose::STANDARD.decode(b64).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
            texts.push(format!("<file name=\"{}\">\n{content}\n</file>", f.filename));
        } else {
            attachments.push(json!({"uri": f.url, "name": f.filename}));
        }
    }
    let prompt = Prompt {
        client: client.clone(),
        session_id: target.session_id.clone(),
        directory: target.directory.clone(),
        message: texts.join("\n\n"),
        files: attachments,
        agent: target.agent.clone(),
        model: target.model.clone(),
        thinking: input.thinking,
        format: input.format,
        auto: input.auto,
        standalone: server.standalone.is_some(),
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

/// `SessionTargetMutationError`: the request's own message.
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

async fn location_get(client: &Client, directory: Option<&str>) -> Result<Value, ClientError> {
    let req = match directory {
        Some(d) => Req::new().location(d),
        None => Req::new(),
    };
    client.call("location.get", req).await
}

/// `createWorktreeLocation`: a new worktree of the location's project, keeping the caller's subfolder when the
/// worktree has it. `(root, directory)`.
async fn create_worktree_location(client: &Client, directory: &str, name: Option<&str>) -> anyhow::Result<(String, String)> {
    let base = location_get(client, Some(directory)).await?;
    let project_dir = base.pointer("/project/directory").and_then(Value::as_str).unwrap_or("").to_string();
    let mut body = Map::new();
    body.insert("projectID".into(), base.pointer("/project/id").cloned().unwrap_or(Value::Null));
    body.insert("from".into(), json!(project_dir));
    if let Some(name) = name {
        body.insert("name".into(), json!(name));
    }
    let created = client.call("worktree.create", Req::new().body(Value::Object(body))).await.map_err(mutation)?;
    let created = created.get("directory").and_then(Value::as_str).unwrap_or("").to_string();
    let base_dir = base.get("directory").and_then(Value::as_str).unwrap_or("");
    let subfolder = relative(&project_dir, base_dir);
    if subfolder.is_empty() || subfolder.starts_with("..") || std::path::Path::new(&subfolder).is_absolute() {
        return Ok((created.clone(), created));
    }
    let joined = normalize(&std::path::Path::new(&created).join(&subfolder)).display().to_string();
    Ok(match location_get(client, Some(&joined)).await {
        Ok(_) => (created, joined),
        Err(_) => (created.clone(), created),
    })
}

/// `path.relative(from, to)` for absolute POSIX paths.
fn relative(from: &str, to: &str) -> String {
    let parts = |p: &str| -> Vec<String> {
        normalize(std::path::Path::new(p)).components().filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        }).collect()
    };
    let (a, b) = (parts(from), parts(to));
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut out: Vec<String> = std::iter::repeat_n("..".to_string(), a.len() - common).collect();
    out.extend(b[common..].iter().cloned());
    out.join(std::path::MAIN_SEPARATOR_STR)
}

async fn latest_session(client: &Client, location: &Value) -> anyhow::Result<Option<Value>> {
    let dir = location.get("directory").cloned().unwrap_or(Value::Null);
    let mut cursor: Option<String> = None;
    loop {
        let mut req = Req::new()
            .query("directory", dir.clone())
            .query("parentID", Value::Null)
            .query("archived", json!(false))
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
    Ok(client.call("session.fork", Req::new().path("sessionID", id).body(json!({}))).await.map_err(mutation)?.get("data").cloned().unwrap_or(Value::Null))
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
// prompt execution (`run/noninteractive.ts` `runNonInteractivePrompt`): one prompt to completion,
//
//   admitting → running → settling → waiting for this prompt's jobs → done(exit code)
//   any state + stop signal → cancelling → done

struct Prompt {
    client: Client,
    session_id: String,
    directory: String,
    message: String,
    files: Vec<Value>,
    agent: Option<String>,
    model: Option<Value>,
    thinking: bool,
    format: Format,
    auto: bool,
    /// The server is private to this run, so MCP input requests in its location are this run's too.
    standalone: bool,
    exit: Arc<AtomicI32>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    Running,
    Settled,
    Cancelling,
}

/// A terminal execution outcome (`Pick<SessionMessageIdle, "id" | "outcome" | "error">`).
#[derive(Clone, Debug)]
struct Outcome {
    id: String,
    outcome: &'static str,
    error: Option<Value>,
}

#[derive(Default, Clone)]
struct ToolState {
    id: String,
    timestamp: i64,
    name: String,
    input: Value,
    raw: Option<String>,
    call: Option<Value>,
    metadata: Value,
}

/// The renderer's own state (`renderer(input)`).
#[derive(Default)]
struct Render {
    starts: HashMap<String, (String, i64)>,
    tools: HashMap<String, ToolState>,
    /// `true` once the step ended.
    steps: HashMap<String, bool>,
    /// The session can go idle before the last live events arrive; reconcile prints what they missed.
    printed: HashMap<String, String>,
}

struct State {
    phase: Phase,
    promoted: bool,
    failure: Option<Value>,
    outcomes: Vec<Outcome>,
    /// Sessions in this run's tree that own jobs; an execution ending in one can change background state.
    job_sessions: indexmap::IndexSet<String>,
    owned: HashMap<String, bool>,
}

/// `state.ended`: a deferred replaced each time it resolves (a terminal execution event of a job session), or
/// rejected when the event stream fails.
#[derive(Clone, Default)]
struct Ended {
    generation: u64,
    error: Option<String>,
}

type Admission = futures::future::Shared<futures::future::BoxFuture<'static, Result<Value, String>>>;

struct Shared {
    p: Prompt,
    message_id: String,
    state: Mutex<State>,
    render: tokio::sync::Mutex<Render>,
    ended: tokio::sync::watch::Sender<Ended>,
    admission: Mutex<Option<Admission>>,
    /// Set once the run is over (`controller.abort()`): the stream ending is no failure then.
    closed: AtomicBool,
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

/// JavaScript truthiness of a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn put(m: &mut Map<String, Value>, k: &str, v: Option<&Value>) {
    if let Some(v) = v {
        m.insert(k.into(), v.clone());
    }
}

/// `terminalOutcome(event)`
fn terminal_outcome(event: &Value) -> Option<Outcome> {
    let id = oc_schema::id::message_from_event(event.get("id").and_then(Value::as_str).unwrap_or(""));
    let data = event.get("data").unwrap_or(&Value::Null);
    match event.get("type").and_then(Value::as_str)? {
        "session.execution.succeeded" => Some(Outcome { id, outcome: "succeeded", error: None }),
        "session.execution.failed" => Some(Outcome { id, outcome: "failed", error: data.get("error").cloned() }),
        "session.execution.interrupted" => {
            let reason = match data.get("reason") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => "undefined".into(),
            };
            Some(Outcome {
                id,
                outcome: "interrupted",
                error: Some(json!({"type": "aborted", "message": format!("Session interrupted: {reason}")})),
            })
        }
        _ => None,
    }
}

/// The projected messages after this run's prompt (`projectedMessages`): `(found, messages)`.
struct Projected {
    found: bool,
    messages: Vec<Value>,
    background: bool,
}

/// `finalError(run, state)`: the run's exit code comes from here alone.
fn final_error(run: &Projected, failure: Option<&Value>, outcomes: &[Outcome]) -> Option<Value> {
    if let Some(f) = failure {
        return Some(f.clone());
    }
    if !run.found {
        return Some(json!({"type": "unknown", "message": "Prompt was not promoted"}));
    }
    let idles: Vec<Outcome> = run
        .messages
        .iter()
        .filter(|m| m.get("type").and_then(Value::as_str) == Some("idle"))
        .map(|m| Outcome {
            id: m.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
            outcome: match m.get("outcome").and_then(Value::as_str) {
                Some("succeeded") => "succeeded",
                Some("failed") => "failed",
                Some("interrupted") => "interrupted",
                _ => "unknown",
            },
            error: m.get("error").filter(|e| !e.is_null()).cloned(),
        })
        .collect();
    let recorded: &[Outcome] = if idles.is_empty() { outcomes } else { &idles };
    // The first marker closes this prompt's turn. Later ones count only as replies woken by this prompt's
    // background jobs; otherwise they belong to other prompts, possibly from another client.
    let turns = if run.background { recorded } else { &recorded[..recorded.len().min(1)] };
    let Some(last) = turns.iter().find(|t| t.outcome != "succeeded").or_else(|| turns.first()) else {
        return Some(json!({"type": "unknown", "message": "Session ended without a recorded outcome"}));
    };
    if last.outcome == "succeeded" {
        return None;
    }
    let fallback =
        json!({"type": if last.outcome == "failed" { "unknown" } else { "aborted" }, "message": format!("Session {}", last.outcome)});
    Some(last.error.clone().or_else(|| outcomes.iter().find(|t| t.id == last.id).and_then(|t| t.error.clone())).unwrap_or(fallback))
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

    fn block(line: &str) {
        ui::empty();
        ui::println(&[line]);
        ui::empty();
    }

    // ---- ownership, permissions and forms ---------------------------------------------------------------

    /// `ownsSession`: this run's session and its descendants (subagents' asks belong to the run too).
    async fn owns(&self, session_id: &str) -> bool {
        let mut chain = vec![];
        let mut current = session_id.to_string();
        let result = loop {
            if current == self.p.session_id {
                break true;
            }
            if let Some(v) = self.state.lock().owned.get(&current).copied() {
                break v;
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

    /// `ownsForm`: MCP elicitations belong to the "global" sentinel rather than a session; on a shared server they
    /// may belong to another client.
    async fn owns_form(&self, form: &Value, location: Option<&Value>) -> bool {
        let session = form.get("sessionID").and_then(Value::as_str).unwrap_or("");
        if session == GLOBAL_FORM_SESSION_ID {
            return self.p.standalone && location.and_then(|l| l.get("directory")).and_then(Value::as_str) == Some(self.p.directory.as_str());
        }
        self.owns(session).await
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

    /// `cancelForm`: a cancelled question lets the model continue; any other input request fails the run.
    async fn cancel_form(&self, form: &Value) -> Result<(), String> {
        let question = form.pointer("/metadata/kind").and_then(Value::as_str) == Some("question");
        let session = form.get("sessionID").and_then(Value::as_str).unwrap_or("");
        let mut req = Req::new().path("sessionID", session).path("formID", form.get("id").and_then(Value::as_str).unwrap_or(""));
        if question {
            req = req.query("message", json!(QUESTION_CANCELLED_FEEDBACK));
        }
        // A global form is found through its location.
        if session == GLOBAL_FORM_SESSION_ID {
            req = req.header("x-opencode-directory", crate::client::encode_component(&self.p.directory));
        }
        if let Err(e) = self.p.client.call("session.form.cancel", req).await
            && e.tag() != Some("FormAlreadySettledError")
        {
            return Err(e.to_string());
        }
        if !question {
            let title = match form.get("title") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => "undefined".into(),
            };
            let mut st = self.state.lock();
            if st.failure.is_none() {
                st.failure = Some(json!({"type": "unknown", "message": format!("Cancelled a request for input this run cannot answer: {title}")}));
            }
        }
        Ok(())
    }

    // ---- the event consumer ------------------------------------------------------------------------------

    fn bump(&self) {
        self.ended.send_modify(|e| e.generation += 1);
    }

    /// Wait until the deferred captured at `generation` settles.
    async fn ended(&self, generation: u64) -> Result<(), String> {
        let mut rx = self.ended.subscribe();
        loop {
            {
                let e = rx.borrow_and_update();
                if e.generation > generation {
                    return Ok(());
                }
                if let Some(err) = &e.error {
                    return Err(err.clone());
                }
            }
            if rx.changed().await.is_err() {
                return std::future::pending().await;
            }
        }
    }

    fn generation(&self) -> u64 {
        self.ended.borrow().generation
    }

    async fn consume(self: Arc<Self>, mut stream: EventStream) -> Result<(), String> {
        loop {
            let event = match stream.next().await {
                Some(Ok(e)) => e,
                Some(Err(e)) => {
                    if self.closed.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                    return Err(e.to_string());
                }
                None => {
                    if self.closed.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                    return Err("Event stream disconnected during prompt execution".into());
                }
            };
            let ty = event.get("type").and_then(Value::as_str).unwrap_or("").to_string();
            let data = event.get("data").cloned().unwrap_or(Value::Null);
            if ty == "permission.asked" {
                if self.owns(data.get("sessionID").and_then(Value::as_str).unwrap_or("")).await {
                    self.reply_permission(&data).await;
                }
                continue;
            }
            if ty == "form.created" {
                let form = data.get("form").cloned().unwrap_or(Value::Null);
                if self.owns_form(&form, event.get("location")).await {
                    self.cancel_form(&form).await?;
                }
                continue;
            }
            let Some(session_id) = data.get("sessionID") else { continue };
            let session_id = session_id.as_str().unwrap_or("").to_string();
            let terminal = terminal_outcome(&event);
            let (jobs, phase) = {
                let st = self.state.lock();
                (st.job_sessions.contains(&session_id), st.phase)
            };
            if jobs {
                if ty == "session.execution.started" && phase == Phase::Cancelling {
                    let client = self.p.client.clone();
                    let sid = session_id.clone();
                    tokio::spawn(async move {
                        let _ = client.call("session.interrupt", Req::new().path("sessionID", &sid)).await;
                    });
                }
                if terminal.is_some() {
                    self.bump();
                }
            }
            if session_id != self.p.session_id {
                continue;
            }
            let render = {
                let mut st = self.state.lock();
                if ty == "session.inbox.delivered" && data.get("inboxID").and_then(Value::as_str) == Some(self.message_id.as_str()) {
                    st.promoted = true;
                }
                // A busy period that ends before this prompt is delivered leaves the prompt queued for good.
                if let Some(t) = &terminal {
                    if !st.promoted && t.error.is_some() && st.failure.is_none() {
                        st.failure = t.error.clone();
                    }
                    if st.promoted {
                        st.outcomes.push(t.clone());
                    }
                }
                st.promoted && st.phase == Phase::Running
            };
            if render {
                self.event(&event).await;
            }
        }
    }

    // ---- background jobs ---------------------------------------------------------------------------------

    /// The jobs this prompt started (older ones belong to someone else), or `[]` while one of their wakes is
    /// queued or running; `None` once nothing is left. Work moves job → inbox → active, so reading in that order
    /// can't miss it.
    async fn background(&self) -> Result<Option<Vec<Value>>, String> {
        let admission = self.admission.lock().clone();
        let Some(admission) = admission else { return Ok(None) };
        let admitted = admission.await?;
        let Some(since) = admitted.pointer("/time/created").and_then(Value::as_f64) else { return Ok(None) };
        let client = &self.p.client;
        let jobs = match client
            .call("experimental.job.list", Req::new().query("sessionID", json!(self.p.session_id)).query("descendants", json!(true)))
            .await
        {
            Ok(v) => v.get("data").and_then(Value::as_array).cloned().unwrap_or_default(),
            // Servers older than the job list endpoint answer 404 with no JSON body; they have no jobs to report.
            Err(ClientError::Client { reason: "UnsupportedContentType", .. }) => vec![],
            Err(ClientError::Client { reason: "UnexpectedStatus", status: Some(404), .. }) => vec![],
            Err(e) => return Err(e.to_string()),
        };
        let running: Vec<Value> =
            jobs.into_iter().filter(|j| j.pointer("/time/started").and_then(Value::as_f64).is_some_and(|t| t >= since)).collect();
        let sessions = {
            let mut st = self.state.lock();
            for job in &running {
                if let Some(s) = job.get("sessionID").and_then(Value::as_str) {
                    st.job_sessions.insert(s.to_string());
                }
                if job.get("kind").and_then(Value::as_str) == Some("subagent")
                    && let Some(s) = job.get("childSessionID").and_then(Value::as_str)
                {
                    st.job_sessions.insert(s.to_string());
                }
            }
            st.job_sessions.iter().cloned().collect::<Vec<_>>()
        };
        if !running.is_empty() {
            return Ok(Some(running));
        }
        let inboxes = futures::future::join_all(
            sessions.iter().map(|sid| client.call("session.inbox.list", Req::new().path("sessionID", sid))),
        )
        .await;
        let mut busy = false;
        for inbox in inboxes {
            let inbox = inbox.map_err(|e| e.to_string())?;
            busy |= inbox.get("data").and_then(Value::as_array).is_some_and(|a| !a.is_empty());
        }
        if busy {
            return Ok(Some(running));
        }
        let active = client.call("session.active", Req::new()).await.map_err(|e| e.to_string())?;
        for sid in active.get("data").and_then(Value::as_object).map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
            if self.owns(&sid).await {
                return Ok(Some(running));
            }
        }
        Ok(None)
    }

    async fn projected(&self) -> Result<(bool, Vec<Value>), String> {
        let mut messages = vec![];
        let mut cursor: Option<String> = None;
        loop {
            let mut req = Req::new().path("sessionID", &self.p.session_id).query("limit", json!(200));
            req = match &cursor {
                Some(c) => req.query("cursor", json!(c)),
                None => req.query("order", json!("desc")),
            };
            let page = self.p.client.call("session.message.list", req).await.map_err(|e| e.to_string())?;
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

    // ---- the lifecycle -----------------------------------------------------------------------------------

    /// admitting → running → settling → waiting for this prompt's jobs. `None` when the run was stopped first.
    async fn lifecycle(self: Arc<Self>, stream: EventStream) -> Result<Option<Projected>, String> {
        let client = self.p.client.clone();
        let sid = self.p.session_id.clone();
        if let Some(agent) = &self.p.agent {
            client.call("session.switchAgent", Req::new().path("sessionID", &sid).body(json!({"agent": agent}))).await.map_err(|e| e.to_string())?;
        }
        if let Some(model) = &self.p.model {
            client.call("session.switchModel", Req::new().path("sessionID", &sid).body(json!({"model": model}))).await.map_err(|e| e.to_string())?;
        }
        if crate::signals::received().is_some() {
            return Ok(None);
        }
        // A failed stream rejects whatever wait is pending, since no further end could be seen.
        {
            let me = self.clone();
            tokio::spawn(async move {
                if let Err(e) = me.clone().consume(stream).await {
                    me.ended.send_modify(|s| s.error = Some(e));
                }
            });
        }
        let body = json!({"id": self.message_id, "text": self.p.message, "files": self.p.files, "delivery": "steer"});
        let admission: Admission = {
            let client = client.clone();
            let sid = sid.clone();
            async move {
                client
                    .call("session.prompt", Req::new().path("sessionID", &sid).body(body))
                    .await
                    .map(|v| v.get("data").cloned().unwrap_or(Value::Null))
                    .map_err(|e| e.to_string())
            }
            .boxed()
            .shared()
        };
        *self.admission.lock() = Some(admission.clone());
        admission.await?;
        let (permissions, forms, globals) = tokio::join!(
            client.call("session.permission.list", Req::new().path("sessionID", &sid)),
            client.call("session.form.list", Req::new().path("sessionID", &sid)),
            async {
                if self.p.standalone {
                    client.call("form.list", Req::new().location(&self.p.directory)).await.ok()
                } else {
                    None
                }
            },
        );
        let list = |v: Result<Value, ClientError>| v.ok().and_then(|v| v.get("data").and_then(Value::as_array).cloned()).unwrap_or_default();
        let mut cancels = list(forms);
        if let Some(globals) = &globals {
            let location = globals.get("location");
            for form in globals.get("data").and_then(Value::as_array).cloned().unwrap_or_default() {
                // Only the global forms of this run's location (`ownsForm(...) === true`).
                if form.get("sessionID").and_then(Value::as_str) == Some(GLOBAL_FORM_SESSION_ID) && self.owns_form(&form, location).await {
                    cancels.push(form);
                }
            }
        }
        let permissions = list(permissions);
        let (_, cancelled) = tokio::join!(
            futures::future::join_all(permissions.iter().map(|p| self.reply_permission(p))),
            futures::future::join_all(cancels.iter().map(|f| self.cancel_form(f))),
        );
        cancelled.into_iter().collect::<Result<Vec<_>, _>>()?;

        let mut waited = false;
        let mut shown = 0usize;
        loop {
            let generation = self.generation();
            client.call("experimental.session.wait", Req::new().path("sessionID", &sid)).await.map_err(|e| e.to_string())?;
            if crate::signals::received().is_some() {
                return Ok(None);
            }
            let (found, messages) = self.projected().await?;
            let (failure, outcomes) = {
                let st = self.state.lock();
                (st.failure.is_some(), !st.outcomes.is_empty())
            };
            if failure || outcomes || messages.iter().any(|m| m.get("type").and_then(Value::as_str) == Some("idle")) {
                let pending = if failure { None } else { self.background().await? };
                let Some(pending) = pending else { return Ok(Some(Projected { found, messages, background: waited })) };
                if !pending.is_empty() && pending.len() != shown {
                    self.waiting(&pending);
                }
                if !pending.is_empty() {
                    shown = pending.len();
                }
                waited = true;
            }
            self.ended(generation).await?;
        }
    }

    /// cancelling: a wake can still start after the interrupt (the notice each cancelled job queues), so the
    /// consumer interrupts every execution that starts until the tree is quiet.
    async fn cancel(&self) -> Result<(), String> {
        self.state.lock().phase = Phase::Cancelling;
        let admission = self.admission.lock().clone();
        let Some(admission) = admission else { return Ok(()) };
        if admission.await.is_err() {
            return Ok(());
        }
        let client = &self.p.client;
        let sid = &self.p.session_id;
        if !self.state.lock().promoted {
            let _ = client.call("session.inbox.cancel", Req::new().path("sessionID", sid).path("inboxID", &self.message_id)).await;
        }
        let running = self.background().await.ok().flatten().unwrap_or_default();
        let jobs = running.iter().map(|job| async move {
            match job.get("kind").and_then(Value::as_str) {
                Some("shell") => {
                    let id = job.get("shellID").and_then(Value::as_str).unwrap_or("");
                    let _ = client.call("shell.remove", Req::new().path("id", id).location(&self.p.directory)).await;
                }
                Some("subagent") => {
                    let child = job.get("childSessionID").and_then(Value::as_str).unwrap_or("");
                    let _ = client.call("session.interrupt", Req::new().path("sessionID", child)).await;
                }
                _ => {}
            }
        });
        tokio::join!(
            async {
                let _ = client.call("session.interrupt", Req::new().path("sessionID", sid)).await;
            },
            futures::future::join_all(jobs),
        );
        loop {
            let generation = self.generation();
            client.call("experimental.session.wait", Req::new().path("sessionID", sid)).await.map_err(|e| e.to_string())?;
            let pending = self.background().await?;
            // Image jobs can't be cancelled through the API, so waiting on them would only hold the exit.
            match pending {
                None => return Ok(()),
                Some(p) if !p.is_empty() && p.iter().all(|j| j.get("kind").and_then(Value::as_str) == Some("image")) => return Ok(()),
                _ => {}
            }
            self.ended(generation).await?;
        }
    }

    // ---- the renderer ------------------------------------------------------------------------------------

    fn error(&self, error: &Value) {
        if !self.emit("error", chrono::Utc::now().timestamp_millis(), vec![("error", error.clone())]) {
            ui::error(error.get("message").and_then(Value::as_str).unwrap_or(""));
        }
    }

    fn waiting(&self, jobs: &[Value]) {
        let list: Vec<Value> = jobs
            .iter()
            .map(|j| {
                let mut o = Map::new();
                o.insert("id".into(), j.get("id").cloned().unwrap_or(Value::Null));
                o.insert("kind".into(), j.get("kind").cloned().unwrap_or(Value::Null));
                put(&mut o, "title", j.get("title"));
                Value::Object(o)
            })
            .collect();
        if self.emit("background_wait", chrono::Utc::now().timestamp_millis(), vec![("jobs", Value::Array(list))]) {
            return;
        }
        ui::println(&[&format!(
            "{}waiting for {} background job{}{}",
            ui::TEXT_DIM,
            jobs.len(),
            if jobs.len() == 1 { "" } else { "s" },
            ui::TEXT_NORMAL
        )]);
    }

    /// `text(part, start, end)`: prints the part's text beyond what was printed for it, once.
    fn text(&self, r: &mut Render, kind: &str, id: String, message_id: &str, ordinal: i64, text: &str, metadata: Option<&Value>, start: i64, end: i64) {
        if kind == "reasoning" && !self.p.thinking {
            return;
        }
        let k = key(kind, key(message_id, ordinal));
        let previous = r.printed.get(&k).cloned();
        if let Some(prev) = &previous
            && (prev == text || !text.starts_with(prev.as_str()))
        {
            return;
        }
        r.printed.insert(k, text.to_string());
        let value = &text[previous.as_deref().unwrap_or("").len()..];
        let mut part = Map::new();
        part.insert("id".into(), json!(id));
        part.insert("sessionID".into(), json!(self.p.session_id));
        part.insert("messageID".into(), json!(message_id));
        part.insert("type".into(), json!(kind));
        part.insert("text".into(), json!(value));
        if kind == "reasoning" {
            put(&mut part, "metadata", metadata);
        }
        part.insert("time".into(), json!({"start": start, "end": end}));
        if self.emit(kind, end, vec![("part", Value::Object(part))]) {
            return;
        }
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return;
        }
        let line = if kind == "text" { trimmed.to_string() } else { format!("Thinking: {trimmed}") };
        if !crate::term::stdout_is_tty() {
            write_stdout(&format!("{line}{EOL}"));
            return;
        }
        Self::block(&if kind == "text" { line } else { format!("{}\x1b[3m{line}\x1b[0m{}", ui::TEXT_DIM, ui::TEXT_NORMAL) });
    }

    /// `tool(partID, messageID, item, metadata, timestamp)` for a completed or failed tool.
    fn tool(&self, r: &mut Render, part_id: String, message_id: &str, item: &Value, metadata: Value, timestamp: i64) {
        let tid = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let k = key(message_id, &tid);
        if r.printed.contains_key(&k) {
            return;
        }
        r.printed.insert(k, String::new());
        let name = item.get("name").and_then(Value::as_str).unwrap_or("tool").to_string();
        let state = item.get("state").cloned().unwrap_or(Value::Null);
        let start = item.pointer("/time/ran").filter(|v| !v.is_null()).or_else(|| item.pointer("/time/created")).cloned();
        let end = item.pointer("/time/completed").filter(|v| !v.is_null()).cloned().unwrap_or(json!(timestamp));
        let completed = state.get("status").and_then(Value::as_str) == Some("completed");
        let pstate = if completed {
            json!({"status": "completed", "input": state.get("input"), "output": tool_output_text(&name, state.get("content")), "title": name, "metadata": metadata, "time": {"start": start, "end": end}})
        } else {
            json!({"status": "error", "input": state.get("input"), "error": state.pointer("/error/message"), "metadata": metadata, "time": {"start": start, "end": end}})
        };
        let part = json!({"partID": part_id, "sessionID": self.p.session_id, "messageID": message_id, "type": "tool", "id": tid, "tool": name, "state": pstate});
        if self.emit("tool_use", timestamp, vec![("part", part)]) {
            return;
        }
        if completed {
            render_tool(item, &self.p.directory);
            return;
        }
        if state.get("content").is_some_and(|c| !c.is_null()) && !tool_output_text(&name, state.get("content")).trim().is_empty() {
            let mut shown = item.clone();
            let mut s = Map::new();
            s.insert("status".into(), json!("completed"));
            put(&mut s, "input", state.get("input"));
            put(&mut s, "metadata", state.get("metadata"));
            put(&mut s, "content", state.get("content"));
            shown["state"] = Value::Object(s);
            render_tool(&shown, &self.p.directory);
        }
        render_tool_error(item, &self.p.directory);
        ui::error(state.pointer("/error/message").and_then(Value::as_str).unwrap_or(""));
    }

    fn step(&self, r: &mut Render, start: bool, timestamp: i64, id: String, message_id: &str, rest: Vec<(&str, Option<&Value>)>) -> bool {
        r.steps.insert(message_id.to_string(), !start);
        let mut part = Map::new();
        part.insert("id".into(), json!(id));
        part.insert("sessionID".into(), json!(self.p.session_id));
        part.insert("messageID".into(), json!(message_id));
        part.insert("type".into(), json!(if start { "step-start" } else { "step-finish" }));
        for (k, v) in rest {
            put(&mut part, k, v);
        }
        self.emit(if start { "step_start" } else { "step_finish" }, timestamp, vec![("part", Value::Object(part))])
    }

    /// `render.event(event)`: a live event of this run's session while it runs.
    async fn event(&self, event: &Value) {
        let mut r = self.render.lock().await;
        let r = &mut *r;
        let ty = event.get("type").and_then(Value::as_str).unwrap_or("");
        let data = event.get("data").cloned().unwrap_or(Value::Null);
        let time = to_millis(event.get("created"));
        let event_id = event.get("id").and_then(Value::as_str).unwrap_or("");
        let amid = data.get("assistantMessageID").and_then(Value::as_str).unwrap_or("").to_string();
        match ty {
            "session.retry.scheduled" => {
                let mut retry = Map::new();
                put(&mut retry, "messageID", data.get("assistantMessageID"));
                put(&mut retry, "attempt", data.get("attempt"));
                put(&mut retry, "nextRetryAt", data.get("at"));
                put(&mut retry, "error", data.get("error"));
                let fields: Vec<(&str, Value)> = retry.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                if self.emit("retry", time, fields) {
                    return;
                }
                let attempt = match data.get("attempt") {
                    Some(Value::Number(n)) => n.to_string(),
                    Some(v) => v.to_string(),
                    None => "undefined".into(),
                };
                ui::println(&[
                    &format!("{}!", ui::TEXT_WARNING_BOLD),
                    &format!(
                        "{}retrying (attempt {attempt}): {}",
                        ui::TEXT_NORMAL,
                        data.pointer("/error/message").and_then(Value::as_str).unwrap_or("undefined")
                    ),
                ]);
            }
            // Each retried attempt of a step starts it again.
            "session.step.started" if !r.steps.contains_key(&amid) => {
                if !self.step(r, true, time, part_id(event_id), &amid, vec![("snapshot", data.get("snapshot"))]) {
                    Self::block(&format!(
                        "> {} · {}",
                        data.get("agent").and_then(Value::as_str).unwrap_or(""),
                        data.pointer("/model/id").and_then(Value::as_str).unwrap_or("")
                    ));
                }
            }
            "session.step.ended" => {
                self.step(
                    r,
                    false,
                    time,
                    part_id(event_id),
                    &amid,
                    vec![("reason", data.get("finish")), ("snapshot", data.get("snapshot")), ("cost", data.get("cost")), ("tokens", data.get("tokens"))],
                );
            }
            "session.text.started" | "session.reasoning.started" => {
                let kind = if ty == "session.text.started" { "text" } else { "reasoning" };
                let ordinal = data.get("ordinal").cloned().unwrap_or(Value::Null);
                r.starts.insert(key(kind, key(&amid, ordinal)), (part_id(event_id), time));
            }
            "session.text.ended" | "session.reasoning.ended" => {
                let kind = if ty == "session.text.ended" { "text" } else { "reasoning" };
                let ordinal = data.get("ordinal").cloned().unwrap_or(Value::Null);
                let started = r.starts.get(&key(kind, key(&amid, &ordinal))).cloned();
                let metadata = if kind == "reasoning" { data.get("native").cloned() } else { None };
                let text = data.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                let (id, start) = started.unwrap_or_else(|| (part_id(event_id), time));
                self.text(r, kind, id, &amid, ordinal.as_i64().unwrap_or(0), &text, metadata.as_ref(), start, time);
            }
            "session.tool.input.started" | "session.tool.called" => {
                let k = key(&amid, data.get("id").and_then(Value::as_str).unwrap_or(""));
                let current = r.tools.get(&k).cloned();
                let called = ty == "session.tool.called";
                let mut call = Map::new();
                put(&mut call, "executed", data.get("executed"));
                put(&mut call, "state", data.get("native"));
                r.tools.insert(
                    k,
                    ToolState {
                        id: current.as_ref().map(|c| c.id.clone()).unwrap_or_else(|| part_id(event_id)),
                        timestamp: current.as_ref().map(|c| c.timestamp).unwrap_or(time),
                        name: if called {
                            current.as_ref().map(|c| c.name.clone()).unwrap_or_else(|| "tool".into())
                        } else {
                            data.get("name").and_then(Value::as_str).unwrap_or("").to_string()
                        },
                        input: if called { data.get("input").cloned().unwrap_or(json!({})) } else { json!({}) },
                        raw: current.and_then(|c| c.raw),
                        call: called.then_some(Value::Object(call)),
                        metadata: json!({}),
                    },
                );
            }
            "session.tool.input.delta" | "session.tool.input.ended" | "session.tool.progress" => {
                let k = key(&amid, data.get("id").and_then(Value::as_str).unwrap_or(""));
                if let Some(t) = r.tools.get_mut(&k) {
                    match ty {
                        "session.tool.input.delta" => {
                            t.raw = Some(format!("{}{}", t.raw.clone().unwrap_or_default(), data.get("delta").and_then(Value::as_str).unwrap_or("")))
                        }
                        "session.tool.input.ended" => t.raw = data.get("text").and_then(Value::as_str).map(String::from),
                        _ => t.metadata = data.get("metadata").cloned().unwrap_or(Value::Null),
                    }
                }
            }
            "session.tool.success" | "session.tool.failed" => {
                let success = ty == "session.tool.success";
                let tid = data.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                let current = r.tools.get(&key(&amid, &tid)).cloned().unwrap_or_else(|| ToolState {
                    id: part_id(event_id),
                    timestamp: time,
                    name: "tool".into(),
                    input: json!({}),
                    metadata: json!({}),
                    ..Default::default()
                });
                let mut result = Map::new();
                put(&mut result, "executed", data.get("executed"));
                put(&mut result, "state", data.get("native"));
                let native = match (data.get("native").filter(|n| truthy(n)), current.call.as_ref().and_then(|c| c.get("state"))) {
                    (Some(n), Some(Value::Object(prev))) => {
                        let mut merged = prev.clone();
                        if let Some(o) = n.as_object() {
                            merged.extend(o.clone());
                        }
                        Some(Value::Object(merged))
                    }
                    (Some(n), _) => Some(n.as_object().map(|o| Value::Object(o.clone())).unwrap_or(json!({}))),
                    (None, prev) => prev.cloned(),
                };
                let mut item = Map::new();
                item.insert("type".into(), json!("tool"));
                item.insert("id".into(), json!(tid));
                item.insert("name".into(), json!(current.name));
                put(&mut item, "executed", data.get("executed"));
                put(&mut item, "native", native.as_ref());
                item.insert("time".into(), json!({"created": current.timestamp, "ran": current.timestamp, "completed": time}));
                let mut metadata = Map::new();
                if success {
                    put(&mut metadata, "metadata", data.get("metadata"));
                    put(&mut metadata, "content", data.get("content"));
                }
                put(&mut metadata, "providerCall", current.call.as_ref());
                metadata.insert("providerResult".into(), Value::Object(result));
                put(&mut metadata, "rawInput", current.raw.as_ref().map(|r| json!(r)).as_ref());
                let mut state = Map::new();
                state.insert("status".into(), json!(if success { "completed" } else { "error" }));
                state.insert("input".into(), current.input.clone());
                if success {
                    put(&mut state, "metadata", data.get("metadata"));
                    put(&mut state, "content", data.get("content"));
                } else {
                    let meta = data.get("metadata").filter(|m| !m.is_null()).cloned().unwrap_or(current.metadata.clone());
                    state.insert("metadata".into(), meta);
                    put(&mut state, "content", data.get("content"));
                    put(&mut state, "error", data.get("error"));
                }
                item.insert("state".into(), Value::Object(state));
                self.tool(r, current.id.clone(), &amid, &Value::Object(item), Value::Object(metadata), time);
            }
            _ => {}
        }
    }

    /// `render.reconcile(messages)`: what the live events missed, from the projected messages.
    async fn reconcile(&self, messages: &[Value]) {
        let mut r = self.render.lock().await;
        let r = &mut *r;
        for message in messages {
            if message.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let mid = message.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let created = to_millis(message.pointer("/time/created"));
            let timestamp = message.pointer("/time/completed").filter(|v| !v.is_null()).map(|v| to_millis(Some(v))).unwrap_or(created);
            let seen = r.steps.get(&mid).copied();
            if seen.is_none() {
                self.step(r, true, created, projected_part_id(&mid, "step-start"), &mid, vec![("snapshot", message.pointer("/snapshot/start"))]);
            }
            let (mut text_ordinal, mut reasoning_ordinal) = (0i64, 0i64);
            for item in message.get("content").and_then(Value::as_array).cloned().unwrap_or_default() {
                let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
                if ty == "tool" {
                    let status = item.pointer("/state/status").and_then(Value::as_str).unwrap_or("");
                    if status == "streaming" || status == "running" {
                        continue;
                    }
                    let mut metadata = Map::new();
                    put(&mut metadata, "metadata", item.pointer("/state/metadata"));
                    put(&mut metadata, "content", item.pointer("/state/content"));
                    let tid = item.get("id").and_then(Value::as_str).unwrap_or("");
                    self.tool(r, projected_part_id(&mid, &format!("tool-{tid}")), &mid, &item, Value::Object(metadata), timestamp);
                    continue;
                }
                let ordinal = if ty == "text" { &mut text_ordinal } else { &mut reasoning_ordinal };
                let n = *ordinal;
                *ordinal += 1;
                let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                // A part never printed live and still empty has nothing to show.
                if text.is_empty() && !r.printed.contains_key(&key(ty, key(&mid, n))) {
                    continue;
                }
                let metadata = if ty == "reasoning" { item.get("native").cloned() } else { None };
                self.text(r, ty, projected_part_id(&mid, &format!("{ty}-{n}")), &mid, n, text, metadata.as_ref(), created, timestamp);
            }
            let finish = message.get("finish").filter(|f| truthy(f));
            let tokens = message.get("tokens").filter(|t| truthy(t));
            if seen != Some(true)
                && let (Some(finish), Some(tokens)) = (finish, tokens)
            {
                let cost = message.get("cost").filter(|c| !c.is_null()).cloned().unwrap_or(json!(0));
                self.step(
                    r,
                    false,
                    timestamp,
                    projected_part_id(&mid, "step-finish"),
                    &mid,
                    vec![("reason", Some(finish)), ("snapshot", message.pointer("/snapshot/end")), ("cost", Some(&cost)), ("tokens", Some(tokens))],
                );
            }
        }
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
            state: Mutex::new(State {
                phase: Phase::Running,
                promoted: false,
                failure: None,
                outcomes: vec![],
                job_sessions: [self.session_id.clone()].into_iter().collect(),
                owned: HashMap::new(),
            }),
            p: self,
            render: tokio::sync::Mutex::new(Render::default()),
            ended: tokio::sync::watch::channel(Ended::default()).0,
            admission: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        let result = shared.clone().settle(stream).await;
        // `finally`: the consumer stops reading.
        shared.closed.store(true, Ordering::SeqCst);
        result.map_err(|e| anyhow::anyhow!(e))
    }
}

impl Shared {
    async fn settle(self: Arc<Self>, stream: EventStream) -> Result<(), String> {
        let mut running = tokio::spawn(self.clone().lifecycle(stream));
        let settled = tokio::select! {
            r = &mut running => match r {
                Ok(r) => r?,
                Err(e) => return Err(e.to_string()),
            },
            _ = crate::signals::stopped() => None,
        };
        let Some(settled) = settled else {
            // Requests failing during or after cancellation (e.g. the private server shutting down) are ignored.
            let _ = self.cancel().await;
            return Ok(());
        };
        self.state.lock().phase = Phase::Settled;
        self.reconcile(&settled.messages).await;
        let error = {
            let st = self.state.lock();
            final_error(&settled, st.failure.as_ref(), &st.outcomes)
        };
        if let Some(error) = error {
            self.p.exit.store(1, Ordering::SeqCst);
            self.error(&error);
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
        assert_eq!(merge_input(Some("from args".into()), Some("from stdin".into())).unwrap(), "from args\nfrom stdin");
        assert_eq!(merge_input(None, Some(String::new())), None);
        assert!(is_binary(&[0, 1]));
        assert!(!is_binary(b"hello\n"));
    }

    #[test]
    fn worktree_subfolders() {
        // `path.relative` joins with the platform separator.
        let sep = |s: &str| s.replace('/', std::path::MAIN_SEPARATOR_STR);
        assert_eq!(relative("/p", "/p"), "");
        assert_eq!(relative("/p", "/p/a/b"), sep("a/b"));
        assert_eq!(relative("/p/a", "/p/b"), sep("../b"));
        assert_eq!(relative("/p", "/q"), sep("../q"));
    }

    fn outcome(id: &str, outcome: &'static str, error: Option<Value>) -> Outcome {
        Outcome { id: id.into(), outcome, error }
    }

    #[test]
    fn final_errors() {
        let idle = |id: &str, outcome: &str| json!({"type": "idle", "id": id, "outcome": outcome});
        let run = |messages: Vec<Value>, background: bool| Projected { found: true, messages, background };
        let failure = json!({"type": "unknown", "message": "Cancelled a request for input this run cannot answer: X"});
        assert_eq!(final_error(&run(vec![], false), Some(&failure), &[]), Some(failure));
        assert_eq!(
            final_error(&Projected { found: false, messages: vec![], background: false }, None, &[]),
            Some(json!({"type": "unknown", "message": "Prompt was not promoted"}))
        );
        assert_eq!(final_error(&run(vec![], false), None, &[]), Some(json!({"type": "unknown", "message": "Session ended without a recorded outcome"})));
        assert_eq!(final_error(&run(vec![idle("msg_1", "succeeded"), idle("msg_2", "failed")], false), None, &[]), None);
        // With a background wait, every idle marker counts.
        assert_eq!(
            final_error(&run(vec![idle("msg_1", "succeeded"), idle("msg_2", "failed")], true), None, &[]),
            Some(json!({"type": "unknown", "message": "Session failed"}))
        );
        let err = json!({"type": "provider", "message": "boom"});
        assert_eq!(final_error(&run(vec![idle("msg_1", "failed")], false), None, &[outcome("msg_1", "failed", Some(err.clone()))]), Some(err));
        assert_eq!(
            final_error(&run(vec![idle("msg_1", "interrupted")], false), None, &[]),
            Some(json!({"type": "aborted", "message": "Session interrupted"}))
        );
        // Without stored markers, the live outcomes decide.
        assert_eq!(final_error(&run(vec![], false), None, &[outcome("msg_1", "succeeded", None)]), None);
    }

    #[test]
    fn terminal_outcomes() {
        let e = json!({"id": "evt_1", "type": "session.execution.interrupted", "data": {"sessionID": "ses_1", "reason": "user"}});
        let o = terminal_outcome(&e).unwrap();
        assert_eq!((o.id.as_str(), o.outcome), ("msg_1", "interrupted"));
        assert_eq!(o.error, Some(json!({"type": "aborted", "message": "Session interrupted: user"})));
        assert!(terminal_outcome(&json!({"id": "evt_2", "type": "session.step.ended", "data": {}})).is_none());
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
