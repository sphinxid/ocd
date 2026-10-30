//! `toSessionError` for provider errors (`core/session/to-session-error.ts`).

use oc_ai::{AIError, ErrorKind};
use oc_schema::types::{ErrorResponse, SessionError};
use once_cell::sync::Lazy;
use regex::Regex;

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
    let body = e.body.as_deref().unwrap_or_default();
    // ChatGPT's usage limit arrives in HTTP and stream errors alike, whatever their reason, so its code decides the type.
    if body.contains("subscription_sharing_usage_limit_exceeded") {
        return SessionError { kind: "provider.chatgpt-limit".into(), ..error };
    }
    if !matches!(e.kind, ErrorKind::QuotaExceeded { .. }) {
        return error;
    }
    // The OpenCode free tier's quota error asks the user to connect a provider or subscribe.
    if body.contains("FreeUsageLimitError") {
        return SessionError { kind: "provider.free-limit".into(), message: FREE_USAGE_LIMIT_MESSAGE.into(), ..error };
    }
    // OpenCode Console names what a refusing budget covers, in its envelope's metadata or, for Google
    // clients, ErrorInfo.
    if body.contains("BudgetLimitError") {
        static COVERAGE_MODELS: Lazy<Regex> = Lazy::new(|| Regex::new(r#""coverage"\s*:\s*"models""#).unwrap());
        return if COVERAGE_MODELS.is_match(body) {
            SessionError { kind: "provider.model-budget-limit".into(), message: MODEL_BUDGET_LIMIT_MESSAGE.into(), ..error }
        } else {
            SessionError { kind: "provider.budget-limit".into(), message: BUDGET_LIMIT_MESSAGE.into(), ..error }
        };
    }
    // OpenCode Console names the Go limit that refused the request (5 hour, weekly or monthly) in its envelope's
    // metadata.
    if body.contains("GoUsageLimitError") {
        static LIMIT_NAME: Lazy<Regex> = Lazy::new(|| Regex::new(r#""limitName"\s*:\s*"([^"]+)""#).unwrap());
        let name = LIMIT_NAME.captures(body).map(|c| format!("{} ", &c[1])).unwrap_or_default();
        return SessionError {
            kind: "provider.go-limit".into(),
            message: format!(
                "OpenCode Go {name}usage limit reached. Switch to a free model, or turn on Extra Usage on the Go page in OpenCode Console to keep going."
            ),
            ..error
        };
    }
    error
}

const FREE_USAGE_LIMIT_MESSAGE: &str =
    "Free usage limit reached. Connect a provider or subscribe to OpenCode Go to keep going, or switch models.";
const BUDGET_LIMIT_MESSAGE: &str = "Workspace usage limit reached. Model use is paused until the limit resets.";
const MODEL_BUDGET_LIMIT_MESSAGE: &str = "You've reached the spend limit for this model. Switch models to keep going.";

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
        let go_body = json!({
            "type": "error",
            "error": {"type": "GoUsageLimitError", "message": "Go usage limit exceeded"},
            "metadata": {"workspace": "wrk_go_limit", "limitName": "5 hour"},
        });
        let go = fail("https://opencode.ai/inference/go/openai/v1/chat/completions", go_body.clone(), "120");
        assert!(!go.is_retryable());
        assert_eq!(
            serde_json::to_value(from_ai(&go)).unwrap(),
            json!({
                "type": "provider.go-limit",
                "message": "OpenCode Go 5 hour usage limit reached. Switch to a free model, or turn on Extra Usage on the Go page in OpenCode Console to keep going.",
                "status": 429,
                "response": {"body": go_body.to_string()},
                "retryAfterMs": 120_000,
            })
        );
        let unnamed = fail(
            "https://opencode.ai/inference/go/openai/v1/chat/completions",
            json!({"type": "error", "error": {"type": "GoUsageLimitError", "message": "Go usage limit exceeded"}}),
            "120",
        );
        assert_eq!(
            from_ai(&unnamed).message,
            "OpenCode Go usage limit reached. Switch to a free model, or turn on Extra Usage on the Go page in OpenCode Console to keep going."
        );
        let credit = oc_ai::error::http_failure(
            "Credit limit exceeded.".into(),
            Some("https://opencode.ai/inference/openai/v1/chat/completions"),
            Some(402),
            &IndexMap::new(),
            Some(json!({"type": "error", "error": {"type": "CreditLimitExceeded", "message": "Credit limit exceeded."}}).to_string()),
        );
        assert_eq!(from_ai(&credit).kind, "provider.quota");
        let other = fail(
            "https://api.openai.com/v1/chat/completions",
            json!({"error": {"type": "rate_limit_error", "message": "Too many requests"}}),
            "10",
        );
        assert!(other.is_retryable());
        let e = from_ai(&other);
        assert_eq!((e.kind.as_str(), e.retry_after_ms), ("provider.rate-limit", Some(10_000)));
    }

    // "tells a budget on the requested model from one pausing all model use"
    #[test]
    fn budget_limits() {
        let refuse = |body: &serde_json::Value| {
            oc_ai::error::http_failure(
                "A budget policy for this model has been reached.".into(),
                Some("https://opencode.ai/inference/openai/v1/chat/completions"),
                Some(429),
                &IndexMap::new(),
                Some(body.to_string()),
            )
        };
        let model_body = json!({
            "type": "error",
            "error": {"type": "BudgetLimitError", "message": "A budget policy for this model has been reached."},
            "metadata": {"coverage": "models", "resetsAt": "2026-11-01T00:00:00.000Z"},
        });
        let model = refuse(&model_body);
        assert!(!model.is_retryable());
        assert_eq!(
            serde_json::to_value(from_ai(&model)).unwrap(),
            json!({
                "type": "provider.model-budget-limit",
                "message": "You've reached the spend limit for this model. Switch models to keep going.",
                "status": 429,
                "response": {"body": model_body.to_string()},
            })
        );
        // Google clients get Google's envelope, with the same metadata in its ErrorInfo.
        let all = refuse(&json!({
            "error": {
                "code": 429,
                "status": "RESOURCE_EXHAUSTED",
                "message": "Policy budget exceeded",
                "details": [{
                    "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                    "reason": "BudgetLimitError",
                    "domain": "opencode.ai",
                    "metadata": {"coverage": "all", "resetsAt": "2026-11-01T00:00:00.000Z"},
                }],
            }
        }));
        assert!(!all.is_retryable());
        let e = from_ai(&all);
        assert_eq!(e.kind, "provider.budget-limit");
        assert_eq!(e.message, "Workspace usage limit reached. Model use is paused until the limit resets.");
    }

    // "types the ChatGPT usage limit whichever way the failure arrives"
    #[test]
    fn chatgpt_limit() {
        let body = json!({"error": {"code": "subscription_sharing_usage_limit_exceeded", "message": "Limit"}}).to_string();
        for kind in [ErrorKind::RateLimit { retry_after_ms: None, rate_limit: None }, ErrorKind::UnknownProvider] {
            let mut error = AIError::new(kind, "Request failed");
            error.body = Some(body.clone());
            assert_eq!(
                serde_json::to_value(from_ai(&error)).unwrap(),
                json!({
                    "type": "provider.chatgpt-limit",
                    "message": "ChatGPT usage limit reached. Check ChatGPT Settings → Usage for details.",
                    "response": {"body": body},
                })
            );
        }
    }
}
