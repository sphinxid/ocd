//! Schedule checks (`core/src/schedule/check.ts`): a shell command or a Code Mode program that gates a schedule,
//! run with no model call before it fires.
//!
//! A check is authorized once at creation while the user is present (permissions asked without `save`), then each
//! tick runs it under narrow `Unattended` rules: an ask the rules do not settle is denied with `Permission <action>
//! needs approval, and nobody is present to give it`.

use std::collections::HashSet;
use std::time::Duration;

use serde_json::{Map, Value, json};

use oc_codemode::codemode::{ExecResult, ExecutionLimits};
use oc_codemode::interp::errors::DiagnosticKind;
use oc_schema::types::{Effect, PermissionRule, SessionInfo};

use super::{ScheduleError, Schedules, StepError};
use crate::permission::{AssertInput, PermissionError};

pub const TIMEOUT_MS: u64 = 30_000;
const MAX_OUTPUT_BYTES: usize = 32 * 1024;

/// `CODE_LIMITS`
pub fn code_limits() -> ExecutionLimits {
    ExecutionLimits { timeout_ms: Some(TIMEOUT_MS as f64), max_tool_calls: Some(20.0), max_output_bytes: Some(MAX_OUTPUT_BYTES as f64) }
}

/// Tools a check may not call.
pub const FORBIDDEN_CODE_TOOLS: [&str; 8] = [
    "opencode.schedule_create",
    "opencode.schedule_pause",
    "opencode.schedule_resume",
    "opencode.schedule_delete",
    "opencode.session_move",
    "opencode.session_rename",
    "opencode.worktree_create",
    "opencode.worktree_remove",
];

fn forbidden(path: &str) -> String {
    format!("Tool {path} cannot be used inside a schedule check")
}

/// The result of one check run.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Ok { wake: bool, state: Value, message: Option<String> },
    Failed { error: String, invalid: bool },
}

impl Outcome {
    fn failed(error: impl Into<String>) -> Outcome {
        Outcome::Failed { error: error.into(), invalid: false }
    }
}

/// `new RegExp(pattern)`'s error message as Bun reports it, from acorn's reason for the same source.
fn regexp_message(reason: &str) -> String {
    let text = match reason {
        "Unterminated group" => "missing )",
        "Unmatched ')'" => "unmatched parentheses",
        "Unterminated character class" => "missing terminating ] for character class",
        "Nothing to repeat" | "Lone quantifier brackets" => "nothing to repeat",
        "Duplicate capture group name" => "duplicate group specifier name",
        "Range out of order in character class" => "range out of order in character class",
        "Invalid named capture referenced" | "Invalid named reference" => "invalid \\k<> named backreference",
        "Invalid capture group name" => "invalid group specifier name",
        "Invalid group" => "unrecognized character after (?",
        "Invalid quantifier" => "invalid quantifier",
        "Incomplete quantifier" => "incomplete quantifier",
        "Invalid escape" => "invalid escaped character for unicode pattern",
        other => return format!("Invalid regular expression: {}", other.to_lowercase()),
    };
    format!("Invalid regular expression: {text}")
}

fn compile_check(pattern: &str) -> Result<(), String> {
    oc_codemode::parser::validate_regexp(pattern, "").map_err(|reason| regexp_message(&reason))
}

/// `validate(condition)`: condition syntax, checked before asking for permissions or spawning anything.
pub fn validate(condition: &Value) -> Result<(), String> {
    match condition.get("type").and_then(Value::as_str) {
        Some("code") => {
            let code = condition.get("code").and_then(Value::as_str).unwrap_or_default();
            let paths = oc_codemode::scan::scan_tools(code).map_err(|d| d.message)?;
            if let Some(path) = paths.iter().find(|p| FORBIDDEN_CODE_TOOLS.contains(&p.as_str())) {
                return Err(forbidden(path));
            }
            Ok(())
        }
        _ => {
            let run = condition.get("run").and_then(Value::as_str).unwrap_or_default();
            if run.trim().is_empty() {
                return Err("A shell check needs a command".into());
            }
            let predicates = ["is", "matches", "changes"].iter().filter(|k| condition.get(**k).is_some_and(|v| !v.is_null())).count();
            if predicates > 1 {
                return Err("Choose at most one of is, matches or changes".into());
            }
            let Some(matches) = condition.get("matches").and_then(Value::as_str) else { return Ok(()) };
            compile_check(matches).map_err(|message| format!("Invalid regular expression {}: {message}", Value::from(matches)))
        }
    }
}

