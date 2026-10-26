//! Stateless generation (`core/session/generate.ts`, `core/generate.ts`).

use std::sync::Arc;

use oc_ai::types::{ContentPart, Message as LlmMessage, Role};
use oc_ai::{CancellationToken, LLMRequest};
use oc_schema::types::ModelRef;

use crate::runner::Engine;

#[derive(Debug, thiserror::Error)]
pub enum GenerateError {
    #[error("{0}")]
    ModelSelection(String),
    #[error("{message}")]
    Unavailable { message: String, service: Option<String> },
}

impl Engine {
    /// `SessionGenerate.generate`: text from the session's context without mutating it.
    pub async fn session_generate(self: &Arc<Self>, session_id: &str, prompt: &str) -> anyhow::Result<String> {
        let sel = self.select(session_id).await?;
        let (_, update) = crate::instruction_state::preview(&self.app.bus, session_id, &sel.sources)?;
        let loaded = self.load(sel).await?;
        let (mut req, _) = self.request(&loaded, false);
        req.generation = None;
        if !update.is_empty() {
            req.messages.push(LlmMessage::system(update));
        }
        req.messages.push(LlmMessage::user_text(prompt));
        let shaped = crate::plugin_request::shape(
            &self.app.plugins,
            &loaded.sel.location.key,
            crate::plugin_request::Kind::Generate,
            session_id,
            &loaded.sel.agent.id,
            &loaded.resolved.model_ref(),
            &mut req,
            None,
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        tracing::info!(sessionID = session_id, providerID = %loaded.resolved.model.provider_id, modelID = %loaded.resolved.model.id, "sending session generation request");
        let response = crate::retry::buffered(|| self.client.generate_with(req.clone(), CancellationToken::new(), shaped.http.clone())).await?;
        Ok(response.text())
    }

    /// `Generate.text`: a one-off prompt against `model` or the default model.
    pub async fn generate_text(self: &Arc<Self>, directory: &str, prompt: &str, model: Option<ModelRef>) -> Result<String, GenerateError> {
        let location = self.location(directory).await.map_err(|e| GenerateError::Unavailable { message: e.to_string(), service: None })?;
        let catalog = self.models.catalog_at(&location.entries, Some(&location.key)).await.map_err(|e| GenerateError::Unavailable { message: e.to_string(), service: None })?;
        let pick = match &model {
            Some(m) => catalog.model(&m.provider_id, &m.id).map(|_| m.clone()),
            None => catalog.select(None).map(|m| ModelRef { id: m.id.clone(), provider_id: m.provider_id.clone(), variant: None }),
        };
        let Some(r) = pick else {
            return Err(GenerateError::ModelSelection(match &model {
                Some(m) => format!("Model unavailable: {}/{}", m.provider_id, m.id),
                None => "No model specified and no supported model is available".into(),
            }));
        };
        let cred = self.models.credential(Some(&location.key), &catalog, &r.provider_id).await;
        let resolved = self.models.resolve_model(Some(&location.key), &catalog, &r.provider_id, &r.id, r.variant.as_deref(), cred.as_ref()).await.map_err(|e| {
            if model.is_some() {
                GenerateError::ModelSelection(e.to_string())
            } else {
                GenerateError::Unavailable { message: e.to_string(), service: Some(r.provider_id.clone()) }
            }
        })?;
        let mut req = LLMRequest::new(resolved.language_model.clone());
        req.messages = vec![LlmMessage::new(Role::User, vec![ContentPart::text(prompt)])];
        let mut headers = indexmap::IndexMap::new();
        headers.insert("x-opencode-session".to_string(), oc_schema::id::session());
        req.http = Some(oc_ai::types::HttpOptions { headers: Some(headers), ..Default::default() });
        let response = crate::retry::buffered(|| self.client.generate(req.clone(), CancellationToken::new()))
            .await
            .map_err(|e| GenerateError::Unavailable { message: e.message.clone(), service: Some(r.provider_id.clone()) })?;
        Ok(response.text())
    }
}
