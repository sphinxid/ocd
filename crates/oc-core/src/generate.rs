//! Stateless generation (`core/session/generate.ts`, `core/generate.ts`).

use std::sync::Arc;

use oc_ai::catalog::resolve::{MediaModel, MediaType};
use oc_ai::decision::{DecisionPart, DecisionRequest};
use oc_ai::types::{ContentPart, Message as LlmMessage, Role};
use oc_ai::{AIError, CancellationToken, ErrorKind, LLMRequest};
use oc_schema::types::ModelRef;
use serde_json::{json, Map, Value};

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

/// A model's type (`Model.Info.type`, chat when absent).
fn model_type(model: &oc_ai::catalog::ModelInfo) -> &str {
    model.kind.as_deref().unwrap_or("chat")
}

/// `models.available(["chat", "image", "decision"])`.
fn generation_models(catalog: &oc_ai::catalog::Catalog) -> Vec<&oc_ai::catalog::ModelInfo> {
    catalog.available_models_of(&["chat".to_string(), "image".to_string(), "decision".to_string()])
}

/// `missing`: a ref no model of this type has: another type's model (422) or no model at all (404).
fn missing(catalog: &oc_ai::catalog::Catalog, model: &ModelRef, kind: &str) -> GenerateError {
    let other = generation_models(catalog).into_iter().find(|m| m.provider_id == model.provider_id && m.id == model.id);
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

/// `SessionGenerate.Result`: the text and the usage it was recorded with.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionGenerated {
    pub text: String,
    pub cost: f64,
    pub tokens: oc_schema::types::TokenUsage,
}

