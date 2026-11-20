//! Code actions (`core/src/automation/code.ts`): a Code Mode program an automation runs on each trigger with no
//! model call and no `fetch`.
//!
//! The tools a program calls are found statically (`tools.<path>(...)`) and authorized once at creation while the
//! user is present (permissions asked without `save`). Each run then executes under narrow `Unattended` rules: an
//! ask the rules do not settle is denied with `Permission <action> needs approval, and nobody is present to give
//! it`. A program receives `input = { state, now, runs, args, event? }` (also as globals; `event` = `{id, timestamp,
//! data}` for an event trigger) and returns nothing or
//! `{ state?, done?, send? }`.

use std::collections::HashSet;
use std::time::Duration;

use serde_json::{Map, Value, json};

use oc_codemode::codemode::{ExecResult, ExecutionLimits};
use oc_codemode::interp::errors::DiagnosticKind;
use oc_schema::types::{Effect, PermissionRule, SessionInfo};

use super::{AutomationError, Automations, Invocation, matches_author};
use crate::permission::{AssertInput, PermissionError};

pub const TIMEOUT_MS: u64 = 30_000;
const MAX_OUTPUT_BYTES: usize = 32 * 1024;

/// `CODE_LIMITS`
pub fn code_limits() -> ExecutionLimits {
    ExecutionLimits { timeout_ms: Some(TIMEOUT_MS as f64), max_tool_calls: Some(20.0), max_output_bytes: Some(MAX_OUTPUT_BYTES as f64) }
}

/// `boundedOutput(text)`: at most 32 KB of UTF-8, cut at a character and marked with `…`.
pub fn bounded_output(text: &str) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_OUTPUT_BYTES - "…".len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// `handoffOutput(delivery, logs?)`: keeps the delivery visible first, including when the logs use the entire byte
/// budget.
pub fn handoff_output(delivery: &str, logs: Option<&str>) -> String {
    match logs {
        None => bounded_output(delivery),
        Some(logs) => bounded_output(&format!("{delivery}\n{logs}")),
    }
}

/// Tools that must not receive an unattended grant from an automation.
pub const FORBIDDEN_TOOLS: [&str; 9] = [
    // Approving the shell tool unattended would approve every command, not a particular command.
    "shell",
    "opencode.automation_create",
    "opencode.automation_pause",
    "opencode.automation_resume",
    "opencode.automation_delete",
    "opencode.session_move",
    "opencode.session_rename",
    "opencode.worktree_create",
    "opencode.worktree_remove",
];

/// What one run receives.
#[derive(Debug, Clone)]
pub struct RunInput {
    /// `msg_<automation>_<run>`: the run's tool calls get `call_<messageID>_<n>`.
    pub message_id: String,
    pub state: Value,
    pub now: i64,
    pub runs: i64,
    /// The event an event trigger fired on (`AutomationEvent.input`: `{id, timestamp, data}`).
    pub event: Option<Value>,
}

/// `{ send: { prompt, to?: { subagent } } }`
#[derive(Debug, Clone, PartialEq)]
pub struct Send {
    pub prompt: String,
    pub subagent: Option<String>,
}

/// The result of one run.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// `state` is `None` when the program left it out (the stored state is kept).
    Ok { state: Option<Value>, done: bool, send: Option<Send>, output: Option<String> },
    /// `invalid`: the program cannot succeed on a retry, e.g. a parse error or an unknown tool.
    Failed { error: String, output: Option<String>, invalid: bool },
}

impl Outcome {
    fn failed(error: impl Into<String>) -> Outcome {
        Outcome::Failed { error: error.into(), output: None, invalid: false }
    }

    fn invalid(error: impl Into<String>) -> Outcome {
        Outcome::Failed { error: error.into(), output: None, invalid: true }
    }
}

