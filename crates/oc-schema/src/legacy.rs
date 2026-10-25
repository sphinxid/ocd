//! Storage and wire compatibility for data written before v2.1 (`schema/src/session-legacy.ts`).
//!
//! Native data was then named `state` or `providerState`, a tool's result native data `resultState` or
//! `providerResultState`, summary compactions stored the text of the messages they kept in `recent`, and
//! provider compactions stored their replacement window in `providerContext`. Stored events, projected
//! messages, and exported transcripts keep whichever shape they were written with, so these upgrade them
//! before decoding. Every function is a no-op on data already in the current shape.

use serde_json::{Map, Value};

/// Durable events whose native data only changed name.
const RENAMES: [(&str, &str, &str); 8] = [
    ("session.text.ended", "state", "native"),
    ("session.reasoning.started", "state", "native"),
    ("session.reasoning.ended", "state", "native"),
    ("session.tool.called", "state", "native"),
    ("session.step.ended", "providerState", "native"),
    ("session.step.failed", "providerState", "native"),
    ("session.tool.success", "resultState", "native"),
    ("session.tool.failed", "resultState", "native"),
];

/// `SessionLegacy.event`: upgrades a durable event payload by event type.
pub fn event(kind: &str, data: Value) -> Value {
    let Value::Object(data) = data else { return data };
    let data = match kind {
        "session.message.content.updated" => content(data),
        // Messages carry a legacy compaction's kept text; its events have no reader for it.
        "session.compaction.started" => omit(data, "recent"),
        "session.compaction.ended" => provider_context(omit(rename(data, "providerState", "native"), "recent")),
        _ => match RENAMES.iter().find(|(k, _, _)| *k == kind) {
            Some((_, from, to)) => rename(data, from, to),
            None => data,
        },
    };
    Value::Object(data)
}

/// `SessionLegacy.message`: upgrades a projected message's data by message type.
pub fn message(kind: &str, data: Map<String, Value>) -> Map<String, Value> {
    match kind {
        "assistant" => content(rename(data, "providerState", "native")),
        "compaction" => provider_context(recent(rename(data, "providerState", "native"))),
        _ => data,
    }
}

/// `SessionLegacy.messages`: upgrades complete messages, such as those in an exported session, before decoding.
pub fn messages(values: Vec<Value>) -> Vec<Value> {
    values
        .into_iter()
        .map(|value| match value {
            Value::Object(m) => {
                let kind = m.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
                Value::Object(message(&kind, m))
            }
            other => other,
        })
        .collect()
}

fn content(mut data: Map<String, Value>) -> Map<String, Value> {
    if let Some(Value::Array(items)) = data.get_mut("content") {
        for item in items.iter_mut() {
            *item = part(std::mem::take(item));
        }
    }
    data
}

fn part(value: Value) -> Value {
    let Value::Object(m) = value else { return value };
    let m = match m.get("type").and_then(Value::as_str) {
        Some("text" | "reasoning") => rename(m, "state", "native"),
        // A tool part's own `state` is its lifecycle status, not native data.
        Some("tool") => tool_result(rename(m, "providerState", "native")),
        _ => m,
    };
    Value::Object(m)
}

/// Kept text a summary compaction stored before it referenced `metadata.keepID`; replay still reads it.
fn recent(data: Map<String, Value>) -> Map<String, Value> {
    let text = data.get("recent").cloned();
    let mut rest = omit(data, "recent");
    let Some(Value::String(text)) = text.filter(|t| t.as_str().is_some_and(|s| !s.is_empty())) else { return rest };
    let metadata = match rest.get("metadata") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    let mut merged = Map::new();
    merged.insert("recent".into(), Value::String(text));
    merged.extend(metadata);
    rest.insert("metadata".into(), Value::Object(merged));
    rest
}