fn first_line(code: &str) -> Option<String> {
    code.split('\n')
        .map(js_trim)
        .find(|l| !l.is_empty())
        .map(|l| l.encode_utf16().take(60).collect::<Vec<u16>>())
        .map(|u| String::from_utf16_lossy(&u))
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| (c as u32) < 0x10000 && oc_codemode::number::is_js_whitespace(c as u16))
}

/// `describe(condition)`: the condition in a timeout note.
pub fn describe(condition: &Value) -> String {
    match condition.get("type").and_then(Value::as_str) {
        Some("code") => match first_line(condition.get("code").and_then(Value::as_str).unwrap_or_default()) {
            Some(line) => format!("code ({line})"),
            None => "code".into(),
        },
        _ => condition.get("run").and_then(Value::as_str).unwrap_or_default().to_string(),
    }
}

/// `formatCondition` of the schedule tools' summaries.
pub fn format_condition(condition: &Value) -> String {
    match condition.get("type").and_then(Value::as_str) {
        Some("code") => {
            let first = first_line(condition.get("code").and_then(Value::as_str).unwrap_or_default()).unwrap_or_default();
            format!("code {}", Value::from(first))
        }
        _ => Value::from(condition.get("run").and_then(Value::as_str).unwrap_or_default()).to_string(),
    }
}

fn dedupe(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items.into_iter().filter(|i| seen.insert(i.clone())).collect()
}

fn allow(action: &str, resource: &str) -> PermissionRule {
    PermissionRule { action: action.into(), resource: resource.into(), effect: Effect::Allow }
}

/// The session's environment for checks: its location must exist, then the plugins activate.
async fn env(schedules: &Schedules, session: &SessionInfo) -> Result<crate::runner::CheckEnv, StepError> {
    let directory = &session.location.directory;
    if !std::path::Path::new(directory).is_dir() {
        return Err(StepError::DirectoryNotFound(directory.clone()));
    }
    let engine = schedules.engine().ok_or_else(|| StepError::Other("Session runner is not available".into()))?;
    engine.check_env(session).await.map_err(|e| StepError::Other(e.to_string()))
}

fn assert_input(session: &SessionInfo, agent: &str, action: &str, resources: Vec<String>, metadata: Option<Map<String, Value>>) -> AssertInput {
    AssertInput {
        id: None,
        session_id: session.id.clone(),
        action: action.into(),
        resources,
        save: None,
        metadata,
        source: None,
        agent: Some(agent.into()),
    }
}

/// `authorize`: runs in the session's location while the user is present; approves the required permissions once
/// without saving them.
pub async fn authorize(
    schedules: &Schedules,
    session: &SessionInfo,
    name: Option<&str>,
    when: &Value,
    mode: &str,
    condition: &Value,
) -> Result<(), ScheduleError> {
    let env = env(schedules, session).await.map_err(|e| match e {
        StepError::DirectoryNotFound(dir) => ScheduleError::Invalid(format!("Session directory does not exist: {dir}")),
        StepError::Other(message) => ScheduleError::Invalid(message),
    })?;
    let mut metadata = Map::new();
    if let Some(name) = name {
        metadata.insert("schedule".into(), json!(name));
    }
    metadata.insert("mode".into(), json!(mode));
    metadata.insert("when".into(), super::encode_when(when));
    metadata.insert("unattended".into(), json!(true));
    let app = schedules.app().ok_or(ScheduleError::Declined)?;
    let permissions = &app.permissions;
    match condition.get("type").and_then(Value::as_str) {
        Some("code") => {
            let code = condition.get("code").and_then(Value::as_str).unwrap_or_default();
            let paths = oc_codemode::scan::scan_tools(code).map_err(|d| ScheduleError::Invalid(d.message))?;
            permissions
                .assert_not_denied(assert_input(session, &env.agent_id, "execute", vec!["*".into()], None))
                .await
                .map_err(ScheduleError::from_permission)?;
            let resolved = resolve_code_tools(&env, &paths).await.map_err(ScheduleError::Invalid)?;
            let actions = dedupe(resolved.iter().map(|(id, tool)| tool.options().permission.unwrap_or_else(|| id.clone())));
            metadata.insert("code".into(), json!(code));
            metadata.insert("tools".into(), json!(if actions.is_empty() { Vec::new() } else { paths.clone() }));
            if actions.is_empty() {
                return permissions
                    .assert(assert_input(session, &env.agent_id, "execute", vec!["*".into()], Some(metadata)))
                    .await
                    .map_err(ScheduleError::from_permission);
            }
            // Fail fast on any explicit deny before asking the user to approve earlier tools.
            for action in &actions {
                permissions
                    .assert_not_denied(assert_input(session, &env.agent_id, action, vec!["*".into()], None))
                    .await
                    .map_err(ScheduleError::from_permission)?;
            }
            for action in &actions {
                // No `save`: "always" would save a project-wide allow, while scheduled ticks run unattended.
                permissions
                    .assert(assert_input(session, &env.agent_id, action, vec!["*".into()], Some(metadata.clone())))
                    .await
                    .map_err(ScheduleError::from_permission)?;
            }
            Ok(())
        }
        _ => {
            let run = condition.get("run").and_then(Value::as_str).unwrap_or_default();
            let scanned = scan_shell(&env, run).map_err(ScheduleError::Invalid)?;
            metadata.insert("run".into(), json!(run));
            assert_shell(&app, session, &env.agent_id, &scanned, Some(metadata)).await.map_err(ScheduleError::from_permission)
        }
    }
}

