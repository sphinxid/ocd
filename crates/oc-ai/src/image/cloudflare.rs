//! `protocols/cloudflare-workers-ai-images.ts`: Workers AI's Run API for image models.

use super::input::{self, append_fields};
use super::openai::shape;
use super::sizing::{self, Limits};
use super::{ImageModel, ImageProtocol, ImageRequest, ImageResponse, RouteInput};
use crate::error::AIError;
use crate::json::{merge_records, Object};
use crate::media::MediaAsset;
use crate::route::media::{inline_required, Deployment, FormData, Identity, MediaBody, MediaPath, MediaResponse};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

static IDENTITY: Lazy<Identity> =
    Lazy::new(|| Identity::new("cloudflare-workers-ai-images", "Cloudflare Workers AI", "cloudflare-workers-ai"));

/// FLUX.2 models only accept multipart form fields, even for a prompt alone.
const MULTIPART_MODEL_PREFIX: &str = "@cf/black-forest-labs/flux-2-";
const RESERVED_FORM_FIELDS: &[&str] = &["prompt"];

pub struct CloudflareWorkersAIImages;

impl ImageProtocol for CloudflareWorkersAIImages {
    fn identity(&self) -> &Identity {
        &IDENTITY
    }

    fn body(&self, request: &ImageRequest) -> Result<MediaBody, AIError> {
        if request.n.is_some_and(|n| n > 1) {
            return Err(IDENTITY.unsupported("media.n", format!("{} generates one image per request; call it once per image", IDENTITY.name)));
        }
        let mut fields = Object::new();
        fields.insert("prompt".into(), json!(request.prompt));
        let limits = Limits { multiple: 16.0, min_edge: Some(256.0), max_edge: Some(2048.0), min_pixels: None, max_pixels: None, max_ratio: None };
        if let Some(size) = sizing::exact(&IDENTITY, shape(request), limits)? {
            fields.insert("width".into(), json!(size.width as i64));
            fields.insert("height".into(), json!(size.height as i64));
        }
        let native = merge_records(&[request.provider_options.as_ref(), request.http.as_ref().and_then(|h| h.body.as_ref())]);
        let images = &request.images;
        if !request.model.id.starts_with(MULTIPART_MODEL_PREFIX) {
            if images.len() > 1 {
                return Err(IDENTITY.unsupported("media.images", format!("{} takes one input image as image_b64", request.model.id)));
            }
            let image = images.first().map(MediaAsset::inline);
            if let (Some(asset), Some(None)) = (images.first(), &image) {
                return Err(inline_required(&IDENTITY.id, asset));
            }
            if let Some(Some(inline)) = image {
                fields.insert("image_b64".into(), json!(inline.base64));
            }
            return Ok(MediaBody::json(merge_records(&[Some(&fields), native.as_ref()]).unwrap_or_default()));
        }
        let mut form = FormData::new();
        append_fields(&mut form, &fields, native.as_ref(), RESERVED_FORM_FIELDS, None);
        for (index, image) in images.iter().enumerate() {
            let bytes = input::inline_bytes(&IDENTITY.id, image)?;
            form.append_file(format!("input_image_{index}"), bytes, image.media_type(), format!("image_{index}"));
        }
        Ok(MediaBody::Multipart(form))
    }

    // FLUX and Lucid Origin wrap base64 in the Run API envelope; the Stable Diffusion family, DreamShaper and
    // Phoenix answer with the raw image, whose declared content type does not always match the bytes.
    fn decode(&self, response: &MediaResponse, _request: &ImageRequest, _body: &MediaBody) -> Result<ImageResponse, AIError> {
        if !response.header("content-type").is_some_and(|t| t.starts_with("application/json")) {
            return Ok(ImageResponse { images: vec![MediaAsset::bytes(response.body.to_vec(), None)], usage: None, provider_metadata: None });
        }
        let decoded: Document = IDENTITY.decode_json(response)?;
        let invalid = |message: String| IDENTITY.invalid(response, message);
        let Some(image) = &decoded.result.image else {
            return Err(invalid(format!("{} returned no image", IDENTITY.name)));
        };
        let asset = input::decoded_asset(&invalid, &format!("{} result", IDENTITY.name), image, None)?;
        let provider_metadata = decoded.result.usage.map(|usage| {
            let mut out = Object::new();
            out.insert("cloudflare-workers-ai".into(), json!({ "usage": usage }));
            out
        });
        Ok(ImageResponse { images: vec![asset], usage: None, provider_metadata })
    }
}

#[derive(Deserialize)]
struct RunResult {
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    usage: Option<Object>,
}

#[derive(Deserialize)]
struct Document {
    result: RunResult,
}

/// `CloudflareWorkersAIImages.model(input)`: the account's Run API base comes from the provider.
pub fn model(id: &str, deployment: &Deployment, provider_options: Option<Object>) -> ImageModel {
    ImageModel::from_route(
        RouteInput {
            protocol: Arc::new(CloudflareWorkersAIImages),
            provider: None,
            path: MediaPath::Dynamic(Arc::new(|r: &ImageRequest| format!("/{}", r.model.id))),
            base_url: None,
            headers: None,
        },
        id,
        deployment,
        provider_options,
    )
}
