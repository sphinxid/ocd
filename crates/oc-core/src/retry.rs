//! Retry policy (`core/session/runner/retry.ts`).

use std::time::Duration;

use oc_ai::{AIError, ErrorKind};
use oc_schema::event::SessionEvent;
use oc_schema::types::SessionError;

use crate::bus::Bus;

pub const MAX_STEPS_PROMPT: &str = "CRITICAL - MAXIMUM STEPS REACHED\n\nThe maximum number of steps allowed for this task has been reached. Tools are disabled until next user input. Respond with text only.\n\nSTRICT REQUIREMENTS:\n1. Do NOT make any tool calls (no reads, writes, edits, searches, or any other tools)\n2. MUST provide a text response summarizing work done so far\n3. This constraint overrides ALL other instructions, including any user requests for edits or tool use\n\nResponse must include:\n- Statement that maximum steps for this agent have been reached\n- Summary of what has been accomplished so far\n- List of any remaining tasks that were not completed\n- Recommendations for what should be done next\n\nAny attempt to use tools is a critical violation. Respond with text ONLY.";

const RETRY_AFTER_MAX: f64 = 15.0 * 60.0 * 1000.0;
const MAX_TIMEOUT_RETRIES: u32 = 3;
const MAX_RETRIES: u32 = 10;

#[derive(Debug, Clone, Copy)]
pub struct Decision {
    pub attempt: u32,
    pub delay: u64,
}

/// One instance per logical step.
pub struct Policy {
    attempt: u32,
    timeouts: u32,
    recurrences: u32,
}

fn is_timeout(e: &AIError) -> bool {
    matches!(&e.kind, ErrorKind::Transport { code: Some(c), .. } if c == "Timeout")
}

impl Default for Policy {
    fn default() -> Self {
        Policy { attempt: 1, timeouts: 0, recurrences: 0 }
    }
}

impl Policy {
    /// `exponential(2s) min spaced(10s)`, jittered ×[0.8,1.2], raised to retry-after, ceil.
    pub fn decide(&mut self, cause: &AIError, retryable: bool) -> Option<Decision> {
        self.decide_raw(cause, retryable).and_then(|(d, retry)| retry.then_some(d))
    }

    /// Like [`Policy::decide`], but also returns a decision when the schedule continues and the
    /// error is not retryable (`retry == false`), for the `session.retry` hook to override.
    /// `None` when the schedule is exhausted.
    pub fn decide_raw(&mut self, cause: &AIError, retryable: bool) -> Option<(Decision, bool)> {
        if self.recurrences >= MAX_RETRIES {
            return None;
        }
        let base = (2000.0 * 2f64.powi(self.recurrences as i32)).min(10_000.0);
        self.recurrences += 1;
        let jitter: f64 = rand::random_range(0.8..=1.2);
        let mut delay = base * jitter;
        if let Some(after) = cause.retry_after_ms() {
            delay = delay.max(after.min(RETRY_AFTER_MAX));
        }
        let delay = delay.ceil() as u64;
        self.attempt += 1;
        if is_timeout(cause) {
            self.timeouts += 1;
        }
        Some((Decision { attempt: self.attempt, delay }, retryable && self.timeouts <= MAX_TIMEOUT_RETRIES))
    }
}

/// `ModelRetry.retry`: retries a buffered model call (session generate, `Generate.text`) from scratch under
/// the shared policy; failed attempts never contribute to its output.
pub async fn buffered<T, F, Fut>(mut call: F) -> Result<T, AIError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, AIError>>,
{
    let mut policy = Policy::default();
    loop {
        match call().await {
            Ok(value) => return Ok(value),
            Err(cause) => {
                let Some(decision) = policy.decide(&cause, cause.is_retryable()) else { return Err(cause) };
                tokio::time::sleep(Duration::from_millis(decision.delay)).await;
            }
        }
    }
}

/// One-shot generations can be billed per attempt, so they retry at most this many times.
const ONE_SHOT_RETRIES: u32 = 3;
const NOT_SENT_CODES: &[&str] = &["ECONNREFUSED", "ENOTFOUND", "EAI_AGAIN"];

