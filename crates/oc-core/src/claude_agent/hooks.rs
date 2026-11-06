//! The plugin's session hooks for provider `claude-agent` (`session.ts`, `instructions.ts`), applied
//! natively before external plugin hooks (upstream registers them as internal hooks, scoped to the
//! provider, ahead of the identity and tool-guidance hooks):
//!
//! - `model.request`: the request kind header, and for a forked session's primary request its source.
//! - `context`: no `maxTokens` (Claude picks its own budget), and failed tool results flagged in
//!   `providerMetadata["claude-agent"]` (`toolError`, `toolAborted`) before the AI SDK lowering
//!   flattens them to text.
//! - `context` / `generate` / `compaction`: the agent's role and the rendered instructions (minus the
//!   date and environment, which Claude Code writes itself) as provider options.
//! - `retry`: never; Claude Code already retries API failures.

use oc_ai::LLMRequest;
use oc_ai::types::{ContentPart, ToolResultValue};
use serde_json::{Value, json};

use super::bridge::{AGENT_OPTION, FORK_HEADER, INSTRUCTIONS_OPTION, KIND_HEADER};
use crate::plugin_request::Kind;

/// Claude Code writes its own date and environment, so OpenCode's would repeat them.
const SUPPLIED_BY_CLAUDE: [&str; 2] = ["core/date", "core/environment"];

/// The `{experimental: {instructions: [{key, text}]}}` system-part metadata.
fn decode_instructions(metadata: Option<&serde_json::Map<String, Value>>) -> Option<Vec<(String, String)>> {
    let items = metadata?.get("experimental")?.as_object()?.get("instructions")?.as_array()?;
    items
        .iter()
        .map(|item| {
            let key = item.get("key")?.as_str()?.to_string();
            let text = item.get("text")?.as_str()?.to_string();
            Some((key, text))
        })
        .collect()
}

/// `instructions(system)`: the first part is OpenCode's own prompt, which Claude Code's prompt replaces.
pub fn instructions(system: &[oc_ai::types::SystemPart]) -> String {
    let mut texts: Vec<String> = vec![];
    for part in system.iter().skip(1) {
        match decode_instructions(part.metadata.as_ref()) {
            Some(sources) => texts.extend(sources.into_iter().filter(|(k, _)| !SUPPLIED_BY_CLAUDE.contains(&k.as_str())).map(|(_, t)| t)),
            None => {
                // ocd lays out the identity and tool-guidance parts before plugin hooks run; upstream adds them
                // after this hook, so they are not part of the instructions.
                if part.text.starts_with("# Your Model\n") || part.text == crate::prompt::SESSION_MOVE_GUIDANCE {
                    continue;
                }
                texts.push(part.text.clone());
            }
        }
    }
    texts.join("\n\n")
}

/// The `context`, `generate` and `compaction` hooks.
pub fn shape(kind: Kind, agent_system: Option<&str>, req: &mut LLMRequest) {
    if kind == Kind::Title {
        return;
    }
    if kind == Kind::Primary {
        // Let Claude choose its normal output budget. Later hooks can still set an explicit maxTokens.
        if let Some(generation) = req.generation.as_mut() {
            generation.max_tokens = None;
            if generation.is_empty() {
                req.generation = None;
            }
        }
        // Preserve tool error status before the AI SDK bridge flattens error results to plain text.
        for message in req.messages.iter_mut() {
            for part in message.content.iter_mut() {
                let ContentPart::ToolResult(result) = part else { continue };
                let ToolResultValue::Error(value) = &result.result else { continue };
                let aborted = value.get("error").and_then(|e| e.get("type")).and_then(Value::as_str) == Some("aborted");
                let mut metadata = result.provider_metadata.clone().unwrap_or_default();
                let mut flags = metadata.get(super::PROVIDER_ID).and_then(Value::as_object).cloned().unwrap_or_default();
                flags.insert("toolError".into(), json!(true));
                flags.insert("toolAborted".into(), json!(aborted));
                metadata.insert(super::PROVIDER_ID.into(), Value::Object(flags));
                result.provider_metadata = Some(metadata);
            }
        }
    }
    // Read the role from the resolved agent, not by guessing which prompt text is a built-in.
    let options = req.provider_options.get_or_insert_default();
    options.insert(AGENT_OPTION.into(), json!(agent_system.unwrap_or_default()));
    options.insert(INSTRUCTIONS_OPTION.into(), json!(instructions(&req.system)));
}

/// The `model.request` hook: the request kind and, for a forked session's primary request, its source.
pub fn model_request(kind: Kind, fork_source: Option<&str>, req: &mut LLMRequest) {
    let mut http = req.http.clone().unwrap_or_default();
    let mut headers = http.headers.clone().unwrap_or_default();
    headers.insert(KIND_HEADER.into(), kind.as_str().into());
    if kind == Kind::Primary
        && let Some(source) = fork_source
    {
        headers.insert(FORK_HEADER.into(), source.into());
    }
    http.headers = Some(headers);
    req.http = Some(http);
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_ai::types::SystemPart;

    #[test]
    fn instructions_skip_claude_supplied_sources() {
        let mut keyed = SystemPart::text("AGENTS\n\nDATE");
        keyed.metadata = Some(
            json!({ "experimental": { "instructions": [{ "key": "file/AGENTS.md", "text": "AGENTS" }, { "key": "core/date", "text": "DATE" }, { "key": "core/environment", "text": "ENV" }] } })
                .as_object()
                .cloned()
                .unwrap(),
        );
        let system = vec![
            SystemPart::text("You are OpenCode"),
            SystemPart::text("# Your Model\n- Name: Opus"),
            keyed,
            SystemPart::text(crate::prompt::SESSION_MOVE_GUIDANCE),
            SystemPart::text("Plugin note"),
        ];
        assert_eq!(instructions(&system), "AGENTS\n\nPlugin note");
    }
}
