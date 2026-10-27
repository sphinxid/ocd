//! API errors: the `_tag` error table (spec 06 §6), schema-decode errors and defects.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use crate::generated::ErrorEncoded;

/// Error returned by [`Backend`](crate::Backend) methods and by request decoding.
///
/// * [`ApiError::Typed`]: a declared error (`{"_tag": ..., ...}`) served as JSON with its status.
/// * [`ApiError::Defect`]: an unexpected failure — `500` with an **empty** body (the message is
///   only logged), like an Effect defect.
/// * [`ApiError::Empty`]: a bare status with an empty body (e.g. the PTY pre-upgrade `403`).
/// * [`ApiError::Text`]: a `text/plain` body (e.g. `415 Unsupported content-type`).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ApiError {
    #[error("{status} {body}")]
    Typed { status: StatusCode, body: Value },
    #[error("defect: {0}")]
    Defect(String),
    #[error("{0}")]
    Empty(StatusCode),
    #[error("{status} {body}")]
    Text { status: StatusCode, body: String },
}

fn tagged(status: u16, tag: &str, fields: Value) -> ApiError {
    let mut body = Map::new();
    body.insert("_tag".into(), Value::String(tag.into()));
    if let Value::Object(map) = fields {
        for (k, v) in map {
            if !v.is_null() {
                body.insert(k, v);
            }
        }
    }
    ApiError::Typed { status: StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), body: Value::Object(body) }
}

impl ApiError {
    /// Wrap a generated `*ErrorEncoded` value.
    pub fn typed<E: ErrorEncoded>(error: E) -> Self {
        let body = serde_json::to_value(&error).unwrap_or_else(|_| json!({ "_tag": E::TAG }));
        ApiError::Typed {
            status: StatusCode::from_u16(E::STATUS).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            body,
        }
    }

    /// A defect: `500` with an empty body; the message is logged.
    pub fn defect(message: impl Into<String>) -> Self {
        ApiError::Defect(message.into())
    }

    /// Default body of unimplemented [`Backend`](crate::Backend) methods.
    pub fn not_implemented(operation: &str) -> Self {
        ApiError::Defect(format!("operation {operation} is not implemented by the backend"))
    }

    /// `_tag` of a typed error.
    pub fn tag(&self) -> Option<&str> {
        match self {
            ApiError::Typed { body, .. } => body.get("_tag").and_then(Value::as_str),
            _ => None,
        }
    }

