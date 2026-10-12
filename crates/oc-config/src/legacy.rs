//! v1 (legacy) location configuration schemas and their v2 migrations
//! (`packages/core/src/v1/config/*`).

use indexmap::IndexMap;
use serde_json::{Map, Number, Value, json};

use crate::decode::{Decode, Object, finite, int, literal, opt, opt_with, positive_int, record, req};

// ---------------------------------------------------------------------------------------------
// Helpers shared with normalize
// ---------------------------------------------------------------------------------------------

/// Renames retired provider IDs in V1 fields and the shared top-level model.
pub fn provider_id(input: &str) -> String {
    match input {
        "azure-cognitive-services" => "azure".into(),
        "google-vertex-anthropic" => "google-vertex".into(),
        other => other.into(),
    }
}

/// Map v1 permission/tool keys onto their renamed v2 tool actions.
pub fn normalize_action(action: &str) -> String {
    match action {
        "write" | "patch" => "edit".into(),
        "task" => "subagent".into(),
        "bash" => "shell".into(),
        other => other.into(),
    }
}

/// `/^[^/#]+\/[^#]+$/`
pub fn is_legacy_model_ref(input: &str) -> bool {
    let Some(slash) = input.find('/') else { return false };
    let provider = &input[..slash];
    let model = &input[slash + 1..];
    !provider.is_empty() && !provider.contains('#') && !model.is_empty() && !model.contains('#')
}

/// `ConfigMigrateV1.modelSelection` → encoded `{ providerID, model, variant? }`.
pub fn model_selection(input: Option<&str>, variant: Option<&str>) -> Option<Value> {
    let input = input?;
    if !is_legacy_model_ref(input) {
        return None;
    }
    let slash = input.find('/')?;
    let mut out = Map::new();
    out.insert("providerID".into(), Value::String(provider_id(&input[..slash])));
    out.insert("model".into(), Value::String(input[slash + 1..].to_string()));
    if let Some(v) = variant
        && !v.is_empty()
        && !v.contains('#')
    {
        out.insert("variant".into(), Value::String(v.into()));
    }
    Some(Value::Object(out))
}

/// `Provider.aisdk`: prefix `aisdk:` unless already present.
pub fn aisdk(value: &str) -> String {
    if value.starts_with("aisdk:") { value.to_string() } else { format!("aisdk:{value}") }
}

fn insert_opt(map: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(v) = value {
        map.insert(key.into(), v);
    }
}

// ---------------------------------------------------------------------------------------------
// Permission (`v1/config/permission.ts`)
// ---------------------------------------------------------------------------------------------

/// `Action = "ask" | "allow" | "deny"`
pub fn decode_action(value: &Value) -> Option<String> {
    literal(value, &["ask", "allow", "deny"]).map(str::to_string)
}

/// v1 permission rule: an action or ordered `[resource, action]` entries.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionRuleV1 {
    Action(String),
    Entries(Vec<(String, String)>),
}

/// Decoded `ConfigPermissionV1.Info` (ordered entries).
pub type PermissionV1 = Vec<(String, PermissionRuleV1)>;

const ACTION_ONLY: &[&str] = &["question", "websearch", "doom_loop"];

pub fn decode_permission(value: &Value) -> Option<PermissionV1> {
    let object: Object = match value {
        Value::String(_) => {
            let action = decode_action(value)?;
            let mut m = Map::new();
            m.insert("*".into(), Value::String(action));
            m
        }
        Value::Object(o) => o.clone(),
        _ => return None,
    };
    let mut out = Vec::new();
    for (key, raw) in &object {
        let rule = if let Some(action) = decode_action(raw) {
            PermissionRuleV1::Action(action)
        } else {
            let o = record(raw)?;
            let entries = o
                .iter()
                .map(|(resource, effect)| Some((resource.clone(), decode_action(effect)?)))
                .collect::<Option<Vec<_>>>()?;
            PermissionRuleV1::Entries(entries)
        };
        out.push((key.clone(), rule));
    }
    if out.iter().any(|(k, r)| ACTION_ONLY.contains(&k.as_str()) && !matches!(r, PermissionRuleV1::Action(_))) {
        return None;
    }
    Some(out)
}

