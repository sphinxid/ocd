//! AI SDK bridge route (`packages/core/src/aisdk.ts`): models whose package is an `aisdk:` npm
//! AI SDK provider without a native implementation.
//!
//! The provider package runs in a JS runtime behind [`AiSdkBridge`] (ocd: the oc-plugin Bun
//! sidecar, which also runs the `aisdk.sdk` / `aisdk.language` plugin hooks and constructs the
//! `LanguageModelV3`). This module owns everything upstream does on the server side of that object:
//!
//! - the route (`modelFromLanguage`): id `ai-sdk:<package>`, metadata key, defaults;
//! - request lowering to `LanguageModelV3CallOptions` (`callOptions`, `prompt`, ...), the body of
//!   the compiled request;
//! - raising `LanguageModelV3StreamPart`s to [`LLMEvent`]s (`streamLanguage`, `streamPartEvents`);
//! - error classification of thrown/streamed AI SDK errors (`llmError`), from the error shape the
//!   bridge reports.
//!
//! Binary media crosses the bridge as `{"$bytes": "<base64>"}` (a `Uint8Array`) and URL media as
//! `{"$url": "..."}` (a `URL`); base64 sources stay plain strings, as upstream passes them.

use crate::catalog::types::{merge_overlay, package_name, ModelInfo};
use crate::client::LLMStream;
use crate::error::{
    classify_provider_failure, rate_limit_details, retry_after_ms, AIError, ErrorKind, HttpContext, ProviderFailure,
    TransportOperation, TransportType,
};
use crate::json::{encode, num, Object};
use crate::media::{MediaAsset, MediaSource};
use crate::protocols::shared::{flatten_tool_request, parse_tool_input, tool_file_media, unsupported_content, wrap_system_update};
use crate::protocols::{Protocol, Step, StreamParser};
use crate::route::framing::{Frame, Framing};
use crate::route::transport::HttpHook;
use crate::route::{Endpoint, LanguageModel, PreparedStream, Route};
use crate::types::{
    ContentPart, FinishReason, FinishReasonDetails, HttpOptions, LLMEvent, LLMRequest, Message, ModelDefaults, ProviderMetadata,
    Role, ToolChoiceType, ToolContent, ToolDefinition, ToolResultPart, ToolResultValue, Usage,
};
use base64::Engine;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// `ProviderShared.parseToolInput("aisdk", ...)` route label.
const ROUTE: &str = "aisdk";

// ---------------------------------------------------------------------------------------------
// Bridge
// ---------------------------------------------------------------------------------------------

/// A failure reported by the bridge.
#[derive(Debug, Clone, PartialEq)]
pub enum BridgeError {
    /// An error the provider threw from `doStream` (`operation` `request`) or streamed / threw
    /// while reading (`read`), described by the error shape (see [`error_from_shape`]).
    Provider { operation: TransportOperation, shape: Value },
    /// The bridge itself failed (the JS runtime is unavailable or died).
    Host(String),
}

/// One `doStream` call.
pub struct StreamCall {
    /// `RuntimeInfo`: the effective catalog model (`Model.Info` with provider settings merged).
    pub model: Value,
    /// `LanguageModelV3CallOptions` (wire form).
    pub options: Value,
    /// `session.http.*` hooks around the SDK's `fetch` (upstream `StreamOptions.http`).
    pub hook: Option<Arc<dyn HttpHook>>,
    /// Cancelling aborts the call (`abortSignal`).
    pub cancel: CancellationToken,
}

/// The stream parts of one `doStream` call, in order. Ends after the last part; a failure ends it.
pub type PartStream = BoxStream<'static, Result<Value, BridgeError>>;

/// The JS side of the bridge (`AISDK.Service` in upstream; the oc-plugin sidecar in ocd).
pub trait AiSdkBridge: Send + Sync {
    /// `AISDK.language(model)`: run the `aisdk.sdk` / `aisdk.language` hooks and construct the
    /// language model (cached by the bridge). `Err` carries the cause message upstream reports
    /// in `ModelInitializationError.detail`, or `None` when the cause had none.
    fn load(&self, model: Value) -> BoxFuture<'static, Result<(), Option<String>>>;
    /// `language.doStream(options)`.
    fn stream(&self, call: StreamCall) -> BoxFuture<'static, Result<PartStream, BridgeError>>;
}

// ---------------------------------------------------------------------------------------------
// Route (`modelFromLanguage`)
// ---------------------------------------------------------------------------------------------

/// `providerOptionKey(packageName, providerID)`.
pub fn provider_option_key(package: &str, provider_id: &str) -> String {
    match package {
        "@ai-sdk/google" => "google".into(),
        "@ai-sdk/google-vertex" => "vertex".into(),
        "@ai-sdk/google-vertex/anthropic" => "anthropic".into(),
        "@ai-sdk/amazon-bedrock" => "bedrock".into(),
        "@ai-sdk/amazon-bedrock/mantle" => "openai".into(),
        "@ai-sdk/azure" => "azure".into(),
        "@ai-sdk/github-copilot" => "copilot".into(),
        "@jerome-benoit/sap-ai-provider-v2" => "sap-ai".into(),
        "@ai-sdk/openai-compatible" => provider_id.split('.').next().unwrap_or_default().to_string(),
        "@openrouter/ai-sdk-provider" => "openrouter".into(),
        "ai-gateway-provider" => "openaiCompatible".into(),
        _ => match package.strip_prefix("@ai-sdk/") {
            Some(rest) => rest.to_string(),
            None => provider_id.to_string(),
        },
    }
}

