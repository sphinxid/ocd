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
    let retry_after_ms = match &e.kind {
        ErrorKind::RateLimit { retry_after_ms, .. }
        | ErrorKind::QuotaExceeded { retry_after_ms }
        | ErrorKind::ProviderInternal { retry_after_ms } => {
            retry_after_ms.filter(|ms| ms.is_finite() && *ms >= 0.0).map(|ms| ms.ceil() as u64)
        }
        _ => None,
    };
    let error = SessionError {
        kind: kind.into(),
        message,
        status: e.http.as_ref().map(|h| h.status),
        response: e.body.clone().map(|body| ErrorResponse { body }),
        retry_after_ms,
    };
    // The OpenCode free tier's quota error asks the user to connect a provider or subscribe.
    if matches!(e.kind, ErrorKind::QuotaExceeded { .. }) && e.body.as_deref().is_some_and(|b| b.contains("FreeUsageLimitError")) {
        return SessionError { kind: "provider.free-limit".into(), message: FREE_USAGE_LIMIT_MESSAGE.into(), ..error };
    }
    error
}

const FREE_USAGE_LIMIT_MESSAGE: &str =
    "Free usage limit reached. Connect a provider or subscribe to OpenCode Go to keep going, or switch models.";

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

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use serde_json::json;

    fn ai(kind: ErrorKind, message: &str) -> AIError {
        AIError::new(kind, message)
    }

    // session-error.test.ts: provider session errors carry `retryAfterMs` when the AI error has it.
    #[test]
    fn retry_after_on_provider_errors() {
        let json = |e: SessionError| serde_json::to_value(e).unwrap();
        assert_eq!(
            json(from_ai(&ai(ErrorKind::RateLimit { retry_after_ms: Some(123.0), rate_limit: None }, "rate"))),
            json!({"type": "provider.rate-limit", "message": "rate", "retryAfterMs": 123})
        );
        assert_eq!(
            json(from_ai(&ai(ErrorKind::QuotaExceeded { retry_after_ms: Some(456.0) }, "quota"))),
            json!({"type": "provider.quota", "message": "quota", "retryAfterMs": 456})
        );
        assert_eq!(
            json(from_ai(&ai(ErrorKind::ProviderInternal { retry_after_ms: Some(789.0) }, "internal"))),
            json!({"type": "provider.internal", "message": "internal", "retryAfterMs": 789})
        );
        assert_eq!(json(from_ai(&ai(ErrorKind::Authentication, "auth"))), json!({"type": "provider.auth", "message": "auth"}));
    }

    // "separates the free usage limit from paid rate limits, Go quota, and other providers' 429s"
    #[test]
    fn free_usage_limit() {
        let fail = |url: &str, body: serde_json::Value, retry_after: &str| {
            let mut headers = IndexMap::new();
            headers.insert("retry-after".to_string(), retry_after.to_string());
            oc_ai::error::http_failure(
                "Rate limit exceeded. Please try again later.".into(),
                Some(url),
                Some(429),
                &headers,
                Some(body.to_string()),
            )
        };
        let free_body = json!({"type": "error", "error": {"type": "FreeUsageLimitError", "message": "Rate limit exceeded. Please try again later."}});
        let free = fail("https://opencode.ai/inference/openai/v1/chat/completions", free_body.clone(), "3600");
        assert!(!free.is_retryable());
        assert_eq!(
            serde_json::to_value(from_ai(&free)).unwrap(),
            json!({
                "type": "provider.free-limit",
                "message": "Free usage limit reached. Connect a provider or subscribe to OpenCode Go to keep going, or switch models.",
                "status": 429,
                "response": {"body": free_body.to_string()},
                "retryAfterMs": 3_600_000,
            })
        );
        let paid = fail(
            "https://opencode.ai/inference/openai/v1/chat/completions",
            json!({"error": {"type": "server_error", "message": "Upstream request failed: Rate limit exceeded."}}),
            "45",
        );
        assert!(paid.is_retryable());
        let e = from_ai(&paid);
        assert_eq!((e.kind.as_str(), e.retry_after_ms), ("provider.rate-limit", Some(45_000)));
        let go = fail(
            "https://opencode.ai/inference/go/openai/v1/chat/completions",
            json!({"type": "error", "error": {"type": "GoUsageLimitError", "message": "Subscription quota exceeded."}}),
            "120",
        );
        assert!(!go.is_retryable());
        let e = from_ai(&go);
        assert_eq!((e.kind.as_str(), e.retry_after_ms), ("provider.quota", Some(120_000)));
        let other = fail(
            "https://api.openai.com/v1/chat/completions",
            json!({"error": {"type": "rate_limit_error", "message": "Too many requests"}}),
            "10",
        );
        assert!(other.is_retryable());
        let e = from_ai(&other);
        assert_eq!((e.kind.as_str(), e.retry_after_ms), ("provider.rate-limit", Some(10_000)));
    }
}
