//! The tool interface (`schema/src/tool.ts`, `core/src/tool/runtime.ts`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use oc_schema::{Metadata, ToolContent};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::host::ToolHost;

/// Registration options (`Tool.Options`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolOptions {
    /// Dotted namespace; flattens the model name to `<namespace with _>_<name>`.
    pub namespace: Option<String>,
    /// Permission action used for whole-tool visibility filtering (defaults to the effective name).
    pub permission: Option<String>,
    /// `false` keeps the tool on the provider's native list; `true` (default) routes it through CodeMode.
    pub codemode: bool,
    /// Always shown in the CodeMode catalog.
    pub pinned: bool,
}

impl ToolOptions {
    pub fn direct() -> Self {
        Self { codemode: false, ..Self::default() }
    }

    pub fn codemode(namespace: &str) -> Self {
        Self { namespace: Some(namespace.to_string()), codemode: true, ..Self::default() }
    }
}

/// Definition sent to the model (`{type:"tool", name, description, inputSchema, outputSchema?}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(rename = "outputSchema", skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

impl ToolDefinition {
    pub fn new(name: impl Into<String>, description: impl Into<String>, input_schema: Value) -> Self {
        Self { kind: "tool".into(), name: name.into(), description: description.into(), input_schema, output_schema: None }
    }

    pub fn with_output(mut self, output: Option<Value>) -> Self {
        self.output_schema = output;
        self
    }
}

/// Summary of a subagent for the `subagent` description hook.
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentSummary {
    pub id: String,
    pub description: Option<String>,
}

/// Inputs to the per-request description/visibility hooks (session `context`/`compaction`/`generate`).
/// `DefinitionContext::default()` yields the registered (pre-hook) definitions.
#[derive(Debug, Clone, Default)]
pub struct DefinitionContext {
    /// Model id of the request (`gpt-` patch/edit swap).
    pub model_id: Option<String>,
    /// Compatible shell path (shell description names `ShellSelect.name(shell)`).
    pub shell: Option<String>,
    /// Subagents already filtered (mode != primary, not hidden, `subagent` permission not denied), any order.
    pub subagents: Option<Vec<SubagentSummary>>,
    /// Web search selection is `false`.
    pub websearch_disabled: bool,
}

impl DefinitionContext {
    /// Hooks only run for real requests, which always carry a model.
    pub fn is_hooked(&self) -> bool {
        self.model_id.is_some()
    }
}

/// Image limits (`media.image.*`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageLimits {
    pub auto_resize: bool,
    pub max_width: u32,
    pub max_height: u32,
    pub max_base64_bytes: u64,
}

impl Default for ImageLimits {
    fn default() -> Self {
        Self { auto_resize: true, max_width: 2_000, max_height: 2_000, max_base64_bytes: 5 * 1024 * 1024 }
    }
}

/// Config values tools read.
#[derive(Debug, Clone)]
pub struct ToolConfig {
    /// `tool_output.max_lines` / `tool_output.max_bytes` (None → 2000 / 51200).
    pub tool_output_max_lines: Option<usize>,
    pub tool_output_max_bytes: Option<usize>,
    /// `formatter` config (`None`/`false` → formatting off).
    pub formatter: crate::formatter::FormatterConfig,
    /// `experimental.portable_shell_scanner`.
    pub portable_shell_scanner: Option<bool>,
    /// `experimental.subagent_depth`.
    pub subagent_depth: Option<u64>,
    /// `shell` config.
    pub shell: Option<String>,
    /// App channel (`local`/`dev` select the portable shell scanner by default).
    pub channel: String,
    /// `media.image.*`.
    pub image: ImageLimits,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            tool_output_max_lines: None,
            tool_output_max_bytes: None,
            formatter: crate::formatter::FormatterConfig::Disabled,
            portable_shell_scanner: None,
            subagent_depth: None,
            shell: None,
            channel: "prod".into(),
            image: ImageLimits::default(),
        }
    }
}

impl ToolConfig {
    pub fn max_lines(&self) -> usize {
        self.tool_output_max_lines.unwrap_or(crate::tool_output::MAX_LINES)
    }

    pub fn max_bytes(&self) -> usize {
        self.tool_output_max_bytes.unwrap_or(crate::tool_output::MAX_BYTES)
    }
}

pub type ProgressFn = Arc<dyn Fn(Metadata) + Send + Sync>;