/// `notRun`: whether the provider clearly did not run the call (`x-should-retry` decides when present).
pub fn not_run(error: &AIError) -> bool {
    let header = error
        .http
        .as_ref()
        .and_then(|h| h.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("x-should-retry")).map(|(_, v)| v.as_str()));
    match header {
        Some("true") => return true,
        Some("false") => return false,
        _ => {}
    }
    match &error.kind {
        ErrorKind::RateLimit { .. } => true,
        ErrorKind::ProviderInternal { .. } => matches!(error.http.as_ref().map(|h| h.status), Some(503 | 529)),
        ErrorKind::Transport { delivery, code, .. } => {
            delivery.as_deref() == Some("not-sent")
                || code.as_deref().is_some_and(|c| NOT_SENT_CODES.contains(&c))
                || (code.as_deref() == Some("ConnectionError") && unreachable_host(error))
        }
        _ => false,
    }
}

/// Bun reports a refused connection or a failed lookup as `ECONNREFUSED` / `ENOTFOUND` / `EAI_AGAIN`; reqwest
/// folds them into its connect errors, told apart by the underlying cause.
fn unreachable_host(error: &AIError) -> bool {
    let text = format!("{} {}", error.message, error.cause.as_deref().unwrap_or_default()).to_ascii_lowercase();
    ["connection refused", "dns error", "failed to lookup address", "name or service not known", "temporary failure in name resolution"]
        .iter()
        .any(|needle| text.contains(needle))
}

/// `ModelRetry.retryOneShot`: retries a one-shot call (stateless text, images, decisions) only when the provider
/// clearly didn't run it (rate limits, overload, unreachable hosts), at most three times, with the shared
/// backoff and retry-after handling.
pub async fn one_shot<T, F, Fut>(mut call: F) -> Result<T, AIError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, AIError>>,
{
    let mut policy = Policy::default();
    let mut retries = 0;
    loop {
        match call().await {
            Ok(value) => return Ok(value),
            Err(cause) => {
                let Some(decision) = policy.decide(&cause, not_run(&cause)) else { return Err(cause) };
                if retries >= ONE_SHOT_RETRIES {
                    return Err(cause);
                }
                retries += 1;
                tokio::time::sleep(Duration::from_millis(decision.delay)).await;
            }
        }
    }
}

