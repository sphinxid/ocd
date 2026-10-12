//! Parallel (`plugin/websearch/parallel.ts`): MCP `web_search`, results from `structuredContent`.

use super::http::{first_text_content, mcp_call, optional_string, required_string};
use super::{ProviderError, ProviderInfo, SearchProvider, SearchResult, WebSearchStore, js_date_parse};
use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::{Map, Value, json};
use std::sync::Arc;

pub const ENDPOINT: &str = "https://search.parallel.ai/mcp";

pub struct ParallelProvider {
    pub endpoint: String,
    client: reqwest::Client,
    store: Arc<dyn WebSearchStore>,
    user_agent: Arc<RwLock<String>>,
}

impl ParallelProvider {
    pub fn new(client: reqwest::Client, store: Arc<dyn WebSearchStore>, user_agent: Arc<RwLock<String>>) -> Self {
        ParallelProvider { endpoint: ENDPOINT.into(), client, store, user_agent }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }
}

fn nullable_array<'a>(object: &'a Map<String, Value>, key: &str) -> Result<Option<&'a Vec<Value>>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => Ok(Some(items)),
        Some(_) => Err(format!("[\"{key}\"]\n  Expected array | null")),
    }
}

/// Validate the MCP output (`content` + `structuredContent: SearchResponse`) and map results.
pub fn decode(result: &Value) -> Result<Vec<SearchResult>, String> {
    first_text_content(result, |_| Ok(()))?;
    let structured = result
        .get("structuredContent")
        .and_then(Value::as_object)
        .ok_or_else(|| "[\"structuredContent\"]\n  Missing key".to_string())?;
    required_string(structured, "search_id")?;
    required_string(structured, "session_id")?;
    if let Some(warnings) = nullable_array(structured, "warnings")? {
        for warning in warnings {
            let warning = warning.as_object().ok_or("[\"warnings\"]\n  Expected object")?;
            let kind = required_string(warning, "type")?;
            if !["spec_validation_warning", "input_validation_warning", "warning"].contains(&kind.as_str()) {
                return Err("[\"warnings\"][\"type\"]\n  Expected a valid warning type".into());
            }
            required_string(warning, "message")?;
            match warning.get("detail") {
                None | Some(Value::Null) | Some(Value::Object(_)) => {}
                Some(_) => return Err("[\"detail\"]\n  Expected object | null".into()),
            }
        }
    }
    if let Some(usage) = nullable_array(structured, "usage")? {
        for item in usage {
            let item = item.as_object().ok_or("[\"usage\"]\n  Expected object")?;
            required_string(item, "name")?;
            let integer = item.get("count").and_then(Value::as_f64).is_some_and(|n| n.fract() == 0.0);
            if !integer {
                return Err("[\"count\"]\n  Expected an integer".into());
            }
        }
    }
    let results = structured
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| "[\"results\"]\n  Expected array".to_string())?;
    results
        .iter()
        .map(|item| {
            let item = item.as_object().ok_or("[\"results\"]\n  Expected object")?;
            let url = required_string(item, "url")?;
            let title = optional_string(item, "title")?;
            let publish_date = optional_string(item, "publish_date")?;
            let excerpts = item
                .get("excerpts")
                .and_then(Value::as_array)
                .ok_or("[\"excerpts\"]\n  Expected array")?
                .iter()
                .map(|e| e.as_str().map(str::to_string).ok_or("[\"excerpts\"]\n  Expected string".to_string()))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(SearchResult {
                url,
                title: title.filter(|t| !t.is_empty()),
                content: (!excerpts.is_empty()).then(|| excerpts.join("\n\n")),
                published: publish_date.filter(|p| !p.is_empty()).and_then(|p| js_date_parse(&p)),
            })
        })
        .collect()
}

#[async_trait]
impl SearchProvider for ParallelProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { id: "parallel".into(), name: "Parallel".into() }
    }

    async fn execute(&self, query: &str) -> Result<Vec<SearchResult>, ProviderError> {
        let key = self.store.api_key("parallel").await;
        let mut headers = vec![("User-Agent", self.user_agent.read().clone())];
        if let Some(key) = key {
            headers.push(("Authorization", format!("Bearer {key}")));
        }
        let results = mcp_call(
            &self.client,
            &self.endpoint,
            "web_search",
            json!({ "objective": query, "search_queries": [query] }),
            &headers,
            decode,
        )
        .await?;
        Ok(results.unwrap_or_default())
    }
}
