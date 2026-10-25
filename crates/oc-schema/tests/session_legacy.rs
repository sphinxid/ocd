//! `packages/schema/test/session-legacy.test.ts`: pre-2.1 payloads decode into the current shapes.

use oc_schema::event::SessionEvent;
use oc_schema::legacy;
use oc_schema::message::{AssistantContent, CompactionStatus, ToolState};
use oc_schema::Message;
use serde_json::{json, Map, Value};

fn native() -> Value {
    json!({ "signature": "kept" })
}

fn tool_part() -> Value {
    json!({
        "type": "tool",
        "id": "call",
        "name": "search",
        "providerState": native(),
        "providerResultState": { "result": "kept" },
        "state": { "status": "streaming", "input": "" },
        "time": { "created": 0 },
    })
}

fn with_ref(extra: Value) -> Value {
    let mut base = json!({ "sessionID": "ses_legacy", "assistantMessageID": "msg_legacy" });
    base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    base
}

fn tokens() -> Value {
    json!({ "input": 0, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } })
}

/// Decodes the payload and re-encodes it, as the current `native` shape.
fn decode(kind: &str, data: Value) -> Value {
    SessionEvent::from_parts(kind, data).unwrap_or_else(|e| panic!("{kind}: {e}")).data()
}

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

#[test]
fn upgrades_renamed_event_fields() {
    let cases = [
        ("session.text.ended", with_ref(json!({ "ordinal": 0, "text": "hi", "state": native() }))),
        ("session.reasoning.started", with_ref(json!({ "ordinal": 0, "state": native() }))),
        ("session.reasoning.ended", with_ref(json!({ "ordinal": 0, "text": "thought", "state": native() }))),
        ("session.tool.called", with_ref(json!({ "id": "call", "input": {}, "executed": true, "state": native() }))),
        ("session.step.ended", with_ref(json!({ "finish": "stop", "providerState": native(), "cost": 0, "tokens": tokens() }))),
        (
            "session.step.failed",
            with_ref(json!({ "error": { "type": "provider.unknown", "message": "failed" }, "providerState": native() })),
        ),
        (
            "session.compaction.ended",
            json!({ "sessionID": "ses_legacy", "reason": "auto", "text": "s", "recent": "", "providerState": native() }),
        ),
    ];
    for (kind, data) in cases {
        assert_eq!(decode(kind, data)["native"], native(), "{kind}");
    }
}

#[test]
fn upgrades_result_native_data() {
    let cases = [
        ("session.tool.success", with_ref(json!({ "id": "call", "content": [{ "type": "text", "text": "done" }], "executed": true }))),
        (
            "session.tool.failed",
            with_ref(json!({ "id": "call", "error": { "type": "tool.execution", "message": "failed" }, "executed": true })),
        ),
    ];
    for (kind, mut data) in cases {
        data["resultState"] = native();
        let decoded = decode(kind, data);
        assert_eq!(decoded["native"], native(), "{kind}");
        assert!(decoded.get("resultState").is_none());
    }
}

#[test]
fn upgrades_replaced_assistant_content_without_touching_tool_status() {
    let decoded = decode(
        "session.message.content.updated",
        json!({
            "sessionID": "ses_legacy",
            "messageID": "msg_legacy",
            "content": [{ "type": "text", "text": "hi", "state": native() }, tool_part()],
        }),
    );
    assert_eq!(decoded["content"][0]["native"], native());
    assert_eq!(decoded["content"][1]["native"], json!({ "signature": "kept", "result": "kept" }));
    assert_eq!(decoded["content"][1]["state"], json!({ "status": "streaming", "input": "" }));
}

#[test]
fn leaves_current_and_unrelated_payloads_unchanged() {
    let current = with_ref(json!({ "ordinal": 0, "text": "hi", "native": native() }));
    assert_eq!(legacy::event("session.text.ended", current.clone()), current);
    let success = with_ref(json!({ "id": "call", "content": [], "executed": true, "native": native() }));
    assert_eq!(legacy::event("session.tool.success", success.clone()), success);
    let user = json!({ "type": "user", "text": "hi", "state": native() });
    assert_eq!(Value::Object(legacy::message("user", obj(user.clone()))), user);
}

#[test]
fn keeps_the_current_name_when_a_payload_carries_both() {
    assert_eq!(
        legacy::event("session.text.ended", json!({ "text": "hi", "state": { "old": true }, "native": native() })),
        json!({ "text": "hi", "native": native() })
    );
}

#[test]
fn merges_legacy_tool_result_data_over_call_data() {
    let mut part = tool_part();
    part["native"] = json!({ "itemId": "shared", "call": true });
    part["providerResultState"] = json!({ "itemId": "shared", "result": true });
    let upgraded = legacy::message("assistant", obj(json!({ "content": [part] })));
    assert_eq!(upgraded["content"][0]["native"], json!({ "itemId": "shared", "call": true, "result": true }));
    assert_eq!(upgraded["content"][0]["state"], json!({ "status": "streaming", "input": "" }));
}

#[test]
fn upgrades_assistant_and_compaction_messages() {
    let upgraded = legacy::message(
        "assistant",
        obj(json!({ "providerState": native(), "content": [{ "type": "text", "text": "hi", "state": native() }, tool_part()] })),
    );
    assert_eq!(
        Value::Object(upgraded),
        json!({
            "native": native(),
            "content": [
                { "type": "text", "text": "hi", "native": native() },
                {
                    "type": "tool",
                    "id": "call",
                    "name": "search",
                    "state": { "status": "streaming", "input": "" },
                    "time": { "created": 0 },
                    "native": { "signature": "kept", "result": "kept" },
                },
            ],
        })
    );
    assert_eq!(
        Value::Object(legacy::message("compaction", obj(json!({ "status": "completed", "providerState": native() })))),
        json!({ "status": "completed", "native": native() })
    );
}

