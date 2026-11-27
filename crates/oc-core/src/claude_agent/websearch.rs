//! Claude Agent as a web search provider (`claude/websearch.ts`, `websearch.ts`, 2.4.1): a one-off `claude` with
//! only Claude Code's WebSearch tool relays the query, and the provider is offered while the integration is
//! connected. It is never made the default; users pick it with `websearch.provider`.

use std::sync::Weak;
use std::time::Duration;

use async_trait::async_trait;
use oc_tools::websearch::{ProviderError, ProviderInfo, SearchProvider, SearchResult};
use serde_json::{Value, json};

use super::process::{Delivered, Options, Query, SystemPrompt};
use super::sessions::{self, Io};
use super::stream::Failure;
use super::transcripts::RealIo;
use super::{ClaudeAgent, Kv, PROVIDER_ID, accounts};

const SYSTEM: &str = "You are a web search relay. Call the WebSearch tool exactly once, with the user's message as the query, verbatim. Do not answer from memory and do not call any other tool.";
/// opencode sets no limit on a search provider, so a stuck claude would hold the tool call forever.
const TIMEOUT: Duration = Duration::from_secs(60);

/// What Claude Code reports after WebSearch: lists of links, with Claude's notes on them as plain strings.
enum Item {
    Note(String),
    Links(Vec<(String, String)>),
}

fn decode(output: &Value) -> Option<Vec<Item>> {
    let mut items = vec![];
    for item in output.get("results")?.as_array()? {
        if let Some(note) = item.as_str() {
            items.push(Item::Note(note.to_string()));
            continue;
        }
        let mut links = vec![];
        for link in item.get("content")?.as_array()? {
            let title = link.get("title")?.as_str()?;
            let url = link.get("url")?.as_str()?;
            links.push((title.to_string(), url.to_string()));
        }
        items.push(Item::Links(links));
    }
    Some(items)
}

/// `ClaudeWebSearch.results(message)`: the search results a WebSearch tool result carries.
pub fn results(message: &Value) -> Option<Vec<SearchResult>> {
    if message.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let items = decode(message.get("tool_use_result")?)?;
    let mut found: Vec<SearchResult> = items
        .iter()
        .flat_map(|item| match item {
            Item::Note(_) => vec![],
            Item::Links(links) => links
                .iter()
                .map(|(title, url)| SearchResult { url: url.clone(), title: Some(title.clone()), content: None, published: None })
                .collect(),
        })
        .collect();
    let summary = items
        .iter()
        .filter_map(|item| match item {
            Item::Note(note) => Some(note.trim().to_string()),
            Item::Links(_) => None,
        })
        .filter(|note| !note.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let Some(first) = found.first_mut().filter(|_| !summary.is_empty()) else { return Some(found) };
    // opencode lists results without a field for the whole search, so the summary rides on the first one, which says so
    first.title =
        Some(format!("Summary of all results below, written by Claude (top source: {})", first.title.as_deref().unwrap_or_default()));
    first.content = Some(summary);
    Some(found)
}

/// `ClaudeWebSearch.search`: runs `claude` with only WebSearch and returns the first tool result's links.
pub async fn search(options: Options, query: &str) -> Result<Vec<SearchResult>, String> {
    let session = Query::start(options);
    session.send_user(json!({
        "type": "user",
        "session_id": "",
        "message": { "role": "user", "content": [{ "type": "text", "text": query }] },
        "parent_tool_use_id": null,
    }));
    let read = async {
        let mut failure = Failure::default();
        loop {
            match session.next().await {
                Some(Delivered::Message(message)) => {
                    if let Some(failed) = failure.failed(&message) {
                        return Err(failed.message);
                    }
                    if let Some(found) = results(&message) {
                        return Ok(found);
                    }
                    // A single-turn query ends its input once the run ended, and Claude Code then exits.
                    if message.get("type").and_then(Value::as_str) == Some("result") {
                        session.end_input();
                    }
                }
                Some(Delivered::End(Some(failure))) => return Err(failure),
                Some(Delivered::End(None)) | None => return Err("Claude finished without searching the web".into()),
            }
        }
    };
    let result = tokio::time::timeout(TIMEOUT, read).await.unwrap_or_else(|_| Err("Claude did not finish the web search within a minute".into()));
    // Closing as soon as the results arrive skips Claude's own answer. The model that asked reads the results itself.
    session.close();
    result
}

/// The provider of one location (`ClaudeWebSearchProvider.register`).
pub struct ClaudeSearch {
    pub owner: Weak<ClaudeAgent>,
    pub key: String,
}

#[async_trait]
impl SearchProvider for ClaudeSearch {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { id: PROVIDER_ID.into(), name: "Claude Agent".into() }
    }

    async fn execute(&self, query: &str) -> Result<Vec<SearchResult>, ProviderError> {
        let owner = self.owner.upgrade().ok_or_else(|| ProviderError::new("Claude Agent is unavailable at this location"))?;
        let io = RealIo::new(owner.plugins.clone());
        let Some(executable) = io.which() else { return Err(ProviderError::new("Install Claude Code and run `claude auth login` first")) };
        let mut connection = owner.connection(&self.key);
        // Chats move to the next account folder when a plan limit refuses them, and the search follows them there
        let last = sessions::Storage::get(&Kv::new(owner.db.clone()), accounts::LAST).await;
        let dir = owner.config_dirs(&self.key).into_iter().find(|dir| last.as_ref().and_then(Value::as_str) == Some(dir.as_str()));
        if let Some(dir) = dir {
            match connection.iter_mut().find(|(k, _)| k == "CLAUDE_CONFIG_DIR") {
                Some(slot) => slot.1 = dir,
                None => connection.push(("CLAUDE_CONFIG_DIR".into(), dir)),
            }
        }
        let process = io.env();
        let connection = super::without_default_folder(&connection, &process);
        let bun = if super::process::is_script(&executable) { io.bun().await } else { None };
        search(options(executable, bun, process, &connection), query).await.map_err(ProviderError::new)
    }
}

