//! `websearch` (`core/src/tool/plugin/websearch.ts`).

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::host::{FormField, FormOption, FormRequest, FormResponse};
use crate::tool::{DefinitionContext, ErrorCause, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, decode, meta};
use crate::websearch::{SearchResult, Selection, WebSearch, WebSearchError};

pub const NO_RESULTS: &str = "No search results found. Please try a different query.";

static SELECTION_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Description (year computed at call time; upstream computes it at module load).
pub fn description() -> String {
    use chrono::Datelike;
    format!(
        "Search the web using the user's selected search integration. Use this for current information beyond knowledge cutoff.\n\nThe current year is {}. Use this year when searching for recent information or current events.",
        chrono::Local::now().year()
    )
}

/// Model content for results.
pub fn to_model_content(results: &[SearchResult]) -> String {
    if results.is_empty() {
        return NO_RESULTS.into();
    }
    results
        .iter()
        .map(|r| {
            let title = r.title.clone().unwrap_or_else(|| r.url.clone());
            let published = r
                .published
                .filter(|p| *p != 0)
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|d| format!("\nPublished: {}", d.format("%Y-%m-%dT%H:%M:%S%.3fZ")))
                .unwrap_or_default();
            let content = r.content.as_ref().filter(|c| !c.is_empty()).map(|c| format!("\n\n{c}")).unwrap_or_default();
            format!("## [{title}]({}){published}{content}", r.url)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[derive(Deserialize)]
struct Input {
    query: String,
}

pub struct WebSearchTool {
    websearch: Arc<WebSearch>,
}

impl WebSearchTool {
    pub fn new(websearch: Arc<WebSearch>) -> Self {
        Self { websearch }
    }

    /// Consent flow when no provider is selected. `Ok(Some(id))` = chosen provider.
    async fn choose(&self, ctx: &ToolContext) -> Result<Option<String>, ErrorCause> {
        if matches!(self.websearch.default_provider().await, Ok(Some(_))) {
            return Ok(None);
        }
        let providers = self.websearch.providers().await;
        if providers.is_empty() {
            return Err(ErrorCause::Error(String::new()));
        }
        let names = providers.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ");
        let form = |title: &str, key: &str, description: &str, options: Vec<FormOption>| FormRequest {
            session_id: ctx.session_id.clone(),
            title: title.into(),
            metadata: meta(vec![("kind", json!("websearch.provider"))]),
            fields: vec![FormField {
                key: key.into(),
                title: None,
                description: Some(description.into()),
                kind: "string".into(),
                required: Some(true),
                options,
                custom: false,
            }],
        };
        let opt = |value: &str, label: String| FormOption { value: value.into(), label, description: None };
        let cancelled = || ErrorCause::Error("Web search cancelled".into());
        let response = ctx
            .host
            .ask_form(form(
                "Web Search",
                "choice",
                "Allow OpenCode to search the web for up-to-date information?",
                vec![
                    opt("allow", format!("Allow search via {names}")),
                    opt("choose", "Choose another provider".into()),
                    opt("disable", "Disable web search".into()),
                ],
            ))
            .await
            .map_err(ErrorCause::Error)?;
        let FormResponse::Answered(answer) = response else { return Err(cancelled()) };
        let choice = answer.get("choice").and_then(Value::as_str).unwrap_or("").to_string();
        if choice == "disable" {
            self.websearch.select(Selection::Disabled).await;
            return Err(ErrorCause::Error(String::new()));
        }
        let provider_id = if choice == "choose" {
            let response = ctx
                .host
                .ask_form(form(
                    "Choose a web search provider",
                    "provider",
                    "Choose a provider for web search.",
                    providers.iter().map(|p| opt(&p.id, p.name.clone())).collect(),
                ))
                .await
                .map_err(ErrorCause::Error)?;
            let FormResponse::Answered(answer) = response else { return Err(cancelled()) };
            answer.get("provider").and_then(Value::as_str).map(String::from).unwrap_or_else(|| "random".into())
        } else {
            "random".into()
        };
        if provider_id == "random" {
            self.websearch.select(Selection::Random).await;
            return Ok(None);
        }
        let Some(provider) = providers.iter().find(|p| p.id == provider_id) else {
            return Err(ErrorCause::Error(String::new()));
        };
        self.websearch.select(Selection::Provider(provider.id.clone())).await;
        Ok(Some(provider.id.clone()))
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "websearch"
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::direct()
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        super::definition("websearch", description(), include_str!("../../schemas/websearch.json"))
    }

    fn advertised(&self, ctx: &DefinitionContext) -> bool {
        !ctx.websearch_disabled
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let input: Input = decode(input)?;
        let fallback = format!("Unable to search the web for {}", input.query);
        super::assert(&ctx, "websearch", vec![input.query.clone()], Some(vec!["*".into()]), Some(meta(vec![("query", json!(input.query))])))
            .await
            .map_err(super::perm(fallback.clone()))?;
        let progress = ctx.progress.clone();
        let on_provider = move |p: &crate::websearch::ProviderInfo| progress(meta(vec![("provider", json!(p.id))]));
        let mut result = self.websearch.query(&input.query, None, Some(&ctx.session_id), &on_provider).await;
        if matches!(result, Err(WebSearchError::ProviderRequired)) {
            let chosen = {
                let _guard = SELECTION_LOCK.lock().await;
                match tokio::time::timeout(Duration::from_secs(60), self.choose(&ctx)).await {
                    Err(_) => Err(ErrorCause::Error("Web search cancelled".into())),
                    Ok(r) => r,
                }
            };
            result = match chosen {
                Err(cause) => return Err(ToolError::with_cause(fallback, cause)),
                Ok(id) => self.websearch.query(&input.query, id.as_deref(), Some(&ctx.session_id), &on_provider).await,
            };
        }
        let response = match result {
            Ok(response) => response,
            Err(WebSearchError::Request { provider_id, error }) => {
                let message = match error.status {
                    None => fallback,
                    Some(429) => "Web search rate limited (HTTP 429)".into(),
                    Some(401) => "Web search authentication failed (HTTP 401)".into(),
                    Some(status) => format!("Web search request failed (HTTP {status})"),
                };
                return Err(ToolError::with_cause(message, ErrorCause::Error(String::new()))
                    .with_metadata(meta(vec![("provider", json!(provider_id))])));
            }
            Err(_) => return Err(ToolError::with_cause(fallback, ErrorCause::Error(String::new()))),
        };
        Ok(ToolResult::text(to_model_content(&response.results))
            .with_output(json!({"provider": response.provider_id, "results": response.results}))
            .with_metadata(meta(vec![("provider", json!(response.provider_id))])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_format() {
        assert_eq!(to_model_content(&[]), NO_RESULTS);
        let results = vec![
            SearchResult { url: "https://a".into(), title: Some("A".into()), content: Some("body".into()), published: Some(0) },
            SearchResult { url: "https://b".into(), title: None, content: None, published: Some(1_700_000_000_000) },
        ];
        assert_eq!(to_model_content(&results), "## [A](https://a)\n\nbody\n\n## [https://b](https://b)\nPublished: 2023-11-14T22:13:20.000Z");
        assert!(description().contains("The current year is "));
    }
}