/// `scan(code)`: the tools the program calls, or why it cannot be an automation: it does not parse, or reaches a
/// forbidden tool.
pub fn scan(code: &str) -> Result<Vec<String>, String> {
    let paths = oc_codemode::scan::scan_tools(code).map_err(|d| d.message)?;
    if let Some(path) = paths.iter().find(|p| FORBIDDEN_TOOLS.contains(&p.as_str())) {
        return Err(format!("Tool {path} cannot be used inside an automation"));
    }
    Ok(paths)
}

fn dedupe(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items.into_iter().filter(|i| seen.insert(i.clone())).collect()
}

fn allow(action: &str, resource: &str) -> PermissionRule {
    PermissionRule { action: action.into(), resource: resource.into(), effect: Effect::Allow }
}

/// `scopedRules(rules, action)`: rules for `action` that name specific resources rather than `*`, in order.
fn scoped_rules(rules: &[PermissionRule], action: &str) -> Vec<PermissionRule> {
    rules.iter().filter(|r| r.resource != "*" && crate::wildcard::matches(action, &r.action)).cloned().collect()
}

/// `resolveTools(paths)`: the permission each registered Code Mode tool at `paths` asks for, deduplicated.
async fn resolve_tools(env: &crate::runner::CheckEnv, paths: &[String]) -> Result<Vec<String>, String> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    env.flush_mcp().await;
    let registered = env.registry.list();
    let mut actions = Vec::new();
    for path in paths {
        let Some((id, tool)) = registered.iter().find(|(_, t)| t.options().codemode && crate::codemode::qualified_name(t.as_ref()) == *path) else {
            return Err(format!("Unknown Code Mode tool: {path}"));
        };
        actions.push(tool.options().permission.unwrap_or_else(|| id.clone()));
    }
    Ok(dedupe(actions))
}

fn assert_input(
    session: &SessionInfo,
    agent: &str,
    invocation: Option<&Invocation>,
    action: &str,
    resources: Vec<String>,
    metadata: Option<Map<String, Value>>,
) -> AssertInput {
    AssertInput {
        id: None,
        session_id: invocation.map(|i| i.session_id.clone()).unwrap_or_else(|| session.id.clone()),
        action: action.into(),
        resources,
        save: None,
        metadata,
        source: invocation.map(|i| i.source.clone()),
        agent: Some(invocation.map(|i| i.agent.clone()).unwrap_or_else(|| agent.to_string())),
    }
}

/// `authorize`: runs in the creating session's location while the user is present. Asks once for each tool's
/// permission (and for `execute` unless a tool asked), without saving, so every run can be allowed unattended.
pub async fn authorize(
    automations: &Automations,
    session: &SessionInfo,
    name: &str,
    trigger: &Value,
    action: &Value,
    invocation: Option<&Invocation>,
    directory: &str,
) -> Result<(), AutomationError> {
    let code = action.get("code").and_then(Value::as_str).unwrap_or_default();
    let paths = scan(code).map_err(AutomationError::Invalid)?;
    let env = automations.env(session, directory).await?;
    if let Some(invocation) = invocation
        && !matches_author(session, env.agent.as_ref().map(|a| a.id.as_str()), invocation)
    {
        return Err(AutomationError::Invalid("Create the automation with the session's current agent".into()));
    }
    let actions = resolve_tools(&env, &paths).await.map_err(AutomationError::Invalid)?;
    let app = automations.app().ok_or(AutomationError::Declined)?;
    let agent_rules = env.agent.as_ref().map(|a| a.permissions.clone()).unwrap_or_default();
    let rules = crate::permission::merge(&[&agent_rules, session.permissions.as_deref().unwrap_or(&[])]);
    // A rule scoped to some resources (`vault:*: ask`) must prompt too, not only the catch-all `*`.
    let resources = |item: &str| -> Vec<String> {
        dedupe(std::iter::once("*".to_string()).chain(scoped_rules(&rules, item).into_iter().filter(|r| r.effect == Effect::Ask).map(|r| r.resource)))
    };
    let request = |item: &str, metadata: Option<Map<String, Value>>| assert_input(session, &env.agent_id, invocation, item, resources(item), metadata);
    let mut all = vec!["execute".to_string()];
    all.extend(actions.iter().cloned());
    // Fail fast on any explicit deny before asking the user to approve earlier tools.
    for item in &all {
        app.permissions.assert_not_denied(request(item, None)).await.map_err(AutomationError::from_permission)?;
    }
    // A tool's approval also covers running the program; with no tool to ask about, `execute` itself must be
    // approved.
    let mut allowed = true;
    for item in &actions {
        allowed &= app.permissions.allows(request(item, None)).await;
    }
    let mut metadata = Map::new();
    metadata.insert("automation".into(), json!(name));
    metadata.insert("trigger".into(), trigger.clone());
    metadata.insert("unattended".into(), json!(true));
    metadata.insert("code".into(), json!(code));
    metadata.insert("tools".into(), json!(paths));
    for item in if allowed { &all } else { &actions } {
        // No `save`: "always" would save a project-wide allow, while runs are allowed through Unattended.
        app.permissions.assert(request(item, Some(metadata.clone()))).await.map_err(AutomationError::from_permission)?;
    }
    Ok(())
}