/// `evaluate`: runs one check in the session's location under narrow unattended permissions and a 30 s cap.
pub async fn evaluate(
    schedules: &Schedules,
    session: &SessionInfo,
    condition: &Value,
    prev_state: &Value,
    message_id: &str,
    now: i64,
    checks: i64,
) -> Result<Outcome, StepError> {
    let env = env(schedules, session).await?;
    let run = async {
        match condition.get("type").and_then(Value::as_str) {
            Some("code") => run_code(schedules, &env, session, condition, prev_state, message_id, now, checks).await,
            _ => run_shell(schedules, &env, session, condition, prev_state).await,
        }
    };
    match tokio::time::timeout(Duration::from_millis(TIMEOUT_MS), run).await {
        Ok(outcome) => Ok(outcome),
        Err(_) => Ok(Outcome::failed(format!("Check timed out after {TIMEOUT_MS}ms"))),
    }
}

/// `resolveCodeTools`: the registered Code Mode tools at `paths` (id, tool).
async fn resolve_code_tools(env: &crate::runner::CheckEnv, paths: &[String]) -> Result<Vec<(String, std::sync::Arc<dyn oc_tools::Tool>)>, String> {
    if !paths.is_empty() {
        env.flush_mcp().await;
    }
    let registered = env.registry.list();
    let mut resolved = Vec::new();
    for path in paths {
        if FORBIDDEN_CODE_TOOLS.contains(&path.as_str()) {
            return Err(forbidden(path));
        }
        let Some(found) = registered.iter().find(|(_, t)| t.options().codemode && crate::codemode::qualified_name(t.as_ref()) == *path) else {
            return Err(format!("Unknown Code Mode tool: {path}"));
        };
        if found.1.options().permission.as_deref() == Some("edit") {
            return Err(forbidden(path));
        }
        resolved.push(found.clone());
    }
    Ok(resolved)
}

