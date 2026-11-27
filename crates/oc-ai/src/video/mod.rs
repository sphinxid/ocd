//! Video generation (`packages/ai/src/video.ts`, `video-client.ts`, the queued half of `route/media.ts` and
//! `route/media-protocol.ts`).
//!
//! A video is always a queued job: [`start`] submits it and returns the provider's job ID, which [`status`],
//! [`result`] and [`cancel`] take, from any process. A [`VideoModel`] carries its route: one protocol (`xai-video`,
//! `minimax-video`, ...) composed with an endpoint, auth and deployment headers. Status and result reads retry
//! transient failures; a submit or cancel never does.

pub mod google;
pub mod minimax;
pub mod openrouter;
pub mod seedance;
pub mod xai;

use crate::error::{AIError, ErrorKind, HttpContext};
use crate::image::TokenUsage;
use crate::json::{merge_records, Object};
use crate::media::{MediaAsset, MediaKind, MediaSource};
use crate::route::auth::{self, Auth, AuthInput};
use crate::route::media::{self, Deployment, Failure, Identity, MediaBody, MediaCall, MediaEndpoint, MediaPath, MediaResponse};
use crate::types::HttpOptions;
use async_trait::async_trait;
use bytes::Bytes;
use indexmap::IndexMap;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// `Generation.Status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Completed,
    Error,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Error => "error",
        }
    }
}

/// `Generation.Reason`: why a generation ended in `error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Failed,
    Cancelled,
    Expired,
}

impl Reason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
        }
    }
}

/// `VideoStatus`: one status read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoStatus {
    pub status: Status,
    /// Set when `status` is `error`.
    pub reason: Option<Reason>,
    /// 0..1, only when the provider reports progress.
    pub progress: Option<f64>,
}

impl VideoStatus {
    pub fn running() -> Self {
        Self { status: Status::Running, reason: None, progress: None }
    }
}

/// `MediaProtocol.State`: a provider status in package terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Completed,
    Ended(Reason),
}

/// `MediaProtocol.state`: unknown values (inherited property names included) keep the generation running.
pub fn state(table: &[(&str, State)], raw: &str) -> State {
    table.iter().find(|(k, _)| *k == raw).map(|(_, s)| *s).unwrap_or(State::Running)
}

/// `MediaProtocol.status`: the snapshot status and reason for a provider status string.
pub fn status_of(table: &[(&str, State)], raw: &str) -> VideoStatus {
    match state(table, raw) {
        State::Running => VideoStatus::running(),
        State::Completed => VideoStatus { status: Status::Completed, reason: None, progress: None },
        State::Ended(reason) => VideoStatus { status: Status::Error, reason: Some(reason), progress: None },
    }
}

/// `MediaProtocol.failure`: missing or unmapped codes are `ProviderInternal`.
pub fn failure(table: &[(&str, Failure)], code: Option<&str>) -> Failure {
    code.and_then(|c| table.iter().find(|(k, _)| *k == c)).map(|(_, f)| *f).unwrap_or(Failure::ProviderInternal)
}

/// `MediaUsage` of a video result.
#[derive(Debug, Clone, PartialEq)]
pub enum VideoUsage {
    Tokens(TokenUsage),
    Seconds(f64),
}

/// One finished video, with the headers a download of a `url` source sends (`Media.url(url, {headers})`).
#[derive(Debug, Clone, PartialEq)]
pub struct VideoOutput {
    pub asset: MediaAsset,
    /// When the provider's URL stops working (epoch milliseconds), if known.
    pub expires_at: Option<f64>,
    pub headers: Option<IndexMap<String, String>>,
}

impl VideoOutput {
    pub fn new(asset: MediaAsset) -> Self {
        Self { asset, expires_at: None, headers: None }
    }
    /// `Media.url(url, {mediaType, info})`.
    pub fn url(url: &str, media_type: &str, info: Option<crate::media::MediaInfo>) -> Self {
        let mut asset = MediaAsset::url(url, Some(media_type.to_string()));
        asset.info = info;
        Self::new(asset)
    }
    /// `MediaProtocol.expiringUrl`: a URL whose provider-declared retention window starts now.
    pub fn expiring(mut self, retention: Duration) -> Self {
        self.expires_at = Some(now_ms() + retention.as_millis() as f64);
        self
    }
    pub fn media_type(&self) -> String {
        self.asset.media_type()
    }
}

