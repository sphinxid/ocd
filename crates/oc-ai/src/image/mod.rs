//! Image generation (`packages/ai/src/image.ts`, `image-client.ts`, `media-model.ts`).
//!
//! An [`ImageModel`] carries its route: one protocol (`openai-images`, `google-images`, ...) composed with an
//! endpoint, auth and deployment headers. [`generate`] runs the request through it, the inline call core's
//! `Generate.image` makes. `stream`, `start` and `resume` are not ported: nothing in ocd calls them.

pub mod alibaba;
pub mod cloudflare;
pub mod google;
pub mod input;
pub mod meta;
pub mod minimax;
pub mod openai;
pub mod sizing;
pub mod xai;
pub mod zai;

use crate::error::AIError;
use crate::json::{merge_records, Object};
use crate::media::MediaAsset;
use crate::route::auth::Auth;
use crate::route::media::{self, Deployment, Identity, MediaBody, MediaEndpoint, MediaPath, MediaResponse};
use crate::types::HttpOptions;
use indexmap::IndexMap;
use serde_json::Value;
use std::sync::Arc;

/// `MediaUsage` of kind `tokens` (`tokenUsage`); the other kinds have no image protocol that reports them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenUsage {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub total: Option<f64>,
    pub details: Option<Object>,
}

/// `tokenUsage(counts)`: a usage without any count is no usage.
pub fn token_usage(input: Option<f64>, output: Option<f64>, total: Option<f64>, details: Option<Object>) -> Option<TokenUsage> {
    if input.is_none() && output.is_none() && total.is_none() {
        return None;
    }
    Some(TokenUsage { input, output, total, details })
}

/// `Image.Request` after `Image.request(...)`.
#[derive(Clone)]
pub struct ImageRequest {
    pub model: ImageModel,
    pub prompt: String,
    /// Edit sources or style/subject references, in order.
    pub images: Vec<MediaAsset>,
    pub n: Option<i64>,
    pub aspect: Option<String>,
    /// Size tier: `1K` is about as many pixels as 1024x1024 at the requested aspect, `2K` as 2048x2048.
    pub resolution: Option<String>,
    pub provider_options: Option<Object>,
    pub http: Option<HttpOptions>,
}

impl ImageRequest {
    pub fn new(model: ImageModel, prompt: impl Into<String>) -> Self {
        Self { model, prompt: prompt.into(), images: vec![], n: None, aspect: None, resolution: None, provider_options: None, http: None }
    }
    /// A string provider option (`providerOptions.<key>` when it is a string).
    pub fn option_str(&self, key: &str) -> Option<&str> {
        self.provider_options.as_ref()?.get(key)?.as_str()
    }
    pub fn option(&self, key: &str) -> Option<&Value> {
        self.provider_options.as_ref()?.get(key)
    }
}

/// `Image.Response`.
#[derive(Debug, Clone, Default)]
pub struct ImageResponse {
    pub images: Vec<MediaAsset>,
    pub usage: Option<TokenUsage>,
    pub provider_metadata: Option<Object>,
}

/// One image protocol: how a request becomes its body and how a response becomes an [`ImageResponse`]
/// (`MediaProtocol.Inline`; the streamed OpenAI protocol answers `generate` with one document).
pub trait ImageProtocol: Send + Sync {
    fn identity(&self) -> &Identity;
    /// Common request fields this protocol cannot lower; the route rejects them before `body` runs.
    fn unsupported(&self) -> &'static [&'static str] {
        &[]
    }
    fn body(&self, request: &ImageRequest) -> Result<MediaBody, AIError>;
    fn decode(&self, response: &MediaResponse, request: &ImageRequest, body: &MediaBody) -> Result<ImageResponse, AIError>;
}

/// A protocol composed with one deployment (`MediaRoute.inline` / `stream` via `composeRoute`).
pub struct ImageRoute {
    pub protocol: Arc<dyn ImageProtocol>,
    /// `route.protocol.provider`: the protocol's own, or the host a factory attributes models to.
    pub provider: String,
    pub endpoint: MediaEndpoint<ImageRequest>,
    pub auth: Arc<dyn Auth>,
    /// Deployment headers applied before transport authentication.
    pub headers: Option<IndexMap<String, String>>,
}

/// `ImageModel`: ids, the configured route and deployment overlays.
#[derive(Clone)]
pub struct ImageModel {
    pub id: String,
    pub provider: String,
    pub route: Arc<ImageRoute>,
    pub http: Option<HttpOptions>,
    /// Defaults under each request's `providerOptions`.
    pub provider_options: Option<Object>,
}

impl std::fmt::Debug for ImageModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageModel")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("route", &self.route.protocol.identity().id)
            .finish()
    }
}

