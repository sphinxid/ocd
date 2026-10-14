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

