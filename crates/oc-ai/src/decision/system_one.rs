//! `protocols/system-one.ts`: the System One decision API (TypeSafe, OpenRouter, DigitalOcean, OpenCode Zen,
//! Baseten), plus `protocols/cloudflare-run-decisions.ts`, which wraps the same body for Cloudflare's `/ai/run`.

use super::{identity, DecisionPart, DecisionProtocol, DecisionRequest, DecisionResponse, DecisionRoute, DecisionUsage};
use crate::error::AIError;
use crate::json::{encode, merge_records, Object};
use crate::media::{MediaAsset, MediaKind};
use crate::route::media::{invalid_request, require_inline, total_tokens, MediaEndpoint, MediaResponse};
use serde_json::{json, Value};
use std::sync::Arc;

pub const DEFAULT_PATH: &str = "/systemone";
pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai/v1";

const ID: &str = "system-one";
const NAME: &str = "System One";

/// Where media goes in the body: `assets` (TypeSafe's assets map, referenced from the state), `state` (items inside
/// a state list), `root` (one top-level list per kind) or Baseten's `state.messages`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaInput {
    Assets,
    State,
    Root,
    StateMessages,
}

impl MediaInput {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "assets" => Some(Self::Assets),
            "state" => Some(Self::State),
            "root" => Some(Self::Root),
            "state.messages" => Some(Self::StateMessages),
            _ => None,
        }
    }
}

fn encode_questions(questions: &[Object]) -> Result<Object, AIError> {
    let mut out = Object::new();
    for question in questions {
        let name = question.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
        let instructions = question.get("instructions").cloned().unwrap_or(Value::Null);
        let value = match question.get("type").and_then(Value::as_str) {
            Some("predicate") => match question.get("criteria") {
                Some(criteria) => json!({ "type": "noul", "instructions": instructions, "criteria": criteria }),
                None => json!({ "type": "noul", "instructions": instructions }),
            },
            Some("score") => {
                let criteria: Vec<Value> = question
                    .get("levels")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|level| {
                        let label = level.get("label").and_then(Value::as_str).unwrap_or_default();
                        match level.get("description").and_then(Value::as_str) {
                            Some(d) => json!(format!("{label}: {d}")),
                            None => json!(label),
                        }
                    })
                    .collect();
                json!({ "type": "score", "instructions": instructions, "criteria": criteria })
            }
            _ => {
                // Options are map keys, so `true` is sent as "true" and mapped back by the choice value.
                let choices = question.get("choices").and_then(Value::as_array).cloned().unwrap_or_default();
                let keys: Vec<String> = choices.iter().map(|c| key_of(c.get("value"))).collect();
                let unique: std::collections::HashSet<&String> = keys.iter().collect();
                if unique.len() != keys.len() {
                    return Err(invalid_request(format!(
                        "System One choice options are strings; \"{name}\" has a boolean and a string with the same text"
                    )));
                }
                let mut criteria = Object::new();
                for (choice, key) in choices.iter().zip(keys) {
                    criteria.insert(key, choice.get("description").cloned().unwrap_or(Value::Null));
                }
                json!({ "type": "choice", "instructions": instructions, "criteria": criteria })
            }
        };
        out.insert(name, value);
    }
    Ok(out)
}

/// `String(choice.value)`.
fn key_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(other) => crate::json::js_string(other),
        None => "undefined".into(),
    }
}

/// A lone text or JSON part is the state itself; several are a list.
fn state(mut items: Vec<Value>) -> Value {
    if items.len() == 1 { items.remove(0) } else { Value::Array(items) }
}

/// `plain(part)`; media parts are `undefined`, which a JSON array encodes as `null`.
fn plain(part: &DecisionPart) -> Value {
    match part {
        DecisionPart::Text(t) => json!(t),
        DecisionPart::Json(v) => v.clone(),
        DecisionPart::Media { .. } => Value::Null,
    }
}

