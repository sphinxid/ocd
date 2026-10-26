//! Gemini response continuation (upstream `test/provider/gemini.test.ts`, "Gemini response
//! continuation"), plus `MALFORMED_RESPONSE` being retryable.

use std::sync::Arc;

use oc_ai::protocols::gemini as g;
use oc_ai::route::auth;
use oc_ai::testing::decode;
use oc_ai::*;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One scripted answer: a whole SSE body, a body whose connection drops after it, or a body that
/// stalls forever after it.
enum Reply {
    Body(String),
    Dropped(String),
    Stalled(String),
}

/// Serves one scripted reply per request and records every sent body.
async fn scripted(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1beta", listener.local_addr().unwrap());
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = bodies.clone();
    tokio::spawn(async move {
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let body = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..end]).to_string();
                    let length = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + length {
                        break buf[end + 4..end + 4 + length].to_vec();
                    }
                }
                assert!(n > 0);
            };
            seen.lock().push(serde_json::from_slice(&body).unwrap());
            let (text, declared) = match &reply {
                Reply::Body(text) => (text.clone(), text.len()),
                // Promise more bytes than are sent, so the read fails as a dropped connection.
                Reply::Dropped(text) | Reply::Stalled(text) => (text.clone(), text.len() + 100),
            };
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {declared}\r\nconnection: close\r\n\r\n"
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(text.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
            match reply {
                Reply::Stalled(_) => {
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        drop(socket);
                    });
                }
                _ => drop(socket),
            }
        }
    });
    (base, bodies)
}

fn sse(values: &[Value]) -> String {
    values.iter().map(|v| format!("data: {v}\n\n")).collect()
}

fn text(value: &str, extra: Value) -> Value {
    let mut candidate = json!({"content": {"role": "model", "parts": [{"text": value}]}});
    for (k, v) in extra.as_object().unwrap() {
        candidate[k] = v.clone();
    }
    json!({"candidates": [candidate]})
}

fn continuation(token: Option<&str>) -> Value {
    let mut candidate = json!({"content": {"role": "model", "parts": [{"text": ""}]}, "finishReason": "CONTINUATION"});
    if let Some(token) = token {
        candidate["continuationToken"] = json!(token);
    }
    json!({"candidates": [candidate]})
}

fn with_usage(mut event: Value, usage: Value) -> Value {
    event["usageMetadata"] = usage;
    event
}

fn request(base: &str) -> LLMRequest {
    let model = g::route()
        .base_url(Some(base.into()))
        .auth(auth::none())
        .model("gemini-test")
        .unwrap();
    LLMRequest::new(model)
        .prompt("Count.")
        .tool(ToolDefinition::new(
            "lookup",
            "Lookup data",
            json!({"type": "object"}),
        ))
}

async fn generate(req: LLMRequest) -> Result<LLMResponse, AIError> {
    Client::new().generate(req, Default::default()).await
}

fn with_token(body: &Value, token: &str) -> Value {
    let mut out = body.clone();
    out["continuationToken"] = json!(token);
    out
}

