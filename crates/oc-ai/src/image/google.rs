//! `protocols/google-images.ts`: Gemini image models through `generateContent` (Gemini API and Vertex).

use super::input;
use super::{token_usage, ImageModel, ImageProtocol, ImageRequest, ImageResponse, RouteInput};
use crate::error::AIError;
use crate::json::{merge_records, Object};
use crate::media::{MediaAsset, MediaKind, MediaSource};
use crate::route::media::{require_inline, total_tokens, Deployment, Identity, MediaBody, MediaPath, MediaResponse};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

static IDENTITY: Lazy<Identity> = Lazy::new(|| Identity::new("google-images", "Google Images", "google"));

/// `GeminiGenerateContent.contentFiltered`: finish reasons for which Gemini stops output on safety grounds.
pub fn content_filtered(reason: &str) -> bool {
    matches!(
        reason,
        "IMAGE_SAFETY"
            | "RECITATION"
            | "SAFETY"
            | "BLOCKLIST"
            | "PROHIBITED_CONTENT"
            | "SPII"
            | "MODEL_ARMOR"
            | "IMAGE_PROHIBITED_CONTENT"
            | "IMAGE_RECITATION"
            | "LANGUAGE"
    )
}

/// `GeminiGenerateContent.mediaPart`.
pub fn media_part(route: &str, asset: &MediaAsset) -> Result<Value, AIError> {
    let file_uri = match &asset.source {
        MediaSource::Url { url, .. } if asset.kind() == MediaKind::Video && is_http(url) => Some(url.clone()),
        _ => input::ref_id(asset, "google").map(String::from),
    };
    if let Some(uri) = file_uri {
        return Ok(json!({ "fileData": { "mimeType": asset.media_type(), "fileUri": uri } }));
    }
    let inline = require_inline(route, asset)?;
    Ok(json!({ "inlineData": { "mimeType": inline.mime, "data": inline.base64 } }))
}