fn unsupported(request: &DecisionRequest, asset: &MediaAsset, accepted: &str) -> AIError {
    AIError::unsupported(
        "decision.media",
        format!("{NAME} takes {accepted} here; got {}", asset.media_type()),
        Some(&request.model.provider),
        Some(&request.model.route.id),
    )
}

fn state_item(request: &DecisionRequest, part: &DecisionPart) -> Result<Value, AIError> {
    let DecisionPart::Media { media: asset, .. } = part else { return Ok(plain(part)) };
    let kind = asset.kind();
    if !matches!(kind, MediaKind::Image | MediaKind::Audio | MediaKind::Video) {
        return Err(unsupported(request, asset, "images, audio and video"));
    }
    let inline = require_inline(NAME, asset)?;
    Ok(match kind {
        MediaKind::Image => json!({ "type": "image_url", "image_url": { "url": inline.data_url } }),
        MediaKind::Video => json!({ "type": "video_url", "video_url": { "url": inline.data_url } }),
        _ => json!({ "type": "input_audio", "input_audio": { "data": inline.base64, "format": &inline.mime["audio/".len().min(inline.mime.len())..] } }),
    })
}

fn place_media(request: &DecisionRequest, media_input: MediaInput) -> Result<Object, AIError> {
    let parts = &request.input;
    let media: Vec<&MediaAsset> = parts
        .iter()
        .filter_map(|p| match p {
            DecisionPart::Media { media, .. } => Some(media),
            _ => None,
        })
        .collect();
    let mut out = Object::new();
    if media.is_empty() {
        out.insert("state".into(), state(parts.iter().map(plain).collect()));
        return Ok(out);
    }
    match media_input {
        MediaInput::State => {
            let mut items = vec![];
            for part in parts {
                items.push(state_item(request, part)?);
            }
            out.insert("state".into(), Value::Array(items));
        }
        // Chat-style content in one user message: text and JSON as text parts, media as in `state`.
        MediaInput::StateMessages => {
            let mut content = vec![];
            for part in parts {
                content.push(match part {
                    DecisionPart::Media { .. } => state_item(request, part)?,
                    DecisionPart::Text(t) => json!({ "type": "text", "text": t }),
                    DecisionPart::Json(v) => json!({ "type": "text", "text": encode(v) }),
                });
            }
            out.insert("state".into(), json!({ "messages": [{ "role": "user", "content": content }] }));
        }
        MediaInput::Root => {
            let lists = [(MediaKind::Image, "images"), (MediaKind::Audio, "audio"), (MediaKind::Video, "videos")];
            if let Some(unplaced) = media.iter().find(|a| !lists.iter().any(|(k, _)| *k == a.kind())) {
                return Err(unsupported(request, unplaced, "images, audio and video"));
            }
            let mut inline = vec![];
            for asset in &media {
                inline.push(require_inline(NAME, asset)?);
            }
            out.insert(
                "state".into(),
                state(parts.iter().filter(|p| !matches!(p, DecisionPart::Media { .. })).map(plain).collect()),
            );
            for (kind, field) in lists {
                let items: Vec<Value> =
                    inline.iter().zip(&media).filter(|(_, a)| a.kind() == kind).map(|(i, _)| json!(i.data_url)).collect();
                if !items.is_empty() {
                    out.insert(field.into(), Value::Array(items));
                }
            }
        }
        MediaInput::Assets => {
            if let Some(unplaced) = media.iter().find(|a| a.kind() != MediaKind::Image) {
                return Err(unsupported(request, unplaced, "images"));
            }
            let mut inline = vec![];
            for asset in &media {
                inline.push(require_inline(NAME, asset)?);
            }
            // Every occurrence of a reference string in the body is replaced with its image, so it must not be
            // ordinary text.
            let mut next = 0;
            let mut items = vec![];
            for part in parts {
                if matches!(part, DecisionPart::Media { .. }) {
                    next += 1;
                    items.push(json!(format!("<image-{next}>")));
                } else {
                    items.push(plain(part));
                }
            }
            out.insert("state".into(), Value::Array(items));
            let mut assets = Object::new();
            for (index, item) in inline.iter().enumerate() {
                assets.insert(
                    format!("<image-{}>", index + 1),
                    json!({ "type": "image", "format": &item.mime["image/".len().min(item.mime.len())..], "data_base64": item.base64 }),
                );
            }
            out.insert("assets".into(), Value::Object(assets));
        }
    }
    Ok(out)
}