#[tokio::test]
async fn stitches_text_split_across_three_requests_into_one_block() {
    let (base, bodies) = scripted(vec![
        Reply::Body(sse(&[
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "plan", "thought": true, "thoughtSignature": "sig_1"}]}}]}),
            text("THIRTEEN THOUSAND TWO", json!({})),
            continuation(Some("token-1")),
        ])),
        Reply::Body(sse(&[
            text(" HUNDRED SEVEN", json!({})),
            // The finishing chunk's token supersedes an earlier checkpoint's.
            text("TY-TWO", json!({"continuationToken": "checkpoint"})),
            continuation(Some("token-2")),
        ])),
        Reply::Body(sse(&[text(" DONE", json!({"finishReason": "STOP"}))])),
    ])
    .await;
    let response = generate(request(&base)).await.unwrap();
    assert_eq!(
        response.text(),
        "THIRTEEN THOUSAND TWO HUNDRED SEVENTY-TWO DONE"
    );
    assert_eq!(response.reasoning(), "plan");
    let kinds: Vec<&str> = response
        .events
        .iter()
        .map(LLMEvent::type_name)
        .filter(|t| *t != "text-delta")
        .collect();
    assert_eq!(
        kinds,
        [
            "step-start",
            "reasoning-start",
            "reasoning-delta",
            "reasoning-end",
            "text-start",
            "text-end",
            "step-finish",
            "finish"
        ]
    );
    let end = response
        .events
        .iter()
        .find(|e| matches!(e, LLMEvent::ReasoningEnd { .. }))
        .unwrap();
    let LLMEvent::ReasoningEnd {
        provider_metadata, ..
    } = end
    else {
        unreachable!()
    };
    assert_eq!(
        provider_metadata.as_ref().unwrap()["google"],
        json!({"thoughtSignature": "sig_1"})
    );
    assert_eq!(response.finish_reason.normalized, FinishReason::Stop);
    assert_eq!(response.finish_reason.raw.as_deref(), Some("STOP"));
    let bodies = bodies.lock().clone();
    assert_eq!(bodies.len(), 3);
    assert!(bodies[0].get("continuationToken").is_none());
    assert_eq!(bodies[1], with_token(&bodies[0], "token-1"));
    assert_eq!(bodies[2], with_token(&bodies[0], "token-2"));
}

#[tokio::test]
async fn finishes_with_a_signed_tool_call_from_a_later_request() {
    let (base, _) = scripted(vec![
        Reply::Body(sse(&[text("Looking", json!({})), continuation(Some("token-1"))])),
        Reply::Body(sse(&[json!({"candidates": [{
            "content": {"role": "model", "parts": [
                {"text": " it up."},
                {"functionCall": {"id": "call_1", "name": "lookup", "args": {}}, "thoughtSignature": "call_sig"}
            ]},
            "finishReason": "STOP"
        }]})])),
    ])
    .await;
    let response = generate(request(&base)).await.unwrap();
    assert_eq!(response.text(), "Looking it up.");
    assert_eq!(
        response
            .events
            .iter()
            .filter(|e| matches!(e, LLMEvent::TextStart { .. }))
            .count(),
        1
    );
    let call = response
        .events
        .iter()
        .find(|e| matches!(e, LLMEvent::ToolCall { .. }))
        .unwrap();
    let LLMEvent::ToolCall {
        id,
        provider_metadata,
        ..
    } = call
    else {
        unreachable!()
    };
    assert_eq!(id, "call_1");
    assert_eq!(
        provider_metadata.as_ref().unwrap()["google"],
        json!({"thoughtSignature": "call_sig"})
    );
    assert_eq!(response.finish_reason.normalized, FinishReason::ToolCalls);
    assert_eq!(response.finish_reason.raw.as_deref(), Some("STOP"));
}

