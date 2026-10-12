//! Conversion and error-mapping helpers shared by every endpoint.

use oc_server::ApiError;
use oc_storage::projector::ProjectError;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Convert between two serde types with the same JSON shape (core/schema type → generated API
/// type and back). A mismatch is a server bug, so it becomes a defect (500, empty body).
pub fn conv<T: DeserializeOwned>(value: impl Serialize) -> Result<T, ApiError> {
    let json = serde_json::to_value(value).map_err(|e| ApiError::defect(format!("encode failed: {e}")))?;
    from_json(json)
}

/// Decode a JSON value into an API type (defect on mismatch).
pub fn from_json<T: DeserializeOwned>(json: Value) -> Result<T, ApiError> {
    let text = if tracing::enabled!(tracing::Level::DEBUG) { Some(json.to_string()) } else { None };
    serde_json::from_value(json).map_err(|e| {
        ApiError::defect(format!(
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
