//! Firecrawl (`plugin/websearch/firecrawl.ts`): MCP `firecrawl_search`.

use super::http::{first_text_content, mcp_call, optional_string, required_string};
use super::{ProviderError, ProviderInfo, SearchProvider, SearchResult, WebSearchStore};
use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::{Value, json};
use std::sync::Arc;

pub const ENDPOINT: &str = "https://mcp.firecrawl.dev/v2/mcp";

pub struct FirecrawlProvider {
    pub endpoint: String,
    client: reqwest::Client,
    store: Arc<dyn WebSearchStore>,
    user_agent: Arc<RwLock<String>>,
}

impl FirecrawlProvider {
    pub fn new(client: reqwest::Client, store: Arc<dyn WebSearchStore>, user_agent: Arc<RwLock<String>>) -> Self {
        FirecrawlProvider { endpoint: ENDPOINT.into(), client, store, user_agent }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }
}

/// `decodeUnknownOption(SearchResponse)` over the text content; `None` on any mismatch.
pub fn parse_search_response(text: &str) -> Option<Vec<SearchResult>> {
    let value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    if !object.get("success").is_some_and(Value::is_boolean) {
        return None;
    }
    let web = object.get("data")?.as_object()?.get("web")?.as_array()?;
    web.iter()
        .map(|item| {
            let item = item.as_object()?;
            let url = required_string(item, "url").ok()?;
            let title = optional_string(item, "title").ok()?;
            let description = optional_string(item, "description").ok()?;
            Some(SearchResult {
                url,
                title: title.filter(|t| !t.is_empty()),
                content: description.filter(|d| !d.is_empty()),
                published: None,
            })
        })
        .collect()
}

#[async_trait]
impl SearchProvider for FirecrawlProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { id: "firecrawl".into(), name: "Firecrawl".into() }
    }

    async fn execute(&self, query: &str) -> Result<Vec<SearchResult>, ProviderError> {
        let key = self.store.api_key("firecrawl").await;
        let mut headers = vec![("User-Agent", self.user_agent.read().clone())];
        if let Some(key) = key {
            headers.push(("Authorization", format!("Bearer {key}")));
        }
        let text = mcp_call(
            &self.client,
            &self.endpoint,
            "firecrawl_search",
            json!({ "query": query, "limit": 8 }),
            &headers,
            |result| first_text_content(result, |_| Ok(())),
        )
        .await?;
        Ok(text.flatten().and_then(|text| parse_search_response(&text)).unwrap_or_default())
    }
}
