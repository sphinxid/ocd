//! `protocols/alibaba-images.ts`: Model Studio's multimodal generation (Qwen-Image, Wan).

use super::openai::shape;
use super::sizing::{self, Limits};
use super::zai::expiring_url;
use super::{ImageModel, ImageProtocol, ImageRequest, ImageResponse, RouteInput};
use crate::error::AIError;
use crate::json::{merge_records, Object};
use crate::route::media::{media_reference, Deployment, Identity, MediaBody, MediaPath, MediaResponse};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub const PATH: &str = "/services/aigc/multimodal-generation/generation";
const OUTPUT_RETENTION_MS: f64 = 24.0 * 3600.0 * 1000.0;

static IDENTITY: Lazy<Identity> = Lazy::new(|| Identity::new("alibaba-images", "Alibaba Images", "alibaba"));

fn native_options(options: Option<&Object>) -> Option<Object> {
    let options = options?;
    let mut out = Object::new();
    for (camel, snake) in [
        ("negativePrompt", "negative_prompt"),
        ("promptExtend", "prompt_extend"),
        ("thinkingMode", "thinking_mode"),
        ("enableSequential", "enable_sequential"),
    ] {
        if let Some(v) = options.get(camel) {
            out.insert(snake.into(), v.clone());
        }
    }
    for (k, v) in options {
        if !matches!(k.as_str(), "negativePrompt" | "promptExtend" | "thinkingMode" | "enableSequential") {
            out.insert(k.clone(), v.clone());
        }
    }
    Some(out)
}

pub struct AlibabaImages;

impl ImageProtocol for AlibabaImages {
    fn identity(&self) -> &Identity {
        &IDENTITY
    }

    fn body(&self, request: &ImageRequest) -> Result<MediaBody, AIError> {
        let mut content = vec![];
        for image in &request.images {
            let reference = media_reference(image, None, &format!("{} input", IDENTITY.name))?;
            content.push(json!({ "image": reference.value() }));
        }
        content.push(json!({ "text": request.prompt }));
        let limits = Limits { multiple: 16.0, min_edge: None, max_edge: None, min_pixels: Some(512.0 * 512.0), max_pixels: Some(2048.0 * 2048.0), max_ratio: None };
        let size = sizing::exact(&IDENTITY, shape(request), limits)?;
        let mut parameters = Object::new();
        if let Some(size) = size {
            parameters.insert("size".into(), json!(size.render("*")));
        }
        if let Some(n) = request.n {
            parameters.insert("n".into(), json!(n));
        }
        let native = native_options(request.provider_options.as_ref());
        let parameters = merge_records(&[Some(&parameters), native.as_ref()]);
        let mut base = Object::new();
        base.insert("model".into(), json!(request.model.id));
        base.insert("input".into(), json!({ "messages": [{ "role": "user", "content": content }] }));
        if let Some(p) = parameters {
            base.insert("parameters".into(), Value::Object(p));
        }
        Ok(MediaBody::json(merge_records(&[Some(&base), request.http.as_ref().and_then(|h| h.body.as_ref())]).unwrap_or_default()))
    }

    fn decode(&self, response: &MediaResponse, _request: &ImageRequest, _body: &MediaBody) -> Result<ImageResponse, AIError> {
        let decoded: Document = IDENTITY.decode_json(response)?;
        // Qwen-Image returns every image in one choice; Wan returns one choice per image.
        let urls: Vec<&str> = decoded
            .output
            .choices
            .iter()
            .flat_map(|c| c.message.content.iter().filter_map(|item| item.image.as_deref()))
            .collect();
        if urls.is_empty() {
            return Err(IDENTITY.invalid(response, format!("{} returned no images", IDENTITY.name)));
        }
        let mut alibaba = Object::new();
        if let Some(id) = &decoded.request_id {
            alibaba.insert("requestID".into(), json!(id));
        }
        if let Some(Value::Object(usage)) = &decoded.usage {
            alibaba.insert("usage".into(), Value::Object(usage.clone()));
        }
        let mut metadata = Object::new();
        metadata.insert("alibaba".into(), Value::Object(alibaba));
        // Model Studio bills per image; its token counts are informational, so the raw record stays in metadata.
        Ok(ImageResponse {
            images: urls.into_iter().map(|u| expiring_url(u, OUTPUT_RETENTION_MS)).collect(),
            usage: None,
            provider_metadata: Some(metadata),
        })
    }
}

#[derive(Deserialize)]
struct ContentItem {
    #[serde(default)]
    image: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    content: Vec<ContentItem>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Deserialize)]
struct Output {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Document {
    output: Output,
    #[serde(default)]
    usage: Option<Value>,
    #[serde(default)]
    request_id: Option<String>,
}

/// `AlibabaImages.model(input)`: no default base URL, the region's endpoint comes from the provider.
pub fn model(id: &str, deployment: &Deployment, provider_options: Option<Object>) -> ImageModel {
    ImageModel::from_route(
        RouteInput { protocol: Arc::new(AlibabaImages), provider: None, path: MediaPath::Static(PATH.into()), base_url: None, headers: None },
        id,
        deployment,
        provider_options,
    )
}