pub(crate) fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}

/// `Video.Response`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoResponse {
    pub videos: Vec<VideoOutput>,
    pub usage: Option<VideoUsage>,
    pub provider_metadata: Option<Object>,
}

/// `Video.Request` after `Video.request(...)`.
#[derive(Clone)]
pub struct VideoRequest {
    pub model: VideoModel,
    pub prompt: String,
    /// Image the video starts on exactly.
    pub start: Option<MediaAsset>,
    /// Image the video ends on exactly.
    pub end: Option<MediaAsset>,
    /// Images, videos or audio that guide the subject or style without pinning a frame.
    pub references: Vec<MediaAsset>,
    /// Output length in seconds.
    pub duration: Option<f64>,
    /// `width:height`, or `auto` to follow the input image.
    pub aspect: Option<String>,
    pub resolution: Option<String>,
    /// Whether the video has a generated soundtrack; unset leaves the model's default.
    pub sound: Option<bool>,
    pub provider_options: Option<Object>,
    pub http: Option<HttpOptions>,
}

impl VideoRequest {
    pub fn new(model: VideoModel, prompt: impl Into<String>) -> Self {
        Self {
            model,
            prompt: prompt.into(),
            start: None,
            end: None,
            references: vec![],
            duration: None,
            aspect: None,
            resolution: None,
            sound: None,
            provider_options: None,
            http: None,
        }
    }
    pub fn option(&self, key: &str) -> Option<&Value> {
        self.provider_options.as_ref()?.get(key)
    }
}

/// `Media.image`'s check (`ImageAssetSchema`): an image, or a URL or provider reference whose type it doesn't state.
pub fn is_image_asset(asset: &MediaAsset) -> bool {
    if asset.kind() == MediaKind::Image {
        return true;
    }
    match &asset.source {
        MediaSource::Url { media_type, .. } | MediaSource::Ref { media_type, .. } => media_type.is_none(),
        _ => false,
    }
}

/// `Media.image(asset)`: fails typed for an asset known not to be an image.
pub fn image(asset: MediaAsset) -> Result<MediaAsset, AIError> {
    if is_image_asset(&asset) {
        return Ok(asset);
    }
    Err(AIError::invalid_request(format!("expected an image, got {}", asset.media_type())))
}

/// `Video.Duration`: `"<n> second(s)"` in seconds, the form the TypeScript API takes.
pub fn parse_duration(value: &str) -> Option<f64> {
    let caps = regex::Regex::new(r"^(\d+(?:\.\d+)?) seconds?$").ok()?.captures(value)?;
    caps[1].parse().ok()
}

/// One video protocol (`MediaProtocol.Queued`): the submission body, the start decode, and the status, result and
/// cancel calls addressed by the provider's job ID.
#[async_trait]
pub trait VideoProtocol: Send + Sync {
    fn identity(&self) -> &Identity;
    fn body(&self, request: &VideoRequest) -> Result<MediaBody, AIError>;
    /// The job ID from the submission response.
    fn decode_start(&self, response: &MediaResponse) -> Result<String, AIError>;
    fn status_path(&self, token: &str) -> String;
    fn decode_status(&self, response: &MediaResponse, token: &str) -> Result<VideoStatus, AIError>;
    fn result_path(&self, token: &str) -> String {
        self.status_path(token)
    }
    async fn decode_result(&self, response: &MediaResponse, context: &PollContext<'_>) -> Result<VideoResponse, AIError>;
    /// The method and path that cancel a job, when the provider has one.
    fn cancel(&self, _token: &str) -> Option<(&'static str, String)> {
        None
    }
}

/// A protocol composed with one deployment (`MediaRoute.queued`).
pub struct VideoRoute {
    pub protocol: Arc<dyn VideoProtocol>,
    pub provider: String,
    pub endpoint: MediaEndpoint<VideoRequest>,
    pub auth: Arc<dyn Auth>,
    /// Headers the protocol requires on every call, with the deployment's on top.
    pub headers: Option<IndexMap<String, String>>,
}

/// `VideoModel`: ids, the configured route and deployment overlays.
#[derive(Clone)]
pub struct VideoModel {
    pub id: String,
    pub provider: String,
    pub route: Arc<VideoRoute>,
    pub http: Option<HttpOptions>,
    /// Defaults under each request's `providerOptions`.
    pub provider_options: Option<Object>,
}

impl std::fmt::Debug for VideoModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoModel")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("route", &self.route.protocol.identity().id)
            .finish()
    }
}

