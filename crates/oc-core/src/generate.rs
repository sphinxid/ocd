//! Stateless generation (`core/session/generate.ts`, `core/generate.ts`).

use std::sync::Arc;

use oc_ai::types::{ContentPart, Message as LlmMessage, Role};
use oc_ai::{AIError, CancellationToken, ErrorKind, LLMRequest};
use oc_schema::types::ModelRef;

use crate::runner::Engine;

/// `Generate.Error` (`core/src/generate.ts`); the server maps each to its protocol error.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum GenerateError {
    /// `ModelNotFoundError` (404): no model of any type has this ref.
    #[error("{message}")]
    ModelNotFound { provider_id: String, model_id: String, message: String },
    /// `UnsupportedModelError` (422): the model exists but cannot serve this request.
    #[error("{message}")]
    UnsupportedModel { provider_id: String, model_id: String, capability: String, message: String },
    /// `InvalidRequestError` (400).
    #[error("{message}")]
    InvalidRequest { message: String, field: Option<String> },
    /// `ProviderRequestError` (502): the provider rejected or failed the request after it was sent.
    #[error("{message}")]
    ProviderRequest { provider_id: String, reason: &'static str, message: String, status: Option<u16>, retry_after_ms: Option<i64> },
    /// `ServiceUnavailableError` (503).
    #[error("{message}")]
    Unavailable { message: String, service: Option<String> },
}

/// A model's type (`Model.Info.type`, chat when absent). The catalog has no other types yet.
fn model_type(_model: &oc_ai::catalog::ModelInfo) -> &'static str {
    "chat"
}

/// `missing`: a ref no model of this type has: another type's model (422) or no model at all (404).
fn missing(catalog: &oc_ai::catalog::Catalog, model: &ModelRef, kind: &str) -> GenerateError {
    let other = catalog.available_models().into_iter().find(|m| m.provider_id == model.provider_id && m.id == model.id);
    match other {
        Some(other) => GenerateError::UnsupportedModel {
            provider_id: model.provider_id.clone(),
            model_id: model.id.clone(),
            capability: kind.into(),
            message: format!("{}/{} is a {} model, not a {kind} model", model.provider_id, model.id, model_type(other)),
        },
        None => GenerateError::ModelNotFound {
            provider_id: model.provider_id.clone(),
            model_id: model.id.clone(),
            message: format!("Model not found: {}/{}", model.provider_id, model.id),
        },
    }
}

/// `resolutionError`: a model resolution failure for an explicit ref.
fn resolution_error(model: &ModelRef, kind: &str, error: &oc_ai::catalog::resolve::ResolveError) -> GenerateError {
    use oc_ai::catalog::resolve::ResolveError as R;
    match error {
        R::VariantUnavailable { .. } => GenerateError::InvalidRequest { message: error.to_string(), field: Some("model.variant".into()) },
        R::UnsupportedPackage { package, .. } | R::AisdkPackage { package, .. } => GenerateError::UnsupportedModel {
            provider_id: model.provider_id.clone(),
            model_id: model.id.clone(),
            capability: kind.into(),
            message: format!("{}/{} has no {kind} API opencode can call (package {package})", model.provider_id, model.id),
        },
        _ => GenerateError::InvalidRequest { message: error.to_string(), field: Some("model".into()) },
    }
}

/// `failureReason`.
fn failure_reason(error: &AIError, status: Option<u16>) -> &'static str {
    match &error.kind {
        ErrorKind::Authentication => "authentication",
        ErrorKind::RateLimit { .. } | ErrorKind::QuotaExceeded { .. } => "rate_limit",
        ErrorKind::ContentPolicy => "content_filter",
        ErrorKind::InvalidRequest { .. } => "invalid_request",
        ErrorKind::ProviderInternal { .. } if matches!(status, Some(503 | 529)) => "overloaded",
        _ => "provider",
    }
}

