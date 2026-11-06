//! session-runner-message.test.ts (opencode 2.2.5, #60498): errored or interrupted reasoning is dropped rather
//! than lowered to text, and readable reasoning after a model switch stays reasoning.

use oc_ai::types::{ContentPart, Role};
use oc_core::lower::to_llm_messages;
use oc_schema::types::ModelRef;
use oc_schema::Message as SessionMessage;
use serde_json::{json, Value};

fn model(provider: &str, id: &str) -> ModelRef {
    ModelRef { id: id.into(), provider_id: provider.into(), variant: None }
}

fn assistant(content: Value, error: Option<Value>) -> SessionMessage {
    let mut m = json!({
        "type": "assistant",
        "id": "msg_1",
        "time": {"created": 1, "completed": 2},
        "agent": "build",
        "model": {"id": "claude", "providerID": "anthropic"},
        "content": content,
    });
    if let Some(e) = error {
        m["error"] = e;
    }
    serde_json::from_value(m).expect("assistant")
}

fn reasoning(text: &str, completed: bool) -> Value {
    let mut r = json!({"type": "reasoning", "text": text, "native": {"signature": "sig"}, "time": {"created": 1}});
    if completed {
        r["time"]["completed"] = json!(2);
    }
    r
}

#[test]
fn drops_failed_assistant_reasoning() {
    let msg = assistant(
        json!([reasoning("Partial thought", true), {"type": "text", "text": "Answer"}]),
        Some(json!({"type": "unknown", "message": "boom"})),
    );
    let out = to_llm_messages(&[msg], &model("anthropic", "claude"), Some("anthropic"));
    assert_eq!(out.len(), 1);
    assert!(matches!(&out[0].content[..], [ContentPart::Text(t)] if t.text == "Answer"), "{:?}", out[0].content);
}

#[test]
fn omits_unfinished_reasoning_from_an_interrupted_assistant_message() {
    let msg = assistant(json!([reasoning("Completed summary part", false)]), None);
    assert!(to_llm_messages(&[msg], &model("anthropic", "claude"), Some("anthropic")).is_empty());
}

#[test]
fn keeps_readable_reasoning_after_a_model_switch_without_metadata() {
    let msg = assistant(json!([reasoning("Thought", true), {"type": "text", "text": "Answer"}]), None);
    let out = to_llm_messages(&[msg], &model("openai", "gpt-5"), Some("openai"));
    assert_eq!(out[0].role, Role::Assistant);
    match &out[0].content[0] {
        ContentPart::Reasoning(r) => {
            assert_eq!(r.text, "Thought");
            assert!(r.provider_metadata.is_none());
        }
        other => panic!("{other:?}"),
    }
    // The same model replays it with its provider metadata.
    let msg = assistant(json!([reasoning("Thought", true)]), None);
    let out = to_llm_messages(&[msg], &model("anthropic", "claude"), Some("anthropic"));
    match &out[0].content[0] {
        ContentPart::Reasoning(r) => assert!(r.provider_metadata.is_some()),
        other => panic!("{other:?}"),
    }
}
