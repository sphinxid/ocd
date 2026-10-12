//! Exa (`plugin/websearch/exa.ts`): MCP `web_search_exa`, key passed as `?exaApiKey=`.

use super::http::{first_text_content, mcp_call};
use super::{ProviderError, ProviderInfo, SearchProvider, SearchResult, WebSearchStore, js_date_parse};
use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use std::sync::{Arc, LazyLock};

pub const ENDPOINT: &str = "https://mcp.exa.ai/mcp";

pub struct ExaProvider {
    pub endpoint: String,
    client: reqwest::Client,
    store: Arc<dyn WebSearchStore>,
}

impl ExaProvider {
    pub fn new(client: reqwest::Client, store: Arc<dyn WebSearchStore>) -> Self {
        ExaProvider { endpoint: ENDPOINT.into(), client, store }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }
}

fn decode(result: &Value) -> Result<Option<String>, String> {
    first_text_content(result, |item| match item.get("_meta") {
        None => Ok(()),
        Some(Value::Object(meta)) if meta.get("searchTime").is_some_and(Value::is_number) => Ok(()),
        Some(_) => Err("[\"_meta\"]\n  Expected { readonly searchTime: number }".into()),
    })
}

#[async_trait]
impl SearchProvider for ExaProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { id: "exa".into(), name: "Exa".into() }
    }

    async fn execute(&self, query: &str) -> Result<Vec<SearchResult>, ProviderError> {
        let key = self.store.api_key("exa").await;
        let mut url = url::Url::parse(&self.endpoint).map_err(|e| ProviderError::new(format!("Invalid URL: {e}")))?;
        if let Some(key) = key {
            url.query_pairs_mut().append_pair("exaApiKey", &key);
        }
        let text = mcp_call(
            &self.client,
            url.as_str(),
            "web_search_exa",
            json!({ "query": query, "numResults": 8 }),
            &[],
            decode,
        )
        .await?;
        Ok(text.flatten().map(|text| parse_results(&text)).unwrap_or_default())
    }
}

static URL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?mR)^URL:\s*(.+)$").unwrap());
static TITLE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?mR)^Title:\s*(.+)$").unwrap());
static PUBLISHED_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?mR)^Published:\s*(.+)$").unwrap());
static CONTENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^(?:Highlights|Text):\s*\n?([\s\S]*)$").unwrap());

fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}

/// Exa's text blocks: `URL:`/`Title:`/`Published:`/`Highlights|Text:` split on `\n\n---\n\n`.
pub fn parse_results(text: &str) -> Vec<SearchResult> {
    text.split("\n\n---\n\n")
        .filter_map(|block| {
            let capture = |re: &Regex| re.captures(block).and_then(|c| c.get(1)).map(|m| js_trim(m.as_str()).to_string());
            let url = capture(&URL_RE).filter(|u| !u.is_empty())?;
            let title = capture(&TITLE_RE).filter(|t| !t.is_empty() && t != "N/A");
            let published = capture(&PUBLISHED_RE)
                .filter(|p| !p.is_empty() && p != "N/A")
                .and_then(|p| js_date_parse(&p));
            let content = capture(&CONTENT_RE).filter(|c| !c.is_empty());
            Some(SearchResult { url, title, content, published })
        })
        .collect()
}
