//! The runtime half of the built-in `opencode.provider.github.copilot` plugin
//! (`packages/core/src/plugin/provider/github-copilot.ts`); the catalog transforms are in
//! `oc_ai::catalog::copilot` and the AI SDK `sdk`/`language` hooks in the plugin sidecar
//! (`oc-plugin/host/src/aisdk.ts`).
//!
//! - **Loaded state** per location: the active `github-copilot` OAuth connection's API base and token,
//!   and the account's live `/models` list. Loaded when the location's catalog is first built (upstream:
//!   in the background when the plugin activates; here that first build waits for it) and again on
//!   every `credential.switched` for the integration.
//! - **`session.model.request`**: `X-Interaction-Type`, `X-Interaction-Id` and, for background,
//!   compaction and subagent requests, `x-initiator: agent`.
//! - **`session.title`**: titles from the free utility model (`gpt-4o-mini`) when the account has it.
//! - **`session.http.request`**: Copilot headers for requests carrying `x-api-key` (the native
//!   Anthropic route). Registering it also routes every Copilot request through the HTTP middleware,
//!   as upstream.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use indexmap::IndexMap;
use oc_ai::catalog::copilot::{decode_snapshot, State, API_VERSION, PROVIDER_ID, UTILITY_TITLE_MODELS};
use oc_ai::credentials::{CredentialValue, Integrations};
use oc_ai::route::transport::WireRequest;
use oc_ai::types::{ContentPart, Message, SystemPart};
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};

use crate::bus::Bus;
use crate::plugin_request::Kind;

/// The built-in plugin id (gated by the location's `plugins` configuration).
pub const PLUGIN_ID: &str = "opencode.provider.github.copilot";

struct Location {
    state: RwLock<State>,
    /// `loading` semaphore: one load at a time.
    loading: tokio::sync::Mutex<()>,
    /// The initial load has run.
    initialized: tokio::sync::OnceCell<()>,
    integrations: Arc<Integrations>,
    /// The `github-copilot` provider's headers from the latest catalog (`providers.get(...).headers`).
    headers: RwLock<Option<IndexMap<String, String>>>,
}

pub struct Copilot {
    locations: Mutex<HashMap<String, Arc<Location>>>,
    http: reqwest::Client,
    user_agent: String,
    bus: Bus,
    subscribed: std::sync::Once,
    me: Weak<Copilot>,
}

/// `App.useragent(ctx.app)`.
pub fn user_agent(version: &str) -> String {
    oc_ai::credentials::user_agent(oc_util::version::CHANNEL, version, oc_util::version::ARTIFACT)
}

impl Copilot {
    pub fn new(bus: Bus, version: &str) -> Arc<Copilot> {
        Arc::new_cyclic(|me| Copilot {
            locations: Mutex::new(HashMap::new()),
            http: reqwest::Client::new(),
            user_agent: user_agent(version),
            bus,
            subscribed: std::sync::Once::new(),
            me: me.clone(),
        })
    }

    /// The loaded state of location `key` (empty before the first load).
    pub fn state(&self, key: &str) -> State {
        self.locations.lock().get(key).map(|l| l.state.read().clone()).unwrap_or_default()
    }

