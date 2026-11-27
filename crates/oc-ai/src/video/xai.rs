//! `protocols/xai-video.ts`: xAI's queued `/videos/generations` (Grok Imagine), with `/videos/edits` and
//! `/videos/extensions` behind `providerOptions.mode`.

use super::{decode_doc, encode_component, ended, null_or, opt_num, opt_str, pending, state, status_of, PollContext, Reason, State};
use super::{VideoModel, VideoOutput, VideoProtocol, VideoRequest, VideoResponse, VideoStatus};
use crate::error::AIError;
use crate::json::{merge_records, num, Object};
use crate::media::{MediaAsset, MediaInfo, MediaKind};
use crate::route::media::{media_reference, Deployment, Failure, Identity, MediaBody, MediaPath, MediaReference, MediaResponse};
use async_trait::async_trait;
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::sync::Arc;

pub const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";
pub const PATH: &str = "/videos/generations";
pub const EDIT_PATH: &str = "/videos/edits";
pub const EXTEND_PATH: &str = "/videos/extensions";
pub const STATUS_PATH: &str = "/videos";

static IDENTITY: Lazy<Identity> = Lazy::new(|| Identity::new("xai-video", "xAI Video", "xai"));

const STATUS: &[(&str, State)] = &[
    ("pending", State::Running),
    ("done", State::Completed),
    ("failed", State::Ended(Reason::Failed)),
    ("expired", State::Ended(Reason::Expired)),
];

// Documented video error codes; `service_unavailable`, `internal_error`, and unknown codes are provider-side.
const FAILURE: &[(&str, Failure)] = &[
    ("invalid_argument", Failure::InvalidRequest),
    ("failed_precondition", Failure::InvalidRequest),
    ("permission_denied", Failure::Authentication),
];

fn media_input(asset: &MediaAsset) -> Result<Value, AIError> {
    Ok(match media_reference(asset, Some(&IDENTITY.provider), &IDENTITY.name)? {
        MediaReference::Ref(id) => json!({ "file_id": id }),
        other => json!({ "url": other.value() }),
    })
}

