//! Session title generation (`core/session/title.ts`, `context.selectTitle`).

use std::sync::Arc;

use futures::StreamExt;
use oc_ai::types::{Message as LlmMessage, SystemPart};
use oc_ai::{LLMEvent, LLMRequest};
use oc_schema::event::SessionEvent;
use oc_schema::types::{SessionInfo, TokenUsage};
use oc_schema::Message;
use tokio_util::sync::CancellationToken;

use crate::runner::Engine;

const MAX_LENGTH: usize = 100;
const MAX_CONTEXT_LENGTH: usize = 8_000;
const MAX_FIRST_MESSAGE_LENGTH: usize = 2_000;

pub fn truncate(v: &str) -> String {
    let chars: Vec<char> = v.chars().collect();
    if v.encode_utf16().count() <= MAX_LENGTH {
        return v.to_string();
    }
    format!("{}...", chars.iter().take(MAX_LENGTH - 3).collect::<String>())
}

/// `isExactRootFallback`: no title, or exactly `New session - {ISO(created)}`.
pub fn is_untitled(s: &SessionInfo) -> bool {
    match &s.title {
        None => true,
        Some(t) => {
            let iso = chrono::DateTime::from_timestamp_millis(s.time.created as i64)
                .map(|d| d.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
                .unwrap_or_default();
            *t == format!("New session - {iso}")
        }
    }
}

impl Engine {
    async fn title_attempt(
        &self,
        session: &SessionInfo,
        key: &str,
        system: Option<&str>,
        agent_model: bool,
        text: &str,
        model: &oc_ai::catalog::ResolvedModel,
    ) -> Option<String> {
        let mut req = LLMRequest::new(model.language_model.clone());
        if let Some(s) = system {
            req.system = vec![SystemPart::text(s)];
        }
        req.messages = vec![LlmMessage::user_text(text)];
        self.session_http(session, &mut req);
        req.cache = crate::runner::request_cache(model);
        // The built-in Copilot `session.title` hook runs first: the account's free utility model.
        let preset = if model.model.provider_id == oc_ai::catalog::copilot::PROVIDER_ID && self.app.plugins.copilot_enabled(key) {
            self.app.plugins.copilot.utility_title(key, &session.id, agent_model, &req.system, &req.messages, None).await
        } else {
            None
        };
        // `session.title` may shape the request or answer it (`result`, used verbatim).
        let shaped = crate::plugin_request::shape(
            &self.app.plugins,
            &session.location.directory,
            crate::plugin_request::Kind::Title,
            &session.id,
            "title",
            &model.model_ref(),
            &mut req,
            preset,
            None,
        )
        .await;
        let http = match shaped {
            Ok(shaped) if shaped.result.is_some() => return shaped.result,
            Ok(shaped) => shaped.http,
            Err(error) => {
                tracing::warn!(%error, "session.title hook failed");
                return None;
            }
        };
        let metadata_key = req.metadata_key();
        let mut chunks: Vec<(String, String)> = vec![];
        let mut commentary: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut failed = false;
        let mut tokens = TokenUsage::default();
        let mut cost = 0.0;
        let costs: Vec<crate::usage::Cost> = model
            .cost()
            .iter()
            .map(|c| crate::usage::Cost { tier_size: c.tier.as_ref().map(|t| t.size.max(0) as u64), input: c.input, output: c.output, cache_read: c.cache.read, cache_write: c.cache.write })
            .collect();
        let mut any_usage = false;
        match self.client.stream_with(req, CancellationToken::new(), http).await {
            Err(_) => failed = true,
            Ok(mut stream) => {
                while let Some(ev) = stream.next().await {
                    match ev {
                        Err(_) => {
                            failed = true;
                            break;
                        }
                        Ok(LLMEvent::ProviderError { .. }) => failed = true,
                        // Responses can classify a text block only when it ends; filter after the stream completes.
                        Ok(LLMEvent::TextStart { id, provider_metadata } | LLMEvent::TextEnd { id, provider_metadata, .. }) => {
                            phase(&mut commentary, &id, provider_metadata.as_ref(), &metadata_key);
                        }
                        Ok(LLMEvent::TextDelta { id, text, provider_metadata }) => {
                            phase(&mut commentary, &id, provider_metadata.as_ref(), &metadata_key);
                            chunks.push((id, text));
                        }
                        Ok(LLMEvent::StepFinish { usage, .. }) => {
                            let (c, t) = crate::usage::record(usage.as_ref(), &costs);
                            cost += c;
                            tokens.add(&t);
                            any_usage = true;
                        }
                        Ok(_) => {}
                    }
                }
            }
        }
        if any_usage {
            let _ = self.app.bus.commit_one(SessionEvent::UsageRecorded {
                session_id: session.id.clone(),
                source: "title".into(),
                cost,
                tokens,
            });
        }
        if failed {
            return None;
        }
        let out: String = chunks.into_iter().filter(|(id, _)| !commentary.contains(id)).map(|(_, text)| text).collect();
        out.split('\n').map(str::trim).find(|l| !l.is_empty()).map(str::to_string)
    }

    pub async fn generate_title(self: &Arc<Self>, session_id: &str) -> anyhow::Result<()> {
        let Ok(session) = self.app.sessions.get(session_id) else { return Ok(()) };
        let msgs = self.app.db.session_messages(session_id)?;
        let Some((first_id, first)) = msgs.iter().find_map(|(_, m)| match m {
            Message::User { id, text, .. } => Some((id.clone(), text.clone())),
            _ => None,
        }) else {
            return Ok(());
        };
        // An already titled session is renamed from the conversation so far.
        let text = if is_untitled(&session) {
            message_prompt(&first)
        } else {
            match crate::history::load(&self.app.db, session_id, crate::history::Boundary::Latest) {
                Ok(messages) => conversation_text(&first_id, &first, &messages),
                Err(_) => message_prompt(&first),
            }
        };
        // `context.selectTitle` reads only the title agent and the models, not the step's full context
        // (`context.select`: tools, MCP, skills, instructions), so the title request is not held behind that work.
        let location = self.location(&session.location.directory).await?;
        let agents = self.app.plugins.agents(&location, &self.app.global.home).await;
        let Some(agent) = agents.items.get("title").cloned() else { return Ok(()) };
        let catalog = self.models.catalog_at(&location.entries, Some(&location.key)).await?;
        let config_model = oc_config::fold::model(&location.entries).map(|m| oc_schema::types::ModelRef {
            id: m.model.clone(),
            provider_id: m.provider_id.clone(),
            variant: m.variant.clone(),
        });
        let session_model = session
            .model
            .clone()
            .or_else(|| agents.select(session.agent.as_deref()).and_then(|a| crate::runner::agent_model(&catalog, a)));
        let primary = self
            .models
            .resolve_for_session(
                Some(&location.key),
                &catalog,
                session_id,
                session_model.as_ref(),
                config_model.as_ref(),
                crate::plugin_request::Kind::Title,
                &agent.id,
            )
            .await
            .ok();
        // The title model at its lowest reasoning variant: the agent's model, the provider's small model, or the
        // primary model when it has a variant below the one the session selected.
        let candidate: Option<(String, String, Option<String>)> = match (&agent.model, &primary) {
            (Some(m), _) => catalog
                .model(&m.provider_id, &m.id)
                .map(|info| (info.provider_id.clone(), info.id.clone(), m.variant.clone().or_else(|| minimal_variant(info)))),
            (None, Some(p)) => match catalog.small_model(&p.model.provider_id) {
                Some(small) => Some((small.provider_id.clone(), small.id.clone(), minimal_variant(small))),
                None => catalog.model(&p.model.provider_id, &p.model.id).and_then(|info| {
                    lower_variant(info, p.variant.as_deref()).map(|v| (info.provider_id.clone(), info.id.clone(), Some(v)))
                }),
            },
            _ => None,
        };
        let preferred = match candidate {
            Some((provider, id, variant)) => {
                let candidate = oc_schema::types::ModelRef { id, provider_id: provider, variant };
                self.models
                    .resolve_selected(Some(&location.key), &catalog, session_id, &candidate, crate::plugin_request::Kind::Title, &agent.id)
                    .await
                    .ok()
            }
            None => None,
        };
        let Some(selected) = preferred.clone().or(primary.clone()) else { return Ok(()) };
        let agent_model = agent.model.is_some();
        let mut title = self.title_attempt(&session, &location.key, agent.system.as_deref(), agent_model, &text, &selected).await;
        if title.is_none() {
            if let (Some(p), true) = (&primary, preferred.is_some()) {
                if p.model_ref() != selected.model_ref() {
                    title = self.title_attempt(&session, &location.key, agent.system.as_deref(), agent_model, &text, p).await;
                }
            }
        }
        let Some(title) = title.filter(|t| !t.is_empty()) else { return Ok(()) };
        let title = truncate(&title);
        let current = self.app.sessions.get(session_id)?;
        if current.title != session.title || current.title.as_deref() == Some(title.as_str()) {
            return Ok(());
        }
        self.app.sessions.rename(session_id, &title)?;
        Ok(())
    }
}

/// Tracks the text blocks a Responses model marked as commentary (`phase`), which a title leaves out.
fn phase(commentary: &mut std::collections::HashSet<String>, id: &str, metadata: Option<&oc_ai::ProviderMetadata>, key: &str) {
    match metadata.and_then(|m| m.get(key)).and_then(|m| m.get("phase")) {
        Some(serde_json::Value::String(p)) if p == "commentary" => {
            commentary.insert(id.to_string());
        }
        Some(serde_json::Value::String(p)) if p == "final_answer" => {
            commentary.remove(id);
        }
        Some(serde_json::Value::Null) => {
            commentary.remove(id);
        }
        _ => {}
    }
}

/// Variant IDs that minimize reasoning output, in preference order.
const MINIMAL_REASONING_VARIANTS: [&str; 3] = ["none", "minimal", "low"];

fn variant_ids(info: &oc_ai::catalog::types::ModelInfo) -> Vec<&str> {
    info.variants.iter().map(|x| x.id.as_str()).collect()
}

fn minimal_variant(info: &oc_ai::catalog::types::ModelInfo) -> Option<String> {
    minimal_of(&variant_ids(info))
}

fn lower_variant(info: &oc_ai::catalog::types::ModelInfo, selected: Option<&str>) -> Option<String> {
    lower_of(&variant_ids(info), selected)
}

/// `minimalVariant`
pub fn minimal_of(variants: &[&str]) -> Option<String> {
    MINIMAL_REASONING_VARIANTS.into_iter().find(|v| variants.contains(v)).map(String::from)
}

/// `lowerVariant`: `none` and `minimal` always reduce reasoning. `low` can enable reasoning on a model whose
/// default does not reason, so it is only used when it ranks below the variant the session selected.
pub fn lower_of(variants: &[&str], selected: Option<&str>) -> Option<String> {
    let rank = |id: Option<&str>| -> i64 {
        id.and_then(|id| variants.iter().position(|x| *x == id)).map(|i| i as i64).unwrap_or(-1)
    };
    MINIMAL_REASONING_VARIANTS
        .into_iter()
        .find(|v| rank(Some(v)) != -1 && (*v != "low" || rank(Some(v)) < rank(selected)))
        .map(String::from)
}

// Framing the input as text to name keeps models from answering or carrying out the request instead.
pub fn message_prompt(text: &str) -> String {
    format!("Here is the first message of the conversation:\n\n<message>\n{text}\n</message>\n\nReply with only the title.")
}

pub fn conversation_prompt(text: &str) -> String {
    format!("Here is the conversation so far:\n\n<conversation>\n{text}\n</conversation>\n\nReply with only the title.")
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// JS `s.slice(0, n)` on UTF-16 units.
fn js_head(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().collect();
    if units.len() <= n { s.to_string() } else { String::from_utf16_lossy(&units[..n]) }
}

/// JS `s.slice(-n)` on UTF-16 units (`n` > 0).
fn js_tail(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().collect();
    if units.len() <= n { s.to_string() } else { String::from_utf16_lossy(&units[units.len() - n..]) }
}

/// The re-title input for an already titled session: the original request and the newest conversation that fits.
pub fn conversation_text(first_id: &str, first: &str, messages: &[Message]) -> String {
    let js_trim = oc_tools::util::js::trim;
    let original = format!("Original request:\n{}", js_head(first, MAX_FIRST_MESSAGE_LENGTH));
    let recent = messages
        .iter()
        .filter_map(|m| match m {
            Message::User { id, text, .. } if id != first_id => Some(format!("User: {}", js_trim(text))),
            Message::Assistant(a) => {
                let text = a
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        oc_schema::message::AssistantContent::Text { text, .. } => Some(js_trim(text)),
                        _ => None,
                    })
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                (!text.is_empty()).then(|| format!("Assistant: {text}"))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if recent.is_empty() {
        return conversation_prompt(&original);
    }
    let prefix = format!("{original}\n\nRecent conversation:\n");
    let budget = MAX_CONTEXT_LENGTH as i64 - utf16_len(&conversation_prompt("")) as i64 - utf16_len(&prefix) as i64;
    // `slice(-budget)`: a budget of zero or less keeps everything (or drops the first characters), never reached
    // with a first message cut to 2,000 characters.
    let tail = if budget > 0 { js_tail(&recent, budget as usize) } else { recent };
    conversation_prompt(&format!("{prefix}{tail}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // session-title.test.ts (#62267): Responses mark commentary text blocks by `phase`, which a title leaves out.
    #[test]
    fn commentary_phases() {
        let meta = |phase: serde_json::Value| -> oc_ai::ProviderMetadata { json!({"openai": {"phase": phase}}).as_object().unwrap().clone() };
        let mut commentary = std::collections::HashSet::new();
        phase(&mut commentary, "a", Some(&meta(json!("commentary"))), "openai");
        phase(&mut commentary, "b", Some(&meta(json!("final_answer"))), "openai");
        phase(&mut commentary, "c", Some(&meta(json!("commentary"))), "other");
        assert_eq!(commentary.iter().collect::<Vec<_>>(), [&"a".to_string()]);
        // A later `final_answer` or `null` phase clears it; a missing one keeps it.
        phase(&mut commentary, "a", None, "openai");
        assert!(commentary.contains("a"));
        phase(&mut commentary, "a", Some(&meta(serde_json::Value::Null)), "openai");
        assert!(commentary.is_empty());
    }

    // session-title.test.ts: which variant of the primary model titles a session without a small model.
    #[test]
    fn primary_variant_selection() {
        // "uses the primary model at its lowest variant without a small model"
        assert_eq!(lower_of(&["none", "low", "high"], Some("high")).as_deref(), Some("none"));
        // "falls back to the selected primary variant when the lowest variant fails": low ranks below high.
        assert_eq!(lower_of(&["low", "high"], Some("high")).as_deref(), Some("low"));
        // "uses the selected primary variant when no lower variant exists"
        assert_eq!(lower_of(&["high", "max"], Some("high")), None);
        // "keeps the primary default variant instead of a low variant that may enable reasoning"
        assert_eq!(lower_of(&["low", "high"], None), None);
        // "uses the primary none variant from its default variant"
        assert_eq!(lower_of(&["none", "low", "high"], None).as_deref(), Some("none"));
        // `low` ranked above the selection is not lower.
        assert_eq!(lower_of(&["high", "low"], Some("high")), None);
        // The agent and small models take the first minimal variant they have.
        assert_eq!(minimal_of(&["high", "minimal", "low"]).as_deref(), Some("minimal"));
        assert_eq!(minimal_of(&["low"]).as_deref(), Some("low"));
        assert_eq!(minimal_of(&["high"]), None);
    }

    fn msg(v: serde_json::Value) -> Message {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn title_inputs() {
        assert_eq!(
            message_prompt("fix the bug"),
            "Here is the first message of the conversation:\n\n<message>\nfix the bug\n</message>\n\nReply with only the title."
        );
        let first = msg(json!({"type": "user", "id": "u1", "time": {"created": 1}, "text": "fix the bug"}));
        let answer = msg(json!({
            "type": "assistant", "id": "a1", "time": {"created": 1}, "agent": "build",
            "model": {"id": "m", "providerID": "p"},
            "content": [{"type": "reasoning", "text": "hmm"}, {"type": "text", "text": "  Done.  "}, {"type": "text", "text": " "}]
        }));
        let later = msg(json!({"type": "user", "id": "u2", "time": {"created": 1}, "text": " now the docs\n"}));
        // Only the original request when nothing followed it.
        assert_eq!(
            conversation_text("u1", "fix the bug", std::slice::from_ref(&first)),
            "Here is the conversation so far:\n\n<conversation>\nOriginal request:\nfix the bug\n</conversation>\n\nReply with only the title."
        );
        assert_eq!(
            conversation_text("u1", "fix the bug", &[first.clone(), answer.clone(), later.clone()]),
            "Here is the conversation so far:\n\n<conversation>\nOriginal request:\nfix the bug\n\nRecent conversation:\nAssistant: Done.\n\nUser: now the docs\n</conversation>\n\nReply with only the title."
        );
        // The first message is cut to 2,000 characters and the newest conversation fills the rest of 8,000.
        let long_first = "f".repeat(3_000);
        let long_later = msg(json!({"type": "user", "id": "u2", "time": {"created": 1}, "text": "x".repeat(10_000)}));
        let text = conversation_text("u1", &long_first, &[long_later]);
        assert!(text.contains(&format!("Original request:\n{}\n\n", "f".repeat(2_000))));
        assert!(!text.contains(&"f".repeat(2_001)));
        assert_eq!(text.encode_utf16().count(), MAX_CONTEXT_LENGTH);
    }
}
