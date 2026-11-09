//! Decision models (`packages/ai/src/decision.ts`, `decision-client.ts`, `route/decision.ts`): typed predicate,
//! choice and score questions about some input, answered with probabilities.

pub mod openai;
pub mod system_one;

use crate::error::AIError;
use crate::json::{merge_records, Object};
use crate::media::MediaAsset;
use crate::route::auth::Auth;
use crate::route::media::{self, MediaBody, MediaEndpoint, MediaResponse};
use crate::types::HttpOptions;
use indexmap::IndexMap;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

/// One input part (`DecisionPart`).
#[derive(Debug, Clone, PartialEq)]
pub enum DecisionPart {
    Text(String),
    Json(Value),
    Media { media: MediaAsset, filename: Option<String> },
}

/// `DecisionUsage`: what decision hosts report; their raw usage stays in the response's provider metadata.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DecisionUsage {
    pub input_tokens: Option<f64>,
    pub output_tokens: Option<f64>,
    pub total_tokens: Option<f64>,
    /// USD the host says it charged for this request.
    pub cost: Option<f64>,
}

/// `DecisionResponse`. Answers are JSON objects in the ai package's shapes (`{name, type, ...}`).
#[derive(Debug, Clone, Default)]
pub struct DecisionResponse {
    pub answers: Vec<Object>,
    pub usage: Option<DecisionUsage>,
    pub provider_metadata: Option<Object>,
}

/// `DecisionRequest`. Questions are JSON objects in the `DecisionQuestion` shapes.
#[derive(Clone)]
pub struct DecisionRequest {
    pub model: DecisionModel,
    pub input: Vec<DecisionPart>,
    pub questions: Vec<Object>,
    pub provider_options: Option<Object>,
    pub http: Option<HttpOptions>,
}

/// One decision API: how a request becomes its JSON body and how its response becomes a [`DecisionResponse`].
pub trait DecisionProtocol: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    /// The host only takes inline data, so URL media is downloaded before `body`.
    fn inline_media(&self) -> bool {
        false
    }
    fn body(&self, request: &DecisionRequest) -> Result<Object, AIError>;
    fn decode(&self, response: &MediaResponse, request: &DecisionRequest, metadata_key: &str) -> Result<DecisionResponse, AIError>;
}

/// `DecisionRoute.Route`.
#[derive(Clone)]
pub struct DecisionRoute {
    pub id: String,
    pub provider: String,
    /// ProviderMetadata namespace for responses; defaults to the provider.
    pub metadata_key: String,
    pub protocol: Arc<dyn DecisionProtocol>,
    pub endpoint: MediaEndpoint<DecisionRequest>,
    pub auth: Arc<dyn Auth>,
    pub headers: Option<IndexMap<String, String>>,
    pub http: Option<HttpOptions>,
}

impl DecisionRoute {
    /// `DecisionRoute.make(input)`.
    pub fn new(id: &str, provider: &str, protocol: Arc<dyn DecisionProtocol>, endpoint: MediaEndpoint<DecisionRequest>) -> Self {
        Self {
            id: id.into(),
            provider: provider.into(),
            metadata_key: provider.into(),
            protocol,
            endpoint,
            auth: crate::route::auth::none(),
            headers: None,
            http: None,
        }
    }

    /// `route.with({provider})`: a new provider gets its own metadata key unless the patch names one.
    pub fn with_provider(mut self, provider: &str, metadata_key: Option<&str>) -> Self {
        self.provider = provider.into();
        self.metadata_key = metadata_key.unwrap_or(provider).into();
        self
    }

    /// `route.with({endpoint: {baseURL}})`.
    pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
        if base_url.is_some() {
            self.endpoint.base_url = base_url;
        }
        self
    }

    pub fn with_auth(mut self, auth: Arc<dyn Auth>) -> Self {
        self.auth = auth;
        self
    }

    /// `route.with({headers})`: merged over the route's own.
    pub fn with_headers(mut self, headers: Option<IndexMap<String, String>>) -> Self {
        if let Some(h) = headers {
            let mut merged = self.headers.take().unwrap_or_default();
            for (k, v) in h {
                merged.insert(k, v);
            }
            self.headers = Some(merged);
        }
        self
    }

    /// `route.with({http})`: replaces the route's HTTP options when given.
    pub fn with_http(mut self, http: Option<HttpOptions>) -> Self {
        if http.is_some() {
            self.http = http;
        }
        self
    }

    /// `route.model({id, compatibility, providerOptions})`.
    pub fn model(self, id: &str, media_input: Option<&str>, provider_options: Option<Object>) -> DecisionModel {
        DecisionModel {
            id: id.into(),
            provider: self.provider.clone(),
            http: self.http.clone(),
            media_input: media_input.map(String::from),
            provider_options,
            route: Arc::new(self),
        }
    }
}

/// `DecisionModel`.
#[derive(Clone)]
pub struct DecisionModel {
    pub id: String,
    pub provider: String,
    pub route: Arc<DecisionRoute>,
    pub http: Option<HttpOptions>,
    /// `compatibility.mediaInput`: where a System One host reads media (`assets`, `state` or `root`).
    pub media_input: Option<String>,
    pub provider_options: Option<Object>,
}

impl std::fmt::Debug for DecisionModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionModel").field("id", &self.id).field("provider", &self.provider).field("route", &self.route.id).finish()
    }
}

/// The `Decision.Request` constructor's checks: only what makes a request meaningless.
fn validate(questions: &[Object]) -> Result<(), AIError> {
    if questions.is_empty() {
        return Err(AIError::invalid_request("Decision questions must not be empty"));
    }
    let names: HashSet<&str> = questions.iter().filter_map(|q| q.get("name").and_then(Value::as_str)).collect();
    if names.len() != questions.len() {
        return Err(AIError::invalid_request("Decision question names must be unique"));
    }
    Ok(())
}

