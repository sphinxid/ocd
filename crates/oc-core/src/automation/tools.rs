//! The automation tools (`core/src/tool/plugin/automation.ts`, plugin `opencode.tool.automation`):
//! `automation_create`, `automation_list`, `automation_pause`, `automation_resume` and `automation_delete` in the
//! `opencode` namespace, through Code Mode. They act only on automations in the calling session's project, and each
//! call rechecks config where that session lives, since a request keeps the tools it started
//! with after a config reload.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use oc_schema::types::{SessionInfo, ToolContent};
use oc_tools::tool::{DefinitionContext, ErrorCause, ToolOptions};
use oc_tools::{Registry, Tool, ToolContext, ToolDefinition, ToolError, ToolResult};
use serde_json::{Map, Value, json};

use super::{AutomationError, Automations, CreateInput, Invocation, ListInput, time};

/// The tools' JSON Schemas as upstream's `definition` emits them (`scripts/automation-schemas.ts`).
static SCHEMAS: LazyLock<Vec<(String, Value, Value)>> = LazyLock::new(|| {
    let list: Vec<Value> = serde_json::from_str(include_str!("../../../../assets/automation/tools.json")).expect("automation tool schemas");
    list.into_iter()
        .map(|v| {
            (
                v["name"].as_str().unwrap_or_default().to_string(),
                v.get("inputSchema").cloned().unwrap_or(Value::Null),
                v.get("outputSchema").cloned().unwrap_or(Value::Null),
            )
        })
        .collect()
});

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Create,
    List,
    Pause,
    Resume,
    Delete,
}

const KINDS: [Kind; 5] = [Kind::Create, Kind::List, Kind::Pause, Kind::Resume, Kind::Delete];

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Create => "automation_create",
            Kind::List => "automation_list",
            Kind::Pause => "automation_pause",
            Kind::Resume => "automation_resume",
            Kind::Delete => "automation_delete",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Kind::Create => {
                "Run a prompt or Code Mode action on a timer or a connected MCP server's push event. A prompt can target this session, another session in this project or a new subagent child. Code runs without a model and can return one send: { send: { prompt } } steers this session; add to: { subagent } inside send for a permitted quiet child. Omit send to stay quiet."
            }
            Kind::List => "List this project's automations with their status and next run, and the current time.",
            Kind::Pause => "Pause an automation. It keeps its settings and does not run until resumed.",
            Kind::Resume => {
                "Resume a paused automation. A missed `at` runs once; repeating timers resume at their next slot. Event watches request replay from their saved cursor."
            }
            Kind::Delete => "Delete an automation for good.",
        }
    }
}

/// The five tools (`editor.add` in the plugin's tool transform).
pub fn tools(automations: &Arc<Automations>) -> Vec<Arc<dyn Tool>> {
    KINDS.into_iter().map(|kind| Arc::new(AutomationTool { kind, automations: automations.clone() }) as Arc<dyn Tool>).collect()
}

/// Registers the five tools at the end of `registry` (tests; the runner inserts them in plugin order).
pub fn register(registry: &mut Registry, automations: &Arc<Automations>) {
    for tool in tools(automations) {
        let _ = registry.add(tool);
    }
}

/// Whether a tool (un-namespaced) is one of the automation tools (plugin `opencode.tool.automation`).
pub fn is_automation_tool(name: &str) -> bool {
    KINDS.iter().any(|k| k.name() == name)
}

struct AutomationTool {
    kind: Kind,
    automations: Arc<Automations>,
}

/// `new ToolFailure({ message: error.message, error })`: the cause decides the session error kind (a permission
/// block or correction is `permission.rejected`, the automation errors `unknown`).
fn failure(error: AutomationError) -> ToolError {
    let message = error.message();
    match error {
        AutomationError::Declined => ToolError::declined(),
        AutomationError::Blocked(m) | AutomationError::Corrected(m) => ToolError::with_cause(message, ErrorCause::PermissionRejected(m)),
        _ => ToolError::with_cause(message.clone(), ErrorCause::Error(message)),
    }
}