#[test]
fn moves_the_kept_text_a_compaction_stored_in_recent_into_metadata() {
    assert_eq!(
        Value::Object(legacy::message(
            "compaction",
            obj(json!({ "status": "completed", "recent": "[User]: kept", "metadata": { "source": "x" } }))
        )),
        json!({ "status": "completed", "metadata": { "recent": "[User]: kept", "source": "x" } })
    );
    assert_eq!(
        Value::Object(legacy::message("compaction", obj(json!({ "status": "completed", "recent": "" })))),
        json!({ "status": "completed" })
    );
    assert_eq!(
        legacy::event("session.compaction.ended", json!({ "reason": "auto", "text": "s", "recent": "[User]: kept" })),
        json!({ "reason": "auto", "text": "s" })
    );
    assert_eq!(legacy::event("session.compaction.started", json!({ "reason": "auto", "recent": "" })), json!({ "reason": "auto" }));
}

#[test]
fn moves_a_provider_compactions_window_from_provider_context_into_native() {
    let messages = json!([{ "role": "assistant", "content": [{ "type": "compaction", "provider": "openai", "encrypted": "opaque" }] }]);
    let provider_context = json!({ "version": 1, "provenance": { "providerID": "openai" }, "messages": messages });
    assert_eq!(
        Value::Object(legacy::message(
            "compaction",
            obj(json!({ "status": "completed", "summary": "", "native": { "step": true }, "providerContext": provider_context }))
        )),
        json!({ "status": "completed", "summary": "", "native": { "messages": messages } })
    );
    assert_eq!(
        legacy::event("session.compaction.ended", json!({ "reason": "auto", "text": "", "providerContext": provider_context })),
        json!({ "reason": "auto", "text": "", "native": { "messages": messages } })
    );
}

#[test]
fn upgrades_exported_messages_before_decoding() {
    let message = json!({
        "id": "msg_legacy",
        "type": "assistant",
        "agent": "build",
        "model": { "providerID": "anthropic", "id": "claude" },
        "providerState": native(),
        "content": [{ "type": "text", "text": "hi", "state": native() }, { "type": "reasoning", "text": "thought", "state": native() }],
        "time": { "created": 0 },
    });
    let decoded: Vec<Message> = legacy::messages(vec![message])
        .into_iter()
        .map(|m| serde_json::from_value(m).unwrap())
        .collect();
    let Message::Assistant(a) = &decoded[0] else { panic!("assistant") };
    assert_eq!(a.native.as_ref().map(|n| Value::Object(n.clone())), Some(native()));
    assert!(matches!(&a.content[0], AssistantContent::Text { native: Some(n), .. } if Value::Object(n.clone()) == native()));
    assert!(matches!(&a.content[1], AssistantContent::Reasoning { native: Some(n), .. } if Value::Object(n.clone()) == native()));
}

/// ocd shares `opencode.db` with upstream: rows written by 2.0.x and by 2.1.x both decode, and encoding
/// writes only the current shape.
#[test]
fn decodes_old_and_new_rows_and_writes_only_the_new_shape() {
    let old = json!({
        "time": { "created": 1 },
        "agent": "build",
        "model": { "providerID": "openai", "id": "gpt" },
        "providerState": { "step": 1 },
        "content": [
            { "type": "text", "text": "hi", "state": { "sig": 1 } },
            {
                "type": "tool", "id": "call", "name": "read",
                "providerState": { "call": 1 }, "providerResultState": { "result": 1 },
                "state": { "status": "completed", "input": {}, "content": [{ "type": "text", "text": "ok" }] },
                "time": { "created": 1 },
            },
        ],
    });
    let new = json!({
        "time": { "created": 1 },
        "agent": "build",
        "model": { "providerID": "openai", "id": "gpt" },
        "native": { "step": 1 },
        "content": [
            { "type": "text", "text": "hi", "native": { "sig": 1 } },
            {
                "type": "tool", "id": "call", "name": "read",
                "native": { "call": 1, "result": 1 },
                "state": { "status": "completed", "input": {}, "content": [{ "type": "text", "text": "ok" }] },
                "time": { "created": 1 },
            },
        ],
    });
    let from_old = Message::from_row("msg_1", "assistant", &old.to_string()).unwrap();
    let from_new = Message::from_row("msg_1", "assistant", &new.to_string()).unwrap();
    assert_eq!(from_old, from_new);
    let written: Value = serde_json::from_str(&from_old.to_row_data()).unwrap();
    assert_eq!(written, new);
    let Message::Assistant(a) = &from_old else { panic!("assistant") };
    assert!(matches!(&a.content[1], AssistantContent::Tool { state: ToolState::Completed { .. }, .. }));

    let old_compaction = json!({
        "time": { "created": 2 }, "status": "completed", "reason": "auto", "summary": "s", "recent": "[User]: kept",
    });
    let decoded = Message::from_row("msg_2", "compaction", &old_compaction.to_string()).unwrap();
    let written: Value = serde_json::from_str(&decoded.to_row_data()).unwrap();
    assert_eq!(
        written,
        json!({ "metadata": { "recent": "[User]: kept" }, "time": { "created": 2 }, "status": "completed", "reason": "auto", "summary": "s" })
    );
    let Message::Compaction(c) = decoded else { panic!("compaction") };
    assert!(matches!(c.status, CompactionStatus::Completed { native: None, .. }));
}