/// Per-call execution context.
#[derive(Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub message_id: String,
    /// Tool call id.
    pub call_id: String,
    pub agent: String,
    /// Location directory.
    pub directory: PathBuf,
    /// Project (worktree) directory.
    pub worktree: PathBuf,
    pub project_id: String,
    /// Location workspace id (shell env: none when a workspace is set).
    pub workspace_id: Option<String>,
    pub global: Arc<oc_util::Global>,
    pub cancel: CancellationToken,
    /// Publishes `session.tool.progress` metadata.
    pub progress: ProgressFn,
    pub config: Arc<ToolConfig>,
    pub host: Arc<dyn ToolHost>,
}

impl ToolContext {
    pub fn progress(&self, metadata: Metadata) {
        (self.progress)(metadata)
    }

    pub fn permission_source(&self) -> crate::host::PermissionSource {
        crate::host::PermissionSource { message_id: self.message_id.clone(), call_id: self.call_id.clone() }
    }

    /// Build a permission request for this invocation.
    pub fn permission(&self, action: &str, resources: Vec<String>, save: Option<Vec<String>>, metadata: Option<Metadata>) -> crate::host::PermissionRequest {
        crate::host::PermissionRequest {
            session_id: self.session_id.clone(),
            agent: Some(self.agent.clone()),
            action: action.to_string(),
            resources,
            save,
            metadata,
            source: self.permission_source(),
        }
    }
}

/// A successful local tool result (mirrors `session.tool.success` + declared output).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolResult {
    /// Declared structured output (CodeMode reads it; providers see `content`).
    pub output: Option<Value>,
    /// Model content. Empty → normalized to `[text: stringify(output)]`.
    pub content: Vec<ToolContent>,
    pub metadata: Option<Metadata>,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self { output: None, content: vec![ToolContent::Text { text: text.into() }], metadata: None }
    }

    pub fn with_output(mut self, output: Value) -> Self {
        self.output = Some(output);
        self
    }

    pub fn with_metadata(mut self, metadata: Metadata) -> Self {
        self.metadata = Some(metadata);
        self
    }

    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| match c {
                ToolContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The underlying cause of a tool failure. `toSessionError` unwraps it for the model.
#[derive(Debug, Clone, PartialEq)]
pub enum ErrorCause {
    /// A plain error; `message` may be empty (Effect tagged errors without a message getter).
    Error(String),
    /// A nested tool failure (unwrapped recursively).
    Tool(Box<ToolError>),
    /// `Permission.BlockedError` (message) / `Permission.CorrectedError` (feedback).
    PermissionRejected(String),
}

impl ErrorCause {
    pub fn msg(message: impl Into<String>) -> Self {
        ErrorCause::Error(message.into())
    }
}

/// Classifies how a failure is handled by the session runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolErrorKind {
    /// Ordinary model-visible failure (`ToolFailure`).
    Failure,
    /// User declined a permission request (defect tunnel; ends the step).
    Declined,
    /// User dismissed a question without a message (`The user dismissed this question`; ends the step).
    QuestionDismissed,
    /// Location shut down while waiting for an interaction.
    LocationClosed,
    /// Cancelled via the context's cancellation token.
    Interrupted,
}

/// A tool failure (`Tool.Error` / `ToolFailure`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolError {
    pub message: String,
    pub cause: Option<ErrorCause>,
    pub content: Option<Vec<ToolContent>>,
    pub metadata: Option<Metadata>,
    pub kind: ToolErrorKind,
}

