//! Shared vocabulary of the mini frontend (`src/mini/types.ts`).
//!
//! Data flow: V2 events → [`StreamCommit`]s + footer events → footer (scrollback queue and footer view) →
//! split-footer renderer.

use serde_json::{Map, Value};

/// The semantic role of a scrollback entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum EntryKind {
    #[default]
    System,
    User,
    Assistant,
    Reasoning,
    Tool,
    Error,
}

/// Lifecycle phase of a scrollback entry: `start` opens it, `progress` appends, `final` closes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Start,
    Progress,
    Final,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Source {
    Assistant,
    Reasoning,
    Tool,
    #[default]
    System,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStateKind {
    Running,
    Completed,
    Error,
}

impl ToolStateKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolStateKind::Running => "running",
            ToolStateKind::Completed => "completed",
            ToolStateKind::Error => "error",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct TurnSummary {
    pub agent: String,
    pub model: String,
    pub duration: String,
}

/// Status of a `SessionMessageAssistantTool`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ToolStatus {
    #[default]
    Streaming,
    Running,
    Completed,
    Error,
}

impl ToolStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolStatus::Streaming => "streaming",
            ToolStatus::Running => "running",
            ToolStatus::Completed => "completed",
            ToolStatus::Error => "error",
        }
    }
}

/// `SessionMessageAssistantTool` (an assistant message `tool` content item).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ToolPart {
    pub id: String,
    pub name: String,
    pub status: ToolStatus,
    /// Streaming: the raw input text (a JSON string); otherwise the input object.
    pub input: Value,
    pub metadata: Value,
    pub content: Vec<Value>,
    /// `state.error` (`{type, message}`) for failed tools.
    pub error: Option<Value>,
    pub created: i64,
    pub ran: Option<i64>,
    pub completed: Option<i64>,
    pub executed: Option<Value>,
    pub native: Option<Value>,
}

impl ToolPart {
    /// Parse an API tool content item.
    pub fn from_value(v: &Value) -> ToolPart {
        let state = v.get("state").cloned().unwrap_or(Value::Null);
        let status = match state.get("status").and_then(|s| s.as_str()) {
            Some("running") => ToolStatus::Running,
            Some("completed") => ToolStatus::Completed,
            Some("error") => ToolStatus::Error,
            _ => ToolStatus::Streaming,
        };
        let time = v.get("time");
        let num = |k: &str| time.and_then(|t| t.get(k)).and_then(|n| n.as_f64()).map(|n| n as i64);
        ToolPart {
            id: v.get("id").and_then(|s| s.as_str()).unwrap_or_default().to_string(),
            name: v.get("name").and_then(|s| s.as_str()).unwrap_or_default().to_string(),
            status,
            input: match status {
                ToolStatus::Streaming => Value::String(state.get("input").and_then(|s| s.as_str()).unwrap_or_default().to_string()),
                _ => state.get("input").cloned().unwrap_or(Value::Object(Map::new())),
            },
            metadata: match status {
                ToolStatus::Streaming => Value::Null,
                _ => state.get("metadata").cloned().unwrap_or(Value::Object(Map::new())),
            },
            content: state.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default(),
            error: state.get("error").cloned().filter(|e| !e.is_null()),
            created: num("created").unwrap_or(0),
            ran: num("ran"),
            completed: num("completed"),
            executed: v.get("executed").cloned(),
            native: v.get("native").cloned(),
        }
    }

    pub fn error_message(&self) -> String {
        self.error.as_ref().and_then(|e| e.get("message")).and_then(|m| m.as_str()).unwrap_or_default().to_string()
    }

    /// `dict(tool.state)`: the state as a record (only used for `state.error` text lookups).
    pub fn state_dict(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("status".into(), Value::String(self.status.as_str().into()));
        m.insert("input".into(), self.input.clone());
        if self.status != ToolStatus::Streaming {
            m.insert("metadata".into(), self.metadata.clone());
        }
        if matches!(self.status, ToolStatus::Completed | ToolStatus::Error) {
            m.insert("content".into(), Value::Array(self.content.clone()));
        }
        if let Some(e) = &self.error {
            m.insert("error".into(), e.clone());
        }
        m
    }
}

