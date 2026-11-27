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
        JobCodeMode, JobImage, JobShell, JobStatus, JobSubagent, JobTime, JobVideo, LitCodemode, LitImage, LitShell, LitSubagent, LitVideo,
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
        JobRecovery::Subagent { parent_session_id, child_session_id, agent, description, .. } => JobSubagent {
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
        JobRecovery::Codemode { session_id, .. } => JobCodeMode { kind: LitCodemode, id, session_id, status, title, time }.into(),
        JobRecovery::Image { session_id, path, model } => {
            JobImage { kind: LitImage, id, session_id, status, title, time, path, model }.into()
        }
        JobRecovery::Video { session_id, path, .. } => JobVideo { kind: LitVideo, id, session_id, status, title, time, path }.into(),
    }
}

/// Any unexpected failure → defect: `500 UnknownError` with a ref, or with the storage reason when the failure is a
/// storage failure (`StorageFailure.message`: a full disk, a locked, read-only or damaged database).
pub fn defect<E: std::fmt::Display + 'static>(e: E) -> ApiError {
    match storage_failure(&e) {
        Some(reason) => ApiError::storage(reason, format!("{e:#}")),
        None => ApiError::defect(e.to_string()),
    }
}

/// [`defect`] of an `anyhow` error, logging its whole chain.
pub fn defect_chain(e: anyhow::Error) -> ApiError {
    match oc_storage::failure::message(&e) {
        Some(reason) => ApiError::storage(reason, format!("{e:#}")),
        None => ApiError::defect(format!("{e:#}")),
    }
}

/// The storage reason of an error of any type that can carry a SQLite failure.
pub fn storage_failure<E: 'static>(e: &E) -> Option<&'static str> {
    let any = e as &dyn std::any::Any;
    if let Some(e) = any.downcast_ref::<anyhow::Error>() {
        return oc_storage::failure::message(e);
    }
    if let Some(e) = any.downcast_ref::<rusqlite::Error>() {
        return oc_storage::failure::from_sqlite(e);
    }
    None
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
