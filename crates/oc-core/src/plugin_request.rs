//! Plugin hooks around model requests (`core/src/session/model-request.ts` `prepare`):
//! `session.context` / `session.compaction` / `session.generate` / `session.title` shape the
//! request, `session.model.request` may change the base URL and headers, and
//! `session.http.request` / `session.http.response` wrap the HTTP exchange.

use std::collections::HashMap;
use std::sync::Arc;

use oc_ai::LLMRequest;
use oc_ai::route::transport::{HttpHook, WireRequest, WireResponse};
use oc_ai::types::{GenerationOptions, SystemPart, ToolDefinition, ToolEntry};
use oc_plugin::HookError;
use oc_schema::types::ModelRef;
use serde_json::{Map, Value, json};

use crate::plugins::Plugins;

/// `GenerationOptions` field names (`GENERATION_KEYS`).
const GENERATION_KEYS: [&str; 8] = ["maxTokens", "temperature", "topP", "topK", "frequencyPenalty", "presencePenalty", "seed", "stop"];

/// What a request flow is (`SessionRequestKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Compaction,
    Generate,
    Title,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Primary => "primary",
            Kind::Compaction => "compaction",
            Kind::Generate => "generate",
            Kind::Title => "title",
        }
    }

    fn hook(self) -> &'static str {
        match self {
            Kind::Primary => "session.context",
            Kind::Compaction => "session.compaction",
            Kind::Generate => "session.generate",
            Kind::Title => "session.title",
        }
    }
}

/// Result of [`shape`].
#[derive(Default)]
pub struct Shaped {
    /// Advertised tool name → registered tool name, after hooks renamed or removed tools; `None`
    /// when no hook ran (every advertised name is its own).
    pub tools: Option<HashMap<String, String>>,
    /// `result` set by a compaction/title hook: the model call is skipped.
    pub result: Option<String>,
    /// The `session.http.*` middleware (`StreamOptions.http`), when a plugin registered either hook.
    pub http: Option<Arc<dyn HttpHook>>,
}

impl std::fmt::Debug for Shaped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shaped").field("tools", &self.tools).field("result", &self.result).field("http", &self.http.is_some()).finish()
    }
}

/// `Model.Ref` as upstream passes it to hooks (`{id, providerID, variant?}`).
fn model_json(model: &ModelRef) -> Value {
    serde_json::to_value(model).unwrap_or(Value::Null)
}