fn permission_rules(info: &PermissionV1) -> Option<Value> {
    let mut rules = Vec::new();
    for (key, rule) in info {
        let action = normalize_action(key);
        match rule {
            PermissionRuleV1::Action(effect) => {
                rules.push(json!({ "action": action, "resource": "*", "effect": effect }))
            }
            PermissionRuleV1::Entries(entries) => {
                for (resource, effect) in entries {
                    rules.push(json!({ "action": action, "resource": resource, "effect": effect }));
                }
            }
        }
    }
    if rules.is_empty() { None } else { Some(Value::Array(rules)) }
}

// ---------------------------------------------------------------------------------------------
// Agent (`v1/config/agent.ts`)
// ---------------------------------------------------------------------------------------------

/// Decoded (and decode-time normalized) `ConfigAgentV1.Info`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AgentV1 {
    pub model: Option<String>,
    pub variant: Option<String>,
    pub temperature: Option<Number>,
    pub top_p: Option<Number>,
    pub prompt: Option<String>,
    pub disable: Option<bool>,
    pub description: Option<String>,
    pub mode: Option<String>,
    pub hidden: Option<bool>,
    /// `options` plus unknown keys moved in by decode normalization.
    pub options: Map<String, Value>,
    pub color: Option<String>,
    pub steps: Option<u64>,
    /// Merged `tools` + `permission` entries.
    pub permission: PermissionV1,
}

/// Declared field names of the v1 agent schema.
pub const AGENT_V1_FIELDS: &[&str] = &[
    "model",
    "variant",
    "temperature",
    "top_p",
    "prompt",
    "tools",
    "disable",
    "description",
    "mode",
    "hidden",
    "options",
    "color",
    "steps",
    "maxSteps",
    "permission",
];

fn decode_v1_color(value: &Value) -> Option<String> {
    let s = value.as_str()?;
    if crate::schema::is_hex_color(s)
        || ["primary", "secondary", "accent", "success", "warning", "error", "info"].contains(&s)
    {
        Some(s.to_string())
    } else {
        None
    }
}

/// `Entries(Schema.Boolean)`: a non-array object whose values are booleans.
fn decode_bool_entries(value: &Value) -> Option<Vec<(String, bool)>> {
    record(value)?.iter().map(|(k, v)| Some((k.clone(), v.as_bool()?))).collect()
}

pub fn decode_agent(value: &Value) -> Option<AgentV1> {
    let o = record(value)?;
    let model: Option<String> = opt(o, "model")?;
    let variant: Option<String> = opt(o, "variant")?;
    let temperature = opt_with(o, "temperature", finite)?;
    let top_p = opt_with(o, "top_p", finite)?;
    let prompt: Option<String> = opt(o, "prompt")?;
    let tools = opt_with(o, "tools", decode_bool_entries)?;
    let disable: Option<bool> = opt(o, "disable")?;
    let description: Option<String> = opt(o, "description")?;
    let mode = opt_with(o, "mode", |v| literal(v, &["subagent", "primary", "all"]).map(str::to_string))?;
    let hidden: Option<bool> = opt(o, "hidden")?;
    let options: Option<Map<String, Value>> = opt_with(o, "options", |v| record(v).cloned())?;
    let color = opt_with(o, "color", decode_v1_color)?;
    let steps = opt_with(o, "steps", positive_int)?;
    let max_steps = opt_with(o, "maxSteps", positive_int)?;
    let permission = opt_with(o, "permission", decode_permission)?;

    // normalize: unknown keys (not fields ∪ name) move into options.
    let mut options = options.unwrap_or_default();
    for (key, value) in o {
        if key == "name" || AGENT_V1_FIELDS.contains(&key.as_str()) {
            continue;
        }
        options.insert(key.clone(), value.clone());
    }
    // A Map keeps each key's first position when a later entry overrides it.
    let mut merged: IndexMap<String, PermissionRuleV1> = IndexMap::new();
    for (tool, enabled) in tools.unwrap_or_default() {
        let action = if enabled { "allow" } else { "deny" };
        let key = if tool == "write" || tool == "patch" { "edit".to_string() } else { tool };
        merged.insert(key, PermissionRuleV1::Action(action.into()));
    }
    for (key, rule) in permission.unwrap_or_default() {
        merged.insert(key, rule);
    }
    Some(AgentV1 {
        model,
        variant,
        temperature,
        top_p,
        prompt,
        disable,
        description,
        mode,
        hidden,
        options,
        color,
        steps: steps.or(max_steps),
        permission: merged.into_iter().collect(),
    })
}

