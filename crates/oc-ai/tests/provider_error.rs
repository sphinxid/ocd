//! Ports of the cases opencode 2.2 added to `packages/ai/test/provider-error.test.ts`.

use indexmap::IndexMap;
use oc_ai::error::{
    classify_provider_failure, is_context_overflow, provider_error_message, FailureClassification, HttpContext,
    ProviderFailure,
};
use serde_json::{json, Value};

fn classify(message: &str, status: Option<u16>, body: &Value) -> oc_ai::AIError {
    classify_provider_failure(ProviderFailure {
        message: message.into(),
        status,
        raw_body: Some(body.to_string()),
        ..Default::default()
    })
}

#[test]
fn classifies_alibaba_model_studio_data_inspection_failures() {
    let alibaba = json!({
        "request_id": "1537ecc9-6873-93f7-9865-6fe4eb1d7851",
        "code": "DataInspectionFailed",
        "message": "Green net check failed for text (input): Input data may contain inappropriate content.",
    });
    assert_eq!(classify(alibaba["message"].as_str().unwrap(), Some(400), &alibaba).tag(), "ContentPolicy");
}

#[test]
fn classifies_openai_image_moderation_blocks_directly_and_through_the_vercel_ai_gateway() {
    let message = "Your request was rejected by the safety system. If you believe this is an error, contact us at help.openai.com and include the request ID req_123.";
    let openai = json!({
        "error": {
            "message": message,
            "type": "image_generation_user_error",
            "param": null,
            "code": "moderation_blocked",
            "moderation_details": { "moderation_stage": "input", "categories": ["other"] },
        }
    });
    // The gateway keeps OpenAI's message but replaces its type and drops the code.
    let vercel = json!({
        "error": {
            "message": message,
            "type": "AI_APICallError",
            "param": { "error": message, "statusCode": 400, "name": "AI_APICallError", "message": message, "isRetryable": false },
        }
    });
    for body in [openai, vercel] {
        assert_eq!(classify(message, Some(400), &body).tag(), "ContentPolicy");
    }
}

#[test]
fn stability_name_codes_are_no_longer_read() {
    // 2.2 removed Stability's `{ id, name, errors }` code; a 403 without other signals is an auth failure.
    let stability = json!({ "id": "a1b2", "name": "content_moderation", "errors": ["Your request was flagged"] });
    assert_eq!(classify("Forbidden", Some(403), &stability).tag(), "Authentication");
}

#[test]
fn classifies_budget_limits_as_quota() {
    let typed = |kind: &str, message: &str| json!({ "type": "error", "error": { "type": kind, "message": message } });
    let google = json!({
        "error": {
            "code": 429,
            "status": "RESOURCE_EXHAUSTED",
            "message": "A budget policy for this model has been reached.",
            "details": [{
                "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                "reason": "BudgetLimitError",
                "domain": "opencode.ai",
                "metadata": { "coverage": "models", "resetsAt": "2026-11-01T00:00:00.000Z" },
            }],
        }
    });
    for body in [typed("BudgetLimitError", "A budget policy for this model has been reached."), google] {
        let message = body["error"]["message"].as_str().unwrap();
        assert_eq!(classify(message, Some(429), &body).tag(), "QuotaExceeded");
    }
}

#[test]
fn classifies_bedrock_throttling_as_a_retryable_rate_limit_not_context_overflow() {
    for body in [
        r#"{"message":"Too many tokens, please wait before trying again."}"#,
        r#"{"message":"Too many tokens per day, please wait before trying again."}"#,
    ] {
        let error = classify_provider_failure(ProviderFailure {
            message: provider_error_message(body).unwrap(),
            status: Some(429),
            raw_body: Some(body.into()),
            ..Default::default()
        });
        assert_eq!(error.tag(), "RateLimit");
        assert!(!error.is_context_overflow());
        assert!(error.is_retryable());
    }
    assert!(!is_context_overflow("Too many tokens, please wait before trying again."));
    assert!(!is_context_overflow("Too many tokens per day, please wait before trying again."));
    // Plain overflow phrasing without the throttle cue still compacts.
    assert!(is_context_overflow("Too many tokens"));
}

