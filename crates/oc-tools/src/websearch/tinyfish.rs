//! TinyFish (`plugin/websearch/tinyfish.ts`): MCP `search`, keyless unless a key is configured.

use super::http::{
    MAX_RESPONSE_BYTES, TIMEOUT, check_status, collect_bounded, first_text_content, mcp_call, required_string,
    transport_error,
};
use super::{ProviderError, ProviderInfo, SearchProvider, SearchResult, WebSearchStore};
use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::{Value, json};
use std::sync::Arc;

pub const ENDPOINT: &str = "https://agent.tinyfish.ai/mcp";

pub struct TinyFishProvider {
    pub endpoint: String,
    client: reqwest::Client,
    store: Arc<dyn WebSearchStore>,
    user_agent: Arc<RwLock<String>>,
}

impl TinyFishProvider {
    pub fn new(client: reqwest::Client, store: Arc<dyn WebSearchStore>, user_agent: Arc<RwLock<String>>) -> Self {
        TinyFishProvider { endpoint: ENDPOINT.into(), client, store, user_agent }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }
}

/// `decodeUnknownOption(SearchResponse)`; `None` on any mismatch.
pub fn parse_search_response(text: &str) -> Option<Vec<SearchResult>> {
    let value: Value = serde_json::from_str(text).ok()?;
    let results = value.as_object()?.get("results")?.as_array()?;
    results
        .iter()
        .map(|item| {
            let item = item.as_object()?;
            let url = required_string(item, "url").ok()?;
            let title = required_string(item, "title").ok()?;
            let snippet = required_string(item, "snippet").ok()?;
            Some(SearchResult {
                url,
                title: Some(title),
                content: Some(snippet).filter(|s| !s.is_empty()),
                published: None,
            })
        })
        .collect()
}

impl TinyFishProvider {
    /// An MCP `initialize` plus `notifications/initialized` for a keyed session (25 s in all), returning the
    /// session id the server assigned.
    async fn initialize(&self, headers: &[(&str, String)]) -> Result<String, ProviderError> {
        let url = self.endpoint.as_str();
        let post = |body: Value, session: Option<&str>| {
            let mut request = self
                .client
                .post(url)
                .header("accept", "application/json, text/event-stream")
                .header("content-type", "application/json");
            for (name, value) in headers {
                request = request.header(*name, value);
            }
            if let Some(session) = session {
                request = request.header("Mcp-Session-Id", session);
            }
            for (k, v) in oc_util::trace::headers() {
                request = request.header(k, v);
            }
            request.body(serde_json::to_vec(&body).unwrap_or_default())
        };
        let run = async {
            let initialize = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "opencode", "version": "1.0" },
                },
            });
            let response = post(initialize, None).send().await.map_err(|e| transport_error("POST", url, &e))?;
            check_status(&response, "POST", url)?;
            let session = response.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()).map(str::to_string);
            collect_bounded(
                response,
                MAX_RESPONSE_BYTES,
                || ProviderError::new(format!("initialize response exceeded {MAX_RESPONSE_BYTES} bytes")),
                "POST",
                url,
            )
            .await?;
            let Some(session) = session else {
                return Err(ProviderError::new("TinyFish MCP initialize did not return an Mcp-Session-Id header"));
            };
            let notification = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
            let response =
                post(notification, Some(&session)).send().await.map_err(|e| transport_error("POST", url, &e))?;
            check_status(&response, "POST", url)?;
            Ok(session)
        };
        match tokio::time::timeout(TIMEOUT, run).await {
            Ok(result) => result,
            Err(_) => Err(ProviderError::new("TinyFish initialize request timed out")),
        }
    }
}

#[async_trait]
impl SearchProvider for TinyFishProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { id: "tinyfish".into(), name: "TinyFish".into() }
    }

    async fn execute(&self, query: &str) -> Result<Vec<SearchResult>, ProviderError> {
        let key = self.store.api_key("tinyfish").await;
        let keyed = key.is_some();
        let mut headers = vec![("User-Agent", self.user_agent.read().clone())];
        match key {
            Some(key) => headers.push(("X-API-Key", key)),
            None => headers.push(("X-TinyFish-Access-Mode", "keyless".into())),
        }
        // Keyed requests are rejected without the `Mcp-Session-Id` returned by `initialize`; keyless ones are not.
        if keyed {
            let session = self.initialize(&headers).await?;
            headers.push(("Mcp-Session-Id", session));
        }
        let text = mcp_call(
            &self.client,
            &self.endpoint,
            "search",
            json!({ "query": query }),
            &headers,
            |result| first_text_content(result, |_| Ok(())),
        )
        .await?;
        Ok(text.flatten().and_then(|text| parse_search_response(&text)).unwrap_or_default())
    }
}