/// The relay's `query()` options: `ClaudeQueryOptions.base` with only WebSearch, outside the project.
pub fn options(executable: String, bun: Option<std::path::PathBuf>, process: std::collections::BTreeMap<String, String>, connection: &[(String, String)]) -> Options {
    Options {
        executable,
        bun,
        // The search reads no files, so it runs outside the project
        cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
        env: sessions::base_env(process, connection),
        // WebSearch returns page contents encrypted, so a model has to run it. The cheapest one is enough.
        model: Some("haiku".into()),
        effort: None,
        resume: None,
        session_id: None,
        include_partial_messages: false,
        persist: false,
        system: SystemPrompt::Text(SYSTEM.into()),
        hooks: None,
        tools: None,
        web_search: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_put_claudes_summary_on_the_first_link() {
        let message = json!({ "type": "user", "tool_use_result": { "query": "q", "results": [
            { "tool_use_id": "srv", "content": [{ "title": "A", "url": "https://a" }, { "title": "B", "url": "https://b" }] },
            "  First note.  ",
            "",
            "Second note.",
        ] } });
        let found = results(&message).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].title.as_deref(), Some("Summary of all results below, written by Claude (top source: A)"));
        assert_eq!(found[0].content.as_deref(), Some("First note.\n\nSecond note."));
        assert_eq!(found[0].url, "https://a");
        assert_eq!(serde_json::to_value(&found[1]).unwrap(), json!({ "url": "https://b", "title": "B", "time": {} }));
        // Without notes the links are kept as they are; anything else is not a search result.
        let plain = json!({ "type": "user", "tool_use_result": { "results": [{ "content": [{ "title": "A", "url": "https://a" }] }] } });
        assert_eq!(results(&plain).unwrap()[0].title.as_deref(), Some("A"));
        assert!(results(&json!({ "type": "user", "tool_use_result": { "results": [{ "content": [{ "title": "A" }] }] } })).is_none());
        assert!(results(&json!({ "type": "assistant", "tool_use_result": { "results": [] } })).is_none());
        assert_eq!(results(&json!({ "type": "user", "tool_use_result": { "results": ["Only a note"] } })).unwrap(), vec![]);
    }
}