/// `SessionError.Error` subset produced by `toSessionError` for tool failures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionErrorInfo {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), cause: None, content: None, metadata: None, kind: ToolErrorKind::Failure }
    }

    pub fn with_cause(message: impl Into<String>, cause: ErrorCause) -> Self {
        Self { cause: Some(cause), ..Self::new(message) }
    }

    pub fn caused_by(message: impl Into<String>, cause: impl Into<String>) -> Self {
        Self::with_cause(message, ErrorCause::Error(cause.into()))
    }

    pub fn declined() -> Self {
        Self { kind: ToolErrorKind::Declined, ..Self::new("The user declined this tool call") }
    }

    pub fn question_dismissed() -> Self {
        Self { kind: ToolErrorKind::QuestionDismissed, ..Self::new("The user dismissed this question") }
    }

    pub fn location_closed() -> Self {
        Self { kind: ToolErrorKind::LocationClosed, ..Self::new("Interaction cancelled because the location shut down") }
    }

    pub fn interrupted() -> Self {
        Self { kind: ToolErrorKind::Interrupted, ..Self::new("Tool execution interrupted") }
    }

    pub fn with_metadata(mut self, metadata: Metadata) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// True for failures that tunnel past leaf error mapping (declines, dismissals, interrupts).
    pub fn is_defect(&self) -> bool {
        self.kind != ToolErrorKind::Failure
    }

    /// Wrap a lower-level error as the cause of a leaf failure, unless it is a defect
    /// (declines/interrupts pass through untouched, like upstream `mapError` blankets).
    pub fn wrap(self, message: impl Into<String>) -> Self {
        if self.is_defect() {
            return self;
        }
        ToolError::with_cause(message, ErrorCause::Tool(Box::new(self)))
    }

    /// `toSessionError` for a tool failure (§0.9): unwrap the cause recursively; an empty
    /// cause message falls back to this failure's message.
    pub fn to_session_error(&self) -> SessionErrorInfo {
        let Some(cause) = &self.cause else {
            return SessionErrorInfo { kind: "tool.execution".into(), message: self.message.clone() };
        };
        let unwrapped = match cause {
            ErrorCause::Error(message) => SessionErrorInfo { kind: "unknown".into(), message: message.clone() },
            ErrorCause::PermissionRejected(message) => {
                SessionErrorInfo { kind: "permission.rejected".into(), message: message.clone() }
            }
            ErrorCause::Tool(inner) => inner.to_session_error(),
        };
        if unwrapped.message.is_empty() {
            SessionErrorInfo { kind: "tool.execution".into(), message: self.message.clone() }
        } else {
            unwrapped
        }
    }

    /// The text the model sees.
    pub fn model_message(&self) -> String {
        self.to_session_error().message
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ToolError {}

/// A tool implementation.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Registered (un-namespaced) name.
    fn name(&self) -> &str;

    fn options(&self) -> ToolOptions {
        ToolOptions::direct()
    }

    /// Definition with description and JSON schemas exactly as upstream emits.
    /// `name` in the returned definition is the effective name.
    fn definition(&self, ctx: &DefinitionContext) -> ToolDefinition;

    /// Whether the tool is advertised for this request (session hooks may delete tools).
    fn advertised(&self, _ctx: &DefinitionContext) -> bool {
        true
    }

    /// Validate raw model input; return issues in upstream order. Default validates against
    /// the input JSON schema with Effect-compatible messages.
    fn validate(&self, input: &Value) -> Vec<crate::validate::Issue> {
        let definition = self.definition(&DefinitionContext::default());
        crate::validate::validate(&definition.input_schema, input, crate::validate::Flavor::Effect)
    }

    /// `tool.execute.before` rewrite owned by the tool's plugin (e.g. subagent strips empty strings).
    fn prepare_input(&self, input: Value) -> Value {
        input
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError>;
}

/// `Tool.Namespace`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Namespace {
    pub name: String,
    pub description: String,
}

/// Normalized tool name: `[^a-zA-Z0-9_-]` → `_`.
pub fn normalized_name(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

/// Effective (model) name.
pub fn effective_name(name: &str, namespace: Option<&str>) -> String {
    match namespace {
        None => normalized_name(name),
        Some(ns) => format!("{}_{}", ns.replace('.', "_"), normalized_name(name)),
    }
}

/// Helper: parse a JSON schema stored under `schemas/`.
pub(crate) fn schema_json(text: &str) -> (Value, Option<Value>) {
    let value: Value = serde_json::from_str(text).expect("valid embedded schema");
    let input = value.get("inputSchema").cloned().unwrap_or(Value::Null);
    let output = value.get("outputSchema").cloned();
    (input, output)
}

/// Helper to build metadata maps.
pub fn meta(pairs: Vec<(&str, Value)>) -> Metadata {
    let mut map = Metadata::new();
    for (k, v) in pairs {
        map.insert(k.to_string(), v);
    }
    map
}

/// Decode tool input after validation.
pub fn decode<T: serde::de::DeserializeOwned>(input: Value) -> Result<T, ToolError> {
    serde_json::from_value(input).map_err(|e| ToolError::new(e.to_string()))
}

pub type Env = HashMap<String, String>;
