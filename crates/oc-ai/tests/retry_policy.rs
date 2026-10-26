use indexmap::IndexMap;
use oc_ai::error::{AIError, ErrorKind, FailureClassification, HttpContext, TransportOperation};
use oc_ai::retry::{
    self, CONTINUE_TEXT, Decision, MAX_RETRIES, RetryOutcome, RetryPolicy, base_delay_ms, delay_ms, eligible,
    normalize_hook_delay, session_error, session_error_type,
};
use serde_json::json;

fn rate_limit(retry_after_ms: Option<f64>) -> AIError {
    AIError::new(ErrorKind::RateLimit { retry_after_ms, rate_limit: None }, "slow down")
}

fn timeout() -> AIError {
    AIError::transport(TransportOperation::Request, "Response headers timed out", Some("Timeout"), None)
}

#[test]
fn base_schedule() {
    let delays: Vec<f64> = (1..=6).map(base_delay_ms).collect();
    assert_eq!(delays, [2000.0, 4000.0, 8000.0, 10000.0, 10000.0, 10000.0]);
    assert_eq!(base_delay_ms(40), 10000.0);
}

#[test]
fn jitter_and_retry_after() {
    let error = AIError::provider_internal("boom");
    assert_eq!(delay_ms(1, 0.8, &error), 1600);
    assert_eq!(delay_ms(1, 1.2, &error), 2400);
    assert_eq!(delay_ms(2, 1.0000001, &error), 4001);
    // retry-after raises the floor, capped at 15 minutes.
    assert_eq!(delay_ms(1, 1.0, &rate_limit(Some(30_000.0))), 30_000);
    assert_eq!(delay_ms(1, 1.0, &rate_limit(Some(10.0))), 2_000);
    assert_eq!(delay_ms(1, 1.0, &rate_limit(Some(3_600_000.0))), 900_000);
    let internal = AIError::new(ErrorKind::ProviderInternal { retry_after_ms: Some(5_000.5) }, "x");
    assert_eq!(delay_ms(1, 1.0, &internal), 5_001);
}

#[test]
fn policy_schedule_runs_ten_retries() {
    let mut policy = RetryPolicy::with_jitter(|| 1.0);
    let error = AIError::provider_internal("boom");
    let mut decisions = Vec::new();
    loop {
        match policy.decide(&error, false) {
            Decision::Retry { attempt, delay_ms } => decisions.push((attempt, delay_ms)),
            Decision::Stop => break,
        }
    }
    assert_eq!(decisions.len(), MAX_RETRIES as usize);
    assert_eq!(decisions[0], (2, 2000));
    assert_eq!(decisions[1], (3, 4000));
    assert_eq!(decisions[2], (4, 8000));
    assert_eq!(decisions[3], (5, 10000));
    assert_eq!(decisions[9], (11, 10000));
}

#[test]
fn jitter_bounds() {
    let mut policy = RetryPolicy::new();
    let error = AIError::provider_internal("boom");
    for n in 1..=MAX_RETRIES {
        let Decision::Retry { delay_ms, .. } = policy.decide(&error, false) else { panic!("expected retry") };
        let base = base_delay_ms(n);
        assert!(delay_ms as f64 >= (base * 0.8).floor() && delay_ms as f64 <= (base * 1.2).ceil(), "{n}: {delay_ms}");
    }
}

#[test]
fn timeout_retries_are_capped() {
    let mut policy = RetryPolicy::with_jitter(|| 1.0);
    let error = timeout();
    assert!(policy.decide(&error, false).is_retry());
    assert!(policy.decide(&error, false).is_retry());
    assert!(policy.decide(&error, false).is_retry());
    assert_eq!(policy.decide(&error, false), Decision::Stop);
    // Upstream keeps the cumulative counter: once exceeded, the step stops retrying altogether.
    assert_eq!(policy.decide(&AIError::provider_internal("x"), false), Decision::Stop);
    assert_eq!(policy.attempt(), 6);
}