    /// Called after the first catalog build of location `key` (and harmless later): remembers the
    /// provider headers and runs the initial load (upstream's `refresh()` at activation) once.
    /// Returns whether that load changed the state, i.e. the catalog must be rebuilt.
    pub async fn register(&self, key: &str, integrations: &Arc<Integrations>, headers: Option<IndexMap<String, String>>) -> bool {
        let location = {
            let mut locations = self.locations.lock();
            locations
                .entry(key.to_string())
                .or_insert_with(|| {
                    Arc::new(Location {
                        state: RwLock::new(State::default()),
                        loading: tokio::sync::Mutex::new(()),
                        initialized: tokio::sync::OnceCell::new(),
                        integrations: integrations.clone(),
                        headers: RwLock::new(None),
                    })
                })
                .clone()
        };
        *location.headers.write() = headers;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            self.subscribe(&runtime);
        }
        if location.initialized.initialized() {
            return false;
        }
        let before = location.state.read().clone();
        location.initialized.get_or_init(|| self.refresh(&location)).await;
        *location.state.read() != before
    }

    /// Drops a closed location's state (the plugin scope ends with it).
    pub fn close(&self, key: &str) {
        self.locations.lock().remove(key);
    }

    /// `credential.switched` for `github-copilot` reloads every location.
    fn subscribe(&self, runtime: &tokio::runtime::Handle) {
        let me = self.me.clone();
        let bus = self.bus.clone();
        self.subscribed.call_once(|| {
            let mut events = bus.subscribe();
            runtime.spawn(async move {
                loop {
                    let event = match events.recv().await {
                        Ok(event) => event,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => return,
                    };
                    if event.kind != "credential.switched" || event.data.get("integrationID").and_then(Value::as_str) != Some(PROVIDER_ID) {
                        continue;
                    }
                    let Some(copilot) = me.upgrade() else { return };
                    let locations: Vec<Arc<Location>> = copilot.locations.lock().values().cloned().collect();
                    for location in locations {
                        let copilot = copilot.clone();
                        tokio::spawn(async move { copilot.refresh(&location).await });
                    }
                }
            });
        });
    }

    async fn refresh(&self, location: &Arc<Location>) {
        let _permit = location.loading.lock().await;
        self.load(location).await;
    }

    /// `GithubCopilotPlugin.load`.
    async fn load(&self, location: &Location) {
        let integrations = &location.integrations;
        let connection = integrations.active(PROVIDER_ID).await.ok().flatten();
        let credential = match &connection {
            Some(c) => integrations.resolve(c).await.ok().flatten(),
            None => None,
        };
        let Some(CredentialValue::Oauth { refresh, metadata, .. }) = credential else {
            *location.state.write() = State::default();
            return;
        };
        let url = oc_ai::credentials::flows::copilot::copilot_base_url(metadata.as_ref());
        let headers = location.headers.read().clone().unwrap_or_default();
        let remote = match self.fetch_models(&url, &headers, &refresh).await {
            Ok(models) => Some(Arc::new(models)),
            Err(cause) => {
                tracing::warn!(%cause, "failed to sync GitHub Copilot models");
                None
            }
        };
        let current = integrations.active(PROVIDER_ID).await.ok().flatten();
        if connection.as_ref().map(|c| c.key()) != current.as_ref().map(|c| c.key()) {
            return;
        }
        *location.state.write() =
            State { base_url: Some(url), token: Some(refresh), models: remote, connection: connection.map(|c| c.key()) };
    }

    /// `CopilotModels.load`: `GET <base>/models` (5 s timeout).
    async fn fetch_models(&self, base_url: &str, provider_headers: &IndexMap<String, String>, token: &str) -> Result<oc_ai::catalog::copilot::Snapshot, String> {
        // `{...provider?.headers, Authorization, "User-Agent", "X-GitHub-Api-Version"}`.
        let mut headers = provider_headers.clone();
        headers.insert("Authorization".into(), format!("Bearer {token}"));
        headers.insert("User-Agent".into(), self.user_agent.clone());
        headers.insert("X-GitHub-Api-Version".into(), API_VERSION.into());
        let mut request = self.http.get(format!("{base_url}/models")).timeout(Duration::from_secs(5));
        for (name, value) in &headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("Failed to fetch Copilot models: {}", response.status().as_u16()));
        }
        let body: Value = response.json().await.map_err(|e| e.to_string())?;
        decode_snapshot(&body)
    }

    /// The `session.title` hook: a title from the account's utility model, or `None` (the normal
    /// title request then runs). `agent_model`: the title agent configures its own model.
    pub async fn utility_title(
        &self,
        key: &str,
        session_id: &str,
        agent_model: bool,
        system: &[SystemPart],
        messages: &[Message],
        max_tokens: Option<u64>,
    ) -> Option<String> {
        let state = self.state(key);
        let (Some(base_url), Some(token), Some(models)) = (state.base_url, state.token, state.models) else { return None };
        if agent_model {
            return None;
        }
        let model = UTILITY_TITLE_MODELS.iter().find(|id| models.contains_key(**id))?;
        match self.send_utility_title(&base_url, &token, model, session_id, system, messages, max_tokens).await {
            Ok(title) => Some(title),
            Err(cause) => {
                tracing::debug!(model, %cause, "Copilot utility title failed");
                None
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_utility_title(
        &self,
        base_url: &str,
        token: &str,
        model: &str,
        session_id: &str,
        system: &[SystemPart],
        messages: &[Message],
        max_tokens: Option<u64>,
    ) -> Result<String, String> {
        let text = |parts: &mut dyn Iterator<Item = Option<&str>>| parts.flatten().collect::<Vec<_>>().join("\n");
        let mut wire = vec![];
        if !system.is_empty() {
            let content = text(&mut system.iter().map(|p| (p.kind == "text").then_some(p.text.as_str())));
            wire.push(json!({"role": "system", "content": content}));
        }
        for message in messages {
            let content = text(&mut message.content.iter().map(|p| match p {
                ContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            }));
            wire.push(json!({"role": message.role.as_str(), "content": content}));
        }
        let mut body = json!({"model": model, "messages": wire, "stream": false});
        if let Some(max) = max_tokens {
            body["max_tokens"] = json!(max);
        }
        let request = self
            .http
            .post(format!("{base_url}/chat/completions"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .header("User-Agent", self.user_agent.as_str())
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("Openai-Intent", "conversation-edits")
            .header("X-Interaction-Type", "agent-session-name-generation")
            .header("X-Interaction-Id", session_id)
            .header("x-initiator", "agent")
            .body(serde_json::to_string(&body).map_err(|e| e.to_string())?);
        let exchange = async {
            let response = request.send().await.map_err(|e| e.to_string())?;
            if !response.status().is_success() {
                return Err(format!("Utility title request failed: {}", response.status().as_u16()));
            }
            response.text().await.map_err(|e| e.to_string())
        };
        let text = tokio::time::timeout(Duration::from_secs(20), exchange).await.map_err(|_| "timed out".to_string())??;
        let completion: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let choices = completion.get("choices").and_then(Value::as_array).ok_or("invalid completion")?;
        // `UtilityCompletion`: every choice needs `message.content` (a string or null).
        for choice in choices {
            match choice.get("message").and_then(|m| m.get("content")) {
                Some(Value::String(_) | Value::Null) => {}
                _ => return Err("invalid completion".into()),
            }
        }
        let title = choices.first().and_then(|c| c["message"]["content"].as_str()).map(str::trim).unwrap_or_default();
        if title.is_empty() {
            return Err("Utility title response was empty".into());
        }
        Ok(title.to_string())
    }
}

/// `interactionType`: the Copilot client's `X-Interaction-Type` vocabulary.
pub fn interaction_type(kind: Kind, child: bool) -> &'static str {
    match kind {
        Kind::Title => "conversation-background",
        Kind::Compaction => "conversation-compaction",
        _ if child => "conversation-subagent",
        _ => "conversation-agent",
    }
}

/// The `session.model.request` hook on the request headers. The session's identity comes from the
/// headers the request was assembled with (`x-opencode-parent-session-id`, `x-session-affinity`).
pub fn model_request_headers(headers: &mut IndexMap<String, String>, kind: Kind, session_id: &str) {
    let child = headers.contains_key("x-opencode-parent-session-id");
    let affinity = headers.get("x-session-affinity").cloned().unwrap_or_else(|| session_id.to_string());
    let interaction = interaction_type(kind, child);
    headers.insert("X-Interaction-Type".into(), interaction.into());
    headers.insert("X-Interaction-Id".into(), affinity);
    if interaction != "conversation-agent" {
        headers.insert("x-initiator".into(), "agent".into());
    }
}

/// `requestMetadata`: whether the request continues an agent turn and whether it carries images.
pub fn request_metadata(url: &str, body: Option<&Value>) -> (bool, bool) {
    let record = |v: &Value| v.is_object();
    let Some(body) = body.filter(|b| record(b)) else { return (false, false) };
    let typed = |part: &Value, kind: &str| part.is_object() && part.get("type").and_then(Value::as_str) == Some(kind);
    let user = |last: Option<&Value>| last.is_some_and(|l| record(l) && l.get("role").and_then(Value::as_str) == Some("user"));
    let content = |item: &Value| item.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
    if let Some(input) = body.get("input").and_then(Value::as_array) {
        let agent = !user(input.last());
        let vision = input.iter().any(|item| record(item) && content(item).iter().any(|p| typed(p, "input_image")));
        return (agent, vision);
    }
    let Some(messages) = body.get("messages").and_then(Value::as_array) else { return (false, false) };
    let last = messages.last();
    if url.contains("completions") {
        let agent = !user(last);
        let vision = messages.iter().any(|m| record(m) && content(m).iter().any(|p| typed(p, "image_url")));
        return (agent, vision);
    }
    let last_content = last.filter(|l| record(l)).map(content).unwrap_or_default();
    let agent = !user(last) || !last_content.iter().any(|p| p.is_object() && p.get("type").and_then(Value::as_str) != Some("tool_result"));
    let vision = messages.iter().any(|m| {
        record(m)
            && content(m).iter().any(|p| {
                typed(p, "image")
                    || (typed(p, "tool_result")
                        && p.get("content").and_then(Value::as_array).is_some_and(|nested| nested.iter().any(|n| typed(n, "image"))))
            })
    });
    (agent, vision)
}

/// The `session.http.request` hook: requests with an `x-api-key` header (the native Anthropic
/// route) get the Copilot token as bearer and the Copilot headers (`applyHeaders`, anthropic).
pub fn http_request(request: &mut WireRequest, user_agent: &str) {
    let headers = &mut request.headers;
    let Some(token) = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("x-api-key")).map(|(_, v)| v.clone()) else { return };
    let body = request.body.as_deref().and_then(|b| serde_json::from_str::<Value>(b).ok());
    let (agent, vision) = request_metadata(&request.url, body.as_ref());
    let set = |headers: &mut IndexMap<String, String>, name: &str, value: &str| {
        let existing = headers.keys().find(|k| k.eq_ignore_ascii_case(name)).cloned();
        match existing {
            Some(key) => {
                headers.insert(key, value.to_string());
            }
            None => {
                headers.insert(name.to_string(), value.to_string());
            }
        }
    };
    headers.retain(|k, _| !k.eq_ignore_ascii_case("authorization") && !k.eq_ignore_ascii_case("x-api-key"));
    set(headers, "authorization", &format!("Bearer {token}"));
    set(headers, "user-agent", user_agent);
    set(headers, "openai-intent", "conversation-edits");
    set(headers, "x-github-api-version", API_VERSION);
    // The step may already have declared itself agent-initiated; the body only escalates.
    if agent {
        set(headers, "x-initiator", "agent");
    } else if !headers.keys().any(|k| k.eq_ignore_ascii_case("x-initiator")) {
        set(headers, "x-initiator", "user");
    }
    if vision {
        set(headers, "copilot-vision-request", "true");
    }
    set(headers, "anthropic-beta", "interleaved-thinking-2025-05-14");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interaction_headers() {
        let mut headers: IndexMap<String, String> =
            [("x-session-affinity".to_string(), "ses_root".to_string())].into_iter().collect();
        model_request_headers(&mut headers, Kind::Primary, "ses_x");
        assert_eq!(headers["X-Interaction-Type"], "conversation-agent");
        assert_eq!(headers["X-Interaction-Id"], "ses_root");
        assert!(!headers.contains_key("x-initiator"));
        let mut child: IndexMap<String, String> = [("x-opencode-parent-session-id".to_string(), "p".to_string())].into_iter().collect();
        model_request_headers(&mut child, Kind::Primary, "ses_child");
        assert_eq!(child["X-Interaction-Type"], "conversation-subagent");
        assert_eq!(child["X-Interaction-Id"], "ses_child");
        assert_eq!(child["x-initiator"], "agent");
        assert_eq!(interaction_type(Kind::Title, true), "conversation-background");
        assert_eq!(interaction_type(Kind::Compaction, false), "conversation-compaction");
    }

    #[test]
    fn metadata_by_wire_shape() {
        let responses = json!({"input": [{"role": "user", "content": [{"type": "input_image"}]}]});
        assert_eq!(request_metadata("http://x/responses", Some(&responses)), (false, true));
        let chat = json!({"messages": [{"role": "user", "content": "hi"}, {"role": "tool", "content": "x"}]});
        assert_eq!(request_metadata("http://x/chat/completions", Some(&chat)), (true, false));
        let messages = json!({"messages": [{"role": "user", "content": [{"type": "tool_result", "content": [{"type": "image"}]}]}]});
        assert_eq!(request_metadata("http://x/v1/messages", Some(&messages)), (true, true));
        let fresh = json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]});
        assert_eq!(request_metadata("http://x/v1/messages", Some(&fresh)), (false, false));
        assert_eq!(request_metadata("http://x", Some(&json!([1]))), (false, false));
    }

    #[test]
    fn native_route_headers() {
        let mut request = WireRequest {
            url: "http://x/v1/messages".into(),
            method: "POST".into(),
            headers: [("x-api-key".to_string(), "tok".to_string()), ("content-type".to_string(), "application/json".to_string())].into_iter().collect(),
            body: Some(json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]}).to_string()),
        };
        http_request(&mut request, "ua");
        assert_eq!(
            serde_json::to_value(&request.headers).unwrap(),
            json!({"content-type": "application/json", "authorization": "Bearer tok", "user-agent": "ua",
                   "openai-intent": "conversation-edits", "x-github-api-version": API_VERSION, "x-initiator": "user",
                   "anthropic-beta": "interleaved-thinking-2025-05-14"})
        );
        let mut untouched = WireRequest { url: "u".into(), method: "POST".into(), headers: IndexMap::new(), body: None };
        http_request(&mut untouched, "ua");
        assert!(untouched.headers.is_empty());
    }
}