/// The issue of `Schema.decodeUnknownResult(Schema.NullOr(CodeResult), { onExcessProperty: "error" })`, first one
/// only, as Effect words it.
fn issue(message: &str, path: &[&str]) -> String {
    if path.is_empty() {
        return message.to_string();
    }
    let at: String = path.iter().map(|k| format!("[{}]", Value::from(*k))).collect();
    format!("{message}\n  at {at}")
}

fn has_non_space(text: &str) -> bool {
    text.encode_utf16().any(|c| !oc_codemode::number::is_js_whitespace(c))
}

/// A struct's fields: the first excess property (in input order), else `None`.
fn excess<'a>(map: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    map.keys().find(|k| !keys.contains(&k.as_str())).map(String::as_str)
}

/// A non-blank string field (`Schema.String.check(Schema.isPattern(/\S/))`).
fn non_blank(map: &Map<String, Value>, key: &str, path: &[&str]) -> Result<String, String> {
    let at: Vec<&str> = path.iter().copied().chain(std::iter::once(key)).collect();
    match map.get(key) {
        None => Err(issue("Missing key", &at)),
        Some(Value::String(s)) if has_non_space(s) => Ok(s.clone()),
        Some(Value::String(_)) => Err(issue("Expected a string matching the RegExp \\S", &at)),
        Some(_) => Err(issue("Expected string", &at)),
    }
}

/// `Automation.decodeCodeResult`: the state (if returned), `done` and the send.
pub fn decode_result(value: &Value) -> Result<(Option<Value>, bool, Option<Send>), String> {
    let map = match value {
        Value::Null => return Ok((None, false, None)),
        Value::Object(map) => map,
        _ => return Err(issue("Expected Automation.CodeResult | null", &[])),
    };
    if let Some(key) = excess(map, &["state", "done", "send"]) {
        return Err(issue("Expected no excess property", &[key]));
    }
    let done = match map.get("done") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(issue("Expected boolean", &["done"])),
    };
    let send = match map.get("send") {
        None => None,
        Some(Value::Object(send)) => {
            if let Some(key) = excess(send, &["prompt", "to"]) {
                return Err(issue("Expected no excess property", &["send", key]));
            }
            let prompt = non_blank(send, "prompt", &["send"])?;
            let subagent = match send.get("to") {
                None => None,
                Some(Value::Object(to)) => {
                    if let Some(key) = excess(to, &["subagent"]) {
                        return Err(issue("Expected no excess property", &["send", "to", key]));
                    }
                    Some(non_blank(to, "subagent", &["send", "to"])?)
                }
                Some(_) => return Err(issue("Expected object", &["send", "to"])),
            };
            Some(Send { prompt, subagent })
        }
        Some(_) => return Err(issue("Expected object", &["send"])),
    };
    Ok((map.get("state").cloned(), done, send))
}