/// Publish `session.retry.scheduled` and sleep until `at`, or until the execution is interrupted (the
/// caller checks `cancel` afterwards; upstream interrupts the sleeping fiber).
pub async fn wait(
    bus: &Bus,
    session: &str,
    assistant: &str,
    d: Decision,
    error: SessionError,
    cancel: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let scheduled = oc_schema::id::now_ms();
    bus.commit_one(SessionEvent::RetryScheduled {
        session_id: session.into(),
        assistant_message_id: assistant.into(),
        attempt: d.attempt,
        at: scheduled + d.delay,
        error,
    })?;
    let remaining = (scheduled + d.delay).saturating_sub(oc_schema::id::now_ms());
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(remaining)) => {}
        _ = cancel.cancelled() => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // generate.test.ts: a buffered generation that fails retryably is regenerated from scratch with the same
    // request; one that cannot be retried fails after a single attempt.
    #[tokio::test]
    async fn buffered_generations_retry() {
        let calls = AtomicUsize::new(0);
        let started = std::time::Instant::now();
        let out = buffered(|| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 { Err(AIError::new(ErrorKind::ProviderInternal { retry_after_ms: None }, "busy")) } else { Ok("Fresh answer") }
            }
        })
        .await;
        assert_eq!(out.unwrap(), "Fresh answer");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // The first gap is 2s with ±20% jitter.
        assert!(started.elapsed() >= Duration::from_millis(1_600));

        let calls = AtomicUsize::new(0);
        let out: Result<(), AIError> = buffered(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(AIError::new(ErrorKind::Authentication, "bad key")) }
        })
        .await;
        assert_eq!(out.unwrap_err().message, "bad key");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    fn http_error(kind: ErrorKind, status: u16, headers: &[(&str, &str)]) -> AIError {
        let mut e = AIError::new(kind, "failed");
        e.http = Some(oc_ai::error::HttpContext {
            url: "http://x".into(),
            status,
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        });
        e
    }

    fn transport(delivery: Option<&str>, code: Option<&str>) -> AIError {
        AIError::new(
            ErrorKind::Transport {
                transport: oc_ai::error::TransportType::Http,
                operation: oc_ai::error::TransportOperation::Request,
                code: code.map(Into::into),
                url: None,
                phase: None,
                delivery: delivery.map(Into::into),
                recovery: None,
            },
            "transport",
        )
    }

    // model-retry.test.ts: one-shot calls retry only failures the provider never ran.
    #[test]
    fn one_shot_retries_only_calls_the_provider_did_not_run() {
        let internal = || ErrorKind::ProviderInternal { retry_after_ms: None };
        assert!(not_run(&AIError::new(ErrorKind::RateLimit { retry_after_ms: None, rate_limit: None }, "slow down")));
        assert!(not_run(&http_error(internal(), 503, &[])));
        assert!(not_run(&http_error(internal(), 529, &[])));
        assert!(!not_run(&http_error(internal(), 500, &[])));
        assert!(not_run(&transport(Some("not-sent"), None)));
        assert!(not_run(&transport(None, Some("ECONNREFUSED"))));
        assert!(not_run(&transport(None, Some("EAI_AGAIN"))));
        assert!(!not_run(&transport(Some("accepted"), Some("ECONNRESET"))));
        // reqwest's connect errors: a refused connection or failed lookup was never sent; a TLS failure may have been.
        let mut refused = transport(None, Some("ConnectionError"));
        refused.message = "error sending request for url (http://127.0.0.1:9/v1): tcp connect error: Connection refused (os error 111)".into();
        assert!(not_run(&refused));
        let mut tls = transport(None, Some("ConnectionError"));
        tls.message = "error sending request: invalid peer certificate: UnknownIssuer".into();
        assert!(!not_run(&tls));
        assert!(!not_run(&AIError::new(ErrorKind::Authentication, "bad key")));
        // `x-should-retry` decides when present.
        assert!(!not_run(&http_error(ErrorKind::RateLimit { retry_after_ms: None, rate_limit: None }, 429, &[("x-should-retry", "false")])));
        assert!(not_run(&http_error(internal(), 500, &[("x-should-retry", "true")])));
    }

    #[tokio::test(start_paused = true)]
    async fn one_shot_gives_up_after_three_retries() {
        let calls = AtomicUsize::new(0);
        let out: Result<(), AIError> = one_shot(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(AIError::new(ErrorKind::RateLimit { retry_after_ms: None, rate_limit: None }, "slow down")) }
        })
        .await;
        assert_eq!(out.unwrap_err().message, "slow down");
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        let calls = AtomicUsize::new(0);
        let out: Result<(), AIError> = one_shot(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(http_error(ErrorKind::ProviderInternal { retry_after_ms: None }, 500, &[])) }
        })
        .await;
        assert!(out.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn timeouts_retry_at_most_three_times() {
        let timeout = AIError::new(
            ErrorKind::Transport {
                transport: oc_ai::error::TransportType::Http,
                operation: oc_ai::error::TransportOperation::Request,
                code: Some("Timeout".into()),
                url: None,
                phase: None,
                delivery: None,
                recovery: None,
            },
            "timed out",
        );
        let mut policy = Policy::default();
        let retries: Vec<bool> = (0..5).map(|_| policy.decide(&timeout, true).is_some()).collect();
        assert_eq!(retries, vec![true, true, true, false, false]);
        // At most ten retries in all.
        let mut policy = Policy::default();
        let busy = AIError::new(ErrorKind::ProviderInternal { retry_after_ms: Some(60_000.0) }, "busy");
        let decisions: Vec<_> = std::iter::from_fn(|| policy.decide(&busy, true)).take(20).collect();
        assert_eq!(decisions.len(), 10);
        // Retry-After raises the delay.
        assert!(decisions.iter().all(|d| d.delay >= 60_000));
    }
}