/// `SystemOne.encodeBody(request, fallback)`: the body without `model`; media goes where the model's
/// `media_input` says, else `fallback`.
pub fn encode_body(request: &DecisionRequest, fallback: MediaInput) -> Result<Object, AIError> {
    let questions = encode_questions(&request.questions)?;
    let media_input = request.model.media_input.as_deref().and_then(MediaInput::parse).unwrap_or(fallback);
    let placed = place_media(request, media_input)?;
    let mut out = merge_records(&[request.provider_options.as_ref(), request.http.as_ref().and_then(|h| h.body.as_ref())]).unwrap_or_default();
    for (k, v) in placed {
        out.insert(k, v);
    }
    out.insert("questions".into(), Value::Object(questions));
    Ok(out)
}

fn probability(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|p| (0.0..=1.0).contains(p))
}

/// Decode the System One `Response` schema; `None` when it does not match.
fn valid_response(value: &Value) -> bool {
    let Some(o) = value.as_object() else { return false };
    if !o.get("model").is_some_and(Value::is_string) {
        return false;
    }
    let Some(answers) = o.get("answers").and_then(Value::as_object) else { return false };
    let answers_ok = answers.values().all(|a| {
        let Some(a) = a.as_object() else { return false };
        let probs = || a.get("probabilities").and_then(Value::as_object).is_some_and(|p| p.values().all(|v| probability(Some(v)).is_some()));
        match a.get("type").and_then(Value::as_str) {
            Some("noul") => probability(a.get("noul")).is_some(),
            Some("choice") => a.get("choice").is_some_and(Value::is_string) && probs() && probability(a.get("confidence")).is_some(),
            Some("score") => a.get("score").is_some_and(Value::is_number) && probs() && probability(a.get("confidence")).is_some(),
            _ => false,
        }
    });
    let usage_ok = o.get("usage").is_none_or(|u| {
        u.as_object().is_some_and(|u| ["input_tokens", "output_tokens", "cost"].iter().all(|k| u.get(*k).is_none_or(Value::is_number)))
    });
    let meta_ok = o.get("provider_metadata").is_none_or(|m| m.as_object().is_some_and(|m| m.values().all(Value::is_object)));
    answers_ok
        && usage_ok
        && meta_ok
        && o.get("id").is_none_or(Value::is_string)
        && o.get("provider").is_none_or(Value::is_string)
}

/// `SystemOne.toResponse`: map the answers, keyed by question name, back to the request's question order.
pub fn to_response(
    data: &Object,
    request: &DecisionRequest,
    metadata_key: &str,
    invalid: &dyn Fn(String) -> AIError,
) -> Result<DecisionResponse, AIError> {
    let answers = data.get("answers").and_then(Value::as_object).cloned().unwrap_or_default();
    let names: Vec<&str> = request.questions.iter().filter_map(|q| q.get("name").and_then(Value::as_str)).collect();
    if answers.len() != names.len() || !names.iter().all(|n| answers.contains_key(*n)) {
        return Err(invalid("System One answers do not match the requested questions".into()));
    }
    let mapped: Vec<Object> = request
        .questions
        .iter()
        .map(|q| {
            let name = q.get("name").and_then(Value::as_str).unwrap_or_default();
            to_answer(q, answers[name].as_object().unwrap())
        })
        .collect();
    let usage_raw = data.get("usage").and_then(Value::as_object);
    let usage = usage_raw.map(|u| {
        let input = u.get("input_tokens").and_then(Value::as_f64);
        let output = u.get("output_tokens").and_then(Value::as_f64);
        DecisionUsage { input_tokens: input, output_tokens: output, total_tokens: total_tokens(input, output, None), cost: u.get("cost").and_then(Value::as_f64) }
    });
    let mut inner = Object::new();
    inner.insert("model".into(), data["model"].clone());
    if let Some(u) = usage_raw {
        inner.insert("usage".into(), Value::Object(u.clone()));
    }
    if let Some(id) = data.get("id") {
        inner.insert("responseId".into(), id.clone());
    }
    if let Some(p) = data.get("provider") {
        inner.insert("provider".into(), p.clone());
    }
    if let Some(extra) = data.get("provider_metadata").and_then(|m| m.get(metadata_key)).and_then(Value::as_object) {
        for (k, v) in extra {
            inner.insert(k.clone(), v.clone());
        }
    }
    let mut metadata = Object::new();
    metadata.insert(metadata_key.into(), Value::Object(inner));
    Ok(DecisionResponse { answers: mapped, usage, provider_metadata: Some(metadata) })
}

