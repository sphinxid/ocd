//! Responses variants: `xai-responses`, `alibaba-responses`, `meta-responses`
//! (`protocols/{xai,alibaba,meta}-responses.ts`).

use super::open_responses::{self as base, finalize, Adapter, HostedTool, ResponsesParser};
use super::{Protocol, StreamParser};
use crate::error::AIError;
use crate::json::{encode, Object};
use crate::types::*;
use base64::Engine;
use serde_json::{json, Value};
use std::sync::Arc;

fn action(item: &Value) -> Value {
    item.get("action").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!({}))
}
fn queries(item: &Value) -> Value {
    json!({"queries": item.get("queries").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!([]))})
}
fn code_container(item: &Value) -> Value {
    let mut o = Object::new();
    for k in ["code", "container_id"] {
        if let Some(v) = item.get(k) {
            o.insert(k.into(), v.clone());
        }
    }
    Value::Object(o)
}
fn code_only(item: &Value) -> Value {
    let mut o = Object::new();
    if let Some(v) = item.get("code") {
        o.insert("code".into(), v.clone());
    }
    Value::Object(o)
}
fn empty(_: &Value) -> Value {
    json!({})
}
fn mcp(item: &Value) -> Value {
    let mut o = Object::new();
    for k in ["server_label", "name", "arguments"] {
        if let Some(v) = item.get(k) {
            o.insert(k.into(), v.clone());
        }
    }
    Value::Object(o)
}
fn h(item_type: &'static str, name: &'static str, input: fn(&Value) -> Value) -> HostedTool {
    HostedTool { item_type, name, input, result: None }
}

// ---------------------------------------------------------------------------------------------
// xAI
// ---------------------------------------------------------------------------------------------

fn xai_restore(item: &Value) -> bool {
    item.get("id").map(Value::is_string).unwrap_or(false)
        && matches!(item.get("type").and_then(Value::as_str), Some("x_search_call" | "image_generation_call"))
}

pub fn xai_adapter() -> Adapter {
    Adapter {
        id: "xai-responses",
        name: "xAI Responses",
        native_tool: None,
        restore_hosted: Some(xai_restore),
        hosted_tools: vec![
            h("web_search_call", "web_search", action),
            h("x_search_call", "x_search", action),
            h("file_search_call", "file_search", queries),
            h("code_interpreter_call", "code_interpreter", code_container),
            h("image_generation_call", "image_generation", empty),
            h("mcp_call", "mcp", mcp),
        ],
        reasoning_text_delta: false,
        meta: false,
        lower_media: Some(xai_media),
    }
}

/// xAI takes video as an `input_file` (URL or data URL).
fn xai_media(part: &MediaPart) -> Option<Value> {
    if part.media.kind() != crate::media::MediaKind::Video {
        return None;
    }
    let filename = part.filename.clone().unwrap_or_else(|| "file".into());
    if let Some(url) = part.media.url_value() {
        return Some(json!({"type": "input_file", "filename": filename, "file_url": url}));
    }
    let inline = part.media.inline()?;
    Some(json!({"type": "input_file", "filename": filename, "file_data": inline.data_url}))
}

pub struct XaiResponses;

impl Protocol for XaiResponses {
    fn id(&self) -> &str {
        "xai-responses"
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        if request.po("contextManagement").is_some() {
            return Err(AIError::unsupported(
                "in-band-compaction",
                "xAI requires explicit compaction through LLMClient.compact; automatic context management is not supported",
                Some(&request.model.provider),
                Some(&request.model.route.id),
            ));
        }
        let min_p = match request.po("minP") {
            None => None,
            Some(v) if v.is_number() => Some(v.clone()),
            Some(_) => return Err(AIError::invalid_request("Expected number | undefined\n  at [\"minP\"]")),
        };
        let mut f = base::body_fields(request, &xai_adapter())?;
        // xAI Responses does not support penalties (they are ignored), but does support top_k.
        f.insert("presence_penalty".into(), None);
        f.insert("frequency_penalty".into(), None);
        f.insert("top_k".into(), request.generation.as_ref().and_then(|g| g.top_k).map(crate::json::num));
        f.insert("min_p".into(), min_p);
        Ok(finalize(f))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(ResponsesParser::new(request, &xai_adapter()))
    }
}

pub fn xai_protocol() -> Arc<dyn Protocol> {
    Arc::new(XaiResponses)
}

// ---------------------------------------------------------------------------------------------
// Alibaba
// ---------------------------------------------------------------------------------------------