impl VideoModel {
    /// `VideoModel.fromRoute(route, input)`.
    pub fn from_route(
        protocol: Arc<dyn VideoProtocol>,
        base_url: &str,
        path: MediaPath<VideoRequest>,
        id: &str,
        deployment: &Deployment,
        provider_options: Option<Object>,
    ) -> Self {
        let provider = protocol.identity().provider.clone();
        let endpoint = MediaEndpoint {
            base_url: deployment.base_url.clone().or(Some(base_url.to_string())),
            path,
            query: IndexMap::new(),
        };
        Self {
            id: id.to_string(),
            provider: provider.clone(),
            route: Arc::new(VideoRoute { protocol, provider, endpoint, auth: deployment.auth.clone(), headers: deployment.headers.clone() }),
            http: deployment.http.clone(),
            provider_options,
        }
    }
    pub fn route_id(&self) -> &str {
        &self.route.protocol.identity().id
    }
}

/// The request as the protocol sees it: route and model HTTP overlays merged under the request's own, and the
/// model's provider options under the request's.
pub fn resolve(request: &VideoRequest) -> VideoRequest {
    let route_http = request.model.route.headers.clone().map(HttpOptions::headers);
    let http = HttpOptions::merge(&[route_http.as_ref(), request.model.http.as_ref(), request.http.as_ref()]);
    let provider_options = merge_records(&[request.model.provider_options.as_ref(), request.provider_options.as_ref()]);
    VideoRequest { http, provider_options, ..request.clone() }
}

/// The body the route would send for this request (tests and wire fixtures).
pub fn body(request: &VideoRequest) -> Result<MediaBody, AIError> {
    validate(request)?;
    request.model.route.protocol.body(&resolve(request))
}

/// `Video.request`'s validation: frames must be images (or of a type it can't tell) and the duration positive.
fn validate(request: &VideoRequest) -> Result<(), AIError> {
    for frame in [&request.start, &request.end].into_iter().flatten() {
        if !is_image_asset(frame) {
            return Err(AIError::invalid_request(format!("expected an image, got {}", frame.media_type())));
        }
    }
    if let Some(d) = request.duration
        && !(d.is_finite() && d >= 0.0)
    {
        return Err(AIError::invalid_request(format!("Expected a duration in seconds, got {d}")));
    }
    Ok(())
}

/// `Video.start`: submit a generation and return the provider's job ID at once. Never retried.
pub async fn start(client: &reqwest::Client, request: VideoRequest) -> Result<String, AIError> {
    validate(&request)?;
    let route = request.model.route.clone();
    let resolved = resolve(&request);
    let body = route.protocol.body(&resolved)?;
    let response = media::submit(client, &route.endpoint, route.auth.as_ref(), resolved.http.as_ref(), &resolved, &body).await?;
    route.protocol.decode_start(&response)
}

/// Route and model overlays every follow-up call of a job shares (`transport.exchange(model)`).
fn exchange(model: &VideoModel) -> Option<HttpOptions> {
    let route_http = model.route.headers.clone().map(HttpOptions::headers);
    HttpOptions::merge(&[route_http.as_ref(), model.http.as_ref()])
}