fn pick(source: &Object, keys: &[&str]) -> Object {
    let mut out = Object::new();
    for key in keys {
        if let Some(v) = source.get(*key) {
            out.insert((*key).into(), v.clone());
        }
    }
    out
}

/// A question as the `Decision.Request` schema decodes it: its fields in the schema's order (`name`, `type`,
/// `instructions`, then `choices` or `levels`), unknown keys dropped.
pub fn question(input: &Object) -> Object {
    let mut out = pick(input, &["name", "type", "instructions"]);
    let list = |key: &str, fields: &[&str]| -> Option<Value> {
        let items = input.get(key)?.as_array()?;
        Some(Value::Array(items.iter().map(|i| i.as_object().map(|o| Value::Object(pick(o, fields))).unwrap_or(i.clone())).collect()))
    };
    match input.get("type").and_then(Value::as_str) {
        Some("choice") => {
            if let Some(v) = list("choices", &["value", "description"]) {
                out.insert("choices".into(), v);
            }
        }
        Some("score") => {
            if let Some(v) = list("levels", &["label", "description"]) {
                out.insert("levels".into(), v);
            }
        }
        _ => {}
    }
    out
}

/// The request as the protocol sees it: HTTP overlays and provider options merged.
pub fn resolve(request: &DecisionRequest) -> DecisionRequest {
    let route = &request.model.route;
    let route_http = route.headers.clone().map(HttpOptions::headers);
    let http = HttpOptions::merge(&[route_http.as_ref(), request.model.http.as_ref(), request.http.as_ref()]);
    let provider_options = merge_records(&[request.model.provider_options.as_ref(), request.provider_options.as_ref()]);
    let questions = request.questions.iter().map(question).collect();
    DecisionRequest { http, provider_options, questions, ..request.clone() }
}

/// The body the route would send (tests and wire fixtures); URL media stays as is.
pub fn body(request: &DecisionRequest) -> Result<Object, AIError> {
    validate(&request.questions)?;
    request.model.route.protocol.body(&resolve(request))
}

/// `DecisionClient.generate`: send the request, then check the answers match the questions.
pub async fn generate(client: &reqwest::Client, request: DecisionRequest) -> Result<DecisionResponse, AIError> {
    validate(&request.questions)?;
    let route = request.model.route.clone();
    let mut request = request;
    if route.protocol.inline_media() {
        for part in request.input.iter_mut() {
            if let DecisionPart::Media { media: asset, .. } = part
                && let crate::media::MediaSource::Url { url, media_type, .. } = &asset.source
            {
                let data = media::download(client, url, None).await?;
                *asset = MediaAsset::bytes(data, media_type.as_deref());
            }
        }
    }
    let resolved = resolve(&request);
    let body = MediaBody::json(route.protocol.body(&resolved)?);
    let response = media::submit(client, &route.endpoint, route.auth.as_ref(), resolved.http.as_ref(), &resolved, &body).await?;
    let response = route.protocol.decode(&response, &resolved, &route.metadata_key)?;
    if answers_match(&request.questions, &response.answers) {
        return Ok(response);
    }
    let mut error = AIError::invalid_output(&route.id, "Decision answers do not match the requested questions");
    error.cause = Some(crate::json::encode(&Value::Array(response.answers.iter().cloned().map(Value::Object).collect())));
    Err(error)
}

fn same_members(expected: &[Value], received: &[Value]) -> bool {
    let unique: Vec<&Value> = received.iter().fold(vec![], |mut acc, v| {
        if !acc.contains(&v) {
            acc.push(v);
        }
        acc
    });
    expected.len() == received.len() && unique.len() == received.len() && received.iter().all(|r| expected.contains(r))
}

/// `answersMatch`: host bugs (a missing or extra answer, an option that wasn't offered, a score off the scale) fail
/// the whole call.
fn answers_match(questions: &[Object], answers: &[Object]) -> bool {
    questions.len() == answers.len()
        && questions.iter().zip(answers).all(|(question, answer)| {
            if answer.get("name") != question.get("name") {
                return false;
            }
            let kind = answer.get("type").and_then(Value::as_str);
            if kind == Some("error") {
                return true;
            }
            let probabilities: Vec<Value> = answer
                .get("probabilities")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|p| p.get("value").cloned()).collect())
                .unwrap_or_default();
            match question.get("type").and_then(Value::as_str) {
                Some("predicate") => kind == Some("predicate"),
                Some("choice") => {
                    if kind != Some("choice") {
                        return false;
                    }
                    let values: Vec<Value> = question
                        .get("choices")
                        .and_then(Value::as_array)
                        .map(|c| c.iter().filter_map(|c| c.get("value").cloned()).collect())
                        .unwrap_or_default();
                    answer.get("choice").is_some_and(|c| values.contains(c)) && same_members(&values, &probabilities)
                }
                _ => {
                    let levels = question.get("levels").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
                    let score = answer.get("score").and_then(Value::as_f64);
                    if kind != Some("score") || score.is_none_or(|s| s < 0.0 || s > levels as f64 - 1.0) {
                        return false;
                    }
                    let expected: Vec<Value> = (0..levels).map(|l| Value::from(l as i64)).collect();
                    let received: Vec<Value> = probabilities.iter().map(|v| v.as_f64().map(crate::json::num).unwrap_or(v.clone())).collect();
                    same_members(&expected, &received)
                }
            }
        })
}

/// `MediaProtocol.identity(...).decodeJson` for a decision protocol.
pub(crate) fn identity(id: &str, name: &str, provider: &str) -> media::Identity {
    media::Identity::new(id, name, provider)
}