/// `run`: runs the program once under the permissions authorized at creation, in `directory`. Plugin activation
/// counts toward the time cap.
pub async fn run(automations: &Automations, session: &SessionInfo, action: &Value, input: RunInput, directory: &str) -> Result<Outcome, AutomationError> {
    if !std::path::Path::new(directory).is_dir() {
        return Err(AutomationError::DirectoryNotFound(directory.to_string()));
    }
    match tokio::time::timeout(Duration::from_millis(TIMEOUT_MS), run_once(automations, session, action, input, directory)).await {
        Ok(outcome) => outcome,
        Err(_) => Ok(Outcome::failed(format!("Timed out after {TIMEOUT_MS}ms"))),
    }
}

async fn run_once(automations: &Automations, session: &SessionInfo, action: &Value, input: RunInput, directory: &str) -> Result<Outcome, AutomationError> {
    let env = automations.env(session, directory).await?;
    let Some(agent) = env.agent.clone() else { return Ok(Outcome::failed(format!("Agent not found: {}", env.agent_id))) };
    let Some(app) = automations.app() else { return Ok(Outcome::failed("Session runner is not available")) };
    let Some(engine) = automations.engine() else { return Ok(Outcome::failed("Session runner is not available")) };
    let code = action.get("code").and_then(Value::as_str).unwrap_or_default();
    let paths = match scan(code) {
        Ok(paths) => paths,
        Err(error) => return Ok(Outcome::invalid(error)),
    };
    let resolved = match resolve_tools(&env, &paths).await {
        Ok(actions) => actions,
        Err(error) => return Ok(Outcome::invalid(error)),
    };
    let mut actions = vec!["execute".to_string()];
    actions.extend(resolved);
    let rules = crate::permission::merge(&[&agent.permissions, session.permissions.as_deref().unwrap_or(&[])]);
    // Unattended rules come after the configured ones and the last match wins, so restate every scoped rule after
    // the grant: the approval stands in for the catch-all `*` only, and a scoped `ask` still needs a person.
    let unattended: Vec<PermissionRule> = actions.iter().flat_map(|item| std::iter::once(allow(item, "*")).chain(scoped_rules(&rules, item))).collect();

    // A deny added to config after creation still wins.
    let not_denied = crate::permission::unattended(unattended.clone(), async {
        for item in &actions {
            app.permissions.assert_not_denied(assert_input(session, &agent.id, None, item, vec!["*".into()], None)).await?;
        }
        Ok::<(), PermissionError>(())
    })
    .await;
    if let Err(error) = not_denied {
        return Ok(Outcome::failed(error.to_string()));
    }

    let mut bindings = Map::new();
    bindings.insert("state".into(), input.state.clone());
    bindings.insert("now".into(), json!(input.now));
    bindings.insert("runs".into(), json!(input.runs));
    bindings.insert("args".into(), action.get("args").cloned().unwrap_or(Value::Null));
    if let Some(event) = &input.event {
        bindings.insert("event".into(), event.clone());
    }
    let mut globals: Vec<(String, Value)> = bindings.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    globals.push(("input".into(), Value::Object(bindings)));
    let result = crate::permission::unattended(
        unattended,
        engine.run_check(&env, session, &agent.id, &input.message_id, code, &paths, globals, &rules, code_limits()),
    )
    .await;
    let output = |logs: Option<&Vec<String>>| logs.filter(|l| !l.is_empty()).map(|l| bounded_output(&l.join("\n")));
    match result {
        ExecResult::Failure { error, logs, .. } => {
            let invalid = matches!(
                error.kind,
                DiagnosticKind::ParseError | DiagnosticKind::UnsupportedSyntax | DiagnosticKind::UnknownTool | DiagnosticKind::InvalidToolInput
            ) || error.message.contains("needs approval");
            Ok(Outcome::Failed { error: error.message, output: output(logs.as_ref()), invalid })
        }
        ExecResult::Success { value, logs, .. } => {
            let output = output(logs.as_ref());
            match decode_result(&value) {
                Err(failure) => Ok(Outcome::Failed {
                    error: format!("An automation must return nothing or {{ state?, done?, send? }}: SchemaError({failure})"),
                    output,
                    invalid: true,
                }),
                Ok((state, done, send)) => Ok(Outcome::Ok { state, done, send, output }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(value: Value) -> String {
        decode_result(&value).unwrap_err()
    }

    /// Cases captured from 2.3.1's `Automation.decodeCodeResult` on Bun.
    #[test]
    fn decodes_code_results_like_effect() {
        assert_eq!(failure(json!("not-a-result")), "Expected Automation.CodeResult | null");
        assert_eq!(failure(json!([])), "Expected Automation.CodeResult | null");
        assert_eq!(failure(json!(true)), "Expected Automation.CodeResult | null");
        assert_eq!(failure(json!({"foo": 1})), "Expected no excess property\n  at [\"foo\"]");
        assert_eq!(failure(json!({"done": "yes"})), "Expected boolean\n  at [\"done\"]");
        assert_eq!(failure(json!({"send": {"prompt": " "}})), "Expected a string matching the RegExp \\S\n  at [\"send\"][\"prompt\"]");
        assert_eq!(failure(json!({"send": {"prompt": "x", "to": {"session": "ses_1"}}})), "Expected no excess property\n  at [\"send\"][\"to\"][\"session\"]");
        assert_eq!(failure(json!({"send": {"prompt": "x", "to": {"subagent": " "}}})), "Expected a string matching the RegExp \\S\n  at [\"send\"][\"to\"][\"subagent\"]");
        assert_eq!(failure(json!({"send": "x"})), "Expected object\n  at [\"send\"]");
        assert_eq!(failure(json!({"state": 1, "extra": true, "done": 3})), "Expected no excess property\n  at [\"extra\"]");
        assert_eq!(failure(json!({"send": {"prompt": 5}})), "Expected string\n  at [\"send\"][\"prompt\"]");
        assert_eq!(failure(json!({"send": {}})), "Missing key\n  at [\"send\"][\"prompt\"]");
        assert_eq!(failure(json!({"send": {"prompt": "x", "to": "y"}})), "Expected object\n  at [\"send\"][\"to\"]");
        assert_eq!(failure(json!({"send": 5, "done": 3})), "Expected boolean\n  at [\"done\"]");
        assert_eq!(failure(json!({"send": {"to": 1, "prompt": 5}})), "Expected string\n  at [\"send\"][\"prompt\"]");
        assert_eq!(failure(json!({"send": {"prompt": "x", "to": {}}})), "Missing key\n  at [\"send\"][\"to\"][\"subagent\"]");
        assert_eq!(failure(json!({"send": {"prompt": "\u{a0}"}})), "Expected a string matching the RegExp \\S\n  at [\"send\"][\"prompt\"]");
        assert_eq!(decode_result(&Value::Null), Ok((None, false, None)));
        assert_eq!(decode_result(&json!({"state": null})), Ok((Some(Value::Null), false, None)));
        assert_eq!(
            decode_result(&json!({"state": 1, "done": true, "send": {"prompt": "\n x", "to": {"subagent": "general"}}})),
            Ok((Some(json!(1)), true, Some(Send { prompt: "\n x".into(), subagent: Some("general".into()) })))
        );
    }

    #[test]
    fn bounds_output_in_utf8_bytes() {
        let text = "界".repeat(20_000);
        let bounded = bounded_output(&text);
        assert!(bounded.len() <= MAX_OUTPUT_BYTES);
        assert!(bounded.ends_with("界…"));
        assert_eq!(bounded_output("short"), "short");
        assert_eq!(handoff_output("Prompted ses_1", None), "Prompted ses_1");
        assert_eq!(handoff_output("Prompted ses_1", Some("log")), "Prompted ses_1\nlog");
    }
}
