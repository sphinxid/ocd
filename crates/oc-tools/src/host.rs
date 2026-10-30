//! Everything tools need from core (`ToolHost`). Core implements this trait once per
//! Location/session runtime. Every method has a default that reports the capability as
//! unavailable, so integration can be incremental.

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use futures::future::BoxFuture;
use oc_schema::{Metadata, ModelRef};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `source: {type:"tool", messageID, id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionSource {
    #[serde(rename = "messageID")]
    pub message_id: String,
    #[serde(rename = "id")]
    pub call_id: String,
}

/// `Permission.assert` input.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub session_id: String,
    pub agent: Option<String>,
    pub action: String,
    pub resources: Vec<String>,
    /// What an "always" reply persists. `None` when the leaf passes no `save` (question).
    pub save: Option<Vec<String>>,
    pub metadata: Option<Metadata>,
    pub source: PermissionSource,
}

/// `Permission.assert` failures.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionError {
    /// User declined (defect tunnel; ends the step).
    Declined,
    /// Declined with feedback (`Permission.CorrectedError`): model-visible feedback.
    Corrected { feedback: String },
    /// A deny rule matched (`Permission.BlockedError`): its message.
    Blocked { message: String },
    /// Location shut down while waiting.
    LocationClosed,
    /// Session not found or other error.
    Other { message: String },
}

impl PermissionError {
    /// Convert into the cause/defect form leaves use.
    pub fn into_tool_error(self, leaf_message: impl Into<String>) -> crate::tool::ToolError {
        use crate::tool::{ErrorCause, ToolError};
        match self {
            PermissionError::Declined => ToolError::declined(),
            PermissionError::LocationClosed => ToolError::location_closed(),
            PermissionError::Corrected { feedback } => {
                ToolError::with_cause(leaf_message, ErrorCause::PermissionRejected(feedback))
            }
            PermissionError::Blocked { message } => {
                ToolError::with_cause(leaf_message, ErrorCause::PermissionRejected(message))
            }
            PermissionError::Other { message } => ToolError::with_cause(leaf_message, ErrorCause::Error(message)),
        }
    }
}

/// `Form.Field` option.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormOption {
    pub value: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `Form.Field`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormField {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `string` | `multiselect`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    pub options: Vec<FormOption>,
    pub custom: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FormRequest {
    pub session_id: String,
    pub title: String,
    pub metadata: Metadata,
    pub fields: Vec<FormField>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FormResponse {
    /// Answers keyed by field key (string, array of strings, ...).
    Answered(Metadata),
    Cancelled { message: Option<String> },
}

/// `Skill.Info` subset.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// Absolute path of the skill file.
    pub path: PathBuf,
    pub content: String,
}

/// Session summary for subagent/opencode tools.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub id: String,
    pub parent_id: Option<String>,
    pub agent: Option<String>,
    pub model: Option<ModelRef>,
}

/// `Agent.Info` subset.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentSummary {
    pub id: String,
    pub name: String,
    /// `primary` | `subagent` | `all`.
    pub mode: String,
    pub hidden: bool,
    pub description: Option<String>,
    pub model: Option<ModelRef>,
}

/// A model entry from `models.available()` / `model.list()`.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelSummary {
    pub provider_id: String,
    pub id: String,
    pub name: String,
    pub family: Option<String>,
    /// Release timestamp ms (0 when unknown).
    pub released: f64,
    pub variants: Vec<String>,
    /// `Model.Info.cost` encoded JSON.
    pub cost: Value,
    /// `alpha` | `beta` | `deprecated` | `active`.
    pub status: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderSummary {
    pub id: String,
    pub name: String,
}

/// Job recovery descriptor (`Job.Recovery`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum JobRecovery {
    #[serde(rename = "shell")]
    Shell {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "shellID")]
        shell_id: String,
        command: String,
    },
    #[serde(rename = "subagent")]
    Subagent {
        #[serde(rename = "parentSessionID")]
        parent_session_id: String,
        #[serde(rename = "childSessionID")]
        child_session_id: String,
        agent: String,
        description: String,
    },
}

/// `Job.Status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Running,
    Completed,
    Error,
    Cancelled,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobStatus::Running => "running",
            JobStatus::Completed => "completed",
            JobStatus::Error => "error",
            JobStatus::Cancelled => "cancelled",
        }
    }
}

/// `Job.Info`.
#[derive(Debug, Clone, PartialEq)]
pub struct JobInfo {
    pub id: String,
    pub kind: String,
    pub title: Option<String>,
    pub status: JobStatus,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub notification_id: Option<String>,
}

