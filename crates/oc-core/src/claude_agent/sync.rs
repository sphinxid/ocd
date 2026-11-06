//! History sync between OpenCode's prompt and Claude Code's native session (`sync/*.ts`):
//! fingerprints of visible message groups, the resume decision, safe branch boundaries, the
//! checkpoints saved after a turn, and the catch-up content for turns Claude has not seen.
//!
//! Prompts are AI SDK `LanguageModelV3Prompt`s in their wire form (see `oc_ai::aisdk`): binary
//! file data is `{"$bytes": base64}`, URLs are `{"$url": href}`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::convert;
use super::js;

/// One saved checkpoint (`SessionRecord.Checkpoint`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub hash: String,
    #[serde(rename = "claudeMessageID", default, skip_serializing_if = "Option::is_none")]
    pub claude_message_id: Option<String>,
}

/// `SessionRecord.Record`: the plugin-KV value at `sessions/<opencodeSessionID>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    #[serde(rename = "claudeSessionID")]
    pub claude_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<Checkpoint>>,
    /// After cancellation, Claude owns the partial reply; history ends at the input it received.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<bool>,
}

impl Record {
    /// `Schema.decodeUnknownOption(Record)`: anything else is no record.
    pub fn decode(value: Option<&Value>) -> Option<Record> {
        let object = value?.as_object()?;
        if !object.get("claudeSessionID").is_some_and(Value::is_string) {
            return None;
        }
        if let Some(history) = object.get("history") {
            let items = history.as_array()?;
            for item in items {
                let item = item.as_object()?;
                if !item.get("hash").is_some_and(Value::is_string) {
                    return None;
                }
                if item.get("claudeMessageID").is_some_and(|v| !v.is_string()) {
                    return None;
                }
            }
        }
        if object.get("interrupted").is_some_and(|v| !v.is_boolean()) {
            return None;
        }
        serde_json::from_value(Value::Object(object.clone())).ok()
    }

    pub fn history(&self) -> &[Checkpoint] {
        self.history.as_deref().unwrap_or(&[])
    }
}

/// `Fingerprint.Group`.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub hash: String,
    pub role: String,
    pub start: usize,
    pub end: usize,
}