fn base_url(route: &VideoRoute, path: &str) -> Result<url::Url, AIError> {
    let base = route.endpoint.base_url.as_deref().unwrap_or("").trim_end_matches('/');
    url::Url::parse(&format!("{base}{path}")).map_err(|_| AIError::invalid_request(format!("Invalid URL: {base}{path}")))
}

/// A bodiless follow-up call (status, result, cancel) with the same auth and headers as the submit. Provider-issued
/// absolute URLs are used as-is; everything else resolves against the base.
async fn call(client: &reqwest::Client, route: &VideoRoute, method: &'static str, path: &str, http: Option<&HttpOptions>) -> Result<MediaResponse, AIError> {
    let absolute = path.len() >= 7 && (path[..7].eq_ignore_ascii_case("http://") || path.len() >= 8 && path[..8].eq_ignore_ascii_case("https://"));
    let mut url = if absolute {
        url::Url::parse(path).map_err(|_| AIError::invalid_request(format!("Invalid URL: {path}")))?
    } else {
        base_url(route, path)?
    };
    media::with_query(&mut url, media::string_query(http.and_then(|h| h.query.as_ref())).as_ref());
    if !route.endpoint.query.is_empty() {
        let mut pairs: Vec<(String, String)> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        for (k, v) in &route.endpoint.query {
            pairs.retain(|(key, _)| key != k);
            pairs.push((k.clone(), v.clone()));
        }
        url.query_pairs_mut().clear().extend_pairs(pairs.iter());
    }
    let url = url.to_string();
    let headers = http.and_then(|h| h.headers.as_ref()).map(crate::route::normalize_headers).unwrap_or_default();
    let headers = auth::apply(route.auth.as_ref(), AuthInput { method, url: &url, body: "", headers }).await?;
    let call = MediaCall {
        method: method.into(),
        url,
        headers,
        body: None,
        tls: http.and_then(|h| h.tls.clone()),
        timeouts: crate::route::transport::Timeouts::from_http(http),
    };
    media::execute(client, &call).await
}

const READ_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
const READ_RETRIES: u32 = 8;

/// The gap before retry `attempt` (0-based) of a status or result read: exponential from 1s, capped at 30s, jittered
/// like Effect's `Schedule.jittered` (×0.8..1.2), raised to a provider `retryAfterMs` for rate limits and provider
/// errors, still capped at 30s.
pub fn read_retry_delay(attempt: u32, error: &AIError) -> Duration {
    let base = Duration::from_secs(1u64 << attempt.min(5)).min(READ_RETRY_MAX_DELAY);
    let jitter: f64 = 0.8 + rand::random::<f64>() * 0.4;
    let mut delay = base.mul_f64(jitter);
    if matches!(error.kind, ErrorKind::RateLimit { .. } | ErrorKind::ProviderInternal { .. })
        && let Some(ms) = error.retry_after_ms()
    {
        delay = delay.max(Duration::from_millis(ms.max(0.0) as u64));
    }
    delay.min(READ_RETRY_MAX_DELAY)
}

