//! `protocols/minimax-video.ts`: MiniMax runs two video APIs with different bodies, statuses and results: H3 on
//! `/v2` and Hailuo on `/v1`.

use super::{decode_doc, encode_component, ended, null_or, opt_num, opt_str, pending, state, status_of, PollContext, Reason, State};
use super::{VideoModel, VideoOutput, VideoProtocol, VideoRequest, VideoResponse, VideoStatus, VideoUsage};
use crate::error::AIError;
use crate::json::{merge_records, num, Object};
use crate::media::{MediaAsset, MediaInfo, MediaKind};
use crate::route::media::{media_reference, Deployment, Failure, Identity, MediaBody, MediaPath, MediaReference, MediaResponse};
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// The API host without a version: H3 is served under `/v2` and Hailuo under `/v1`.
pub const DEFAULT_BASE_URL: &str = "https://api.minimax.io";
pub const PATH: &str = "/v2/video_generation";
pub const STATUS_PATH: &str = "/v2/query/video_generation";
pub const HAILUO_PATH: &str = "/v1/video_generation";
pub const HAILUO_STATUS_PATH: &str = "/v1/query/video_generation";
pub const FILE_PATH: &str = "/v1/files/retrieve";
const DOWNLOAD_RETENTION: Duration = Duration::from_secs(3600);

static IDENTITY: Lazy<Identity> = Lazy::new(|| Identity::new("minimax-video", "MiniMax Video", "minimax"));
static HAILUO: Lazy<Identity> = Lazy::new(|| Identity::new("minimax-hailuo-video", "MiniMax Hailuo Video", "minimax"));

const STATUS: &[(&str, State)] = &[
    ("queued", State::Running),
    ("running", State::Running),
    ("succeeded", State::Completed),
    ("failed", State::Ended(Reason::Failed)),
    ("cancelled", State::Ended(Reason::Cancelled)),
];

const HAILUO_STATUS: &[(&str, State)] = &[
    ("Preparing", State::Running),
    ("Queueing", State::Running),
    ("Processing", State::Running),
    ("Success", State::Completed),
    ("Fail", State::Ended(Reason::Failed)),
];

// Error codes from https://platform.minimax.io/docs/api-reference/errorcode; unknown codes are provider-side.
const FAILURE: &[(&str, Failure)] = &[
    ("1002", Failure::RateLimit),
    ("1004", Failure::Authentication),
    ("1039", Failure::RateLimit),
    ("2013", Failure::InvalidRequest),
    ("2049", Failure::Authentication),
];
const INSUFFICIENT_BALANCE: f64 = 1008.0;
// 1026 flags the input and 1027 the generated video.
const CONTENT_POLICY: &[&str] = &["1026", "1027"];

/// A provider error code: a string or a number, as JavaScript's `String(code)` prints it.
fn code_string(code: &Value) -> Option<String> {
    match code {
        Value::String(s) => Some(s.clone()),
        Value::Number(_) => Some(crate::json::js_string(code)),
        _ => None,
    }
}

fn failed(identity: &Identity, response: &MediaResponse, name: &str, code: Option<String>, detail: Option<String>) -> AIError {
    let message = format!(
        "{name} generation failed{}{}",
        code.as_ref().map(|c| format!(" ({c})")).unwrap_or_default(),
        detail.as_ref().map(|d| format!(": {d}")).unwrap_or_default()
    );
    if let Some(c) = &code {
        if CONTENT_POLICY.contains(&c.as_str()) {
            return identity.content_policy(response, message);
        }
        if c.trim().parse::<f64>().ok() == Some(INSUFFICIENT_BALANCE) {
            return identity.quota_exceeded(response, message);
        }
    }
    ended(identity, response, Reason::Failed, message, super::failure(FAILURE, code.as_deref()))
}

/// v1 answers errors with HTTP 200 and a non-zero `base_resp.status_code`.
fn base_error(response: &MediaResponse, base: Option<&Value>) -> Option<AIError> {
    let base = base.filter(|b| b.is_object())?;
    let code = base.get("status_code")?;
    if code.as_f64() == Some(0.0) {
        return None;
    }
    Some(failed(&HAILUO, response, &HAILUO.name, code_string(code), opt_str(base, "status_msg")))
}

fn base_resp_ok(v: &Value) -> bool {
    match v.get("base_resp") {
        None | Some(Value::Null) => true,
        Some(Value::Object(b)) => b.get("status_code").is_some_and(Value::is_number) && b.get("status_msg").is_none_or(|m| m.is_null() || m.is_string()),
        _ => false,
    }
}

/// H3 takes URLs, data URIs and `mm_file://{file_id}` references to MiniMax files.
fn media_url(asset: &MediaAsset) -> Result<String, AIError> {
    Ok(match media_reference(asset, Some(&IDENTITY.provider), &IDENTITY.name)? {
        MediaReference::Ref(id) => format!("mm_file://{id}"),
        other => other.value().to_string(),
    })
}

