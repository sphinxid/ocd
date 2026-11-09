//! Media routes (`packages/ai/src/route/media.ts`, `route/media-protocol.ts`): the transport plumbing every
//! image and decision protocol shares. A route composes a protocol with an endpoint, auth and deployment
//! headers; it merges the HTTP and provider options, renders the URL and query, encodes JSON, multipart or
//! binary bodies, applies auth and hands the response back to the protocol.
//!
//! Only the inline `generate` path is ported: it is the one core's `Generate.image` / `Generate.decisions`
//! call. Streaming (`Image.stream`) and queued generations (`start` / `resume`) have no caller in ocd.

use super::auth::{self, Auth, AuthInput};
use super::Headers;
use crate::error::{http_failure, http_status_message, AIError, ErrorKind, HttpContext, TransportOperation};
use crate::json::{encode, Object};
use crate::types::HttpOptions;
use bytes::Bytes;
use indexmap::IndexMap;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::sync::Arc;

/// `MediaProtocol.Query`: array values become repeated parameters (`keyterm=a&keyterm=b`).
pub type Query = IndexMap<String, Vec<String>>;

/// One multipart field (`FormData.append`).
#[derive(Debug, Clone, PartialEq)]
pub enum FormPart {
    Text { name: String, value: String },
    File { name: String, data: Vec<u8>, content_type: String, filename: String },
}

/// A `FormData` body, encoded the way Bun's `fetch` sends it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FormData {
    pub parts: Vec<FormPart>,
}