/// `requestSettings`: the provider options (`Provider.nativeSettings`) minus the keys that configure the SDK
/// provider when it is created, not each request.
fn request_settings(settings: Option<&Object>) -> Option<Object> {
    const DROP: &[&str] = &["accessToken", "apiKey", "authToken", "baseURL", "fetch"];
    let settings = crate::catalog::types::native_settings(settings?);
    let out: Object = settings.into_iter().filter(|(k, _)| !DROP.contains(&k.as_str())).collect();
    if out.is_empty() { None } else { Some(out) }
}

/// `mapBodyToProviderOptions`: (request provider options, extra body).
fn map_body_to_provider_options(model: &ModelInfo, package: &str) -> (Option<Object>, Option<Object>) {
    let settings = request_settings(model.settings.as_ref());
    let pro = model
        .body
        .as_ref()
        .and_then(|b| b.get("reasoning"))
        .and_then(Value::as_object)
        .and_then(|r| r.get("mode"))
        .and_then(Value::as_str)
        == Some("pro");
    let force = ["@ai-sdk/openai", "@ai-sdk/azure", "@ai-sdk/amazon-bedrock/mantle"].contains(&package)
        && (pro
            || settings.as_ref().is_some_and(|s| s.get("reasoningEffort").is_some_and(|v| !v.is_null()))
            || settings.as_ref().is_some_and(|s| s.get("reasoningSummary").is_some_and(|v| !v.is_null())));
    let normalized = if force {
        let mut patch = Object::new();
        patch.insert("forceReasoning".into(), Value::Bool(true));
        merge_overlay(settings.as_ref(), Some(&patch))
    } else {
        settings
    };
    if !pro {
        return (normalized, model.body.clone());
    }
    let mut body = model.body.clone().unwrap_or_default();
    body.shift_remove("reasoning");
    let mut patch = Object::new();
    patch.insert("reasoningMode".into(), json!("pro"));
    (merge_overlay(normalized.as_ref(), Some(&patch)), if body.is_empty() { None } else { Some(body) })
}

/// `AISDK.model(info)` without the load: the runtime model routed through `bridge`. `info` is the
/// `RuntimeInfo` (provider settings already merged, credential settings applied).
pub fn language_model(info: &ModelInfo, bridge: Arc<dyn AiSdkBridge>) -> LanguageModel {
    let package = package_name(info.package.as_deref().unwrap_or_default());
    let (settings, body) = map_body_to_provider_options(info, &package);
    let provider_id = info.canonical.clone().unwrap_or_else(|| info.provider_id.clone());
    let option_key = provider_option_key(&package, &provider_id);
    let headers: Option<IndexMap<String, String>> =
        info.headers.as_ref().map(|h| h.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
    let http = if body.is_none() && headers.is_none() {
        None
    } else {
        Some(HttpOptions { body: body.clone(), headers: headers.clone(), ..Default::default() })
    };
    let model_id = info.model_id.clone();
    let protocol = Arc::new(AiSdkProtocol { package: package.clone(), model_id: model_id.clone(), option_key: option_key.clone() });
    let mut route = Route::new(&format!("ai-sdk:{package}"), protocol, Endpoint::new(Some("https://ai-sdk.local"), "/"), Framing::Document)
        .initial_provider(&provider_id, Some(&option_key));
    // The route literal's defaults are used as given (no header normalization).
    route.defaults = ModelDefaults { generation: None, provider_options: settings, http };
    route.stream = Some(Arc::new(AiSdkStream { bridge, info: serde_json::to_value(info).unwrap_or(Value::Null) }));
    LanguageModel { id: model_id, provider: provider_id, route: Arc::new(route), defaults: None, compatibility: info.compatibility.clone() }
}

/// The bridge and runtime info behind an AI SDK route, for the eager load at resolution.
pub fn bridge_of(model: &LanguageModel) -> Option<(Arc<dyn AiSdkBridge>, Value)> {
    let stream = model.route.stream.as_ref()?;
    let stream = stream.as_any().downcast_ref::<AiSdkStream>()?;
    Some((stream.bridge.clone(), stream.info.clone()))
}

// ---------------------------------------------------------------------------------------------
// Request lowering (`callOptions`)
// ---------------------------------------------------------------------------------------------

struct AiSdkProtocol {
    package: String,
    model_id: String,
    option_key: String,
}

struct NoParser;

impl StreamParser for NoParser {
    fn step(&mut self, _frame: &Frame) -> Result<Step, AIError> {
        Ok(Step::default())
    }
}

impl Protocol for AiSdkProtocol {
    fn id(&self) -> &str {
        "ai-sdk"
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        call_options(request, &self.package, &self.model_id, &self.option_key)
    }
    fn parser(&self, _request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(NoParser)
    }
    /// AI SDK providers convert tool schemas themselves, so model-family sanitizers stay off.
    fn sanitizer(&self) -> Option<&'static str> {
        Some("none")
    }
}

fn put(out: &mut Object, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        out.insert(key.into(), value);
    }
}