/// `toView(info)`: the automation as the model reads it, with times in the automation's own zone.
pub fn view(info: &Value) -> Value {
    let tz = info["tz"].as_str().unwrap_or_default();
    let zone = time::zone_make_named(tz);
    let local = |key: &str| -> Option<Value> {
        let ms = info.get(key)?.as_i64()?;
        Some(json!(match &zone {
            Some(zone) => time::format_iso_offset(ms, zone),
            None => time::format_iso(ms),
        }))
    };
    let mut out = Map::new();
    for key in ["id", "name", "trigger", "tz", "action", "status"] {
        out.insert(key.into(), info[key].clone());
    }
    for key in ["nextDueAt", "expires"] {
        if let Some(v) = local(key) {
            out.insert(key.into(), v);
        }
    }
    out.insert("runCount".into(), info["runCount"].clone());
    if let Some(v) = local("lastRunAt") {
        out.insert("lastRunAt".into(), v);
    }
    for key in ["lastOutcome", "lastOutput", "state"] {
        if let Some(v) = info.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    Value::Object(out)
}

/// `clip(text)`: at most 80 UTF-16 units.
fn clip(text: &str) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() > 80 { format!("{}…", String::from_utf16_lossy(&units[..79])) } else { text.to_string() }
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| (c as u32) < 0x10000 && oc_codemode::number::is_js_whitespace(c as u16))
}

/// `summary(view)`: one line per automation, for models that read tool content rather than output (Code Mode off).
pub fn summary(view: &Value) -> String {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let trigger = &view["trigger"];
    let trigger = match trigger["type"].as_str().unwrap_or_default() {
        "at" => format!("at {}", s(trigger, "at")),
        "every" => format!("every {}", s(trigger, "every")),
        "cron" => format!("cron \"{}\"", s(trigger, "cron")),
        _ => format!("on {} {}", s(trigger, "server"), s(trigger, "event")),
    };
    let action = &view["action"];
    let action = if action["type"] == "prompt" {
        let to = &action["to"];
        let target = match to.get("session").and_then(Value::as_str) {
            Some(session) => session.to_string(),
            None => format!("subagent {}", s(to, "subagent")),
        };
        format!("prompt {target}: {}", Value::from(clip(&s(action, "prompt"))))
    } else {
        let code = s(action, "code");
        let line = code.split('\n').find(|l| !js_trim(l).is_empty()).map(js_trim).unwrap_or_default();
        format!("code: {}", Value::from(clip(line)))
    };
    let mut parts = vec![format!("{} \"{}\": {trigger} ({})", s(view, "id"), s(view, "name"), s(view, "tz")), s(view, "status")];
    if let Some(next) = view.get("nextDueAt").and_then(Value::as_str).filter(|n| !n.is_empty()) {
        parts.push(format!("next {next}"));
    }
    if let Some(expires) = view.get("expires").and_then(Value::as_str).filter(|n| !n.is_empty()) {
        parts.push(format!("expires {expires}"));
    }
    parts.push(action);
    if let Some(output) = view.get("lastOutput").and_then(Value::as_str).filter(|o| !o.is_empty()) {
        parts.push(format!("output: {}", Value::from(clip(output))));
    }
    parts.retain(|p| !p.is_empty());
    parts.join(", ")
}

fn text(content: String) -> Vec<ToolContent> {
    vec![ToolContent::Text { text: content }]
}

/// `changed(verb)`: what a tool returns for the automation it created, paused or resumed.
fn changed(verb: &str, info: Value) -> ToolResult {
    let view = view(&info);
    let content = format!("{verb}: {}", summary(&view));
    ToolResult { output: Some(view), content: text(content), metadata: None }
}

impl AutomationTool {
    /// `caller(sessionID)`: the calling session, while automations are enabled where it lives.
    async fn caller(&self, session_id: &str) -> Result<SessionInfo, AutomationError> {
        let session = self.automations.session(session_id)?;
        let on = self.automations.enabled(&session.location.directory).await.unwrap_or(false);
        if !on {
            return Err(AutomationError::Disabled);
        }
        Ok(session)
    }

