//! `protocols/openai-images.ts`: OpenAI's `/images/generations` and `/images/edits`, also spoken by OpenAI-compatible
//! hosts, OpenRouter, Vercel's gateway, Azure and DigitalOcean. `generate` reads the whole JSON document as one
//! frame; the SSE stream mode has no caller in ocd.

use super::input::{self, append_fields};
use super::sizing::{self, Limits, Shape, Size};
use super::{token_usage, ImageProtocol, ImageRequest, ImageResponse, RouteInput};
use crate::error::AIError;
use crate::json::{merge_records, Object};
use crate::media::MediaAsset;
use crate::route::media::{self, invalid_request, Deployment, FormData, Identity, MediaBody, MediaPath, MediaReference, MediaResponse};
use crate::obj;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const PATH: &str = "/images/generations";
pub const EDIT_PATH: &str = "/images/edits";

/// Multipart field names the route owns; `http.body` overlays cannot smuggle replacements for them.
const RESERVED_FORM_FIELDS: &[&str] = &["model", "prompt", "image", "image[]", "images", "mask"];

static IDENTITY: Lazy<Identity> = Lazy::new(|| Identity::new("openai-images", "OpenAI Images", "openai"));

/// The body fields `aspect` and `resolution` lower to; gateways that read other fields than `size` pass their own.
pub type Sizing = fn(&ImageRequest) -> Option<Object>;

/// How input images travel. `Edits` is OpenAI's `/images/edits`, uploading owned bytes as multipart; `JsonEdits`
/// sends every edit there as JSON with data URLs, for hosts that reject multipart (the ChatGPT backend);
/// `References` keeps every request on `/images/generations` and lists them as `input_references`, for gateways
/// without an edits endpoint (OpenRouter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageInput {
    Edits,
    JsonEdits,
    References,
}

/// `gpt-image-1`, `chatgpt-image` and DALL·E take a fixed list, `gpt-image-2` and later any size within limits.
/// Other models are gateway-hosted (`bfl/flux-2-pro`); the gateway passes `size` through to them as is.
enum Sizes {
    Free(Limits),
    Listed(Vec<Size>),
}

static GPT_IMAGE_1: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(gpt-image-1|chatgpt-image)").unwrap());

fn sizes(model: &str) -> Sizes {
    let id = &model[model.rfind('/').map(|i| i + 1).unwrap_or(0)..];
    if id.starts_with("dall-e-2") {
        return Sizes::Listed([256.0, 512.0, 1024.0].into_iter().map(|e| Size::new(e, e)).collect());
    }
    if id.starts_with("dall-e-3") {
        return Sizes::Listed(vec![Size::new(1024.0, 1024.0), Size::new(1792.0, 1024.0), Size::new(1024.0, 1792.0)]);
    }
    if GPT_IMAGE_1.is_match(id) {
        return Sizes::Listed(vec![Size::new(1024.0, 1024.0), Size::new(1536.0, 1024.0), Size::new(1024.0, 1536.0)]);
    }
    if id.starts_with("gpt-image-") {
        return Sizes::Free(Limits {
            multiple: 16.0,
            min_edge: None,
            max_edge: Some(3840.0),
            min_pixels: Some(655_360.0),
            max_pixels: Some(8_294_400.0),
            max_ratio: Some(3.0),
        });
    }
    Sizes::Free(Limits::multiple(16.0))
}

pub fn shape(request: &ImageRequest) -> Shape<'_> {
    Shape { aspect: request.aspect.as_deref(), resolution: request.resolution.as_deref() }
}

/// `pixelSize`: `size` as `WIDTHxHEIGHT`.
pub fn pixel_size(request: &ImageRequest) -> Option<Object> {
    let shape = shape(request);
    if !sizing::is_requested(shape) {
        return None;
    }
    let size = match sizes(&request.model.id) {
        Sizes::Free(limits) => sizing::fit(shape, limits),
        Sizes::Listed(list) => sizing::nearest(shape, &list),
    };
    Some(obj! { "size" => size.render("x") }.as_object().cloned().unwrap_or_default())
}

/// `nativeOptions`: known options to their wire names, everything else as given.
fn native_options(options: Option<&Object>) -> Option<Object> {
    let options = options?;
    let mut out = Object::new();
    if let Some(v) = options.get("outputCompression") {
        out.insert("output_compression".into(), v.clone());
    }
    if let Some(v) = options.get("outputFormat") {
        out.insert("output_format".into(), v.clone());
    }
    for (k, v) in options {
        if matches!(k.as_str(), "outputCompression" | "outputFormat" | "partialImages" | "mask") {
            continue;
        }
        out.insert(k.clone(), v.clone());
    }
    // `response_format: null` drops a provider's default, for hosts that reject the field.
    if out.get("response_format").is_some_and(Value::is_null) {
        out.shift_remove("response_format");
    }
    Some(out)
}