impl Engine {
    /// `SessionGenerate.generate`: text from the session's context without mutating its history. Usage is
    /// reported like a step's (`session.usage.recorded`, source `generate`) and added to the session's totals.
    pub async fn session_generate(self: &Arc<Self>, session_id: &str, prompt: &str) -> anyhow::Result<SessionGenerated> {
        let sel = self.select(session_id).await?;
        let (_, update) = crate::instruction_state::preview(&self.app.bus, session_id, &sel.sources)?;
        let loaded = self.load_for(sel, crate::plugin_request::Kind::Generate).await?;
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
            loaded.sel.agent.system.as_deref(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        tracing::info!(sessionID = session_id, providerID = %loaded.resolved.model.provider_id, modelID = %loaded.resolved.model.id, "sending session generation request");
        let response = crate::retry::buffered(|| self.client.generate_with(req.clone(), CancellationToken::new(), shaped.http.clone())).await?;
        let (cost, tokens) = crate::usage::record(response.usage.as_ref(), &crate::usage::costs(&loaded.resolved));
        self.app.bus.commit_one(oc_schema::event::SessionEvent::UsageRecorded {
            session_id: session_id.to_string(),
            source: "generate".into(),
            cost,
            tokens: tokens.clone(),
        })?;
        Ok(SessionGenerated { text: response.text(), cost, tokens })
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

    /// `ModelResolver.resolveImage` / `resolveDecision` at `directory`'s location: an enabled model of that type by
    /// ref, else upstream's `missing` (another type's model is a 422, anything else a 404).
    async fn media_model(self: &Arc<Self>, directory: &str, kind: MediaType, model: &ModelRef) -> Result<MediaModel, GenerateError> {
        let location = self.location(directory).await.map_err(|e| GenerateError::Unavailable { message: e.to_string(), service: None })?;
        let catalog = self
            .models
            .catalog_at(&location.entries, Some(&location.key))
            .await
            .map_err(|e| GenerateError::Unavailable { message: e.to_string(), service: None })?;
        let Some(selected) = catalog
            .available_models_of(&[kind.as_str().to_string()])
            .into_iter()
            .find(|m| m.provider_id == model.provider_id && m.id == model.id)
            .cloned()
        else {
            return Err(missing(&catalog, model, kind.as_str()));
        };
        let credential = self.models.credential(Some(&location.key), &catalog, &selected.provider_id).await;
        oc_ai::catalog::resolve::resolve_media(
            catalog.provider(&selected.provider_id),
            &selected,
            kind,
            model.variant.as_deref(),
            credential.as_ref(),
            &self.models.resolve_options(Some(&location.key)),
        )
        .map_err(|e| resolution_error(model, kind.as_str(), &e))
    }

    /// `Generate.image`: generate or edit images and return them inline (`Generate.ImageResult`).
    pub async fn generate_image(self: &Arc<Self>, directory: &str, input: &Value) -> Result<Value, GenerateError> {
        let model: ModelRef = serde_json::from_value(input["model"].clone())
            .map_err(|e| GenerateError::InvalidRequest { message: e.to_string(), field: Some("model".into()) })?;
        let MediaModel::Image(image_model) = self.media_model(directory, MediaType::Image, &model).await? else { unreachable!() };
        let mut request = oc_ai::image::ImageRequest::new(image_model, input["prompt"].as_str().unwrap_or_default());
        request.images = input
            .get("images")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|f| oc_ai::MediaAsset::base64(f["data"].as_str().unwrap_or_default(), f["mime"].as_str().unwrap_or_default()))
            .collect();
        request.n = input.get("n").and_then(Value::as_i64);
        request.aspect = input.get("aspect").and_then(Value::as_str).map(String::from);
        request.resolution = input.get("resolution").and_then(Value::as_str).map(String::from);
        let http = self.client.http().clone();
        let response = crate::retry::one_shot(|| oc_ai::image::generate(&http, request.clone()))
            .await
            .map_err(|e| provider_error(&model, &e))?;
        // URL results are downloaded so every image comes back inline, like the request's own files. Materializing
        // also detects the type of a downloaded image from its bytes when the URL didn't say.
        let mut images = vec![];
        for asset in &response.images {
            let (data, mime) = oc_ai::image::materialize(&http, asset).await.map_err(|e| provider_error(&model, &e))?;
            images.push(json!({ "type": "image", "data": data, "mime": mime }));
        }
        let mut out = Map::new();
        out.insert("images".into(), Value::Array(images));
        if let Some(usage) = &response.usage
            && (usage.input.is_some() || usage.output.is_some())
        {
            out.insert("tokens".into(), tokens(usage.input, usage.output));
        }
        if let Some(native) = response.provider_metadata {
            out.insert("native".into(), Value::Object(native));
        }
        Ok(Value::Object(out))
    }

    /// `Generate.decisions`: one probability-bearing answer per question, in question order
    /// (`Generate.DecisionResult`).
    pub async fn generate_decisions(self: &Arc<Self>, directory: &str, input: &Value) -> Result<Value, GenerateError> {
        let model: ModelRef = serde_json::from_value(input["model"].clone())
            .map_err(|e| GenerateError::InvalidRequest { message: e.to_string(), field: Some("model".into()) })?;
        let MediaModel::Decision(decision_model) = self.media_model(directory, MediaType::Decision, &model).await? else { unreachable!() };
        let parts = input
            .get("input")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|part| match part["type"].as_str() {
                Some("text") => DecisionPart::Text(part["text"].as_str().unwrap_or_default().into()),
                Some("json") => DecisionPart::Json(part["value"].clone()),
                _ => DecisionPart::Media {
                    media: oc_ai::MediaAsset::base64(part["data"].as_str().unwrap_or_default(), part["mime"].as_str().unwrap_or_default()),
                    filename: part.get("name").and_then(Value::as_str).map(String::from),
                },
            })
            .collect();
        let questions = input.get("questions").and_then(Value::as_array).into_iter().flatten().filter_map(|q| q.as_object().cloned()).collect();
        let request = DecisionRequest { model: decision_model, input: parts, questions, provider_options: None, http: None };
        let http = self.client.http().clone();
        let response = crate::retry::one_shot(|| oc_ai::decision::generate(&http, request.clone()))
            .await
            .map_err(|e| provider_error(&model, &e))?;
        let mut out = Map::new();
        out.insert("answers".into(), Value::Array(response.answers.into_iter().map(http_answer).collect()));
        if let Some(cost) = response.usage.as_ref().and_then(|u| u.cost) {
            out.insert("cost".into(), oc_ai::json::num(cost));
        }
        if let Some(usage) = &response.usage
            && (usage.input_tokens.is_some() || usage.output_tokens.is_some())
        {
            out.insert("tokens".into(), tokens(usage.input_tokens, usage.output_tokens));
        }
        if let Some(native) = response.provider_metadata {
            out.insert("native".into(), Value::Object(native));
        }
        Ok(Value::Object(out))
    }
}

/// `{input, output}` token counts, absent ones left out.
fn tokens(input: Option<f64>, output: Option<f64>) -> Value {
    let mut out = Map::new();
    if let Some(i) = input {
        out.insert("input".into(), oc_ai::json::num(i));
    }
    if let Some(o) = output {
        out.insert("output".into(), oc_ai::json::num(o));
    }
    Value::Object(out)
}

/// An answer in the `Generate.DecisionAnswer` shape (`type` first, the schema's fields only).
fn http_answer(answer: oc_ai::json::Object) -> Value {
    let kind = answer.get("type").cloned().unwrap_or(Value::Null);
    let mut out = Map::new();
    out.insert("type".into(), kind.clone());
    out.insert("name".into(), answer.get("name").cloned().unwrap_or(Value::Null));
    let keys: &[&str] = match kind.as_str() {
        Some("predicate") => &["probability"],
        Some("choice") => &["choice", "probabilities", "confidence"],
        Some("score") => &["score", "probabilities", "confidence"],
        _ => &["reason", "message"],
    };
    for key in keys {
        if let Some(v) = answer.get(*key) {
            out.insert((*key).into(), v.clone());
        }
    }
    Value::Object(out)
}
