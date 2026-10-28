//! Permission dock state machine and presentation (`src/mini/permission.shared.ts`, `util/permission.ts`).

use super::mono::mono_prefix;
use super::tool::tool_path;
use super::types::{MiniPermission, ToolStatus};
use super::util::{canonical_tool_name, dict, finite_number, text, web_search_provider_label};
use crate::util::locale;
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Permission,
    Always,
    Reject,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermOption {
    Once,
    Always,
    Reject,
    Confirm,
    Cancel,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PermissionBodyState {
    pub request_id: String,
    pub session_id: String,
    pub stage: Stage,
    pub selected: PermOption,
    pub message: String,
    pub submitting: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PermissionReply {
    pub session_id: String,
    pub request_id: String,
    pub decision: &'static str,
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PermissionStep {
    pub state: PermissionBodyState,
    pub reply: Option<PermissionReply>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct PermissionPresentation {
    pub icon: String,
    pub title: String,
    pub lines: Vec<String>,
    pub diff: Option<String>,
    pub patch: Option<String>,
    pub file: Option<String>,
}

pub fn create_permission_body_state(id: &str, session_id: &str) -> PermissionBodyState {
    PermissionBodyState { request_id: id.into(), session_id: session_id.into(), stage: Stage::Permission, selected: PermOption::Once, message: String::new(), submitting: false }
}

pub fn permission_options(stage: Stage) -> Vec<PermOption> {
    match stage {
        Stage::Permission => vec![PermOption::Once, PermOption::Always, PermOption::Reject],
        Stage::Always => vec![PermOption::Confirm, PermOption::Cancel],
        Stage::Reject => vec![],
    }
}

/// `permissionOptionLabel`.
pub fn permission_label(option: PermOption) -> &'static str {
    match option {
        PermOption::Once => "Allow once",
        PermOption::Always => "Always allow",
        PermOption::Reject => "Reject",
        PermOption::Confirm => "Confirm",
        PermOption::Cancel => "Cancel",
    }
}

fn normalize_input(action: &str, value: Option<&Value>) -> Map<String, Value> {
    let mut input = dict(value);
    let path = [text(input.get("path")), text(input.get("filePath")), text(input.get("filepath"))].into_iter().find(|s| !s.is_empty()).unwrap_or_default();
    let agent = [text(input.get("agent")), text(input.get("subagent_type"))].into_iter().find(|s| !s.is_empty()).unwrap_or_default();
    if ["read", "edit", "list", "lsp"].contains(&action) && !path.is_empty() {
        input.insert("path".into(), Value::String(path));
    }
    if action == "subagent" && !agent.is_empty() {
        input.insert("agent".into(), Value::String(agent));
    }
    input
}

fn wildcard_directory(value: &str) -> String {
    let Some(w) = value.find('*') else { return value.to_string() };
    let prefix = &value[..w];
    let only_seps = !prefix.is_empty() && prefix.chars().all(|c| c == '/' || c == '\\');
    let drive = regex::Regex::new(r"^[A-Za-z]:[\\/]$").unwrap().is_match(prefix);
    if only_seps || drive {
        return prefix.to_string();
    }
    prefix.trim_end_matches(['/', '\\']).to_string()
}

/// `permissionPresentation(source, formatPath)`.
pub fn permission_presentation(action_raw: &str, resources: &[String], metadata: Option<&Value>, input: Option<&Value>, tool_metadata: Option<&Value>, fmt: &dyn Fn(&str) -> String) -> PermissionPresentation {
    let action = canonical_tool_name(action_raw);
    let input = normalize_input(action, input);
    let mut meta = dict(tool_metadata);
    for (k, v) in dict(metadata) {
        meta.insert(k, v);
    }
    let first = resources.first().cloned().unwrap_or_default();
    let p = |icon: &str, title: String, lines: Vec<String>| PermissionPresentation { icon: icon.into(), title, lines, ..Default::default() };
    let or = |a: String, b: String| if a.is_empty() { b } else { a };
    match action {
        "edit" => {
            let file = or(text(input.get("path")), first);
            let first_file = dict(meta.get("files").and_then(|f| f.as_array()).and_then(|a| a.first()));
            let diff = [text(first_file.get("patch")), text(first_file.get("diff")), text(meta.get("diff"))].into_iter().find(|s| !s.is_empty());
            let patch = if diff.is_some() { None } else { Some(text(input.get("patchText"))).filter(|s| !s.is_empty()) };
            PermissionPresentation { icon: "→".into(), title: format!("Edit {}", fmt(&file)), lines: vec![], diff, patch, file: Some(file) }
        }
        "read" | "list" => {
            let value = or(text(input.get("path")), first);
            let title = if action == "read" { "Read" } else { "List" };
            let lines = if value.is_empty() { vec![] } else { vec![format!("Path: {}", fmt(&value))] };
            p("→", format!("{title} {}", fmt(&value)), lines)
        }
        "glob" | "grep" => {
            let pattern = or(text(input.get("pattern")), first);
            let title = if action == "glob" { "Glob" } else { "Grep" };
            let lines = if pattern.is_empty() { vec![] } else { vec![format!("Pattern: {pattern}")] };
            p("✱", format!("{title} \"{pattern}\""), lines)
        }
        "shell" => {
            let command = text(input.get("command"));
            let lines = if command.is_empty() { resources.iter().map(|r| format!("- {r}")).collect() } else { vec![format!("$ {command}")] };
            p("#", "Shell command".into(), lines)
        }
        "subagent" => {
            let agent = or(text(input.get("agent")), "general".into());
            let description = text(input.get("description"));
            let lines = if description.is_empty() { vec![] } else { vec![format!("◉ {description}")] };
            p("#", format!("{} Subagent", locale::titlecase(&agent)), lines)
        }
        "webfetch" => {
            let url = or(text(input.get("url")), text(meta.get("url")));
            let lines = if url.is_empty() { vec![] } else { vec![format!("URL: {url}")] };
            p("%", format!("WebFetch {url}"), lines)
        }
        "websearch" => {
            let query = or(text(input.get("query")), text(meta.get("query")));
            let title = web_search_provider_label(meta.get("provider"));
            let lines = if query.is_empty() { vec![] } else { vec![format!("Query: {query}")] };
            p("◈", if query.is_empty() { title } else { format!("{title} \"{query}\"") }, lines)
        }
        "lsp" => {
            let file = text(input.get("path"));
            let operation = or(text(input.get("operation")), "request".into());
            let line = finite_number(input.get("line"));
            let character = finite_number(input.get("character"));
            let position = match (line, character) {
                (Some(l), Some(c)) => Some(format!("{}:{}", locale::js_number(l), locale::js_number(c))),
                _ => None,
            };
            let mut lines = Vec::new();
            if input.get("operation").is_some_and(|o| !o.is_null() && o.as_str() != Some("")) {
                lines.push(format!("Operation: {operation}"));
            }
            if !file.is_empty() {
                lines.push(format!("Path: {}", fmt(&file)));
            }
            if let Some(pos) = &position {
                lines.push(format!("Position: {pos}"));
            }
            let title = format!(
                "LSP {operation}{}",
                if file.is_empty() { String::new() } else { format!(" {}{}", fmt(&file), position.as_ref().map(|p| format!(":{p}")).unwrap_or_default()) }
            );
            p("→", title, lines)
        }
        "external_directory" => {
            let raw = [text(meta.get("parentDir")), text(meta.get("filepath")), first].into_iter().find(|s| !s.is_empty()).unwrap_or_default();
            let directory = wildcard_directory(&raw);
            p("←", format!("Access external directory {}", fmt(&directory)), resources.iter().map(|r| format!("- {r}")).collect())
        }
        "doom_loop" => p("⟳", "Continue after repeated failures".into(), vec!["This keeps the session running despite repeated failures.".into()]),
        _ => p("⚙", format!("Call tool {action_raw}"), vec![format!("Tool: {action_raw}")]),
    }
}

/// `permissionInfo(request, directory, mono)`.
pub fn permission_info(request: &MiniPermission, directory: Option<&str>, mono: bool) -> PermissionPresentation {
    let tool = request.tool.as_ref();
    let streaming = tool.is_none_or(|t| t.status == ToolStatus::Streaming);
    let input = if streaming { None } else { tool.map(|t| &t.input) };
    let tool_meta = if streaming { None } else { tool.map(|t| &t.metadata) };
    let fmt = |v: &str| tool_path(Some(v), true, directory);
    let info = permission_presentation(&request.request.action, &request.request.resources, request.request.metadata.as_ref(), input, tool_meta, &fmt);
    if !mono {
        return info;
    }
    let mut lines: Vec<String> = Vec::new();
    if let Some(d) = info.diff.clone().or(info.patch.clone()) {
        lines.push(d);
    }
    lines.extend(info.lines.iter().map(|l| mono_prefix(l, true)));
    PermissionPresentation { icon: mono_prefix(&info.icon, true), lines, diff: None, patch: None, ..info }
}

/// `permissionAlwaysLines(request)`.
pub fn permission_always_lines(action: &str, save: &[String]) -> Vec<String> {
    if save.len() == 1 && save[0] == "*" {
        return vec![format!("This will always allow {action} for this project.")];
    }
    let mut out = vec!["This will always allow the following patterns for this project.".to_string()];
    out.extend(save.iter().map(|s| format!("- {s}")));
    out
}

fn reply(session_id: &str, request_id: &str, decision: &'static str, message: Option<&str>) -> PermissionReply {
    PermissionReply { session_id: session_id.into(), request_id: request_id.into(), decision, message: message.map(|m| m.trim().to_string()).filter(|m| !m.is_empty()) }
}

/// `permissionShift(state, dir, list)`.
pub fn permission_shift(state: &PermissionBodyState, dir: i32, list: &[PermOption]) -> PermissionBodyState {
    if list.is_empty() {
        return state.clone();
    }
    let idx = list.iter().position(|o| *o == state.selected).unwrap_or(0) as i32;
    let len = list.len() as i32;
    PermissionBodyState { selected: list[((idx + dir + len) % len) as usize], ..state.clone() }
}

pub fn permission_hover(state: &PermissionBodyState, option: PermOption) -> PermissionBodyState {
    PermissionBodyState { selected: option, ..state.clone() }
}

/// `permissionRun(state, requestID, option)`.
pub fn permission_run(state: &PermissionBodyState, request_id: &str, option: PermOption) -> PermissionStep {
    let same = || PermissionStep { state: state.clone(), reply: None };
    if state.submitting {
        return same();
    }
    if state.stage == Stage::Permission {
        return match option {
            PermOption::Always => PermissionStep { state: PermissionBodyState { stage: Stage::Always, selected: PermOption::Confirm, ..state.clone() }, reply: None },
            PermOption::Reject => PermissionStep { state: PermissionBodyState { stage: Stage::Reject, selected: PermOption::Reject, ..state.clone() }, reply: None },
            _ => PermissionStep { state: state.clone(), reply: Some(reply(&state.session_id, request_id, "once", None)) },
        };
    }
    if state.stage != Stage::Always {
        return same();
    }
    if option == PermOption::Cancel {
        return PermissionStep { state: PermissionBodyState { stage: Stage::Permission, selected: PermOption::Always, ..state.clone() }, reply: None };
    }
    PermissionStep { state: state.clone(), reply: Some(reply(&state.session_id, request_id, "always", None)) }
}

/// `permissionReject(state, requestID)`.
pub fn permission_reject(state: &PermissionBodyState, request_id: &str) -> Option<PermissionReply> {
    if state.submitting {
        return None;
    }
    Some(reply(&state.session_id, request_id, "reject", Some(&state.message)))
}

pub fn permission_cancel(state: &PermissionBodyState) -> PermissionBodyState {
    PermissionBodyState { stage: Stage::Permission, selected: PermOption::Reject, ..state.clone() }
}

pub fn permission_escape(state: &PermissionBodyState) -> PermissionBodyState {
    if state.stage == Stage::Always {
        return PermissionBodyState { stage: Stage::Permission, selected: PermOption::Always, ..state.clone() };
    }
    PermissionBodyState { stage: Stage::Reject, selected: PermOption::Reject, ..state.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::PermissionRequest;
    use crate::mini::types::ToolPart;
    use serde_json::json;

    fn req(action: &str, resources: &[&str], metadata: Value, tool: Option<ToolPart>) -> MiniPermission {
        MiniPermission {
            request: PermissionRequest {
                id: "perm-1".into(),
                session_id: "session-1".into(),
                action: action.into(),
                resources: resources.iter().map(|s| s.to_string()).collect(),
                metadata: Some(metadata),
                save: Some(vec![]),
                ..Default::default()
            },
            tool,
        }
    }

    fn body() -> PermissionBodyState {
        create_permission_body_state("perm-1", "session-1")
    }

    fn running(name: &str, input: Value, metadata: Value, id: &str) -> ToolPart {
        ToolPart::from_value(&json!({"type": "tool", "id": id, "name": name, "state": {"status": "running", "input": input, "metadata": metadata}, "time": {"created": 1, "ran": 1}}))
    }

    #[test]
    fn allow_once_replies() {
        let out = permission_run(&body(), "perm-1", PermOption::Once);
        assert_eq!(out.reply, Some(PermissionReply { session_id: "session-1".into(), request_id: "perm-1".into(), decision: "once", message: None }));
    }

    #[test]
    fn allow_always_confirms() {
        let next = permission_run(&body(), "perm-1", PermOption::Always);
        assert_eq!((next.state.stage, next.state.selected), (Stage::Always, PermOption::Confirm));
        assert!(next.reply.is_none());
        assert_eq!(permission_run(&next.state, "perm-1", PermOption::Confirm).reply.map(|r| r.decision), Some("always"));
        let back = permission_run(&next.state, "perm-1", PermOption::Cancel).state;
        assert_eq!((back.stage, back.selected), (Stage::Permission, PermOption::Always));
    }

    #[test]
    fn reject_flow() {
        let next = permission_run(&body(), "perm-1", PermOption::Reject);
        assert_eq!(next.state.stage, Stage::Reject);
        let out = permission_reject(&PermissionBodyState { message: "  use rg  ".into(), ..next.state.clone() }, "perm-1");
        assert_eq!(out, Some(PermissionReply { session_id: "session-1".into(), request_id: "perm-1".into(), decision: "reject", message: Some("use rg".into()) }));
        let c = permission_cancel(&next.state);
        assert_eq!((c.stage, c.selected), (Stage::Permission, PermOption::Reject));
        let e = permission_escape(&body());
        assert_eq!((e.stage, e.selected), (Stage::Reject, PermOption::Reject));
        let e = permission_escape(&PermissionBodyState { stage: Stage::Always, selected: PermOption::Confirm, ..next.state });
        assert_eq!((e.stage, e.selected), (Stage::Permission, PermOption::Always));
    }

    #[test]
    fn display_info() {
        let shell = req("shell", &[], json!({}), Some(running("shell", json!({"command": "git status --short"}), json!({}), "call-shell")));
        let info = permission_info(&shell, None, false);
        assert_eq!((info.title.as_str(), info.lines.clone()), ("Shell command", vec!["$ git status --short".to_string()]));
        let ext = permission_info(&req("external_directory", &["/tmp/work/**/*.ts", "/tmp/work/**/*.tsx"], json!({}), None), None, false);
        assert_eq!(ext.title, "Access external directory /tmp/work");
        assert_eq!(ext.lines, vec!["- /tmp/work/**/*.ts", "- /tmp/work/**/*.tsx"]);
        assert_eq!(permission_info(&req("doom_loop", &[], json!({}), None), None, false).title, "Continue after repeated failures");
        let custom = permission_info(&req("custom_tool", &[], json!({}), None), None, false);
        assert_eq!((custom.title.as_str(), custom.lines.clone()), ("Call tool custom_tool", vec!["Tool: custom_tool".to_string()]));
    }

    #[test]
    fn request_metadata_wins() {
        let r = req("websearch", &[], json!({"provider": "parallel"}), Some(running("websearch", json!({"query": "current releases"}), json!({"provider": "exa", "retained": true}), "call-search")));
        let info = permission_info(&r, None, false);
        assert_eq!(info.title, "Web Search via Parallel \"current releases\"");
        assert_eq!(info.lines, vec!["Query: current releases"]);
    }

    #[test]
    fn edit_patch_text() {
        let patch = "*** Begin Patch\n*** Update File: src/index.ts\n@@\n-old\n+const arrow = \"→\"\n*** End Patch";
        let r = req("edit", &["src/index.ts"], json!({}), Some(running("edit", json!({"patchText": patch}), json!({}), "call-edit")));
        let info = permission_info(&r, Some(&super::super::util::cwd()), false);
        assert_eq!((info.title.as_str(), info.diff.clone(), info.patch.as_deref()), ("Edit src/index.ts", None, Some(patch)));
        let info = permission_info(&r, Some(&super::super::util::cwd()), true);
        assert_eq!((info.title.as_str(), info.lines.clone(), info.diff, info.patch), ("Edit src/index.ts", vec![patch.to_string()], None, None));
    }

    #[test]
    fn always_copy() {
        assert_eq!(permission_always_lines("bash", &["*".into()]), vec!["This will always allow bash for this project."]);
        assert_eq!(
            permission_always_lines("read", &["src/**/*.ts".into(), "src/**/*.tsx".into()]),
            vec!["This will always allow the following patterns for this project.", "- src/**/*.ts", "- src/**/*.tsx"]
        );
    }
}