fn sha256(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn hash(value: &Value) -> String {
    sha256(&js::canonical(value))
}

pub(crate) fn role(message: &Value) -> &str {
    message.get("role").and_then(Value::as_str).unwrap_or_default()
}

pub(crate) fn parts(message: &Value) -> &[Value] {
    message.get("content").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

pub(crate) fn kind(part: &Value) -> &str {
    part.get("type").and_then(Value::as_str).unwrap_or_default()
}

/// The visible form of file data: a URL's href, a base64 string as is, bytes as base64.
pub(crate) fn file_data(data: Option<&Value>) -> Value {
    match data {
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(Value::Object(o)) if o.contains_key("$url") => json!(convert::href(o.get("$url").and_then(Value::as_str).unwrap_or_default())),
        Some(Value::Object(o)) if o.contains_key("$bytes") => o.get("$bytes").cloned().unwrap_or(Value::Null),
        Some(other) => other.clone(),
        None => Value::Null,
    }
}

fn without_provider_options(value: &Value) -> Value {
    match value {
        Value::Object(o) => Value::Object(o.iter().filter(|(k, _)| k.as_str() != "providerOptions").map(|(k, v)| (k.clone(), v.clone())).collect()),
        other => other.clone(),
    }
}

/// `visible(message)`: what a message shows, independent of reasoning and provider options.
fn visible(message: &Value) -> Vec<Value> {
    let mut out = vec![];
    let mut text = String::new();
    for part in parts(message) {
        let kind = kind(part);
        if kind == "reasoning" {
            continue;
        }
        if kind == "text" {
            text.push_str(part.get("text").and_then(Value::as_str).unwrap_or_default());
            continue;
        }
        if !text.is_empty() {
            out.push(json!({ "type": "text", "text": text }));
        }
        text = String::new();
        match kind {
            "file" => {
                let mut o = Map::new();
                o.insert("type".into(), json!("file"));
                if let Some(media) = part.get("mediaType") {
                    o.insert("mediaType".into(), media.clone());
                }
                o.insert("data".into(), file_data(part.get("data")));
                out.push(Value::Object(o));
            }
            "tool-call" => {
                let mut o = Map::new();
                o.insert("type".into(), json!("tool-call"));
                for (from, to) in [("toolCallId", "id"), ("toolName", "name"), ("input", "input")] {
                    if let Some(v) = part.get(from) {
                        o.insert(to.into(), v.clone());
                    }
                }
                out.push(Value::Object(o));
            }
            "tool-result" => {
                // `{ ...part.output }`: always an object.
                let mut output = match part.get("output") {
                    Some(o @ Value::Object(_)) => without_provider_options(o),
                    _ => json!({}),
                };
                if output.get("type").and_then(Value::as_str) == Some("content")
                    && let Some(items) = output.get("value").and_then(Value::as_array).cloned()
                {
                    output["value"] = Value::Array(items.iter().map(without_provider_options).collect());
                }
                let mut o = Map::new();
                o.insert("type".into(), json!("tool-result"));
                for (from, to) in [("toolCallId", "id"), ("toolName", "name")] {
                    if let Some(v) = part.get(from) {
                        o.insert(to.into(), v.clone());
                    }
                }
                o.insert("output".into(), output);
                out.push(Value::Object(o));
            }
            _ => {}
        }
    }
    if !text.is_empty() {
        out.push(json!({ "type": "text", "text": text }));
    }
    out
}

/// OpenCode lowers tool attachments into this separate user message.
pub(crate) fn is_tool_media(message: &Value) -> bool {
    let content = parts(message);
    role(message) == "user"
        && content.len() > 1
        && kind(&content[0]) == "text"
        && content[0].get("text").and_then(Value::as_str) == Some("Attached media from tool result:")
        && content[1..].iter().all(|p| kind(p) == "file")
}

/// `Fingerprint.groups(prompt, prefix)`.
pub fn groups(prompt: &[Value], prefix: &[Checkpoint]) -> Vec<Group> {
    let mut groups: Vec<Group> = vec![];
    let mut i = 0;
    while i < prompt.len() {
        let message = &prompt[i];
        let start = i;
        i += 1;
        let role = role(message).to_string();
        if role == "system" {
            continue;
        }
        let visible_parts = visible(message);
        if visible_parts.is_empty() {
            continue;
        }
        let mut members = vec![hash(&Value::Array(visible_parts))];
        if role == "user" {
            while i < prompt.len() {
                // A submitted input remains a boundary even when a later user message arrives without a visible reply.
                if let Some(checkpoint) = prefix.get(groups.len()) {
                    let mut sorted = members.clone();
                    sorted.sort();
                    if hash(&json!({ "role": role, "members": sorted })) == checkpoint.hash {
                        break;
                    }
                }
                let next = &prompt[i];
                if self::role(next) != "user" {
                    break;
                }
                members.push(hash(&Value::Array(visible(next))));
                i += 1;
            }
            members.sort();
        }
        if role == "tool"
            && let Some(next) = prompt.get(i)
            && self::role(next) == "user"
            && is_tool_media(next)
        {
            members.push(hash(&Value::Array(visible(next))));
            i += 1;
        }
        groups.push(Group { hash: hash(&json!({ "role": role, "members": members })), role, start, end: i });
    }
    groups
}

/// `SyncDecision.decide`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionKind {
    Fresh,
    Changed,
    CatchUp,
    Continue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub kind: DecisionKind,
    pub start: usize,
}

pub fn decide(prompt: &[Value], record: Option<&Record>) -> Decision {
    let saved = record.map(Record::history).unwrap_or(&[]);
    let interrupted = record.and_then(|r| r.interrupted) == Some(true);
    let groups = groups(prompt, if interrupted { saved } else { &[] });
    let fresh = Decision { kind: DecisionKind::Fresh, start: 0 };
    let changed = Decision { kind: DecisionKind::Changed, start: 0 };
    if saved.is_empty() {
        return fresh;
    }
    let mut matched = 0;
    while matched < saved.len() && matched < groups.len() {
        if saved[matched].hash != groups[matched].hash {
            break;
        }
        matched += 1;
    }
    if matched == 0 {
        return fresh;
    }
    // Changed histories need an isolated branch, or a rebuild when no safe checkpoint survives.
    if matched < saved.len() {
        return changed;
    }
    // Claude's drained transcript owns the interrupted reply, not OpenCode's partial copy.
    while interrupted && groups.get(matched).is_some_and(|g| g.role == "assistant") {
        let reply = &prompt[groups[matched].start];
        if role(reply) != "assistant" || parts(reply).iter().any(|p| kind(p) == "tool-call") {
            break;
        }
        matched += 1;
    }
    let start = groups.get(matched).map(|g| g.start).unwrap_or(prompt.len());
    if interrupted && start == prompt.len() {
        return changed;
    }
    let missed = &groups[matched.min(groups.len())..];
    let kind = if missed.iter().any(|g| g.role != "user") { DecisionKind::CatchUp } else { DecisionKind::Continue };
    Decision { kind, start }
}

/// A safe branch point (`SyncDecision.boundary`).
#[derive(Debug, Clone, PartialEq)]
pub struct Boundary {
    pub message_id: String,
    pub start: usize,
    pub history: Vec<Checkpoint>,
}

/// A branch must not end between a native tool call and its results.
pub fn boundary(prompt: &[Value], record: Option<&Record>) -> Option<Boundary> {
    let groups = groups(prompt, &[]);
    let history = record.map(Record::history).unwrap_or(&[]);
    let mut pending = std::collections::HashSet::new();
    let mut found = None;
    for (i, group) in groups.iter().enumerate() {
        // Undo can resubmit identical input; keep the current user group for the new turn.
        if i == groups.len() - 1 && group.role == "user" {
            break;
        }
        let Some(checkpoint) = history.get(i) else { break };
        if checkpoint.hash != group.hash {
            break;
        }
        for message in &prompt[group.start..group.end] {
            if role(message) == "system" {
                continue;
            }
            for part in parts(message) {
                let id = part.get("toolCallId").and_then(Value::as_str).unwrap_or_default().to_string();
                if kind(part) == "tool-call" {
                    pending.insert(id);
                } else if kind(part) == "tool-result" {
                    pending.remove(&id);
                }
            }
        }
        if let Some(message_id) = &checkpoint.claude_message_id
            && pending.is_empty()
        {
            found = Some(Boundary { message_id: message_id.clone(), start: group.end, history: history[..=i].to_vec() });
        }
    }
    found
}

/// `Checkpoints.build`.
pub struct BuildInput<'a> {
    pub prompt: &'a [Value],
    pub previous: Option<&'a Record>,
    pub user_id: Option<&'a str>,
    /// The reply's text and tool-call parts (V3 prompt parts).
    pub reply: &'a [Value],
    pub message_id: Option<&'a str>,
    pub tool_messages: &'a std::collections::HashMap<String, String>,
}

