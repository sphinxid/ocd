//! `protocols/minimax-images.ts`: MiniMax's `/image_generation` (`image-01`, `image-01-live`).

use super::input;
use super::openai::shape;
use super::sizing::{self, Limits, Size};
use super::zai::expiring_url;
use super::{ImageModel, ImageProtocol, ImageRequest, ImageResponse, RouteInput};
use crate::error::AIError;
use crate::json::{merge_records, Object};
use crate::route::media::{invalid_request, media_reference, Deployment, Failure, Identity, MediaBody, MediaPath, MediaResponse};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub const DEFAULT_BASE_URL: &str = "https://api.minimax.io/v1";
pub const PATH: &str = "/image_generation";
const OUTPUT_RETENTION_MS: f64 = 24.0 * 3600.0 * 1000.0;

static IDENTITY: Lazy<Identity> = Lazy::new(|| Identity::new("minimax-images", "MiniMax Images", "minimax"));

const INSUFFICIENT_BALANCE: i64 = 1008;
const CONTENT_POLICY: i64 = 1026;

fn failure(code: i64) -> Failure {
    match code {
        1002 => Failure::RateLimit,
        1004 | 2049 => Failure::Authentication,
        2013 => Failure::InvalidRequest,
        _ => Failure::ProviderInternal,
    }
}

/// MiniMax's `aspect_ratio` presets and the sizes it renders them at.
const PRESETS: &[(&str, Size)] = &[
    ("1:1", Size::new(1024.0, 1024.0)),
    ("16:9", Size::new(1280.0, 720.0)),
    ("4:3", Size::new(1152.0, 864.0)),
    ("3:2", Size::new(1248.0, 832.0)),
    ("2:3", Size::new(832.0, 1248.0)),
    ("3:4", Size::new(864.0, 1152.0)),
    ("9:16", Size::new(720.0, 1280.0)),
    ("21:9", Size::new(1344.0, 576.0)),
];

const LIMITS: Limits = Limits { multiple: 8.0, min_edge: Some(512.0), max_edge: Some(2048.0), min_pixels: None, max_pixels: None, max_ratio: None };

/// `image-01-live` rejects `21:9`.
fn presets(model: &str) -> Vec<(&'static str, Size)> {
    PRESETS.iter().copied().filter(|(key, _)| model == "image-01" || *key != "21:9").collect()
}

/// Presets render at about 1K, so they serve a preset aspect with no resolution or `1K`.
fn preset(request: &ImageRequest) -> Option<&'static str> {
    let aspect = request.aspect.as_deref().unwrap_or("1:1");
    let (key, _) = presets(&request.model.id).into_iter().find(|(key, _)| *key == aspect)?;
    if let Some(r) = &request.resolution
        && sizing::tier(r) != Some(1.0)
    {
        return None;
    }
    Some(key)
}

/// Only `image-01` takes free `width` and `height`; other models render the presets alone.
fn sizing_fields(request: &ImageRequest) -> Result<Object, AIError> {
    let mut out = Object::new();
    if !sizing::is_requested(shape(request)) {
        return Ok(out);
    }
    if let Some(aspect) = preset(request) {
        out.insert("aspect_ratio".into(), json!(aspect));
        return Ok(out);
    }
    if request.model.id == "image-01" {
        if let Some(size) = sizing::exact(&IDENTITY, shape(request), LIMITS)? {
            out.insert("width".into(), json!(size.width as i64));
            out.insert("height".into(), json!(size.height as i64));
        }
        return Ok(out);
    }
    let available = presets(&request.model.id);
    let sizes: Vec<Size> = available.iter().map(|(_, s)| *s).collect();
    let closest = sizing::nearest(shape(request), &sizes);
    let name = available.iter().find(|(_, s)| *s == closest).map(|(k, _)| *k).unwrap_or_default();
    let requested: Vec<&str> = [request.aspect.as_deref(), request.resolution.as_deref()].into_iter().flatten().collect();
    Err(IDENTITY.unsupported(
        if request.resolution.is_none() { "media.aspect" } else { "media.resolution" },
        format!(
            "{} can't render {}; the closest size it takes is {}x{} ({name}). Change aspect or resolution, or set aspect_ratio in providerOptions",
            request.model.id,
            requested.join(" at "),
            closest.width as i64,
            closest.height as i64
        ),
    ))
}

fn is_native_size(options: Option<&Object>) -> bool {
    options.is_some_and(|o| o.contains_key("aspect_ratio") || o.contains_key("width") || o.contains_key("height"))
}