/// `Job.start` input. `run` resolves to the job output text or an error message.
pub struct JobStart {
    pub id: String,
    pub kind: String,
    pub title: Option<String>,
    pub metadata: Metadata,
    pub recovery: Option<JobRecovery>,
    pub run: BoxFuture<'static, Result<String, String>>,
}

/// `Job.block` result.
#[derive(Debug, Clone, PartialEq)]
pub enum JobBlockResult {
    Finished(JobInfo),
    Backgrounded(JobInfo),
}

/// `sessions.synthetic(...)` input (background completion notifications).
#[derive(Debug, Clone, PartialEq)]
pub struct SyntheticMessage {
    pub id: Option<String>,
    pub session_id: String,
    pub description: String,
    pub text: String,
    pub metadata: Metadata,
}

/// Shell invocation passed through the `shell.create.before` plugin hook.
#[derive(Debug, Clone, PartialEq)]
pub struct ShellInvocation {
    pub command: String,
    pub cwd: String,
    pub timeout: u64,
    pub shell: String,
    pub env: HashMap<String, String>,
}

/// Worktree removal failure classes (`opencode.ts removeFailure`).
#[derive(Debug, Clone, PartialEq)]
pub enum WorktreeRemoveError {
    DirectoryUnavailable,
    InvalidDirectory,
    StrategyUnavailable { strategy: String },
    ForceRequired,
    Other(String),
}