pub fn build_checkpoints(input: BuildInput) -> Vec<Checkpoint> {
    let mut history = vec![];
    let groups = groups(input.prompt, &[]);
    for (i, group) in groups.iter().enumerate() {
        let previous = input.previous.and_then(|r| r.history.as_ref()).and_then(|h| h.get(i));
        let mut claude_message_id = None;
        if let Some(previous) = previous
            && previous.hash == group.hash
        {
            claude_message_id = previous.claude_message_id.clone();
        }
        if group.role == "tool" {
            for message in &input.prompt[group.start..group.end] {
                if role(message) != "tool" {
                    continue;
                }
                for part in parts(message) {
                    if kind(part) != "tool-result" {
                        continue;
                    }
                    let id = part.get("toolCallId").and_then(Value::as_str).unwrap_or_default();
                    if let Some(tool_message) = input.tool_messages.get(id) {
                        claude_message_id = Some(tool_message.clone());
                    }
                }
            }
        }
        if i == groups.len() - 1
            && let Some(user) = input.user_id
        {
            claude_message_id = Some(user.to_string());
        }
        history.push(Checkpoint { hash: group.hash.clone(), claude_message_id });
    }
    if !input.reply.is_empty() {
        let reply = groups_of_reply(input.reply);
        history.push(Checkpoint { hash: reply, claude_message_id: input.message_id.map(String::from) });
    }
    history
}

