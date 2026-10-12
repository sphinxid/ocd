//! Session retry helpers (`packages/core/src/session/runner/retry.ts`, `step.ts`, `llm.ts`,
//! `session/to-session-error.ts`; spec §7.4). No retries happen inside stream calls; the session runner uses
//! these helpers per logical step.

use crate::error::{AIError, ErrorKind};
use serde_json::{Map, Value, json};

/// Synthetic user text published before continuing an interrupted response (`llm.ts:35-36`).
pub const CONTINUE_TEXT: &str =
    "The previous response was interrupted. Continue from where you left off without repeating completed content.";

/// Message of the InvalidProviderOutput raised when a stream completes with finish reason `unknown`.
pub const UNKNOWN_FINISH_MESSAGE: &str = "The provider response ended with an unknown finish reason.";

/// Retries per step.
pub const MAX_RETRIES: u32 = 10;
/// Retries allowed for Transport `Timeout` failures.
pub const MAX_TIMEOUT_RETRIES: u32 = 3;
/// Upper bound applied to provider `retryAfterMs` (15 min).
pub const RETRY_AFTER_MAX_MS: f64 = 15.0 * 60_000.0;
pub const BASE_DELAY_MS: f64 = 2_000.0;
pub const MAX_DELAY_MS: f64 = 10_000.0;
pub const JITTER_MIN: f64 = 0.8;
pub const JITTER_MAX: f64 = 1.2;

/// ChatGPT token-sharing codes whose presence in a failure body forbids retrying.
pub use crate::credentials::flows::chatgpt::NON_RETRYABLE_SHARING_CODES;

/// Un-jittered delay before retry `n` (1-based): `min(2 s × 2^(n-1), 10 s)` → 2, 4, 8, 10, 10, ...
pub fn base_delay_ms(n: u32) -> f64 {
    let exponent = n.saturating_sub(1).min(30) as i32;
    (BASE_DELAY_MS * 2f64.powi(exponent)).min(MAX_DELAY_MS)
}

/// `retryAfterMs` bounded to 15 min, for RateLimit / ProviderInternal reasons only.
pub fn retry_after(error: &AIError) -> Option<f64> {
    match &error.kind {
        ErrorKind::RateLimit { retry_after_ms, .. } | ErrorKind::ProviderInternal { retry_after_ms } => {
            retry_after_ms.map(|ms| ms.min(RETRY_AFTER_MAX_MS))
        }
        _ => None,
    }
}

/// Transport failure with code `"Timeout"`.
pub fn is_timeout(error: &AIError) -> bool {
    matches!(&error.kind, ErrorKind::Transport { code: Some(code), .. } if code == "Timeout")
}

/// Retry eligibility: `isRetryable(error) || (outputStarted && interrupted stream)`; context overflow never.
pub fn eligible(error: &AIError, output_started: bool) -> bool {
    !error.is_context_overflow() && (error.is_retryable() || (output_started && error.is_interrupted_stream()))
}

/// Final delay for retry `n` with a jitter factor in `[0.8, 1.2]`: `ceil(max(base × jitter, retryAfter))`.
pub fn delay_ms(n: u32, jitter: f64, error: &AIError) -> u64 {
    let delay = base_delay_ms(n) * jitter;
    let delay = match retry_after(error) {
        Some(minimum) => delay.max(minimum),
        None => delay,
    };
    delay.max(0.0).ceil() as u64
}

/// A plugin hook's delay: non-finite or negative values fall back to the computed delay; else ceil.
pub fn normalize_hook_delay(hook_delay: f64, computed: u64) -> u64 {
    if hook_delay.is_finite() && hook_delay >= 0.0 { hook_delay.ceil() as u64 } else { computed }
}

/// Retry decision for one failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Retry after `delay_ms`; `attempt` is the upcoming attempt number (2 for the first retry).
    Retry { attempt: u32, delay_ms: u64 },
    Stop,
}

impl Decision {
    pub fn is_retry(&self) -> bool {
        matches!(self, Self::Retry { .. })
    }
}

/// Outcome kind of a failed step (`step.ts:145-288`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOutcome {
    /// Failure before any output: the same assistant message is retried.
    Retry,
    /// Failure after output started: wait, publish [`CONTINUE_TEXT`] and continue with a new message.
    Continue,
}

pub fn outcome(output_started: bool) -> RetryOutcome {
    if output_started { RetryOutcome::Continue } else { RetryOutcome::Retry }
}

/// Per-step retry state (`SessionRunnerRetry.policy`): exponential schedule capped at 10 s, 10 retries,
/// jitter, provider retry-after floor and the Transport-Timeout cap.
pub struct RetryPolicy {
    attempt: u32,
    steps: u32,
    timeouts: u32,
    jitter: Box<dyn FnMut() -> f64 + Send>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for RetryPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryPolicy")
            .field("attempt", &self.attempt)
            .field("steps", &self.steps)
            .field("timeouts", &self.timeouts)
            .finish()
    }
}

impl RetryPolicy {
    pub fn new() -> Self {
        Self::with_jitter(|| JITTER_MIN + rand::random::<f64>() * (JITTER_MAX - JITTER_MIN))
    }

