//! `protocols/xai-images.ts`: xAI's `/images/generations` and `/images/edits` (Grok Imagine).

use super::input;
use super::sizing;
use super::{ImageModel, ImageProtocol, ImageRequest, ImageResponse, RouteInput};
use crate::error::AIError;
use crate::json::{merge_records, Object};
use crate::media::MediaAsset;
use crate::route::media::{media_reference, Deployment, Identity, MediaBody, MediaPath, MediaReference, MediaResponse};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";
pub const PATH: &str = "/images/generations";
pub const EDIT_PATH: &str = "/images/edits";

static IDENTITY: Lazy<Identity> = Lazy::new(|| Identity::new("xai-images", "xAI Images", "xai"));

/// xAI renders `1k` or `2k`; other tiers use the nearest one.
pub fn resolution(request: &ImageRequest) -> Option<&'static str> {
    let r = request.resolution.as_deref()?;
    Some(if sizing::tier(r).unwrap_or(1.0) < 1.5 { "1k" } else { "2k" })
}

fn reference(asset: &MediaAsset) -> Result<Value, AIError> {
    Ok(match media_reference(asset, Some(&IDENTITY.provider), &IDENTITY.name)? {
        MediaReference::Ref(id) => json!({ "file_id": id }),
        other => json!({ "url": other.value(), "type": "image_url" }),
    })
}

fn native_options(options: Option<&Object>) -> Object {
    let mut out = Object::new();
    let response_format = options.and_then(|o| o.get("responseFormat")).filter(|v| !v.is_null()).cloned().unwrap_or(json!("b64_json"));
    out.insert("response_format".into(), response_format);
    for (k, v) in options.into_iter().flatten() {
        if k != "responseFormat" {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

pub struct XAIImages;

impl ImageProtocol for XAIImages {
    fn identity(&self) -> &Identity {
        &IDENTITY
    }

    fn body(&self, request: &ImageRequest) -> Result<MediaBody, AIError> {
        let mut references = vec![];
        for image in &request.images {
            references.push(reference(image)?);
        }
        let mut base = Object::new();
        base.insert("model".into(), json!(request.model.id));
        base.insert("prompt".into(), json!(request.prompt));
        // xAI takes one edit source as `image` and several as `images`.
        if references.len() == 1 {
            base.insert("image".into(), references[0].clone());
        }
        if references.len() > 1 {
            base.insert("images".into(), Value::Array(references));
        }
        if let Some(n) = request.n {
            base.insert("n".into(), json!(n));
        }
        if let Some(a) = &request.aspect {
            base.insert("aspect_ratio".into(), json!(a));
        }
        if let Some(r) = resolution(request) {
            base.insert("resolution".into(), json!(r));
        }
        let native = native_options(request.provider_options.as_ref());
        Ok(MediaBody::json(
            merge_records(&[Some(&base), Some(&native), request.http.as_ref().and_then(|h| h.body.as_ref())]).unwrap_or_default(),
        ))
    }

    fn decode(&self, response: &MediaResponse, _request: &ImageRequest, _body: &MediaBody) -> Result<ImageResponse, AIError> {
        let decoded: Document = IDENTITY.decode_json(response)?;
        let invalid = |message: String| IDENTITY.invalid(response, message);
        let mut images = vec![];
        for (index, item) in decoded.data.iter().enumerate() {
            let mut asset = input::image_output(
                &invalid,
                &format!("{} result {index}", IDENTITY.name),
                item.b64_json.as_deref(),
                item.url.as_deref(),
                item.mime_type.as_deref(),
            )?;
            if let Some(prompt) = &item.revised_prompt {
                let mut xai = Object::new();
                xai.insert("xai".into(), json!({ "revisedPrompt": prompt }));
                asset.provider_metadata = Some(xai);
            }
            images.push(asset);
        }
        if images.is_empty() {
            return Err(invalid(format!("{} returned no images", IDENTITY.name)));
        }
        // xAI reports a USD cost (`cost_in_usd_ticks`) rather than tokens; the raw record stays in provider metadata.
        let usage = match decoded.usage {
            Some(Value::Object(o)) => Some(o),
            _ => None,
        };
        let provider_metadata = usage.map(|u| {
            let mut out = Object::new();
            out.insert("xai".into(), json!({ "usage": u }));
            out
        });
        Ok(ImageResponse { images, usage: None, provider_metadata })
    }
}

#[derive(Deserialize)]
struct Item {
    #[serde(default)]
    b64_json: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    revised_prompt: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
}

#[derive(Deserialize)]
struct Document {
    data: Vec<Item>,
    #[serde(default)]
    usage: Option<Value>,
}

/// `XAIImages.model(input)`.
pub fn model(id: &str, deployment: &Deployment, provider_options: Option<Object>) -> ImageModel {
    ImageModel::from_route(
        RouteInput {
            protocol: Arc::new(XAIImages),
            provider: None,
            path: MediaPath::Dynamic(Arc::new(|r: &ImageRequest| if r.images.is_empty() { PATH.into() } else { EDIT_PATH.into() })),
            base_url: Some(DEFAULT_BASE_URL.into()),
            headers: None,
        },
        id,
        deployment,
        provider_options,
    )
}
