//! OpenAI Responses (`protocols/openai-responses.ts`): namespaces, hosted tools, image generation,
//! effort updates, context management. WebSocket transport is not ported (HTTP SSE only).

use super::open_responses::{self as base, finalize, Adapter, HostedTool, OpenResponses};
use super::options;
use super::shared::{flatten_tools, match_tool_choice, ToolChoiceMode};
use super::{Protocol, StreamParser};
use crate::compile::resolve_effort_updates;
use crate::error::AIError;
use crate::json::Object;
use crate::route::framing::Framing;
use crate::route::{Endpoint, Route};
use crate::types::*;
use base64::Engine;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use std::sync::Arc;

pub const ADAPTER: &str = "openai-responses";
pub const NAME: &str = "OpenAI Responses";
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const WEBSOCKET_PROTOCOL_HEADER: &str = "responses_websockets=2026-02-06";

fn input_action(item: &Value) -> Value {
    item.get("action").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!({}))
}
fn input_queries(item: &Value) -> Value {
    json!({"queries": item.get("queries").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!([]))})
}
fn input_code(item: &Value) -> Value {
    let mut o = Object::new();
    if let Some(c) = item.get("code") {
        o.insert("code".into(), c.clone());
    }
    if let Some(c) = item.get("container_id") {
        o.insert("container_id".into(), c.clone());
    }
    Value::Object(o)
}
fn input_empty(_: &Value) -> Value {
    json!({})
}
fn input_mcp(item: &Value) -> Value {
    let mut o = Object::new();
    for k in ["server_label", "name", "arguments"] {
        if let Some(v) = item.get(k) {
            o.insert(k.into(), v.clone());
        }
    }
    Value::Object(o)
}

fn image_result(route: &str, item: &Value) -> Result<ToolResultValue, AIError> {
    if let Some(result) = item.get("result").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        base64::engine::general_purpose::STANDARD
            .decode(result)
            .map_err(|_| AIError::invalid_output(route, "OpenAI Responses returned invalid image base64"))?;
        let format = item.get("output_format").and_then(Value::as_str).unwrap_or("png");
        return Ok(ToolResultValue::Content(vec![ToolContent::File {
            uri: format!("data:image/{format};base64,{result}"),
            mime: format!("image/{format}"),
            name: None,
        }]));
    }
    Ok(match item.get("error") {
        Some(e) if !e.is_null() => ToolResultValue::Error(e.clone()),
        _ => ToolResultValue::Json(item.clone()),
    })
}

fn hosted(item_type: &'static str, name: &'static str, input: fn(&Value) -> Value) -> HostedTool {
    HostedTool { item_type, name, input, result: None }
}

fn restore(item: &Value) -> bool {
    let id_ok = item.get("id").map(Value::is_string).unwrap_or(false);
    id_ok && matches!(item.get("type").and_then(Value::as_str), Some("computer_call" | "web_search_preview_call" | "image_generation_call"))
}

pub fn adapter() -> Adapter {
    Adapter {
        id: ADAPTER,
        name: NAME,
        native_tool: None,
        restore_hosted: Some(restore),
        hosted_tools: vec![
            hosted("web_search_call", "web_search", input_action),
            hosted("web_search_preview_call", "web_search_preview", input_action),
            hosted("file_search_call", "file_search", input_queries),
            hosted("code_interpreter_call", "code_interpreter", input_code),
            hosted("computer_call", "computer_use", input_action),
            HostedTool { item_type: "image_generation_call", name: "image_generation", input: input_empty, result: Some(image_result) },
            hosted("mcp_call", "mcp", input_mcp),
        ],
        reasoning_text_delta: true,
        meta: false,
    }
}

static GPT_MAJOR: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)(?:^|/)gpt-(\d+)(?:\.\d+)?(?:-|$)").unwrap());
static IMAGE_SIZE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(\d+)x(\d+)$").unwrap());

pub fn supports_effort_updates(request: &LLMRequest) -> bool {
    if request.po("contextManagement").is_some() {
        return false;
    }
    let pro = request
        .http
        .as_ref()
        .and_then(|h| h.body.as_ref())
        .and_then(|b| b.get("reasoning"))
        .and_then(|r| r.get("mode"))
        .and_then(Value::as_str)
        == Some("pro");
    if pro {
        return false;
    }
    if let Some(v) = request.model.compatibility.as_ref().and_then(|c| c.supports_effort_updates) {
        return v;
    }
    GPT_MAJOR.captures(&request.model.id).and_then(|c| c[1].parse::<u32>().ok()).map(|m| m >= 6).unwrap_or(false)
}

fn native_image_input(tool: &ToolDefinition) -> Option<&Object> {
    let native = tool.native.as_ref()?.get("openai")?.as_object()?;
    if native.get("type").and_then(Value::as_str) == Some("image_generation") { Some(native) } else { None }
}

