//! The schedule tools (`core/src/tool/plugin/schedule.ts`, plugin `opencode.tool.schedule`): `schedule_create`,
//! `schedule_list`, `schedule_pause`, `schedule_resume` and `schedule_delete` in the `opencode` namespace, through
//! Code Mode. They act only on the calling session's schedules, and each call rechecks `experimental.schedules`,
//! since a request keeps the tools it started with after a config reload.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use oc_schema::types::ToolContent;
use oc_tools::tool::{DefinitionContext, ToolOptions};
use oc_tools::{Registry, Tool, ToolContext, ToolDefinition, ToolError, ToolResult};
use serde_json::{Map, Value, json};

use super::{CreateInput, ScheduleError, Schedules, time};

/// Input and output JSON schemas as upstream's `definition()` emits them (captured from 2.2.5).
static SCHEMAS: LazyLock<Vec<(String, Value, Value)>> = LazyLock::new(|| {
    include_str!("schemas.jsonl")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|v| (v["name"].as_str().unwrap_or_default().to_string(), v["inputSchema"].clone(), v["outputSchema"].clone()))
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
            Kind::Create => "schedule_create",
            Kind::List => "schedule_list",
            Kind::Pause => "schedule_pause",
            Kind::Resume => "schedule_resume",
            Kind::Delete => "schedule_delete",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Kind::Create => {
                "Schedule a prompt that wakes this session later: once after a delay, at a time, every interval, on a cron, or gated by an `if` / `until` shell (`{ run }`) or Code Mode (`{ code }`) check. Returns the schedule with its next run (or `done` immediately if `until` already matches)."
            }
            Kind::List => "List this session's schedules with their status and next run, and the current time.",
            Kind::Pause => "Pause a schedule. It keeps its settings and does not fire until resumed.",
            Kind::Resume => {
                "Resume a paused schedule. A one-shot that fell due while paused fires once now; a repeating one resumes at its next slot."
            }
            Kind::Delete => "Delete a schedule for good.",
        }
    }
}

/// Registers the five tools (`editor.add` in the plugin's tool transform).
pub fn register(registry: &mut Registry, schedules: &Arc<Schedules>) {
    for kind in KINDS {
        let _ = registry.add(Arc::new(ScheduleTool { kind, schedules: schedules.clone() }));
    }
}

/// Whether a tool is one of the schedule tools (plugin `opencode.tool.schedule`).
pub fn is_schedule_tool(name: &str) -> bool {
    KINDS.iter().any(|k| k.name() == name)
}

struct ScheduleTool {
    kind: Kind,
    schedules: Arc<Schedules>,
}

/// `new ToolFailure({ message: error.message, error })`: the cause decides the session error kind (a permission
/// block or correction is `permission.rejected`, the schedule errors `unknown`).
fn failure(error: ScheduleError) -> ToolError {
    use oc_tools::tool::ErrorCause;
    let message = error.message();
    match error {
        ScheduleError::Declined => ToolError::declined(),
        ScheduleError::Blocked(m) | ScheduleError::Corrected(m) => ToolError::with_cause(message, ErrorCause::PermissionRejected(m)),
        _ => ToolError::with_cause(message.clone(), ErrorCause::Error(message)),
    }
}

/// `toView(info)`: the schedule as the model reads it, with times in the schedule's own zone.
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
    out.insert("id".into(), info["id"].clone());
    if let Some(n) = info.get("name") {
        out.insert("name".into(), n.clone());
    }
    out.insert("when".into(), info["when"].clone());
    out.insert("tz".into(), info["tz"].clone());
    for key in ["if", "until"] {
        if let Some(c) = info.get(key) {
            out.insert(key.into(), c.clone());
        }
    }
    out.insert("prompt".into(), info["action"]["prompt"].clone());
    out.insert("status".into(), info["status"].clone());
    for key in ["nextDueAt", "endsAt"] {
        if let Some(v) = local(key) {
            out.insert(key.into(), v);
        }
    }
    if let Some(r) = info.get("remainingRuns") {
        out.insert("remainingRuns".into(), r.clone());
    }
    out.insert("runCount".into(), info["runCount"].clone());
    if let Some(v) = local("lastFiredAt") {
        out.insert("lastFiredAt".into(), v);
    }
    for key in ["lastOutcome", "lastOutput"] {
        if let Some(v) = info.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    Value::Object(out)
}

