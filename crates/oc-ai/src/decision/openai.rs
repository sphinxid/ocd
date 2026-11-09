//! `protocols/openai-decisions.ts`: OpenAI's `/decisions`, also served by Vercel's gateway.

use super::{identity, DecisionPart, DecisionProtocol, DecisionRequest, DecisionResponse, DecisionRoute, DecisionUsage};
use crate::error::AIError;
use crate::json::{encode, merge_records, Object};
use crate::media::MediaKind;
use crate::route::media::{invalid_request, media_reference, total_tokens, MediaEndpoint, MediaResponse};
use serde_json::{json, Value};
use std::sync::Arc;

pub const ID: &str = "openai-decisions";
const NAME: &str = "OpenAI Decisions";
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const PATH: &str = "/decisions";

fn is_object(value: &Value) -> bool {
    value.is_object()
}

/// `Options` decoding: the known fields must have their types.
fn decode_options(options: &Object) -> Result<(), AIError> {
    let ok = options.get("promptCacheKey").is_none_or(Value::is_string)
        && options.get("safetyIdentifier").is_none_or(Value::is_string)
        && options.get("promptCacheOptions").is_none_or(|v| {
            v.as_object().is_some_and(|o| o.get("mode").is_none_or(Value::is_string) && o.get("ttl").is_none_or(Value::is_string))
        });
    if ok { Ok(()) } else { Err(invalid_request("Expected valid OpenAI decision options")) }
}

fn input_part(request: &DecisionRequest, part: &DecisionPart) -> Result<Value, AIError> {
    match part {
        DecisionPart::Text(text) => Ok(json!({ "type": "input_text", "text": text })),
        // `input_json` takes an object or a list of objects; other JSON values go as text.
        DecisionPart::Json(value) => Ok(
            if is_object(value) || value.as_array().is_some_and(|a| a.iter().all(is_object)) {
                json!({ "type": "input_json", "data": value })
            } else {
                json!({ "type": "input_text", "text": encode(value) })
            },
        ),
        DecisionPart::Media { media, .. } => {
            if media.kind() != MediaKind::Image {
                let kind = match media.kind() {
                    MediaKind::Video => "video",
                    MediaKind::Audio => "audio",
                    MediaKind::Document => "document",
                    _ => "other",
                };
                return Err(AIError::unsupported(
                    "decision.media",
                    format!("{NAME} takes images only; got {kind} media"),
                    Some(&request.model.provider),
                    Some(&request.model.route.id),
                ));
            }
            let reference = media_reference(media, None, &format!("{NAME} image input"))?;
            Ok(json!({ "type": "input_image", "image_url": reference.value() }))
        }
    }
}

/// A lenient count: a malformed value is absent.
fn count(usage: &Object, key: &str) -> Option<f64> {
    usage.get(key).and_then(Value::as_f64)
}

pub struct OpenAIDecisions;

impl DecisionProtocol for OpenAIDecisions {
    fn id(&self) -> &str {
        ID
    }
    fn name(&self) -> &str {
        NAME
    }

    fn body(&self, request: &DecisionRequest) -> Result<Object, AIError> {
        let options = request.provider_options.clone().unwrap_or_default();
        decode_options(&options)?;
        let mut known = Object::new();
        for (camel, snake) in [
            ("promptCacheKey", "prompt_cache_key"),
            ("promptCacheOptions", "prompt_cache_options"),
            ("safetyIdentifier", "safety_identifier"),
        ] {
            if let Some(v) = options.get(camel) {
                known.insert(snake.into(), v.clone());
            }
        }
        for (k, v) in &options {
            if !matches!(k.as_str(), "promptCacheKey" | "promptCacheOptions" | "safetyIdentifier") {
                known.insert(k.clone(), v.clone());
            }
        }
        let mut out = merge_records(&[Some(&known), request.http.as_ref().and_then(|h| h.body.as_ref())]).unwrap_or_default();
        let mut content = vec![];
        for part in &request.input {
            content.push(input_part(request, part)?);
        }
        out.insert("model".into(), json!(request.model.id));
        out.insert("input".into(), json!([{ "role": "user", "content": content }]));
        out.insert("questions".into(), Value::Array(request.questions.iter().cloned().map(Value::Object).collect()));
        Ok(out)
    }