#[allow(clippy::too_many_arguments)]
async fn run_code(
    schedules: &Schedules,
    env: &crate::runner::CheckEnv,
    session: &SessionInfo,
    condition: &Value,
    prev_state: &Value,
    message_id: &str,
    now: i64,
    checks: i64,
) -> Outcome {
    let Some(agent) = env.agent.clone() else { return Outcome::failed(format!("Agent not found: {}", env.agent_id)) };
    let Some(app) = schedules.app() else { return Outcome::failed("Session runner is not available") };
    let code = condition.get("code").and_then(Value::as_str).unwrap_or_default();
    let paths = match oc_codemode::scan::scan_tools(code) {
        Ok(paths) => paths,
        Err(d) => return Outcome::failed(d.message),
    };
    let resolved = match resolve_code_tools(env, &paths).await {
        Ok(r) => r,
        Err(error) => return Outcome::failed(error),
    };
    let actions = dedupe(resolved.iter().map(|(id, tool)| tool.options().permission.unwrap_or_else(|| id.clone())));
    let mut all = vec!["execute".to_string()];
    all.extend(actions);
    let unattended: Vec<PermissionRule> = all.iter().map(|a| allow(a, "*")).collect();
    let not_denied = crate::permission::unattended(unattended.clone(), async {
        for action in &all {
            app.permissions.assert_not_denied(assert_input(session, &agent.id, action, vec!["*".into()], None)).await?;
        }
        Ok::<(), PermissionError>(())
    })
    .await;
    if let Err(error) = not_denied {
        return Outcome::failed(error.to_string());
    }
    let bindings = vec![
        ("state".to_string(), prev_state.clone()),
        ("now".to_string(), json!(now)),
        ("runs".to_string(), json!(checks)),
        ("input".to_string(), json!({ "state": prev_state, "now": now, "runs": checks })),
    ];
    let permissions = crate::permission::merge(&[&agent.permissions, session.permissions.as_deref().unwrap_or(&[])]);
    let engine = match schedules.engine() {
        Some(e) => e,
        None => return Outcome::failed("Session runner is not available"),
    };
    let result = crate::permission::unattended(
        unattended,
        engine.run_check(env, session, &agent.id, message_id, code, &paths, bindings, &permissions, code_limits()),
    )
    .await;
    match result {
        ExecResult::Failure { error, .. } => {
            let invalid = matches!(
                error.kind,
                DiagnosticKind::ParseError | DiagnosticKind::UnsupportedSyntax | DiagnosticKind::UnknownTool | DiagnosticKind::InvalidToolInput
            ) || error.message.contains("needs approval");
            Outcome::Failed { error: error.message, invalid }
        }
        ExecResult::Success { value: Value::Bool(wake), .. } => Outcome::Ok { wake, state: prev_state.clone(), message: None },
        ExecResult::Success { value, .. } => match decode_result(&value) {
            Err(failure) => Outcome::Failed {
                error: format!("A code check must return a boolean or {{ wake, state?, message? }}: SchemaError({failure})"),
                invalid: true,
            },
            Ok((wake, state, message)) => {
                let state = state.unwrap_or_else(|| prev_state.clone());
                let message = message.map(|m| js_trim(&m).to_string()).filter(|m| !m.is_empty());
                Outcome::Ok { wake, state, message: if wake { message } else { None } }
            }
        },
    }
}

/// `decodeCodeObjectResult`: `{ wake: boolean, state?: Json, message?: string }`, first issue only.
fn decode_result(value: &Value) -> Result<(bool, Option<Value>, Option<String>), String> {
    let Value::Object(map) = value else { return Err("Expected object".into()) };
    let wake = match map.get("wake") {
        None => return Err("Missing key\n  at [\"wake\"]".into()),
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err("Expected boolean\n  at [\"wake\"]".into()),
    };
    let state = map.get("state").cloned();
    let message = match map.get("message") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err("Expected string\n  at [\"message\"]".into()),
    };
    Ok((wake, state, message))
}

/// `scanShell(command)`
pub(crate) struct ScannedShell {
    shell: String,
    cwd: String,
    external: Vec<String>,
    commands: Vec<String>,
}

fn scan_shell(env: &crate::runner::CheckEnv, command: &str) -> Result<ScannedShell, String> {
    use oc_tools::file_access::{Kind, resolve_in, resolve_path};
    let directory = env.location.directory.clone();
    let directory_text = directory.to_string_lossy().into_owned();
    let worktree = env.location.project.canonical.clone();
    let home = env.home.clone();
    let target = resolve_in(&directory, &worktree, &home, &directory_text, Some(Kind::Directory));
    let shell = oc_tools::shell::select::resolve(oc_tools::shell::select::Priority::Compat, env.shell.as_deref(), Some(&env.bin));
    let cwd = target.absolute.to_string_lossy().into_owned();
    let parsed = oc_tools::shell::parse::scan(command, &shell, &cwd, &home)?;
    let directories: Vec<_> = parsed
        .directories
        .iter()
        .map(|d| {
            let absolute = resolve_path(&target.absolute, d, &home);
            resolve_in(&directory, &worktree, &home, &absolute.to_string_lossy(), Some(Kind::Directory))
        })
        .collect();
    let external =
        dedupe(std::iter::once(&target).chain(directories.iter()).filter_map(|t| t.external_directory.as_ref().map(|e| e.resource.clone())));
    let commands = dedupe(parsed.commands.iter().map(|c| c.resource.clone()));
    Ok(ScannedShell { shell, cwd, external, commands })
}