fn alibaba_native(native: &Object) -> Result<Value, AIError> {
    let kind = native.get("alibaba").and_then(|a| a.get("type")).and_then(Value::as_str);
    match kind {
        Some(k @ ("web_search" | "web_extractor" | "code_interpreter")) => Ok(json!({"type": k})),
        _ => Err(AIError::invalid_request("Invalid Alibaba native tool")),
    }
}

fn web_extractor_valid(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("web_extractor_call")
        && item.get("id").map(Value::is_string).unwrap_or(false)
        && item.get("urls").map(|u| u.as_array().map(|a| a.iter().all(Value::is_string)).unwrap_or(false)).unwrap_or(true)
        && item.get("goal").map(Value::is_string).unwrap_or(true)
}

fn web_extractor_input(item: &Value) -> Value {
    let mut o = Object::new();
    if let Some(v) = item.get("urls") {
        o.insert("urls".into(), v.clone());
    }
    if let Some(v) = item.get("goal") {
        o.insert("goal".into(), v.clone());
    }
    Value::Object(o)
}

fn web_extractor_result(route: &str, item: &Value) -> Result<ToolResultValue, AIError> {
    if !web_extractor_valid(item) {
        return Err(AIError::invalid_output(route, "Alibaba returned an invalid web extraction item").with_body(encode(item)));
    }
    Ok(match item.get("error") {
        Some(e) if !e.is_null() => ToolResultValue::Error(e.clone()),
        _ => ToolResultValue::Json(item.clone()),
    })
}

pub fn alibaba_adapter() -> Adapter {
    Adapter {
        id: "alibaba-responses",
        name: "Alibaba Responses",
        native_tool: Some(alibaba_native),
        restore_hosted: Some(web_extractor_valid),
        hosted_tools: vec![
            HostedTool { item_type: "web_extractor_call", name: "web_extractor", input: web_extractor_input, result: Some(web_extractor_result) },
            h("web_search_call", "web_search", action),
            h("code_interpreter_call", "code_interpreter", code_only),
        ],
        reasoning_text_delta: false,
        meta: false,
        lower_media: None,
    }
}

pub struct AlibabaResponses;

impl Protocol for AlibabaResponses {
    fn id(&self) -> &str {
        "alibaba-responses"
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        let check = |k: &str, f: fn(&Value) -> bool| -> Result<Option<Value>, AIError> {
            match request.po(k) {
                None => Ok(None),
                Some(v) if f(v) => Ok(Some(v.clone())),
                Some(_) => Err(AIError::invalid_request(format!("Invalid Alibaba Responses provider option `{k}`"))),
            }
        };
        check("reasoningEffort", Value::is_string)?;
        let enable = check("enableThinking", Value::is_boolean)?;
        check("store", Value::is_boolean)?;
        let previous = check("previousResponseId", Value::is_string)?;
        let conversation = check("conversation", Value::is_string)?;
        let mut f = base::body_fields(request, &alibaba_adapter())?;
        f.insert("enable_thinking".into(), enable);
        f.insert("previous_response_id".into(), previous);
        f.insert("conversation".into(), conversation);
        if let Some(Some(choice)) = f.get("tool_choice") {
            if choice.get("type").and_then(Value::as_str) == Some("function") {
                let rewritten = json!({"type": "allowed_tools", "mode": "required", "tools": [choice.clone()]});
                f.insert("tool_choice".into(), Some(rewritten));
            } else {
                let c = choice.clone();
                f.insert("tool_choice".into(), Some(c));
            }
        } else {
            f.insert("tool_choice".into(), None);
        }
        Ok(finalize(f))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(ResponsesParser::new(request, &alibaba_adapter()))
    }
}

pub fn alibaba_protocol() -> Arc<dyn Protocol> {
    Arc::new(AlibabaResponses)
}

// ---------------------------------------------------------------------------------------------
// Meta
// ---------------------------------------------------------------------------------------------

fn meta_native(native: &Object) -> Result<Value, AIError> {
    let err = || AIError::invalid_request("Invalid Meta native tool");
    let tool = native.get("meta").and_then(Value::as_object).ok_or_else(err)?;
    let order: &[&str] = match tool.get("type").and_then(Value::as_str) {
        Some("web_search") => &["type", "search_context_size", "user_location"],
        Some("image_generation") => {
            &["type", "size", "output_format", "reasoning_strength", "enable_image_search", "enable_web_search", "enable_shell"]
        }
        _ => return Err(err()),
    };
    let mut out = Object::new();
    for k in order {
        if let Some(v) = tool.get(*k) {
            out.insert((*k).into(), v.clone());
        }
    }
    if let Some(loc) = out.get("user_location").and_then(Value::as_object).cloned() {
        if loc.get("type").and_then(Value::as_str) != Some("approximate") {
            return Err(err());
        }
        let mut ordered = Object::new();
        for k in ["type", "city", "region", "country", "timezone"] {
            if let Some(v) = loc.get(k) {
                ordered.insert(k.into(), v.clone());
            }
        }
        out.insert("user_location".into(), Value::Object(ordered));
    }
    Ok(Value::Object(out))
}

