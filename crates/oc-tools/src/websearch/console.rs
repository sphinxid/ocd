//! OpenCode Console web search (`plugin/provider/opencode.ts:357-413`):
//! `POST ${server}/api/websearch` with the Console credential.

use super::http::{TIMEOUT, check_status, transport_error};
use super::{ProviderError, ProviderInfo, Response, SearchProvider, SearchResult};
use async_trait::async_trait;
use futures::future::BoxFuture;
use serde_json::json;
use std::sync::Arc;

pub const DEFAULT_SERVER: &str = "https://opencode.ai/console";

/// Resolved Console credential. `server` = `credential.metadata.server` (default Console when absent).
#[derive(Clone, Debug, PartialEq)]
pub struct ConsoleCredential {
    pub token: String,
    pub org_id: Option<String>,
    pub server: Option<String>,
}

/// Resolves the active Console credential. Errors are provider failures, e.g.
/// `OpenCode Console connection changed` / `OpenCode Console is not connected`.
pub type ConsoleCredentialFn = Arc<dyn Fn() -> BoxFuture<'static, Result<ConsoleCredential, String>> + Send + Sync>;

pub struct ConsoleProvider {
    provider_id: String,
    client: reqwest::Client,
    credential: ConsoleCredentialFn,
}

impl ConsoleProvider {
    /// Builds its own client: the Console request refuses redirects (`redirect: "error"`).
    pub fn new(provider_id: impl Into<String>, credential: ConsoleCredentialFn) -> Self {
        let client = oc_util::http::client_builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        ConsoleProvider { provider_id: provider_id.into(), client, credential }
    }
}

/// `normalizeServer`: origin + pathname without trailing slashes; HTTP(S) only.
pub fn normalize_server(input: &str) -> Result<String, String> {
    let url = url::Url::parse(input).map_err(|_| "Invalid OpenCode server URL: Invalid URL".to_string())?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("Invalid OpenCode server URL: expected HTTP(S)".into());
    }
    Ok(format!("{}{}", url.origin().ascii_serialization(), url.path().trim_end_matches('/')))
}

#[async_trait]
impl SearchProvider for ConsoleProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { id: self.provider_id.clone(), name: "OpenCode Web Search".into() }
    }

    async fn execute(&self, query: &str) -> Result<Vec<SearchResult>, ProviderError> {
        let credential = (self.credential)().await.map_err(ProviderError::new)?;
        let server = normalize_server(credential.server.as_deref().unwrap_or(DEFAULT_SERVER)).map_err(ProviderError::new)?;
        let url = format!("{server}/api/websearch");
        let mut request = self
            .client
            .post(&url)
            .header("accept", "application/json")
            .header("authorization", format!("Bearer {}", credential.token));
        if let Some(org) = credential.org_id.as_deref().filter(|o| !o.is_empty()) {
            request = request.header("x-org-id", org);
        }
        let body = json!({ "query": query, "providerID": self.provider_id });
        let mut request = request.header("content-type", "application/json").body(body.to_string());
        for (k, v) in oc_util::trace::headers() {
            request = request.header(k, v);
        }
        let run = async {
            let response = request.send().await.map_err(|e| transport_error("POST", &url, &e))?;
            check_status(&response, "POST", &url)?;
            let text = response.text().await.map_err(|e| transport_error("POST", &url, &e))?;
            serde_json::from_str::<Response>(&text).map_err(|e| ProviderError::new(format!("DecodeError: {e}")))
        };
        let response = match tokio::time::timeout(TIMEOUT, run).await {
            Ok(result) => result?,
            Err(_) => return Err(ProviderError::new("OpenCode web search request timed out")),
        };
        if response.provider_id != self.provider_id {
            return Err(ProviderError::new(format!(
                "OpenCode web search returned provider {} instead of {}",
                response.provider_id, self.provider_id
            )));
        }
        Ok(response.results)
    }
}