/// `MediaModel.RouteInput`: a protocol plus its canonical base URL and start path.
pub struct RouteInput {
    pub protocol: Arc<dyn ImageProtocol>,
    /// Overrides the protocol's provider (an OpenAI-compatible host).
    pub provider: Option<String>,
    pub path: MediaPath<ImageRequest>,
    pub base_url: Option<String>,
    /// Headers the protocol requires on every call; deployment headers win.
    pub headers: Option<IndexMap<String, String>>,
}

impl ImageModel {
    /// `ImageModel.fromRoute(route, input)`.
    pub fn from_route(route: RouteInput, id: &str, deployment: &Deployment, provider_options: Option<Object>) -> Self {
        let provider = route.provider.clone().unwrap_or_else(|| route.protocol.identity().provider.clone());
        let headers = match (&route.headers, &deployment.headers) {
            (None, None) => None,
            (a, b) => {
                let mut merged = a.clone().unwrap_or_default();
                for (k, v) in b.iter().flatten() {
                    merged.insert(k.clone(), v.clone());
                }
                Some(merged)
            }
        };
        let endpoint = MediaEndpoint {
            base_url: deployment.base_url.clone().or(route.base_url.clone()),
            path: route.path,
            query: IndexMap::new(),
        };
        Self {
            id: id.to_string(),
            provider: provider.clone(),
            route: Arc::new(ImageRoute { protocol: route.protocol, provider, endpoint, auth: deployment.auth.clone(), headers }),
            http: deployment.http.clone(),
            provider_options,
        }
    }
}

fn present(request: &ImageRequest, fields: &[&str]) -> Vec<&'static str> {
    let mut out = vec![];
    for field in fields {
        let set = match *field {
            "images" => !request.images.is_empty(),
            "n" => request.n.is_some(),
            "aspect" => request.aspect.is_some(),
            "resolution" => request.resolution.is_some(),
            _ => false,
        };
        if set {
            out.push(match *field {
                "images" => "images",
                "n" => "n",
                "aspect" => "aspect",
                _ => "resolution",
            });
        }
    }
    out
}

/// The request as the protocol sees it: route and model HTTP overlays merged under the request's own, and the
/// model's provider options under the request's.
pub fn resolve(request: &ImageRequest) -> ImageRequest {
    let route_http = request.model.route.headers.clone().map(HttpOptions::headers);
    let http = HttpOptions::merge(&[route_http.as_ref(), request.model.http.as_ref(), request.http.as_ref()]);
    let provider_options = merge_records(&[request.model.provider_options.as_ref(), request.provider_options.as_ref()]);
    ImageRequest { http, provider_options, ..request.clone() }
}

/// The body the route would send for this request (tests and wire fixtures).
pub fn body(request: &ImageRequest) -> Result<MediaBody, AIError> {
    let route = &request.model.route;
    let identity = route.protocol.identity();
    media::reject_unsupported(&identity.id, &route.provider, &present(request, route.protocol.unsupported()))?;
    route.protocol.body(&resolve(request))
}

/// `Image.generate`: the inline call (`MediaRoute.generate`).
pub async fn generate(client: &reqwest::Client, request: ImageRequest) -> Result<ImageResponse, AIError> {
    let route = request.model.route.clone();
    let identity = route.protocol.identity();
    media::reject_unsupported(&identity.id, &route.provider, &present(&request, route.protocol.unsupported()))?;
    let resolved = resolve(&request);
    let body = route.protocol.body(&resolved)?;
    let response = media::submit(client, &route.endpoint, route.auth.as_ref(), resolved.http.as_ref(), &resolved, &body).await?;
    route.protocol.decode(&response, &resolved, &body)
}

/// `asset.materialize()` + `base64()`: an output image as inline base64 with its media type. URL results are
/// downloaded, and the type of a downloaded image comes from its bytes when the URL didn't declare it.
pub async fn materialize(client: &reqwest::Client, asset: &MediaAsset) -> Result<(String, String), AIError> {
    use crate::media::MediaSource;
    use base64::Engine;
    match &asset.source {
        MediaSource::Base64 { data, media_type } => Ok((data.clone(), media_type.clone())),
        MediaSource::Bytes { data, media_type } => Ok((base64::engine::general_purpose::STANDARD.encode(data), media_type.clone())),
        MediaSource::Url { url, media_type, .. } => {
            let data = media::download(client, url, None).await?;
            let materialized = MediaAsset::bytes(data, media_type.as_deref());
            let mime = materialized.media_type();
            let MediaSource::Bytes { data, .. } = materialized.source else { unreachable!() };
            Ok((base64::engine::general_purpose::STANDARD.encode(data), mime))
        }
        MediaSource::Ref { provider, id, .. } => Err(AIError::invalid_request(format!("Cannot materialize provider ref {provider}:{id}"))),
    }
}