/// `ConfigMigrateV1.migrateAgent` → canonical encoded `ConfigAgent.Info`.
pub fn migrate_agent(info: &AgentV1) -> Value {
    let mut body = info.options.clone();
    if let Some(t) = &info.temperature {
        body.insert("temperature".into(), Value::Number(t.clone()));
    }
    if let Some(t) = &info.top_p {
        body.insert("top_p".into(), Value::Number(t.clone()));
    }
    let mut out = Map::new();
    insert_opt(&mut out, "model", model_selection(info.model.as_deref(), info.variant.as_deref()));
    if !body.is_empty() {
        out.insert("request".into(), json!({ "body": body }));
    }
    insert_opt(&mut out, "system", info.prompt.clone().map(Value::String));
    insert_opt(&mut out, "description", info.description.clone().map(Value::String));
    insert_opt(&mut out, "mode", info.mode.clone().map(Value::String));
    insert_opt(&mut out, "hidden", info.hidden.map(Value::Bool));
    insert_opt(
        &mut out,
        "color",
        info.color.as_ref().map(|c| Value::String(if c.starts_with('#') { c.clone() } else { "#aaaaaa".into() })),
    );
    insert_opt(&mut out, "steps", info.steps.map(|s| Value::Number(s.into())));
    insert_opt(&mut out, "disabled", info.disable.map(Value::Bool));
    insert_opt(&mut out, "permissions", permission_rules(&info.permission));
    let value = Value::Object(out);
    crate::schema::Agent::decode(&value).map(|a| crate::decode::encode(&a)).unwrap_or(value)
}

// ---------------------------------------------------------------------------------------------
// Command (`v1/config/command.ts`)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct CommandV1 {
    pub template: String,
    pub description: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub variant: Option<String>,
    pub subtask: Option<bool>,
}

pub fn decode_command(value: &Value) -> Option<CommandV1> {
    let o = record(value)?;
    Some(CommandV1 {
        template: req(o, "template")?,
        description: opt(o, "description")?,
        agent: opt(o, "agent")?,
        model: opt(o, "model")?,
        variant: opt(o, "variant")?,
        subtask: opt(o, "subtask")?,
    })
}

/// `ConfigMigrateV1.migrateCommand` (not yet canonicalized).
pub fn migrate_command(command: &CommandV1) -> Value {
    let mut out = Map::new();
    out.insert("template".into(), Value::String(command.template.clone()));
    insert_opt(&mut out, "description", command.description.clone().map(Value::String));
    insert_opt(&mut out, "agent", command.agent.clone().map(Value::String));
    insert_opt(&mut out, "model", model_selection(command.model.as_deref(), command.variant.as_deref()));
    insert_opt(&mut out, "subagent", command.subtask.map(Value::Bool));
    Value::Object(out)
}

// ---------------------------------------------------------------------------------------------
// Plugin (`v1/config/plugin.ts`)
// ---------------------------------------------------------------------------------------------

/// `string | [string, Record<string, unknown>]`
pub fn decode_plugin_spec(value: &Value) -> Option<Value> {
    if value.is_string() {
        return Some(value.clone());
    }
    let items = value.as_array()?;
    if items.len() != 2 {
        return None;
    }
    let package = items[0].as_str()?;
    let options = record(&items[1])?;
    Some(json!({ "package": package, "options": options }))
}

