//! Tavily (`plugin/websearch/tavily.ts`): REST Search API, keyless unless a key is configured.

use super::http::{TIMEOUT, check_status, required_string, transport_error};
use super::{ProviderError, ProviderInfo, SearchProvider, SearchResult, WebSearchStore};
use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::{Value, json};
use std::sync::Arc;

pub const ENDPOINT: &str = "https://api.tavily.com/search";

pub struct TavilyProvider {
    pub endpoint: String,
    client: reqwest::Client,
    store: Arc<dyn WebSearchStore>,
    user_agent: Arc<RwLock<String>>,
}

impl TavilyProvider {
    pub fn new(client: reqwest::Client, store: Arc<dyn WebSearchStore>, user_agent: Arc<RwLock<String>>) -> Self {
        TavilyProvider { endpoint: ENDPOINT.into(), client, store, user_agent }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }
}

/// `schemaBodyJson(SearchResponse)` mapping.
pub fn decode(body: &str) -> Result<Vec<SearchResult>, String> {
    let value: Value = serde_json::from_str(body).map_err(|e| format!("SyntaxError: {e}"))?;
    let results = value
        .as_object()
        .and_then(|o| o.get("results"))
        .and_then(Value::as_array)
        .ok_or_else(|| "[\"results\"]\n  Expected array".to_string())?;
    results
        .iter()
        .map(|item| {
            let item = item.as_object().ok_or("[\"results\"]\n  Expected object")?;
            let title = required_string(item, "title")?;
            let url = required_string(item, "url")?;
            let content = required_string(item, "content")?;
            Ok(SearchResult { url, title: Some(title), content: Some(content).filter(|c| !c.is_empty()), published: None })
        })
        .collect()
}

#[async_trait]
impl SearchProvider for TavilyProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { id: "tavily".into(), name: "Tavily".into() }
    }

    async fn execute(&self, query: &str) -> Result<Vec<SearchResult>, ProviderError> {
        let key = self.store.api_key("tavily").await;
        let url = self.endpoint.clone();
        let mut request = self
            .client
            .post(&url)
            .header("accept", "application/json")
            .header("User-Agent", self.user_agent.read().clone())
            .header("X-Client-Name", "opencode2");
        request = match key {
            Some(key) => request.header("Authorization", format!("Bearer {key}")),
            None => request.header("X-Tavily-Access-Mode", "keyless"),
        };
        let body = json!({ "query": query, "search_depth": "basic", "chunks_per_source": 3, "max_results": 8 });
        let request = request.header("content-type", "application/json").body(body.to_string());
        let run = async {
            let response = request.send().await.map_err(|e| transport_error("POST", &url, &e))?;
            check_status(&response, "POST", &url)?;
            let text = response.text().await.map_err(|e| transport_error("POST", &url, &e))?;
            decode(&text).map_err(ProviderError::new)
        };
        match tokio::time::timeout(TIMEOUT, run).await {
            Ok(result) => result,
            Err(_) => Err(ProviderError::new("Tavily web search request timed out")),
        }
    }
}