    /// `owned(id, sessionID)`: tools act only on automations in the calling session's project.
    async fn owned(&self, id: &str, session_id: &str) -> Result<(), AutomationError> {
        let session = self.caller(session_id).await?;
        let info = self.automations.get(id)?;
        if info.get("projectID").and_then(Value::as_str) != Some(session.project_id.as_str()) {
            return Err(AutomationError::NotFound(id.to_string()));
        }
        Ok(())
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult, AutomationError> {
        let session = ctx.session_id.as_str();
        let id = input.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        match self.kind {
            Kind::Create => {
                let mut request = CreateInput::from_value(&input);
                request.session_id = Some(session.to_string());
                request.directory = None;
                let info = self.automations.create(request, Some(&Invocation::of(ctx))).await?;
                Ok(changed("Created", info))
            }
            Kind::List => {
                let caller = self.caller(session).await?;
                let now = time::format_iso(self.automations.clock().now());
                let list: Vec<Value> =
                    self.automations.list(&ListInput { project_id: Some(caller.project_id), ..Default::default() }).iter().map(view).collect();
                let mut lines = vec![format!("Now: {now}")];
                if list.is_empty() {
                    lines.push("No automations.".into());
                } else {
                    lines.extend(list.iter().map(summary));
                }
                Ok(ToolResult { output: Some(json!({ "now": now, "automations": list })), content: text(lines.join("\n")), metadata: None })
            }
            Kind::Pause => {
                self.owned(&id, session).await?;
                Ok(changed("Paused", self.automations.pause(&id)?))
            }
            Kind::Resume => {
                self.owned(&id, session).await?;
                Ok(changed("Resumed", self.automations.resume(&id)?))
            }
            Kind::Delete => {
                self.owned(&id, session).await?;
                self.automations.remove(&id)?;
                Ok(ToolResult {
                    output: Some(json!({ "id": id, "removed": true })),
                    content: text(format!("Deleted automation {id}.")),
                    metadata: None,
                })
            }
        }
    }
}

#[async_trait]
impl Tool for AutomationTool {
    fn name(&self) -> &str {
        self.kind.name()
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::codemode("opencode")
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        let (input, output) = SCHEMAS
            .iter()
            .find(|(n, _, _)| n == self.kind.name())
            .map(|(_, i, o)| (i.clone(), o.clone()))
            .unwrap_or((json!({"type": "object"}), Value::Null));
        ToolDefinition::new(self.kind.name(), self.kind.description(), input).with_output(Some(output))
    }

    /// The JSON schema check, with `trigger` and `action` decoded as Effect's tagged unions: by their `type`, only
    /// that member's issues, or `Expected Automation.Trigger` / `Expected Automation.ActionInput` without one.
    fn validate(&self, input: &Value) -> Vec<oc_tools::validate::Issue> {
        use oc_tools::validate::{Flavor, Issue, PathSegment, validate};
        let schema = self.definition(&DefinitionContext::default()).input_schema;
        let mut issues = validate(&schema, input, Flavor::Effect);
        if self.kind != Kind::Create {
            return issues;
        }
        for (field, identifier) in [("trigger", "Automation.Trigger"), ("action", "Automation.ActionInput")] {
            let under = |issue: &Issue| matches!(issue.path.first(), Some(PathSegment::Key(k)) if k == field) && issue.path.len() > 1
                || (issue.path.len() == 1 && matches!(issue.path.first(), Some(PathSegment::Key(k)) if k == field) && issue.message != "Missing key");
            let Some(at) = issues.iter().position(under) else { continue };
            issues.retain(|issue| !under(issue));
            let members = schema["properties"][field]["anyOf"].as_array().cloned().unwrap_or_default();
            let tag = input.get(field).and_then(|v| v.get("type")).and_then(Value::as_str);
            let member = members.iter().find(|m| tag.is_some() && m["properties"]["type"]["enum"][0].as_str() == tag);
            let replacement = match member {
                Some(member) => validate(member, &input[field], Flavor::Effect)
                    .into_iter()
                    .map(|issue| Issue::new(std::iter::once(PathSegment::Key(field.into())).chain(issue.path).collect(), issue.message))
                    .collect(),
                None => vec![Issue::new(vec![PathSegment::Key(field.into())], format!("Expected {identifier}"))],
            };
            let at = at.min(issues.len());
            issues.splice(at..at, replacement);
        }
        issues
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        self.run(input, &ctx).await.map_err(failure)
    }
}