/// `callOptions(request, packageName, modelID, optionKey)`.
pub fn call_options(request: &LLMRequest, package: &str, model_id: &str, option_key: &str) -> Result<Value, AIError> {
    let (tools, flattened) = flatten_tool_request(request);
    let generation = request.generation.clone().unwrap_or_default();
    let mut out = Object::new();
    out.insert("prompt".into(), Value::Array(prompt(&flattened)?));
    put(&mut out, "maxOutputTokens", generation.max_tokens.map(num));
    put(&mut out, "temperature", generation.temperature.map(num));
    put(&mut out, "stopSequences", generation.stop.clone().map(|s| json!(s)));
    put(&mut out, "topP", generation.top_p.map(num));
    put(&mut out, "topK", generation.top_k.map(num));
    put(&mut out, "presencePenalty", generation.presence_penalty.map(num));
    put(&mut out, "frequencyPenalty", generation.frequency_penalty.map(num));
    put(&mut out, "seed", generation.seed.map(num));
    out.insert("tools".into(), Value::Array(tools.iter().map(tool).collect()));
    put(&mut out, "toolChoice", tool_choice(request));
    put(&mut out, "headers", request.http.as_ref().and_then(|h| h.headers.as_ref()).map(|h| json!(h)));
    put(&mut out, "providerOptions", request_provider_options(request.provider_options.as_ref(), package, model_id, option_key));
    Ok(Value::Object(out))
}

fn tool(input: &ToolDefinition) -> Value {
    json!({ "type": "function", "name": input.name, "description": input.description, "inputSchema": input.input_schema })
}

fn tool_choice(request: &LLMRequest) -> Option<Value> {
    let choice = request.tool_choice.as_ref()?;
    match choice.kind {
        ToolChoiceType::Tool => choice.name.as_ref().map(|name| json!({ "type": "tool", "toolName": name })),
        ToolChoiceType::Auto => Some(json!({ "type": "auto" })),
        ToolChoiceType::None => Some(json!({ "type": "none" })),
        ToolChoiceType::Required => Some(json!({ "type": "required" })),
    }
}

/// `prompt(request)`.
pub fn prompt(request: &LLMRequest) -> Result<Vec<Value>, AIError> {
    let system = request.system.iter().map(|p| p.text.as_str()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n\n");
    let mut pending: Vec<Value> = vec![];
    let mut messages: Vec<Value> = vec![];
    for (index, input) in request.messages.iter().enumerate() {
        if input.role != Role::Tool {
            messages.extend(message(input)?);
            continue;
        }
        let (lowered, media) = tool_message(input)?;
        pending.extend(media);
        let next_is_tool = request.messages.get(index + 1).is_some_and(|m| m.role == Role::Tool);
        messages.extend(lowered);
        if next_is_tool || pending.is_empty() {
            continue;
        }
        let mut content = vec![json!({ "type": "text", "text": "Attached media from tool result:" })];
        content.append(&mut pending);
        messages.push(json!({ "role": "user", "content": content }));
    }
    if system.is_empty() {
        return Ok(messages);
    }
    let mut out = vec![json!({ "role": "system", "content": system })];
    out.extend(messages);
    Ok(out)
}

fn message(input: &Message) -> Result<Vec<Value>, AIError> {
    Ok(match input.role {
        Role::System => {
            // A chronological instruction update: opaque AI SDK providers do not uniformly allow the system
            // role after conversation history, so it becomes wrapped user text.
            let texts: Vec<&str> = input
                .content
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect();
            vec![json!({ "role": "user", "content": [{ "type": "text", "text": wrap_system_update(&texts) }] })]
        }
        Role::User => {
            let mut content = vec![];
            for part in &input.content {
                content.extend(user_part(part)?);
            }
            vec![json!({ "role": "user", "content": content })]
        }
        Role::Assistant => {
            let mut content = vec![];
            for part in &input.content {
                content.extend(assistant_part(part)?);
            }
            vec![json!({ "role": "assistant", "content": content })]
        }
        Role::Tool => tool_message(input)?.0,
    })
}

fn tool_message(input: &Message) -> Result<(Vec<Value>, Vec<Value>), AIError> {
    let mut media = vec![];
    let mut content = vec![];
    for part in &input.content {
        let ContentPart::ToolResult(result) = part else { continue };
        let ToolResultValue::Content(items) = &result.result else {
            content.extend(tool_result_part(result));
            continue;
        };
        let mut kept = vec![];
        for item in items {
            match item {
                ToolContent::File { uri, mime, name } if mime.starts_with("image/") || mime == "application/pdf" => {
                    let lifted = tool_file_media(uri, mime, name.as_deref());
                    let mut file = Object::new();
                    file.insert("type".into(), json!("file"));
                    file.insert("mediaType".into(), json!(mime));
                    file.insert("data".into(), file_data(&lifted.media)?);
                    put(&mut file, "filename", name.clone().map(Value::String));
                    media.push(Value::Object(file));
                }
                other => kept.push(other.clone()),
            }
        }
        let mut lowered = result.clone();
        lowered.result = if kept.is_empty() {
            ToolResultValue::text("Media attached in the following user message.")
        } else {
            ToolResultValue::Content(kept)
        };
        content.extend(tool_result_part(&lowered));
    }
    let messages = if content.is_empty() { vec![] } else { vec![json!({ "role": "tool", "content": content })] };
    Ok((messages, media))
}

fn media_file(media: &MediaAsset, filename: Option<&String>) -> Result<Value, AIError> {
    let mut file = Object::new();
    file.insert("type".into(), json!("file"));
    file.insert("mediaType".into(), json!(media_type(media)));
    file.insert("data".into(), file_data(media)?);
    put(&mut file, "filename", filename.cloned().map(Value::String));
    Ok(Value::Object(file))
}

fn media_type(media: &MediaAsset) -> Option<String> {
    match &media.source {
        MediaSource::Bytes { media_type, .. } | MediaSource::Base64 { media_type, .. } => Some(media_type.clone()),
        MediaSource::Url { media_type, .. } | MediaSource::Ref { media_type, .. } => media_type.clone(),
    }
}

fn user_part(part: &ContentPart) -> Result<Vec<Value>, AIError> {
    Ok(match part {
        ContentPart::Text(t) => vec![json!({ "type": "text", "text": t.text })],
        ContentPart::Media(m) => vec![media_file(&m.media, m.filename.as_ref())?],
        _ => vec![],
    })
}

fn with_provider_options(mut value: Object, metadata: Option<&ProviderMetadata>) -> Value {
    put(&mut value, "providerOptions", metadata.map(|m| Value::Object(m.clone())));
    Value::Object(value)
}

fn assistant_part(part: &ContentPart) -> Result<Vec<Value>, AIError> {
    Ok(match part {
        ContentPart::Compaction(c) => {
            return Err(AIError::unsupported(
                "compaction-replay",
                "AI SDK routes cannot replay native provider compaction state",
                Some(&c.provider),
                None,
            ));
        }
        ContentPart::Text(t) => {
            let mut o = Object::new();
            o.insert("type".into(), json!("text"));
            o.insert("text".into(), json!(t.text));
            vec![with_provider_options(o, t.provider_metadata.as_ref())]
        }
        ContentPart::Media(m) => vec![media_file(&m.media, m.filename.as_ref())?],
        ContentPart::Reasoning(r) => {
            let mut o = Object::new();
            o.insert("type".into(), json!("reasoning"));
            o.insert("text".into(), json!(r.text));
            vec![with_provider_options(o, r.provider_metadata.as_ref())]
        }
        ContentPart::ToolCall(call) => {
            let mut o = Object::new();
            o.insert("type".into(), json!("tool-call"));
            o.insert("toolCallId".into(), json!(call.id));
            o.insert("toolName".into(), json!(call.name));
            o.insert("input".into(), call.input.clone());
            put(&mut o, "providerExecuted", call.provider_executed.map(Value::Bool));
            vec![with_provider_options(o, call.provider_metadata.as_ref())]
        }
        ContentPart::ToolResult(result) => tool_result_part(result),
        ContentPart::Effort(_) => {
            return Err(unsupported_content("AI SDK", Role::Assistant, &["text", "media", "reasoning", "tool-call", "tool-result"]));
        }
    })
}

/// `fileData(media)`.
fn file_data(media: &MediaAsset) -> Result<Value, AIError> {
    match &media.source {
        MediaSource::Bytes { data, .. } => Ok(json!({ "$bytes": base64::engine::general_purpose::STANDARD.encode(data) })),
        MediaSource::Base64 { data, .. } => Ok(Value::String(data.clone())),
        MediaSource::Url { url, .. } => Ok(json!({ "$url": url })),
        MediaSource::Ref { provider, .. } => Err(AIError::unsupported(
            "media-ref",
            "AI SDK routes cannot forward provider media references",
            Some(provider),
            None,
        )),
    }
}

fn tool_result_part(part: &ToolResultPart) -> Vec<Value> {
    let mut o = Object::new();
    o.insert("type".into(), json!("tool-result"));
    o.insert("toolCallId".into(), json!(part.id));
    o.insert("toolName".into(), json!(part.name));
    o.insert("output".into(), tool_output(&part.result));
    vec![with_provider_options(o, part.provider_metadata.as_ref())]
}

static DATA_URL: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)^data:[^;,]+(?:;[^,]*)*;base64,(.*)$").unwrap());