    /// HTTP status of this error.
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::Typed { status, .. } | ApiError::Text { status, .. } => *status,
            ApiError::Defect(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::Empty(status) => *status,
        }
    }

    fn set(mut self, key: &str, value: Value) -> Self {
        if let ApiError::Typed { body: Value::Object(map), .. } = &mut self {
            map.insert(key.into(), value);
        }
        self
    }

    /// Set `kind` (InvalidRequestError).
    pub fn with_kind(self, kind: impl Into<String>) -> Self {
        self.set("kind", Value::String(kind.into()))
    }

    /// Set `field` (InvalidRequestError).
    pub fn with_field(self, field: impl Into<String>) -> Self {
        self.set("field", Value::String(field.into()))
    }

    // ---- 400 ----------------------------------------------------------------------------

    /// `400 InvalidRequestError{message}`.
    pub fn invalid_request(message: impl Into<String>) -> Self {
        tagged(400, "InvalidRequestError", json!({ "message": message.into() }))
    }

    /// `400 InvalidRequestError{message, kind}` — a schema decode failure. The message is
    /// truncated to 1024 chars like upstream.
    pub fn schema(kind: &str, reason: impl Into<String>) -> Self {
        let reason = truncate_reason(reason.into());
        tagged(400, "InvalidRequestError", json!({ "message": reason, "kind": kind }))
    }

    /// `400 InvalidCursorError{message}`.
    pub fn invalid_cursor(message: impl Into<String>) -> Self {
        tagged(400, "InvalidCursorError", json!({ "message": message.into() }))
    }

    /// `400 RpcError{type, message, data?}`.
    pub fn rpc(r#type: impl Into<String>, message: impl Into<String>, data: Option<Value>) -> Self {
        let mut e = tagged(400, "RpcError", json!({ "type": r#type.into(), "message": message.into() }));
        if let Some(data) = data {
            e = e.set("data", data);
        }
        e
    }

    /// `500 RpcInternalError{type, message}` (`type` is `rpc.internal` or `rpc.invalid_output`).
    pub fn rpc_internal(r#type: impl Into<String>, message: impl Into<String>) -> Self {
        tagged(500, "RpcInternalError", json!({ "type": r#type.into(), "message": message.into() }))
    }

    /// `400 FormInvalidAnswerError{id, message}`.
    pub fn form_invalid_answer(id: impl Into<String>, message: impl Into<String>) -> Self {
        tagged(400, "FormInvalidAnswerError", json!({ "id": id.into(), "message": message.into() }))
    }

    /// `400 WorktreeError{name:"WorktreeError", data:{message, forceRequired?}}`.
    pub fn worktree(message: impl Into<String>, force_required: Option<bool>) -> Self {
        let mut data = Map::new();
        data.insert("message".into(), Value::String(message.into()));
        // Upstream always sets the key (`forceRequired: ... : undefined`), which encodes as null.
        data.insert("forceRequired".into(), force_required.map(Value::Bool).unwrap_or(Value::Null));
        tagged(400, "WorktreeError", json!({ "name": "WorktreeError", "data": Value::Object(data) }))
    }

    // ---- 401 / 403 ----------------------------------------------------------------------

    /// `401 UnauthorizedError{message}`.
    pub fn unauthorized(message: impl Into<String>) -> Self {
        tagged(401, "UnauthorizedError", json!({ "message": message.into() }))
    }

    /// `403 ForbiddenError{message}`.
    pub fn forbidden(message: impl Into<String>) -> Self {
        tagged(403, "ForbiddenError", json!({ "message": message.into() }))
    }

    // ---- 404 ----------------------------------------------------------------------------

    /// `404 LocationNotFoundError` — `Location not found: <d>`.
    pub fn location_not_found(directory: impl Into<String>) -> Self {
        let d = directory.into();
        tagged(
            404,
            "LocationNotFoundError",
            json!({ "location": { "directory": d }, "message": format!("Location not found: {d}") }),
        )
    }

    /// `404 SessionNotFoundError` — `Session not found: <id>`.
    pub fn session_not_found(session_id: impl Into<String>) -> Self {
        let id = session_id.into();
        tagged(404, "SessionNotFoundError", json!({ "sessionID": id, "message": format!("Session not found: {id}") }))
    }

    /// `404 MessageNotFoundError` — `Message not found: <messageID>`.
    pub fn message_not_found(session_id: impl Into<String>, message_id: impl Into<String>) -> Self {
        let mid = message_id.into();
        tagged(
            404,
            "MessageNotFoundError",
            json!({ "sessionID": session_id.into(), "messageID": mid, "message": format!("Message not found: {mid}") }),
        )
    }

    /// `404 AgentNotFoundError` — `Agent not found: <id>`.
    pub fn agent_not_found(agent_id: impl Into<String>) -> Self {
        let id = agent_id.into();
        tagged(404, "AgentNotFoundError", json!({ "agentID": id, "message": format!("Agent not found: {id}") }))
    }

    /// `404 ProviderNotFoundError` — `Provider not found: <id>`.
    pub fn provider_not_found(provider_id: impl Into<String>) -> Self {
        let id = provider_id.into();
        tagged(404, "ProviderNotFoundError", json!({ "providerID": id, "message": format!("Provider not found: {id}") }))
    }

    /// `404 IntegrationNotFoundError` — `Integration not found: <id>`.
    pub fn integration_not_found(integration_id: impl Into<String>) -> Self {
        let id = integration_id.into();
        tagged(
            404,
            "IntegrationNotFoundError",
            json!({ "integrationID": id, "message": format!("Integration not found: {id}") }),
        )
    }

    /// `404 IntegrationAttemptNotFoundError` with an explicit message (`OAuth attempt not
    /// found: <id>` / `Command attempt not found: <id>`).
    pub fn integration_attempt_not_found(
        integration_id: impl Into<String>,
        attempt_id: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        tagged(
            404,
            "IntegrationAttemptNotFoundError",
            json!({ "integrationID": integration_id.into(), "attemptID": attempt_id.into(), "message": message.into() }),
        )
    }

    /// `404 IntegrationMethodNotFoundError` — `Integration method not found: <id>`.
    pub fn integration_method_not_found(integration_id: impl Into<String>, method_id: impl Into<String>) -> Self {
        let m = method_id.into();
        tagged(
            404,
            "IntegrationMethodNotFoundError",
            json!({ "integrationID": integration_id.into(), "methodID": m, "message": format!("Integration method not found: {m}") }),
        )
    }

    /// `404 ProjectNotFoundError` — `Project not found: <id>`.
    pub fn project_not_found(project_id: impl Into<String>) -> Self {
        let id = project_id.into();
        tagged(404, "ProjectNotFoundError", json!({ "projectID": id, "message": format!("Project not found: {id}") }))
    }

    /// `404 FileNotFoundError` with an explicit message.
    pub fn file_not_found_message(path: impl Into<String>, message: impl Into<String>) -> Self {
        tagged(404, "FileNotFoundError", json!({ "path": path.into(), "message": message.into() }))
    }

    /// `404 FileNotFoundError` — `File not found: <p>`.
    pub fn file_not_found(path: impl Into<String>) -> Self {
        let p = path.into();
        let m = format!("File not found: {p}");
        Self::file_not_found_message(p, m)
    }

    /// `404 FileNotFoundError` — `Directory not found: <p>` (fs.list).
    pub fn directory_not_found(path: impl Into<String>) -> Self {
        let p = path.into();
        let m = format!("Directory not found: {p}");
        Self::file_not_found_message(p, m)
    }

    /// `404 SkillNotFoundError` — `Skill not found: <id>`.
    pub fn skill_not_found(skill: impl Into<String>) -> Self {
        let s = skill.into();
        tagged(404, "SkillNotFoundError", json!({ "skill": s, "message": format!("Skill not found: {s}") }))
    }

    /// `404 McpServerNotFoundError{server, message}`.
    pub fn mcp_server_not_found(server: impl Into<String>, message: impl Into<String>) -> Self {
        tagged(404, "McpServerNotFoundError", json!({ "server": server.into(), "message": message.into() }))
    }

    /// `404 CommandNotFoundError{command, message}`.
    pub fn command_not_found(command: impl Into<String>, message: impl Into<String>) -> Self {
        tagged(404, "CommandNotFoundError", json!({ "command": command.into(), "message": message.into() }))
    }

    /// `404 PermissionNotFoundError` — `Permission request not found: <id>`.
    pub fn permission_not_found(request_id: impl Into<String>) -> Self {
        let id = request_id.into();
        tagged(
            404,
            "PermissionNotFoundError",
            json!({ "requestID": id, "message": format!("Permission request not found: {id}") }),
        )
    }

    /// `404 PermissionSavedNotFoundError` — `Saved permission not found: <id>`.
    pub fn permission_saved_not_found(id: impl Into<String>) -> Self {
        let id = id.into();
        tagged(404, "PermissionSavedNotFoundError", json!({ "id": id, "message": format!("Saved permission not found: {id}") }))
    }

    /// `404 FormNotFoundError` — `Form not found: <id>`.
    pub fn form_not_found(id: impl Into<String>) -> Self {
        let id = id.into();
        tagged(404, "FormNotFoundError", json!({ "id": id, "message": format!("Form not found: {id}") }))
    }

    /// `404 PtyNotFoundError` — `PTY session not found: <id>`.
    pub fn pty_not_found(pty_id: impl Into<String>) -> Self {
        let id = pty_id.into();
        tagged(404, "PtyNotFoundError", json!({ "ptyID": id, "message": format!("PTY session not found: {id}") }))
    }

    /// `404 ShellNotFoundError` — `Shell command not found: <id>`.
    pub fn shell_not_found(id: impl Into<String>) -> Self {
        let id = id.into();
        tagged(404, "ShellNotFoundError", json!({ "id": id, "message": format!("Shell command not found: {id}") }))
    }

    // ---- 409 / 413 ----------------------------------------------------------------------

    /// `409 ConflictError{message, resource?}`.
    pub fn conflict(message: impl Into<String>, resource: Option<String>) -> Self {
        tagged(409, "ConflictError", json!({ "message": message.into(), "resource": resource }))
    }

    /// `409 SessionBusyError` — `Session is busy: <id>`.
    pub fn session_busy(session_id: impl Into<String>) -> Self {
        let id = session_id.into();
        tagged(409, "SessionBusyError", json!({ "sessionID": id, "message": format!("Session is busy: {id}") }))
    }

    /// `409 FormAlreadySettledError{id, message}`.
    pub fn form_already_settled(id: impl Into<String>, message: impl Into<String>) -> Self {
        tagged(409, "FormAlreadySettledError", json!({ "id": id.into(), "message": message.into() }))
    }

    /// `413 InstructionEntryValueTooLargeError{actualBytes, maxBytes, message}`.
    pub fn instruction_entry_too_large(actual_bytes: i64, max_bytes: i64, message: impl Into<String>) -> Self {
        tagged(
            413,
            "InstructionEntryValueTooLargeError",
            json!({ "actualBytes": actual_bytes, "maxBytes": max_bytes, "message": message.into() }),
        )
    }

    // ---- 5xx ----------------------------------------------------------------------------

    /// `500 UnknownError{message:"Unexpected server error. Check server logs for details.", ref}`
    /// with `ref = "err_" + 8 chars of a random UUID` (logged together with `cause`).
    pub fn unknown(cause: impl std::fmt::Display) -> Self {
        let r = format!("err_{}", &uuid::Uuid::new_v4().to_string()[..8]);
        tracing::error!(r#ref = %r, cause = %cause, "unexpected server error");
        tagged(
            500,
            "UnknownError",
            json!({ "message": "Unexpected server error. Check server logs for details.", "ref": r }),
        )
    }

    /// `500 CommandExecutionError{command, message}`.
    pub fn command_execution(command: impl Into<String>, message: impl Into<String>) -> Self {
        tagged(500, "CommandExecutionError", json!({ "command": command.into(), "message": message.into() }))
    }

    /// `501 VcsInitNotSupportedError` — `VCS provider does not support initialization`.
    pub fn vcs_init_not_supported(provider_id: impl Into<String>) -> Self {
        tagged(
            501,
            "VcsInitNotSupportedError",
            json!({ "providerID": provider_id.into(), "message": "VCS provider does not support initialization" }),
        )
    }

    /// `503 ServiceUnavailableError{message, service?}`.
    pub fn service_unavailable(message: impl Into<String>, service: Option<String>) -> Self {
        tagged(503, "ServiceUnavailableError", json!({ "message": message.into(), "service": service }))
    }
}

/// `rpc.call`: any defect becomes `RpcInternalError{type:"rpc.internal", message:"RPC call failed"}`.
pub(crate) fn rpc_defect(error: ApiError) -> ApiError {
    match error {
        ApiError::Defect(message) => {
            tracing::error!(defect = %message, "rpc call failed");
            ApiError::rpc_internal("rpc.internal", "RPC call failed")
        }
        other => other,
    }
}

const REASON_LIMIT: usize = 1024;

fn truncate_reason(reason: String) -> String {
    let count = reason.chars().count();
    if count <= REASON_LIMIT {
        return reason;
    }
    let head: String = reason.chars().take(REASON_LIMIT).collect();
    format!("{head}... ({} more chars)", count - REASON_LIMIT)
}

/// Build a JSON response with `content-type: application/json`.
pub(crate) fn json_response(status: StatusCode, body: &Value) -> Response {
    let bytes = crate::support::to_js_vec(body).unwrap_or_default();
    let mut res = (status, bytes).into_response();
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    res
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::Typed { status, body } => json_response(status, &body),
            ApiError::Defect(message) => {
                tracing::error!(defect = %message, "request failed with a defect");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
            ApiError::Empty(status) => status.into_response(),
            ApiError::Text { status, body } => {
                let mut res = (status, body).into_response();
                res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
                res
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_order_is_tag_first() {
        let e = ApiError::session_not_found("ses_1");
        let ApiError::Typed { body, status } = e else { panic!() };
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"_tag":"SessionNotFoundError","sessionID":"ses_1","message":"Session not found: ses_1"}"#
        );
    }

    #[test]
    fn optional_fields_are_omitted() {
        let e = ApiError::conflict("x", None);
        let ApiError::Typed { body, .. } = e else { panic!() };
        assert_eq!(serde_json::to_string(&body).unwrap(), r#"{"_tag":"ConflictError","message":"x"}"#);
        let e = ApiError::invalid_request("bad").with_field("sessionID");
        assert_eq!(e.tag(), Some("InvalidRequestError"));
        let ApiError::Typed { body, .. } = e else { panic!() };
        assert_eq!(body["field"], "sessionID");
    }

    #[test]
    fn reason_truncation() {
        let long = "x".repeat(1030);
        let ApiError::Typed { body, .. } = ApiError::schema("Query", long) else { panic!() };
        let m = body["message"].as_str().unwrap();
        assert!(m.ends_with("... (6 more chars)"));
        assert_eq!(body["kind"], "Query");
    }

    #[test]
    fn worktree_shape() {
        let ApiError::Typed { body, .. } = ApiError::worktree("nope", Some(true)) else { panic!() };
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"_tag":"WorktreeError","name":"WorktreeError","data":{"message":"nope","forceRequired":true}}"#
        );
        let ApiError::Typed { body, .. } = ApiError::worktree("nope", None) else { panic!() };
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"_tag":"WorktreeError","name":"WorktreeError","data":{"message":"nope","forceRequired":null}}"#
        );
    }

    #[test]
    fn typed_from_generated() {
        let e = ApiError::typed(crate::generated::ForbiddenErrorEncoded { tag: Default::default(), message: "m".into() });
        assert_eq!(e.status(), StatusCode::FORBIDDEN);
        assert_eq!(e.tag(), Some("ForbiddenError"));
    }
}
