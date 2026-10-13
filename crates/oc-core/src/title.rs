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
        let mut out = String::new();
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
                        Ok(LLMEvent::TextDelta { text, .. }) => out.push_str(&text),
                        Ok(LLMEvent::StepFinish { usage, .. }) => {
                            let t = crate::publisher::tokens(usage.as_ref());
                            cost += crate::usage::calculate_cost(&costs, &t);
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
        out.split('\n').map(str::trim).find(|l| !l.is_empty()).map(str::to_string)
    }

    pub async fn generate_title(self: &Arc<Self>, session_id: &str) -> anyhow::Result<()> {
        let Ok(session) = self.app.sessions.get(session_id) else { return Ok(()) };
        let msgs = self.app.db.session_messages(session_id)?;
        let Some(first) = msgs.iter().find_map(|(_, m)| match m {
            Message::User { text, .. } => Some(text.clone()),
            _ => None,
        }) else {
            return Ok(());
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
        let primary = self.models.resolve_for_session(Some(&location.key), &catalog, session_id, session.model.as_ref(), config_model.as_ref()).await.ok();
        let preferred = match (&agent.model, &primary) {
            (Some(m), _) => {
                let cred = self.models.credential(Some(&location.key), &catalog, &m.provider_id).await;
                self.models.resolve_model(Some(&location.key), &catalog, &m.provider_id, &m.id, m.variant.as_deref(), cred.as_ref()).await.ok()
            }
            (None, Some(p)) => match catalog.small_model(&p.model.provider_id) {
                Some(small) => {
                    let variant = ["none", "minimal", "low"].into_iter().find(|v| small.variants.iter().any(|x| x.id == *v));
                    let cred = self.models.credential(Some(&location.key), &catalog, &small.provider_id).await;
                    self.models.resolve_model(Some(&location.key), &catalog, &small.provider_id, &small.id, variant, cred.as_ref()).await.ok()
                }
                None => None,
            },
            _ => None,
        };
        let Some(selected) = preferred.clone().or(primary.clone()) else { return Ok(()) };
        let agent_model = agent.model.is_some();
        let mut title = self.title_attempt(&session, &location.key, agent.system.as_deref(), agent_model, &first, &selected).await;
        if title.is_none() {
            if let (Some(p), true) = (&primary, preferred.is_some()) {
                if p.model_ref() != selected.model_ref() {
                    title = self.title_attempt(&session, &location.key, agent.system.as_deref(), agent_model, &first, p).await;
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
