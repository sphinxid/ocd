//! Google Interactions response continuation (opencode 2.4.1 #65006, upstream
//! `test/provider/google-interactions.test.ts`, "Google Interactions response continuation"), plus the Interactions
//! safety settings (#65036) and Office documents (#65037).

use std::sync::Arc;

use futures::StreamExt;
use oc_ai::protocols::google_interactions as gi;
use oc_ai::route::auth;
use oc_ai::testing::compile_body;
use oc_ai::*;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// One scripted answer: a whole SSE body, a body whose connection drops after it, a body that stalls after it, or
/// an HTTP error.
enum Reply {
    Body(String),
    Dropped(String),
    Stalled(String),
    Status(u16, Value),
}

/// Serves one scripted reply per request and records every sent body.
async fn scripted(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1beta", listener.local_addr().unwrap());
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = bodies.clone();
    tokio::spawn(async move {
        for reply in replies {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let body = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..end]).to_string();
                    let length = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + length {
                        break buf[end + 4..end + 4 + length].to_vec();
                    }
                }
                assert!(n > 0);
            };
            seen.lock().push(serde_json::from_slice(&body).unwrap());
            let (status, kind, text, declared) = match &reply {
                Reply::Body(text) => (200, "text/event-stream", text.clone(), text.len()),
                // Promise more bytes than are sent, so the read fails as a dropped connection.
                Reply::Dropped(text) | Reply::Stalled(text) => (200, "text/event-stream", text.clone(), text.len() + 100),
                Reply::Status(status, value) => (*status, "application/json", value.to_string(), value.to_string().len()),
            };
            let head = format!("HTTP/1.1 {status} X\r\ncontent-type: {kind}\r\ncontent-length: {declared}\r\nconnection: close\r\n\r\n");
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

fn start_text(index: i64) -> Value {
    json!({"event_type": "step.start", "index": index, "step": {"type": "model_output"}})
}

fn text(index: i64, value: &str) -> Value {
    json!({"event_type": "step.delta", "index": index, "delta": {"type": "text", "text": value}})
}

fn checkpoint(token: &str, usage: Option<Value>) -> Value {
    let mut event = json!({"event_type": "interaction.status_update", "status": "in_progress", "continuation_token": token});
    if let Some(usage) = usage {
        event["usage"] = usage;
    }
    event
}

fn completed(interaction: Value) -> Value {
    let mut out = json!({"status": "completed"});
    for (k, v) in interaction.as_object().unwrap() {
        out[k] = v.clone();
    }
    json!({"event_type": "interaction.completed", "interaction": out})
}

fn continue_with(token: &str) -> Reply {
    Reply::Body(sse(&[completed(json!({"status": "requires_continuation", "continuation_token": token}))]))
}

fn request(base: &str) -> LLMRequest {
    let model = gi::route().base_url(Some(base.into())).auth(auth::none()).model("gemini-3.8-flash").unwrap();
    let mut req = LLMRequest::new(model).prompt("Count.").tool(ToolDefinition::new("lookup", "Lookup", json!({"type": "object"})));
    req.generation = Some(GenerationOptions { max_tokens: Some(66000.0), ..Default::default() });
    req
}

async fn generate(req: LLMRequest) -> Result<LLMResponse, AIError> {
    Client::new().generate(req, Default::default()).await
}

/// The text streamed before the stream failed, and the failure.
async fn partial(req: LLMRequest) -> (String, AIError) {
    let mut stream = Client::new().stream(req, CancellationToken::new()).await.unwrap();
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(LLMEvent::TextDelta { text: delta, .. }) => text.push_str(&delta),
            Ok(_) => {}
            Err(e) => return (text, e),
        }
    }
    panic!("the stream did not fail")
}

fn with_token(body: &Value, token: &str) -> Value {
    let mut out = body.clone();
    out["continuation_token"] = json!(token);
    out
}

fn tokens(bodies: &Arc<Mutex<Vec<Value>>>) -> Vec<Option<String>> {
    bodies.lock().iter().map(|b| b.get("continuation_token").and_then(Value::as_str).map(String::from)).collect()
}

fn incomplete(error: &AIError) -> bool {
    matches!(&error.kind, ErrorKind::InvalidProviderOutput { classification: Some(c), .. } if c == "incomplete-stream")
}