fn is_http(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn generation_config(request: &ImageRequest) -> Object {
    let mut native = request.provider_options.clone().unwrap_or_default();
    let thinking_level = native.shift_remove("thinkingLevel");
    let include_thoughts = native.shift_remove("includeThoughts");
    // Google names the half tier `512` and rejects `0.5K`.
    let image_size = request.resolution.as_ref().map(|r| {
        let upper = r.to_uppercase();
        if upper == "0.5K" { "512".to_string() } else { upper }
    });
    let mut base = Object::new();
    base.insert("responseModalities".into(), json!(["IMAGE"]));
    if request.aspect.is_some() || image_size.is_some() {
        let mut image = Object::new();
        if let Some(a) = &request.aspect {
            image.insert("aspectRatio".into(), json!(a));
        }
        if let Some(s) = image_size {
            image.insert("imageSize".into(), json!(s));
        }
        base.insert("imageConfig".into(), Value::Object(image));
    }
    if thinking_level.is_some() || include_thoughts.is_some() {
        let mut thinking = Object::new();
        if let Some(v) = thinking_level {
            thinking.insert("thinkingLevel".into(), v);
        }
        if let Some(v) = include_thoughts {
            thinking.insert("includeThoughts".into(), v);
        }
        base.insert("thinkingConfig".into(), Value::Object(thinking));
    }
    merge_records(&[Some(&base), Some(&native)]).unwrap_or(base)
}

pub struct GoogleImages;

impl ImageProtocol for GoogleImages {
    fn identity(&self) -> &Identity {
        &IDENTITY
    }

    fn body(&self, request: &ImageRequest) -> Result<MediaBody, AIError> {
        if let Some(n) = request.n.filter(|n| *n > 1) {
            return Err(IDENTITY.unsupported(
                "media.n",
                format!("{} generates one image per request; call it once per image instead of n={n}", IDENTITY.name),
            ));
        }
        let mut parts = vec![json!({ "text": request.prompt })];
        for image in &request.images {
            parts.push(media_part(&IDENTITY.name, image)?);
        }
        let mut base = Object::new();
        base.insert("contents".into(), json!([{ "role": "user", "parts": parts }]));
        base.insert("generationConfig".into(), Value::Object(generation_config(request)));
        Ok(MediaBody::json(merge_records(&[Some(&base), request.http.as_ref().and_then(|h| h.body.as_ref())]).unwrap_or_default()))
    }

    fn decode(&self, response: &MediaResponse, _request: &ImageRequest, _body: &MediaBody) -> Result<ImageResponse, AIError> {
        let decoded: Document = IDENTITY.decode_json(response)?;
        let invalid = |message: String| IDENTITY.invalid(response, message);
        let candidates = decoded.candidates.clone().unwrap_or_default();
        let candidate_metadata: Vec<Value> = candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let parts: Vec<Value> = candidate
                    .content
                    .as_ref()
                    .map(|c| c.parts.as_slice())
                    .unwrap_or_default()
                    .iter()
                    .map(|part| match &part.inline_data {
                        None => object(&[
                            ("type", Some(json!("text"))),
                            ("text", part.text.clone().map(Value::String)),
                            ("thought", part.thought.map(Value::Bool)),
                            ("thoughtSignature", part.thought_signature.clone().map(Value::String)),
                        ]),
                        Some(data) => object(&[
                            ("type", Some(json!("inlineData"))),
                            ("mediaType", Some(json!(data.mime_type))),
                            ("thought", part.thought.map(Value::Bool)),
                            ("thoughtSignature", part.thought_signature.clone().map(Value::String)),
                        ]),
                    })
                    .collect();
                object(&[
                    ("index", Some(crate::json::num(candidate.index.unwrap_or(index as f64)))),
                    ("finishReason", candidate.finish_reason.clone().map(Value::String)),
                    ("finishMessage", candidate.finish_message.clone().map(Value::String)),
                    ("safetyRatings", candidate.safety_ratings.clone()),
                    ("citationMetadata", candidate.citation_metadata.clone()),
                    ("groundingMetadata", candidate.grounding_metadata.clone()),
                    ("parts", Some(Value::Array(parts))),
                ])
            })
            .collect();
        let mut images = vec![];
        for (candidate_index, candidate) in candidates.iter().enumerate() {
            let parts = candidate.content.as_ref().map(|c| c.parts.as_slice()).unwrap_or_default();
            for (part_index, part) in parts.iter().enumerate() {
                // Thought parts are drafts; only non-thought inline data is a final image.
                let Some(data) = &part.inline_data else { continue };
                if part.thought == Some(true) {
                    continue;
                }
                let mut asset = input::decoded_asset(
                    &invalid,
                    &format!("{} candidate {candidate_index} part {part_index}", IDENTITY.name),
                    &data.data,
                    Some(&data.mime_type),
                )?;
                let mut google = Object::new();
                google.insert("candidateIndex".into(), crate::json::num(candidate.index.unwrap_or(candidate_index as f64)));
                google.insert("partIndex".into(), json!(part_index));
                for (key, value) in [
                    ("finishReason", candidate.finish_reason.clone().map(Value::String)),
                    ("safetyRatings", candidate.safety_ratings.clone()),
                    ("citationMetadata", candidate.citation_metadata.clone()),
                    ("groundingMetadata", candidate.grounding_metadata.clone()),
                    ("thoughtSignature", part.thought_signature.clone().map(Value::String)),
                ] {
                    if let Some(v) = value {
                        google.insert(key.into(), v);
                    }
                }
                let mut metadata = Object::new();
                metadata.insert("google".into(), Value::Object(google));
                asset.provider_metadata = Some(metadata);
                images.push(asset);
            }
        }
        if images.is_empty() {
            let reasons: Vec<&str> = candidates.iter().filter_map(|c| c.finish_reason.as_deref()).collect();
            // Safety and recitation stops are Google refusing the prompt, not a malformed response.
            if reasons.iter().any(|r| content_filtered(r)) {
                return Err(IDENTITY.content_policy(
                    response,
                    format!("{} withheld its images (finish reasons: {})", IDENTITY.name, reasons.join(", ")),
                ));
            }
            return Err(invalid(format!(
                "{} returned no final images{}; inspect body for prompt feedback and candidate details",
                IDENTITY.name,
                if reasons.is_empty() { String::new() } else { format!(" (finish reasons: {})", reasons.join(", ")) }
            )));
        }
        let usage = decoded.usage_metadata.as_ref().and_then(|u| {
            let num = |key: &str| u.get(key).and_then(Value::as_f64);
            let input = num("promptTokenCount");
            let thoughts = num("thoughtsTokenCount");
            let output = num("candidatesTokenCount").map(|c| c + thoughts.unwrap_or(0.0));
            let mut details = Object::new();
            if let Some(t) = thoughts {
                details.insert("reasoningTokens".into(), crate::json::num(t));
            }
            if let Some(c) = num("cachedContentTokenCount") {
                details.insert("cacheReadInputTokens".into(), crate::json::num(c));
            }
            details.insert("google".into(), Value::Object(u.clone()));
            token_usage(input, output, total_tokens(input, output, num("totalTokenCount")), Some(details))
        });
        let google = object(&[
            ("modelVersion", decoded.model_version.clone().map(Value::String)),
            ("responseId", decoded.response_id.clone().map(Value::String)),
            ("promptFeedback", decoded.prompt_feedback.clone()),
            ("candidates", Some(Value::Array(candidate_metadata))),
        ]);
        let mut metadata = Object::new();
        metadata.insert("google".into(), google);
        Ok(ImageResponse { images, usage, provider_metadata: Some(metadata) })
    }
}

