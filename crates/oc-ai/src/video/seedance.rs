//! `protocols/seedance-video.ts`: one Ark API (`/contents/generations/tasks`) served by Volcengine Ark in China and
//! BytePlus ModelArk elsewhere, one protocol per provider so assets and metadata carry the right provider ID.

use super::{decode_doc, encode_component, ended, null_or, opt_num, opt_str, pending, state, status_of, PollContext, Reason, State};
use super::{TokenUsage, VideoModel, VideoOutput, VideoProtocol, VideoRequest, VideoResponse, VideoStatus, VideoUsage};
use crate::error::AIError;
use crate::json::{merge_records, num, Object};
use crate::media::{MediaAsset, MediaInfo, MediaKind};
use crate::route::media::{media_reference, Deployment, Failure, Identity, MediaBody, MediaPath, MediaReference, MediaResponse};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub const VOLCENGINE_BASE_URL: &str = "https://ark.cn-beijing.volces.com/api/v3";
pub const BYTEPLUS_BASE_URL: &str = "https://ark.ap-southeast.bytepluses.com/api/v3";
pub const PATH: &str = "/contents/generations/tasks";
/// Video and last-frame URLs are documented as valid for 24 hours.
const OUTPUT_RETENTION: Duration = Duration::from_secs(24 * 3600);

const STATUS: &[(&str, State)] = &[
    ("queued", State::Running),
    ("running", State::Running),
    ("succeeded", State::Completed),
    ("failed", State::Ended(Reason::Failed)),
    ("cancelled", State::Ended(Reason::Cancelled)),
    ("expired", State::Ended(Reason::Expired)),
];

// Ark error codes are dotted (`InvalidParameter.TaskTypeConstraint`); the leading segment is the category.
// `InternalServiceError`, `ServerOverloaded` and unknown codes are provider-side.
const FAILURE: &[(&str, Failure)] = &[
    ("InvalidParameter", Failure::InvalidRequest),
    ("MissingParameter", Failure::InvalidRequest),
    ("InvalidArgumentError", Failure::InvalidRequest),
    ("InvalidImageURL", Failure::InvalidRequest),
    ("AuthenticationError", Failure::Authentication),
    ("AccessDenied", Failure::Authentication),
    ("QuotaExceeded", Failure::RateLimit),
    ("RateLimitExceeded", Failure::RateLimit),
];

/// Input and output moderation codes: `InputImageSensitiveContentDetected.PrivacyInformation`,
/// `OutputVideoSensitiveContentDetected`, `SensitiveContentDetected`, ...
fn moderated(code: Option<&str>) -> bool {
    code.is_some_and(|c| c.contains("SensitiveContentDetected"))
}

/// The Ark provider a protocol serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Volcengine,
    Byteplus,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Volcengine => "volcengine",
            Self::Byteplus => "byteplus",
        }
    }
}

pub struct SeedanceVideo {
    provider: Provider,
    identity: Identity,
}

impl SeedanceVideo {
    pub fn new(provider: Provider) -> Self {
        Self { provider, identity: Identity::new("seedance-video", "Seedance Video", provider.as_str()) }
    }

    /// `asset://<ID>` addresses a ModelArk digital character or authorized asset.
    fn item(&self, asset: &MediaAsset, kind: &str, role: &str) -> Result<Value, AIError> {
        let url = match media_reference(asset, Some(&self.identity.provider), &self.identity.name)? {
            MediaReference::Ref(id) => format!("asset://{id}"),
            other => other.value().to_string(),
        };
        let mut out = Object::new();
        out.insert("type".into(), json!(kind));
        out.insert(kind.into(), json!({ "url": url }));
        out.insert("role".into(), json!(role));
        Ok(Value::Object(out))
    }

    fn reference(&self, asset: &MediaAsset) -> Result<Value, AIError> {
        match asset.kind() {
            MediaKind::Video => self.item(asset, "video_url", "reference_video"),
            MediaKind::Audio => self.item(asset, "audio_url", "reference_audio"),
            _ => self.item(asset, "image_url", "reference_image"),
        }
    }