/// `messageValue`.
fn message_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => encode(other),
    }
}

fn tool_output(result: &ToolResultValue) -> Value {
    match result {
        ToolResultValue::Text(v) | ToolResultValue::Error(v) => json!({ "type": "text", "value": message_value(v) }),
        ToolResultValue::Json(v) => json!({ "type": "json", "value": v }),
        ToolResultValue::Content(items) => {
            let value: Vec<Value> = items
                .iter()
                .map(|item| match item {
                    ToolContent::Text { text } => json!({ "type": "text", "text": text }),
                    ToolContent::File { uri, mime, name } => {
                        let image = mime.to_lowercase().starts_with("image/");
                        match DATA_URL.captures(uri).and_then(|c| c.get(1)).map(|m| m.as_str().to_string()) {
                            Some(data) if image => json!({ "type": "image-data", "data": data, "mediaType": mime }),
                            Some(data) => {
                                let mut o = Object::new();
                                o.insert("type".into(), json!("file-data"));
                                o.insert("data".into(), json!(data));
                                o.insert("mediaType".into(), json!(mime));
                                put(&mut o, "filename", name.clone().map(Value::String));
                                Value::Object(o)
                            }
                            None if image => json!({ "type": "image-url", "url": uri }),
                            None => json!({ "type": "file-url", "url": uri }),
                        }
                    }
                })
                .collect();
            json!({ "type": "content", "value": value })
        }
    }
}

/// `gatewayProviderOptions(modelID, settings)`.
fn gateway_provider_options(model_id: &str, settings: &Object) -> Option<Value> {
    let gateway = settings.get("gateway").and_then(Value::as_object).cloned();
    let model: Object = settings.iter().filter(|(k, _)| k.as_str() != "gateway").map(|(k, v)| (k.clone(), v.clone())).collect();
    if model.is_empty() {
        return gateway.map(|g| json!({ "gateway": g }));
    }
    let prefix = model_id.find('/').filter(|i| *i > 0).map(|i| &model_id[..i]);
    if let Some(prefix) = prefix {
        let mut out = Object::new();
        if let Some(g) = gateway {
            out.insert("gateway".into(), Value::Object(g));
        }
        out.insert(if prefix == "amazon" { "bedrock".into() } else { prefix.to_string() }, Value::Object(model));
        return Some(Value::Object(out));
    }
    if let Some(mut g) = gateway {
        for (k, v) in model {
            g.insert(k, v);
        }
        return Some(json!({ "gateway": g }));
    }
    Some(json!({ "gateway": model }))
}

