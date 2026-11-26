//! `session/runner/live-tool-metadata.ts` (2.4.1, #64929): the latest progress metadata of locally running tool
//! calls, keyed by assistant message and call id. Progress is only published as an ephemeral event, so message reads
//! overlay it onto running tool parts; clients that load a session mid-call then see the call's metadata.

use std::collections::HashMap;
use std::sync::LazyLock;

use oc_schema::Metadata;
use parking_lot::Mutex;
use serde_json::Value;

static LIVE: LazyLock<Mutex<HashMap<String, HashMap<String, Metadata>>>> = LazyLock::new(Default::default);

pub fn set(message_id: &str, call_id: &str, metadata: Metadata) {
    LIVE.lock().entry(message_id.to_string()).or_default().insert(call_id.to_string(), metadata);
}

/// Forgets one call's progress, or every call of the message when `call_id` is `None`.
pub fn clear(message_id: &str, call_id: Option<&str>) {
    let mut live = LIVE.lock();
    let Some(call_id) = call_id else {
        live.remove(message_id);
        return;
    };
    let Some(calls) = live.get_mut(message_id) else { return };
    calls.remove(call_id);
    if calls.is_empty() {
        live.remove(message_id);
    }
}

/// `overlay(message)` over an encoded message: a running tool part's metadata gets the call's latest progress.
pub fn overlay(message: &mut Value) {
    if message.get("type").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let Some(id) = message.get("id").and_then(Value::as_str) else { return };
    let live = LIVE.lock();
    let Some(calls) = live.get(id) else { return };
    let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else { return };
    for part in content {
        if part.get("type").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let Some(metadata) = part.get("id").and_then(Value::as_str).and_then(|call| calls.get(call)) else { continue };
        let Some(state) = part.get_mut("state").and_then(Value::as_object_mut) else { continue };
        if state.get("status").and_then(Value::as_str) != Some("running") {
            continue;
        }
        let mut merged = state.get("metadata").and_then(Value::as_object).cloned().unwrap_or_default();
        for (key, value) in metadata {
            merged.insert(key.clone(), value.clone());
        }
        state.insert("metadata".into(), Value::Object(merged));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    /// session-live-tool-metadata.test.ts: progress is added to running tool parts until the call settles.
    #[test]
    fn adds_progress_to_running_tool_parts_until_the_call_settles() {
        let message = |state: serde_json::Value| {
            json!({"id": "msg_live_1", "type": "assistant", "agent": "build", "model": {"id": "model", "providerID": "provider"},
                "content": [{"type": "tool", "id": "call_1", "name": "execute", "state": state, "time": {"created": 0}}],
                "time": {"created": 0}})
        };
        let running = message(json!({"status": "running", "input": {}, "metadata": {}}));
        let completed = message(json!({"status": "completed", "input": {}, "content": [{"type": "text", "text": "done"}]}));
        super::set("msg_live_1", "call_1", json!({"background": true, "toolCalls": []}).as_object().unwrap().clone());
        let mut overlaid = running.clone();
        super::overlay(&mut overlaid);
        assert_eq!(overlaid["content"][0]["state"]["metadata"], json!({"background": true, "toolCalls": []}));
        let mut same = completed.clone();
        super::overlay(&mut same);
        assert_eq!(same, completed);

        super::clear("msg_live_1", Some("call_1"));
        let mut cleared = running.clone();
        super::overlay(&mut cleared);
        assert_eq!(cleared, running);
    }
}
