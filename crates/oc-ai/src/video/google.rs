//! `protocols/google-interactions-video.ts`: Omni video as a background Google Interaction.

use super::{decode_doc, encode_component, ended, null_or, opt_num, pending, state, status_of, PollContext, Reason, State};
use super::{VideoModel, VideoOutput, VideoProtocol, VideoRequest, VideoResponse, VideoStatus, VideoUsage};
use crate::error::AIError;
use crate::image::input::decoded_asset;
use crate::json::{merge_records, Object};
use crate::media::MediaAsset;
use crate::protocols::gemini::content_filtered;
use crate::protocols::google_interactions::media_content;
use crate::route::media::{Deployment, Failure, Identity, MediaBody, MediaPath, MediaResponse};
use async_trait::async_trait;
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::sync::Arc;

pub const DEFAULT_BASE_URL: &str = crate::protocols::google_interactions::DEFAULT_BASE_URL;
pub const PATH: &str = "/interactions";

static IDENTITY: Lazy<Identity> = Lazy::new(|| Identity::new("google-interactions-video", "Google Interactions Video", "google"));

// `incomplete` is terminal ("completed, but contains incomplete results"); the result read decides whether a video
// came back. `requires_action` and `requires_continuation` are not documented for video and stay running.
const STATUS: &[(&str, State)] = &[
    ("in_progress", State::Running),
    ("completed", State::Completed),
    ("incomplete", State::Completed),
    ("failed", State::Ended(Reason::Failed)),
    ("cancelled", State::Ended(Reason::Cancelled)),
];

// Interaction error codes are documented only as "a URI that identifies the error type"; the last segment carries the
// canonical Google status name.
const FAILURE: &[(&str, Failure)] = &[
    ("INVALID_ARGUMENT", Failure::InvalidRequest),
    ("FAILED_PRECONDITION", Failure::InvalidRequest),
    ("OUT_OF_RANGE", Failure::InvalidRequest),
    ("PERMISSION_DENIED", Failure::Authentication),
    ("UNAUTHENTICATED", Failure::Authentication),
    ("RESOURCE_EXHAUSTED", Failure::RateLimit),
];

fn error_code(code: Option<&str>) -> Option<String> {
    let last = code?.split('/').next_back()?.trim().to_uppercase().replace('-', "_");
    if last.is_empty() { None } else { Some(last) }
}

/// Omni has no role field for media: it reads one image as the start frame and two as start and end. The task hint
/// pins that reading when only frames, or only references, are sent; with both, the prompt decides.
fn task(request: &VideoRequest) -> Option<&'static str> {
    let frames = request.start.is_some() || request.end.is_some();
    let references = !request.references.is_empty();
    match (frames, references) {
        (true, false) => Some("image_to_video"),
        (false, true) => Some("reference_to_video"),
        _ => None,
    }
}

fn decode_interaction(response: &MediaResponse) -> Result<Value, AIError> {
    let content_ok = |c: &Value| {
        c.get("type").is_some_and(Value::is_string)
            && null_or(c, "data", Value::is_string)
            && null_or(c, "uri", Value::is_string)
            && null_or(c, "mime_type", Value::is_string)
            && null_or(c, "text", Value::is_string)
    };
    decode_doc(&IDENTITY, response, |v| {
        v.get("id").is_some_and(Value::is_string)
            && v.get("status").is_some_and(Value::is_string)
            && null_or(v, "model", Value::is_string)
            && null_or(v, "usage", Value::is_object)
            && null_or(v, "steps", |steps| {
                steps.as_array().is_some_and(|a| {
                    a.iter().all(|s| {
                        s.get("type").is_some_and(Value::is_string) && null_or(s, "content", |c| c.as_array().is_some_and(|items| items.iter().all(content_ok)))
                    })
                })
            })
            && null_or(v, "errors", |e| {
                e.as_array().is_some_and(|a| a.iter().all(|x| null_or(x, "code", Value::is_string) && null_or(x, "message", Value::is_string)))
            })
    })
}

