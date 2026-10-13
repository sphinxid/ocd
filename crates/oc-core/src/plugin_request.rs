//! Plugin hooks around model requests (`core/src/session/model-request.ts` `prepare`):
//! `session.context` / `session.compaction` / `session.generate` / `session.title` shape the
//! request, `session.model.request` may change the base URL and headers.

use std::collections::HashMap;
use std::sync::Arc;

use oc_ai::LLMRequest;
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
#[derive(Debug, Default)]
pub struct Shaped {
    /// Advertised tool name → registered tool name, after hooks renamed or removed tools; `None`
    /// when no hook ran (every advertised name is its own).
    pub tools: Option<HashMap<String, String>>,
    /// `result` set by a compaction/title hook: the model call is skipped.
    pub result: Option<String>,
}

fn model_json(model: &ModelRef) -> Value {
    json!({ "providerID": model.provider_id, "modelID": model.id })
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
) -> Result<Shaped, HookError> {
    let mut shaped = Shaped::default();
    let hook = kind.hook();
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
    }
    model_request(plugins, key, kind, session_id, agent, model, req).await?;
    Ok(shaped)
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