/// `providerError`: a provider failure as the generate API reports it.
pub fn provider_error(model: &ModelRef, error: &AIError) -> GenerateError {
    match &error.kind {
        ErrorKind::InvalidRequest { .. } if error.http.is_none() => {
            return GenerateError::InvalidRequest { message: error.message.clone(), field: None };
        }
        ErrorKind::UnsupportedOperation { .. } => return GenerateError::InvalidRequest { message: error.message.clone(), field: None },
        _ => {}
    }
    let status = error.http.as_ref().map(|h| h.status);
    let retry_after_ms = match &error.kind {
        ErrorKind::RateLimit { retry_after_ms, .. }
        | ErrorKind::QuotaExceeded { retry_after_ms }
        | ErrorKind::ProviderInternal { retry_after_ms } => retry_after_ms.map(|ms| ms.round() as i64),
        _ => None,
    };
    GenerateError::ProviderRequest {
        provider_id: model.provider_id.clone(),
        reason: failure_reason(error, status),
        message: error.message.clone(),
        status,
        retry_after_ms,
    }
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

    /// `Generate.text`: a one-off prompt against `model` or the default model, retried only when the
    /// provider clearly did not run it (`ModelRetry.retryOneShot`).
    pub async fn generate_text(self: &Arc<Self>, directory: &str, prompt: &str, model: Option<ModelRef>) -> Result<String, GenerateError> {
        let location = self.location(directory).await.map_err(|e| GenerateError::Unavailable { message: e.to_string(), service: None })?;
        let catalog = self.models.catalog_at(&location.entries, Some(&location.key)).await.map_err(|e| GenerateError::Unavailable { message: e.to_string(), service: None })?;
        let pick = match &model {
            Some(m) => catalog.model(&m.provider_id, &m.id).map(|_| m.clone()),
            None => catalog.select(None).map(|m| ModelRef { id: m.id.clone(), provider_id: m.provider_id.clone(), variant: None }),
        };
        let Some(r) = pick else {
            return Err(match &model {
                Some(m) => missing(&catalog, m, "chat"),
                None => GenerateError::Unavailable { message: "No model specified and no supported model is available".into(), service: None },
            });
        };
        let cred = self.models.credential(Some(&location.key), &catalog, &r.provider_id).await;
        let resolved = self.models.resolve_model(Some(&location.key), &catalog, &r.provider_id, &r.id, r.variant.as_deref(), cred.as_ref()).await.map_err(|e| {
            match &model {
                Some(m) => resolution_error(m, "chat", &e),
                None => GenerateError::Unavailable { message: e.to_string(), service: Some(r.provider_id.clone()) },
            }
        })?;
        let mut req = LLMRequest::new(resolved.language_model.clone());
        req.messages = vec![LlmMessage::new(Role::User, vec![ContentPart::text(prompt)])];
        let mut headers = indexmap::IndexMap::new();
        // Gateways require session attribution even for a stateless call; no Session is stored.
        headers.insert("x-opencode-session".to_string(), oc_schema::id::session());
        req.http = Some(oc_ai::types::HttpOptions { headers: Some(headers), ..Default::default() });
        let response = crate::retry::one_shot(|| self.client.generate(req.clone(), CancellationToken::new()))
            .await
            .map_err(|e| provider_error(&r, &e))?;
        Ok(response.text())
    }

    /// `Generate.image` / `Generate.decisions` (`kind` = `image` / `decision`). Image and decision models are not
    /// ported yet, so a ref resolves like upstream's when no model of that type matches: another type's model is a
    /// 422, anything else a 404.
    pub async fn generate_media(self: &Arc<Self>, directory: &str, kind: &str, model: &ModelRef) -> Result<serde_json::Value, GenerateError> {
        let location = self.location(directory).await.map_err(|e| GenerateError::Unavailable { message: e.to_string(), service: None })?;
        let catalog = self.models.catalog_at(&location.entries, Some(&location.key)).await.map_err(|e| GenerateError::Unavailable { message: e.to_string(), service: None })?;
        let typed = catalog
            .available_models()
            .into_iter()
            .find(|m| m.provider_id == model.provider_id && m.id == model.id && model_type(m) == kind);
        if let Some(found) = typed {
            // `fromCatalogMedia` with no runtime for this type: the package cannot serve it.
            let package = found.package.clone().unwrap_or_else(|| "unknown".into());
            return Err(resolution_error(
                model,
                kind,
                &oc_ai::catalog::resolve::ResolveError::UnsupportedPackage {
                    provider_id: model.provider_id.clone(),
                    model_id: model.id.clone(),
                    package,
                },
            ));
        }
        Err(missing(&catalog, model, kind))
    }
}