#[test]
fn classifies_429_token_throttles_as_retryable_rate_limits_not_context_overflow() {
    for body in [
        // Cerebras, captured live
        r#"{"message":"Tokens per minute limit exceeded - too many tokens processed.","type":"too_many_tokens_error","param":"quota","code":"token_quota_exceeded"}"#,
        r#"{"message":"Token limit reached: too many tokens per minute for this model."}"#,
    ] {
        let error = classify_provider_failure(ProviderFailure {
            message: provider_error_message(body).unwrap(),
            status: Some(429),
            raw_body: Some(body.into()),
            ..Default::default()
        });
        assert_eq!(error.tag(), "RateLimit", "{body}");
        assert!(error.is_retryable());
    }
}

#[test]
fn quota_text_still_matches_outside_token_quota_codes() {
    let body = r#"{"message":"Token quota exceeded for this account; Quota exceeded."}"#;
    let error = classify_provider_failure(ProviderFailure {
        message: "x".into(),
        status: Some(429),
        raw_body: Some(body.into()),
        ..Default::default()
    });
    assert_eq!(error.tag(), "QuotaExceeded");
    let error = classify_provider_failure(ProviderFailure {
        message: "TOKEN_QUOTA_EXCEEDED".into(),
        status: Some(429),
        ..Default::default()
    });
    assert_eq!(error.tag(), "RateLimit");
}

#[test]
fn does_not_infer_context_overflow_from_known_throttling_exceptions() {
    let mut headers = IndexMap::new();
    headers.insert("x-amzn-errortype".to_string(), "ThrottlingException:http://internal.amazon.com/coral/".to_string());
    let cases: Vec<ProviderFailure> = vec![
        ProviderFailure { status: Some(429), data: Some(json!({ "code": "ThrottlingException" })), ..Default::default() },
        ProviderFailure {
            status: Some(429),
            raw_body: Some(r#"{"__type":"com.amazonaws.bedrock#ThrottlingException"}"#.into()),
            ..Default::default()
        },
        ProviderFailure { raw_body: Some(r#"{"exception":{"type":"throttlingException"}}"#.into()), ..Default::default() },
        ProviderFailure {
            status: Some(429),
            http: Some(HttpContext {
                url: "https://bedrock.test/model/example/converse-stream".into(),
                status: 429,
                headers,
            }),
            ..Default::default()
        },
    ];
    for input in cases {
        for message in ["Token limit exceeded", "Too many tokens, please try again later"] {
            let error = classify_provider_failure(ProviderFailure { message: message.into(), ..input.clone() });
            assert_eq!(error.tag(), "RateLimit", "{message} {:?}", input.raw_body);
            assert!(!error.is_context_overflow());
            assert!(error.is_retryable());
        }
    }
    let error = classify_provider_failure(ProviderFailure {
        message: "prompt is too long".into(),
        status: Some(400),
        data: Some(json!({ "code": "ValidationException" })),
        ..Default::default()
    });
    assert_eq!(error.tag(), "InvalidRequest");
    assert_eq!(error.classification(), Some(FailureClassification::ContextOverflow));
}

#[test]
fn reads_namespaced_and_suffixed_aws_error_types() {
    // `code` and `__type` keep only the shape name.
    for body in [
        r#"{"__type":"com.amazonaws.bedrock#ServiceUnavailableException:http://internal/"}"#,
        r#"{"code":"aws.protocols#ServiceUnavailableException"}"#,
    ] {
        let error = classify_provider_failure(ProviderFailure {
            message: "x".into(),
            raw_body: Some(body.into()),
            ..Default::default()
        });
        assert_eq!(error.tag(), "ProviderInternal", "{body}");
    }
}
