//! Shared OpenAI/Responses provider options (`protocols/utils/open-responses-options.ts`): each key is
//! decoded leniently (a malformed value is dropped individually).

use crate::json::Object;
use crate::types::LLMRequest;
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AllowedTools {
    pub tool_names: Vec<String>,
    pub mode: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResponsesOptions {
    pub store: Option<bool>,
    pub metadata: Option<Object>,
    pub safety_identifier: Option<String>,
    pub include_obfuscation: Option<Option<bool>>,
    pub top_logprobs: Option<i64>,
    pub reasoning_effort: Option<String>,
    pub reasoning_summary: Option<String>,
    pub include: Option<Vec<Value>>,
    pub text_verbosity: Option<String>,
    pub service_tier: Option<String>,
    pub truncation: Option<String>,
    pub allowed_tools: Option<AllowedTools>,
    pub max_tool_calls: Option<i64>,
    pub parallel_tool_calls: Option<bool>,
}

fn int(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
}

/// `OpenResponsesOptions.resolve(request)`.
pub fn resolve(request: &LLMRequest) -> ResponsesOptions {
    resolve_object(request.provider_options.as_ref())
}

pub fn resolve_object(po: Option<&Object>) -> ResponsesOptions {
    let mut out = ResponsesOptions::default();
    let Some(po) = po else { return out };
    let get = |k: &str| po.get(k).filter(|v| !v.is_null());
    out.store = get("store").and_then(Value::as_bool);
    out.metadata = get("metadata").and_then(Value::as_object).filter(|m| m.values().all(Value::is_string)).cloned();
    out.safety_identifier = get("safetyIdentifier").and_then(Value::as_str).map(String::from);
    if let Some(so) = get("streamOptions").and_then(Value::as_object) {
        match so.get("includeObfuscation") {
            None => out.include_obfuscation = Some(None),
            Some(Value::Bool(b)) => out.include_obfuscation = Some(Some(*b)),
            Some(_) => {}
        }
    }
    out.top_logprobs = get("topLogprobs").and_then(int).filter(|n| (0..=20).contains(n));
    out.reasoning_effort = get("reasoningEffort").and_then(Value::as_str).map(String::from);
    out.reasoning_summary =
        get("reasoningSummary").and_then(Value::as_str).filter(|s| matches!(*s, "auto" | "concise" | "detailed")).map(String::from);
    out.include = get("include")
        .and_then(Value::as_array)
        .filter(|a| a.iter().all(Value::is_string))
        .filter(|a| !a.is_empty())
        .cloned();
    out.text_verbosity = get("textVerbosity").and_then(Value::as_str).map(String::from);
    out.service_tier = get("serviceTier").and_then(Value::as_str).map(String::from);
    out.truncation = get("truncation").and_then(Value::as_str).filter(|s| matches!(*s, "auto" | "disabled")).map(String::from);
    if let Some(at) = get("allowedTools").and_then(Value::as_object) {
        let names = at.get("toolNames").and_then(Value::as_array).filter(|a| a.iter().all(Value::is_string));
        let mode = match at.get("mode") {
            None => Some("auto".to_string()),
            Some(Value::String(m)) if matches!(m.as_str(), "auto" | "none" | "required") => Some(m.clone()),
            _ => None,
        };
        if let (Some(names), Some(mode)) = (names, mode) {
            if !names.is_empty() {
                out.allowed_tools = Some(AllowedTools {
                    tool_names: names.iter().filter_map(|n| n.as_str().map(String::from)).collect(),
                    mode,
                });
            }
        }
    }
    out.max_tool_calls = get("maxToolCalls").and_then(int);
    out.parallel_tool_calls = get("parallelToolCalls").and_then(Value::as_bool);
    out
}

/// OpenAI provider default provider options (`providers/openai-options.ts`).
pub fn openai_default_provider_options(model_id: &str) -> Object {
    let mut out = Object::new();
    out.insert("store".into(), Value::Bool(false));
    let id = model_id.to_lowercase();
    if id.contains("gpt-5") && !id.contains("gpt-5-chat") && !id.contains("gpt-5-pro") {
        out.insert("reasoningEffort".into(), Value::String("medium".into()));
        out.insert("reasoningSummary".into(), Value::String("auto".into()));
        out.insert("include".into(), serde_json::json!(["reasoning.encrypted_content"]));
    }
    out
}
