//! Conversion and error-mapping helpers shared by every endpoint.

use oc_server::ApiError;
use oc_storage::projector::ProjectError;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Convert between two serde types with the same JSON shape (core/schema type → generated API
/// type and back). A mismatch is a server bug: like a response that fails its schema upstream it is a
/// `500 UnknownError` ("Server failed to encode its response…") with a logged `ref`.
pub fn conv<T: DeserializeOwned>(value: impl Serialize) -> Result<T, ApiError> {
    let json = serde_json::to_value(value).map_err(|e| ApiError::encode_failed(format!("encode failed: {e}")))?;
    from_json(json)
}

/// Decode a JSON value into an API type (`UnknownError` with a `ref` on mismatch).
pub fn from_json<T: DeserializeOwned>(json: Value) -> Result<T, ApiError> {
    let text = if tracing::enabled!(tracing::Level::DEBUG) { Some(json.to_string()) } else { None };
    serde_json::from_value(json).map_err(|e| {
        ApiError::encode_failed(format!(
            "value does not match the API schema ({e}){}",
            text.map(|t| format!(": {}", truncate(&t, 2000))).unwrap_or_default()
        ))
    })
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// A listed background job as the API's `Job.Info` (`JobList.toItem`).
pub fn job_info(job: oc_core::job::ListedJob) -> oc_server::generated::JobInfo {
    use oc_server::generated::{
        JobImage, JobShell, JobStatus, JobSubagent, JobTime, LitImage, LitShell, LitSubagent,
    };
    use oc_tools::host::{JobRecovery, JobStatus as Status};
    let status = match job.status {
        Status::Running => JobStatus::Running,
        Status::Completed => JobStatus::Completed,
        Status::Error => JobStatus::Error,
        Status::Cancelled => JobStatus::Cancelled,
    };
    let time = JobTime { started: job.started as f64, completed: job.completed.map(|c| c as f64) };
    let (id, title) = (job.id, job.title);
    match job.recovery {
        JobRecovery::Shell { session_id, shell_id, command } => {
            JobShell { kind: LitShell, id, session_id, status, title, time, shell_id, command }.into()
        }
        JobRecovery::Subagent { parent_session_id, child_session_id, agent, description } => JobSubagent {
            kind: LitSubagent,
            id,
            session_id: parent_session_id,
            status,
            title,
            time,
            child_session_id,
            agent,
            description,
        }
        .into(),
        JobRecovery::Image { session_id, path, model } => {
            JobImage { kind: LitImage, id, session_id, status, title, time, path, model }.into()
        }
    }
}

/// Any unexpected failure → defect.
pub fn defect(e: impl std::fmt::Display) -> ApiError {
    ApiError::defect(e.to_string())
}

/// `SessionInbox.LifecycleConflict` raised by the projector (the row was already consumed or the
/// ID collides with a durable record).
pub fn is_lifecycle_conflict(e: &anyhow::Error) -> bool {
    e.chain().any(|c| matches!(c.downcast_ref::<ProjectError>(), Some(ProjectError::LifecycleConflict(_))))
}

/// `SessionProjector.SessionAlreadyProjected`.
pub fn is_already_projected(e: &anyhow::Error) -> bool {
    e.chain().any(|c| matches!(c.downcast_ref::<ProjectError>(), Some(ProjectError::SessionAlreadyProjected)))
}

pub fn now_ms() -> i64 {
    oc_schema::id::now_ms() as i64
}