fn native_options(options: Option<&Object>) -> Object {
    let mut out = Object::new();
    let response_format = options.and_then(|o| o.get("responseFormat")).filter(|v| !v.is_null()).cloned().unwrap_or(json!("base64"));
    out.insert("response_format".into(), response_format);
    if let Some(v) = options.and_then(|o| o.get("promptOptimizer")) {
        out.insert("prompt_optimizer".into(), v.clone());
    }
    for (k, v) in options.into_iter().flatten() {
        if k != "responseFormat" && k != "promptOptimizer" {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

pub struct MiniMaxImages;

impl ImageProtocol for MiniMaxImages {
    fn identity(&self) -> &Identity {
        &IDENTITY
    }

    fn body(&self, request: &ImageRequest) -> Result<MediaBody, AIError> {
        if request.images.len() > 1 {
            return Err(IDENTITY.unsupported("media.images", format!("{} takes one subject reference image", IDENTITY.name)));
        }
        if is_native_size(request.provider_options.as_ref()) && sizing::is_requested(shape(request)) {
            return Err(invalid_request(
                "MiniMax providerOptions aspect_ratio, width and height replace aspect and resolution; set one or the other",
            ));
        }
        // The API only takes a `character` (portrait) subject, one per request.
        let mut references = vec![];
        for image in &request.images {
            let reference = media_reference(image, None, &format!("{} input", IDENTITY.name))?;
            references.push(json!({ "type": "character", "image_file": reference.value() }));
        }
        let mut base = Object::new();
        base.insert("model".into(), json!(request.model.id));
        base.insert("prompt".into(), json!(request.prompt));
        if !references.is_empty() {
            base.insert("subject_reference".into(), Value::Array(references));
        }
        if let Some(n) = request.n {
            base.insert("n".into(), json!(n));
        }
        for (k, v) in sizing_fields(request)? {
            base.insert(k, v);
        }
        let native = native_options(request.provider_options.as_ref());
        Ok(MediaBody::json(
            merge_records(&[Some(&base), Some(&native), request.http.as_ref().and_then(|h| h.body.as_ref())]).unwrap_or_default(),
        ))
    }

    fn decode(&self, response: &MediaResponse, _request: &ImageRequest, _body: &MediaBody) -> Result<ImageResponse, AIError> {
        let decoded: Document = IDENTITY.decode_json(response)?;
        let invalid = |message: String| IDENTITY.invalid(response, message);
        // MiniMax answers errors with HTTP 200 and a non-zero `base_resp.status_code`.
        if let Some(status) = &decoded.base_resp
            && status.status_code != 0.0
        {
            let code = status.status_code as i64;
            let message = format!(
                "{} failed ({}): {}",
                IDENTITY.name,
                crate::json::js_number_string(&serde_json::Number::from_f64(status.status_code).unwrap_or(0.into())),
                status.status_msg.as_deref().unwrap_or("unknown error")
            );
            if code == CONTENT_POLICY {
                return Err(IDENTITY.content_policy(response, message));
            }
            if code == INSUFFICIENT_BALANCE {
                return Err(IDENTITY.quota_exceeded(response, message));
            }
            return Err(IDENTITY.failed(response, message, failure(code)));
        }
        let mut images = vec![];
        let data = decoded.data.as_ref();
        for (index, item) in data.and_then(|d| d.image_base64.as_ref()).into_iter().flatten().enumerate() {
            images.push(input::decoded_asset(&invalid, &format!("{} result {index}", IDENTITY.name), item, None)?);
        }
        // MiniMax returns no content type for URLs; the media type resolves when the asset is materialized.
        for url in data.and_then(|d| d.image_urls.as_ref()).into_iter().flatten() {
            images.push(expiring_url(url, OUTPUT_RETENTION_MS));
        }
        let failed = decoded.metadata.as_ref().and_then(|m| m.failed_count.as_ref()).map(js_number).unwrap_or(0.0);
        if images.is_empty() && failed > 0.0 {
            return Err(IDENTITY.content_policy(
                response,
                format!("{} blocked {} image(s) for content safety", IDENTITY.name, crate::json::js_string(&crate::json::num(failed))),
            ));
        }
        if images.is_empty() {
            return Err(invalid(format!("{} returned no images", IDENTITY.name)));
        }
        let mut minimax = Object::new();
        if let Some(id) = &decoded.id {
            minimax.insert("id".into(), json!(id));
        }
        if let Some(v) = decoded.metadata.as_ref().and_then(|m| m.success_count.clone()) {
            minimax.insert("successCount".into(), v);
        }
        if let Some(v) = decoded.metadata.as_ref().and_then(|m| m.failed_count.clone()) {
            minimax.insert("failedCount".into(), v);
        }
        let mut metadata = Object::new();
        metadata.insert("minimax".into(), Value::Object(minimax));
        Ok(ImageResponse { images, usage: None, provider_metadata: Some(metadata) })
    }
}

/// `Number(value)` for a count sent as a number or a string.
fn js_number(value: &Value) -> f64 {
    match value {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) if s.trim().is_empty() => 0.0,
        Value::String(s) => s.trim().parse().unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}

#[derive(Deserialize)]
struct Data {
    #[serde(default)]
    image_urls: Option<Vec<String>>,
    #[serde(default)]
    image_base64: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Metadata {
    #[serde(default, deserialize_with = "count")]
    success_count: Option<Value>,
    #[serde(default, deserialize_with = "count")]
    failed_count: Option<Value>,
}

fn count<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    let value: Option<Value> = Option::deserialize(d)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(v @ (Value::Number(_) | Value::String(_))) => Ok(Some(v)),
        Some(_) => Err(serde::de::Error::custom("count must be a number or a string")),
    }
}

#[derive(Deserialize)]
struct BaseResp {
    status_code: f64,
    #[serde(default)]
    status_msg: Option<String>,
}

#[derive(Deserialize)]
struct Document {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    data: Option<Data>,
    #[serde(default)]
    metadata: Option<Metadata>,
    #[serde(default)]
    base_resp: Option<BaseResp>,
}

/// `MiniMaxImages.model(input)`.
pub fn model(id: &str, deployment: &Deployment, provider_options: Option<Object>) -> ImageModel {
    ImageModel::from_route(
        RouteInput {
            protocol: Arc::new(MiniMaxImages),
            provider: None,
            path: MediaPath::Static(PATH.into()),
            base_url: Some(DEFAULT_BASE_URL.into()),
            headers: None,
        },
        id,
        deployment,
        provider_options,
    )
}