/// A GET that retries transient failures (`READ_RETRY`, `while: isRetryable`), at most 8 times.
async fn get(client: &reqwest::Client, route: &VideoRoute, path: &str, http: Option<&HttpOptions>) -> Result<MediaResponse, AIError> {
    let mut attempt = 0;
    loop {
        match call(client, route, "GET", path, http).await {
            Ok(response) => return Ok(response),
            Err(error) if attempt < READ_RETRIES && error.is_retryable() => {
                tokio::time::sleep(read_retry_delay(attempt, &error)).await;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

/// A follow-up call's inputs (`MediaProtocol.PollContext`): the job ID, a further GET with the same auth and
/// headers, and the credentials a download from the provider's host needs.
pub struct PollContext<'a> {
    pub token: &'a str,
    client: &'a reqwest::Client,
    route: &'a VideoRoute,
    http: Option<&'a HttpOptions>,
}

impl PollContext<'_> {
    /// A further GET, retried like the poll itself (MiniMax v1's file lookup).
    pub async fn get(&self, path: &str) -> Result<MediaResponse, AIError> {
        get(self.client, self.route, path, self.http).await
    }

    /// The headers a GET of `url` sends (deployment headers plus auth), for result URLs that only download with the
    /// same credentials. A URL on another origin than the route's base URL gets none.
    pub async fn credentials(&self, url: &str) -> Result<Option<IndexMap<String, String>>, AIError> {
        let Ok(target) = url::Url::parse(url) else { return Ok(None) };
        let Ok(base) = base_url(self.route, "") else { return Ok(None) };
        if target.origin() != base.origin() {
            return Ok(None);
        }
        let headers = self.http.and_then(|h| h.headers.as_ref()).map(crate::route::normalize_headers).unwrap_or_default();
        let headers = auth::apply(self.route.auth.as_ref(), AuthInput { method: "GET", url, body: "", headers }).await?;
        Ok(Some(headers))
    }
}

/// `Video.status`: one status read for a job ID.
pub async fn status(client: &reqwest::Client, model: &VideoModel, id: &str) -> Result<VideoStatus, AIError> {
    let http = exchange(model);
    let protocol = &model.route.protocol;
    let response = get(client, &model.route, &protocol.status_path(id), http.as_ref()).await?;
    protocol.decode_status(&response, id)
}

/// `Video.result`: the finished videos; fails while the job is still running, or with the provider's reason when
/// it failed.
pub async fn result(client: &reqwest::Client, model: &VideoModel, id: &str) -> Result<VideoResponse, AIError> {
    let http = exchange(model);
    let protocol = &model.route.protocol;
    let response = get(client, &model.route, &protocol.result_path(id), http.as_ref()).await?;
    let context = PollContext { token: id, client, route: &model.route, http: http.as_ref() };
    protocol.decode_result(&response, &context).await
}

/// `Video.cancel`: ask the provider to stop the job; fails as unsupported when the provider has no cancel call.
pub async fn cancel(client: &reqwest::Client, model: &VideoModel, id: &str) -> Result<(), AIError> {
    let Some((method, path)) = model.route.protocol.cancel(id) else {
        return Err(AIError::unsupported(
            "video.cancel",
            format!("{}/{} has no way to cancel a video generation", model.provider, model.route_id()),
            Some(&model.provider),
            Some(model.route_id()),
        ));
    };
    let http = exchange(model);
    call(client, &model.route, method, &path, http.as_ref()).await.map(|_| ())
}

/// The bytes of a finished video as a stream (`asset.stream()`): a `url` source streams from the network with its
/// download headers, without being buffered; inline sources yield their bytes.
pub async fn open(
    client: &reqwest::Client,
    output: &VideoOutput,
) -> Result<futures::stream::BoxStream<'static, Result<Bytes, AIError>>, AIError> {
    use futures::StreamExt;
    match &output.asset.source {
        MediaSource::Bytes { data, .. } => Ok(futures::stream::once(futures::future::ready(Ok(Bytes::from(data.clone())))).boxed()),
        MediaSource::Base64 { data, .. } => {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|_| AIError::invalid_request("Media asset contains invalid base64 data"))?;
            Ok(futures::stream::once(futures::future::ready(Ok(Bytes::from(bytes)))).boxed())
        }
        MediaSource::Ref { provider, id, .. } => Err(AIError::invalid_request(format!("Cannot materialize provider ref {provider}:{id}"))),
        MediaSource::Url { url, .. } => {
            let mut builder = client.get(url);
            for (k, v) in output.headers.iter().flatten() {
                builder = builder.header(k, v);
            }
            let response = builder.send().await.map_err(|e| {
                let mut error = crate::route::transport::send_error(&e, url);
                error.cause = None;
                error
            })?;
            let status = response.status().as_u16();
            let final_url = response.url().to_string();
            let mut headers = IndexMap::new();
            for (name, value) in response.headers() {
                headers.insert(name.as_str().to_lowercase(), String::from_utf8_lossy(value.as_bytes()).into_owned());
            }
            if status >= 400 {
                let text = response.text().await.ok();
                let message = crate::error::http_status_message(status, text.as_deref());
                return Err(crate::error::http_failure(message, Some(&final_url), Some(status), &headers, text));
            }
            let http = HttpContext { url: final_url.clone(), status, headers };
            Ok(response
                .bytes_stream()
                .map(move |chunk| {
                    chunk.map_err(|e| {
                        AIError::transport(
                            crate::error::TransportOperation::Read,
                            format!("Connection lost while reading the response: {e}"),
                            Some("TransportError"),
                            Some(&http.url),
                        )
                        .with_http(Some(http.clone()))
                    })
                })
                .boxed())
        }
    }
}

