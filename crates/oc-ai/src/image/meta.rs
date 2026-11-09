//! `protocols/meta-images.ts`: Meta's `/images/generations` and `/images/edits` (Muse).

use super::input;
use super::sizing::{self, Limits, Shape};
use super::{token_usage, ImageModel, ImageProtocol, ImageRequest, ImageResponse, RouteInput};
use crate::error::AIError;
use crate::json::{merge_records, Object};
use crate::route::media::{invalid_request, media_reference, Deployment, Identity, MediaBody, MediaPath, MediaResponse};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub const DEFAULT_BASE_URL: &str = "https://api.meta.ai/v1";

static IDENTITY: Lazy<Identity> = Lazy::new(|| Identity::new("meta-images", "Meta Images", "meta"));

/// The body's typed fields (`Body` schema): a wrong type fails the request before it is sent.
fn validate(body: &Object) -> Result<(), AIError> {
    let string = |key: &str| body.get(key).is_none_or(Value::is_string);
    let ok = body.get("model").is_some_and(Value::is_string)
        && body.get("prompt").is_some_and(Value::is_string)
        && body.get("images").is_none_or(|v| v.as_array().is_some_and(|a| a.iter().all(Value::is_object)))
        && body.get("n").is_none_or(Value::is_number)
        && string("size")
        && string("output_format")
        && string("response_format")
        && string("reasoning_strength")
        && body.get("tool_enablement").is_none_or(|v| v.as_object().is_some_and(|o| o.values().all(Value::is_boolean)));
    if ok { Ok(()) } else { Err(invalid_request("Expected a valid Meta Images request body")) }
}

pub struct MetaImages;

impl ImageProtocol for MetaImages {
    fn identity(&self) -> &Identity {
        &IDENTITY
    }

    fn body(&self, request: &ImageRequest) -> Result<MediaBody, AIError> {
        // Meta has no file handles: refs are rejected even when they name this provider.
        let mut images = vec![];
        for image in &request.images {
            images.push(json!({ "image_url": media_reference(image, None, &IDENTITY.name)?.value() }));
        }
        let mut native = request.provider_options.clone().unwrap_or_default();
        let output_format = native.shift_remove("outputFormat");
        let response_format = native.shift_remove("responseFormat");
        let reasoning_strength = native.shift_remove("reasoningStrength");
        let tool_enablement = native.shift_remove("toolEnablement");
        if native.contains_key("size") && request.aspect.is_some() {
            return Err(invalid_request("Meta providerOptions.size replaces aspect; set one or the other"));
        }
        let hint = request.aspect.as_deref().map(|aspect| sizing::fit(Shape { aspect: Some(aspect), resolution: None }, Limits::multiple(16.0)));
        let mut base = Object::new();
        base.insert("model".into(), json!(request.model.id));
        base.insert("prompt".into(), json!(request.prompt));
        if !images.is_empty() {
            base.insert("images".into(), Value::Array(images));
        }
        if let Some(n) = request.n {
            base.insert("n".into(), json!(n));
        }
        if let Some(hint) = hint {
            base.insert("size".into(), json!(hint.render("x")));
        }
        for (key, value) in [
            ("output_format", output_format),
            ("response_format", response_format),
            ("reasoning_strength", reasoning_strength),
            ("tool_enablement", tool_enablement),
        ] {
            if let Some(v) = value {
                base.insert(key.into(), v);
            }
        }
        let payload =
            merge_records(&[Some(&base), Some(&native), request.http.as_ref().and_then(|h| h.body.as_ref())]).unwrap_or_default();
        validate(&payload)?;
        Ok(MediaBody::json(payload))
    }

    fn decode(&self, response: &MediaResponse, _request: &ImageRequest, body: &MediaBody) -> Result<ImageResponse, AIError> {
        let decoded: Document = IDENTITY.decode_json(response)?;
        let invalid = |message: String| IDENTITY.invalid(response, message);
        let requested = body.json_value().and_then(|b| b.get("output_format")).and_then(Value::as_str);
        let format = decoded.output_format.clone().or(requested.map(String::from)).unwrap_or_else(|| "webp".into());
        let media_type = format!("image/{format}");
        let mut images = vec![];
        for (index, item) in decoded.data.iter().enumerate() {
            images.push(input::image_output(
                &invalid,
                &format!("{} result {index}", IDENTITY.name),
                item.b64_json.as_deref(),
                item.url.as_deref(),
                Some(&media_type),
            )?);
        }
        if images.is_empty() {
            return Err(invalid(format!("{} returned no images", IDENTITY.name)));
        }
        let usage = decoded.usage.as_ref().and_then(|u| {
            let num = |key: &str| u.get(key).and_then(Value::as_f64);
            let mut details = Object::new();
            details.insert("meta".into(), Value::Object(u.clone()));
            token_usage(num("input_tokens"), num("output_tokens"), num("total_tokens"), Some(details))
        });
        let mut metadata = Object::new();
        metadata.insert("meta".into(), json!({ "outputFormat": format }));
        Ok(ImageResponse { images, usage, provider_metadata: Some(metadata) })
    }
}

#[derive(Deserialize)]
struct Item {
    #[serde(default)]
    b64_json: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize)]
struct Document {
    data: Vec<Item>,
    #[serde(default)]
    output_format: Option<String>,
    #[serde(default)]
    usage: Option<Object>,
}

/// `MetaImages.model(input)`.
pub fn model(id: &str, deployment: &Deployment, provider_options: Option<Object>) -> ImageModel {
    ImageModel::from_route(
        RouteInput {
            protocol: Arc::new(MetaImages),
            provider: None,
            path: MediaPath::Dynamic(Arc::new(|r: &ImageRequest| {
                format!("/images/{}", if r.images.is_empty() { "generations" } else { "edits" })
            })),
            base_url: Some(DEFAULT_BASE_URL.into()),
            headers: None,
        },
        id,
        deployment,
        provider_options,
    )
}