/// MCP tool call content part.
#[derive(Debug, Clone, PartialEq)]
pub enum McpContent {
    Text(String),
    /// image/audio/blob data (base64) with its mime; `uri` names an embedded resource's blob.
    Data { data: String, mime: String, uri: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpCallResult {
    pub content: Vec<McpContent>,
    pub structured: Option<Value>,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum McpCallError {
    /// `MCP.NotFoundError` (server).
    NotFound { server: String },
    /// `MCP.ToolCallError` (message).
    ToolCall { message: String },
    Other(String),
}

pub fn unsupported(what: &str) -> String {
    format!("{what} is not available")
}

/// Core services a tool may call.
#[async_trait]
pub trait ToolHost: Send + Sync {
    // ---------------------------------------------------------------- permissions & forms
    async fn ask_permission(&self, request: PermissionRequest) -> Result<(), PermissionError>;

    async fn ask_form(&self, _request: FormRequest) -> Result<FormResponse, String> {
        Err(unsupported("forms"))
    }

    /// The web search service of the session's location (`None`: the tool's own service).
    fn websearch(&self) -> Option<std::sync::Arc<crate::websearch::WebSearch>> {
        None
    }

    // ---------------------------------------------------------------- instructions (read tool)
    /// `SessionInstructions.load({sessionID, paths})` for nested AGENTS.md files.
    async fn load_instructions(&self, _session_id: &str, _paths: Vec<PathBuf>) -> Result<(), String> {
        Ok(())
    }

    // ---------------------------------------------------------------- skills
    async fn get_skill(&self, _id: &str) -> Result<Option<SkillInfo>, String> {
        Err(unsupported("skills"))
    }

    async fn list_skills(&self) -> Result<Vec<SkillInfo>, String> {
        Ok(Vec::new())
    }

    // ---------------------------------------------------------------- sessions / agents / models
    async fn session_get(&self, _session_id: &str) -> Result<SessionSummary, String> {
        Err(unsupported("sessions"))
    }

    async fn session_create(
        &self,
        _parent_id: &str,
        _title: &str,
        _agent: &str,
        _model: Option<ModelRef>,
    ) -> Result<SessionSummary, String> {
        Err(unsupported("sessions"))
    }

    async fn session_switch_agent(&self, _session_id: &str, _agent: &str) -> Result<(), String> {
        Err(unsupported("sessions"))
    }

    async fn session_switch_model(&self, _session_id: &str, _model: ModelRef) -> Result<(), String> {
        Err(unsupported("sessions"))
    }

    /// `sessions.prompt({sessionID, text, resume?})`.
    async fn session_prompt(&self, _session_id: &str, _text: &str, _resume: Option<bool>) -> Result<(), String> {
        Err(unsupported("sessions"))
    }

    async fn session_interrupt(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn session_rename(&self, _session_id: &str, _title: &str) -> Result<(), String> {
        Err(unsupported("sessions"))
    }

    /// `session.move({sessionID, directory, delivery:"steer"})`.
    async fn session_move(&self, _session_id: &str, _directory: &str) -> Result<(), String> {
        Err(unsupported("sessions"))
    }

    async fn session_synthetic(&self, _message: SyntheticMessage) -> Result<(), String> {
        Ok(())
    }

    /// Session environment variables (`SessionEnvironment.get`); `None` → process env.
    async fn session_environment(&self, _session_id: &str) -> Option<HashMap<String, String>> {
        None
    }

    async fn agent_resolve(&self, _name: &str) -> Result<Option<AgentSummary>, String> {
        Err(unsupported("agents"))
    }

    async fn models_available(&self) -> Result<Vec<ModelSummary>, String> {
        Err(unsupported("models"))
    }

    /// Enabled models of the given types (`ctx.model.list({types})`), chat when none are given.
    async fn models_list(&self, _types: &[String]) -> Result<Vec<ModelSummary>, String> {
        Err(unsupported("models"))
    }

    async fn providers_list(&self) -> Result<Vec<ProviderSummary>, String> {
        Err(unsupported("providers"))
    }

    // ---------------------------------------------------------------- jobs
    async fn job_start(&self, start: JobStart) -> Result<JobInfo, String> {
        // Default: run inline in the background with no job registry.
        let info = JobInfo {
            id: start.id.clone(),
            kind: start.kind.clone(),
            title: start.title.clone(),
            status: JobStatus::Running,
            started_at: crate::util::now_ms(),
            completed_at: None,
            output: None,
            error: None,
            notification_id: None,
        };
        tokio::spawn(start.run);
        Ok(info)
    }

    /// Wait for the job; `Backgrounded` when the user moves it to the background.
    async fn job_block(&self, _id: &str, _session_id: &str) -> Result<Option<JobBlockResult>, String> {
        Err(unsupported("jobs"))
    }

    async fn job_background(&self, _id: &str) -> Result<Option<JobInfo>, String> {
        Ok(None)
    }

    async fn job_wait(&self, _id: &str) -> Result<Option<JobInfo>, String> {
        Ok(None)
    }

    async fn job_cancel(&self, _id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn job_complete_background(&self, _notification_id: &str) -> Result<(), String> {
        Ok(())
    }

    /// `SubagentJob.start(recovery)`.
    async fn subagent_job_start(&self, _recovery: JobRecovery) -> Result<(), String> {
        Ok(())
    }

    /// `SubagentJob.background(recovery)`.
    async fn subagent_job_background(&self, _recovery: JobRecovery) -> Result<(), String> {
        Ok(())
    }

    /// `SubagentJob.notify(recovery, startedAt)`.
    async fn subagent_job_notify(&self, _recovery: JobRecovery, _started_at: u64) -> Result<(), String> {
        Ok(())
    }

    // ---------------------------------------------------------------- shell
    /// `shell.create.before` plugin hook.
    async fn shell_create_before(&self, _invocation: &mut ShellInvocation) -> Result<(), String> {
        Ok(())
    }

    // ---------------------------------------------------------------- worktrees
    async fn worktree_create(&self, _project_id: &str, _name: &str) -> Result<Value, String> {
        Err(unsupported("worktrees"))
    }

    async fn worktree_list(&self, _project_id: &str) -> Result<Value, String> {
        Err(unsupported("worktrees"))
    }

    async fn worktree_remove(&self, _project_id: &str, _directory: &str, _force: bool) -> Result<(), WorktreeRemoveError> {
        Err(WorktreeRemoveError::Other(unsupported("worktrees")))
    }

    // ---------------------------------------------------------------- MCP
    async fn mcp_servers(&self) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    /// `{resources, templates}` across all servers.
    async fn mcp_resource_catalog(&self) -> Result<Value, String> {
        Err(unsupported("MCP"))
    }

    /// `{resources, templates}` for one server.
    async fn mcp_resources(&self, _server: &str) -> Result<Value, String> {
        Err(unsupported("MCP"))
    }

    /// `Mcp.ResourceContent` (`{server, uri, contents}`) or `None` when not connected.
    async fn mcp_read_resource(&self, _server: &str, _uri: &str) -> Result<Option<Value>, String> {
        Err(unsupported("MCP"))
    }

    async fn mcp_call_tool(
        &self,
        _server: &str,
        _name: &str,
        _args: Value,
        _session_id: &str,
    ) -> Result<McpCallResult, McpCallError> {
        Err(McpCallError::Other(unsupported("MCP")))
    }

    // ---------------------------------------------------------------- formatter support
    /// `Npm.which(pkg)` (may install the package).
    async fn npm_which(&self, _package: &str) -> Option<String> {
        None
    }

    // ---------------------------------------------------------------- file change notifications
    /// Called after mutation tools changed files (watchers/LSP when they exist upstream).
    async fn files_changed(&self, _session_id: &str, _paths: &[PathBuf]) {}
}