#[tokio::test]
async fn joins_two_hops_into_one_response_with_signatures_tool_calls_and_combined_usage() {
    let (base, bodies) = scripted(vec![
        Reply::Body(sse(&[
            json!({"event_type": "step.start", "index": 0, "step": {"type": "thought", "summary": [{"type": "text", "text": "Plan."}], "signature": "signed-thought"}}),
            json!({"event_type": "step.stop", "index": 0}),
            start_text(1),
            text(1, "One"),
            checkpoint("older-checkpoint", Some(json!({"total_input_tokens": 7, "total_output_tokens": 1}))),
            text(1, ", two"),
            completed(json!({
                "status": "requires_continuation",
                "continuation_token": "next-hop",
                "usage": {"total_input_tokens": 10, "total_cached_tokens": 3, "total_output_tokens": 3, "total_thought_tokens": 4, "total_tokens": 17}
            })),
        ])),
        Reply::Body(sse(&[
            start_text(1),
            text(1, ", three."),
            json!({"event_type": "step.stop", "index": 1}),
            json!({"event_type": "step.start", "index": 2, "step": {"type": "function_call", "id": "call_1", "name": "lookup", "arguments": {"count": 3}}}),
            json!({"event_type": "step.delta", "index": 2, "delta": {"type": "thought_signature", "signature": "signed-tool"}}),
            json!({"event_type": "step.stop", "index": 2}),
            completed(json!({
                "id": "final-hop",
                "usage": {"total_input_tokens": 17, "total_cached_tokens": 10, "total_output_tokens": 4, "total_thought_tokens": 2, "total_tokens": 23}
            })),
        ])),
    ])
    .await;
    let result = generate(request(&base)).await.unwrap();

    let bodies = bodies.lock().clone();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["store"], json!(false));
    assert_eq!(bodies[0]["generation_config"]["max_output_tokens"], json!(66000));
    assert_eq!(bodies[1], with_token(&bodies[0], "next-hop"));
    assert_eq!(result.text(), "One, two, three.");
    assert_eq!(result.reasoning(), "Plan.");
    let reasoning_end = result.events.iter().find_map(|e| match e {
        LLMEvent::ReasoningEnd { provider_metadata, .. } => provider_metadata.clone(),
        _ => None,
    });
    assert_eq!(reasoning_end.unwrap()["google"], json!({"interactionSignature": "signed-thought"}));
    let calls: Vec<(String, Value, Value)> = result
        .events
        .iter()
        .filter_map(|e| match e {
            LLMEvent::ToolCall { id, input, provider_metadata, .. } => {
                Some((id.clone(), input.clone(), provider_metadata.as_ref().map(|m| m["google"].clone()).unwrap_or_default()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(calls, vec![("call_1".to_string(), json!({"count": 3}), json!({"interactionSignature": "signed-tool"}))]);
    assert_eq!(result.events.iter().filter(|e| e.type_name() == "finish").count(), 1);
    assert_eq!(result.events.iter().filter(|e| e.type_name() == "step-start").count(), 1);
    assert_eq!(result.finish_reason.normalized, FinishReason::ToolCalls);
    assert_eq!(result.finish_reason.raw.as_deref(), Some("completed"));
    let finish_metadata = result.events.iter().find_map(|e| match e {
        LLMEvent::Finish { provider_metadata, .. } => provider_metadata.clone(),
        _ => None,
    });
    assert_eq!(finish_metadata.unwrap()["google"], json!({"interactionId": "final-hop"}));
    let usage = result.usage.unwrap();
    assert_eq!(usage.input_tokens, Some(10));
    assert_eq!(usage.non_cached_input_tokens, Some(7));
    assert_eq!(usage.cache_read_input_tokens, Some(3));
    assert_eq!(usage.output_tokens, Some(13));
    assert_eq!(usage.reasoning_tokens, Some(6));
    assert_eq!(usage.total_tokens, Some(23));
    assert_eq!(
        usage.provider_metadata.unwrap()["google"],
        json!({"total_input_tokens": 27, "total_cached_tokens": 13, "total_output_tokens": 7, "total_thought_tokens": 6, "total_tokens": 40, "requests": 2})
    );
    // The second request's blocks are numbered under its hop.
    let ids: Vec<String> = result
        .events
        .iter()
        .filter_map(|e| match e {
            LLMEvent::TextStart { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, ["1", "1:1"]);
}

#[tokio::test]
async fn resumes_a_broken_connection_at_a_checkpoint_with_no_later_output() {
    let frames = sse(&[
        start_text(0),
        text(0, "One"),
        checkpoint("resume", Some(json!({"total_input_tokens": 10, "total_output_tokens": 1}))),
        json!({"event_type": "step.stop", "index": 0}),
    ]);
    let (base, bodies) = scripted(vec![
        Reply::Dropped(frames),
        Reply::Body(sse(&[start_text(0), text(0, ", two."), completed(json!({"usage": {"total_input_tokens": 11, "total_output_tokens": 2}}))])),
    ])
    .await;
    let result = generate(request(&base)).await.unwrap();
    let bodies = bodies.lock().clone();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[1], with_token(&bodies[0], "resume"));
    assert_eq!(result.text(), "One, two.");
    let usage = result.usage.unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (Some(10), Some(3)));
}

#[tokio::test]
async fn does_not_replay_visible_output_emitted_after_a_checkpoint() {
    let (base, bodies) =
        scripted(vec![Reply::Body(sse(&[start_text(0), text(0, "One"), checkpoint("stale", None), text(0, ", two"), checkpoint("stale", None)]))])
            .await;
    let (text, error) = partial(request(&base)).await;
    assert_eq!(bodies.lock().len(), 1);
    assert_eq!(text, "One, two");
    assert!(incomplete(&error), "{error:?}");
    assert!(error.is_retryable());
}

#[tokio::test]
async fn does_not_resume_a_tool_call_whose_arguments_or_completion_can_be_replayed() {
    let (base, bodies) = scripted(vec![Reply::Body(sse(&[
        json!({"event_type": "step.start", "index": 0, "step": {"type": "function_call", "id": "call_1", "name": "lookup", "arguments": {}}}),
        checkpoint("stale", None),
        json!({"event_type": "step.delta", "index": 0, "delta": {"type": "arguments_delta", "arguments": "{}"}}),
    ]))])
    .await;
    let error = generate(request(&base)).await.unwrap_err();
    assert_eq!(bodies.lock().len(), 1);
    assert!(incomplete(&error), "{error:?}");
}

#[tokio::test]
async fn continues_from_a_status_update_owned_by_a_stored_interaction() {
    let mut update = checkpoint("stored-token", None);
    update["interaction_id"] = json!("hop-owner");
    update["status"] = json!("requires_continuation");
    let (base, bodies) = scripted(vec![
        Reply::Body(sse(&[update, completed(json!({"status": "requires_continuation"}))])),
        Reply::Body(sse(&[completed(json!({}))])),
    ])
    .await;
    generate(request(&base).provider_option("store", json!(true))).await.unwrap();
    let bodies = bodies.lock().clone();
    assert_eq!(bodies.len(), 2);
    assert!(bodies[1].get("input").is_none());
    assert_eq!(bodies[1]["store"], json!(true));
    assert_eq!(bodies[1]["continuation_token"], json!("stored-token"));
    assert_eq!(bodies[1]["previous_interaction_id"], json!("hop-owner"));
    // Upstream's key order (checked against the 2.4.1 source): the owner takes the slot `fromRequest` keeps for
    // `previous_interaction_id`, the token goes last.
    let keys: Vec<&str> = bodies[1].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["model", "stream", "store", "previous_interaction_id", "tools", "generation_config", "continuation_token"]);
    let keys: Vec<&str> = bodies[0].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["model", "input", "stream", "store", "tools", "generation_config"]);
}

#[tokio::test]
async fn cancels_a_continued_response_without_reconnecting_from_its_checkpoint() {
    let (base, bodies) = scripted(vec![
        Reply::Body(sse(&[completed(json!({"status": "requires_continuation", "continuation_token": "next-hop"}))])),
        Reply::Stalled(sse(&[checkpoint("resume", None)])),
        Reply::Body(sse(&[completed(json!({}))])),
    ])
    .await;
    let cancel = CancellationToken::new();
    let mut stream = Client::new().stream(request(&base), cancel.clone()).await.unwrap();
    let reader = tokio::spawn(async move { while stream.next().await.is_some() {} });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while bodies.lock().len() < 2 {
        assert!(tokio::time::Instant::now() < deadline, "the continuation request was not sent");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cancel.cancel();
    reader.await.unwrap();
    assert_eq!(bodies.lock().len(), 2);
}

#[tokio::test]
async fn stops_when_a_completed_hop_returns_its_own_start_token() {
    let (base, bodies) = scripted(vec![continue_with("repeat"), continue_with("repeat"), Reply::Body(sse(&[completed(json!({}))]))]).await;
    let error = generate(request(&base)).await.unwrap_err();
    assert_eq!(tokens(&bodies), [None, Some("repeat".to_string())]);
    assert!(incomplete(&error), "{error:?}");
}

#[tokio::test]
async fn reconnects_three_times_without_progress_and_surfaces_the_last_disconnect() {
    let (base, bodies) = scripted(vec![
        continue_with("repeat"),
        Reply::Dropped(String::new()),
        Reply::Body(String::new()),
        Reply::Dropped(String::new()),
        Reply::Body(String::new()),
        Reply::Body(sse(&[completed(json!({}))])),
    ])
    .await;
    let error = generate(request(&base)).await.unwrap_err();
    let repeat = Some("repeat".to_string());
    assert_eq!(tokens(&bodies), [None, repeat.clone(), repeat.clone(), repeat.clone(), repeat]);
    assert!(incomplete(&error), "{error:?}");
    assert!(error.is_retryable());
}

#[tokio::test]
async fn resets_the_reconnect_budget_after_a_new_checkpoint() {
    let empty = || Reply::Body(String::new());
    let (base, bodies) = scripted(vec![
        continue_with("first"),
        empty(),
        empty(),
        empty(),
        Reply::Body(sse(&[checkpoint("new", None)])),
        empty(),
        empty(),
        empty(),
        Reply::Body(sse(&[completed(json!({}))])),
    ])
    .await;
    let result = generate(request(&base)).await.unwrap();
    let (first, new) = (Some("first".to_string()), Some("new".to_string()));
    assert_eq!(
        tokens(&bodies),
        [None, first.clone(), first.clone(), first.clone(), first, new.clone(), new.clone(), new.clone(), new]
    );
    assert_eq!(result.finish_reason.normalized, FinishReason::Stop);
}

#[tokio::test]
async fn does_not_resume_when_a_continuation_request_gets_an_http_error() {
    let (base, bodies) = scripted(vec![
        continue_with("next-hop"),
        Reply::Status(429, json!({"error": {"message": "Too many requests"}})),
        Reply::Body(sse(&[completed(json!({}))])),
    ])
    .await;
    let error = generate(request(&base)).await.unwrap_err();
    assert_eq!(tokens(&bodies), [None, Some("next-hop".to_string())]);
    assert_eq!(error.tag(), "RateLimit");
    assert_eq!(error.http.as_ref().map(|h| h.status), Some(429));
    assert!(error.is_retryable());
}

/// "sends Gemini safety settings in the Interactions request"
#[tokio::test]
async fn sends_gemini_safety_settings_in_the_interactions_request() {
    let req = request("https://generativelanguage.test/v1beta").provider_option(
        "safetySettings",
        json!([
            {"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_LOW_AND_ABOVE"},
            {"category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "BLOCK_ONLY_HIGH"}
        ]),
    );
    let body = compile_body(&req).await.unwrap();
    assert_eq!(
        body["safety_settings"],
        json!([{"type": "harassment", "threshold": "block_low_and_above"}, {"type": "hate_speech", "threshold": "block_only_high"}])
    );
    assert!(body.get("safetySettings").is_none());
}

/// "lowers DOCX as a document and rejects an undocumented Office MIME type"
#[tokio::test]
async fn lowers_docx_as_a_document_and_rejects_an_undocumented_office_mime_type() {
    let docx = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
    let docm = "application/vnd.ms-word.document.macroenabled.12";
    let with = |mime: &str| {
        let model = gi::route().base_url(Some("https://generativelanguage.test/v1beta".into())).auth(auth::none()).model("gemini-3.8-flash").unwrap();
        let mut req = LLMRequest::new(model);
        req.messages = serde_json::from_value(json!([{"role": "user", "content": [
            {"type": "media", "media": {"source": {"type": "base64", "data": "UEsDBA==", "mediaType": mime}}}
        ]}]))
        .unwrap();
        req
    };
    let body = compile_body(&with(docx)).await.unwrap();
    assert_eq!(body["input"], json!([{"type": "user_input", "content": [{"type": "document", "data": "UEsDBA==", "mime_type": docx}]}]));
    let error = compile_body(&with(docm)).await.unwrap_err();
    assert_eq!(error.tag(), "InvalidRequest");
    assert_eq!(error.message, format!("Google Interactions does not support {docm} document input"));
}