/// `requestProviderOptions`.
fn request_provider_options(input: Option<&Object>, package: &str, model_id: &str, option_key: &str) -> Option<Value> {
    let options = input?;
    if package == "@ai-sdk/gateway" {
        return gateway_provider_options(model_id, options);
    }
    if package == "@ai-sdk/azure" {
        return Some(json!({ "openai": options, "azure": options }));
    }
    let mut out = Object::new();
    out.insert(option_key.to_string(), Value::Object(options.clone()));
    Some(Value::Object(out))
}

// ---------------------------------------------------------------------------------------------
// Stream raising (`streamLanguage`, `streamPartEvents`)
// ---------------------------------------------------------------------------------------------

struct AiSdkStream {
    bridge: Arc<dyn AiSdkBridge>,
    info: Value,
}

impl PreparedStream for AiSdkStream {
    fn stream(&self, _request: LLMRequest, body: Value, hook: Option<Arc<dyn HttpHook>>, cancel: CancellationToken) -> LLMStream {
        let bridge = self.bridge.clone();
        let info = self.info.clone();
        let stream = async_stream::stream! {
            let mut state = StreamState::default();
            yield Ok(LLMEvent::StepStart { index: state.step });
            let call = StreamCall { model: info, options: body, hook, cancel: cancel.clone() };
            let started = tokio::select! {
                _ = cancel.cancelled() => {
                    yield Err(AIError::transport(TransportOperation::Request, "Request aborted", Some("Aborted"), None));
                    return;
                }
                result = bridge.stream(call) => result,
            };
            let mut parts = match started {
                Ok(parts) => parts,
                Err(error) => {
                    yield Err(bridge_error(error, TransportOperation::Request));
                    return;
                }
            };
            loop {
                let next = tokio::select! {
                    _ = cancel.cancelled() => {
                        yield Err(AIError::transport(TransportOperation::Read, "Request aborted", Some("Aborted"), None));
                        return;
                    }
                    next = parts.next() => next,
                };
                match next {
                    None => break,
                    Some(Err(error)) => {
                        yield Err(bridge_error(error, TransportOperation::Read));
                        return;
                    }
                    Some(Ok(part)) => match state.events(&part) {
                        Ok(events) => {
                            for event in events {
                                yield Ok(event);
                            }
                        }
                        Err(error) => {
                            yield Err(error);
                            return;
                        }
                    },
                }
            }
        };
        Box::pin(stream)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[derive(Default)]
struct StreamState {
    step: u32,
    tool_names: HashMap<String, String>,
    text: Option<String>,
    reasoning: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Fragment {
    Text,
    Reasoning,
}

fn str_field(part: &Value, key: &str) -> String {
    match part.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => encode(other),
    }
}

/// `providerMetadata(value)`: kept only when it is a `ProviderMetadata` record of records.
fn provider_metadata(value: Option<&Value>) -> Option<ProviderMetadata> {
    let object = value?.as_object()?;
    if object.values().all(Value::is_object) { Some(object.clone()) } else { None }
}

fn number(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_f64).filter(|v| v.is_finite()).map(|v| v.max(0.0) as u64)
}

/// `usage(input)`.
fn usage(input: Option<&Value>) -> Option<Usage> {
    let input = input?;
    let tokens = |side: &str, key: &str| number(input.get(side).and_then(|s| s.get(key)));
    let input_total = tokens("inputTokens", "total");
    let output_total = tokens("outputTokens", "total");
    let out = Usage {
        context_tokens: None,
        input_tokens: input_total,
        non_cached_input_tokens: tokens("inputTokens", "noCache"),
        cache_read_input_tokens: tokens("inputTokens", "cacheRead"),
        cache_write_input_tokens: tokens("inputTokens", "cacheWrite"),
        output_tokens: output_total,
        reasoning_tokens: tokens("outputTokens", "reasoning"),
        total_tokens: match (input_total, output_total) {
            (Some(i), Some(o)) => Some(i + o),
            _ => None,
        },
        cost: None,
        provider_metadata: None,
    };
    if out == Usage::default() { None } else { Some(out) }
}

/// `finishReason(value)`: `other` → `unknown`.
fn finish_reason(value: Option<&Value>) -> FinishReasonDetails {
    let unified = value.and_then(|v| v.get("unified")).and_then(Value::as_str).unwrap_or("unknown");
    let normalized = if unified == "other" {
        FinishReason::Unknown
    } else {
        serde_json::from_value::<FinishReason>(Value::String(unified.into())).unwrap_or(FinishReason::Unknown)
    };
    FinishReasonDetails {
        normalized,
        raw: value.and_then(|v| v.get("raw")).and_then(Value::as_str).map(String::from),
        category: None,
        explanation: None,
    }
}

fn fragment_end(kind: Fragment, id: String, provider_metadata: Option<ProviderMetadata>) -> LLMEvent {
    match kind {
        Fragment::Text => LLMEvent::TextEnd { id, text: None, provider_metadata },
        Fragment::Reasoning => LLMEvent::ReasoningEnd { id, text: None, provider_metadata },
    }
}

impl StreamState {
    fn slot(&mut self, kind: Fragment) -> &mut Option<String> {
        match kind {
            Fragment::Text => &mut self.text,
            Fragment::Reasoning => &mut self.reasoning,
        }
    }

