//! `protocols/zai-images.ts`: Z.ai's `/images/generations` (GLM-Image, CogView).

use super::openai::shape;
use super::sizing::{self, Limits};
use super::{ImageModel, ImageProtocol, ImageRequest, ImageResponse, RouteInput};
use crate::error::AIError;
use crate::json::{merge_records, Object};
use crate::media::MediaAsset;
use crate::route::media::{Deployment, Identity, MediaBody, MediaPath, MediaResponse};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub const DEFAULT_BASE_URL: &str = "https://api.z.ai/api/paas/v4";
pub const PATH: &str = "/images/generations";
/// Z.ai keeps generated images for 30 days.
pub const OUTPUT_RETENTION_MS: f64 = 30.0 * 24.0 * 3600.0 * 1000.0;

static IDENTITY: Lazy<Identity> = Lazy::new(|| Identity::new("zai-images", "Z.ai Images", "zai"));

/// GLM-Image takes 1024–2048 px edges in steps of 32; CogView takes 512–2048 px in steps of 16.
fn limits(model: &str) -> Limits {
    if model.starts_with("glm-image") {
        Limits { multiple: 32.0, min_edge: Some(1024.0), max_edge: Some(2048.0), min_pixels: None, max_pixels: Some(4_194_304.0), max_ratio: None }
    } else {
        Limits { multiple: 16.0, min_edge: Some(512.0), max_edge: Some(2048.0), min_pixels: None, max_pixels: Some(2_097_152.0), max_ratio: None }
    }
}

fn native_options(options: Option<&Object>) -> Option<Object> {
    let options = options?;
    let mut out = Object::new();
    if let Some(v) = options.get("userID") {
        out.insert("user_id".into(), v.clone());
    }
    for (k, v) in options {
        if k != "userID" {
            out.insert(k.clone(), v.clone());
        }
    }
    Some(out)
}

/// A `url` asset whose provider-declared retention window starts now (`MediaProtocol.expiringUrl`).
pub fn expiring_url(url: &str, retention_ms: f64) -> MediaAsset {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0);
    MediaAsset::from_source(crate::media::MediaSource::Url { url: url.into(), media_type: None, expires_at: Some(now + retention_ms) })
}

pub struct ZAIImages;

impl ImageProtocol for ZAIImages {
    fn identity(&self) -> &Identity {
        &IDENTITY
    }

    fn unsupported(&self) -> &'static [&'static str] {
        &["images", "n"]
    }

    fn body(&self, request: &ImageRequest) -> Result<MediaBody, AIError> {
        let size = sizing::exact(&IDENTITY, shape(request), limits(&request.model.id))?;
        let mut base = Object::new();
        base.insert("model".into(), json!(request.model.id));
        base.insert("prompt".into(), json!(request.prompt));
        if let Some(size) = size {
            base.insert("size".into(), json!(size.render("x")));
        }
        let native = native_options(request.provider_options.as_ref());
        Ok(MediaBody::json(
            merge_records(&[Some(&base), native.as_ref(), request.http.as_ref().and_then(|h| h.body.as_ref())]).unwrap_or_default(),
        ))
    }

    fn decode(&self, response: &MediaResponse, _request: &ImageRequest, _body: &MediaBody) -> Result<ImageResponse, AIError> {
        let decoded: Document = IDENTITY.decode_json(response)?;
        if decoded.data.is_empty() {
            return Err(IDENTITY.invalid(response, format!("{} returned no images", IDENTITY.name)));
        }
        let mut zai = Object::new();
        if let Some(v) = decoded.created {
            zai.insert("created".into(), json!(v));
        }
        if let Some(v) = &decoded.id {
            zai.insert("id".into(), json!(v));
        }
        if let Some(v) = &decoded.request_id {
            zai.insert("requestID".into(), json!(v));
        }
        if let Some(v) = &decoded.content_filter {
            zai.insert("contentFilter".into(), v.clone());
        }
        let mut metadata = Object::new();
        metadata.insert("zai".into(), Value::Object(zai));
        // Z.ai returns only URLs and no content type; the media type resolves when the asset is materialized.
        Ok(ImageResponse {
            images: decoded.data.iter().map(|item| expiring_url(&item.url, OUTPUT_RETENTION_MS)).collect(),
            usage: None,
            provider_metadata: Some(metadata),
        })
    }
}

#[derive(Deserialize)]
struct Item {
    url: String,
}

#[derive(Deserialize)]
struct Document {
    #[serde(default)]
    created: Option<i64>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
    data: Vec<Item>,
    #[serde(default)]
    content_filter: Option<Value>,
}

/// `ZAIImages.model(input)`.
pub fn model(id: &str, deployment: &Deployment, provider_options: Option<Object>) -> ImageModel {
    ImageModel::from_route(
        RouteInput {
            protocol: Arc::new(ZAIImages),
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