/// A single append-only commit to scrollback.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct StreamCommit {
    pub kind: EntryKind,
    pub text: String,
    pub image: Option<String>,
    pub phase: Phase,
    pub source: Source,
    pub compaction: bool,
    pub summary: Option<TurnSummary>,
    pub message_id: Option<String>,
    pub part_id: Option<String>,
    pub tool: Option<String>,
    pub directory: Option<String>,
    pub part: Option<ToolPart>,
    pub interrupted: bool,
    pub tool_state: Option<ToolStateKind>,
    pub tool_error: Option<String>,
    /// Direct shell (`!cmd`) commits carry the command.
    pub shell: Option<String>,
}

impl StreamCommit {
    pub fn new(kind: EntryKind, text: impl Into<String>, phase: Phase, source: Source) -> StreamCommit {
        StreamCommit { kind, text: text.into(), phase, source, ..Default::default() }
    }
}

/// A local scrollback row kept for resize replay.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalReplayRow {
    pub commit: StreamCommit,
}

/// Mention source range in the prompt (`{start, end, value}`).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct MentionRange {
    pub start: usize,
    pub end: usize,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PromptPart {
    File {
        url: String,
        filename: Option<String>,
        mime: Option<String>,
        description: Option<String>,
        /// `source: {type: "file", path, text: {start, end, value}}`
        source: Option<(String, MentionRange)>,
    },
    Agent {
        name: String,
        source: Option<MentionRange>,
    },
    Skill {
        id: String,
        source: Option<MentionRange>,
    },
}