fn item(asset: &MediaAsset, role: &str) -> Result<Value, AIError> {
    let url = media_url(asset)?;
    let kind = match asset.kind() {
        MediaKind::Video => "video_url",
        MediaKind::Audio => "audio_url",
        _ => "image_url",
    };
    let mut out = Object::new();
    out.insert("type".into(), json!(kind));
    out.insert(kind.into(), json!({ "url": url }));
    out.insert("role".into(), json!(role));
    Ok(Value::Object(out))
}

fn reference(asset: &MediaAsset) -> Result<Value, AIError> {
    item(
        asset,
        match asset.kind() {
            MediaKind::Video => "reference_video",
            MediaKind::Audio => "reference_audio",
            _ => "reference_image",
        },
    )
}

/// MiniMax spells resolutions in uppercase (`480P`, `768P`, `2K`).
fn resolution(request: &VideoRequest) -> Option<String> {
    request.resolution.as_ref().map(|r| r.to_uppercase())
}

fn task_ok(v: &Value) -> bool {
    null_or(v, "id", Value::is_string)
        && null_or(v, "model", Value::is_string)
        && null_or(v, "status", Value::is_string)
        && null_or(v, "error", Value::is_object)
        && null_or(v, "content", Value::is_object)
        && null_or(v, "resolution", Value::is_string)
        && null_or(v, "duration", Value::is_number)
        && null_or(v, "ratio", Value::is_string)
        && null_or(v, "usage", Value::is_object)
}

/// The H3 task, which `response_format: "flat"` moves from `task` to the top level.
fn decode_task(response: &MediaResponse) -> Result<Value, AIError> {
    let value = decode_doc(&IDENTITY, response, |v| task_ok(v) && null_or(v, "task", |t| t.is_object() && task_ok(t)))?;
    Ok(match value.get("task") {
        Some(task) if task.is_object() => task.clone(),
        _ => value,
    })
}

pub struct MiniMaxVideo;

#[async_trait]
impl VideoProtocol for MiniMaxVideo {
    fn identity(&self) -> &Identity {
        &IDENTITY
    }

    fn body(&self, request: &VideoRequest) -> Result<MediaBody, AIError> {
        // H3 always generates a soundtrack and has no field for `sound`, so it isn't sent.
        let mut content = vec![json!({ "type": "text", "text": request.prompt })];
        if let Some(start) = &request.start {
            content.push(item(start, "first_frame")?);
        }
        if let Some(end) = &request.end {
            content.push(item(end, "last_frame")?);
        }
        for r in &request.references {
            content.push(reference(r)?);
        }
        // H3 rejects a t2va (text-only) request without a concrete `ratio`, so `auto` and unset default to `16:9`.
        let ratio = match request.aspect.as_deref() {
            None | Some("auto") if content.len() == 1 => Some("16:9".to_string()),
            Some("auto") => Some("adaptive".to_string()),
            other => other.map(String::from),
        };
        let mut base = Object::new();
        base.insert("model".into(), json!(request.model.id));
        base.insert("content".into(), Value::Array(content));
        if let Some(r) = resolution(request) {
            base.insert("resolution".into(), json!(r));
        }
        if let Some(d) = request.duration {
            base.insert("duration".into(), num(d));
        }
        if let Some(r) = ratio {
            base.insert("ratio".into(), json!(r));
        }
        Ok(MediaBody::json(
            merge_records(&[Some(&base), request.provider_options.as_ref(), request.http.as_ref().and_then(|h| h.body.as_ref())]).unwrap_or_default(),
        ))
    }

    fn decode_start(&self, response: &MediaResponse) -> Result<String, AIError> {
        let value = decode_doc(&IDENTITY, response, |v| v.get("task_id").is_some_and(Value::is_string))?;
        Ok(value["task_id"].as_str().unwrap_or_default().to_string())
    }

    fn status_path(&self, token: &str) -> String {
        format!("{STATUS_PATH}/{}", encode_component(token))
    }

    fn decode_status(&self, response: &MediaResponse, _token: &str) -> Result<VideoStatus, AIError> {
        let task = decode_task(response)?;
        Ok(status_of(STATUS, task.get("status").and_then(Value::as_str).unwrap_or("")))
    }