    fn decode(&self, response: &MediaResponse, request: &DecisionRequest, metadata_key: &str) -> Result<DecisionResponse, AIError> {
        let ident = identity(ID, NAME, "openai");
        let invalid = || ident.invalid(response, format!("{NAME} returned an invalid response"));
        let value: Value = serde_json::from_slice(&response.body).map_err(|_| invalid())?;
        let model = value.get("model").and_then(Value::as_str).ok_or_else(invalid)?.to_string();
        let raw_answers = value.get("answers").and_then(Value::as_array).ok_or_else(invalid)?;
        let mut answers = vec![];
        for answer in raw_answers {
            let answer = answer.as_object().filter(|a| valid_answer(a)).ok_or_else(invalid)?;
            if answer.get("type").and_then(Value::as_str) == Some("refusal") {
                let mut out = Object::new();
                out.insert("name".into(), answer["name"].clone());
                out.insert("type".into(), json!("error"));
                out.insert("reason".into(), json!("refusal"));
                if let Some(m) = answer.get("message") {
                    out.insert("message".into(), m.clone());
                }
                answers.push(out);
            } else {
                answers.push(answer.clone());
            }
        }
        let usage_raw = match value.get("usage") {
            None => None,
            Some(Value::Object(o)) => Some(o.clone()),
            Some(_) => return Err(invalid()),
        };
        // Answers come back in question order when every one matches a question by name.
        let ordered: Vec<Object> = request
            .questions
            .iter()
            .filter_map(|q| answers.iter().rev().find(|a| a.get("name") == q.get("name")).cloned())
            .collect();
        let answers = if ordered.len() == answers.len() { ordered } else { answers };
        let usage = usage_raw.as_ref().map(|u| {
            let input = count(u, "input_tokens");
            let output = count(u, "output_tokens");
            let cost = u.get("cost").and_then(Value::as_f64).filter(|c| c.is_finite() && *c >= 0.0);
            DecisionUsage { input_tokens: input, output_tokens: output, total_tokens: total_tokens(input, output, count(u, "total_tokens")), cost }
        });
        let mut inner = Object::new();
        inner.insert("model".into(), json!(model));
        if let Some(u) = usage_raw {
            inner.insert("usage".into(), Value::Object(u));
        }
        let mut metadata = Object::new();
        metadata.insert(metadata_key.into(), Value::Object(inner));
        Ok(DecisionResponse { answers, usage, provider_metadata: Some(metadata) })
    }
}

fn probability(value: Option<&Value>) -> bool {
    value.and_then(Value::as_f64).is_some_and(|p| (0.0..=1.0).contains(&p))
}

fn choice_value(value: Option<&Value>) -> bool {
    value.is_some_and(|v| v.is_string() || v.is_boolean())
}

/// The response `Answer` schema.
fn valid_answer(answer: &Object) -> bool {
    if !answer.get("name").is_some_and(Value::is_string) {
        return false;
    }
    let list = |check: &dyn Fn(&Object) -> bool| {
        answer.get("probabilities").and_then(Value::as_array).is_some_and(|a| a.iter().all(|p| p.as_object().is_some_and(check)))
    };
    match answer.get("type").and_then(Value::as_str) {
        Some("predicate") => probability(answer.get("probability")),
        Some("choice") => {
            choice_value(answer.get("choice"))
                && probability(answer.get("confidence"))
                && list(&|p| choice_value(p.get("value")) && probability(p.get("probability")))
        }
        Some("score") => {
            answer.get("score").is_some_and(Value::is_number)
                && probability(answer.get("confidence"))
                && list(&|p| {
                    p.get("value").and_then(Value::as_f64).is_some_and(|v| v.fract() == 0.0)
                        && p.get("label").is_some_and(Value::is_string)
                        && probability(p.get("probability"))
                })
        }
        Some("refusal") => answer.get("message").is_none_or(Value::is_string),
        _ => false,
    }
}

/// `OpenAIDecisions.route`.
pub fn route() -> DecisionRoute {
    DecisionRoute::new(ID, "openai", Arc::new(OpenAIDecisions), MediaEndpoint::new(Some(DEFAULT_BASE_URL), PATH))
}
