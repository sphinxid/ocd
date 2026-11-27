//! `protocols/openrouter-video.ts`: OpenRouter's queued `/videos`.

use super::{decode_doc, encode_component, ended, null_or, opt_str, pending, state, status_of, PollContext, Reason, State};
use super::{VideoModel, VideoOutput, VideoProtocol, VideoRequest, VideoResponse, VideoStatus};
use crate::error::AIError;
use crate::json::{merge_records, num, Object};
use crate::media::{MediaAsset, MediaKind, MediaSource};
use crate::route::media::{media_reference, Deployment, Failure, Identity, MediaBody, MediaPath, MediaResponse};
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use std::sync::Arc;

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const PATH: &str = "/videos";

static IDENTITY: Lazy<Identity> = Lazy::new(|| Identity::new("openrouter-video", "OpenRouter Video", "openrouter"));

const STATUS: &[(&str, State)] = &[
    ("pending", State::Running),
    ("in_progress", State::Running),
    ("completed", State::Completed),
    ("failed", State::Ended(Reason::Failed)),
    ("cancelled", State::Ended(Reason::Cancelled)),
    ("expired", State::Ended(Reason::Expired)),
];

fn image_part(asset: &MediaAsset) -> Result<Object, AIError> {
    let reference = media_reference(asset, None, &IDENTITY.name)?;
    Ok(json!({ "type": "image_url", "image_url": { "url": reference.value() } }).as_object().cloned().unwrap_or_default())
}

fn reference_part(asset: &MediaAsset) -> Result<Value, AIError> {
    let kind = asset.kind();
    // OpenRouter rejects inline video and audio ("Only HTTPS URLs are allowed", verified live); inline images work.
    if matches!(kind, MediaKind::Video | MediaKind::Audio) && !matches!(asset.source, MediaSource::Url { .. }) {
        let name = if kind == MediaKind::Video { "video" } else { "audio" };
        return Err(IDENTITY.unsupported("video.references", format!("{} only takes reference {name} as http(s) URLs", IDENTITY.name)));
    }
    let reference = media_reference(asset, None, &IDENTITY.name)?;
    Ok(match kind {
        MediaKind::Video => json!({ "type": "video_url", "video_url": { "url": reference.value() } }),
        MediaKind::Audio => json!({ "type": "audio_url", "audio_url": { "url": reference.value() } }),
        _ => json!({ "type": "image_url", "image_url": { "url": reference.value() } }),
    })
}

// OpenRouter spells the K resolutions in upper case (`1K`, `2K`, `4K`).
fn resolution(value: &str) -> String {
    static K: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\dk$").unwrap());
    if K.is_match(value) { value.to_uppercase() } else { value.to_string() }
}

fn decode_job(response: &MediaResponse) -> Result<Value, AIError> {
    decode_doc(&IDENTITY, response, |v| {
        v.get("status").is_some_and(Value::is_string)
            && null_or(v, "id", Value::is_string)
            && null_or(v, "generation_id", Value::is_string)
            && null_or(v, "unsigned_urls", |u| u.as_array().is_some_and(|a| a.iter().all(Value::is_string)))
            && null_or(v, "expires_at", Value::is_number)
            && null_or(v, "error", Value::is_string)
            && null_or(v, "usage", Value::is_object)
    })
}

pub struct OpenRouterVideo;

#[async_trait]
impl VideoProtocol for OpenRouterVideo {
    fn identity(&self) -> &Identity {
        &IDENTITY
    }

    fn body(&self, request: &VideoRequest) -> Result<MediaBody, AIError> {
        let mut frames = vec![];
        if let Some(start) = &request.start {
            let mut part = image_part(start)?;
            part.insert("frame_type".into(), json!("first_frame"));
            frames.push(Value::Object(part));
        }
        if let Some(end) = &request.end {
            let mut part = image_part(end)?;
            part.insert("frame_type".into(), json!("last_frame"));
            frames.push(Value::Object(part));
        }
        let references = request.references.iter().map(reference_part).collect::<Result<Vec<_>, _>>()?;
        let mut base = Object::new();
        base.insert("model".into(), json!(request.model.id));
        base.insert("prompt".into(), json!(request.prompt));
        if !frames.is_empty() {
            base.insert("frame_images".into(), Value::Array(frames));
        }
        if !references.is_empty() {
            base.insert("input_references".into(), Value::Array(references));
        }
        if let Some(d) = request.duration {
            base.insert("duration".into(), num(d));
        }
        // Leaving the aspect ratio out is how OpenRouter follows the input image on models that can.
        if let Some(a) = request.aspect.as_deref().filter(|a| *a != "auto") {
            base.insert("aspect_ratio".into(), json!(a));
        }
        if let Some(r) = &request.resolution {
            base.insert("resolution".into(), json!(resolution(r)));
        }
        if let Some(s) = request.sound {
            base.insert("generate_audio".into(), json!(s));
        }
        Ok(MediaBody::json(
            merge_records(&[Some(&base), request.provider_options.as_ref(), request.http.as_ref().and_then(|h| h.body.as_ref())]).unwrap_or_default(),
        ))
    }