fn groups_of_reply(reply: &[Value]) -> String {
    let message = json!({ "role": "assistant", "content": reply });
    groups(std::slice::from_ref(&message), &[]).into_iter().next().map(|g| g.hash).unwrap_or_default()
}

/// `CatchUp.content(prompt, start)`.
pub fn catch_up(prompt: &[Value], start: usize) -> Result<Vec<Value>, String> {
    let missed = &prompt[start.min(prompt.len())..];
    let end = groups(missed, &[]).iter().rev().find(|g| g.role != "user").map(|g| g.end).unwrap_or(0);
    let mut blocks = vec![];
    if end > 0 {
        blocks.push(json!({
            "type": "text",
            "text": "<conversation-checkpoint>\nThe following is conversation history from OpenCode that you have not seen. Treat it as prior conversation, not a new request. Continue with the user messages after this checkpoint.",
        }));
        blocks.extend(convert::content(&missed[..end])?);
        blocks.push(json!({ "type": "text", "text": "</conversation-checkpoint>" }));
    }
    blocks.extend(convert::content(&missed[end..])?);
    Ok(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Value {
        json!({ "role": "user", "content": [{ "type": "text", "text": text }] })
    }
    fn assistant(text: &str) -> Value {
        json!({ "role": "assistant", "content": [{ "type": "text", "text": text }] })
    }
    fn past() -> Vec<Value> {
        vec![user("First marker"), assistant("COPPER-OTTER")]
    }
    fn checkpoints(prompt: &[Value]) -> Vec<Checkpoint> {
        groups(prompt, &[]).into_iter().map(|g| Checkpoint { hash: g.hash, claude_message_id: None }).collect()
    }
    fn record() -> Record {
        Record { claude_session_id: "claude-a".into(), history: Some(checkpoints(&past())), interrupted: None }
    }
    fn cat(parts: &[&[Value]]) -> Vec<Value> {
        parts.iter().flat_map(|p| p.iter().cloned()).collect()
    }
    fn d(kind: DecisionKind, start: usize) -> Decision {
        Decision { kind, start }
    }

    #[test]
    fn interrupted_reply_continues_after_the_submitted_input() {
        let input = cat(&[&past(), &[user("List animals")]]);
        let saved = Record { claude_session_id: "claude-a".into(), history: Some(checkpoints(&input)), interrupted: Some(true) };
        assert_eq!(decide(&cat(&[&input, &[assistant("1. Ot"), user("Stop")]]), Some(&saved)), d(DecisionKind::Continue, 4));
        assert_eq!(decide(&cat(&[&input, &[user("Stop")]]), Some(&saved)), d(DecisionKind::Continue, 3));
        assert_eq!(
            decide(&cat(&[&input, &[assistant("1. Ot"), user("Other model"), assistant("New answer"), user("Back")]]), Some(&saved)),
            d(DecisionKind::CatchUp, 4)
        );
        let edited = vec![past()[0].clone(), assistant("Edited"), input[2].clone(), user("Stop")];
        assert_eq!(decide(&edited, Some(&saved)).kind, DecisionKind::Changed);
    }

    #[test]
    fn branches_only_at_a_matching_checkpoint() {
        let history = cat(&[
            &past(),
            &[
                user("Read two files"),
                json!({ "role": "assistant", "content": [
                    { "type": "tool-call", "toolCallId": "a", "toolName": "read", "input": { "path": "a" } },
                    { "type": "tool-call", "toolCallId": "b", "toolName": "read", "input": { "path": "b" } },
                ] }),
                json!({ "role": "tool", "content": [{ "type": "tool-result", "toolCallId": "a", "toolName": "read", "output": { "type": "text", "value": "A" } }] }),
                json!({ "role": "tool", "content": [{ "type": "tool-result", "toolCallId": "b", "toolName": "read", "output": { "type": "text", "value": "B" } }] }),
                assistant("Read both"),
            ],
        ]);
        let saved = Record {
            claude_session_id: "source".into(),
            history: Some(
                groups(&history, &[])
                    .into_iter()
                    .enumerate()
                    .map(|(i, g)| Checkpoint { hash: g.hash, claude_message_id: Some(format!("uuid-{i}")) })
                    .collect(),
            ),
            interrupted: None,
        };
        assert!(boundary(&history[..1], Some(&saved)).is_none());
        assert_eq!(boundary(&history[..3], Some(&saved)).unwrap().message_id, "uuid-1");
        assert_eq!(boundary(&history[..4], Some(&saved)).unwrap().message_id, "uuid-2");
        assert_eq!(boundary(&history[..5], Some(&saved)).unwrap().message_id, "uuid-2");
        assert_eq!(boundary(&history, Some(&saved)).unwrap().message_id, "uuid-6");
        let edited = cat(&[&[user("Edited source")], &history[1..]]);
        assert!(boundary(&edited, Some(&saved)).is_none());
        let no_boundary = Record {
            history: Some(saved.history().iter().map(|c| Checkpoint { hash: c.hash.clone(), claude_message_id: None }).collect()),
            ..saved.clone()
        };
        assert!(boundary(&history, Some(&no_boundary)).is_none());
    }

    #[test]
    fn chooses_all_four_cases() {
        let r = record();
        assert_eq!(decide(&cat(&[&past(), &[user("Next")]]), Some(&r)), d(DecisionKind::Continue, 2));
        assert_eq!(decide(&cat(&[&past(), &[user("Other model"), assistant("LILAC-HERON"), user("Next")]]), Some(&r)), d(DecisionKind::CatchUp, 2));
        assert_eq!(decide(&[past()[0].clone(), assistant("Edited"), user("Next")], Some(&r)), d(DecisionKind::Changed, 0));
        assert_eq!(decide(&[past()[0].clone(), user("Undo")], Some(&r)).kind, DecisionKind::Fresh);
        assert_eq!(decide(&[past()[0].clone()], Some(&r)).kind, DecisionKind::Changed);
        assert_eq!(decide(&[user("Compacted summary"), user("Next")], Some(&r)).kind, DecisionKind::Fresh);
        let old = Record { claude_session_id: "old".into(), history: None, interrupted: None };
        assert_eq!(decide(&cat(&[&past(), &[user("Next")]]), Some(&old)).kind, DecisionKind::Fresh);
    }

    #[test]
    fn ignores_thinking_metadata_and_reordered_user_notes() {
        let original = cat(&[&[user("Plan reminder")], &past()]);
        let changed = vec![
            past()[0].clone(),
            user("Plan reminder"),
            json!({ "role": "assistant", "content": [
                { "type": "reasoning", "text": "Private reasoning" },
                { "type": "text", "text": "COPPER-", "providerOptions": { "another": { "signature": "opaque" } } },
                { "type": "text", "text": "OTTER" },
            ] }),
        ];
        let hashes = |p: &[Value]| groups(p, &[]).into_iter().map(|g| g.hash).collect::<Vec<_>>();
        assert_eq!(hashes(&changed), hashes(&original));
        assert_ne!(hashes(&cat(&[&[user("Changed reminder")], &past()])), hashes(&original));
    }

    #[test]
    fn tool_inputs_compare_independent_of_key_order() {
        let mut prompt = vec![
            json!({ "role": "assistant", "content": [{ "type": "tool-call", "toolCallId": "call-a", "toolName": "read", "input": { "path": "image.png", "offset": 1 } }] }),
            json!({ "role": "tool", "content": [{ "type": "tool-result", "toolCallId": "call-a", "toolName": "read", "output": { "type": "text", "value": "Image follows" } }] }),
            json!({ "role": "user", "content": [
                { "type": "text", "text": "Attached media from tool result:" },
                { "type": "file", "mediaType": "image/png", "data": { "$bytes": "AQI=" } },
            ] }),
        ];
        let first = groups(&prompt, &[]);
        assert_eq!(first.iter().map(|g| g.role.as_str()).collect::<Vec<_>>(), ["assistant", "tool"]);
        assert_eq!(first[1].end, 3);
        prompt[0] = json!({ "role": "assistant", "content": [{ "type": "tool-call", "toolCallId": "call-a", "toolName": "read", "input": { "offset": 1, "path": "image.png" } }] });
        prompt[2] = json!({ "role": "user", "content": [
            { "type": "text", "text": "Attached media from tool result:" },
            { "type": "file", "mediaType": "image/png", "data": "AQI=" },
        ] });
        assert_eq!(groups(&prompt, &[]), first);
        prompt[1] = json!({ "role": "tool", "content": [{ "type": "tool-result", "toolCallId": "call-a", "toolName": "read", "output": { "type": "text", "value": "Edited output" } }] });
        assert_ne!(groups(&prompt, &[])[1].hash, first[1].hash);
    }

    #[test]
    fn catch_up_includes_only_missed_turns() {
        let prompt = cat(&[&past(), &[user("Other model"), assistant("LILAC-HERON"), user("What happened?")]]);
        let blocks = catch_up(&prompt, 2).unwrap();
        let text = serde_json::to_string(&blocks).unwrap();
        assert!(!text.contains("COPPER-OTTER"));
        assert!(text.contains("LILAC-HERON"));
        assert_eq!(blocks[blocks.len() - 2], json!({ "type": "text", "text": "</conversation-checkpoint>" }));
        assert_eq!(blocks[blocks.len() - 1], json!({ "type": "text", "text": "What happened?" }));
        assert_eq!(catch_up(&prompt, 4).unwrap(), vec![json!({ "type": "text", "text": "What happened?" })]);
    }

    #[test]
    fn catch_up_preserves_tools_images_and_pdfs() {
        let blocks = catch_up(
            &[
                user("Read the files"),
                json!({ "role": "assistant", "content": [
                    { "type": "reasoning", "text": "PRIVATE" },
                    { "type": "tool-call", "toolCallId": "a", "toolName": "read", "input": { "path": "marker.txt" } },
                ] }),
                json!({ "role": "tool", "content": [{ "type": "tool-result", "toolCallId": "a", "toolName": "read", "output": { "type": "text", "value": "LEMON-IBIS\nSecond line" } }] }),
                json!({ "role": "user", "content": [
                    { "type": "text", "text": "Attached media from tool result:" },
                    { "type": "file", "mediaType": "image/png", "data": "AQI=" },
                    { "type": "file", "mediaType": "application/pdf", "data": "AwQ=" },
                ] }),
                user("Continue"),
            ],
            0,
        )
        .unwrap();
        let text = serde_json::to_string(&blocks).unwrap();
        assert!(text.contains("marker.txt"));
        assert!(text.contains("LEMON-IBIS"));
        assert!(!text.contains("PRIVATE"));
        assert!(blocks.iter().any(|b| b["type"] == "image"));
        assert!(blocks.iter().any(|b| b["type"] == "document"));
        assert_eq!(blocks.last().unwrap(), &json!({ "type": "text", "text": "Continue" }));
    }

    #[test]
    fn record_decoding_is_lenient() {
        assert!(Record::decode(Some(&json!({ "claudeSessionID": 1 }))).is_none());
        assert!(Record::decode(Some(&json!({ "claudeSessionID": "a", "history": [{ "hash": 1 }] }))).is_none());
        let r = Record::decode(Some(&json!({ "claudeSessionID": "a", "history": [{ "hash": "h" }], "interrupted": false }))).unwrap();
        assert_eq!(serde_json::to_value(&r).unwrap(), json!({ "claudeSessionID": "a", "history": [{ "hash": "h" }], "interrupted": false }));
    }
}
