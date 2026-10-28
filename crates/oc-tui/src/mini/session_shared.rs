//! Session history helpers (`src/mini/session.shared.ts`, `prompt/codec.ts` projectedPromptInput).

use super::prompt_shared::{prompt_copy, prompt_same};
use super::types::{MentionRange, ModelSel, PromptPart, RunPrompt};
use serde_json::Value;

pub const LIMIT: usize = 200;

#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    pub prompt: RunPrompt,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub variant: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct RunSession {
    pub first: bool,
    pub turns: Vec<Turn>,
    pub model: Option<ModelSel>,
    pub variant: Option<String>,
}

fn mention(v: Option<&Value>) -> Option<MentionRange> {
    let m = v?;
    if !m.is_object() {
        return None;
    }
    Some(MentionRange {
        start: m.get("start").and_then(|n| n.as_u64()).unwrap_or(0) as usize,
        end: m.get("end").and_then(|n| n.as_u64()).unwrap_or(0) as usize,
        value: m.get("text").and_then(|t| t.as_str()).unwrap_or_default().to_string(),
    })
}

/// `messagePrompt(message)`: a user message (or inbox payload) as a prompt.
pub fn message_prompt(message: &Value) -> RunPrompt {
    let mut parts = Vec::new();
    for file in message.get("files").and_then(|f| f.as_array()).into_iter().flatten() {
        let source = file.get("source");
        let mime = file.get("mime").and_then(|m| m.as_str()).map(|s| s.to_string());
        let uri = if source.and_then(|s| s.get("type")).and_then(|t| t.as_str()) == Some("uri") {
            source.and_then(|s| s.get("uri")).and_then(|u| u.as_str()).unwrap_or_default().to_string()
        } else {
            format!("data:{};base64,{}", mime.clone().unwrap_or_default(), file.get("data").and_then(|d| d.as_str()).unwrap_or_default())
        };
        let name = file.get("name").and_then(|n| n.as_str()).map(|s| s.to_string());
        let path = name.clone().unwrap_or_else(|| if uri.starts_with("data:") { "inline attachment".into() } else { uri.clone() });
        parts.push(PromptPart::File {
            url: uri,
            filename: name,
            mime,
            description: file.get("description").and_then(|d| d.as_str()).map(|s| s.to_string()),
            source: mention(file.get("mention")).map(|m| (path, m)),
        });
    }
    for agent in message.get("agents").and_then(|f| f.as_array()).into_iter().flatten() {
        parts.push(PromptPart::Agent { name: agent.get("name").and_then(|n| n.as_str()).unwrap_or_default().to_string(), source: mention(agent.get("mention")) });
    }
    for skill in message.get("skills").and_then(|f| f.as_array()).into_iter().flatten() {
        parts.push(PromptPart::Skill { id: skill.get("id").and_then(|n| n.as_str()).unwrap_or_default().to_string(), source: mention(skill.get("mention")) });
    }
    RunPrompt { text: message.get("text").and_then(|t| t.as_str()).unwrap_or_default().to_string(), parts, ..Default::default() }
}

/// `createSession(messages)` (messages oldest first).
pub fn create_session(messages: &[Value]) -> RunSession {
    RunSession {
        first: messages.is_empty(),
        turns: messages
            .iter()
            .filter(|m| m.get("type").and_then(|t| t.as_str()) == Some("user"))
            .map(|m| Turn { prompt: message_prompt(m), provider: None, model: None, variant: None })
            .collect(),
        model: None,
        variant: None,
    }
}

/// `resolveCurrentSession` from already fetched messages (newest first, as listed) and the session info.
pub fn current_session(messages_desc: &[Value], session: &crate::api::types::SessionInfo) -> RunSession {
    let ordered: Vec<Value> = messages_desc.iter().rev().cloned().collect();
    let mut current = create_session(&ordered);
    for t in current.turns.iter_mut() {
        t.provider = session.model.as_ref().map(|m| m.provider_id.clone());
        t.model = session.model.as_ref().map(|m| m.id.clone());
        t.variant = session.model.as_ref().and_then(|m| m.variant.clone());
    }
    if let Some(m) = &session.model {
        current.model = Some(ModelSel { provider_id: m.provider_id.clone(), model_id: m.id.clone() });
        current.variant = m.variant.clone();
    }
    current
}