/// A provider-options `mask` asset in its JSON form (`Media.Asset` or a `data:` URL string).
fn mask(request: &ImageRequest) -> Option<MediaAsset> {
    let value = request.option("mask")?;
    if let Some(s) = value.as_str() {
        return crate::media::parse_data_url(s);
    }
    serde_json::from_value(value.clone()).ok()
}

fn is_inline(asset: &MediaAsset) -> bool {
    matches!(asset.source_type(), "bytes" | "base64")
}

fn reference(asset: &MediaAsset, provider: &str) -> Result<Value, AIError> {
    Ok(match media::media_reference(asset, Some(provider), &IDENTITY.name)? {
        MediaReference::Ref(id) => json!({ "file_id": id }),
        other => json!({ "image_url": other.value() }),
    })
}

/// The OpenAI Images protocol with a host's sizing and image input.
pub struct OpenAIImages {
    pub sizing: Sizing,
    pub image_input: ImageInput,
}

impl OpenAIImages {
    pub const fn new(sizing: Sizing, image_input: ImageInput) -> Self {
        Self { sizing, image_input }
    }
}

impl ImageProtocol for OpenAIImages {
    fn identity(&self) -> &Identity {
        &IDENTITY
    }

    fn body(&self, request: &ImageRequest) -> Result<MediaBody, AIError> {
        let images = &request.images;
        let mask = mask(request);
        if self.image_input == ImageInput::References && mask.is_some() {
            return Err(IDENTITY.unsupported("media.mask", format!("{} image edits take no mask", request.model.provider)));
        }
        if mask.is_some() && images.is_empty() {
            return Err(invalid_request("An OpenAI image mask requires at least one input image"));
        }
        if request.option("size").is_some() && sizing::is_requested(shape(request)) {
            return Err(invalid_request("OpenAI providerOptions.size replaces aspect and resolution; set one or the other"));
        }
        let mut first = Object::new();
        if let Some(n) = request.n {
            first.insert("n".into(), json!(n));
        }
        for (k, v) in (self.sizing)(request).unwrap_or_default() {
            first.insert(k, v);
        }
        let native = native_options(request.provider_options.as_ref());
        let fields =
            merge_records(&[Some(&first), native.as_ref(), request.http.as_ref().and_then(|h| h.body.as_ref())]).unwrap_or_default();

        if self.image_input == ImageInput::References {
            let mut references = vec![];
            for image in images {
                references.push(media::media_reference(image, None, &IDENTITY.name)?);
            }
            let mut base = Object::new();
            base.insert("model".into(), json!(request.model.id));
            base.insert("prompt".into(), json!(request.prompt));
            if !references.is_empty() {
                base.insert(
                    "input_references".into(),
                    Value::Array(references.iter().map(|r| json!({ "type": "image_url", "image_url": { "url": r.value() } })).collect()),
                );
            }
            return Ok(MediaBody::json(merge_records(&[Some(&base), Some(&fields)]).unwrap_or_default()));
        }

        // Owned bytes go through multipart edits; remote URLs, file IDs and `JsonEdits` hosts use the JSON edits body.
        if self.image_input == ImageInput::Edits
            && !images.is_empty()
            && images.iter().all(is_inline)
            && mask.as_ref().is_none_or(is_inline)
        {
            let mut form = FormData::new();
            let base = obj! { "model" => request.model.id.clone(), "prompt" => request.prompt.clone() };
            append_fields(&mut form, base.as_object().unwrap(), Some(&fields), RESERVED_FORM_FIELDS, None);
            for (index, image) in images.iter().enumerate() {
                let data = input::inline_bytes(&IDENTITY.id, image)?;
                form.append_file("image[]", data, image.media_type(), format!("image-{index}"));
            }
            if let Some(mask) = &mask {
                let data = input::inline_bytes(&IDENTITY.id, mask)?;
                form.append_file("mask", data, mask.media_type(), "mask");
            }
            return Ok(MediaBody::Multipart(form));
        }

        let mut references = vec![];
        for image in images {
            references.push(reference(image, &request.model.provider)?);
        }
        let mask_reference = match &mask {
            Some(m) => Some(reference(m, &request.model.provider)?),
            None => None,
        };
        let mut base = Object::new();
        base.insert("model".into(), json!(request.model.id));
        base.insert("prompt".into(), json!(request.prompt));
        if !references.is_empty() {
            base.insert("images".into(), Value::Array(references));
        }
        if let Some(m) = mask_reference {
            base.insert("mask".into(), m);
        }
        Ok(MediaBody::json(merge_records(&[Some(&base), Some(&fields)]).unwrap_or_default()))
    }

