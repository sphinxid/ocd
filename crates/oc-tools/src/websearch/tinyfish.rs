//! TinyFish (`plugin/websearch/tinyfish.ts`): MCP `search`, keyless unless a key is configured.

use super::http::{first_text_content, mcp_call, required_string};
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

#[async_trait]
impl SearchProvider for TinyFishProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { id: "tinyfish".into(), name: "TinyFish".into() }
    }

    async fn execute(&self, query: &str) -> Result<Vec<SearchResult>, ProviderError> {
        let key = self.store.api_key("tinyfish").await;
        let mut headers = vec![("User-Agent", self.user_agent.read().clone())];
        match key {
            Some(key) => headers.push(("X-API-Key", key)),
            None => headers.push(("X-TinyFish-Access-Mode", "keyless".into())),
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