impl FormData {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn append(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.parts.push(FormPart::Text { name: name.into(), value: value.into() });
    }
    pub fn append_file(&mut self, name: impl Into<String>, data: Vec<u8>, content_type: impl Into<String>, filename: impl Into<String>) {
        self.parts.push(FormPart::File { name: name.into(), data, content_type: content_type.into(), filename: filename.into() });
    }
    /// Text fields by name, in order (tests).
    pub fn text(&self, name: &str) -> Vec<&str> {
        self.parts
            .iter()
            .filter_map(|p| match p {
                FormPart::Text { name: n, value } if n == name => Some(value.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Bun's multipart encoding: a `----WebKitFormBoundary<32 hex>` boundary, `Content-Disposition` per part,
    /// `Content-Type` for files, CRLF line endings.
    pub fn encode(&self, boundary: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for part in &self.parts {
            out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match part {
                FormPart::Text { name, value } => {
                    out.extend_from_slice(format!("Content-Disposition: form-data; name=\"{}\"\r\n\r\n", escape(name)).as_bytes());
                    out.extend_from_slice(value.as_bytes());
                }
                FormPart::File { name, data, content_type, filename } => {
                    out.extend_from_slice(
                        format!(
                            "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: {content_type}\r\n\r\n",
                            escape(name),
                            escape(filename)
                        )
                        .as_bytes(),
                    );
                    out.extend_from_slice(data);
                }
            }
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        out
    }
}

fn escape(value: &str) -> String {
    value.replace('"', "%22").replace('\r', "%0D").replace('\n', "%0A")
}

fn boundary() -> String {
    let bytes: [u8; 16] = rand::random();
    format!("----WebKitFormBoundary{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// `MediaProtocol.Body`.
#[derive(Debug, Clone, PartialEq)]
pub enum MediaBody {
    Json { value: Object, query: Option<Query> },
    Multipart(FormData),
    Binary { value: Vec<u8>, content_type: String, query: Option<Query> },
}

impl MediaBody {
    pub fn json(value: Object) -> Self {
        Self::Json { value, query: None }
    }
    /// The JSON body (tests and protocols that read back what they sent).
    pub fn json_value(&self) -> Option<&Object> {
        match self {
            Self::Json { value, .. } => Some(value),
            _ => None,
        }
    }
    fn query(&self) -> Option<&Query> {
        match self {
            Self::Json { query, .. } | Self::Binary { query, .. } => query.as_ref(),
            Self::Multipart(_) => None,
        }
    }
}

/// A route path: fixed, or computed from the request (the model id in Gemini's or Cloudflare's URL).
pub enum MediaPath<R> {
    Static(String),
    Dynamic(Arc<dyn Fn(&R) -> String + Send + Sync>),
}

impl<R> Clone for MediaPath<R> {
    fn clone(&self) -> Self {
        match self {
            Self::Static(s) => Self::Static(s.clone()),
            Self::Dynamic(f) => Self::Dynamic(f.clone()),
        }
    }
}

/// `Endpoint.Definition` for a media request type.
pub struct MediaEndpoint<R> {
    pub base_url: Option<String>,
    pub path: MediaPath<R>,
    pub query: IndexMap<String, String>,
}

impl<R> Clone for MediaEndpoint<R> {
    fn clone(&self) -> Self {
        Self { base_url: self.base_url.clone(), path: self.path.clone(), query: self.query.clone() }
    }
}

impl<R> MediaEndpoint<R> {
    pub fn new(base_url: Option<&str>, path: &str) -> Self {
        Self { base_url: base_url.map(String::from), path: MediaPath::Static(path.into()), query: IndexMap::new() }
    }
    pub fn dynamic(base_url: Option<&str>, path: impl Fn(&R) -> String + Send + Sync + 'static) -> Self {
        Self { base_url: base_url.map(String::from), path: MediaPath::Dynamic(Arc::new(path)), query: IndexMap::new() }
    }
    /// `Endpoint.render`.
    pub fn render(&self, request: &R) -> Result<url::Url, AIError> {
        let base = self.base_url.as_deref().unwrap_or("").trim_end_matches('/');
        let path = match &self.path {
            MediaPath::Static(p) => p.clone(),
            MediaPath::Dynamic(f) => f(request),
        };
        let mut url = url::Url::parse(&format!("{base}{path}")).map_err(|_| AIError::invalid_request(format!("Invalid URL: {base}{path}")))?;
        super::set_query(&mut url, &self.query);
        Ok(url)
    }
}

/// Deployment inputs every media model factory accepts (`MediaRoute.ModelInput` minus the id).
#[derive(Clone)]
pub struct Deployment {
    pub auth: Arc<dyn Auth>,
    pub base_url: Option<String>,
    pub headers: Option<IndexMap<String, String>>,
    pub http: Option<HttpOptions>,
}

impl std::fmt::Debug for Deployment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deployment").field("base_url", &self.base_url).field("headers", &self.headers).finish()
    }
}

impl Deployment {
    /// `MediaRoute.deployment(input, auth)`: `http` carries only the settings' `body` overlay.
    pub fn new(auth: Arc<dyn Auth>, base_url: Option<String>, headers: Option<IndexMap<String, String>>, body: Option<Object>) -> Self {
        Self { auth, base_url, headers, http: body.map(|b| HttpOptions { body: Some(b), ..Default::default() }) }
    }
}

/// A read response: HTTP context and the whole body.
#[derive(Debug, Clone)]
pub struct MediaResponse {
    pub http: HttpContext,
    pub body: Bytes,
}

impl MediaResponse {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.http.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }
}

/// The failure classes a provider can report for a failed generation (`MediaProtocol.Failure`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    InvalidRequest,
    Authentication,
    RateLimit,
    ProviderInternal,
}

/// One protocol's route id, display name and provider, with the decoders and errors that carry them
/// (`MediaProtocol.identity`).
#[derive(Debug, Clone)]
pub struct Identity {
    pub id: String,
    pub name: String,
    pub provider: String,
}

impl Identity {
    pub fn new(id: &str, name: &str, provider: &str) -> Self {
        Self { id: id.into(), name: name.into(), provider: provider.into() }
    }

    /// `output.invalid`: a malformed provider document, keeping the body and HTTP context.
    pub fn invalid(&self, response: &MediaResponse, message: impl Into<String>) -> AIError {
        let mut e = AIError::invalid_output(&self.id, message);
        e.body = Some(response.text());
        e.http = Some(response.http.clone());
        e
    }

    /// `frameError`: an invalid document or frame, with the frame as the body and no HTTP context.
    pub fn frame_error(&self, message: impl Into<String>, body: Option<String>) -> AIError {
        let mut e = AIError::invalid_output(&self.id, message);
        e.body = body;
        e
    }

    /// `output.ended("failed", message, failure)`.
    pub fn failed(&self, response: &MediaResponse, message: impl Into<String>, failure: Failure) -> AIError {
        let kind = match failure {
            Failure::InvalidRequest => ErrorKind::InvalidRequest { parameter: None, classification: None },
            Failure::Authentication => ErrorKind::Authentication,
            Failure::RateLimit => ErrorKind::RateLimit { retry_after_ms: None, rate_limit: None },
            Failure::ProviderInternal => ErrorKind::ProviderInternal { retry_after_ms: None },
        };
        let mut e = AIError::new(kind, message);
        e.body = Some(response.text());
        e.http = Some(response.http.clone());
        e
    }

    /// `output.contentPolicy`.
    pub fn content_policy(&self, response: &MediaResponse, message: impl Into<String>) -> AIError {
        let mut e = AIError::new(ErrorKind::ContentPolicy, message);
        e.body = Some(response.text());
        e.http = Some(response.http.clone());
        e
    }

    /// A `QuotaExceeded` failure carrying the response.
    pub fn quota_exceeded(&self, response: &MediaResponse, message: impl Into<String>) -> AIError {
        let mut e = AIError::new(ErrorKind::QuotaExceeded { retry_after_ms: None }, message);
        e.body = Some(response.text());
        e.http = Some(response.http.clone());
        e
    }

    /// `route.unsupported(operation, message)`.
    pub fn unsupported(&self, operation: &str, message: impl Into<String>) -> AIError {
        AIError::unsupported(operation, message, Some(&self.provider), Some(&self.id))
    }

    /// `decodeJson(schema)`: parse the body; a failure keeps the raw body (`<name> returned an invalid response`).
    pub fn decode_json<T: DeserializeOwned>(&self, response: &MediaResponse) -> Result<T, AIError> {
        serde_json::from_slice::<T>(&response.body).map_err(|e| {
            let mut error = self.invalid(response, format!("{} returned an invalid response", self.name));
            error.cause = Some(e.to_string());
            error
        })
    }
}

/// `ProviderShared.invalidRequest`.
pub fn invalid_request(message: impl Into<String>) -> AIError {
    AIError::invalid_request(message)
}

/// `rejectUnsupported`: a present field the protocol declared unsupported fails typed. `present` lists the
/// request's set common fields in declaration order.
pub fn reject_unsupported(route: &str, provider: &str, present: &[&str]) -> Result<(), AIError> {
    if present.is_empty() {
        return Ok(());
    }
    Err(AIError::unsupported(
        &format!("media.{}", present[0]),
        format!("{provider}/{route} does not support {}", present.join(", ")),
        Some(provider),
        Some(route),
    ))
}

fn with_query(url: &mut url::Url, query: Option<&Query>) {
    let Some(query) = query else { return };
    if query.is_empty() {
        return;
    }
    let mut pairs: Vec<(String, String)> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    for (key, values) in query {
        pairs.retain(|(k, _)| k != key);
        for value in values {
            pairs.push((key.clone(), value.clone()));
        }
    }
    let encoded: String = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs.iter()).finish();
    url.set_query(Some(&encoded));
}

fn string_query(query: Option<&IndexMap<String, String>>) -> Option<Query> {
    query.map(|q| q.iter().map(|(k, v)| (k.clone(), vec![v.clone()])).collect())
}

/// A prepared media call, as the route hands it to the executor.
#[derive(Debug, Clone)]
pub struct MediaCall {
    pub method: String,
    pub url: String,
    pub headers: Headers,
    pub body: Option<(Vec<u8>, Option<String>)>,
}

/// `makeTransport(...).submit`: POST the protocol body to the rendered endpoint with the merged HTTP options.
pub async fn submit<R>(
    client: &reqwest::Client,
    endpoint: &MediaEndpoint<R>,
    auth: &dyn Auth,
    http: Option<&HttpOptions>,
    request: &R,
    body: &MediaBody,
) -> Result<MediaResponse, AIError> {
    let call = prepare(endpoint, auth, http, request, body).await?;
    execute(client, &call).await
}

/// URL, headers and auth for a submit (no I/O beyond the auth hook).
pub async fn prepare<R>(
    endpoint: &MediaEndpoint<R>,
    auth: &dyn Auth,
    http: Option<&HttpOptions>,
    request: &R,
    body: &MediaBody,
) -> Result<MediaCall, AIError> {
    let mut url = endpoint.render(request)?;
    with_query(&mut url, body.query());
    with_query(&mut url, string_query(http.and_then(|h| h.query.as_ref())).as_ref());
    let headers: Headers = http.and_then(|h| h.headers.as_ref()).map(super::normalize_headers).unwrap_or_default();
    let (text, headers, encoded) = match body {
        MediaBody::Json { value, .. } => {
            let text = encode(&Value::Object(value.clone()));
            (text.clone(), headers, Some((text.into_bytes(), Some("application/json".to_string()))))
        }
        MediaBody::Binary { value, content_type, .. } => {
            (format!("[{content_type}]"), headers, Some((value.clone(), Some(content_type.clone()))))
        }
        MediaBody::Multipart(form) => {
            // The HTTP client sets the multipart boundary; a caller-supplied content-type would corrupt it.
            let mut headers = headers;
            headers.shift_remove("content-type");
            let boundary = boundary();
            ("[multipart/form-data]".to_string(), headers, Some((form.encode(&boundary), Some(format!("multipart/form-data; boundary={boundary}")))))
        }
    };
    let url = url.to_string();
    let mut headers = auth::apply(auth, AuthInput { method: "POST", url: &url, body: &text, headers }).await?;
    if let Some((_, Some(content_type))) = &encoded {
        headers.insert("content-type".into(), content_type.clone());
    }
    Ok(MediaCall { method: "POST".into(), url, headers, body: encoded.map(|(b, _)| (b, None)) })
}

fn reqwest_message(error: &reqwest::Error) -> String {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(s) = source {
        let text = s.to_string();
        if !message.contains(&text) {
            message = format!("{message}: {text}");
        }
        source = s.source();
    }
    message
}

/// `RequestExecutor.execute`: send, read the whole body, and classify a status of 400 or more.
pub async fn execute(client: &reqwest::Client, call: &MediaCall) -> Result<MediaResponse, AIError> {
    let method = reqwest::Method::from_bytes(call.method.as_bytes()).unwrap_or(reqwest::Method::POST);
    let mut builder = client.request(method, &call.url);
    for (k, v) in &call.headers {
        builder = builder.header(k, v);
    }
    for (k, v) in oc_util::trace::headers() {
        builder = builder.header(k, v);
    }
    if let Some((body, _)) = &call.body {
        builder = builder.body(body.clone());
    }
    let response = builder.send().await.map_err(|e| {
        let code = if e.is_timeout() {
            Some("Timeout")
        } else if e.is_connect() {
            Some("ConnectionError")
        } else {
            Some("TransportError")
        };
        AIError::transport(TransportOperation::Request, reqwest_message(&e), code, Some(&call.url))
    })?;
    let status = response.status().as_u16();
    let url = response.url().to_string();
    let mut headers = IndexMap::new();
    for (name, value) in response.headers() {
        headers.insert(name.as_str().to_lowercase(), String::from_utf8_lossy(value.as_bytes()).into_owned());
    }
    if status >= 400 {
        let text = response.text().await.ok();
        let message = http_status_message(status, text.as_deref());
        return Err(http_failure(message, Some(&url), Some(status), &headers, text));
    }
    let http = HttpContext { url: url.clone(), status, headers };
    let body = response.bytes().await.map_err(|e| {
        AIError::transport(
            TransportOperation::Read,
            format!("Connection lost while reading the response: {}", reqwest_message(&e)),
            Some("TransportError"),
            Some(&url),
        )
        .with_http(Some(http.clone()))
    })?;
    Ok(MediaResponse { http, body })
}

/// `Media.download`: GET a `url` asset through the executor.
pub async fn download(client: &reqwest::Client, url: &str, headers: Option<&IndexMap<String, String>>) -> Result<Vec<u8>, AIError> {
    let call = MediaCall {
        method: "GET".into(),
        url: url.to_string(),
        headers: headers.map(super::normalize_headers).unwrap_or_default(),
        body: None,
    };
    let response = execute(client, &call).await?;
    Ok(response.body.to_vec())
}

/// `ProviderShared.mediaReference`: the one string a provider can address an asset by.
#[derive(Debug, Clone, PartialEq)]
pub enum MediaReference {
    DataUrl(String),
    Url(String),
    Ref(String),
}

impl MediaReference {
    pub fn value(&self) -> &str {
        match self {
            Self::DataUrl(v) | Self::Url(v) | Self::Ref(v) => v,
        }
    }
}

/// `ProviderShared.mediaReference(asset, provider, label)`.
pub fn media_reference(asset: &crate::media::MediaAsset, provider: Option<&str>, label: &str) -> Result<MediaReference, AIError> {
    use crate::media::MediaSource;
    if let Some(inline) = asset.inline() {
        return Ok(MediaReference::DataUrl(inline.data_url));
    }
    if let Some(url) = asset.url_value() {
        return Ok(MediaReference::Url(url.to_string()));
    }
    if let (Some(provider), MediaSource::Ref { provider: owner, id, .. }) = (provider, &asset.source)
        && owner == provider
    {
        return Ok(MediaReference::Ref(id.clone()));
    }
    let accepted = provider.map(|p| format!(", and {p} references")).unwrap_or_default();
    let got = match &asset.source {
        MediaSource::Ref { provider, id, .. } => format!("; got {provider}:{id}"),
        _ => String::new(),
    };
    Err(invalid_request(format!("{label} accepts inline bytes, data URLs, http(s) URLs{accepted}{got}")))
}

/// `ProviderShared.inlineRequired`.
pub fn inline_required(route: &str, asset: &crate::media::MediaAsset) -> AIError {
    invalid_request(format!(
        "{route} requires inline media (bytes or base64); {} sources must be materialized first",
        asset.source_type()
    ))
}

/// `ProviderShared.requireInlineMedia`.
pub fn require_inline(route: &str, asset: &crate::media::MediaAsset) -> Result<crate::media::Inline, AIError> {
    asset.inline().ok_or_else(|| inline_required(route, asset))
}

/// `ProviderShared.totalTokens`.
pub fn total_tokens(input: Option<f64>, output: Option<f64>, total: Option<f64>) -> Option<f64> {
    if total.is_some() {
        return total;
    }
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(input.unwrap_or(0.0) + output.unwrap_or(0.0))
}