    /// A start or delta for another fragment closes the open one; repeated starts are ignored.
    fn open(&mut self, kind: Fragment, id: &str, provider_metadata: Option<ProviderMetadata>) -> Vec<LLMEvent> {
        let slot = self.slot(kind);
        if slot.as_deref() == Some(id) {
            return vec![];
        }
        let previous = slot.replace(id.to_string());
        let start = match kind {
            Fragment::Text => LLMEvent::TextStart { id: id.to_string(), provider_metadata },
            Fragment::Reasoning => LLMEvent::ReasoningStart { id: id.to_string(), provider_metadata },
        };
        match previous {
            None => vec![start],
            Some(open) => vec![fragment_end(kind, open, None), start],
        }
    }

    /// Ends for fragments that are not open are dropped.
    fn close(&mut self, kind: Fragment, id: &str, provider_metadata: Option<ProviderMetadata>) -> Vec<LLMEvent> {
        let slot = self.slot(kind);
        if slot.as_deref() != Some(id) {
            return vec![];
        }
        *slot = None;
        vec![fragment_end(kind, id.to_string(), provider_metadata)]
    }

    /// `streamPartEvents(state, part)`.
    fn events(&mut self, part: &Value) -> Result<Vec<LLMEvent>, AIError> {
        let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
        let id = str_field(part, "id");
        let metadata = provider_metadata(part.get("providerMetadata"));
        Ok(match kind {
            "text-start" => self.open(Fragment::Text, &id, metadata),
            "text-delta" => {
                let mut out = self.open(Fragment::Text, &id, None);
                out.push(LLMEvent::TextDelta { id, text: str_field(part, "delta"), provider_metadata: metadata });
                out
            }
            "text-end" => self.close(Fragment::Text, &id, metadata),
            "reasoning-start" => self.open(Fragment::Reasoning, &id, metadata),
            "reasoning-delta" => {
                let mut out = self.open(Fragment::Reasoning, &id, None);
                out.push(LLMEvent::ReasoningDelta { id, text: str_field(part, "delta"), provider_metadata: metadata });
                out
            }
            "reasoning-end" => self.close(Fragment::Reasoning, &id, metadata),
            "tool-input-start" => {
                let name = str_field(part, "toolName");
                self.tool_names.insert(id.clone(), name.clone());
                vec![LLMEvent::ToolInputStart {
                    id,
                    name,
                    namespace: None,
                    provider_executed: part.get("providerExecuted").and_then(Value::as_bool),
                    provider_metadata: metadata,
                }]
            }
            "tool-input-delta" => {
                let name = self.tool_names.get(&id).cloned().unwrap_or_else(|| "unknown".into());
                vec![LLMEvent::ToolInputDelta { id, name, namespace: None, text: str_field(part, "delta"), input: None }]
            }
            "tool-input-end" => {
                let name = self.tool_names.get(&id).cloned().unwrap_or_else(|| "unknown".into());
                vec![LLMEvent::ToolInputEnd { id, name, namespace: None, provider_metadata: metadata }]
            }
            "tool-call" => {
                let call_id = str_field(part, "toolCallId");
                let name = str_field(part, "toolName");
                self.tool_names.insert(call_id.clone(), name.clone());
                let raw = str_field(part, "input");
                let executed = part.get("providerExecuted").and_then(Value::as_bool);
                match parse_tool_input(ROUTE, &name, &raw) {
                    Ok(input) => vec![LLMEvent::ToolCall {
                        id: call_id,
                        name,
                        namespace: None,
                        input,
                        provider_executed: executed,
                        provider_metadata: metadata,
                    }],
                    Err(error) if executed == Some(true) => return Err(error),
                    Err(_) => vec![LLMEvent::ToolInputError { id: call_id, name, namespace: None, raw }],
                }
            }
            "tool-result" => {
                let call_id = str_field(part, "toolCallId");
                self.tool_names.remove(&call_id);
                let value = part.get("result").cloned().unwrap_or(Value::Null);
                let result = serde_json::from_value::<ToolResultValue>(value.clone()).ok().filter(|_| is_tool_result_value(&value)).unwrap_or_else(|| {
                    if part.get("isError").and_then(Value::as_bool) == Some(true) {
                        ToolResultValue::Error(value)
                    } else {
                        ToolResultValue::Json(value)
                    }
                });
                vec![LLMEvent::ToolResult {
                    id: call_id,
                    name: str_field(part, "toolName"),
                    namespace: None,
                    result,
                    output: None,
                    provider_executed: Some(true),
                    provider_metadata: metadata,
                }]
            }
            "finish" => {
                let index = self.step;
                self.step += 1;
                let reason = finish_reason(part.get("finishReason"));
                let usage = usage(part.get("usage"));
                vec![
                    LLMEvent::StepFinish { index, reason: reason.clone(), usage: usage.clone(), provider_metadata: metadata.clone() },
                    LLMEvent::Finish { reason, usage, provider_metadata: metadata },
                ]
            }
            "error" => return Err(error_from_shape(part.get("error").unwrap_or(&Value::Null), TransportOperation::Read)),
            // stream-start, response-metadata, raw, file, source, tool-approval-request
            _ => vec![],
        })
    }
}

/// `Schema.is(ToolResultValue)`: `{type, value}` with a known type and a value of that type.
fn is_tool_result_value(value: &Value) -> bool {
    let Some(object) = value.as_object() else { return false };
    match (object.get("type").and_then(Value::as_str), object.get("value")) {
        (Some("json" | "text" | "error"), Some(_)) => true,
        (Some("content"), Some(Value::Array(_))) => serde_json::from_value::<Vec<ToolContent>>(object["value"].clone()).is_ok(),
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Errors (`llmError`)
// ---------------------------------------------------------------------------------------------

fn bridge_error(error: BridgeError, fallback: TransportOperation) -> AIError {
    match error {
        BridgeError::Provider { operation, shape } => error_from_shape(&shape, operation),
        BridgeError::Host(message) => AIError::transport(fallback, message, None, None),
    }
}

fn headers_of(value: Option<&Value>) -> IndexMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.to_lowercase(), s.to_string()))).collect())
        .unwrap_or_default()
}

