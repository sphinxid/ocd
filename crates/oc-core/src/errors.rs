//! `toSessionError` for provider errors (`core/session/to-session-error.ts`).

use oc_ai::{AIError, ErrorKind};
use oc_schema::types::{ErrorResponse, SessionError};

const TOKEN_SHARING: &[(&str, &str)] = &[
    ("subscription_sharing_user_not_eligible", "ChatGPT token sharing isn't available for this account. Connect with an API key or choose another provider."),
    ("subscription_sharing_usage_limit_exceeded", "ChatGPT usage limit reached. Check ChatGPT Settings → Usage for details."),
    ("subscription_sharing_usage_unavailable", "ChatGPT usage can't be checked right now. Try again later."),
    ("subscription_sharing_unsupported_capability", "This request uses a feature ChatGPT token sharing doesn't support. Remove the unsupported feature and try again."),
    ("subscription_sharing_route_not_supported", "ChatGPT token sharing doesn't support this API route. Check the configured endpoint and HTTP method."),
    ("subscription_sharing_invalid_user", "This ChatGPT connection is no longer valid. Reconnect to ChatGPT."),
    ("subscription_sharing_user_unavailable", "Your ChatGPT account is temporarily unavailable. Try again later."),
    ("chatpass_v2_scope_not_authorized", "This ChatGPT connection isn't authorized for this request. Reconnect to ChatGPT or choose another connection."),
    ("chatpass_v2_invalid_authorization_context", "This ChatGPT connection isn't authorized for this request. Reconnect to ChatGPT or choose another connection."),
];

pub fn from_ai(e: &AIError) -> SessionError {
    let kind = match &e.kind {
        ErrorKind::RateLimit { .. } => "provider.rate-limit",
        ErrorKind::Authentication => "provider.auth",
        ErrorKind::QuotaExceeded { .. } => "provider.quota",
        ErrorKind::ContentPolicy => "provider.content-filter",
        ErrorKind::Transport { .. } => "provider.transport",
        ErrorKind::ProviderInternal { .. } => "provider.internal",
        ErrorKind::InvalidProviderOutput { .. } => "provider.invalid-output",
        ErrorKind::InvalidRequest { .. } => "provider.invalid-request",
        ErrorKind::UnsupportedOperation { .. } => "provider.unsupported-operation",
        ErrorKind::NoRoute { .. } => "provider.no-route",
        ErrorKind::UnknownProvider => "provider.unknown",
        ErrorKind::Timeout { .. } => "provider.timeout",
    };
    let message = e
        .body
        .as_deref()
        .and_then(|b| TOKEN_SHARING.iter().find(|(code, _)| b.contains(code)).map(|(_, m)| m.to_string()))
        .unwrap_or_else(|| e.message.clone());
    SessionError {
        kind: kind.into(),
        message,
        status: e.http.as_ref().map(|h| h.status),
        response: e.body.clone().map(|body| ErrorResponse { body }),
    }
}

/// Generic fallback (`{type:"unknown", message}`).
pub fn unknown(e: &anyhow::Error) -> SessionError {
    if let Some(ai) = e.downcast_ref::<AIError>() {
        return from_ai(ai);
    }
    if let Some(step) = e.downcast_ref::<StepFailed>() {
        return step.0.clone();
    }
    SessionError::new("unknown", e.to_string())
}

/// `StepFailedError`
#[derive(Debug, Clone, thiserror::Error)]
#[error("{}", .0.message)]
pub struct StepFailed(pub SessionError);