    fn decode_task(&self, response: &MediaResponse) -> Result<Value, AIError> {
        decode_doc(&self.identity, response, |v| {
            v.get("status").is_some_and(Value::is_string)
                && null_or(v, "id", Value::is_string)
                && null_or(v, "model", Value::is_string)
                && null_or(v, "content", Value::is_object)
                && null_or(v, "error", Value::is_object)
                && null_or(v, "usage", Value::is_object)
                && null_or(v, "updated_at", Value::is_number)
                && null_or(v, "seed", Value::is_number)
                && null_or(v, "resolution", Value::is_string)
                && null_or(v, "ratio", Value::is_string)
                && null_or(v, "duration", Value::is_number)
                && null_or(v, "frames", Value::is_number)
                && null_or(v, "framespersecond", Value::is_number)
                && null_or(v, "generate_audio", Value::is_boolean)
                && null_or(v, "output_format", Value::is_string)
                && null_or(v, "draft", Value::is_boolean)
                && null_or(v, "service_tier", Value::is_string)
        })
    }
}

/// Ark returns `frames` when the request set it, otherwise `duration` rounded down to whole seconds.
fn duration_seconds(task: &Value) -> Option<f64> {
    if let (Some(frames), Some(fps)) = (opt_num(task, "frames"), opt_num(task, "framespersecond"))
        && fps != 0.0
    {
        return Some(frames / fps);
    }
    opt_num(task, "duration")
}

#[async_trait]
impl VideoProtocol for SeedanceVideo {
    fn identity(&self) -> &Identity {
        &self.identity
    }

    // Frames and references are separate Seedance modes; mixing them is sent as-is and rejected by the provider.
    fn body(&self, request: &VideoRequest) -> Result<MediaBody, AIError> {
        let mut content = vec![];
        // Every media mode accepts an optional prompt, so an empty one adds no text item.
        if !request.prompt.is_empty() {
            content.push(json!({ "type": "text", "text": request.prompt }));
        }
        if let Some(start) = &request.start {
            content.push(self.item(start, "image_url", "first_frame")?);
        }
        if let Some(end) = &request.end {
            content.push(self.item(end, "image_url", "last_frame")?);
        }
        for r in &request.references {
            content.push(self.reference(r)?);
        }
        if let Some(draft) = request.option("draft_task") {
            content.push(json!({ "type": "draft_task", "draft_task": draft }));
        }
        let mut base = Object::new();
        base.insert("model".into(), json!(request.model.id));
        base.insert("content".into(), Value::Array(content));
        if let Some(d) = request.duration {
            base.insert("duration".into(), num(d));
        }
        if let Some(a) = &request.aspect {
            base.insert("ratio".into(), json!(if a == "auto" { "adaptive" } else { a.as_str() }));
        }
        if let Some(r) = &request.resolution {
            base.insert("resolution".into(), json!(r));
        }
        if let Some(s) = request.sound {
            base.insert("generate_audio".into(), json!(s));
        }
        let native: Option<Object> =
            request.provider_options.as_ref().map(|o| o.iter().filter(|(k, _)| k.as_str() != "draft_task").map(|(k, v)| (k.clone(), v.clone())).collect());
        Ok(MediaBody::json(
            merge_records(&[Some(&base), native.as_ref(), request.http.as_ref().and_then(|h| h.body.as_ref())]).unwrap_or_default(),
        ))
    }

    fn decode_start(&self, response: &MediaResponse) -> Result<String, AIError> {
        let value = decode_doc(&self.identity, response, |v| v.get("id").is_some_and(Value::is_string))?;
        Ok(value["id"].as_str().unwrap_or_default().to_string())
    }

    fn status_path(&self, token: &str) -> String {
        format!("{PATH}/{}", encode_component(token))
    }

    fn decode_status(&self, response: &MediaResponse, _token: &str) -> Result<VideoStatus, AIError> {
        let task = self.decode_task(response)?;
        Ok(status_of(STATUS, task["status"].as_str().unwrap_or_default()))
    }