/// JS object key order: integer-like keys ascending first, then the rest in insertion order.
fn js_entries(map: &Object) -> Vec<(&String, &Value)> {
    let is_index = |k: &str| k == "0" || (!k.starts_with('0') && !k.is_empty() && k.len() < 10 && k.bytes().all(|b| b.is_ascii_digit()));
    let mut indexed: Vec<(&String, &Value)> = map.iter().filter(|(k, _)| is_index(k)).collect();
    indexed.sort_by_key(|(k, _)| k.parse::<u64>().unwrap_or(0));
    indexed.extend(map.iter().filter(|(k, _)| !is_index(k)));
    indexed
}

fn to_answer(question: &Object, answer: &Object) -> Object {
    let name = question.get("name").cloned().unwrap_or(Value::Null);
    let mut out = Object::new();
    out.insert("name".into(), name);
    let empty = Object::new();
    let probabilities = answer.get("probabilities").and_then(Value::as_object).unwrap_or(&empty);
    match answer.get("type").and_then(Value::as_str) {
        Some("noul") => {
            out.insert("type".into(), json!("predicate"));
            out.insert("probability".into(), answer["noul"].clone());
        }
        Some("choice") => {
            let choices: Vec<Value> = if question.get("type").and_then(Value::as_str) == Some("choice") {
                question.get("choices").and_then(Value::as_array).cloned().unwrap_or_default()
            } else {
                vec![]
            };
            let value = |key: &str| -> Value {
                choices.iter().find(|c| key_of(c.get("value")) == key).and_then(|c| c.get("value").cloned()).unwrap_or(json!(key))
            };
            let rank = |key: &str| choices.iter().position(|c| key_of(c.get("value")) == key).unwrap_or(choices.len());
            let mut entries = js_entries(probabilities);
            entries.sort_by_key(|(k, _)| rank(k));
            out.insert("type".into(), json!("choice"));
            out.insert("choice".into(), value(answer["choice"].as_str().unwrap_or_default()));
            out.insert(
                "probabilities".into(),
                Value::Array(entries.into_iter().map(|(k, p)| json!({ "value": value(k), "probability": p })).collect()),
            );
            out.insert("confidence".into(), answer["confidence"].clone());
        }
        _ => {
            let levels: Vec<Value> = if question.get("type").and_then(Value::as_str) == Some("score") {
                question.get("levels").and_then(Value::as_array).cloned().unwrap_or_default()
            } else {
                vec![]
            };
            let mut items: Vec<(f64, Value)> = js_entries(probabilities)
                .into_iter()
                .map(|(k, p)| {
                    let n: f64 = k.parse().unwrap_or(f64::NAN);
                    let label = (n.fract() == 0.0 && n >= 0.0)
                        .then(|| levels.get(n as usize).and_then(|l| l.get("label")).cloned())
                        .flatten()
                        .unwrap_or(json!(k));
                    (n, json!({ "value": crate::json::num(n), "label": label, "probability": p }))
                })
                .collect();
            items.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            out.insert("type".into(), json!("score"));
            out.insert("score".into(), answer["score"].clone());
            out.insert("probabilities".into(), Value::Array(items.into_iter().map(|(_, v)| v).collect()));
            out.insert("confidence".into(), answer["confidence"].clone());
        }
    }
    out
}

