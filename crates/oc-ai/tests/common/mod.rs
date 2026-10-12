#![allow(dead_code)]
//! Shared test helpers: the upstream "golden weather tool loop" scenario driven offline against recordings.

use oc_ai::testing::{compile_text, decode_ok, recording, Recording};
use oc_ai::*;
use serde_json::{json, Value};

pub const WEATHER: &str = "get_weather";
pub const GOLDEN_SYSTEM: &str = "Use the get_weather tool exactly once. After the tool result, reply exactly: Paris is sunny.";
pub const LOOP_SYSTEM: &str = "Use the get_weather tool, then answer in one short sentence.";

pub fn weather_tool() -> ToolDefinition {
    ToolDefinition::new(
        WEATHER,
        "Get current weather for a city.",
        json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}),
    )
}

pub fn load(path: &str) -> Recording {
    recording(&std::fs::read_to_string(format!("{}/tests/fixtures/{path}", env!("CARGO_MANIFEST_DIR"))).expect("fixture"))
}

/// `weatherToolLoopRequest`.
pub fn tool_loop_request(model: LanguageModel, system: &str, max_tokens: f64, temperature: Option<f64>) -> LLMRequest {
    let mut req = LLMRequest::new(model).system(system).prompt("What is the weather in Paris?");
    req.cache = Some(CachePolicy::none());
    req.generation = Some(GenerationOptions { max_tokens: Some(max_tokens), temperature, ..Default::default() });
    req.tools.push(ToolEntry::Tool(weather_tool()));
    req
}

pub fn json_eq(actual: &str, expected: &str) {
    let a: Value = serde_json::from_str(actual).unwrap();
    let e: Value = serde_json::from_str(expected).unwrap();
    assert_eq!(a, e, "\nactual:   {actual}\nexpected: {expected}");
    assert_eq!(actual, expected, "key order differs");
}

/// Drive the weather tool loop over a recording: each request body must equal the recorded body
/// byte-for-byte; each recorded response is decoded and fed back.
pub async fn run_loop(req: LLMRequest, rec: &Recording) -> Vec<LLMEvent> {
    run_loop_with(req, rec, true).await
}

/// Like [`run_loop`]; `exact_order = false` compares bodies as JSON values only (for recordings made
/// before upstream reordered body keys).
pub async fn run_loop_with(mut req: LLMRequest, rec: &Recording, exact_order: bool) -> Vec<LLMEvent> {
    let mut all = vec![];
    for (i, interaction) in rec.interactions.iter().enumerate() {
        let body = compile_text(&req).await.expect("compile");
        if exact_order {
            json_eq(&body, &interaction.request.body);
        } else {
            let a: Value = serde_json::from_str(&body).unwrap();
            let e: Value = serde_json::from_str(&interaction.request.body).unwrap();
            assert_eq!(a, e, "\nactual:   {body}\nexpected: {}", interaction.request.body);
        }
        let events = decode_ok(&req, &interaction.response.bytes());
        assert!(matches!(events.last(), Some(LLMEvent::Finish { .. })), "interaction {i} must finish");
        let mut state = ResponseState::new();
        for e in &events {
            state.reduce(e.clone());
        }
        let calls: Vec<(String, String)> = events
            .iter()
            .filter_map(|e| match e {
                LLMEvent::ToolCall { id, name, provider_executed: None, .. } => Some((id.clone(), name.clone())),
                _ => None,
            })
            .collect();
        all.extend(events);
        if calls.is_empty() {
            break;
        }
        req.messages.push(Message::assistant(state.message.content.clone()));
        for (id, name) in calls {
            req.messages.push(Message::tool(ToolResultPart {
                id,
                name,
                namespace: None,
                result: ToolResultValue::Json(json!({"temperature": 22, "condition": "sunny"})),
                provider_executed: None,
                cache: None,
                metadata: None,
                provider_metadata: None,
            }));
        }
    }
    all
}

pub fn text_of(events: &[LLMEvent]) -> String {
    LLMResponse::from_events(events.to_vec()).map(|r| r.text()).unwrap_or_default()
}

pub fn finish_reasons(events: &[LLMEvent]) -> Vec<FinishReason> {
    events
        .iter()
        .filter_map(|e| match e {
            LLMEvent::StepFinish { reason, .. } => Some(reason.normalized),
            _ => None,
        })
        .collect()
}

/// Generate one recorded turn: assert body equality, decode, and return the folded response.
pub async fn turn(req: &LLMRequest, rec: &Recording, index: usize) -> LLMResponse {
    let body = compile_text(req).await.expect("compile");
    json_eq(&body, &rec.interactions[index].request.body);
    let events = decode_ok(req, &rec.interactions[index].response.bytes());
    LLMResponse::from_events(events).expect("terminal event")
}

pub fn tool_result(id: &str, name: &str, value: Value) -> Message {
    Message::tool(ToolResultPart {
        id: id.into(),
        name: name.into(),
        namespace: None,
        result: ToolResultValue::Json(value),
        provider_executed: None,
        cache: None,
        metadata: None,
        provider_metadata: None,
    })
}

pub fn tool_calls(response: &LLMResponse) -> Vec<(String, String, Value)> {
    response
        .events
        .iter()
        .filter_map(|e| match e {
            LLMEvent::ToolCall { id, name, input, .. } => Some((id.clone(), name.clone(), input.clone())),
            _ => None,
        })
        .collect()
}