/// `RequestExecutor.httpFailure`.
fn http_failure(message: String, url: Option<&str>, status: Option<u16>, data: Option<Value>, headers: IndexMap<String, String>, body: Option<String>, cause: Option<String>) -> AIError {
    let retry_after = retry_after_ms(&headers);
    let rate_limit = rate_limit_details(&headers, retry_after);
    let http = match (status, url) {
        (Some(status), Some(url)) => Some(HttpContext { url: url.to_string(), status, headers }),
        _ => None,
    };
    classify_provider_failure(ProviderFailure { message, status, raw_body: body, data, http, cause, retry_after_ms: retry_after, rate_limit })
}

/// `llmError(error, operation)` from the bridge's error shape:
///
/// - `{kind: "api", message, url?, statusCode?, data?, responseHeaders?, responseBody?, isRetryable, cause?}`:
///   an `APICallError` (message already resolved by `providerErrorMessage`);
/// - `{kind: "network", message, code?, cause?}`: a runtime network failure;
/// - `{kind: "unknown", message, data?, responseBody?, cause?}`: anything else (`unknownErrorMessage`).
pub fn error_from_shape(shape: &Value, operation: TransportOperation) -> AIError {
    let kind = shape.get("kind").and_then(Value::as_str).unwrap_or("unknown");
    let message = shape.get("message").and_then(Value::as_str).unwrap_or("Provider request failed").to_string();
    let cause = shape.get("cause").and_then(Value::as_str).map(String::from);
    let data = shape.get("data").cloned().filter(|d| !d.is_null());
    let body = shape.get("responseBody").and_then(Value::as_str).map(String::from);
    match kind {
        "api" => {
            let url = shape.get("url").and_then(Value::as_str);
            let status = shape.get("statusCode").and_then(Value::as_u64).map(|s| s as u16);
            let failure = http_failure(message, url, status, data, headers_of(shape.get("responseHeaders")), body, cause.clone());
            let retryable = shape.get("isRetryable").and_then(Value::as_bool).unwrap_or(false);
            if status.is_some() || !retryable {
                return failure;
            }
            let mut error = AIError::new(
                ErrorKind::Transport {
                    transport: TransportType::Http,
                    operation: TransportOperation::Request,
                    code: None,
                    url: url.map(String::from),
                    phase: None,
                    delivery: None,
                    recovery: None,
                },
                failure.message.clone(),
            );
            error.body = failure.body.clone();
            error.http = failure.http.clone();
            error.cause = failure.cause.clone();
            error
        }
        "network" => {
            let code = shape.get("code").and_then(Value::as_str);
            let mut error = AIError::transport(operation, message, code, None);
            error.cause = cause;
            error
        }
        _ => http_failure(message, None, None, data, IndexMap::new(), body, cause),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MediaPart, TextPart, ToolCallPart};

    struct NoBridge;
    impl AiSdkBridge for NoBridge {
        fn load(&self, _model: Value) -> BoxFuture<'static, Result<(), Option<String>>> {
            Box::pin(async { Ok(()) })
        }
        fn stream(&self, _call: StreamCall) -> BoxFuture<'static, Result<PartStream, BridgeError>> {
            Box::pin(async { Err(BridgeError::Host("none".into())) })
        }
    }

    fn info(package: &str) -> ModelInfo {
        let mut m = ModelInfo::default_for("fixture", "m1");
        m.package = Some(package.into());
        m.settings = Some(json!({ "baseURL": "http://x", "apiKey": "k", "custom": 1 }).as_object().cloned().unwrap());
        m
    }

    #[test]
    fn route_shape() {
        let model = language_model(&info("aisdk:@ai-sdk/perplexity"), Arc::new(NoBridge));
        assert_eq!(model.route.id, "ai-sdk:@ai-sdk/perplexity");
        assert_eq!(model.route.provider_metadata_key.as_deref(), Some("perplexity"));
        assert_eq!(model.route.defaults.provider_options, Some(json!({ "custom": 1 }).as_object().cloned().unwrap()));
        assert!(bridge_of(&model).is_some());
        assert_eq!(provider_option_key("@ai-sdk/openai-compatible", "acme.chat"), "acme");
        assert_eq!(provider_option_key("file:///x.js", "fx"), "fx");
    }

    #[test]
    fn lowering() {
        let model = language_model(&info("aisdk:my-provider"), Arc::new(NoBridge));
        let mut request = LLMRequest::new(model).system("sys a").system("").system("sys b");
        request.messages = vec![
            Message::user(vec![
                ContentPart::text("hi"),
                ContentPart::Media(MediaPart {
                    media: MediaAsset::from_source(MediaSource::Bytes { data: vec![1, 2], media_type: "image/png".into() }),
                    filename: Some("a.png".into()),
                    cache: None,
                    metadata: None,
                    provider_metadata: None,
                }),
            ]),
            Message::assistant(vec![
                ContentPart::Text(TextPart { text: "ok".into(), cache: None, metadata: None, provider_metadata: None }),
                ContentPart::ToolCall(ToolCallPart {
                    id: "c1".into(),
                    name: "read".into(),
                    namespace: Some("ns".into()),
                    input: json!({ "path": "x" }),
                    provider_executed: None,
                    cache: None,
                    metadata: None,
                    provider_metadata: None,
                }),
            ]),
            Message::tool(ToolResultPart {
                id: "c1".into(),
                name: "read".into(),
                namespace: Some("ns".into()),
                result: ToolResultValue::Content(vec![
                    ToolContent::Text { text: "body".into() },
                    ToolContent::File { uri: "data:image/png;base64,AAA".into(), mime: "image/png".into(), name: None },
                ]),
                provider_executed: None,
                cache: None,
                metadata: None,
                provider_metadata: None,
            }),
            Message::system("update"),
        ];
        request.provider_options = Some(json!({ "custom": 1 }).as_object().cloned().unwrap());
        let body = call_options(&request, "my-provider", "m1", "fixture").unwrap();
        assert_eq!(
            body["prompt"],
            json!([
                { "role": "system", "content": "sys a\n\nsys b" },
                { "role": "user", "content": [
                    { "type": "text", "text": "hi" },
                    { "type": "file", "mediaType": "image/png", "data": { "$bytes": "AQI=" }, "filename": "a.png" }
                ] },
                { "role": "assistant", "content": [
                    { "type": "text", "text": "ok" },
                    { "type": "tool-call", "toolCallId": "c1", "toolName": "ns_read", "input": { "path": "x" } }
                ] },
                { "role": "tool", "content": [
                    { "type": "tool-result", "toolCallId": "c1", "toolName": "ns_read",
                      "output": { "type": "content", "value": [{ "type": "text", "text": "body" }] } }
                ] },
                { "role": "user", "content": [
                    { "type": "text", "text": "Attached media from tool result:" },
                    { "type": "file", "mediaType": "image/png", "data": "AAA" }
                ] },
                { "role": "user", "content": [{ "type": "text", "text": "<system-update>\nupdate\n</system-update>" }] }
            ])
        );
        assert_eq!(body["tools"], json!([]));
        assert_eq!(body["providerOptions"], json!({ "fixture": { "custom": 1 } }));
    }

    #[test]
    fn stream_parts() {
        let mut state = StreamState::default();
        let mut events = vec![];
        for part in [
            json!({ "type": "stream-start", "warnings": [] }),
            json!({ "type": "reasoning-start", "id": "r" }),
            json!({ "type": "reasoning-delta", "id": "r", "delta": "think" }),
            json!({ "type": "text-delta", "id": "t", "delta": "he" }),
            json!({ "type": "text-delta", "id": "t", "delta": "llo" }),
            json!({ "type": "reasoning-end", "id": "r" }),
            json!({ "type": "text-end", "id": "t" }),
            json!({ "type": "text-end", "id": "t" }),
            json!({ "type": "tool-input-start", "id": "c", "toolName": "read" }),
            json!({ "type": "tool-input-delta", "id": "c", "delta": "{}" }),
            json!({ "type": "tool-input-end", "id": "c" }),
            json!({ "type": "tool-call", "toolCallId": "c", "toolName": "read", "input": "{\"a\":1}" }),
            json!({ "type": "tool-call", "toolCallId": "d", "toolName": "read", "input": "{oops" }),
            json!({ "type": "finish", "finishReason": { "unified": "other", "raw": "weird" },
                    "usage": { "inputTokens": { "total": 10, "noCache": 8, "cacheRead": 2 }, "outputTokens": { "total": 5, "reasoning": 1 } } }),
        ] {
            events.extend(state.events(&part).unwrap());
        }
        let names: Vec<&str> = events.iter().map(|e| e.type_name()).collect();
        assert_eq!(
            names,
            [
                "reasoning-start", "reasoning-delta", "text-start", "text-delta", "text-delta", "reasoning-end", "text-end",
                "tool-input-start", "tool-input-delta", "tool-input-end", "tool-call", "tool-input-error", "step-finish", "finish"
            ]
        );
        let LLMEvent::Finish { reason, usage, .. } = events.last().unwrap() else { panic!() };
        assert_eq!(reason.normalized, FinishReason::Unknown);
        assert_eq!(reason.raw.as_deref(), Some("weird"));
        let usage = usage.clone().unwrap();
        assert_eq!((usage.input_tokens, usage.non_cached_input_tokens, usage.cache_read_input_tokens), (Some(10), Some(8), Some(2)));
        assert_eq!((usage.output_tokens, usage.reasoning_tokens, usage.total_tokens), (Some(5), Some(1), Some(15)));
        let err = state.events(&json!({ "type": "error", "error": { "kind": "unknown", "message": "boom" } })).unwrap_err();
        assert_eq!(err.message, "boom");
    }

    #[test]
    fn errors() {
        let api = error_from_shape(
            &json!({ "kind": "api", "message": "Too many", "url": "http://x/v1", "statusCode": 429,
                     "responseHeaders": { "Retry-After": "2" }, "isRetryable": true }),
            TransportOperation::Request,
        );
        assert_eq!(api.tag(), "RateLimit");
        assert_eq!(api.retry_after_ms(), Some(2000.0));
        let net = error_from_shape(&json!({ "kind": "network", "message": "fetch failed", "code": "ECONNREFUSED" }), TransportOperation::Request);
        assert_eq!(net.tag(), "Transport");
        let retryable = error_from_shape(&json!({ "kind": "api", "message": "conn", "url": "http://x", "isRetryable": true }), TransportOperation::Read);
        assert_eq!(retryable.tag(), "Transport");
    }
}
