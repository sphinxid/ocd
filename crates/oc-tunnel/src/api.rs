//! The OpenTunnel HTTP API (`@opentunnel/protocol/api`), as `HttpApiClient` calls it.
//!
//! | endpoint | request | success |
//! |---|---|---|
//! | `POST /api/tunnel` | `{}` | 201 `{tunnel: {id, hostname, state, certificateID?}, token}` |
//! | `POST /api/tunnel/:id/certificate` | `{csr}`, bearer token | 202 `{id, state}` |
//! | `GET /api/tunnel/:id/certificate` | bearer token | 200 `{id, state}` |
//! | `DELETE /api/tunnel/:id` | bearer token | 204 |
//! | `GET /api/tunnel/:id/connect` | WebSocket upgrade, subprotocol `opentunnel` | 101 |
//!
//! Errors are `{"_tag": "...Error", "message": ...}` bodies with the status of the tag (400, 401, 404, 409, 503).
//! A certificate `state` is one of `{type: "challenge", token, key}`, `{type: "issuing"}`,
//! `{type: "ready", certificate, chain, expiry}` or `{type: "failed", reason}`.
//!
//! Calls are sent again after transient failures as `withRetries` does (`crate::retry`), and go through the proxy
//! `fetch` would use (`crate::proxy`).

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use crate::proxy::Transport;
use crate::retry::{self, Repeat};

/// The public OpenTunnel service.
pub const DEFAULT_API: &str = "https://opentunnel.xyz";

/// How the SDK identifies itself to the server, on API calls and the bridge.
pub const USER_AGENT: &str = "opentunnel-sdk/0.6.1";

/// The user agent of a plain `fetch` in upstream's runtime (Bun 1.4.4).
pub const BUN_USER_AGENT: &str = "Bun/1.4.4";

/// A failed API call. `tag` and `message` are the API error the body decoded to (one the endpoint declares for that
/// status); without one it is an `HttpClientError`: `status` is the HTTP status of the response (`message`
/// `Decode error (<status> <method> <url>)`, or `StatusCode error (...)` for a declared status whose body did not
/// decode), and without a response `message` is the transport error (`Transport error (<method> <url>)`).
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: Option<u16>,
    pub tag: Option<String>,
    pub message: String,
}

impl ApiError {
    /// `describeApiError`: the API's error, the HTTP status (and what Cloudflare means by it), or the network
    /// failure.
    pub fn describe(&self) -> String {
        match (&self.tag, self.status) {
            (Some(tag), _) => format!("{tag}: {}", self.message),
            (None, Some(status)) => {
                let message = retry::status_message(status, "");
                if message.starts_with("HTTP ") { message } else { format!("{message} (HTTP {status})") }
            }
            (None, None) => self.message.clone(),
        }
    }