/// `summary(view)`: one line per schedule, for models that read tool content rather than output.
pub fn summary(view: &Value) -> String {
    let when = &view["when"];
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let trigger = match when["type"].as_str().unwrap_or_default() {
        "in" => format!("in {}", s(when, "in")),
        "at" => format!("at {}", s(when, "at")),
        "every" => format!("every {}", s(when, "every")),
        _ => format!("cron \"{}\"", s(when, "cron")),
    };
    let gate = match (view.get("until"), view.get("if")) {
        (Some(c), _) => Some(format!("until {}", super::check::format_condition(c))),
        (None, Some(c)) => Some(format!("if {}", super::check::format_condition(c))),
        _ => None,
    };
    let prompt = s(view, "prompt");
    let units: Vec<u16> = prompt.encode_utf16().collect();
    let prompt = if units.len() > 80 { format!("{}…", String::from_utf16_lossy(&units[..79])) } else { prompt };
    let name = view.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).map(|n| format!(" \"{n}\"")).unwrap_or_default();
    let mut parts = vec![format!("{}{name}: {trigger} ({})", s(view, "id"), s(view, "tz"))];
    parts.extend(gate);
    parts.push(s(view, "status"));
    if let Some(next) = view.get("nextDueAt").and_then(Value::as_str).filter(|n| !n.is_empty()) {
        parts.push(format!("next {next}"));
    }
    parts.push(format!("prompt: {}", Value::from(prompt)));
    if let Some(output) = view.get("lastOutput").and_then(Value::as_str).filter(|o| !o.is_empty()) {
        parts.push(format!("output: {}", Value::from(output)));
    }
    parts.join(", ")
}

fn text(content: String) -> Vec<ToolContent> {
    vec![ToolContent::Text { text: content }]
}

impl ScheduleTool {
    /// `allowed(sessionID)`
    async fn allowed(&self, session_id: &str) -> Result<(), ScheduleError> {
        if self.schedules.enabled(session_id).await? { Ok(()) } else { Err(ScheduleError::Disabled) }
    }

    /// `owned(id, sessionID)`: tools act only on the calling session's schedules.
    async fn owned(&self, id: &str, session_id: &str) -> Result<(), ScheduleError> {
        self.allowed(session_id).await?;
        let info = self.schedules.get(id)?;
        if info.get("sessionID").and_then(Value::as_str) != Some(session_id) {
            return Err(ScheduleError::NotFound(id.to_string()));
        }
        Ok(())
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult, ScheduleError> {
        let session = ctx.session_id.as_str();
        let id = input.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let one = |info: Value, prefix: &str| {
            let view = view(&info);
            let content = format!("{prefix}: {}", summary(&view));
            ToolResult { output: Some(view), content: text(content), metadata: None }
        };
        match self.kind {
            Kind::Create => {
                let info = self.schedules.create(CreateInput::from_request(session, &input)).await?;
                let v = view(&info);
                let prefix = if v["status"] == "done" && v.get("until").is_some() { "Matched immediately" } else { "Scheduled" };
                Ok(ToolResult { content: text(format!("{prefix}: {}", summary(&v))), output: Some(v), metadata: None })
            }
            Kind::List => {
                self.allowed(session).await?;
                let now = time::format_iso(oc_schema::id::now_ms() as i64);
                let list: Vec<Value> = self.schedules.list(Some(session)).iter().map(view).collect();
                let mut lines = vec![format!("Now: {now}")];
                if list.is_empty() {
                    lines.push("No schedules.".into());
                } else {
                    lines.extend(list.iter().map(summary));
                }
                Ok(ToolResult { output: Some(json!({ "now": now, "schedules": list })), content: text(lines.join("\n")), metadata: None })
            }
            Kind::Pause => {
                self.owned(&id, session).await?;
                Ok(one(self.schedules.pause(&id)?, "Paused"))
            }
            Kind::Resume => {
                self.owned(&id, session).await?;
                Ok(one(self.schedules.resume(&id)?, "Resumed"))
            }
            Kind::Delete => {
                self.owned(&id, session).await?;
                self.schedules.remove(&id)?;
                Ok(ToolResult {
                    output: Some(json!({ "id": id, "removed": true })),
                    content: text(format!("Deleted schedule {id}.")),
                    metadata: None,
                })
            }
        }
    }
}

#[async_trait]
impl Tool for ScheduleTool {
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

    /// The JSON schema check, with the messages naming the schemas' identifiers like Effect's
    /// (`Schedule.WhenInput`, `Schedule.ConditionInput`, `Schedule.Ends`, `Schedule.Duration`).
    fn validate(&self, input: &Value) -> Vec<oc_tools::validate::Issue> {
        use oc_tools::validate::{Flavor, PathSegment, validate};
        let definition = self.definition(&DefinitionContext::default());
        let mut issues = validate(&definition.input_schema, input, Flavor::Effect);
        for issue in &mut issues {
            let keys: Vec<&str> = issue
                .path
                .iter()
                .map(|segment| match segment {
                    PathSegment::Key(k) => k.as_str(),
                    PathSegment::Index(_) => "",
                })
                .collect();
            let renamed = match (keys.as_slice(), issue.message.as_str()) {
                (["when"], m) if m.starts_with("Expected ") => "Expected Schedule.WhenInput",
                (["if"] | ["until"], m) if m.starts_with("Expected ") => "Expected Schedule.ConditionInput",
                (["ends"], m) if m.starts_with("Expected ") => "Expected Schedule.Ends",
                (["when", "in" | "every"] | ["ends", "after"], "Expected string") => "Expected Schedule.Duration",
                _ => continue,
            };
            issue.message = renamed.to_string();
        }
        issues
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        self.run(input, &ctx).await.map_err(failure)
    }
}