    /// Deterministic jitter source (tests): must return factors in `[0.8, 1.2]`.
    pub fn with_jitter(jitter: impl FnMut() -> f64 + Send + 'static) -> Self {
        Self { attempt: 1, steps: 0, timeouts: 0, jitter: Box::new(jitter) }
    }

    /// Current attempt number (1 before any retry).
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Steps the schedule with an explicit eligibility (`input.retry`), as the hook-aware runner does.
    /// Returns `Stop` once 10 retries were scheduled; otherwise the attempt counter advances even when the
    /// decision is `Stop` (ineligible or too many timeouts), matching upstream.
    pub fn step(&mut self, error: &AIError, retry: bool) -> Decision {
        if self.steps >= MAX_RETRIES {
            return Decision::Stop;
        }
        self.steps += 1;
        let jitter = (self.jitter)();
        let delay = delay_ms(self.steps, jitter, error);
        self.attempt += 1;
        if is_timeout(error) {
            self.timeouts += 1;
        }
        if retry && self.timeouts <= MAX_TIMEOUT_RETRIES {
            Decision::Retry { attempt: self.attempt, delay_ms: delay }
        } else {
            Decision::Stop
        }
    }

    /// Decides for a failure. Context-overflow failures are never retried (they trigger compaction) and do
    /// not consume the schedule.
    pub fn decide(&mut self, error: &AIError, output_started: bool) -> Decision {
        if error.is_context_overflow() {
            return Decision::Stop;
        }
        self.step(error, eligible(error, output_started))
    }
}

/// `toSessionError` type for an AI error (§7.4 mapping).
pub fn session_error_type(error: &AIError) -> &'static str {
    match &error.kind {
        ErrorKind::RateLimit { .. } => "provider.rate-limit",
        ErrorKind::Authentication => "provider.auth",
        ErrorKind::QuotaExceeded => "provider.quota",
        ErrorKind::ContentPolicy => "provider.content-filter",
        ErrorKind::Transport { .. } => "provider.transport",
        ErrorKind::ProviderInternal { .. } => "provider.internal",
        ErrorKind::InvalidProviderOutput { .. } => "provider.invalid-output",
        ErrorKind::InvalidRequest { .. } => "provider.invalid-request",
        ErrorKind::UnsupportedOperation { .. } => "provider.unsupported-operation",
        ErrorKind::NoRoute { .. } => "provider.no-route",
        ErrorKind::UnknownProvider => "provider.unknown",
        ErrorKind::Timeout { .. } => "provider.timeout",
    }
}

/// Friendly messages for ChatGPT token-sharing error codes found in a response body
/// (`to-session-error.ts:10-26`), in upstream order.
pub const TOKEN_SHARING_MESSAGES: &[(&str, &str)] = &[
    (
        "subscription_sharing_user_not_eligible",
        "ChatGPT token sharing isn't available for this account. Connect with an API key or choose another provider.",
    ),
    (
        "subscription_sharing_usage_limit_exceeded",
        "ChatGPT usage limit reached. Check ChatGPT Settings → Usage for details.",
    ),
    ("subscription_sharing_usage_unavailable", "ChatGPT usage can't be checked right now. Try again later."),
    (
        "subscription_sharing_unsupported_capability",
        "This request uses a feature ChatGPT token sharing doesn't support. Remove the unsupported feature and try again.",
    ),
    (
        "subscription_sharing_route_not_supported",
        "ChatGPT token sharing doesn't support this API route. Check the configured endpoint and HTTP method.",
    ),
    ("subscription_sharing_invalid_user", "This ChatGPT connection is no longer valid. Reconnect to ChatGPT."),
    ("subscription_sharing_user_unavailable", "Your ChatGPT account is temporarily unavailable. Try again later."),
    (
        "chatpass_v2_scope_not_authorized",
        "This ChatGPT connection isn't authorized for this request. Reconnect to ChatGPT or choose another connection.",
    ),
    (
        "chatpass_v2_invalid_authorization_context",
        "This ChatGPT connection isn't authorized for this request. Reconnect to ChatGPT or choose another connection.",
    ),
];

/// `providerError(type, reason)`: `{type, message, status?, response?: {body}}`.
pub fn session_error(error: &AIError) -> Value {
    let body = error.body.as_deref();
    let message = body
        .and_then(|body| TOKEN_SHARING_MESSAGES.iter().find(|(code, _)| body.contains(code)).map(|(_, m)| *m))
        .unwrap_or(&error.message);
    let mut out = Map::new();
    out.insert("type".into(), json!(session_error_type(error)));
    out.insert("message".into(), json!(message));
    if let Some(http) = &error.http {
        out.insert("status".into(), json!(http.status));
    }
    if let Some(body) = body {
        out.insert("response".into(), json!({ "body": body }));
    }
    Value::Object(out)
}

/// ChatGPT plugin `session.retry` hook: token-sharing failures are not retried.
pub fn chatgpt_forbids_retry(response_body: Option<&str>) -> bool {
    crate::credentials::flows::chatgpt::forbids_retry(response_body)
}