// ---------------------------------------------------------------------------------------------
// MCP (`v1/config/mcp.ts`)
// ---------------------------------------------------------------------------------------------

pub fn decode_mcp(value: &Value) -> Option<Map<String, Value>> {
    let o = record(value)?;
    match o.get("type")?.as_str()? {
        "local" => {
            let command: Vec<String> = req(o, "command")?;
            let cwd: Option<String> = opt(o, "cwd")?;
            let environment: Option<IndexMap<String, String>> = opt(o, "environment")?;
            let enabled: Option<bool> = opt(o, "enabled")?;
            let timeout = opt_with(o, "timeout", positive_int)?;
            let mut out = Map::new();
            out.insert("type".into(), json!("local"));
            out.insert("command".into(), json!(command));
            insert_opt(&mut out, "cwd", cwd.map(Value::String));
            insert_opt(&mut out, "environment", environment.map(|e| json!(e)));
            insert_opt(&mut out, "enabled", enabled.map(Value::Bool));
            insert_opt(&mut out, "timeout", timeout.map(|t| json!(t)));
            Some(out)
        }
        "remote" => {
            let url: String = req(o, "url")?;
            let enabled: Option<bool> = opt(o, "enabled")?;
            let headers: Option<IndexMap<String, String>> = opt(o, "headers")?;
            let oauth = opt_with(o, "oauth", |v| {
                if v == &Value::Bool(false) {
                    return Some(Value::Bool(false));
                }
                let a = record(v)?;
                let mut out = Map::new();
                insert_opt(&mut out, "clientId", opt::<String>(a, "clientId")?.map(Value::String));
                insert_opt(&mut out, "clientSecret", opt::<String>(a, "clientSecret")?.map(Value::String));
                insert_opt(&mut out, "scope", opt::<String>(a, "scope")?.map(Value::String));
                insert_opt(
                    &mut out,
                    "callbackPort",
                    opt_with(a, "callbackPort", crate::schema::port)?.map(|p| json!(p)),
                );
                insert_opt(&mut out, "redirectUri", opt::<String>(a, "redirectUri")?.map(Value::String));
                Some(Value::Object(out))
            })?;
            let timeout = opt_with(o, "timeout", positive_int)?;
            let mut out = Map::new();
            out.insert("type".into(), json!("remote"));
            out.insert("url".into(), json!(url));
            insert_opt(&mut out, "enabled", enabled.map(Value::Bool));
            insert_opt(&mut out, "headers", headers.map(|h| json!(h)));
            insert_opt(&mut out, "oauth", oauth);
            insert_opt(&mut out, "timeout", timeout.map(|t| json!(t)));
            Some(out)
        }
        _ => None,
    }
}

/// `ConfigMigrateV1.migrateMcp` (not yet canonicalized).
pub fn migrate_mcp(info: &Map<String, Value>) -> Value {
    let disabled = info.get("enabled").and_then(Value::as_bool).map(|e| Value::Bool(!e));
    let timeout = info.get("timeout").map(|t| json!({ "catalog": t, "execution": t }));
    let mut out = Map::new();
    out.insert("type".into(), info["type"].clone());
    if info["type"] == "local" {
        out.insert("command".into(), info["command"].clone());
        insert_opt(&mut out, "cwd", info.get("cwd").cloned());
        insert_opt(&mut out, "environment", info.get("environment").cloned());
        insert_opt(&mut out, "disabled", disabled);
        insert_opt(&mut out, "timeout", timeout);
        return Value::Object(out);
    }
    out.insert("url".into(), info["url"].clone());
    insert_opt(&mut out, "headers", info.get("headers").cloned());
    match info.get("oauth") {
        Some(Value::Object(a)) => {
            let mut oauth = Map::new();
            insert_opt(&mut oauth, "client_id", a.get("clientId").cloned());
            insert_opt(&mut oauth, "client_secret", a.get("clientSecret").cloned());
            insert_opt(&mut oauth, "scope", a.get("scope").cloned());
            insert_opt(&mut oauth, "callback_port", a.get("callbackPort").cloned());
            insert_opt(&mut oauth, "redirect_uri", a.get("redirectUri").cloned());
            out.insert("oauth".into(), Value::Object(oauth));
        }
        Some(Value::Bool(false)) => {
            out.insert("oauth".into(), Value::Bool(false));
        }
        _ => {}
    }
    insert_opt(&mut out, "disabled", disabled);
    insert_opt(&mut out, "timeout", timeout);
    Value::Object(out)
}