    fn decode(&self, response: &MediaResponse, request: &ImageRequest, _body: &MediaBody) -> Result<ImageResponse, AIError> {
        // Frame errors carry the response's HTTP context, as the stream route adds it.
        let invalid = |message: String| IDENTITY.invalid(response, message);
        let decoded: Document = serde_json::from_slice(&response.body).map_err(|e| {
            let mut error = invalid(format!("{} returned an invalid response", IDENTITY.name));
            error.cause = Some(e.to_string());
            error
        })?;
        // Compatible hosts may omit `output_format` and ignore the requested one, so the bytes decide unreported types.
        let format = decoded.output_format.clone();
        let media_type = format.as_ref().map(|f| format!("image/{f}"));
        let mut images = vec![];
        for (index, item) in decoded.data.iter().enumerate() {
            images.push(input::image_output(
                &invalid,
                &format!("{} result {index}", IDENTITY.name),
                item.b64_json.as_deref(),
                item.url.as_deref(),
                media_type.as_deref(),
            )?);
        }
        if images.is_empty() {
            return Err(invalid(format!("{} returned no images", IDENTITY.name)));
        }
        let provider = request.model.provider.clone();
        let usage = decoded.usage.as_ref().and_then(|u| {
            let mut details = Object::new();
            details.insert(provider.clone(), Value::Object(u.raw.clone()));
            token_usage(u.input_tokens, u.output_tokens, u.total_tokens, Some(details))
        });
        let mut settings = Object::new();
        for (key, value) in [
            ("outputFormat", &decoded.output_format),
            ("size", &decoded.size),
            ("quality", &decoded.quality),
            ("background", &decoded.background),
        ] {
            if let Some(v) = value {
                settings.insert(key.into(), json!(v));
            }
        }
        let mut metadata = Object::new();
        metadata.insert(provider, Value::Object(settings));
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

/// `usage`: the counts with the raw record kept for the usage details.
struct Usage {
    input_tokens: Option<f64>,
    output_tokens: Option<f64>,
    total_tokens: Option<f64>,
    raw: Object,
}

#[derive(Deserialize)]
struct Document {
    data: Vec<Item>,
    #[serde(default)]
    output_format: Option<String>,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    quality: Option<String>,
    #[serde(default)]
    background: Option<String>,
    #[serde(default, deserialize_with = "usage")]
    usage: Option<Usage>,
}

fn usage<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Usage>, D::Error> {
    let value: Option<Value> = Option::deserialize(d)?;
    let Some(Value::Object(raw)) = value else { return Ok(None) };
    let count = |key: &str| -> Result<Option<f64>, D::Error> {
        match raw.get(key) {
            None => Ok(None),
            Some(Value::Number(n)) => Ok(n.as_f64()),
            Some(_) => Err(serde::de::Error::custom(format!("usage.{key} is not a number"))),
        }
    };
    Ok(Some(Usage { input_tokens: count("input_tokens")?, output_tokens: count("output_tokens")?, total_tokens: count("total_tokens")?, raw }))
}

/// `OpenAIImages.protocol` with the defaults: pixel sizes and multipart edits.
pub fn protocol() -> Arc<dyn ImageProtocol> {
    Arc::new(OpenAIImages::new(pixel_size, ImageInput::Edits))
}

/// `OpenAIImages.model(input)`: `provider` attributes models, refs and metadata to an OpenAI-compatible host;
/// `sizing` replaces the `size` that `aspect` and `resolution` lower to, and `image_input` how input images go.
pub fn model(
    id: &str,
    deployment: &Deployment,
    provider: Option<&str>,
    provider_options: Option<Object>,
    sizing: Option<Sizing>,
    image_input: Option<ImageInput>,
) -> super::ImageModel {
    let image_input = image_input.unwrap_or(ImageInput::Edits);
    let protocol: Arc<dyn ImageProtocol> = Arc::new(OpenAIImages::new(sizing.unwrap_or(pixel_size), image_input));
    let path = MediaPath::Dynamic(Arc::new(move |request: &ImageRequest| {
        if image_input != ImageInput::References && !request.images.is_empty() { EDIT_PATH.to_string() } else { PATH.to_string() }
    }));
    super::ImageModel::from_route(
        RouteInput { protocol, provider: provider.map(String::from), path, base_url: Some(DEFAULT_BASE_URL.into()), headers: None },
        id,
        deployment,
        provider_options,
    )
}