fn meta_image_valid(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("image_generation_call")
        && item.get("id").map(Value::is_string).unwrap_or(false)
        && item.get("status").map(Value::is_string).unwrap_or(true)
        && item.get("result").map(|v| v.is_string() || v.is_null()).unwrap_or(true)
        && item.get("output_format").map(Value::is_string).unwrap_or(true)
}

fn meta_image_result(route: &str, item: &Value) -> Result<ToolResultValue, AIError> {
    if !meta_image_valid(item) {
        return Err(AIError::invalid_output(route, "Meta returned an invalid image item").with_body(encode(item)));
    }
    if let Some(e) = item.get("error").filter(|e| !e.is_null()) {
        return Ok(ToolResultValue::Error(e.clone()));
    }
    let Some(result) = item.get("result").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
        return Err(AIError::invalid_output(route, "Meta returned an image without data").with_body(encode(item)));
    };
    let data = base64::engine::general_purpose::STANDARD
        .decode(result)
        .map_err(|_| AIError::invalid_output(route, "Meta returned invalid image base64").with_body(encode(item)))?;
    let mime = match item.get("output_format").and_then(Value::as_str) {
        Some(f) => format!("image/{f}"),
        None => crate::media::detect_media_type(&data).unwrap_or("application/octet-stream").to_string(),
    };
    Ok(ToolResultValue::Content(vec![ToolContent::File { uri: format!("data:{mime};base64,{result}"), mime, name: None }]))
}

pub fn meta_adapter() -> Adapter {
    Adapter {
        id: "meta-responses",
        name: "Meta Responses",
        native_tool: Some(meta_native),
        restore_hosted: Some(meta_image_valid),
        hosted_tools: vec![
            h("web_search_call", "web_search", action),
            HostedTool { item_type: "image_generation_call", name: "image_generation", input: empty, result: Some(meta_image_result) },
        ],
        reasoning_text_delta: false,
        meta: true,
        lower_media: None,
    }
}

pub struct MetaResponses;

impl Protocol for MetaResponses {
    fn id(&self) -> &str {
        "meta-responses"
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        let key = request.metadata_key();
        let mut req = request.clone();
        for message in &mut req.messages {
            for part in &mut message.content {
                if let ContentPart::ToolResult(r) = part {
                    let item_matches = r.provider_metadata.as_ref().and_then(|pm| pm.get(&key)).and_then(|m| m.get("itemId")).and_then(Value::as_str)
                        == Some(r.id.as_str());
                    if r.provider_executed == Some(true) && r.name == "image_generation" && matches!(r.result, ToolResultValue::Content(_)) && item_matches {
                        r.result = ToolResultValue::Json(json!({"type": "image_generation_call", "id": r.id, "status": "completed", "result": null}));
                    }
                }
            }
        }
        let mut f = base::body_fields(&req, &meta_adapter())?;
        if let Some(Some(Value::Array(input))) = f.get_mut("input") {
            *input = attach_tool_videos(std::mem::take(input));
        }
        Ok(finalize(f))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(ResponsesParser::new(request, &meta_adapter()))
    }
}

pub fn meta_protocol() -> Arc<dyn Protocol> {
    Arc::new(MetaResponses)
}

/// Meta cannot see video nested in tool results; attach it after the complete result batch.
fn attach_tool_videos(items: Vec<Value>) -> Vec<Value> {
    let mut input = vec![];
    let mut videos: Vec<Value> = vec![];
    let flush = |input: &mut Vec<Value>, videos: &mut Vec<Value>| {
        if !videos.is_empty() {
            input.push(json!({"type": "message", "role": "user", "content": std::mem::take(videos)}));
        }
    };
    for mut item in items {
        let kind = item.get("type").and_then(Value::as_str);
        if kind != Some("function_call_output") && kind != Some("custom_tool_call_output") {
            flush(&mut input, &mut videos);
            input.push(item);
            continue;
        }
        if let Some(Value::Array(output)) = item.get_mut("output") {
            let total = output.len();
            let (moved, kept): (Vec<Value>, Vec<Value>) =
                output.drain(..).partition(|part| part.get("type").and_then(Value::as_str) == Some("input_video"));
            videos.extend(moved);
            item["output"] = if kept.is_empty() && total > 0 {
                json!("Video attached in the following user message.")
            } else {
                Value::Array(kept)
            };
        }
        input.push(item);
    }
    flush(&mut input, &mut videos);
    input
}