/// Validate and order the image generation tool (`OpenAIResponsesImageGenerationTool`).
fn image_tool(native: &Object) -> Option<Value> {
    let lit = |k: &str, allowed: &[&str]| native.get(k).map(|v| v.as_str().map(|s| allowed.contains(&s)).unwrap_or(false)).unwrap_or(true);
    let int_in = |k: &str, min: i64, max: i64| {
        native.get(k).map(|v| v.as_f64().map(|f| f.fract() == 0.0 && f as i64 >= min && f as i64 <= max).unwrap_or(false)).unwrap_or(true)
    };
    let size_ok = native
        .get("size")
        .map(|v| match v.as_str() {
            Some("auto") => true,
            Some(s) => IMAGE_SIZE.captures(s).map(|c| c[1].parse::<u64>().unwrap_or(0) > 0 && c[2].parse::<u64>().unwrap_or(0) > 0).unwrap_or(false),
            None => false,
        })
        .unwrap_or(true);
    let ok = lit("action", &["auto", "generate", "edit"])
        && lit("background", &["auto", "opaque", "transparent"])
        && lit("input_fidelity", &["low", "high"])
        && int_in("output_compression", 0, 100)
        && lit("output_format", &["png", "jpeg", "webp"])
        && int_in("partial_images", 0, i64::MAX)
        && lit("quality", &["auto", "low", "medium", "high"])
        && size_ok;
    if !ok {
        return None;
    }
    let mut out = Object::new();
    for k in ["type", "action", "background", "input_fidelity", "output_compression", "output_format", "partial_images", "quality", "size"] {
        if let Some(v) = native.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    Some(Value::Object(out))
}

fn lower_tool(tool: &ToolDefinition) -> Result<Value, AIError> {
    if let Some(native) = native_image_input(tool) {
        return image_tool(native).ok_or_else(|| AIError::invalid_request("OpenAI Responses image generation tool options are invalid"));
    }
    base::lower_tool(NAME, tool)
}

pub fn lower_tools(tools: &[ToolEntry]) -> Result<Vec<Value>, AIError> {
    tools
        .iter()
        .map(|entry| match entry {
            ToolEntry::Tool(t) => lower_tool(t),
            ToolEntry::Namespace(ns) => Ok(json!({
                "type": "namespace",
                "name": ns.name,
                "description": ns.description.clone().unwrap_or_else(|| format!("Tools in the {} namespace.", ns.name)),
                "tools": flatten_tools(&ns.tools).iter().map(|leaf| base::lower_tool(NAME, leaf)).collect::<Result<Vec<_>, _>>()?,
            })),
        })
        .collect()
}

fn lower_tool_choice(choice: &ToolChoice, tools: &[ToolEntry]) -> Result<Value, AIError> {
    Ok(match match_tool_choice(NAME, choice)? {
        ToolChoiceMode::Auto => json!("auto"),
        ToolChoiceMode::None => json!("none"),
        ToolChoiceMode::Required => json!("required"),
        ToolChoiceMode::Tool(name) => {
            let image = tools.iter().any(|t| matches!(t, ToolEntry::Tool(d) if d.name == name && native_image_input(d).and_then(image_tool).is_some()));
            if image { json!({"type": "image_generation"}) } else { json!({"type": "function", "name": name}) }
        }
    })
}

fn decode_context_management(value: Option<&Value>) -> Result<Option<Value>, AIError> {
    let Some(value) = value else { return Ok(None) };
    let err = || AIError::invalid_request("Invalid OpenAI Responses provider option `contextManagement`");
    let items = value.as_array().ok_or_else(err)?;
    let mut out = vec![];
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("compaction") {
            return Err(err());
        }
        let mut o = Object::new();
        o.insert("type".into(), json!("compaction"));
        if let Some(t) = item.get("compactThreshold") {
            let ok = t.as_f64().map(|f| f.fract() == 0.0 && f > 0.0).unwrap_or(false);
            if !ok {
                return Err(err());
            }
            o.insert("compact_threshold".into(), t.clone());
        }
        out.push(Value::Object(o));
    }
    Ok(Some(Value::Array(out)))
}

/// Body fields (`fromRequest`).
pub fn body_fields(request: &LLMRequest) -> Result<base::Fields, AIError> {
    let management = decode_context_management(request.po("contextManagement"))?;
    let mut options = options::resolve(request);
    let (messages, effort) = resolve_effort_updates(request, options.reasoning_effort.clone());
    let mut updated = request.clone();
    updated.messages = messages;
    let adapter = adapter();
    let mut f = base::lower_conversation(&updated, &adapter)?;
    options.reasoning_effort = effort;
    base::lower_generation(request, &options, &mut f);
    f.insert("context_management".into(), management);
    if request.tools.is_empty() {
        f.insert("tools".into(), None);
        f.insert("tool_choice".into(), None);
    } else {
        f.insert("tools".into(), Some(Value::Array(lower_tools(&request.tools)?)));
        let choice = match base::allowed_tool_choice(&options::resolve(request)) {
            Some(c) => Some(c),
            None => request.tool_choice.as_ref().map(|c| lower_tool_choice(c, &request.tools)).transpose()?,
        };
        f.insert("tool_choice".into(), choice);
    }
    Ok(f)
}

pub struct OpenAIResponses;

impl Protocol for OpenAIResponses {
    fn id(&self) -> &str {
        ADAPTER
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        Ok(finalize(body_fields(request)?))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(base::ResponsesParser::new(request, &adapter()))
    }
    fn supports_effort_updates(&self, request: &LLMRequest) -> bool {
        supports_effort_updates(request)
    }
}

pub fn protocol() -> Arc<dyn Protocol> {
    Arc::new(OpenAIResponses)
}

/// `openai-responses` route.
pub fn route() -> Route {
    Route::new(ADAPTER, protocol(), Endpoint::new(Some(DEFAULT_BASE_URL), base::PATH), Framing::Sse)
        .initial_provider("openai", Some("openai"))
        .with_provider_options(base::default_options())
}

#[allow(dead_code)]
fn _unused(_: OpenResponses) {}