#[test]
fn eligibility_rules() {
    assert!(eligible(&rate_limit(None), false));
    assert!(!eligible(&AIError::authentication("no"), false));
    let read = AIError::transport(TransportOperation::Read, "reset", None, None);
    assert!(eligible(&read, true));
    let incomplete = AIError::incomplete_stream("openai", "cut");
    assert!(eligible(&incomplete, false));
    let overflow = AIError::new(
        ErrorKind::InvalidRequest { parameter: None, classification: Some(FailureClassification::ContextOverflow) },
        "prompt is too long",
    );
    assert!(!eligible(&overflow, true));
    let mut policy = RetryPolicy::with_jitter(|| 1.0);
    assert_eq!(policy.decide(&overflow, false), Decision::Stop);
    assert_eq!(policy.attempt(), 1, "overflow does not consume the schedule");
    // An ineligible error still advances the attempt counter.
    assert_eq!(policy.decide(&AIError::authentication("no"), false), Decision::Stop);
    assert_eq!(policy.attempt(), 2);
    // x-should-retry overrides.
    let mut headers = IndexMap::new();
    headers.insert("x-should-retry".to_string(), "false".to_string());
    let error = rate_limit(None).with_http(Some(HttpContext { url: "u".into(), status: 429, headers }));
    assert!(!eligible(&error, false));
}

#[test]
fn hook_delay_normalization_and_outcomes() {
    assert_eq!(normalize_hook_delay(f64::NAN, 2000), 2000);
    assert_eq!(normalize_hook_delay(-1.0, 2000), 2000);
    assert_eq!(normalize_hook_delay(0.2, 2000), 1);
    assert_eq!(normalize_hook_delay(0.0, 2000), 0);
    assert_eq!(retry::outcome(false), RetryOutcome::Retry);
    assert_eq!(retry::outcome(true), RetryOutcome::Continue);
    assert_eq!(
        CONTINUE_TEXT,
        "The previous response was interrupted. Continue from where you left off without repeating completed content."
    );
}

#[test]
fn session_error_mapping() {
    let cases = [
        (rate_limit(None), "provider.rate-limit"),
        (AIError::authentication("x"), "provider.auth"),
        (AIError::new(ErrorKind::QuotaExceeded { retry_after_ms: None }, "x"), "provider.quota"),
        (AIError::new(ErrorKind::ContentPolicy, "x"), "provider.content-filter"),
        (timeout(), "provider.transport"),
        (AIError::provider_internal("x"), "provider.internal"),
        (AIError::incomplete_stream("r", "x"), "provider.invalid-output"),
        (AIError::invalid_request("x"), "provider.invalid-request"),
        (AIError::unsupported("op", "x", None, None), "provider.unsupported-operation"),
        (
            AIError::new(ErrorKind::NoRoute { route: "r".into(), provider: "p".into(), model: "m".into() }, "x"),
            "provider.no-route",
        ),
        (AIError::unknown_provider("x"), "provider.unknown"),
        (AIError::new(ErrorKind::Timeout { timeout_ms: None }, "x"), "provider.timeout"),
    ];
    for (error, expected) in cases {
        assert_eq!(session_error_type(&error), expected);
    }
    let error = AIError::new(ErrorKind::QuotaExceeded { retry_after_ms: None }, "Provider request failed with HTTP 429")
        .with_body(r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}"#)
        .with_http(Some(HttpContext { url: "u".into(), status: 429, headers: IndexMap::new() }));
    assert_eq!(
        session_error(&error),
        json!({
            "type": "provider.quota",
            "message": "ChatGPT usage limit reached. Check ChatGPT Settings → Usage for details.",
            "status": 429,
            "response": {"body": r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}"#}
        })
    );
    assert_eq!(serde_json::to_string(&session_error(&AIError::authentication("Bad key"))).unwrap(), r#"{"type":"provider.auth","message":"Bad key"}"#);
    assert!(retry::chatgpt_forbids_retry(Some("chatpass_v2_scope_not_authorized")));
    assert!(!retry::chatgpt_forbids_retry(None));
}