fn object(entries: &[(&str, Option<Value>)]) -> Value {
    let mut out = Object::new();
    for (key, value) in entries {
        if let Some(v) = value {
            out.insert((*key).into(), v.clone());
        }
    }
    Value::Object(out)
}

#[derive(Deserialize, Clone)]
struct InlineData {
    #[serde(rename = "mimeType")]
    mime_type: String,
    data: String,
}

#[derive(Deserialize, Clone)]
struct Part {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thought: Option<bool>,
    #[serde(default, rename = "thoughtSignature")]
    thought_signature: Option<String>,
    #[serde(default, rename = "inlineData")]
    inline_data: Option<InlineData>,
}

#[derive(Deserialize, Clone)]
struct Content {
    parts: Vec<Part>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Candidate {
    #[serde(default)]
    index: Option<f64>,
    #[serde(default)]
    content: Option<Content>,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    finish_message: Option<String>,
    #[serde(default)]
    safety_ratings: Option<Value>,
    #[serde(default)]
    citation_metadata: Option<Value>,
    #[serde(default)]
    grounding_metadata: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Document {
    #[serde(default)]
    candidates: Option<Vec<Candidate>>,
    #[serde(default)]
    usage_metadata: Option<Object>,
    #[serde(default)]
    model_version: Option<String>,
    #[serde(default)]
    response_id: Option<String>,
    #[serde(default)]
    prompt_feedback: Option<Value>,
}

/// `GoogleImages.model(input)`.
pub fn model(id: &str, deployment: &Deployment, provider_options: Option<Object>) -> ImageModel {
    ImageModel::from_route(
        RouteInput {
            protocol: Arc::new(GoogleImages),
            provider: None,
            path: MediaPath::Dynamic(Arc::new(|r: &ImageRequest| format!("/models/{}:generateContent", r.model.id))),
            base_url: Some(DEFAULT_BASE_URL.into()),
            headers: None,
        },
        id,
        deployment,
        provider_options,
    )
}