/// `sessionHistory(session)`.
pub fn session_history(session: &RunSession) -> Vec<RunPrompt> {
    let prompts: Vec<&RunPrompt> = session.turns.iter().map(|t| &t.prompt).filter(|p| !p.text.trim().is_empty() || p.parts.iter().any(|x| x.is_file())).collect();
    let mut out: Vec<RunPrompt> = Vec::new();
    for (index, p) in prompts.iter().enumerate() {
        if index > 0 && prompt_same(prompts[index - 1], p) {
            continue;
        }
        out.push(prompt_copy(p));
    }
    let start = out.len().saturating_sub(LIMIT);
    out.split_off(start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_user_prompts() {
        let msgs = vec![json!({
            "id": "msg-user-1", "type": "user", "text": "look @scan @note.ts", "time": {"created": 1},
            "agents": [{"name": "scan", "mention": {"start": 5, "end": 10, "text": "@scan"}}],
            "files": [{"data": "", "mime": "text/plain", "source": {"type": "uri", "uri": "file:///tmp/note.ts"}, "mention": {"start": 11, "end": 19, "text": "@note.ts"}}]
        })];
        let out = create_session(&msgs);
        assert!(!out.first);
        assert_eq!(out.turns.len(), 1);
        assert_eq!(out.turns[0].prompt.text, "look @scan @note.ts");
        assert_eq!(
            out.turns[0].prompt.parts,
            vec![
                PromptPart::File {
                    url: "file:///tmp/note.ts".into(),
                    filename: None,
                    mime: Some("text/plain".into()),
                    description: None,
                    source: Some(("file:///tmp/note.ts".into(), MentionRange { start: 11, end: 19, value: "@note.ts".into() })),
                },
                PromptPart::Agent { name: "scan".into(), source: Some(MentionRange { start: 5, end: 10, value: "@scan".into() }) },
            ]
        );
    }

    #[test]
    fn sources_undefined_without_mentions() {
        let out = create_session(&[json!({"id": "m", "type": "user", "text": "look", "agents": [{"name": "scan"}], "files": [{"data": "", "mime": "text/plain", "source": {"type": "uri", "uri": "file:///tmp/note.ts"}}]})]);
        assert_eq!(
            out.turns[0].prompt.parts,
            vec![
                PromptPart::File { url: "file:///tmp/note.ts".into(), filename: None, mime: Some("text/plain".into()), description: None, source: None },
                PromptPart::Agent { name: "scan".into(), source: None },
            ]
        );
    }

    #[test]
    fn history_dedupes_and_drops_blanks() {
        let parts = vec![PromptPart::Agent { name: "scan".into(), source: Some(MentionRange { start: 0, end: 5, value: "@scan".into() }) }];
        let turn = |text: &str, parts: Vec<PromptPart>| Turn { prompt: RunPrompt { parts, ..RunPrompt::text(text) }, provider: Some("openai".into()), model: Some("gpt-5".into()), variant: None };
        let session = RunSession { first: false, turns: vec![turn("one", parts.clone()), turn("one", parts.clone()), turn("   ", vec![]), turn("two", vec![])], model: None, variant: None };
        let out = session_history(&session);
        assert_eq!(out.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["one", "two"]);
        assert_eq!(out[0].parts, parts);
    }

    #[test]
    fn image_only_history_and_skills() {
        let message = json!({
            "id": "msg_image", "type": "user", "text": "", "time": {"created": 1},
            "files": [
                {"data": "cG5n", "mime": "image/png", "source": {"type": "inline"}, "name": "image.png", "description": "Diagram"},
                {"data": "c2VydmVy", "mime": "image/png", "source": {"type": "uri", "uri": "file:///remote/image.png"}}
            ],
            "skills": [{"id": "effect", "name": "Effect", "mention": {"text": "@effect", "start": 0, "end": 7}}]
        });
        assert_eq!(
            session_history(&create_session(&[message])),
            vec![RunPrompt {
                parts: vec![
                    PromptPart::File { url: "data:image/png;base64,cG5n".into(), filename: Some("image.png".into()), mime: Some("image/png".into()), description: Some("Diagram".into()), source: None },
                    PromptPart::File { url: "file:///remote/image.png".into(), filename: None, mime: Some("image/png".into()), description: None, source: None },
                    PromptPart::Skill { id: "effect".into(), source: Some(MentionRange { start: 0, end: 7, value: "@effect".into() }) },
                ],
                ..RunPrompt::text("")
            }]
        );
    }

    #[test]
    fn latest_matching_variant() {
        let turn = |text: &str, provider: &str, model: &str, variant: Option<&str>| Turn { prompt: RunPrompt::text(text), provider: Some(provider.into()), model: Some(model.into()), variant: variant.map(|s| s.into()) };
        let mut session = RunSession { first: false, turns: vec![turn("one", "openai", "gpt-5", Some("high")), turn("two", "anthropic", "sonnet", Some("max")), turn("three", "openai", "gpt-5", None)], model: None, variant: None };
        let model = ModelSel { provider_id: "openai".into(), model_id: "gpt-5".into() };
        assert_eq!(super::super::variant::session_variant(&session, Some(&model)), None);
        session.turns.push(turn("four", "openai", "gpt-5", Some("minimal")));
        assert_eq!(super::super::variant::session_variant(&session, Some(&model)).as_deref(), Some("minimal"));
    }

    #[test]
    fn restores_from_stored_text_and_files() {
        let messages = vec![json!({
            "id": "msg_prompt", "type": "user", "text": "Review @note.ts",
            "files": [{"data": "", "mime": "text/plain", "name": "note.ts", "source": {"type": "uri", "uri": "file:///tmp/note.ts"}, "mention": {"start": 7, "end": 15, "text": "@note.ts"}}],
            "agents": [], "time": {"created": 1}
        })];
        let info: crate::api::types::SessionInfo = serde_json::from_value(json!({"id": "ses_1", "title": "Session", "projectID": "proj_1", "location": {"directory": "/tmp"}, "time": {"created": 1, "updated": 1}, "cost": 0, "model": {"providerID": "openai", "id": "gpt-5", "variant": "high"}})).unwrap();
        let out = current_session(&messages, &info);
        assert_eq!(out.model, Some(ModelSel { provider_id: "openai".into(), model_id: "gpt-5".into() }));
        assert_eq!(out.variant.as_deref(), Some("high"));
        assert_eq!(
            out.turns[0].prompt,
            RunPrompt {
                parts: vec![PromptPart::File {
                    url: "file:///tmp/note.ts".into(),
                    filename: Some("note.ts".into()),
                    mime: Some("text/plain".into()),
                    description: None,
                    source: Some(("note.ts".into(), MentionRange { start: 7, end: 15, value: "@note.ts".into() })),
                }],
                ..RunPrompt::text("Review @note.ts")
            }
        );
    }
}