/// `output.pending(id)`: a result read before the generation finished, which is caller misuse.
pub fn pending(identity: &Identity, response: &MediaResponse, id: &str) -> AIError {
    AIError::new(
        ErrorKind::InvalidRequest { parameter: None, classification: None },
        format!("{} generation {id} has not finished; await it before reading the result", identity.name),
    )
    .with_body(response.text())
    .with_http(Some(response.http.clone()))
}

/// `output.ended(reason, message, failure)`: a generation that ended in `error` without output. `failed` carries the
/// provider's classification; `cancelled` and `expired` mean the result will never exist.
pub fn ended(identity: &Identity, response: &MediaResponse, reason: Reason, message: impl Into<String>, failure: Failure) -> AIError {
    match reason {
        Reason::Failed => identity.failed(response, message, failure),
        _ => AIError::invalid_request(message).with_body(response.text()).with_http(Some(response.http.clone())),
    }
}

/// `optionalNull` string fields: absent and `null` are both unset.
pub(crate) fn opt_str(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(String::from)
}

pub(crate) fn opt_num(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

/// `encodeURIComponent`.
pub(crate) fn encode_component(value: &str) -> String {
    crate::protocols::bedrock_converse::sigv4::encode_uri_component(value)
}

/// Decode a JSON document, checking the fields a protocol's schema requires.
pub(crate) fn decode_doc(identity: &Identity, response: &MediaResponse, check: impl Fn(&Value) -> bool) -> Result<Value, AIError> {
    let value: Value = identity.decode_json(response)?;
    if !value.is_object() || !check(&value) {
        return Err(identity.invalid(response, format!("{} returned an invalid response", identity.name)));
    }
    Ok(value)
}

/// A field that must be absent, `null` or of the given type (`optionalNull(schema)`).
pub(crate) fn null_or(value: &Value, key: &str, is: impl Fn(&Value) -> bool) -> bool {
    match value.get(key) {
        None | Some(Value::Null) => true,
        Some(v) => is(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_keep_unknown_values_running() {
        let table = [("done", State::Completed), ("failed", State::Ended(Reason::Failed))];
        assert_eq!(state(&table, "toString"), State::Running);
        assert_eq!(status_of(&table, "failed").reason, Some(Reason::Failed));
        assert_eq!(status_of(&table, "pending").status, Status::Running);
    }

    #[test]
    fn durations_are_seconds() {
        assert_eq!(parse_duration("8 seconds"), Some(8.0));
        assert_eq!(parse_duration("1 second"), Some(1.0));
        assert_eq!(parse_duration("2.5 seconds"), Some(2.5));
        assert_eq!(parse_duration("8s"), None);
        assert_eq!(parse_duration("8 minutes"), None);
    }

    #[test]
    fn retry_delays_grow_and_cap() {
        let error = AIError::provider_internal("x");
        assert!(read_retry_delay(0, &error) >= Duration::from_millis(800) && read_retry_delay(0, &error) <= Duration::from_millis(1200));
        assert!(read_retry_delay(7, &error) <= READ_RETRY_MAX_DELAY);
        let limited = AIError::new(ErrorKind::RateLimit { retry_after_ms: Some(5000.0), rate_limit: None }, "slow");
        assert!(read_retry_delay(0, &limited) >= Duration::from_secs(5));
        let long = AIError::new(ErrorKind::RateLimit { retry_after_ms: Some(120_000.0), rate_limit: None }, "slow");
        assert_eq!(read_retry_delay(0, &long), READ_RETRY_MAX_DELAY);
    }
}