pub struct GoogleInteractionsVideo;

#[async_trait]
impl VideoProtocol for GoogleInteractionsVideo {
    fn identity(&self) -> &Identity {
        &IDENTITY
    }

    fn body(&self, request: &VideoRequest) -> Result<MediaBody, AIError> {
        if request.end.is_some() && request.start.is_none() {
            return Err(IDENTITY.unsupported("video.end", format!("{} reads a lone image as the start frame; send start too", IDENTITY.name)));
        }
        let assets: Vec<&MediaAsset> = request.start.iter().chain(request.end.iter()).chain(request.references.iter()).collect();
        let mut input = assets.into_iter().map(media_content).collect::<Result<Vec<_>, _>>()?;
        input.push(json!({ "type": "text", "text": request.prompt }));
        let mut format = Object::new();
        // `auto` follows the input image, which Omni expresses by leaving the ratio out.
        if let Some(a) = request.aspect.as_deref().filter(|a| *a != "auto") {
            format.insert("aspect_ratio".into(), json!(a));
        }
        if let Some(r) = &request.resolution {
            format.insert("resolution".into(), json!(r.to_lowercase()));
        }
        if let Some(d) = request.duration {
            format.insert("duration".into(), json!(format!("{}s", crate::json::js_string(&crate::json::num(d)))));
        }
        let mut base = Object::new();
        base.insert("model".into(), json!(request.model.id));
        base.insert("input".into(), Value::Array(input));
        base.insert("background".into(), json!(true));
        if !format.is_empty() {
            let mut response_format = Object::new();
            response_format.insert("type".into(), json!("video"));
            response_format.extend(format);
            base.insert("response_format".into(), Value::Object(response_format));
        }
        if let Some(t) = task(request) {
            base.insert("generation_config".into(), json!({ "video_config": { "task": t } }));
        }
        Ok(MediaBody::json(
            merge_records(&[Some(&base), request.provider_options.as_ref(), request.http.as_ref().and_then(|h| h.body.as_ref())]).unwrap_or_default(),
        ))
    }

    fn decode_start(&self, response: &MediaResponse) -> Result<String, AIError> {
        let value = decode_interaction(response)?;
        Ok(value["id"].as_str().unwrap_or_default().to_string())
    }

    fn status_path(&self, token: &str) -> String {
        format!("{PATH}/{}", encode_component(token))
    }

    fn decode_status(&self, response: &MediaResponse, _token: &str) -> Result<VideoStatus, AIError> {
        let value = decode_interaction(response)?;
        Ok(status_of(STATUS, value["status"].as_str().unwrap_or_default()))
    }

