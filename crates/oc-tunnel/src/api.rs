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

use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

/// The public OpenTunnel service.
pub const DEFAULT_API: &str = "https://opentunnel.xyz";

/// How the SDK identifies itself to the server, on API calls and the bridge.
pub const USER_AGENT: &str = "opentunnel-sdk/0.4.1";

/// The user agent of a plain `fetch` in upstream's runtime (Bun 1.4.4).
pub const BUN_USER_AGENT: &str = "Bun/1.4.4";

/// A failed API call: the HTTP status and the error body's `_tag` and `message` when the server answered.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: Option<u16>,
    pub tag: Option<String>,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.tag, self.status) {
            (Some(tag), _) => write!(f, "{tag}: {}", self.message),
            (None, Some(status)) => write!(f, "HTTP {status}: {}", self.message),
            (None, None) => f.write_str(&self.message),
        }
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

#[derive(Clone)]
pub struct Api {
    base: Url,
    http: reqwest::Client,
}

impl Api {
    pub fn new(base: Url) -> Api {
        let http = oc_util::http::client_builder().build().expect("reqwest client");
        Api { base, http }
    }

    pub fn base(&self) -> &Url {
        &self.base
    }

    fn url(&self, path: &str) -> Url {
        self.base.join(path).unwrap_or_else(|_| self.base.clone())
    }

    fn tunnel_path(id: &str, rest: &str) -> String {
        format!("/api/tunnel/{}{rest}", encode_component(id))
    }

    async fn call(&self, request: reqwest::RequestBuilder) -> Result<Value, ApiError> {
        // The SDK's calls go through Effect's HTTP client, which adds the trace headers of a client span.
        let mut request = request.header("user-agent", USER_AGENT);
        for (name, value) in oc_util::trace::headers() {
            request = request.header(name, value);
        }
        let response =
            request.send().await.map_err(|e| ApiError { status: None, tag: None, message: oc_util_error(&e) })?;
        let status = response.status();
        let text = response.text().await.map_err(|e| ApiError {
            status: Some(status.as_u16()),
            tag: None,
            message: oc_util_error(&e),
        })?;
        if status.is_success() {
            if text.is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text).map_err(|e| ApiError {
                status: Some(status.as_u16()),
                tag: None,
                message: format!("Invalid response body: {e}"),
            });
        }
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        Err(ApiError {
            status: Some(status.as_u16()),
            tag: body.get("_tag").and_then(Value::as_str).map(String::from),
            message: body
                .get("message")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| status.canonical_reason().unwrap_or("").to_string()),
        })
    }

    fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ApiError> {
        serde_json::from_value(value).map_err(|e| ApiError {
            status: None,
            tag: None,
            message: format!("Invalid response body: {e}"),
        })
    }

    /// `tunnel.create`
    pub async fn create(&self) -> Result<Created, ApiError> {
        let request = self.http.post(self.url("/api/tunnel")).json(&json!({}));
        Self::decode(self.call(request).await?)
    }

    /// `tunnel.bindCertificate`
    pub async fn bind_certificate(&self, token: &str, id: &str, csr: &str) -> Result<CertificateInfo, ApiError> {
        let request = self
            .http
            .post(self.url(&Self::tunnel_path(id, "/certificate")))
            .bearer_auth(token)
            .json(&json!({ "csr": csr }));
        Self::decode(self.call(request).await?)
    }

    /// `tunnel.getCertificate`
    pub async fn get_certificate(&self, token: &str, id: &str) -> Result<CertificateInfo, ApiError> {
        let request = self.http.get(self.url(&Self::tunnel_path(id, "/certificate"))).bearer_auth(token);
        Self::decode(self.call(request).await?)
    }

    /// `tunnel.remove`
    pub async fn remove(&self, token: &str, id: &str) -> Result<(), ApiError> {
        let request = self.http.delete(self.url(&Self::tunnel_path(id, ""))).bearer_auth(token);
        self.call(request).await.map(|_| ())
    }

    /// The running tunnel's certificate check (a plain `fetch` with only the bearer token, so Bun's default user
    /// agent and no trace headers): the certificate state, or `None` on any failure.
    pub async fn current_certificate(&self, token: &str, id: &str) -> Option<Value> {
        let response = self
            .http
            .get(self.url(&Self::tunnel_path(id, "/certificate")))
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

fn oc_util_error(e: &reqwest::Error) -> String {
    let mut message = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        message = format!("{message}: {s}");
        source = s.source();
    }
    message
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
}