/// Runs the flow's shaping hook on `req` (system, messages, options and, except for titles, the
/// tool definitions) and then `session.model.request`.
pub async fn shape(
    plugins: &Arc<Plugins>,
    key: &str,
    kind: Kind,
    session_id: &str,
    agent: &str,
    model: &ModelRef,
    req: &mut LLMRequest,
    preset: Option<String>,
) -> Result<Shaped, HookError> {
    let mut shaped = Shaped::default();
    let hook = kind.hook();
    omit_openai_output_limit(plugins, key, kind, model, req);
    if plugins.has_hook(key, hook, Some(&model.provider_id)) {
        let mut event = Map::new();
        event.insert("sessionID".into(), json!(session_id));
        event.insert("model".into(), model_json(model));
        if kind != Kind::Title {
            event.insert("agent".into(), json!(agent));
        }
        event.insert("system".into(), serde_json::to_value(&req.system).unwrap_or(json!([])));
        event.insert("messages".into(), serde_json::to_value(&req.messages).unwrap_or(json!([])));
        let mut options = Map::new();
        if matches!(kind, Kind::Primary | Kind::Compaction)
            && let Some(max) = req.generation.as_ref().and_then(|g| g.max_tokens)
        {
            options.insert("maxTokens".into(), json!(max));
        }
        event.insert("options".into(), Value::Object(options));
        // A result an internal hook already set (the Copilot utility title).
        if let Some(result) = &preset {
            event.insert("result".into(), json!(result));
        }
        let originals: Vec<ToolDefinition> = req
            .tools
            .iter()
            .filter_map(|t| match t {
                ToolEntry::Tool(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        if kind != Kind::Title {
            let tools: Map<String, Value> = originals
                .iter()
                .map(|d| (d.name.clone(), json!({ "description": d.description, "input": Value::Object(d.input_schema.clone()) })))
                .collect();
            event.insert("tools".into(), Value::Object(tools));
        }
        let out = plugins.trigger(key, hook, Value::Object(event)).await?;
        if let Some(system) = out.get("system").cloned().and_then(|v| serde_json::from_value::<Vec<SystemPart>>(v).ok()) {
            req.system = system;
        }
        if let Some(messages) = out.get("messages").cloned().and_then(|v| serde_json::from_value(v).ok()) {
            req.messages = messages;
        }
        let options = out.get("options").and_then(Value::as_object).cloned().unwrap_or_default();
        let generation: Map<String, Value> = options.iter().filter(|(k, _)| GENERATION_KEYS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
        let provider: Map<String, Value> = options.iter().filter(|(k, _)| !GENERATION_KEYS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
        req.generation = if generation.is_empty() { None } else { serde_json::from_value::<GenerationOptions>(Value::Object(generation)).ok() };
        if !provider.is_empty() {
            req.provider_options = Some(provider);
        }
        if kind != Kind::Title {
            // Match by identity first (an unchanged definition moved to a new key), then by key;
            // entries matching neither were invented by a hook and are dropped.
            let mut renames = HashMap::new();
            let mut tools = Vec::new();
            for (name, def) in out.get("tools").and_then(Value::as_object).cloned().unwrap_or_default() {
                let description = def.get("description").and_then(Value::as_str).unwrap_or_default().to_string();
                let input = def.get("input").cloned().unwrap_or(json!({}));
                let moved = originals.iter().find(|o| {
                    o.name != name && o.description == description && Value::Object(o.input_schema.clone()) == input && !out["tools"].as_object().is_some_and(|t| t.contains_key(&o.name))
                });
                let Some(original) = moved.or_else(|| originals.iter().find(|o| o.name == name)) else { continue };
                renames.insert(name.clone(), original.name.clone());
                let mut definition = original.clone();
                definition.name = name;
                definition.description = description;
                definition.input_schema = input.as_object().cloned().unwrap_or_default();
                tools.push(ToolEntry::Tool(definition));
            }
            // Keep provider-native entries (namespaces) as they were.
            tools.extend(req.tools.iter().filter(|t| !matches!(t, ToolEntry::Tool(_))).cloned());
            req.tools = tools;
            shaped.tools = Some(renames);
        }
        if matches!(kind, Kind::Compaction | Kind::Title) {
            shaped.result = out.get("result").and_then(Value::as_str).map(String::from);
        }
    } else if matches!(kind, Kind::Compaction | Kind::Title) {
        shaped.result = preset;
    }
    model_request(plugins, key, kind, session_id, agent, model, req).await?;
    shaped.http = http_hooks(plugins, key, kind, session_id, agent, model);
    Ok(shaped)
}

/// The OpenAI plugin's `session.context` / `session.compaction` hooks (openai.ts): provider `openai`
/// requests carry no output limit. The ChatGPT backend rejects one, and OpenAI counts one against
/// rate limits. Built-in plugins run before plugin hooks.
fn omit_openai_output_limit(plugins: &Arc<Plugins>, key: &str, kind: Kind, model: &ModelRef, req: &mut LLMRequest) {
    if !matches!(kind, Kind::Primary | Kind::Compaction)
        || model.provider_id != oc_ai::catalog::openai::PROVIDER_ID
        || !plugins.builtin_enabled(key, oc_ai::catalog::openai::OPENAI_PLUGIN_ID)
    {
        return;
    }
    if let Some(generation) = req.generation.as_mut() {
        generation.max_tokens = None;
        if generation.is_empty() {
            req.generation = None;
        }
    }
}

/// The OpenAI and ChatGPT plugins' `session.model.request` hooks for the location's sign-in.
fn openai_model_request(plugins: &Arc<Plugins>, key: &str, session_id: &str, model: &ModelRef, req: &mut LLMRequest) {
    use oc_ai::catalog::openai::{self, SignIn};
    if model.provider_id != openai::PROVIDER_ID {
        return;
    }
    let sign_in = plugins.openai.get(key);
    let chatgpt = matches!(sign_in, SignIn::TokenSharing { .. }) && plugins.builtin_enabled(key, openai::CHATGPT_PLUGIN_ID);
    let codex = matches!(sign_in, SignIn::Codex { .. }) && plugins.builtin_enabled(key, openai::OPENAI_PLUGIN_ID);
    if !chatgpt && !codex {
        return;
    }
    let mut http = req.http.clone().unwrap_or_default();
    let mut headers = http.headers.clone().unwrap_or_default();
    // `SessionAffinity.get(session)`: the request was assembled with it as `x-session-affinity`.
    let affinity = headers.get("x-session-affinity").cloned().unwrap_or_else(|| session_id.to_string());
    if chatgpt {
        openai::token_sharing_model_request(&mut headers, session_id, &affinity);
    }
    if codex {
        let base_url = req.model.route.endpoint.base_url.clone();
        if let Some(next) = openai::codex_model_request(base_url.as_deref(), &mut headers, &affinity)
            && Some(&next) != base_url.as_ref()
        {
            let mut route = (*req.model.route).clone();
            route.endpoint.base_url = Some(next);
            req.model.route = Arc::new(route);
        }
    }
    http.headers = Some(headers);
    req.http = Some(http);
}

/// Whether a `session.retry` hook runs for the model: a plugin registered one, or the ChatGPT
/// plugin's built-in hook applies (token sharing).
pub fn has_retry_hook(plugins: &Arc<Plugins>, key: &str, provider_id: &str) -> bool {
    plugins.has_hook(key, "session.retry", Some(provider_id)) || chatgpt_retry_applies(plugins, key, provider_id)
}

fn chatgpt_retry_applies(plugins: &Arc<Plugins>, key: &str, provider_id: &str) -> bool {
    use oc_ai::catalog::openai;
    provider_id == openai::PROVIDER_ID
        && matches!(plugins.openai.get(key), openai::SignIn::TokenSharing { .. })
        && plugins.builtin_enabled(key, openai::CHATGPT_PLUGIN_ID)
}

/// `session.http.request` / `session.http.response` around every HTTP request of the call
/// (`model-request.ts` `hasHttpHooks` → `options.http`). `None` when no plugin registered either.
pub fn http_hooks(plugins: &Arc<Plugins>, key: &str, kind: Kind, session_id: &str, agent: &str, model: &ModelRef) -> Option<Arc<dyn HttpHook>> {
    let request = plugins.has_hook(key, "session.http.request", Some(&model.provider_id));
    let response = plugins.has_hook(key, "session.http.response", Some(&model.provider_id));
    // The built-in Copilot plugin registers `http.request` for its provider.
    let copilot = (model.provider_id == oc_ai::catalog::copilot::PROVIDER_ID && plugins.copilot_enabled(key))
        .then(|| crate::copilot::user_agent(plugins.version()));
    if !request && !response && copilot.is_none() {
        return None;
    }
    let mut scope = Map::new();
    scope.insert("sessionID".into(), json!(session_id));
    scope.insert("agent".into(), json!(agent));
    scope.insert("model".into(), model_json(model));
    scope.insert("kind".into(), json!(kind.as_str()));
    Some(Arc::new(PluginHttp { plugins: plugins.clone(), key: key.to_string(), scope, request, response, copilot }))
}

struct PluginHttp {
    plugins: Arc<Plugins>,
    key: String,
    scope: Map<String, Value>,
    request: bool,
    response: bool,
    /// The Copilot hook's user agent, when the built-in hook applies.
    copilot: Option<String>,
}

fn headers_json(headers: &indexmap::IndexMap<String, String>) -> Value {
    Value::Object(headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect())
}

fn headers_of(value: Option<&Value>) -> indexmap::IndexMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|o| o.iter().filter_map(|(k, v)| Some((k.to_ascii_lowercase(), v.as_str()?.to_string()))).collect())
        .unwrap_or_default()
}

fn request_json(request: &WireRequest) -> Value {
    json!({ "url": request.url, "method": request.method, "headers": headers_json(&request.headers), "body": request.body })
}

#[async_trait::async_trait]
impl HttpHook for PluginHttp {
    async fn request(&self, mut request: WireRequest) -> Result<WireRequest, String> {
        if let Some(user_agent) = &self.copilot {
            crate::copilot::http_request(&mut request, user_agent);
        }
        if !self.request {
            return Ok(request);
        }
        let mut event = self.scope.clone();
        event.insert("request".into(), request_json(&request));
        let out = self.plugins.trigger(&self.key, "session.http.request", Value::Object(event)).await.map_err(|e| e.to_string())?;
        let Some(next) = out.get("request").and_then(Value::as_object) else { return Ok(request) };
        Ok(WireRequest {
            url: next.get("url").and_then(Value::as_str).map(String::from).unwrap_or(request.url),
            method: next.get("method").and_then(Value::as_str).map(String::from).unwrap_or(request.method),
            headers: headers_of(next.get("headers")),
            body: next.get("body").and_then(Value::as_str).map(String::from),
        })
    }

    async fn response(&self, request: &WireRequest, response: WireResponse) -> Result<WireResponse, String> {
        if !self.response {
            return Ok(response);
        }
        let mut event = self.scope.clone();
        event.insert("request".into(), request_json(request));
        event.insert(
            "response".into(),
            json!({
                "status": response.status,
                "statusText": response.status_text,
                "headers": headers_json(&response.headers),
                "body": response.body,
            }),
        );
        let out = self.plugins.trigger(&self.key, "session.http.response", Value::Object(event)).await.map_err(|e| e.to_string())?;
        let Some(next) = out.get("response").and_then(Value::as_object) else { return Ok(response) };
        Ok(WireResponse {
            status: next.get("status").and_then(Value::as_u64).map(|s| s as u16).unwrap_or(response.status),
            status_text: next.get("statusText").and_then(Value::as_str).map(String::from).unwrap_or(response.status_text),
            headers: headers_of(next.get("headers")),
            body: next.get("body").and_then(Value::as_str).map(String::from),
        })
    }

    fn wants_response(&self) -> bool {
        self.response
    }
}

const WS_HOOKS: [&str; 3] = ["session.experimental.ws.handshake", "session.experimental.ws.send", "session.experimental.ws.receive"];

/// `experimental.ws.handshake` / `experimental.ws.send` / `experimental.ws.receive` around the
/// session's Responses WebSocket (the `SessionModelTransport` interceptor of `model-request.ts`).
/// `None` when no plugin registered any of them.
pub fn ws_hooks(
    plugins: &Arc<Plugins>,
    key: &str,
    kind: Kind,
    session_id: &str,
    agent: &str,
    model: &ModelRef,
) -> Option<Arc<dyn crate::model_transport::Interceptor>> {
    let has = WS_HOOKS.map(|name| plugins.has_hook(key, name, Some(&model.provider_id)));
    if !has.iter().any(|h| *h) {
        return None;
    }
    let mut scope = Map::new();
    scope.insert("sessionID".into(), json!(session_id));
    scope.insert("agent".into(), json!(agent));
    scope.insert("model".into(), model_json(model));
    scope.insert("kind".into(), json!(kind.as_str()));
    Some(Arc::new(PluginWs { plugins: plugins.clone(), key: key.to_string(), scope, has }))
}

struct PluginWs {
    plugins: Arc<Plugins>,
    key: String,
    scope: Map<String, Value>,
    has: [bool; 3],
}

impl PluginWs {
    async fn frame(&self, index: usize, frame: String) -> Result<String, String> {
        if !self.has[index] {
            return Ok(frame);
        }
        let mut event = self.scope.clone();
        event.insert("frame".into(), json!(frame));
        let out = self.plugins.trigger(&self.key, WS_HOOKS[index], Value::Object(event)).await.map_err(|e| e.to_string())?;
        Ok(out.get("frame").and_then(Value::as_str).map(String::from).unwrap_or(frame))
    }
}

#[async_trait::async_trait]
impl crate::model_transport::Interceptor for PluginWs {
    async fn handshake(&self, url: String, headers: oc_ai::route::Headers) -> Result<(String, oc_ai::route::Headers), String> {
        if !self.has[0] {
            return Ok((url, headers));
        }
        let mut event = self.scope.clone();
        event.insert("url".into(), json!(url));
        event.insert("headers".into(), headers_json(&headers));
        let out = self.plugins.trigger(&self.key, WS_HOOKS[0], Value::Object(event)).await.map_err(|e| e.to_string())?;
        let url = out.get("url").and_then(Value::as_str).map(String::from).unwrap_or(url);
        Ok((url, headers_of(out.get("headers"))))
    }

    async fn send(&self, frame: String) -> Result<String, String> {
        self.frame(1, frame).await
    }

    async fn receive(&self, frame: String) -> Result<String, String> {
        self.frame(2, frame).await
    }
}

/// `session.model.request`: the hook may change the base URL and the headers.
pub async fn model_request(
    plugins: &Arc<Plugins>,
    key: &str,
    kind: Kind,
    session_id: &str,
    agent: &str,
    model: &ModelRef,
    req: &mut LLMRequest,
) -> Result<(), HookError> {
    if model.provider_id == oc_ai::catalog::copilot::PROVIDER_ID && plugins.copilot_enabled(key) {
        let mut http = req.http.clone().unwrap_or_default();
        let mut headers = http.headers.clone().unwrap_or_default();
        crate::copilot::model_request_headers(&mut headers, kind, session_id);
        http.headers = Some(headers);
        req.http = Some(http);
    }
    openai_model_request(plugins, key, session_id, model, req);
    if !plugins.has_hook(key, "session.model.request", Some(&model.provider_id)) {
        return Ok(());
    }
    let base_url = req.model.route.endpoint.base_url.clone();
    let headers = req.http.as_ref().and_then(|h| h.headers.clone()).unwrap_or_default();
    let mut event = json!({
        "sessionID": session_id,
        "agent": agent,
        "model": model_json(model),
        "kind": kind.as_str(),
        "headers": headers,
    });
    if let Some(url) = &base_url {
        event["baseURL"] = json!(url);
    }
    let out = plugins.trigger(key, "session.model.request", event).await?;
    let next_url = out.get("baseURL").and_then(Value::as_str).map(String::from);
    if next_url.is_some() && next_url != base_url {
        let mut route = (*req.model.route).clone();
        route.endpoint.base_url = next_url;
        req.model.route = Arc::new(route);
    }
    let headers: indexmap::IndexMap<String, String> = out
        .get("headers")
        .and_then(Value::as_object)
        .map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
        .unwrap_or_default();
    let mut http = req.http.clone().unwrap_or_default();
    http.headers = if headers.is_empty() { None } else { Some(headers) };
    req.http = Some(http);
    Ok(())
}

/// `session.retry`: may override the retry decision (`{retry: false}` stops retrying).
pub async fn retry(
    plugins: &Arc<Plugins>,
    key: &str,
    session_id: &str,
    agent: &str,
    model: &ModelRef,
    error: &oc_schema::types::SessionError,
    attempt: u32,
    decision: Value,
) -> Value {
    // The ChatGPT plugin's hook (chatgpt.ts): token-sharing failures that will not clear by
    // themselves stop retrying.
    let mut decision = decision;
    if chatgpt_retry_applies(plugins, key, &model.provider_id)
        && oc_ai::catalog::openai::token_sharing_forbids_retry(error.response.as_ref().map(|r| r.body.as_str()))
    {
        decision = json!({ "retry": false });
    }
    if !plugins.has_hook(key, "session.retry", Some(&model.provider_id)) {
        return decision;
    }
    let event = json!({
        "sessionID": session_id,
        "agent": agent,
        "model": model_json(model),
        "error": error,
        "attempt": attempt,
        "decision": decision,
    });
    match plugins.trigger(key, "session.retry", event).await {
        Ok(out) => out.get("decision").cloned().unwrap_or(decision),
        Err(error) => {
            tracing::error!(%error, "session.retry hook failed");
            decision
        }
    }
}