// ---------------------------------------------------------------------------------------------
// Provider (`v1/config/provider.ts`, `provider-options.ts`, `migrate.ts`)
// ---------------------------------------------------------------------------------------------

/// Decoded v1 provider. Records keep the decoded key order (declared fields first, then rest).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProviderV1 {
    pub api: Option<String>,
    pub name: Option<String>,
    pub env: Option<Vec<String>>,
    pub npm: Option<String>,
    pub options: Option<Map<String, Value>>,
    pub models: Option<IndexMap<String, ModelV1>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CostV1 {
    pub input: Number,
    pub output: Number,
    pub cache_read: Option<Number>,
    pub cache_write: Option<Number>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModelV1 {
    pub id: Option<String>,
    pub name: Option<String>,
    pub family: Option<String>,
    pub tool_call: Option<bool>,
    pub interleaved: Option<Value>,
    pub cost: Option<(CostV1, Option<CostV1>)>,
    pub limit: Option<(Number, Option<Number>, Number)>,
    pub modalities_input: Option<Vec<String>>,
    pub modalities_output: Option<Vec<String>>,
    pub status: Option<String>,
    pub provider_npm: Option<String>,
    pub provider_api: Option<String>,
    pub options: Option<Map<String, Value>>,
    pub headers: Option<IndexMap<String, String>>,
    pub variants: Option<IndexMap<String, Map<String, Value>>>,
}

fn decode_cost_v1(value: &Value) -> Option<CostV1> {
    let o = record(value)?;
    Some(CostV1 {
        input: crate::decode::req_with(o, "input", finite)?,
        output: crate::decode::req_with(o, "output", finite)?,
        cache_read: opt_with(o, "cache_read", finite)?,
        cache_write: opt_with(o, "cache_write", finite)?,
    })
}

const MODALITIES: &[&str] = &["text", "audio", "image", "video", "pdf"];

fn decode_modalities(value: &Value) -> Option<Vec<String>> {
    value.as_array()?.iter().map(|v| literal(v, MODALITIES).map(str::to_string)).collect()
}

/// Struct-with-rest decoding: declared fields (in schema order) then rest keys in input order.
fn reorder(o: &Object, declared: &[&str]) -> Map<String, Value> {
    let mut out = Map::new();
    for key in declared {
        if let Some(v) = o.get(*key) {
            out.insert((*key).to_string(), v.clone());
        }
    }
    for (k, v) in o {
        if !declared.contains(&k.as_str()) {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

pub fn decode_model_v1(value: &Value) -> Option<ModelV1> {
    let o = record(value)?;
    let _: Option<String> = opt(o, "release_date")?;
    let _: Option<bool> = opt(o, "attachment")?;
    let _: Option<bool> = opt(o, "reasoning")?;
    let _: Option<bool> = opt(o, "temperature")?;
    let _: Option<bool> = opt(o, "experimental")?;
    let interleaved = opt_with(o, "interleaved", |v| match v {
        Value::Bool(_) | Value::String(_) => Some(v.clone()),
        Value::Object(m) => {
            m.get("field")?.as_str()?;
            Some(json!({ "field": m["field"] }))
        }
        _ => None,
    })?;
    let cost = opt_with(o, "cost", |v| {
        let c = record(v)?;
        let base = decode_cost_v1(v)?;
        let over = opt_with(c, "context_over_200k", decode_cost_v1)?;
        Some((base, over))
    })?;
    let limit = opt_with(o, "limit", |v| {
        let l = record(v)?;
        Some((
            crate::decode::req_with(l, "context", finite)?,
            opt_with(l, "input", finite)?,
            crate::decode::req_with(l, "output", finite)?,
        ))
    })?;
    let (modalities_input, modalities_output) = match o.get("modalities") {
        None => (None, None),
        Some(m) => {
            let m = record(m)?;
            (opt_with(m, "input", decode_modalities)?, opt_with(m, "output", decode_modalities)?)
        }
    };
    let status =
        opt_with(o, "status", |v| literal(v, &["alpha", "beta", "deprecated", "active"]).map(str::to_string))?;
    let (provider_npm, provider_api) = match o.get("provider") {
        None => (None, None),
        Some(p) => {
            let p = record(p)?;
            (opt(p, "npm")?, opt(p, "api")?)
        }
    };
    let variants = opt_with(o, "variants", |v| {
        record(v)?
            .iter()
            .map(|(k, item)| {
                let i = record(item)?;
                let _: Option<bool> = opt(i, "disabled")?;
                Some((k.clone(), reorder(i, &["disabled"])))
            })
            .collect::<Option<IndexMap<_, _>>>()
    })?;
    Some(ModelV1 {
        id: opt(o, "id")?,
        name: opt(o, "name")?,
        family: opt(o, "family")?,
        tool_call: opt(o, "tool_call")?,
        interleaved,
        cost,
        limit,
        modalities_input,
        modalities_output,
        status,
        provider_npm,
        provider_api,
        options: opt_with(o, "options", |v| record(v).cloned())?,
        headers: opt(o, "headers")?,
        variants,
    })
}

const PROVIDER_OPTION_FIELDS: &[&str] =
    &["apiKey", "baseURL", "enterpriseUrl", "setCacheKey", "timeout", "headerTimeout", "chunkTimeout"];

fn positive_or_false(v: &Value) -> Option<Value> {
    if v == &Value::Bool(false) {
        return Some(v.clone());
    }
    positive_int(v).map(|n| json!(n))
}

pub fn decode_provider(value: &Value) -> Option<ProviderV1> {
    let o = record(value)?;
    let _: Option<String> = opt(o, "id")?;
    let _: Option<Vec<String>> = opt(o, "whitelist")?;
    let _: Option<Vec<String>> = opt(o, "blacklist")?;
    let options = opt_with(o, "options", |v| {
        let m = record(v)?;
        let _: Option<String> = opt(m, "apiKey")?;
        let _: Option<String> = opt(m, "baseURL")?;
        let _: Option<String> = opt(m, "enterpriseUrl")?;
        let _: Option<bool> = opt(m, "setCacheKey")?;
        opt_with(m, "timeout", positive_or_false)?;
        opt_with(m, "headerTimeout", positive_or_false)?;
        opt_with(m, "chunkTimeout", positive_int)?;
        Some(reorder(m, PROVIDER_OPTION_FIELDS))
    })?;
    Some(ProviderV1 {
        api: opt(o, "api")?,
        name: opt(o, "name")?,
        env: opt(o, "env")?,
        npm: opt(o, "npm")?,
        options,
        models: opt_with(o, "models", |v| {
            record(v)?.iter().map(|(k, m)| Some((k.clone(), decode_model_v1(m)?))).collect()
        })?,
    })
}

fn is_record(v: &Value) -> bool {
    v.is_object()
}

/// `ConfigProviderOptionsV1.provider`
fn provider_options(options: &Map<String, Value>) -> (Map<String, Value>, Option<Value>, Option<Value>) {
    let settings: Map<String, Value> =
        options.iter().filter(|(k, _)| *k != "headers" && *k != "body").map(|(k, v)| (k.clone(), v.clone())).collect();
    let headers = options.get("headers").filter(|h| is_record(h)).map(|h| {
        Value::Object(
            h.as_object()
                .unwrap()
                .iter()
                .filter(|(_, v)| v.is_string())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )
    });
    let body = options.get("body").filter(|b| is_record(b)).cloned();
    (settings, headers, body)
}

fn migrate_model(info: &ModelV1) -> Map<String, Value> {
    let binding_off = |key: &str| {
        info.options
            .as_ref()
            .and_then(|o| o.get(key))
            .and_then(|v| v.as_object())
            .and_then(|v| v.get("blockBinding"))
            == Some(&Value::Bool(false))
    };
    let disable_binding = binding_off("thinking") || binding_off("reasoningConfig");
    let mut options = info.options.clone();
    if let Some(options) = options.as_mut()
        && disable_binding
    {
        for key in ["thinking", "reasoningConfig"] {
            let Some(Value::Object(inner)) = options.get(key) else { continue };
            if inner.get("blockBinding") != Some(&Value::Bool(false)) {
                continue;
            }
            let mut rest = inner.clone();
            rest.remove("blockBinding");
            if !rest.is_empty() {
                options.insert(key.into(), Value::Object(rest));
                continue;
            }
            options.shift_remove(key);
        }
    }
    let settings = options;
    // Model.compatibility(interleaved)
    let compatibility = match &info.interleaved {
        Some(Value::String(s)) => Some(json!({ "reasoningField": s })),
        Some(Value::Object(m)) => m.get("field").and_then(Value::as_str).map(|f| json!({ "reasoningField": f })),
        _ => None,
    };
    let compatibility = if disable_binding {
        let mut c = compatibility.and_then(|c| c.as_object().cloned()).unwrap_or_default();
        c.insert("supportsThinkingBlockBinding".into(), Value::Bool(false));
        Some(Value::Object(c))
    } else {
        compatibility
    };
    let cost = info.cost.as_ref().map(|(base, over)| {
        let tier = |c: &CostV1| {
            let mut cache = Map::new();
            insert_opt(&mut cache, "read", c.cache_read.clone().map(Value::Number));
            insert_opt(&mut cache, "write", c.cache_write.clone().map(Value::Number));
            (c.input.clone(), c.output.clone(), cache)
        };
        let (i, o, cache) = tier(base);
        let mut list = vec![json!({ "input": i, "output": o, "cache": cache })];
        if let Some(over) = over {
            let (i, o, cache) = tier(over);
            list.push(json!({ "tier": { "type": "context", "size": 200000 }, "input": i, "output": o, "cache": cache }));
        }
        Value::Array(list)
    });
    let capabilities =
        if info.tool_call.is_some() || info.modalities_input.is_some() || info.modalities_output.is_some() {
            Some(json!({
                "tools": info.tool_call.unwrap_or(true),
                "input": info.modalities_input.clone().unwrap_or_else(|| vec!["text".into(), "image".into()]),
                "output": info.modalities_output.clone().unwrap_or_else(|| vec!["text".into()]),
            }))
        } else {
            None
        };
    let settings = match (&info.provider_api, settings) {
        (Some(api), s) => {
            let mut s = s.unwrap_or_default();
            s.insert("baseURL".into(), Value::String(api.clone()));
            Some(Value::Object(s))
        }
        (None, s) => s.map(Value::Object),
    };
    let mut out = Map::new();
    insert_opt(&mut out, "modelID", info.id.clone().map(Value::String));
    insert_opt(&mut out, "family", info.family.clone().map(Value::String));
    insert_opt(&mut out, "name", info.name.clone().map(Value::String));
    insert_opt(&mut out, "compatibility", compatibility);
    insert_opt(&mut out, "package", info.provider_npm.as_deref().filter(|n| !n.is_empty()).map(|n| json!(aisdk(n))));
    insert_opt(&mut out, "settings", settings);
    insert_opt(&mut out, "capabilities", capabilities);
    insert_opt(&mut out, "headers", info.headers.as_ref().map(|h| json!(h)));
    insert_opt(
        &mut out,
        "variants",
        info.variants.as_ref().map(|v| {
            Value::Array(v.iter().map(|(id, options)| json!({ "id": id, "settings": options })).collect())
        }),
    );
    insert_opt(&mut out, "cost", cost);
    if info.status.as_deref() == Some("deprecated") {
        out.insert("disabled".into(), Value::Bool(true));
    }
    if let Some((context, input, output)) = &info.limit {
        let mut limit = Map::new();
        limit.insert("context".into(), json!(trunc_int(context)));
        insert_opt(&mut limit, "input", input.as_ref().map(|i| json!(trunc_int(i))));
        limit.insert("output".into(), json!(trunc_int(output)));
        out.insert("limit".into(), Value::Object(limit));
    }
    out
}

/// `Math.max(MIN_SAFE_INTEGER, Math.min(MAX_SAFE_INTEGER, Math.trunc(value)))`
fn trunc_int(n: &Number) -> i64 {
    const MAX: f64 = 9_007_199_254_740_991.0;
    if let Some(i) = n.as_i64() {
        return i.clamp(-(MAX as i64), MAX as i64);
    }
    let f = n.as_f64().unwrap_or(0.0).trunc().clamp(-MAX, MAX);
    f as i64
}

fn migrate_standard_provider(info: &ProviderV1) -> Map<String, Value> {
    let (settings, headers, body) = provider_options(info.options.as_ref().unwrap_or(&Map::new()));
    let mut out = Map::new();
    insert_opt(&mut out, "name", info.name.clone().map(Value::String));
    insert_opt(&mut out, "env", info.env.as_ref().map(|e| json!(e)));
    insert_opt(&mut out, "package", info.npm.as_deref().filter(|n| !n.is_empty()).map(|n| json!(aisdk(n))));
    let settings = match (&info.api, &info.options) {
        (Some(api), _) if !api.is_empty() => {
            let mut s = settings;
            s.insert("baseURL".into(), Value::String(api.clone()));
            Some(Value::Object(s))
        }
        (_, Some(_)) => Some(Value::Object(settings)),
        _ => None,
    };
    insert_opt(&mut out, "settings", settings);
    if info.options.is_some() {
        insert_opt(&mut out, "headers", headers);
        insert_opt(&mut out, "body", body);
    }
    insert_opt(
        &mut out,
        "models",
        info.models.as_ref().map(|m| {
            Value::Object(m.iter().map(|(name, model)| (name.clone(), Value::Object(migrate_model(model)))).collect())
        }),
    );
    out
}

/// `ConfigMigrateV1.migrateProvider` (not yet canonicalized).
pub fn migrate_provider(source_id: &str, info: &ProviderV1) -> Value {
    let mut migrated = migrate_standard_provider(info);
    match source_id {
        "azure-cognitive-services" => {
            if let Some(Value::Array(env)) = migrated.get_mut("env") {
                env.retain(|n| n != "AZURE_COGNITIVE_SERVICES_RESOURCE_NAME");
            }
            if info.npm.as_deref() == Some("@ai-sdk/openai-compatible") && info.api.as_deref().unwrap_or("").is_empty()
            {
                let mut settings = migrated.get("settings").and_then(Value::as_object).cloned().unwrap_or_default();
                settings.insert(
                    "baseURL".into(),
                    json!("https://${AZURE_COGNITIVE_SERVICES_RESOURCE_NAME}.cognitiveservices.azure.com/openai"),
                );
                migrated.insert("settings".into(), Value::Object(settings));
            }
        }
        "google-vertex-anthropic" => {
            let package = migrated
                .shift_remove("package")
                .unwrap_or_else(|| json!(aisdk("@ai-sdk/google-vertex/anthropic")));
            if let Some(Value::Object(models)) = migrated.get_mut("models") {
                for model in models.values_mut() {
                    if let Value::Object(m) = model
                        && !m.get("package").is_some_and(|p| p.as_str().is_some_and(|s| !s.is_empty()))
                    {
                        m.insert("package".into(), package.clone());
                    }
                }
            }
        }
        _ => {}
    }
    Value::Object(migrated)
}

#[allow(dead_code)]
fn _int_unused(v: &Value) -> Option<i64> {
    int(v)
}