    /// `isTransient`: the API unreachable, rate limited, or failing on the server side (an HTTP client error, not an
    /// error the API answered with).
    pub fn is_transient(&self) -> bool {
        self.tag.is_none() && self.status.is_none_or(|status| retry::retryable_status(status, Repeat::Idempotent))
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.describe())
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum CertificateState {
    Challenge { token: String, key: String },
    Issuing,
    Ready { certificate: String, chain: String, expiry: String },
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CertificateInfo {
    pub id: String,
    pub state: CertificateState,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TunnelInfo {
    pub id: String,
    pub hostname: String,
    pub state: String,
    #[serde(rename = "certificateID", default)]
    pub certificate_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Created {
    pub tunnel: TunnelInfo,
    pub token: String,
}

/// `encodeURIComponent`.
pub(crate) fn encode_component(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// An API error schema: its `_tag`, status and the fields it requires besides `message`.
type ErrorSchema = (&'static str, u16, &'static [&'static str]);

const INVALID_REQUEST: ErrorSchema = ("InvalidRequestError", 400, &[]);
const UNAUTHORIZED: ErrorSchema = ("UnauthorizedError", 401, &[]);
const TUNNEL_NOT_FOUND: ErrorSchema = ("TunnelNotFoundError", 404, &["tunnelID"]);
const CERTIFICATE_NOT_FOUND: ErrorSchema = ("CertificateNotFoundError", 404, &["tunnelID"]);
const INVALID_HOSTNAME: ErrorSchema = ("InvalidHostnameError", 400, &["provided", "expected"]);
const HOSTNAME_UNAVAILABLE: ErrorSchema = ("HostnameUnavailableError", 409, &["name"]);
const CERTIFICATE_IN_PROGRESS: ErrorSchema = ("CertificateInProgressError", 409, &["tunnelID"]);
const SERVICE_UNAVAILABLE: ErrorSchema = ("ServiceUnavailableError", 503, &[]);

/// The errors each endpoint declares (the authorized ones also get the middleware's `UnauthorizedError`).
const CREATE_ERRORS: &[ErrorSchema] = &[INVALID_REQUEST, HOSTNAME_UNAVAILABLE, SERVICE_UNAVAILABLE];
const BIND_ERRORS: &[ErrorSchema] =
    &[INVALID_REQUEST, INVALID_HOSTNAME, TUNNEL_NOT_FOUND, CERTIFICATE_IN_PROGRESS, SERVICE_UNAVAILABLE, UNAUTHORIZED];
const GET_CERTIFICATE_ERRORS: &[ErrorSchema] =
    &[TUNNEL_NOT_FOUND, CERTIFICATE_NOT_FOUND, SERVICE_UNAVAILABLE, UNAUTHORIZED];
const REMOVE_ERRORS: &[ErrorSchema] = &[TUNNEL_NOT_FOUND, SERVICE_UNAVAILABLE, UNAUTHORIZED];

/// The declared error an error response decodes to, as `(tag, message)`.
fn declared_error(errors: &[ErrorSchema], status: u16, body: &Value) -> Option<(String, String)> {
    let tag = body.get("_tag")?.as_str()?;
    let (_, _, fields) = errors.iter().find(|(t, s, _)| *t == tag && *s == status)?;
    let message = body.get("message")?.as_str()?;
    fields.iter().all(|f| body.get(*f).is_some_and(Value::is_string)).then(|| (tag.to_string(), message.to_string()))
}

#[derive(Clone)]
pub struct Api {
    base: Url,
    http: reqwest::Client,
    transport: Transport,
    /// Scales the retry delays (1 outside tests).
    retry_scale: f64,
}

/// What `fetch` reports for a request that could not be sent (`Transport error (<method> <url>)`).
fn transport_error(method: &reqwest::Method, url: &Url) -> ApiError {
    ApiError { status: None, tag: None, message: format!("Transport error ({method} {url})") }
}

impl Api {
    /// Through the proxies the process environment selects.
    pub fn new(base: Url) -> Api {
        Api::with_transport(base, Transport::from_env(crate::proxy::process_env()))
    }

    pub fn with_transport(base: Url, transport: Transport) -> Api {
        let proxies = transport.clone();
        let mut proxy_headers = reqwest::header::HeaderMap::new();
        proxy_headers.insert("proxy-connection", reqwest::header::HeaderValue::from_static("Keep-Alive"));
        // Like Bun's `fetch`, which also sends `Proxy-Connection: Keep-Alive` to a proxy. `call` refuses a request
        // whose proxy is unusable before reqwest gets to pick it.
        let proxy = reqwest::Proxy::custom(move |url| proxies.fetch_proxy(url).ok().flatten()).headers(proxy_headers);
        let http = oc_util::http::client_builder().no_proxy().proxy(proxy).build().expect("reqwest client");
        Api { base, http, transport, retry_scale: 1.0 }
    }

    /// Test hook: scales the retry delays.
    pub fn set_retry_scale(&mut self, scale: f64) {
        self.retry_scale = scale;
    }

    pub fn base(&self) -> &Url {
        &self.base
    }

    pub fn transport(&self) -> &Transport {
        &self.transport
    }

    fn url(&self, path: &str) -> Url {
        self.base.join(path).unwrap_or_else(|_| self.base.clone())
    }

    fn tunnel_path(id: &str, rest: &str) -> String {
        format!("/api/tunnel/{}{rest}", encode_component(id))
    }

    async fn wait(&self, attempt: u32, retry_after: Option<&str>) {
        let (min, max) = retry::retry_delay_ms(attempt, retry_after);
        let delay = min + rand::random::<f64>() * (max - min);
        tokio::time::sleep(Duration::from_secs_f64(delay * self.retry_scale / 1000.0)).await;
    }

    /// Sends a request through `fetch`'s proxy, retrying as `withRetries` does, and decodes the answer.
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
        errors: &[ErrorSchema],
    ) -> Result<Value, ApiError> {
        let url = self.url(path);
        let repeat = if method == reqwest::Method::GET
            || (method == reqwest::Method::POST && url.as_str().ends_with("/certificate"))
        {
            Repeat::Idempotent
        } else {
            Repeat::Once
        };
        let mut attempt = 0;
        let response = loop {
            // Bun's `fetch` fails a request whose proxy it cannot use (`UnsupportedProxyProtocol`), a network error
            // that is not a connection failure.
            let sent = match self.transport.fetch_proxy(&url) {
                Err(_) => Err(false),
                Ok(_) => {
                    // The SDK's calls go through Effect's HTTP client, which adds the trace headers of a client span.
                    let mut request = self.http.request(method.clone(), url.clone()).header("user-agent", USER_AGENT);
                    if let Some(token) = token {
                        request = request.bearer_auth(token);
                    }
                    if let Some(body) = &body {
                        request = request.json(body);
                    }
                    for (name, value) in oc_util::trace::headers() {
                        request = request.header(name, value);
                    }
                    request.send().await.map_err(|e| e.is_connect())
                }
            };
            match sent {
                Err(connect) => {
                    // A request that never left may be sent again; one that may have reached the server only when
                    // repeating it is harmless.
                    if attempt < retry::RETRIES && (repeat == Repeat::Idempotent || connect) {
                        self.wait(attempt, None).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(transport_error(&method, &url));
                }
                Ok(response) => {
                    let status = response.status().as_u16();
                    if attempt < retry::RETRIES && retry::retryable_status(status, repeat) {
                        let retry_after =
                            response.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(String::from);
                        self.wait(attempt, retry_after.as_deref()).await;
                        attempt += 1;
                        continue;
                    }
                    break response;
                }
            }
        };
        let status = response.status();
        let code = status.as_u16();
        let text = response.text().await.map_err(|_| transport_error(&method, &url))?;
        if status.is_success() {
            if text.is_empty() {
                return Ok(Value::Null);
            }
            // A body that is not JSON fails to decode as an HTTP client error with the response.
            return serde_json::from_str(&text).map_err(|_| ApiError {
                status: Some(code),
                tag: None,
                message: format!("Decode error ({code} {method} {url})"),
            });
        }
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        match declared_error(errors, code, &body) {
            Some((tag, message)) => Err(ApiError { status: Some(code), tag: Some(tag), message }),
            // `HttpClientError`: a `StatusCodeError` for a declared status whose body did not decode, else a
            // `DecodeError`.
            None => {
                let reason = if errors.iter().any(|(_, s, _)| *s == code) { "StatusCode" } else { "Decode" };
                Err(ApiError {
                    status: Some(code),
                    tag: None,
                    message: format!("{reason} error ({code} {method} {url})"),
                })
            }
        }
    }

    fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ApiError> {
        serde_json::from_value(value).map_err(|e| ApiError {
            status: None,
            tag: Some("SchemaError".into()),
            message: format!("Invalid response body: {e}"),
        })
    }

    /// `tunnel.create`
    pub async fn create(&self) -> Result<Created, ApiError> {
        let value = self.call(reqwest::Method::POST, "/api/tunnel", None, Some(json!({})), CREATE_ERRORS).await?;
        Self::decode(value)
    }

    /// `tunnel.bindCertificate`
    pub async fn bind_certificate(&self, token: &str, id: &str, csr: &str) -> Result<CertificateInfo, ApiError> {
        let path = Self::tunnel_path(id, "/certificate");
        let value =
            self.call(reqwest::Method::POST, &path, Some(token), Some(json!({ "csr": csr })), BIND_ERRORS).await?;
        Self::decode(value)
    }

    /// `tunnel.getCertificate`
    pub async fn get_certificate(&self, token: &str, id: &str) -> Result<CertificateInfo, ApiError> {
        let path = Self::tunnel_path(id, "/certificate");
        let value = self.call(reqwest::Method::GET, &path, Some(token), None, GET_CERTIFICATE_ERRORS).await?;
        Self::decode(value)
    }

    /// `tunnel.remove`
    pub async fn remove(&self, token: &str, id: &str) -> Result<(), ApiError> {
        let path = Self::tunnel_path(id, "");
        self.call(reqwest::Method::DELETE, &path, Some(token), None, REMOVE_ERRORS).await.map(|_| ())
    }

    /// The running tunnel's certificate check (a plain `fetch` with only the bearer token, so Bun's default user
    /// agent, no trace headers and no retries): the certificate state, or `None` on any failure.
    pub async fn current_certificate(&self, token: &str, id: &str) -> Option<Value> {
        let url = self.url(&Self::tunnel_path(id, "/certificate"));
        self.transport.fetch_proxy(&url).ok()?;
        let response = self
            .http
            .get(url)
            .header("authorization", format!("Bearer {token}"))
            .header("user-agent", BUN_USER_AGENT)
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: Value = response.json().await.ok()?;
        body.get("state").cloned()
    }

    /// `/api/tunnel/:id/connect` as a WebSocket URL.
    pub fn connect_url(&self, id: &str) -> Url {
        let mut url = self.url(&Self::tunnel_path(id, "/connect"));
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        let _ = url.set_scheme(scheme);
        url
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let api = Api::new(Url::parse("https://opentunnel.xyz").unwrap());
        assert_eq!(api.connect_url("tun 1").as_str(), "wss://opentunnel.xyz/api/tunnel/tun%201/connect");
        let api = Api::new(Url::parse("http://127.0.0.1:9/").unwrap());
        assert_eq!(api.connect_url("t").as_str(), "ws://127.0.0.1:9/api/tunnel/t/connect");
        assert_eq!(encode_component("a/b~c"), "a%2Fb~c");
    }

    #[test]
    fn certificate_states() {
        let ready: CertificateInfo = serde_json::from_value(
            json!({"id": "c", "state": {"type": "ready", "certificate": "C", "chain": "", "expiry": "2027-01-01T00:00:00Z"}}),
        )
        .unwrap();
        assert!(matches!(ready.state, CertificateState::Ready { .. }));
        let issuing: CertificateInfo =
            serde_json::from_value(json!({"id": "c", "state": {"type": "issuing"}})).unwrap();
        assert_eq!(issuing.state, CertificateState::Issuing);
    }

    #[test]
    fn errors_are_described_like_the_sdk() {
        let tagged =
            ApiError { status: Some(503), tag: Some("ServiceUnavailableError".into()), message: "Down".into() };
        assert_eq!(tagged.describe(), "ServiceUnavailableError: Down");
        assert!(!tagged.is_transient());
        let status = ApiError { status: Some(500), tag: None, message: String::new() };
        assert_eq!(status.describe(), "HTTP 500");
        assert!(status.is_transient());
        let cloudflare = ApiError { status: Some(522), tag: None, message: String::new() };
        assert_eq!(cloudflare.describe(), "Cloudflare 522: the connection to the server timed out (HTTP 522)");
        let conflict = ApiError { status: Some(409), tag: None, message: String::new() };
        assert!(!conflict.is_transient());
        let transport = transport_error(&reqwest::Method::GET, &Url::parse("http://127.0.0.1:1/api/tunnel/t").unwrap());
        assert_eq!(transport.describe(), "Transport error (GET http://127.0.0.1:1/api/tunnel/t)");
        assert!(transport.is_transient());
    }

    #[test]
    fn only_declared_errors_decode() {
        let body = json!({"_tag": "CertificateInProgressError", "tunnelID": "t", "message": "busy"});
        assert_eq!(declared_error(BIND_ERRORS, 409, &body), Some(("CertificateInProgressError".into(), "busy".into())));
        // Wrong status, undeclared tag, or a missing field: no API error.
        assert_eq!(declared_error(BIND_ERRORS, 400, &body), None);
        assert_eq!(declared_error(CREATE_ERRORS, 409, &body), None);
        let body = json!({"_tag": "CertificateInProgressError", "message": "busy"});
        assert_eq!(declared_error(BIND_ERRORS, 409, &body), None);
        let body = json!({"_tag": "UnauthorizedError", "message": "no"});
        assert!(declared_error(REMOVE_ERRORS, 401, &body).is_some());
        assert!(declared_error(CREATE_ERRORS, 401, &body).is_none());
    }
}