impl PromptPart {
    pub fn is_file(&self) -> bool {
        matches!(self, PromptPart::File { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Steer,
    Queue,
}

impl Delivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Delivery::Steer => "steer",
            Delivery::Queue => "queue",
        }
    }
    pub fn parse(s: &str) -> Delivery {
        if s == "queue" { Delivery::Queue } else { Delivery::Steer }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PromptCommand {
    pub name: String,
    pub arguments: String,
    pub source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct RunPrompt {
    pub message_id: Option<String>,
    pub text: String,
    pub parts: Vec<PromptPart>,
    pub delivery: Option<Delivery>,
    pub shell: bool,
    pub command: Option<PromptCommand>,
}

impl RunPrompt {
    pub fn text(text: impl Into<String>) -> RunPrompt {
        RunPrompt { text: text.into(), ..Default::default() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueuedPrompt {
    pub message_id: String,
    pub prompt: RunPrompt,
    pub delivery: Delivery,
    pub skills: Vec<(String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueuedPromptAction {
    Steer,
    Queue,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct RunAgent {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// `subagent` | `primary` | `all`
    pub mode: String,
    pub hidden: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct RunCommand {
    pub name: String,
    pub description: Option<String>,
    pub source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct RunReference {
    pub name: String,
    pub path: String,
    pub description: Option<String>,
    /// `source.type` (`git` | `local`), `source.repository` / `source.path`.
    pub source_type: String,
    pub source_detail: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct RunProviderModel {
    pub name: Option<String>,
    pub cost_input: Option<f64>,
    pub context: Option<f64>,
    pub status: Option<String>,
    pub variants: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct RunProvider {
    pub id: String,
    pub name: String,
    pub models: indexmap::IndexMap<String, RunProviderModel>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModelSel {
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FooterPhase {
    #[default]
    Idle,
    Running,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Usage {
    pub tokens: f64,
    pub percent: Option<i64>,
    pub cost: Option<f64>,
}

/// Footer status-bar state.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct FooterState {
    pub phase: FooterPhase,
    pub status: String,
    pub notice: String,
    pub model: String,
    pub usage: Option<Usage>,
    pub first: bool,
    pub interrupt: u32,
    pub exit: u32,
}

/// Partial update of [`FooterState`].
#[derive(Clone, Debug, PartialEq, Default)]
pub struct FooterPatch {
    pub phase: Option<FooterPhase>,
    pub status: Option<String>,
    pub notice: Option<String>,
    pub model: Option<String>,
    /// `Some(None)` clears the usage (`"usage" in patch`).
    pub usage: Option<Option<Usage>>,
    pub first: Option<bool>,
    pub interrupt: Option<u32>,
    pub exit: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FooterView {
    Prompt,
    Permission(Box<MiniPermission>),
    Form(Box<MiniForm>),
}

impl FooterView {
    pub fn kind(&self) -> &'static str {
        match self {
            FooterView::Prompt => "prompt",
            FooterView::Permission(_) => "permission",
            FooterView::Form(_) => "form",
        }
    }
    pub fn request_id(&self) -> Option<&str> {
        match self {
            FooterView::Prompt => None,
            FooterView::Permission(p) => Some(&p.request.id),
            FooterView::Form(f) => Some(&f.form.id),
        }
    }
}

/// `MiniPermissionRequest`: a permission request with its source tool part when known.
#[derive(Clone, Debug, PartialEq)]
pub struct MiniPermission {
    pub request: crate::api::types::PermissionRequest,
    pub tool: Option<ToolPart>,
}

/// `MiniFormRequest`: a form with its location (global forms).
#[derive(Clone, Debug, PartialEq)]
pub struct MiniForm {
    pub form: crate::api::types::FormInfo,
    pub location: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptRoute {
    Composer,
    QueuedMenu,
    SubagentMenu,
    Subagent(String),
    Command,
    Agent,
    Model,
    Variant,
    Settings,
}

impl PromptRoute {
    pub fn name(&self) -> &'static str {
        match self {
            PromptRoute::Composer => "composer",
            PromptRoute::QueuedMenu => "queued-menu",
            PromptRoute::SubagentMenu => "subagent-menu",
            PromptRoute::Subagent(_) => "subagent",
            PromptRoute::Command => "command",
            PromptRoute::Agent => "agent",
            PromptRoute::Model => "model",
            PromptRoute::Variant => "variant",
            PromptRoute::Settings => "settings",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubagentStatus {
    Running,
    Completed,
    Cancelled,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubagentTab {
    pub session_id: String,
    pub label: String,
    pub description: String,
    pub status: SubagentStatus,
    pub background: bool,
    pub title: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct SubagentState {
    pub tabs: Vec<SubagentTab>,
    pub details: indexmap::IndexMap<String, Vec<StreamCommit>>,
    pub permissions: Vec<MiniPermission>,
    pub forms: Vec<MiniForm>,
}

/// Mini transcript/terminal settings (`mini.*` in the TUI config).
#[derive(Clone, Debug, PartialEq)]
pub struct MiniSettings {
    pub thinking: bool,
    pub tools: bool,
    pub shell_output: bool,
    pub turn_summary: bool,
    pub footer: bool,
    pub splash: bool,
    pub work_spinner: String,
    pub mono: bool,
}

impl Default for MiniSettings {
    /// `resolveMiniSettings()` defaults.
    fn default() -> Self {
        MiniSettings {
            thinking: true,
            tools: false,
            shell_output: false,
            turn_summary: true,
            footer: true,
            splash: true,
            work_spinner: "block-soft-slide".into(),
            mono: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verbosity {
    Quiet,
    Default,
    Everything,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MiniSettingChange {
    Thinking(bool),
    Tools(bool),
    ShellOutput(bool),
    TurnSummary(bool),
    Footer(bool),
    Splash(bool),
    WorkSpinner(String),
    Mono(bool),
    Verbosity(Verbosity),
}

pub fn show(v: bool) -> &'static str {
    if v { "show" } else { "hide" }
}
