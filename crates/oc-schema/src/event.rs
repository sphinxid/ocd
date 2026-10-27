//! Event envelope and durable session events (`schema/src/event.ts`, `schema/src/session-event.ts`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::{AssistantContent, ShellInfo, ShellOutput};
use crate::types::*;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Durable {
    #[serde(rename = "aggregateID")]
    pub aggregate_id: String,
    pub seq: i64,
    pub version: u32,
}

/// Wire/in-memory `Event.Payload`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub id: String,
    pub created: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub durable: Option<Durable>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<LocationRef>,
    pub data: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    Steer,
    Queue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InterruptReason {
    User,
    Shutdown,
    Superseded,
    Inactivity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboxItem {
    #[serde(rename = "type")]
    pub kind: String,
    pub payload: Value,
    pub delivery: Delivery,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstructionEntryValue {
    pub key: String,
    pub value: Option<Value>,
    pub removed: bool,
}


/// Durable session events. Serialized as `{"type": "...", ...payload}`; the stored type
/// string is `<type>.<version>` (see [`SessionEvent::version`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionEvent {
    #[serde(rename = "session.created")]
    Created {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "projectID")]
        project_id: String,
        location: LocationRef,
        #[serde(skip_serializing_if = "Option::is_none")]
        subpath: Option<String>,
        #[serde(rename = "parentID", skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        slug: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<ModelRef>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Metadata>,
        #[serde(skip_serializing_if = "Option::is_none")]
        permissions: Option<Ruleset>,
        version: String,
    },
    #[serde(rename = "session.agent.selected")]
    AgentSelected {
        #[serde(rename = "sessionID")]
        session_id: String,
        agent: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous: Option<String>,
    },
    #[serde(rename = "session.model.selected")]
    ModelSelected {
        #[serde(rename = "sessionID")]
        session_id: String,
        model: ModelRef,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous: Option<ModelRef>,
    },
    #[serde(rename = "session.moved")]
    Moved {
        #[serde(rename = "sessionID")]
        session_id: String,
        location: LocationRef,
        #[serde(rename = "projectID")]
        project_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        subpath: Option<String>,
    },
    #[serde(rename = "session.renamed")]
    Renamed {
        #[serde(rename = "sessionID")]
        session_id: String,
        title: String,
    },
    #[serde(rename = "session.metadata.updated")]
    MetadataUpdated {
        #[serde(rename = "sessionID")]
        session_id: String,
        metadata: Metadata,
    },
    #[serde(rename = "session.permissions")]
    Permissions {
        #[serde(rename = "sessionID")]
        session_id: String,
        permissions: Ruleset,
    },
    #[serde(rename = "session.viewed")]
    Viewed {
        #[serde(rename = "sessionID")]
        session_id: String,
        idle: u64,
    },
    #[serde(rename = "session.archived")]
    Archived {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    #[serde(rename = "session.unarchived")]
    Unarchived {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    #[serde(rename = "session.message.content.updated")]
    MessageContentUpdated {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "messageID")]
        message_id: String,
        content: Vec<AssistantContent>,
    },
    #[serde(rename = "session.usage.recorded")]
    UsageRecorded {
        #[serde(rename = "sessionID")]
        session_id: String,
        source: String,
        cost: f64,
        tokens: TokenUsage,
    },
    #[serde(rename = "session.deleted")]
    Deleted {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    #[serde(rename = "session.forked")]
    Forked {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "parentID")]
        parent_id: String,
        boundary: ForkBoundary,
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<serde_json::Map<String, Value>>,
        #[serde(rename = "instructionEntries", skip_serializing_if = "Option::is_none")]
        instruction_entries: Option<Vec<InstructionEntryValue>>,
    },
    #[serde(rename = "session.inbox.delivered")]
    InboxDelivered {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "inboxID")]
        inbox_id: String,
    },
    #[serde(rename = "session.inbox.enqueued")]
    InboxEnqueued {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "inboxID")]
        inbox_id: String,
        item: InboxItem,
    },
    #[serde(rename = "session.inbox.cancelled")]
    InboxCancelled {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "inboxID")]
        inbox_id: String,
    },
    #[serde(rename = "session.inbox.delivery.changed")]
    InboxDeliveryChanged {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "inboxID")]
        inbox_id: String,
        delivery: Delivery,
    },
    #[serde(rename = "session.execution.started")]
    ExecutionStarted {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    #[serde(rename = "session.execution.succeeded")]
    ExecutionSucceeded {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    #[serde(rename = "session.execution.failed")]
    ExecutionFailed {
        #[serde(rename = "sessionID")]
        session_id: String,
        error: SessionError,
    },
    #[serde(rename = "session.execution.interrupted")]
    ExecutionInterrupted {
        #[serde(rename = "sessionID")]
        session_id: String,
        reason: InterruptReason,
    },
    #[serde(rename = "session.instructions.updated")]
    InstructionsUpdated {
        #[serde(rename = "sessionID")]
        session_id: String,
        delta: serde_json::Map<String, Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    #[serde(rename = "session.synthetic")]
    Synthetic {
        #[serde(rename = "sessionID")]
        session_id: String,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Metadata>,
    },
    #[serde(rename = "session.skill.activated")]
    SkillActivated {
        #[serde(rename = "sessionID")]
        session_id: String,
        id: String,
        name: String,
        text: String,
    },
    #[serde(rename = "session.shell.started")]
    ShellStarted {
        #[serde(rename = "sessionID")]
        session_id: String,
        shell: ShellInfo,
    },
    #[serde(rename = "session.shell.ended")]
    ShellEnded {
        #[serde(rename = "sessionID")]
        session_id: String,
        shell: ShellInfo,
        output: ShellOutput,
    },
    #[serde(rename = "session.step.started")]
    StepStarted {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        agent: String,
        model: ModelRef,
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot: Option<String>,
        started: u64,
    },
    #[serde(rename = "session.step.streamed")]
    StepStreamed {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
    },
    #[serde(rename = "session.step.ended")]
    StepEnded {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        finish: FinishReason,
        #[serde(rename = "rawFinish", skip_serializing_if = "Option::is_none")]
        raw_finish: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        native: Option<Metadata>,
        cost: f64,
        tokens: TokenUsage,
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        files: Option<Vec<String>>,
    },
    #[serde(rename = "session.step.failed")]
    StepFailed {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        error: SessionError,
        #[serde(skip_serializing_if = "Option::is_none")]
        finish: Option<FinishReason>,
        #[serde(rename = "rawFinish", skip_serializing_if = "Option::is_none")]
        raw_finish: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        native: Option<Metadata>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens: Option<TokenUsage>,
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        files: Option<Vec<String>>,
    },
    #[serde(rename = "session.text.started")]
    TextStarted {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        ordinal: u32,
    },
    #[serde(rename = "session.text.ended")]
    TextEnded {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        ordinal: u32,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        native: Option<Metadata>,
    },
    #[serde(rename = "session.reasoning.started")]
    ReasoningStarted {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        ordinal: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        native: Option<Metadata>,
    },
    #[serde(rename = "session.reasoning.ended")]
    ReasoningEnded {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        ordinal: u32,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        native: Option<Metadata>,
    },
    #[serde(rename = "session.tool.input.started")]
    ToolInputStarted {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        id: String,
        name: String,
    },
    #[serde(rename = "session.tool.input.ended")]
    ToolInputEnded {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        id: String,
        text: String,
    },
    #[serde(rename = "session.tool.called")]
    ToolCalled {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        id: String,
        input: Metadata,
        executed: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        native: Option<Metadata>,
    },
    #[serde(rename = "session.tool.success")]
    ToolSuccess {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        id: String,
        content: Vec<ToolContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Metadata>,
        executed: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        native: Option<Metadata>,
    },
    #[serde(rename = "session.tool.failed")]
    ToolFailed {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        id: String,
        error: SessionError,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ToolContent>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Metadata>,
        executed: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        native: Option<Metadata>,
    },
    #[serde(rename = "session.retry.scheduled")]
    RetryScheduled {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        attempt: u32,
        at: u64,
        error: SessionError,
    },
    #[serde(rename = "session.compaction.started")]
    CompactionStarted {
        #[serde(rename = "sessionID")]
        session_id: String,
        reason: crate::message::CompactionReason,
        #[serde(rename = "inputID", skip_serializing_if = "Option::is_none")]
        input_id: Option<String>,
    },
    #[serde(rename = "session.compaction.ended")]
    CompactionEnded {
        #[serde(rename = "sessionID")]
        session_id: String,
        reason: crate::message::CompactionReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<ModelRef>,
        #[serde(skip_serializing_if = "Option::is_none")]
        native: Option<Metadata>,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens: Option<TokenUsage>,
    },
    #[serde(rename = "session.compaction.failed")]
    CompactionFailed {
        #[serde(rename = "sessionID")]
        session_id: String,
        reason: crate::message::CompactionReason,
        error: SessionError,
        #[serde(rename = "inputID", skip_serializing_if = "Option::is_none")]
        input_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens: Option<TokenUsage>,
    },
    #[serde(rename = "session.revert.staged")]
    RevertStaged {
        #[serde(rename = "sessionID")]
        session_id: String,
        revert: Revert,
    },
    #[serde(rename = "session.revert.cleared")]
    RevertCleared {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    #[serde(rename = "session.revert.committed")]
    RevertCommitted {
        #[serde(rename = "sessionID")]
        session_id: String,
        to: String,
    },
}

impl SessionEvent {
    pub fn session_id(&self) -> &str {
        session_id_of(self)
    }

    pub fn type_name(&self) -> String {
        let v = serde_json::to_value(self).expect("event serializes");
        v["type"].as_str().unwrap_or_default().to_string()
    }

    /// Schema version of the event definition.
    pub fn version(&self) -> u32 {
        match self {
            SessionEvent::Deleted { .. }
            | SessionEvent::Forked { .. }
            | SessionEvent::InstructionsUpdated { .. }
            | SessionEvent::ToolSuccess { .. }
            | SessionEvent::ToolFailed { .. } => 2,
            _ => 1,
        }
    }

    /// Payload without the `type` tag.
    pub fn data(&self) -> Value {
        let mut v = serde_json::to_value(self).expect("event serializes");
        if let Value::Object(m) = &mut v {
            m.shift_remove("type");
        }
        v
    }

    /// Decode a payload by event type. Stored events decode through here, so payloads written in older
    /// shapes are upgraded in one place (`bus.ts` `decodeStoredData`).
    pub fn from_parts(kind: &str, data: Value) -> serde_json::Result<SessionEvent> {
        let mut v = crate::legacy::event(kind, data);
        if let Value::Object(m) = &mut v {
            m.insert("type".into(), Value::String(kind.into()));
        }
        serde_json::from_value(v)
    }
}

fn session_id_of(e: &SessionEvent) -> &str {
    use SessionEvent::*;
    match e {
        Created { session_id, .. }
        | AgentSelected { session_id, .. }
        | ModelSelected { session_id, .. }
        | Moved { session_id, .. }
        | Renamed { session_id, .. }
        | MetadataUpdated { session_id, .. }
        | Permissions { session_id, .. }
        | Viewed { session_id, .. }
        | Archived { session_id }
        | Unarchived { session_id }
        | MessageContentUpdated { session_id, .. }
        | UsageRecorded { session_id, .. }
        | Deleted { session_id }
        | Forked { session_id, .. }
        | InboxDelivered { session_id, .. }
        | InboxEnqueued { session_id, .. }
        | InboxCancelled { session_id, .. }
        | InboxDeliveryChanged { session_id, .. }
        | ExecutionStarted { session_id }
        | ExecutionSucceeded { session_id }
        | ExecutionFailed { session_id, .. }
        | ExecutionInterrupted { session_id, .. }
        | InstructionsUpdated { session_id, .. }
        | Synthetic { session_id, .. }
        | SkillActivated { session_id, .. }
        | ShellStarted { session_id, .. }
        | ShellEnded { session_id, .. }
        | StepStarted { session_id, .. }
        | StepStreamed { session_id, .. }
        | StepEnded { session_id, .. }
        | StepFailed { session_id, .. }
        | TextStarted { session_id, .. }
        | TextEnded { session_id, .. }
        | ReasoningStarted { session_id, .. }
        | ReasoningEnded { session_id, .. }
        | ToolInputStarted { session_id, .. }
        | ToolInputEnded { session_id, .. }
        | ToolCalled { session_id, .. }
        | ToolSuccess { session_id, .. }
        | ToolFailed { session_id, .. }
        | RetryScheduled { session_id, .. }
        | CompactionStarted { session_id, .. }
        | CompactionEnded { session_id, .. }
        | CompactionFailed { session_id, .. }
        | RevertStaged { session_id, .. }
        | RevertCleared { session_id }
        | RevertCommitted { session_id, .. } => session_id,
    }
}