/// The window moves to `native.messages`; the `native` stored beside it was never replayed.
fn provider_context(data: Map<String, Value>) -> Map<String, Value> {
    let context = data.get("providerContext").cloned();
    let mut rest = omit(data, "providerContext");
    let Some(Value::Object(context)) = context else { return rest };
    let messages = match context.get("messages") {
        Some(Value::Array(list)) => Some(Value::Array(list.iter().cloned().map(media).collect())),
        other => other.cloned(),
    };
    let mut native = Map::new();
    if let Some(messages) = messages {
        native.insert("messages".into(), messages);
    }
    rest.insert("native".into(), Value::Object(native));
    rest
}

/// Before 2.0.15, window media parts stored `mediaType` and base64 `data` directly.
fn media(message: Value) -> Value {
    let Value::Object(mut message) = message else { return message };
    if let Some(Value::Array(parts)) = message.get_mut("content") {
        for part in parts.iter_mut() {
            let Value::Object(p) = part else { continue };
            if p.get("type").and_then(Value::as_str) != Some("media") || p.contains_key("media") {
                continue;
            }
            let Some(Value::String(data)) = p.get("data").cloned() else { continue };
            let media_type = p.shift_remove("mediaType");
            p.shift_remove("data");
            let mut source = Map::new();
            source.insert("type".into(), Value::String("base64".into()));
            source.insert("data".into(), Value::String(data));
            if let Some(t) = media_type {
                source.insert("mediaType".into(), t);
            }
            let mut wrapped = Map::new();
            wrapped.insert("source".into(), Value::Object(source));
            p.insert("media".into(), Value::Object(wrapped));
        }
    }
    Value::Object(message)
}

fn omit(mut value: Map<String, Value>, key: &str) -> Map<String, Value> {
    value.shift_remove(key);
    value
}

/// Result data now merges into the tool's single `native`, as projection does for current result events.
fn tool_result(mut value: Map<String, Value>) -> Map<String, Value> {
    let Some(result) = value.shift_remove("providerResultState") else { return value };
    let Value::Object(result) = result else { return value };
    let mut native = match value.get("native") {
        Some(Value::Object(n)) => n.clone(),
        _ => Map::new(),
    };
    native.extend(result);
    value.insert("native".into(), Value::Object(native));
    value
}