    async fn decode_result(&self, response: &MediaResponse, context: &PollContext<'_>) -> Result<VideoResponse, AIError> {
        let task = decode_task(response)?;
        let token = context.token;
        match state(STATUS, task.get("status").and_then(Value::as_str).unwrap_or("")) {
            State::Running => return Err(pending(&IDENTITY, response, token)),
            State::Ended(Reason::Failed) => {
                let error = task.get("error").cloned().unwrap_or(Value::Null);
                return Err(failed(&IDENTITY, response, &IDENTITY.name, error.get("code").and_then(code_string), opt_str(&error, "message")));
            }
            State::Ended(_) => {
                return Err(ended(&IDENTITY, response, Reason::Cancelled, format!("{} task {token} was cancelled", IDENTITY.name), Failure::ProviderInternal));
            }
            State::Completed => {}
        }
        let content = task.get("content").cloned().unwrap_or(Value::Null);
        let Some(url) = opt_str(&content, "url") else {
            return Err(IDENTITY.invalid(response, format!("{} completed without a video URL", IDENTITY.name)));
        };
        let duration = opt_num(&task, "duration");
        let usage = task.get("usage").filter(|u| u.is_object()).cloned();
        let total = usage.as_ref().and_then(|u| u.get("total_seconds")).and_then(Value::as_f64);
        let mut metadata = Object::new();
        metadata.insert("taskId".into(), json!(token));
        for (key, field) in [("model", "model"), ("resolution", "resolution"), ("ratio", "ratio")] {
            if let Some(v) = opt_str(&task, field) {
                metadata.insert(key.into(), json!(v));
            }
        }
        if let Some(u) = usage {
            metadata.insert("usage".into(), u);
        }
        let mut provider_metadata = Object::new();
        provider_metadata.insert("minimax".into(), Value::Object(metadata));
        Ok(VideoResponse {
            // The URL is time-limited without a documented window; reading the result again returns a fresh one.
            videos: vec![VideoOutput::url(&url, "video/mp4", duration.map(|d| MediaInfo { duration_seconds: Some(d), ..Default::default() }))],
            usage: total.map(VideoUsage::Seconds),
            provider_metadata: Some(provider_metadata),
        })
    }

    // Cancels a queued task; MiniMax rejects it once the task is running.
    fn cancel(&self, token: &str) -> Option<(&'static str, String)> {
        Some(("DELETE", format!("{PATH}/{}", encode_component(token))))
    }
}

/// v1 documents URLs and data URLs only, so MiniMax file references aren't sent.
fn hailuo_url(asset: &MediaAsset) -> Result<String, AIError> {
    Ok(media_reference(asset, None, &HAILUO.name)?.value().to_string())
}

fn decode_hailuo_task(response: &MediaResponse) -> Result<Value, AIError> {
    decode_doc(&HAILUO, response, |v| {
        null_or(v, "status", Value::is_string)
            && null_or(v, "file_id", Value::is_string)
            && null_or(v, "video_width", Value::is_number)
            && null_or(v, "video_height", Value::is_number)
            && base_resp_ok(v)
    })
}

pub struct MiniMaxHailuoVideo;

#[async_trait]
impl VideoProtocol for MiniMaxHailuoVideo {
    fn identity(&self) -> &Identity {
        &HAILUO
    }

    fn body(&self, request: &VideoRequest) -> Result<MediaBody, AIError> {
        if request.references.iter().any(|a| matches!(a.kind(), MediaKind::Video | MediaKind::Audio)) {
            return Err(HAILUO.unsupported("video.references", format!("{} takes only image (subject) references", HAILUO.name)));
        }
        // Hailuo output is always silent and its shape follows the model or the first frame, with no field for either.
        if request.aspect.as_deref().is_some_and(|a| a != "auto") {
            return Err(HAILUO.unsupported("video.aspect", format!("{} has no aspect ratio input", HAILUO.name)));
        }
        let subjects = request.references.iter().map(|a| hailuo_url(a).map(|url| json!({ "type": "character", "image": [url] }))).collect::<Result<Vec<_>, _>>()?;
        let mut base = Object::new();
        base.insert("model".into(), json!(request.model.id));
        base.insert("prompt".into(), json!(request.prompt));
        if let Some(start) = &request.start {
            base.insert("first_frame_image".into(), json!(hailuo_url(start)?));
        }
        if let Some(end) = &request.end {
            base.insert("last_frame_image".into(), json!(hailuo_url(end)?));
        }
        if !subjects.is_empty() {
            base.insert("subject_reference".into(), Value::Array(subjects));
        }
        if let Some(d) = request.duration {
            base.insert("duration".into(), num(d));
        }
        if let Some(r) = resolution(request) {
            base.insert("resolution".into(), json!(r));
        }
        Ok(MediaBody::json(
            merge_records(&[Some(&base), request.provider_options.as_ref(), request.http.as_ref().and_then(|h| h.body.as_ref())]).unwrap_or_default(),
        ))
    }

