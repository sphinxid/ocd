//! Wire types for the opencode v2 HTTP API (subset consumed by the TUI). Every struct is lenient:
//! unknown fields are ignored and optional fields default, so newer servers stay compatible.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelRef {
    pub id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheUsage {
    #[serde(default)]
    pub read: f64,
    #[serde(default)]
    pub write: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub reasoning: f64,
    #[serde(default)]
    pub cache: CacheUsage,
}

impl TokenUsage {
    pub fn total(&self) -> f64 {
        self.input + self.output + self.reasoning + self.cache.read + self.cache.write
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LocationRef {
    pub directory: String,
    #[serde(rename = "workspaceID", default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionError {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<i64>,
    /// Milliseconds until a rate or usage limit resets (`provider.free-limit`).
    #[serde(rename = "retryAfterMs", default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<f64>,
    /// The provider's refusal (`provider.*-limit` errors), whose body names the limit and its reset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ErrorResponse>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    #[serde(default)]
    pub body: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FileDiff {
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub patch: String,
    #[serde(default)]
    pub additions: i64,
    #[serde(default)]
    pub deletions: i64,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Revert {
    #[serde(rename = "messageID")]
    pub message_id: String,
    #[serde(rename = "partID", default, skip_serializing_if = "Option::is_none")]
    pub part_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FileDiff>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionTime {
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub updated: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewed: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(rename = "parentID", default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(rename = "projectID", default)]
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(default)]
    pub cost: f64,
    #[serde(default)]
    pub tokens: TokenUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default)]
    pub time: SessionTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub location: LocationRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revert: Option<Revert>,
}

impl SessionInfo {
    /// `withTimestampedFallback`.
    pub fn display_title(&self) -> String {
        match &self.title {
            Some(t) if !t.is_empty() => t.clone(),
            _ => {
                let prefix = if self.parent_id.is_some() { "Child session" } else { "New session" };
                format!("{prefix} - {}", crate::util::locale::iso(self.time.created))
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageTime {
    #[serde(default)]
    pub created: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streamed: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Mention {
    #[serde(default)]
    pub start: f64,
    #[serde(default)]
    pub end: f64,
    #[serde(default)]
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FileAttachment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mention: Option<Mention>,
}

impl FileAttachment {
    pub fn source_uri(&self) -> Option<String> {
        self.uri.clone().or_else(|| self.source.as_ref()?.get("uri")?.as_str().map(|s| s.to_string()))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentAttachment {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mention: Option<Mention>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SkillAttachment {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mention: Option<Mention>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolContent {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolState {
    /// `streaming` | `running` | `completed` | `error`.
    pub status: String,
    /// Raw partial JSON string while streaming, an object afterwards.
    #[serde(default)]
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ToolContent>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<SessionError>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolTime {
    #[serde(default)]
    pub created: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ran: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PartTime {
    #[serde(default)]
    pub created: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Content {
    Text {
        #[serde(default)]
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native: Option<Value>,
    },
    Reasoning {
        #[serde(default)]
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        time: Option<PartTime>,
    },
    Tool(ToolPart),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolPart {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executed: Option<bool>,
    pub state: ToolState,
    #[serde(default)]
    pub time: ToolTime,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Retry {
    #[serde(default)]
    pub attempt: i64,
    #[serde(default)]
    pub at: i64,
    #[serde(default)]
    pub error: SessionError,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ShellOutput {
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub cursor: i64,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub truncated: bool,
}

/// One projected session message. Variant-specific fields are optional on a single struct so
/// that live events can mutate messages without re-tagging.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub time: MessageTime,
    // user / synthetic / system / skill
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FileAttachment>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentAttachment>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<SkillAttachment>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    // agent-switched / model-switched / location-switched
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<LocationRef>,
    // shell
    #[serde(rename = "shellID", default, skip_serializing_if = "Option::is_none")]
    pub shell_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<ShellOutput>,
    // assistant
    #[serde(default)]
    pub content: Vec<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<SessionError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<Retry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenUsage>,
    // compaction
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// A provider compaction's native data; its replacement window is `native.messages`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<Value>,
    // idle
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Client-side content revision (not on the wire): `Rev(0)` means "new or changed since the
    /// transcript cache last saw it". See [`Message::touch`].
    #[serde(skip)]
    pub rev: Rev,
}

/// Content revision of a [`Message`], used by the incremental transcript renderer. Every in-place
/// edit resets it to 0 (`Message::touch`); new messages start at 0; the transcript cache stamps a
/// fresh, process-unique value on every 0 it finds. It never takes part in equality.
#[derive(Clone, Copy, Debug, Default)]
pub struct Rev(pub u64);

impl PartialEq for Rev {
    fn eq(&self, _: &Rev) -> bool {
        true
    }
}

impl Rev {
    /// A fresh, process-unique non-zero revision.
    pub fn next() -> Rev {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Rev(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

impl Message {
    pub fn from_value(v: Value) -> Option<Message> {
        serde_json::from_value(v).ok()
    }
    /// Mark the message as changed for the transcript cache. Call after every in-place edit.
    pub fn touch(&mut self) {
        self.rev = Rev(0);
    }
    pub fn is_assistant(&self) -> bool {
        self.kind == "assistant"
    }
    pub fn text_str(&self) -> &str {
        self.text.as_deref().unwrap_or("")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InboxItem {
    pub id: String,
    #[serde(rename = "sessionID", default)]
    pub session_id: String,
    #[serde(default)]
    pub time: MessageTime,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub delivery: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub id: String,
    #[serde(rename = "sessionID", default)]
    pub session_id: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub resources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl PermissionRequest {
    /// Tool call id this request belongs to, if any.
    pub fn tool_id(&self) -> Option<&str> {
        self.source.as_ref()?.get("id")?.as_str()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FormInfo {
    pub id: String,
    #[serde(rename = "sessionID", default)]
    pub session_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub fields: Vec<Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelVariant {
    pub id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelCost {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelLimit {
    #[serde(default)]
    pub context: f64,
    #[serde(default)]
    pub output: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelReleased {
    #[serde(default)]
    pub released: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(rename = "modelID", default)]
    pub model_id: String,
    #[serde(rename = "providerID", default)]
    pub provider_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub variants: Vec<ModelVariant>,
    #[serde(default)]
    pub time: ModelReleased,
    #[serde(default)]
    pub cost: Vec<ModelCost>,
    #[serde(default)]
    pub status: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub limit: ModelLimit,
}

fn yes() -> bool {
    true
}

impl ModelInfo {
    pub fn free(&self) -> bool {
        !self.cost.is_empty() && self.cost.iter().all(|c| c.input == 0.0)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(rename = "integrationID", default, skip_serializing_if = "Option::is_none")]
    pub integration_id: Option<String>,
    #[serde(default)]
    pub activation: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CommandInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SkillInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct McpStatus {
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct McpServer {
    pub name: String,
    #[serde(default)]
    pub status: McpStatus,
    #[serde(rename = "integrationID", default, skip_serializing_if = "Option::is_none")]
    pub integration_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReferenceInfo {
    pub name: String,
    #[serde(default)]
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FsEntry {
    pub path: String,
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectRef {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub directory: String,
    #[serde(default)]
    pub canonical: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LocationInfo {
    pub directory: String,
    #[serde(default)]
    pub project: ProjectRef,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ShellInfo {
    pub id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// Envelope for location-scoped responses.
#[derive(Clone, Debug, Deserialize)]
pub struct LocationEnvelope<T> {
    #[serde(default)]
    pub location: Option<LocationRef>,
    pub data: T,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DataEnvelope<T> {
    pub data: T,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Cursor {
    #[serde(default)]
    pub previous: Option<String>,
    #[serde(default)]
    pub next: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Page<T> {
    pub data: Vec<T>,
    #[serde(default)]
    pub cursor: Cursor,
}

/// One event from `GET /api/event`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Event {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub created: i64,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<LocationRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub data: Value,
}

impl Event {
    pub fn str(&self, key: &str) -> Option<&str> {
        self.data.get(key)?.as_str()
    }
    pub fn session_id(&self) -> Option<&str> {
        self.str("sessionID")
    }
    /// `messageIDFromEvent`: `evt_` → `msg_`.
    pub fn message_id(&self) -> String {
        match self.id.strip_prefix("evt_") {
            Some(rest) => format!("msg_{rest}"),
            None => self.id.clone(),
        }
    }
}