/// The current name wins when a payload carries both.
fn rename(mut value: Map<String, Value>, from: &str, to: &str) -> Map<String, Value> {
    let Some(moved) = value.shift_remove(from) else { return value };
    if !value.contains_key(to) {
        value.insert(to.into(), moved);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn renames_event_native_data() {
        for (kind, from) in [
            ("session.text.ended", "state"),
            ("session.reasoning.started", "state"),
            ("session.reasoning.ended", "state"),
            ("session.tool.called", "state"),
            ("session.step.ended", "providerState"),
            ("session.step.failed", "providerState"),
            ("session.tool.success", "resultState"),
            ("session.tool.failed", "resultState"),
        ] {
            let upgraded = event(kind, json!({ "sessionID": "ses_1", from: { "id": "x" } }));
            assert_eq!(upgraded, json!({ "sessionID": "ses_1", "native": { "id": "x" } }), "{kind}");
        }
    }

    #[test]
    fn current_name_wins() {
        let upgraded = event("session.text.ended", json!({ "state": { "old": true }, "native": { "new": true } }));
        assert_eq!(upgraded, json!({ "native": { "new": true } }));
    }

    #[test]
    fn upgrades_compaction_events() {
        assert_eq!(
            event("session.compaction.started", json!({ "sessionID": "ses_1", "reason": "auto", "recent": "kept" })),
            json!({ "sessionID": "ses_1", "reason": "auto" })
        );
        let ended = event(
            "session.compaction.ended",
            json!({
                "sessionID": "ses_1",
                "reason": "auto",
                "providerState": { "ignored": true },
                "providerContext": { "version": 1, "provenance": {}, "messages": [{ "role": "user", "content": [] }] },
                "text": "",
                "recent": "",
            }),
        );
        assert_eq!(
            ended,
            json!({
                "sessionID": "ses_1",
                "reason": "auto",
                "native": { "messages": [{ "role": "user", "content": [] }] },
                "text": "",
            })
        );
    }

    #[test]
    fn upgrades_content_updates_and_assistant_messages() {
        let parts = json!([
            { "type": "text", "text": "a", "state": { "sig": 1 } },
            { "type": "reasoning", "text": "b", "state": { "sig": 2 } },
            {
                "type": "tool",
                "id": "call_1",
                "name": "read",
                "providerState": { "call": 1, "shared": "call" },
                "providerResultState": { "result": 1, "shared": "result" },
                "state": { "status": "running", "input": {}, "metadata": {} },
            },
        ]);
        let expected = json!([
            { "type": "text", "text": "a", "native": { "sig": 1 } },
            { "type": "reasoning", "text": "b", "native": { "sig": 2 } },
            {
                "type": "tool",
                "id": "call_1",
                "name": "read",
                "native": { "call": 1, "shared": "result", "result": 1 },
                "state": { "status": "running", "input": {}, "metadata": {} },
            },
        ]);
        assert_eq!(
            event("session.message.content.updated", json!({ "content": parts.clone() })),
            json!({ "content": expected.clone() })
        );
        let assistant = message("assistant", obj(json!({ "providerState": { "step": 1 }, "content": parts })));
        assert_eq!(Value::Object(assistant), json!({ "native": { "step": 1 }, "content": expected }));
    }

    #[test]
    fn tool_result_state_alone_becomes_native() {
        let upgraded = message(
            "assistant",
            obj(json!({ "content": [{ "type": "tool", "providerResultState": { "r": 1 }, "state": { "status": "error" } }] })),
        );
        assert_eq!(upgraded["content"][0], json!({ "type": "tool", "state": { "status": "error" }, "native": { "r": 1 } }));
    }

    #[test]
    fn moves_compaction_recent_text_into_metadata() {
        let upgraded = message(
            "compaction",
            obj(json!({ "status": "completed", "summary": "s", "recent": "kept", "metadata": { "a": 1 } })),
        );
        assert_eq!(Value::Object(upgraded), json!({ "status": "completed", "summary": "s", "metadata": { "recent": "kept", "a": 1 } }));
        let empty = message("compaction", obj(json!({ "status": "running", "summary": "", "recent": "" })));
        assert_eq!(Value::Object(empty), json!({ "status": "running", "summary": "" }));
    }

    #[test]
    fn moves_provider_context_window_and_upgrades_media() {
        let upgraded = message(
            "compaction",
            obj(json!({
                "status": "completed",
                "summary": "",
                "recent": "",
                "providerState": { "dropped": true },
                "providerContext": {
                    "version": 1,
                    "provenance": { "providerID": "openai" },
                    "messages": [{
                        "role": "user",
                        "content": [{ "type": "media", "mediaType": "image/png", "data": "AAAA" }, { "type": "text", "text": "hi" }],
                    }],
                },
            })),
        );
        assert_eq!(
            Value::Object(upgraded),
            json!({
                "status": "completed",
                "summary": "",
                "native": { "messages": [{
                    "role": "user",
                    "content": [
                        { "type": "media", "media": { "source": { "type": "base64", "data": "AAAA", "mediaType": "image/png" } } },
                        { "type": "text", "text": "hi" },
                    ],
                }] },
            })
        );
    }

    #[test]
    fn leaves_current_shapes_alone() {
        let current = json!({
            "status": "completed",
            "summary": "s",
            "native": { "messages": [] },
            "metadata": { "keepID": "msg_1" },
        });
        assert_eq!(Value::Object(message("compaction", obj(current.clone()))), current);
        let user = json!({ "text": "hi", "state": "kept" });
        assert_eq!(Value::Object(message("user", obj(user.clone()))), user);
        assert_eq!(event("session.created", json!({ "state": 1 })), json!({ "state": 1 }));
        assert_eq!(messages(vec![json!(1), json!({ "type": "user", "state": 1 })]), vec![json!(1), json!({ "type": "user", "state": 1 })]);
    }
}