    fn decode_start(&self, response: &MediaResponse) -> Result<String, AIError> {
        let value = decode_doc(&HAILUO, response, |v| null_or(v, "task_id", Value::is_string) && base_resp_ok(v))?;
        if let Some(error) = base_error(response, value.get("base_resp")) {
            return Err(error);
        }
        let task_id = opt_str(&value, "task_id").unwrap_or_default();
        if task_id.is_empty() {
            return Err(HAILUO.invalid(response, format!("{} returned no task_id", HAILUO.name)));
        }
        Ok(task_id)
    }

    fn status_path(&self, token: &str) -> String {
        format!("{HAILUO_STATUS_PATH}?task_id={}", encode_component(token))
    }

    fn decode_status(&self, response: &MediaResponse, _token: &str) -> Result<VideoStatus, AIError> {
        let task = decode_hailuo_task(response)?;
        let status = status_of(HAILUO_STATUS, task.get("status").and_then(Value::as_str).unwrap_or(""));
        // A failed task also carries its reason in `base_resp`; any other non-zero code is a failed read.
        if status.reason != Some(Reason::Failed)
            && let Some(error) = base_error(response, task.get("base_resp"))
        {
            return Err(error);
        }
        Ok(status)
    }

    async fn decode_result(&self, response: &MediaResponse, context: &PollContext<'_>) -> Result<VideoResponse, AIError> {
        let task = decode_hailuo_task(response)?;
        let token = context.token;
        let status = state(HAILUO_STATUS, task.get("status").and_then(Value::as_str).unwrap_or(""));
        let error = base_error(response, task.get("base_resp"));
        if status == State::Ended(Reason::Failed) {
            return Err(error.unwrap_or_else(|| failed(&HAILUO, response, &HAILUO.name, None, None)));
        }
        if let Some(error) = error {
            return Err(error);
        }
        if status != State::Completed {
            return Err(pending(&HAILUO, response, token));
        }
        let file_id = opt_str(&task, "file_id").unwrap_or_default();
        if file_id.is_empty() {
            return Err(HAILUO.invalid(response, format!("{} completed without a file_id", HAILUO.name)));
        }
        // The task names a file; its download URL takes a second lookup and is valid for one hour.
        let file_response = context.get(&format!("{FILE_PATH}?file_id={}", encode_component(&file_id))).await?;
        let file = decode_doc(&HAILUO, &file_response, |v| null_or(v, "file", |f| f.is_object() && null_or(f, "download_url", Value::is_string)) && base_resp_ok(v))?;
        if let Some(error) = base_error(&file_response, file.get("base_resp")) {
            return Err(error);
        }
        let Some(url) = file.get("file").and_then(|f| opt_str(f, "download_url")) else {
            return Err(HAILUO.invalid(&file_response, format!("{} returned no download_url for {file_id}", HAILUO.name)));
        };
        let width = opt_num(&task, "video_width");
        let height = opt_num(&task, "video_height");
        let info = (width.is_some() || height.is_some()).then(|| MediaInfo { width, height, ..Default::default() });
        let mut provider_metadata = Object::new();
        provider_metadata.insert("minimax".into(), json!({ "taskId": token, "fileId": file_id }));
        Ok(VideoResponse {
            videos: vec![VideoOutput::url(&url, "video/mp4", info).expiring(DOWNLOAD_RETENTION)],
            usage: None,
            provider_metadata: Some(provider_metadata),
        })
    }
}

/// H3 and later models on the v2 API.
pub fn h3_model(id: &str, deployment: &Deployment, provider_options: Option<Object>) -> VideoModel {
    VideoModel::from_route(Arc::new(MiniMaxVideo), DEFAULT_BASE_URL, MediaPath::Static(PATH.into()), id, deployment, provider_options)
}

/// Hailuo and the `*-01` models on the v1 API.
pub fn hailuo_model(id: &str, deployment: &Deployment, provider_options: Option<Object>) -> VideoModel {
    VideoModel::from_route(Arc::new(MiniMaxHailuoVideo), DEFAULT_BASE_URL, MediaPath::Static(HAILUO_PATH.into()), id, deployment, provider_options)
}

// v1 is MiniMax's legacy API with a fixed model list; every newer model ships on v2.
static HAILUO_MODEL: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^(minimax-hailuo-|[tis]2v-01|video-01)").unwrap());

/// Picks the API by model family: Hailuo and `*-01` models use v1, everything else v2.
pub fn model(id: &str, deployment: &Deployment, provider_options: Option<Object>) -> VideoModel {
    if HAILUO_MODEL.is_match(id) { hailuo_model(id, deployment, provider_options) } else { h3_model(id, deployment, provider_options) }
}

/// H3 is under `/v2` and Hailuo under `/v1`, so video drops the version a shared `baseURL` carries for the other APIs.
pub fn video_base_url(base: &str) -> String {
    static VERSION: Lazy<Regex> = Lazy::new(|| Regex::new(r"(/v1)?/*$").unwrap());
    VERSION.replace(base, "").into_owned()
}