    async fn decode_result(&self, response: &MediaResponse, context: &PollContext<'_>) -> Result<VideoResponse, AIError> {
        let decoded = decode_interaction(response)?;
        let token = context.token;
        let raw = decoded["status"].as_str().unwrap_or_default();
        let status = state(STATUS, raw);
        if status == State::Running {
            return Err(pending(&IDENTITY, response, token));
        }
        let errors: Vec<Value> = decoded.get("errors").and_then(Value::as_array).cloned().unwrap_or_default();
        let codes: Vec<String> = errors.iter().filter_map(|e| error_code(e.get("code").and_then(Value::as_str))).collect();
        let messages: Vec<String> =
            errors.iter().filter_map(|e| e.get("message").and_then(Value::as_str).map(str::trim).filter(|m| !m.is_empty()).map(String::from)).collect();
        let blocked = codes.iter().any(|c| c == "CONTENT_BLOCKED" || content_filtered(c));
        if status == State::Ended(Reason::Cancelled) {
            return Err(ended(&IDENTITY, response, Reason::Cancelled, format!("{} interaction {token} was cancelled", IDENTITY.name), Failure::ProviderInternal));
        }
        if status == State::Ended(Reason::Failed) {
            let message = format!(
                "{} generation failed{}{}",
                IDENTITY.name,
                if codes.is_empty() { String::new() } else { format!(" ({})", codes.join(", ")) },
                if messages.is_empty() { String::new() } else { format!(": {}", messages.join("\n")) }
            );
            if blocked {
                return Err(IDENTITY.content_policy(response, message));
            }
            return Err(ended(&IDENTITY, response, Reason::Failed, message, super::failure(FAILURE, codes.first().map(String::as_str))));
        }
        let contents: Vec<Value> = decoded
            .get("steps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|s| s.get("type").and_then(Value::as_str) == Some("model_output"))
            .flat_map(|s| s.get("content").and_then(Value::as_array).cloned().unwrap_or_default())
            .collect();
        let mut videos = vec![];
        for (index, content) in contents.iter().filter(|c| c.get("type").and_then(Value::as_str) == Some("video")).enumerate() {
            let media_type = content.get("mime_type").and_then(Value::as_str);
            let label = format!("{} video {index}", IDENTITY.name);
            if let Some(data) = content.get("data").and_then(Value::as_str).filter(|d| !d.is_empty()) {
                let invalid = |message: String| IDENTITY.invalid(response, message);
                videos.push(VideoOutput::new(decoded_asset(&invalid, &label, data, media_type)?));
                continue;
            }
            // `GET /interactions/{id}` documents inline data even for `delivery: "uri"`; a Files API URI needs the API key
            // to download, so it stays a Google reference that can be passed back as input.
            if let Some(uri) = content.get("uri").and_then(Value::as_str).filter(|u| !u.is_empty()) {
                videos.push(VideoOutput::new(MediaAsset::reference(IDENTITY.provider.clone(), uri, media_type.map(String::from))));
                continue;
            }
            return Err(IDENTITY.invalid(response, format!("{} returned a video without data or a URI", IDENTITY.name)));
        }
        if videos.is_empty() {
            if blocked {
                return Err(IDENTITY.content_policy(
                    response,
                    format!("{} withheld the video{}", IDENTITY.name, if messages.is_empty() { String::new() } else { format!(": {}", messages.join("\n")) }),
                ));
            }
            return Err(IDENTITY.invalid(response, format!("{} {raw} without a video", IDENTITY.name)));
        }
        let usage = decoded.get("usage").filter(|u| u.is_object()).and_then(|u| {
            let mut details = Object::new();
            if let Some(r) = opt_num(u, "total_thought_tokens") {
                details.insert("reasoningTokens".into(), crate::json::num(r));
            }
            details.insert("google".into(), u.clone());
            crate::image::token_usage(opt_num(u, "total_input_tokens"), opt_num(u, "total_output_tokens"), opt_num(u, "total_tokens"), Some(details))
                .map(VideoUsage::Tokens)
        });
        let mut metadata = Object::new();
        metadata.insert("interactionId".into(), decoded["id"].clone());
        if let Some(m) = decoded.get("model").and_then(Value::as_str) {
            metadata.insert("model".into(), json!(m));
        }
        metadata.insert("status".into(), json!(raw));
        let mut provider_metadata = Object::new();
        provider_metadata.insert("google".into(), Value::Object(metadata));
        Ok(VideoResponse { videos, usage, provider_metadata: Some(provider_metadata) })
    }

    fn cancel(&self, token: &str) -> Option<(&'static str, String)> {
        Some(("POST", format!("{}/cancel", self.status_path(token))))
    }
}

/// `GoogleInteractionsVideo.model`.
pub fn model(id: &str, deployment: &Deployment, provider_options: Option<Object>) -> VideoModel {
    VideoModel::from_route(Arc::new(GoogleInteractionsVideo), DEFAULT_BASE_URL, MediaPath::Static(PATH.into()), id, deployment, provider_options)
}