#[tokio::test]
async fn forgets_a_block_once_the_resumed_request_finishes() {
    let (base, _) = scripted(vec![
        Reply::Body(sse(&[
            text("THIRTEEN", json!({"continuationToken": "checkpoint-1"})),
            json!({"promptFeedback": {"blockReason": "OTHER"}}),
        ])),
        Reply::Body(sse(&[text(" THOUSAND", json!({"finishReason": "STOP"}))])),
    ])
    .await;
    let response = generate(request(&base)).await.unwrap();
    assert_eq!(response.text(), "THIRTEEN THOUSAND");
    assert_eq!(response.finish_reason.normalized, FinishReason::Stop);
    let metadata: Vec<_> = response
        .events
        .iter()
        .filter_map(|e| {
            if let LLMEvent::Finish {
                provider_metadata, ..
            } = e
            {
                Some(provider_metadata.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(metadata, vec![None]);
}

#[tokio::test]
async fn resumes_a_dropped_stream_from_its_latest_checkpoint() {
    let (base, bodies) = scripted(vec![
        Reply::Dropped(sse(&[
            text("THIRTEEN", json!({})),
            text(" THOUSAND", json!({"continuationToken": "checkpoint-1"})),
        ])),
        Reply::Body(sse(&[text(" TWO", json!({"finishReason": "STOP"}))])),
    ])
    .await;
    let response = generate(request(&base)).await.unwrap();
    assert_eq!(response.text(), "THIRTEEN THOUSAND TWO");
    assert_eq!(response.finish_reason.normalized, FinishReason::Stop);
    let bodies = bodies.lock().clone();
    assert_eq!(bodies[1], with_token(&bodies[0], "checkpoint-1"));
}

#[tokio::test]
async fn counts_the_usage_of_a_dropped_request_once_resumed() {
    let (base, _) = scripted(vec![
        Reply::Dropped(sse(&[
            with_usage(text("THIRTEEN", json!({})), json!({"promptTokenCount": 120, "candidatesTokenCount": 5})),
            with_usage(
                text(" THOUSAND", json!({"continuationToken": "checkpoint-1"})),
                json!({"promptTokenCount": 142, "candidatesTokenCount": 32768, "totalTokenCount": 32910}),
            ),
        ])),
        Reply::Body(sse(&[with_usage(
            text(" TWO", json!({"finishReason": "STOP"})),
            json!({"promptTokenCount": 32910, "candidatesTokenCount": 100, "totalTokenCount": 33010}),
        )])),
    ])
    .await;
    let usage = generate(request(&base)).await.unwrap().usage.unwrap();
    assert_eq!(usage.input_tokens, Some(142));
    assert_eq!(usage.output_tokens, Some(32868));
    assert_eq!(usage.non_cached_input_tokens, Some(142));
    assert_eq!(usage.total_tokens, Some(33010));
    assert_eq!(usage.cache_read_input_tokens, None);
    assert_eq!(
        usage.provider_metadata.unwrap()["google"],
        json!({"promptTokenCount": 33052, "candidatesTokenCount": 32868, "totalTokenCount": 65920, "requests": 2})
    );
}

#[tokio::test]
async fn continues_a_stream_that_ends_at_a_checkpoint_without_finishing() {
    let (base, bodies) = scripted(vec![
        Reply::Body(sse(&[text(
            "THIRTEEN",
            json!({"continuationToken": "checkpoint-1"}),
        )])),
        Reply::Body(sse(&[text(" THOUSAND", json!({"finishReason": "STOP"}))])),
    ])
    .await;
    let response = generate(request(&base)).await.unwrap();
    assert_eq!(response.text(), "THIRTEEN THOUSAND");
    let bodies = bodies.lock().clone();
    assert_eq!(bodies[1], with_token(&bodies[0], "checkpoint-1"));
}

#[tokio::test]
async fn fails_a_stream_dropped_before_any_checkpoint() {
    let (base, bodies) = scripted(vec![Reply::Dropped(sse(&[text("Partial", json!({}))]))]).await;
    let error = generate(request(&base)).await.unwrap_err();
    assert_eq!(error.tag(), "Transport");
    assert_eq!(bodies.lock().len(), 1);
}

#[tokio::test]
async fn does_not_resume_a_response_cut_off_by_its_own_timeout() {
    let (base, bodies) = scripted(vec![Reply::Stalled(sse(&[text(
        "THIRTEEN",
        json!({"continuationToken": "checkpoint-1"}),
    )]))])
    .await;
    let mut req = request(&base);
    req.http = Some(HttpOptions {
        timeout: Some(HttpTimeout::Millis(1_000.0)),
        ..Default::default()
    });
    let error = generate(req).await.unwrap_err();
    assert!(
        matches!(&error.kind, ErrorKind::Transport { code: Some(code), .. } if code == "Timeout"),
        "{error:?}"
    );
    assert_eq!(bodies.lock().len(), 1);
}

#[tokio::test]
async fn fails_a_continuation_without_a_token() {
    let (base, bodies) = scripted(vec![Reply::Body(sse(&[
        text("Partial", json!({})),
        continuation(None),
    ]))])
    .await;
    let error = generate(request(&base)).await.unwrap_err();
    assert_eq!(error.tag(), "InvalidProviderOutput");
    assert_eq!(
        error.message,
        "Gemini continued the response without a token"
    );
    assert_eq!(error.http.as_ref().map(|h| h.status), Some(200));
    assert_eq!(bodies.lock().len(), 1);
}

#[tokio::test]
async fn does_not_continue_a_length_finish() {
    let (base, bodies) = scripted(vec![Reply::Body(sse(&[text(
        "Partial",
        json!({"finishReason": "MAX_TOKENS", "continuationToken": "token-1"}),
    )]))])
    .await;
    let response = generate(request(&base)).await.unwrap();
    assert_eq!(response.finish_reason.normalized, FinishReason::Length);
    assert_eq!(response.finish_reason.raw.as_deref(), Some("MAX_TOKENS"));
    assert_eq!(bodies.lock().len(), 1);
}

#[tokio::test]
async fn reports_the_first_requests_input_with_all_output() {
    let usage = [
        json!({"promptTokenCount": 142, "candidatesTokenCount": 32768, "totalTokenCount": 32910}),
        json!({"promptTokenCount": 32910, "candidatesTokenCount": 32768, "totalTokenCount": 65678}),
        json!({"promptTokenCount": 65678, "cachedContentTokenCount": 32911, "candidatesTokenCount": 32768, "totalTokenCount": 98446}),
        json!({"promptTokenCount": 98446, "cachedContentTokenCount": 65679, "candidatesTokenCount": 7257, "totalTokenCount": 105703}),
    ];
    let (base, _) = scripted(vec![
        Reply::Body(sse(&[
            with_usage(text("a", json!({})), json!({"promptTokenCount": 120, "candidatesTokenCount": 10, "totalTokenCount": 130})),
            with_usage(continuation(Some("token-1")), usage[0].clone()),
        ])),
        Reply::Body(sse(&[with_usage(continuation(Some("token-2")), usage[1].clone())])),
        Reply::Body(sse(&[with_usage(continuation(Some("token-3")), usage[2].clone())])),
        Reply::Body(sse(&[with_usage(text("b", json!({"finishReason": "STOP"})), usage[3].clone())])),
    ])
    .await;
    let usage = generate(request(&base)).await.unwrap().usage.unwrap();
    assert_eq!(usage.input_tokens, Some(142));
    assert_eq!(usage.output_tokens, Some(105561));
    assert_eq!(usage.non_cached_input_tokens, Some(142));
    assert_eq!(usage.total_tokens, Some(105703));
    assert_eq!(
        usage.provider_metadata.unwrap()["google"],
        json!({
            "promptTokenCount": 197176,
            "cachedContentTokenCount": 98590,
            "candidatesTokenCount": 105561,
            "totalTokenCount": 302737,
            "requests": 4
        })
    );
}

#[test]
fn malformed_response_is_retryable() {
    let model = g::route()
        .auth(auth::none())
        .model("gemini-2.5-flash")
        .unwrap();
    let req = LLMRequest::new(model);
    for raw in ["MALFORMED_RESPONSE", "MALFORMED_FUNCTION_CALL"] {
        let body = format!("data: {{\"candidates\":[{{\"finishReason\":\"{raw}\"}}]}}\n\n");
        let error = decode(&req, body.as_bytes()).pop().unwrap().unwrap_err();
        assert_eq!(error.message, format!("Gemini stopped with {raw}"));
        assert_eq!(error.tag(), "InvalidProviderOutput");
        assert_eq!(error.is_retryable(), raw == "MALFORMED_RESPONSE", "{raw}");
    }
}