    async fn decode_result(&self, response: &MediaResponse, context: &PollContext<'_>) -> Result<VideoResponse, AIError> {
        let task = self.decode_task(response)?;
        let token = context.token;
        let name = &self.identity.name;
        match state(STATUS, task["status"].as_str().unwrap_or_default()) {
            State::Running => return Err(pending(&self.identity, response, token)),
            State::Ended(Reason::Failed) => {
                let error = task.get("error").cloned().unwrap_or(Value::Null);
                let code = opt_str(&error, "code");
                let detail = opt_str(&error, "message");
                let message = format!(
                    "{name} generation failed{}{}",
                    code.as_ref().map(|c| format!(" ({c})")).unwrap_or_default(),
                    detail.as_ref().map(|d| format!(": {d}")).unwrap_or_default()
                );
                if moderated(code.as_deref()) {
                    return Err(self.identity.content_policy(response, message));
                }
                let category = code.as_deref().map(|c| c.split('.').next().unwrap_or(c));
                return Err(ended(&self.identity, response, Reason::Failed, message, super::failure(FAILURE, category)));
            }
            State::Ended(Reason::Cancelled) => {
                return Err(ended(&self.identity, response, Reason::Cancelled, format!("{name} task {token} was cancelled"), Failure::ProviderInternal));
            }
            State::Ended(_) => {
                return Err(ended(&self.identity, response, Reason::Expired, format!("{name} task {token} expired"), Failure::ProviderInternal));
            }
            State::Completed => {}
        }
        let content = task.get("content").cloned().unwrap_or(Value::Null);
        let Some(video_url) = opt_str(&content, "video_url") else {
            return Err(self.identity.invalid(response, format!("{name} completed without a video URL")));
        };
        let duration = duration_seconds(&task);
        let media_type = if task.get("output_format").and_then(Value::as_str) == Some("mov") { "video/quicktime" } else { "video/mp4" };
        let info = duration.map(|d| MediaInfo { duration_seconds: Some(d), ..Default::default() });
        // `updated_at` is when the task reached `succeeded`, which starts the URL's retention window.
        let mut video = VideoOutput::url(&video_url, media_type, info);
        video = match opt_num(&task, "updated_at") {
            Some(at) => {
                video.expires_at = Some(at * 1000.0 + OUTPUT_RETENTION.as_millis() as f64);
                video
            }
            None => video.expiring(OUTPUT_RETENTION),
        };
        let usage = task.get("usage").cloned().unwrap_or(Value::Null);
        let usage = opt_num(&usage, "completion_tokens").map(|output| {
            VideoUsage::Tokens(TokenUsage { input: None, output: Some(output), total: opt_num(&usage, "total_tokens"), details: None })
        });
        let mut metadata = Object::new();
        metadata.insert("taskId".into(), json!(token));
        for (key, field) in [
            ("model", "model"),
            ("seed", "seed"),
            ("ratio", "ratio"),
            ("resolution", "resolution"),
            ("duration", "duration"),
            ("frames", "frames"),
            ("framesPerSecond", "framespersecond"),
            ("generateAudio", "generate_audio"),
            ("outputFormat", "output_format"),
            ("draft", "draft"),
            ("serviceTier", "service_tier"),
        ] {
            if let Some(v) = task.get(field).filter(|v| !v.is_null()) {
                metadata.insert(key.into(), crate::json::normalize_numbers(v.clone()));
            }
        }
        if let Some(last) = opt_str(&content, "last_frame_url") {
            metadata.insert("lastFrameUrl".into(), json!(last));
        }
        let mut provider_metadata = Object::new();
        provider_metadata.insert(self.provider.as_str().into(), Value::Object(metadata));
        Ok(VideoResponse { videos: vec![video], usage, provider_metadata: Some(provider_metadata) })
    }

    // DELETE cancels a queued task. Ark refuses it for a running task, and deletes the record of a finished one.
    fn cancel(&self, token: &str) -> Option<(&'static str, String)> {
        Some(("DELETE", self.status_path(token)))
    }
}

/// `SeedanceVideo.<provider>.model`.
pub fn model(provider: Provider, id: &str, deployment: &Deployment, provider_options: Option<Object>) -> VideoModel {
    let base = match provider {
        Provider::Volcengine => VOLCENGINE_BASE_URL,
        Provider::Byteplus => BYTEPLUS_BASE_URL,
    };
    VideoModel::from_route(Arc::new(SeedanceVideo::new(provider)), base, MediaPath::Static(PATH.into()), id, deployment, provider_options)
}