    fn decode_start(&self, response: &MediaResponse) -> Result<String, AIError> {
        let value = decode_doc(&IDENTITY, response, |v| v.get("id").is_some_and(Value::is_string) && null_or(v, "status", Value::is_string))?;
        Ok(value["id"].as_str().unwrap_or_default().to_string())
    }

    fn status_path(&self, token: &str) -> String {
        format!("{PATH}/{}", encode_component(token))
    }

    fn decode_status(&self, response: &MediaResponse, _token: &str) -> Result<VideoStatus, AIError> {
        let job = decode_job(response)?;
        Ok(status_of(STATUS, job["status"].as_str().unwrap_or_default()))
    }

    async fn decode_result(&self, response: &MediaResponse, context: &PollContext<'_>) -> Result<VideoResponse, AIError> {
        let job = decode_job(response)?;
        let token = context.token;
        let error = opt_str(&job, "error").filter(|e| !e.is_empty());
        let raw = job["status"].as_str().unwrap_or_default();
        match state(STATUS, raw) {
            State::Running => return Err(pending(&IDENTITY, response, token)),
            State::Ended(Reason::Failed) => {
                let message = format!("{} generation failed{}", IDENTITY.name, error.as_ref().map(|e| format!(": {e}")).unwrap_or_default());
                static MODERATED: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)content policy|moderat").unwrap());
                if error.as_ref().is_some_and(|e| MODERATED.is_match(e)) {
                    return Err(IDENTITY.content_policy(response, message));
                }
                return Err(ended(&IDENTITY, response, Reason::Failed, message, Failure::ProviderInternal));
            }
            State::Ended(reason) => {
                let message = format!("{} job {token} {}{}", IDENTITY.name, reason.as_str(), error.as_ref().map(|e| format!(": {e}")).unwrap_or_default());
                return Err(ended(&IDENTITY, response, reason, message, Failure::ProviderInternal));
            }
            State::Completed => {}
        }
        let urls: Vec<String> = job.get("unsigned_urls").and_then(Value::as_array).into_iter().flatten().filter_map(|u| u.as_str().map(String::from)).collect();
        if urls.is_empty() {
            return Err(IDENTITY.invalid(response, format!("{} completed without a video URL", IDENTITY.name)));
        }
        let expires_at = job.get("expires_at").and_then(Value::as_f64).map(|s| s * 1000.0);
        // The content URLs are OpenRouter's own and only download with the API key.
        let mut videos = vec![];
        for url in urls {
            let mut output = VideoOutput::url(&url, "video/mp4", None);
            output.expires_at = expires_at;
            output.headers = context.credentials(&url).await?;
            videos.push(output);
        }
        let usage = job.get("usage").cloned().unwrap_or(Value::Null);
        let mut metadata = Object::new();
        metadata.insert("id".into(), json!(token));
        if let Some(g) = opt_str(&job, "generation_id") {
            metadata.insert("generationId".into(), json!(g));
        }
        if let Some(c) = usage.get("cost").and_then(Value::as_f64) {
            metadata.insert("cost".into(), num(c));
        }
        if let Some(b) = usage.get("is_byok").and_then(Value::as_bool) {
            metadata.insert("isByok".into(), json!(b));
        }
        let mut provider_metadata = Object::new();
        provider_metadata.insert("openrouter".into(), Value::Object(metadata));
        Ok(VideoResponse { videos, usage: None, provider_metadata: Some(provider_metadata) })
    }
}

/// `OpenRouterVideo.model`. OpenRouter has no cancel endpoint; `cancelled` only appears when it stops a job itself.
pub fn model(id: &str, deployment: &Deployment, provider_options: Option<Object>) -> VideoModel {
    VideoModel::from_route(Arc::new(OpenRouterVideo), DEFAULT_BASE_URL, MediaPath::Static(PATH.into()), id, deployment, provider_options)
}