/// Decode a System One document (or one wrapped by `unwrap`) and map it to a [`DecisionResponse`].
fn decode_with(
    response: &MediaResponse,
    request: &DecisionRequest,
    metadata_key: &str,
    id: &str,
    name: &str,
    provider: &str,
    unwrap: fn(&Value) -> Option<&Value>,
) -> Result<DecisionResponse, AIError> {
    let ident = identity(id, name, provider);
    let invalid = |message: String| ident.invalid(response, message);
    let value: Value = serde_json::from_slice(&response.body).map_err(|_| invalid(format!("{name} returned an invalid response")))?;
    let data = unwrap(&value).filter(|d| valid_response(d)).ok_or_else(|| invalid(format!("{name} returned an invalid response")))?;
    to_response(data.as_object().unwrap(), request, metadata_key, &invalid)
}

/// The System One protocol with a body shape: `system-one` (media in assets), Baseten's `baseten-system-one`
/// (`state.messages`) and Cloudflare's `cloudflare-run-decisions` (`{model, input}`, media in root lists).
pub struct SystemOne {
    pub id: &'static str,
    pub fallback: MediaInput,
    pub cloudflare: bool,
}

impl DecisionProtocol for SystemOne {
    fn id(&self) -> &str {
        self.id
    }
    fn name(&self) -> &str {
        if self.cloudflare { "Cloudflare Run" } else { NAME }
    }
    fn inline_media(&self) -> bool {
        true
    }

    fn body(&self, request: &DecisionRequest) -> Result<Object, AIError> {
        let body = encode_body(request, self.fallback)?;
        if self.cloudflare {
            let mut out = Object::new();
            out.insert("model".into(), json!(request.model.id));
            out.insert("input".into(), Value::Object(body));
            return Ok(out);
        }
        let mut out = body;
        out.insert("model".into(), json!(request.model.id));
        Ok(out)
    }

    fn decode(&self, response: &MediaResponse, request: &DecisionRequest, metadata_key: &str) -> Result<DecisionResponse, AIError> {
        if self.cloudflare {
            // Workers AI models answer with the System One response; third-party models such as TypeSafe Jev wrap
            // it in a completed run record.
            return decode_with(response, request, metadata_key, "cloudflare-run-decisions", "Cloudflare Run", "cloudflare-workers-ai", |v| {
                let result = v.get("result")?;
                if result.get("answers").is_some() {
                    return Some(result);
                }
                (result.get("state").and_then(Value::as_str) == Some("Completed")).then(|| result.get("result")).flatten()
            });
        }
        decode_with(response, request, metadata_key, ID, NAME, "typesafe-ai", |v| Some(v))
    }
}

/// `SystemOne.route`: TypeSafe's API, the reference System One host.
pub fn route() -> DecisionRoute {
    DecisionRoute::new(
        ID,
        "typesafe-ai",
        Arc::new(SystemOne { id: ID, fallback: MediaInput::Assets, cloudflare: false }),
        MediaEndpoint::new(Some(DEFAULT_BASE_URL), DEFAULT_PATH),
    )
}

/// Baseten's decision route: media only as parts in a user message inside `state.messages`.
pub fn baseten_route(base_url: &str) -> DecisionRoute {
    DecisionRoute::new(
        "baseten-system-one",
        "baseten",
        Arc::new(SystemOne { id: "baseten-system-one", fallback: MediaInput::StateMessages, cloudflare: false }),
        MediaEndpoint::new(Some(base_url), DEFAULT_PATH),
    )
}

/// `CloudflareRunDecisions.route`: providers set the Run API base with `with_base_url`.
pub fn cloudflare_route() -> DecisionRoute {
    DecisionRoute::new(
        "cloudflare-run-decisions",
        "cloudflare-workers-ai",
        Arc::new(SystemOne { id: "cloudflare-run-decisions", fallback: MediaInput::Root, cloudflare: true }),
        MediaEndpoint::new(None, ""),
    )
}
