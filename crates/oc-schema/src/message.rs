//! Session messages (`schema/src/session-message.ts`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::*;

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Created {
    pub created: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreviousLocation {
    pub location: LocationRef,
    #[serde(rename = "projectID", skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellStatus {
    Running,
    Exited,
    Timeout,
    Killed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellOutput {
    pub output: String,
    pub cursor: u64,
    pub size: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellTime {
    pub started: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<u64>,
}

/// `Shell.Info`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellInfo {
    pub id: String,
    pub status: ShellStatus,
    pub command: String,
    pub cwd: String,
    pub shell: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    pub metadata: Metadata,
    pub time: ShellTime,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CompletedTime {
    pub created: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct AssistantTime {
    pub created: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streamed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolTime {
    pub created: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ran: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum ToolState {
    Streaming {
        input: String,
    },
    Running {
        input: Metadata,
        metadata: Metadata,
    },
    Completed {
        input: Metadata,
        content: Vec<ToolContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Metadata>,
    },
    Error {
        input: Metadata,
        error: SessionError,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ToolContent>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Metadata>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AssistantContent {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        native: Option<Metadata>,
    },
    Reasoning {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        native: Option<Metadata>,
        #[serde(skip_serializing_if = "Option::is_none")]
        time: Option<CompletedTime>,
    },
    Tool {
        id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        executed: Option<bool>,
        /// Provider-owned replay data. A tool's result data is merged into its call data.
        #[serde(skip_serializing_if = "Option::is_none")]
        native: Option<Metadata>,
        state: ToolState,
        time: ToolTime,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssistantSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantRetry {
    pub attempt: u32,
    pub at: u64,
    pub error: SessionError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionReason {
    Auto,
    Manual,
}

/// The stored/API message. Serialized with `type` tag; storage strips `id` and `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Message {
    #[serde(rename = "agent-switched")]
    AgentSwitched {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Metadata>,
        time: Created,
        agent: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous: Option<String>,
    },
    #[serde(rename = "model-switched")]
    ModelSwitched {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Metadata>,
        time: Created,
        model: ModelRef,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous: Option<ModelRef>,
    },
    #[serde(rename = "location-switched")]
    LocationSwitched {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Metadata>,
        time: Created,
        location: LocationRef,
        #[serde(rename = "projectID", skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        subpath: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous: Option<PreviousLocation>,
    },
    #[serde(rename = "user")]
    User {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Metadata>,
        time: Created,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        files: Option<Vec<FileAttachment>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        agents: Option<Vec<AgentAttachment>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        skills: Option<Vec<SkillAttachment>>,
    },
    #[serde(rename = "synthetic")]
    Synthetic {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Metadata>,
        time: Created,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    #[serde(rename = "system")]
    System {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Metadata>,
        time: Created,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    #[serde(rename = "skill")]
    Skill {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Metadata>,
        time: Created,
        skill: String,
        name: String,
        text: String,
    },
    #[serde(rename = "shell")]
    Shell {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Metadata>,
        time: CompletedTime,
        #[serde(rename = "shellID")]
        shell_id: String,
        command: String,
        status: ShellStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<ShellOutput>,
    },
    #[serde(rename = "assistant")]
    Assistant(Box<Assistant>),
    #[serde(rename = "compaction")]
    Compaction(Box<Compaction>),
    #[serde(rename = "idle")]
    Idle {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Metadata>,
        time: Created,
        outcome: Outcome,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<SessionError>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assistant {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    pub time: AssistantTime,
    pub agent: String,
    pub model: ModelRef,
    pub content: Vec<AssistantContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<AssistantSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish: Option<FinishReason>,
    #[serde(rename = "rawFinish", skip_serializing_if = "Option::is_none")]
    pub raw_finish: Option<String>,
    /// Step-level native data, such as a provider's reason for blocking a response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<Metadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<SessionError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<AssistantRetry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Compaction {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    pub time: Created,
    #[serde(flatten)]
    pub status: CompactionStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum CompactionStatus {
    Running {
        reason: CompactionReason,
        summary: String,
    },
    Completed {
        reason: CompactionReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<ModelRef>,
        /// A provider compaction stores its replacement window in `native.messages`, replayed instead of
        /// `summary`. A summary references the messages it keeps verbatim with `metadata.keepID`.
        #[serde(skip_serializing_if = "Option::is_none")]
        native: Option<Metadata>,
        summary: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens: Option<TokenUsage>,
    },
    Failed {
        reason: CompactionReason,
        error: SessionError,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens: Option<TokenUsage>,
    },
}

impl Message {
    pub fn id(&self) -> &str {
        match self {
            Message::AgentSwitched { id, .. }
            | Message::ModelSwitched { id, .. }
            | Message::LocationSwitched { id, .. }
            | Message::User { id, .. }
            | Message::Synthetic { id, .. }
            | Message::System { id, .. }
            | Message::Skill { id, .. }
            | Message::Shell { id, .. }
            | Message::Idle { id, .. } => id,
            Message::Assistant(a) => &a.id,
            Message::Compaction(c) => &c.id,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Message::AgentSwitched { .. } => "agent-switched",
            Message::ModelSwitched { .. } => "model-switched",
            Message::LocationSwitched { .. } => "location-switched",
            Message::User { .. } => "user",
            Message::Synthetic { .. } => "synthetic",
            Message::System { .. } => "system",
            Message::Skill { .. } => "skill",
            Message::Shell { .. } => "shell",
            Message::Assistant(_) => "assistant",
            Message::Compaction(_) => "compaction",
            Message::Idle { .. } => "idle",
        }
    }

    pub fn created(&self) -> u64 {
        match self {
            Message::AgentSwitched { time, .. }
            | Message::ModelSwitched { time, .. }
            | Message::LocationSwitched { time, .. }
            | Message::User { time, .. }
            | Message::Synthetic { time, .. }
            | Message::System { time, .. }
            | Message::Skill { time, .. }
            | Message::Idle { time, .. } => time.created,
            Message::Shell { time, .. } => time.created,
            Message::Assistant(a) => a.time.created,
            Message::Compaction(c) => c.time.created,
        }
    }

    /// Storage form: encoded message minus `id` and `type`.
    pub fn to_row_data(&self) -> String {
        let mut v = serde_json::to_value(self).expect("message serializes");
        if let Value::Object(m) = &mut v {
            m.shift_remove("id");
            m.shift_remove("type");
        }
        v.to_string()
    }

    /// Reconstruct from a `session_message` row (`{...data, id, type}`). Every stored message row decodes
    /// through here, so rows written in older shapes are upgraded in one place (`history.ts` `rowInput`).
    pub fn from_row(id: &str, kind: &str, data: &str) -> serde_json::Result<Message> {
        serde_json::from_value(Value::Object(row_input(id, kind, data)?))
    }
}

/// `{...SessionLegacy.message(type, data), id, type}` for a `session_message` row.
pub fn row_input(id: &str, kind: &str, data: &str) -> serde_json::Result<serde_json::Map<String, Value>> {
    let data: serde_json::Map<String, Value> = serde_json::from_str(data)?;
    let mut m = crate::legacy::message(kind, data);
    m.insert("id".into(), Value::String(id.into()));
    m.insert("type".into(), Value::String(kind.into()));
    Ok(m)
}