async fn assert_shell(
    app: &crate::app::App,
    session: &SessionInfo,
    agent: &str,
    scanned: &ScannedShell,
    metadata: Option<Map<String, Value>>,
) -> Result<(), PermissionError> {
    if !scanned.external.is_empty() {
        app.permissions.assert(assert_input(session, agent, "external_directory", scanned.external.clone(), metadata.clone())).await?;
    }
    if !scanned.commands.is_empty() {
        app.permissions.assert(assert_input(session, agent, "shell", scanned.commands.clone(), metadata)).await?;
    }
    Ok(())
}

/// Reads at most `max` bytes of a stream, then drains the rest.
async fn collect(mut stream: impl tokio::io::AsyncRead + Unpin, max: usize) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return out,
            Ok(n) => {
                let room = max.saturating_sub(out.len());
                out.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
}

fn utf8_decode(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).into_owned();
    match text.strip_suffix('\u{FFFD}') {
        Some(t) => t.to_string(),
        None => text,
    }
}

async fn run_shell(schedules: &Schedules, env: &crate::runner::CheckEnv, session: &SessionInfo, condition: &Value, prev_state: &Value) -> Outcome {
    let Some(app) = schedules.app() else { return Outcome::failed("Session runner is not available") };
    let run = condition.get("run").and_then(Value::as_str).unwrap_or_default();
    let scanned = match scan_shell(env, run) {
        Ok(s) => s,
        Err(error) => return Outcome::failed(error),
    };
    let mut unattended: Vec<PermissionRule> = scanned.external.iter().map(|r| allow("external_directory", r)).collect();
    unattended.extend(scanned.commands.iter().map(|r| allow("shell", r)));
    let authorized = crate::permission::unattended(unattended, assert_shell(&app, session, &env.agent_id, &scanned, None)).await;
    if let Err(error) = authorized {
        return Outcome::failed(error.to_string());
    }

    let mut command = tokio::process::Command::new(&scanned.shell);
    command
        .args(oc_tools::shell::select::args(&scanned.shell, run))
        .current_dir(&scanned.cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return Outcome::failed(e.to_string()),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (stdout, stderr, status) = tokio::join!(
        async {
            match stdout {
                Some(s) => collect(s, MAX_OUTPUT_BYTES).await,
                None => Vec::new(),
            }
        },
        async {
            match stderr {
                Some(s) => collect(s, MAX_OUTPUT_BYTES).await,
                None => Vec::new(),
            }
        },
        child.wait()
    );
    let exit_code = match status {
        Ok(status) => status.code().unwrap_or(-1),
        Err(e) => return Outcome::failed(e.to_string()),
    };
    let stdout = utf8_decode(&stdout);
    let stderr = utf8_decode(&stderr);
    let trimmed = js_trim(&stdout).to_string();
    let message = (!trimmed.is_empty()).then(|| trimmed.clone());
    let is = condition.get("is").and_then(Value::as_str);
    let matches = condition.get("matches").and_then(Value::as_str);
    let changes = condition.get("changes") == Some(&Value::Bool(true));
    if is.is_none() && matches.is_none() && !changes {
        let wake = exit_code == 0;
        return Outcome::Ok { wake, state: Value::Null, message: if wake { message } else { None } };
    }
    if exit_code != 0 {
        let detail = match js_trim(&stderr) {
            "" => trimmed.clone(),
            d => d.to_string(),
        };
        return Outcome::failed(if detail.is_empty() {
            format!("Command exited with code {exit_code}")
        } else {
            format!("Command exited with code {exit_code}: {detail}")
        });
    }
    if let Some(is) = is {
        let wake = trimmed == js_trim(is);
        return Outcome::Ok { wake, state: Value::Null, message: if wake { message } else { None } };
    }
    if let Some(pattern) = matches {
        let wake = regress::Regex::with_flags(pattern, "").map(|re| re.find(&stdout).is_some()).unwrap_or(false);
        return Outcome::Ok { wake, state: Value::Null, message: if wake { message } else { None } };
    }
    let previous = prev_state.get("stdout").and_then(Value::as_str);
    let wake = previous.is_some_and(|p| p != stdout);
    Outcome::Ok { wake, state: json!({ "stdout": stdout }), message: if wake { message } else { None } }
}