fn native_options(options: Option<&Object>) -> Option<Object> {
    let options = options?;
    Some(options.iter().filter(|(k, _)| k.as_str() != "mode").map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// xAI's progress is undocumented but observed live as a 0..100 percentage.
fn fraction(progress: Option<f64>) -> Option<f64> {
    progress.filter(|p| (0.0..=100.0).contains(p)).map(|p| p / 100.0)
}

fn decode_video_status(response: &MediaResponse) -> Result<Value, AIError> {
    decode_doc(&IDENTITY, response, |v| {
        v.get("status").is_some_and(Value::is_string)
            && null_or(v, "progress", Value::is_number)
            && null_or(v, "model", Value::is_string)
            && null_or(v, "video", Value::is_object)
            && null_or(v, "error", Value::is_object)
            && v.get("video").and_then(Value::as_object).is_none_or(|video| {
                let video = Value::Object(video.clone());
                null_or(&video, "url", Value::is_string) && null_or(&video, "duration", Value::is_number) && null_or(&video, "respect_moderation", Value::is_boolean)
            })
    })
}

pub struct XAIVideo;

#[async_trait]
impl VideoProtocol for XAIVideo {
    fn identity(&self) -> &Identity {
        &IDENTITY
    }

    fn body(&self, request: &VideoRequest) -> Result<MediaBody, AIError> {
        if request.references.iter().any(|a| a.kind() == MediaKind::Video) {
            return Err(IDENTITY.unsupported("video.references", format!("{} has no reference video input", IDENTITY.name)));
        }
        let image = request.start.as_ref().map(media_input).transpose()?;
        let last_frame = request.end.as_ref().map(media_input).transpose()?;
        let images = request.references.iter().filter(|a| a.kind() != MediaKind::Audio).map(media_input).collect::<Result<Vec<_>, _>>()?;
        let audios = request
            .references
            .iter()
            .filter(|a| a.kind() == MediaKind::Audio)
            .map(|a| media_reference(a, Some(&IDENTITY.provider), &IDENTITY.name).map(|r| json!({ "url": r.value() })))
            .collect::<Result<Vec<_>, _>>()?;
        let mut base = Object::new();
        base.insert("model".into(), json!(request.model.id));
        base.insert("prompt".into(), json!(request.prompt));
        if let Some(v) = image {
            base.insert("image".into(), v);
        }
        if let Some(v) = last_frame {
            base.insert("last_frame".into(), v);
        }
        if !images.is_empty() {
            base.insert("reference_images".into(), Value::Array(images));
        }
        if !audios.is_empty() {
            base.insert("reference_audios".into(), Value::Array(audios));
        }
        if let Some(d) = request.duration {
            base.insert("duration".into(), num(d));
        }
        // xAI follows the input image's shape when a start frame is set, which is what `auto` asks for.
        if let Some(a) = &request.aspect
            && !(a == "auto" && request.start.is_some())
        {
            base.insert("aspect_ratio".into(), json!(a));
        }
        if let Some(r) = &request.resolution {
            base.insert("resolution".into(), json!(r));
        }
        if let Some(s) = request.sound {
            base.insert("generate_audio".into(), json!(s));
        }
        let native = native_options(request.provider_options.as_ref());
        Ok(MediaBody::json(
            merge_records(&[Some(&base), native.as_ref(), request.http.as_ref().and_then(|h| h.body.as_ref())]).unwrap_or_default(),
        ))
    }

    fn decode_start(&self, response: &MediaResponse) -> Result<String, AIError> {
        let value = decode_doc(&IDENTITY, response, |v| v.get("request_id").is_some_and(Value::is_string))?;
        Ok(value["request_id"].as_str().unwrap_or_default().to_string())
    }

    fn status_path(&self, token: &str) -> String {
        format!("{STATUS_PATH}/{}", encode_component(token))
    }

    fn decode_status(&self, response: &MediaResponse, _token: &str) -> Result<VideoStatus, AIError> {
        let value = decode_video_status(response)?;
        let mut status = status_of(STATUS, value["status"].as_str().unwrap_or_default());
        status.progress = fraction(opt_num(&value, "progress"));
        Ok(status)
    }

    async fn decode_result(&self, response: &MediaResponse, context: &PollContext<'_>) -> Result<VideoResponse, AIError> {
        let decoded = decode_video_status(response)?;
        let token = context.token;
        match state(STATUS, decoded["status"].as_str().unwrap_or_default()) {
            State::Running => return Err(pending(&IDENTITY, response, token)),
            State::Ended(Reason::Failed) => {
                let error = decoded.get("error").cloned().unwrap_or(Value::Null);
                let code = opt_str(&error, "code");
                let message = opt_str(&error, "message");
                let text = format!(
                    "{} generation failed{}{}",
                    IDENTITY.name,
                    code.as_ref().map(|c| format!(" ({c})")).unwrap_or_default(),
                    message.as_ref().map(|m| format!(": {m}")).unwrap_or_default()
                );
                return Err(ended(&IDENTITY, response, Reason::Failed, text, super::failure(FAILURE, code.as_deref())));
            }
            State::Ended(_) => {
                return Err(ended(&IDENTITY, response, Reason::Expired, format!("{} request {token} expired", IDENTITY.name), Failure::ProviderInternal));
            }
            State::Completed => {}
        }
        let video = decoded.get("video").cloned().unwrap_or(Value::Null);
        // `respect_moderation: false` marks a filtered result; a URL may still be present, so only a missing one fails.
        let respect = video.get("respect_moderation").and_then(Value::as_bool);
        let url = opt_str(&video, "url");
        let Some(url) = url else {
            if respect == Some(false) {
                return Err(IDENTITY.content_policy(response, format!("{} withheld the video for moderation", IDENTITY.name)));
            }
            return Err(IDENTITY.invalid(response, format!("{} completed without a video URL", IDENTITY.name)));
        };
        let duration = opt_num(&video, "duration");
        let info = duration.map(|d| MediaInfo { duration_seconds: Some(d), ..Default::default() });
        let mut metadata = Object::new();
        metadata.insert("requestId".into(), json!(token));
        if let Some(model) = opt_str(&decoded, "model") {
            metadata.insert("model".into(), json!(model));
        }
        if let Some(r) = respect {
            metadata.insert("respectModeration".into(), json!(r));
        }
        let mut provider_metadata = Object::new();
        provider_metadata.insert("xai".into(), Value::Object(metadata));
        Ok(VideoResponse { videos: vec![VideoOutput::url(&url, "video/mp4", info)], usage: None, provider_metadata: Some(provider_metadata) })
    }
}

/// `providerOptions.mode` sends the native `video` source to the edit or extension endpoint.
fn start_path(request: &VideoRequest) -> String {
    match request.option("mode").and_then(Value::as_str) {
        Some("edit") => EDIT_PATH.into(),
        Some("extend") => EXTEND_PATH.into(),
        _ => PATH.into(),
    }
}

/// `XAIVideo.model`.
pub fn model(id: &str, deployment: &Deployment, provider_options: Option<Object>) -> VideoModel {
    VideoModel::from_route(Arc::new(XAIVideo), DEFAULT_BASE_URL, MediaPath::Dynamic(Arc::new(start_path)), id, deployment, provider_options)
}
