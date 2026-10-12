//! Gemini generateContent protocol (`protocols/gemini.ts`, `utils/gemini-generate-content.ts`) and the
//! Google / Vertex Gemini routes (`providers/google.ts`, `google-vertex.ts`, `google-vertex-shared.ts`).

pub mod adc;

use super::lifecycle::Lifecycle;
use super::shared::{self, fit_thinking_budget, flatten_tool_request, match_tool_choice, tool_file_media, tool_result_text, unsupported_content, wrapped_system_update, ToolChoiceMode};
use super::{decode_json_frame, route_label, Protocol, Step, StreamParser};
use crate::error::{classify_provider_failure, AIError, ProviderConfigurationError, ProviderFailure};
use crate::json::{encode, num, Object};
use crate::media::{MediaAsset, MediaSource};
use crate::route::framing::{Frame, Framing};
use crate::route::{Endpoint, Headers, Route};
use crate::types::*;
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

pub const ADAPTER: &str = "gemini";
const SKIP_THOUGHT_SIGNATURE_VALIDATOR: &str = "skip_thought_signature_validator";
const MIN_THINKING_BUDGET: f64 = 512.0;
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

pub type Fields = IndexMap<String, Option<Value>>;

pub fn finalize(fields: Fields) -> Value {
    Value::Object(fields.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect())
}

static GEMINI_ANY: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)(^|/)gemini-").unwrap());
static GEMINI_1_2: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)(^|/)gemini-(?:1|2)(?:[.-]|$)").unwrap());
static GEMINI_PRO: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)(^|/)gemini-pro(?:-vision)?$").unwrap());
static GEMINI_ROBOTICS: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)(^|/)gemini-robotics-er-1\.5(?:[.-]|$)").unwrap());
static GEMINI_25: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)gemini-2[.-]5(?:[.-]|$)").unwrap());
static GEMINI_MAJOR: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^gemini(?:-live)?-(\d+)").unwrap());

pub fn requires_thought_signature_fallback(model: &str) -> bool {
    GEMINI_ANY.is_match(model) && !GEMINI_1_2.is_match(model) && !GEMINI_PRO.is_match(model) && !GEMINI_ROBOTICS.is_match(model)
}

pub fn routes_legacy_tool_media(model: &str) -> bool {
    GEMINI_25.is_match(model)
}

pub fn omits_function_call_ids(model: &str) -> bool {
    GEMINI_MAJOR.captures(model).and_then(|c| c[1].parse::<u32>().ok()).map(|m| m < 3).unwrap_or(false)
}

const CONTENT_FILTER_REASONS: &[&str] = &[
    "IMAGE_SAFETY",
    "RECITATION",
    "SAFETY",
    "BLOCKLIST",
    "PROHIBITED_CONTENT",
    "SPII",
    "MODEL_ARMOR",
    "IMAGE_PROHIBITED_CONTENT",
    "IMAGE_RECITATION",
    "LANGUAGE",
];

pub fn content_filtered(reason: &str) -> bool {
    CONTENT_FILTER_REASONS.contains(&reason)
}

/// `GeminiGenerateContent.mediaPart`: Google file refs → `fileData`, else inline data.
pub fn media_part(route: &str, asset: &MediaAsset) -> Result<Value, AIError> {
    if let MediaSource::Ref { provider, id, .. } = &asset.source {
        if provider == "google" {
            return Ok(json!({"fileData": {"mimeType": asset.media_type(), "fileUri": id}}));
        }
    }
    let inline = shared::require_inline_media(route, asset)?;
    Ok(json!({"inlineData": {"mimeType": inline.mime, "data": inline.base64}}))
}

fn signature(pm: Option<&ProviderMetadata>, key: &str) -> Option<String> {
    pm?.get(key)?.as_object()?.get("thoughtSignature")?.as_str().map(String::from)
}

fn with_sig(mut o: Object, sig: Option<String>) -> Value {
    if let Some(s) = sig {
        o.insert("thoughtSignature".into(), json!(s));
    }
    Value::Object(o)
}

fn has_function_response(content: &Value) -> bool {
    content.get("parts").and_then(Value::as_array).map(|p| p.iter().any(|i| i.get("functionResponse").is_some())).unwrap_or(false)
}

fn lower_messages(request: &LLMRequest) -> Result<Vec<Value>, AIError> {
    let mut contents: Vec<Value> = vec![];
    let key = request.metadata_key();
    let model = request.model.id.as_str();
    let omit_ids = omits_function_call_ids(model);
    let legacy = routes_legacy_tool_media(model);
    let mut pending_media: Option<Vec<Value>> = None;
    for message in &request.messages {
        if message.role != Role::Tool {
            if let Some(media) = pending_media.take() {
                let mut parts = vec![json!({"text": "Attached media from tool result:"})];
                parts.extend(media);
                contents.push(json!({"role": "user", "parts": parts}));
            }
        }
        match message.role {
            Role::System => {
                let part = wrapped_system_update("Gemini", message)?;
                let merge = contents.last().map(|c| c.get("role").and_then(Value::as_str) == Some("user") && !has_function_response(c)).unwrap_or(false);
                if merge {
                    let last = contents.last_mut().unwrap();
                    match last.get_mut("parts").and_then(Value::as_array_mut) {
                        Some(parts) => parts.push(json!({"text": part.text})),
                        None => *last = json!({"role": "user", "parts": [{"text": part.text}]}),
                    }
                } else {
                    contents.push(json!({"role": "user", "parts": [{"text": part.text}]}));
                }
            }
            Role::User => {
                let mut parts = vec![];
                for part in &message.content {
                    match part {
                        ContentPart::Text(t) => parts.push(json!({"text": t.text})),
                        ContentPart::Media(m) => parts.push(media_part("Gemini", &m.media)?),
                        _ => return Err(unsupported_content("Gemini", Role::User, &["text", "media"])),
                    }
                }
                contents.push(json!({"role": "user", "parts": parts}));
            }
            Role::Assistant => {
                let mut parts = vec![];
                let mut has_signed = false;
                for part in &message.content {
                    match part {
                        ContentPart::Text(t) => {
                            let mut o = Object::new();
                            o.insert("text".into(), json!(t.text));
                            parts.push(with_sig(o, signature(t.provider_metadata.as_ref(), &key)));
                        }
                        ContentPart::Media(m) => {
                            let lowered = media_part("Gemini", &m.media)?;
                            parts.push(with_sig(lowered.as_object().unwrap().clone(), signature(m.provider_metadata.as_ref(), &key)));
                        }
                        ContentPart::Reasoning(r) => {
                            let mut o = Object::new();
                            o.insert("text".into(), json!(r.text));
                            o.insert("thought".into(), json!(true));
                            parts.push(with_sig(o, signature(r.provider_metadata.as_ref(), &key)));
                        }
                        ContentPart::ToolCall(c) => {
                            let mut call = Object::new();
                            if !omit_ids {
                                call.insert("id".into(), json!(c.id));
                            }
                            call.insert("name".into(), json!(c.name));
                            if !c.input.is_null() {
                                call.insert("args".into(), c.input.clone());
                            }
                            let sig = signature(c.provider_metadata.as_ref(), &key);
                            let resolved = sig.clone().or_else(|| {
                                if requires_thought_signature_fallback(model) && !has_signed {
                                    Some(SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string())
                                } else {
                                    None
                                }
                            });
                            let mut o = Object::new();
                            o.insert("functionCall".into(), Value::Object(call));
                            parts.push(with_sig(o, resolved));
                            if sig.is_some() {
                                has_signed = true;
                            }
                        }
                        _ => return Err(unsupported_content("Gemini", Role::Assistant, &["text", "reasoning", "tool-call", "media"])),
                    }
                }
                contents.push(json!({"role": "model", "parts": parts}));
            }
            Role::Tool => {
                let mut parts = vec![];
                for part in &message.content {
                    let ContentPart::ToolResult(r) = part else {
                        return Err(unsupported_content("Gemini", Role::Tool, &["tool-result"]));
                    };
                    let mut response = Object::new();
                    if !omit_ids {
                        response.insert("id".into(), json!(r.id));
                    }
                    response.insert("name".into(), json!(r.name));
                    match &r.result {
                        ToolResultValue::Content(items) => {
                            let (texts, files) = shared::content_text_and_files(items);
                            let mut media = vec![];
                            for (uri, mime, name) in files {
                                let inline = shared::require_inline_media("Gemini", &tool_file_media(uri, mime, name).media)?;
                                media.push(json!({"inlineData": {"mimeType": inline.mime, "data": inline.base64}}));
                            }
                            response.insert("response".into(), json!({"name": r.name, "content": texts.join("\n")}));
                            if legacy && !media.is_empty() {
                                pending_media.get_or_insert_with(Vec::new).extend(media);
                            } else if !media.is_empty() {
                                response.insert("parts".into(), Value::Array(media));
                            }
                        }
                        other => {
                            response.insert("response".into(), json!({"name": r.name, "content": tool_result_text(other)}));
                        }
                    }
                    parts.push(json!({"functionResponse": response}));
                }
                let merge = contents.last().map(|c| c.get("role").and_then(Value::as_str) == Some("user") && has_function_response(c)).unwrap_or(false);
                if merge {
                    contents.last_mut().unwrap()["parts"].as_array_mut().unwrap().extend(parts);
                } else {
                    contents.push(json!({"role": "user", "parts": parts}));
                }
            }
        }
    }
    if let Some(media) = pending_media.take() {
        let mut parts = vec![json!({"text": "Attached media from tool result:"})];
        parts.extend(media);
        contents.push(json!({"role": "user", "parts": parts}));
    }
    Ok(contents)
}

/// Lenient Gemini provider options.
struct Options {
    cached_content: Option<Value>,
    safety_settings: Option<Value>,
    service_tier: Option<Value>,
    thinking_config: Option<Object>,
}

fn decode_options(request: &LLMRequest) -> Options {
    let cached_content = request.po("cachedContent").filter(|v| v.is_string()).cloned();
    let safety_settings = request
        .po("safetySettings")
        .and_then(Value::as_array)
        .filter(|a| a.iter().all(|s| s.get("category").map(Value::is_string).unwrap_or(false) && s.get("threshold").map(Value::is_string).unwrap_or(false)))
        .map(|a| Value::Array(a.iter().map(|s| json!({"category": s["category"].clone(), "threshold": s["threshold"].clone()})).collect()));
    let service_tier = request.po("serviceTier").filter(|v| v.is_string()).cloned();
    let thinking_config = request.po("thinkingConfig").and_then(Value::as_object).map(|tc| {
        let mut out = Object::new();
        if let Some(b) = tc.get("thinkingBudget").filter(|v| v.is_number()) {
            out.insert("thinkingBudget".into(), b.clone());
        }
        if let Some(b) = tc.get("includeThoughts").filter(|v| v.is_boolean()) {
            out.insert("includeThoughts".into(), b.clone());
        }
        if let Some(b) = tc.get("thinkingLevel").filter(|v| v.is_string()) {
            out.insert("thinkingLevel".into(), b.clone());
        }
        out
    });
    Options { cached_content, safety_settings, service_tier, thinking_config }
}

/// Ordered Gemini body fields.
pub fn body_fields(request: &LLMRequest) -> Result<Fields, AIError> {
    let (tools, flattened) = flatten_tool_request(request);
    let has_tools = !tools.is_empty();
    let g = request.generation_options();
    let options = decode_options(request);
    let thinking = options.thinking_config.map(|tc| {
        let mut out = tc.clone();
        out.insert("includeThoughts".into(), tc.get("includeThoughts").cloned().unwrap_or(json!(true)));
        match tc.get("thinkingBudget").and_then(Value::as_f64) {
            Some(b) => out.insert("thinkingBudget".into(), num(fit_thinking_budget(b, g.max_tokens, MIN_THINKING_BUDGET))),
            None => out.shift_remove("thinkingBudget"),
        };
        if tc.get("thinkingBudget").is_none() {
            // `thinkingBudget: undefined` appended after includeThoughts → omitted.
        }
        Value::Object(out)
    });
    let mut gc = Object::new();
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            gc.insert(k.into(), v);
        }
    };
    put("maxOutputTokens", g.max_tokens.map(num));
    put("temperature", g.temperature.map(num));
    put("topP", g.top_p.map(num));
    put("topK", g.top_k.map(num));
    put("frequencyPenalty", g.frequency_penalty.map(num));
    put("presencePenalty", g.presence_penalty.map(num));
    put("seed", g.seed.map(num));
    put("stopSequences", g.stop.clone().map(|s| json!(s)));
    put("thinkingConfig", thinking);
    let tool_config = match (&request.tool_choice, has_tools) {
        (Some(choice), true) => Some(match match_tool_choice("Gemini", choice)? {
            ToolChoiceMode::Auto => json!({"functionCallingConfig": {"mode": "AUTO"}}),
            ToolChoiceMode::None => json!({"functionCallingConfig": {"mode": "NONE"}}),
            ToolChoiceMode::Required => json!({"functionCallingConfig": {"mode": "ANY"}}),
            ToolChoiceMode::Tool(name) => json!({"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": [name]}}),
        }),
        _ => None,
    };
    let mut f = Fields::new();
    f.insert("cachedContent".into(), options.cached_content);
    f.insert("contents".into(), Some(Value::Array(lower_messages(&flattened)?)));
    f.insert("safetySettings".into(), options.safety_settings);
    f.insert("serviceTier".into(), options.service_tier);
    f.insert(
        "systemInstruction".into(),
        if request.system.is_empty() { None } else { Some(json!({"parts": request.system.iter().map(|p| json!({"text": p.text})).collect::<Vec<_>>()})) },
    );
    f.insert(
        "tools".into(),
        if has_tools {
            Some(json!([{"functionDeclarations": tools.iter().map(|t| json!({"name": t.name, "description": t.description, "parametersJsonSchema": Value::Object(t.input_schema.clone())})).collect::<Vec<_>>()}]))
        } else {
            None
        },
    );
    f.insert("toolConfig".into(), tool_config);
    f.insert("generationConfig".into(), if gc.is_empty() { None } else { Some(Value::Object(gc)) });
    Ok(f)
}

pub struct Gemini;

impl Protocol for Gemini {
    fn id(&self) -> &str {
        ADAPTER
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        Ok(finalize(body_fields(request)?))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(GeminiParser::new(request))
    }
    fn sanitizer(&self) -> Option<&'static str> {
        Some("gemini")
    }
}

pub fn protocol() -> Arc<dyn Protocol> {
    Arc::new(Gemini)
}

/// `gemini` route (Google AI Studio).
pub fn route() -> Route {
    Route::new(
        ADAPTER,
        protocol(),
        Endpoint::dynamic(Some(DEFAULT_BASE_URL), |request, _| format!("/models/{}:streamGenerateContent?alt=sse", request.model.id)),
        Framing::Sse,
    )
    .initial_provider("google", Some("google"))
}

/// Vertex Gemini body: `serviceTier` removed, `labels` (string values) appended.
pub struct VertexGemini;

impl Protocol for VertexGemini {
    fn id(&self) -> &str {
        ADAPTER
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        let mut f = body_fields(request)?;
        f.shift_remove("serviceTier");
        let labels = request.po("labels").and_then(Value::as_object).map(|l| {
            Value::Object(l.iter().filter(|(_, v)| v.is_string()).map(|(k, v)| (k.clone(), v.clone())).collect())
        });
        f.insert("labels".into(), labels);
        Ok(finalize(f))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(GeminiParser::new(request))
    }
    fn sanitizer(&self) -> Option<&'static str> {
        Some("gemini")
    }
}

pub fn vertex_protocol() -> Arc<dyn Protocol> {
    Arc::new(VertexGemini)
}

// ---------------------------------------------------------------------------------------------
// Vertex helpers (`google-vertex-shared.ts`)
// ---------------------------------------------------------------------------------------------

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

pub fn vertex_project(value: Option<&str>) -> Option<String> {
    value
        .map(String::from)
        .or_else(|| env("GOOGLE_VERTEX_PROJECT"))
        .or_else(|| env("GOOGLE_CLOUD_PROJECT"))
        .or_else(|| env("GCP_PROJECT"))
        .or_else(|| env("GCLOUD_PROJECT"))
}

pub fn vertex_location(value: Option<&str>) -> String {
    value
        .map(String::from)
        .or_else(|| env("GOOGLE_VERTEX_LOCATION"))
        .or_else(|| env("GOOGLE_CLOUD_LOCATION"))
        .or_else(|| env("VERTEX_LOCATION"))
        .unwrap_or_else(|| "global".into())
}

pub fn vertex_host(location: &str) -> String {
    match location {
        "global" => "aiplatform.googleapis.com".into(),
        "eu" | "us" => format!("aiplatform.{location}.rep.googleapis.com"),
        other => format!("{other}-aiplatform.googleapis.com"),
    }
}

pub fn require_project(value: Option<String>) -> Result<String, ProviderConfigurationError> {
    value.ok_or_else(|| ProviderConfigurationError::new("google-vertex", "Google Vertex requires a project when baseURL is not configured"))
}

/// Express-mode API key resolution.
pub fn vertex_api_key(
    api_key: Option<&str>,
    access_token: Option<&str>,
    has_auth: bool,
    project: Option<&str>,
    location: Option<&str>,
) -> Result<Option<String>, ProviderConfigurationError> {
    if api_key.is_some() && (access_token.is_some() || has_auth) {
        return Err(ProviderConfigurationError::new("google-vertex", "Google Vertex apiKey cannot be combined with accessToken or auth"));
    }
    if access_token.is_some() || has_auth {
        return Ok(None);
    }
    if let Some(k) = api_key {
        return Ok(Some(k.into()));
    }
    if project.is_some() || location.is_some() {
        return Ok(None);
    }
    Ok(std::env::var("GOOGLE_VERTEX_API_KEY").ok())
}

/// Vertex OAuth auth: access token bearer, else Google ADC.
pub fn vertex_oauth(access_token: Option<&str>, project: Option<String>) -> Arc<dyn crate::route::Auth> {
    match access_token {
        Some(t) => crate::route::auth::bearer(crate::route::Credential::value(t)),
        None => adc::google_adc_auth(project),
    }
}

/// `google-vertex-gemini` route.
pub fn vertex_route() -> Route {
    Route::new(
        "google-vertex-gemini",
        vertex_protocol(),
        Endpoint::dynamic(None, |request, _| {
            let model = &request.model.id;
            let path = if model.starts_with("endpoints/") { model.clone() } else { format!("models/{model}") };
            format!("/{path}:streamGenerateContent?alt=sse")
        }),
        Framing::Sse,
    )
    .initial_provider("google-vertex", Some("vertex"))
    .headers_fn(|request| {
        let mut h = Headers::new();
        if let Some(tier) = request.po_str("serviceTier") {
            h.insert("x-vertex-ai-llm-shared-request-type".into(), tier.into());
        }
        h
    })
}

/// Vertex Gemini base URL rules.
pub fn vertex_gemini_base_url(
    base_url: Option<&str>,
    api_key: bool,
    location: &str,
    project: Option<String>,
    model_id: &str,
) -> Result<String, ProviderConfigurationError> {
    if let Some(b) = base_url {
        return Ok(b.into());
    }
    if api_key {
        return Ok("https://aiplatform.googleapis.com/v1/publishers/google".into());
    }
    let endpoint_model = model_id.starts_with("endpoints/");
    Ok(format!(
        "https://{}/v1beta1/projects/{}/locations/{location}{}",
        vertex_host(location),
        require_project(project)?,
        if endpoint_model { "" } else { "/publishers/google" }
    ))
}

// ---------------------------------------------------------------------------------------------
// Stream parsing
// ---------------------------------------------------------------------------------------------

fn map_finish(reason: Option<&str>, has_tool_calls: bool) -> Option<FinishReason> {
    Some(match reason {
        None => {
            if has_tool_calls {
                FinishReason::ToolCalls
            } else {
                FinishReason::Unknown
            }
        }
        Some("STOP") => {
            if has_tool_calls {
                FinishReason::ToolCalls
            } else {
                FinishReason::Stop
            }
        }
        Some("MAX_TOKENS") => FinishReason::Length,
        Some(r) if content_filtered(r) => FinishReason::ContentFilter,
        Some(
            "MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL" | "NO_IMAGE" | "TOO_MANY_TOOL_CALLS" | "MISSING_THOUGHT_SIGNATURE"
            | "MALFORMED_RESPONSE",
        ) => return None,
        Some(_) => FinishReason::Unknown,
    })
}

fn map_usage(usage: &Value, key: &str) -> Option<Usage> {
    if !usage.is_object() {
        return None;
    }
    let u = |k: &str| crate::json::get_u64(usage, k);
    let prompt = u("promptTokenCount");
    let cached = u("cachedContentTokenCount");
    let thoughts = u("thoughtsTokenCount");
    let visible = u("candidatesTokenCount");
    let output = visible.map(|v| v + thoughts.unwrap_or(0));
    let mut pm = Object::new();
    pm.insert(key.into(), usage.clone());
    Some(Usage {
        context_tokens: None,
        input_tokens: prompt,
        output_tokens: output,
        non_cached_input_tokens: shared::subtract_tokens(prompt, cached),
        cache_read_input_tokens: cached,
        cache_write_input_tokens: None,
        reasoning_tokens: thoughts,
        total_tokens: shared::total_tokens(prompt, output, u("totalTokenCount")),
        provider_metadata: Some(pm),
    })
}

pub struct GeminiParser {
    route: String,
    key: String,
    has_tool_calls: bool,
    lifecycle: Lifecycle,
    next_reasoning: u32,
    next_text: u32,
    reasoning_id: Option<String>,
    text_id: Option<String>,
    reasoning_sig: Option<String>,
    text_sig: Option<String>,
    seen_call_ids: HashSet<String>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    prompt_feedback: Option<Value>,
}

impl GeminiParser {
    pub fn new(request: &LLMRequest) -> Self {
        Self {
            route: route_label(request),
            key: request.metadata_key(),
            has_tool_calls: false,
            lifecycle: Lifecycle::new(),
            next_reasoning: 0,
            next_text: 0,
            reasoning_id: None,
            text_id: None,
            reasoning_sig: None,
            text_sig: None,
            seen_call_ids: HashSet::new(),
            finish_reason: None,
            usage: None,
            prompt_feedback: None,
        }
    }

    fn sig_meta(&self, sig: &Option<String>) -> Option<ProviderMetadata> {
        sig.as_ref().map(|s| {
            let mut o = Object::new();
            o.insert(self.key.clone(), json!({"thoughtSignature": s}));
            o
        })
    }

    fn close_reasoning(&mut self, events: &mut Vec<LLMEvent>) {
        if let Some(id) = self.reasoning_id.take() {
            let meta = self.sig_meta(&self.reasoning_sig);
            self.lifecycle.reasoning_end(events, &id, meta, None);
            self.reasoning_sig = None;
        }
    }

    fn close_text(&mut self, events: &mut Vec<LLMEvent>) {
        if let Some(id) = self.text_id.take() {
            let meta = self.sig_meta(&self.text_sig);
            self.lifecycle.text_end(events, &id, meta, None);
            self.text_sig = None;
        }
    }

    fn handle(&mut self, event: &Value) -> Result<Vec<LLMEvent>, AIError> {
        if let Some(error) = event.get("error") {
            if let Some(e) = error.as_object() {
                let message = e
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .or_else(|| e.get("status").and_then(Value::as_str).filter(|s| !s.is_empty()))
                    .unwrap_or("Gemini provider error");
                let status = e.get("code").and_then(Value::as_u64).map(|c| c as u16);
                return Err(classify_provider_failure(ProviderFailure { message: message.into(), status, raw_body: Some(encode(event)), ..Default::default() }));
            }
            return Err(AIError::invalid_output(&self.route, format!("Invalid {} stream event", self.route)).with_body(encode(event)));
        }
        if let Some(pf) = event.get("promptFeedback").filter(|v| !v.is_null()) {
            self.prompt_feedback = Some(pf.clone());
        }
        if let Some(usage) = event.get("usageMetadata").filter(|v| !v.is_null()) {
            if let Some(u) = map_usage(usage, &self.key) {
                self.usage = Some(u);
            }
        }
        let candidate = event.get("candidates").and_then(Value::as_array).and_then(|c| c.first());
        let finish = candidate.and_then(|c| c.get("finishReason")).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
        if let Some(f) = &finish {
            if map_finish(Some(f), self.has_tool_calls).is_none() {
                return Err(AIError::invalid_output(&self.route, format!("Gemini stopped with {f}")).with_body(encode(event)));
            }
        }
        let mut events = vec![];
        let content = candidate.and_then(|c| c.get("content")).filter(|c| c.is_object());
        if let Some(content) = content {
            for part in content.get("parts").and_then(Value::as_array).cloned().unwrap_or_default() {
                let Some(obj) = part.as_object() else {
                    return Err(AIError::invalid_output(ADAPTER, format!("Invalid {} stream event", self.route)).with_body(encode(event)));
                };
                if !["text", "inlineData", "functionCall", "functionResponse"].iter().any(|k| obj.contains_key(*k)) {
                    continue;
                }
                let sig = obj.get("thoughtSignature").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
                let thought = obj.get("thought").and_then(Value::as_bool).unwrap_or(false);
                let is_text = obj.get("text").map(Value::is_string).unwrap_or(false);
                if let Some(s) = &sig {
                    if thought && is_text {
                        self.reasoning_sig = Some(s.clone());
                    } else if is_text {
                        self.text_sig = Some(s.clone());
                    }
                }
                if let Some(inline) = obj.get("inlineData").filter(|_| !is_text) {
                    let (Some(mime), Some(data)) = (inline.get("mimeType").and_then(Value::as_str), inline.get("data").and_then(Value::as_str)) else {
                        return Err(AIError::invalid_output(ADAPTER, format!("Invalid {} stream event", self.route)).with_body(encode(event)));
                    };
                    self.lifecycle.step_start(&mut events);
                    let meta = self.sig_meta(&sig);
                    events.push(LLMEvent::Media { media: MediaAsset::base64(data, mime), provider_metadata: meta });
                    continue;
                }
                if is_text {
                    let text = obj["text"].as_str().unwrap();
                    if text.is_empty() {
                        continue;
                    }
                    if thought {
                        self.close_text(&mut events);
                        if self.reasoning_id.is_none() {
                            self.reasoning_id = Some(format!("reasoning-{}", self.next_reasoning));
                            self.next_reasoning += 1;
                        }
                        let id = self.reasoning_id.clone().unwrap();
                        let meta = self.sig_meta(&sig);
                        self.lifecycle.reasoning_delta(&mut events, &id, text, meta);
                        continue;
                    }
                    self.close_reasoning(&mut events);
                    if self.text_id.is_none() {
                        self.text_id = Some(format!("text-{}", self.next_text));
                        self.next_text += 1;
                    }
                    let id = self.text_id.clone().unwrap();
                    let meta = self.sig_meta(&self.text_sig);
                    self.lifecycle.text_delta(&mut events, &id, text, meta);
                    self.text_sig = None;
                    continue;
                }
                if let Some(call) = obj.get("functionCall") {
                    let Some(name) = call.get("name").and_then(Value::as_str) else {
                        return Err(AIError::invalid_output(ADAPTER, format!("Invalid {} stream event", self.route)).with_body(encode(event)));
                    };
                    let input = call.get("args").cloned().unwrap_or_else(|| json!({}));
                    let supplied = call.get("id").and_then(Value::as_str).map(String::from);
                    let duplicate = supplied.as_ref().map(|s| self.seen_call_ids.contains(s)).unwrap_or(false);
                    if let Some(s) = &supplied {
                        self.seen_call_ids.insert(s.clone());
                    }
                    let id = match supplied {
                        Some(s) if !duplicate => s,
                        _ => format!("tool_{}", uuid::Uuid::new_v4().simple()),
                    };
                    self.close_reasoning(&mut events);
                    self.close_text(&mut events);
                    self.lifecycle.step_start(&mut events);
                    let meta = self.sig_meta(&sig);
                    events.push(LLMEvent::ToolCall {
                        id,
                        name: name.into(),
                        namespace: None,
                        input,
                        provider_executed: None,
                        provider_metadata: meta,
                    });
                    self.has_tool_calls = true;
                }
            }
        }
        if finish.is_some() {
            self.finish_reason = finish;
        }
        Ok(events)
    }

    fn finish(&mut self) -> Vec<LLMEvent> {
        let block = if self.finish_reason.is_none() {
            self.prompt_feedback.as_ref().and_then(|p| p.get("blockReason")).and_then(Value::as_str).map(String::from)
        } else {
            None
        };
        let reason = self.finish_reason.clone().or_else(|| block.clone());
        if reason.is_none() && self.usage.is_none() {
            return vec![];
        }
        let mut events = vec![];
        self.close_reasoning(&mut events);
        self.close_text(&mut events);
        let normalized = if block.is_some() {
            FinishReason::ContentFilter
        } else {
            map_finish(reason.as_deref(), self.has_tool_calls).unwrap_or(FinishReason::Unknown)
        };
        let metadata = self.prompt_feedback.as_ref().map(|pf| {
            let mut o = Object::new();
            o.insert(self.key.clone(), json!({"promptFeedback": pf}));
            o
        });
        self.lifecycle.finish(&mut events, FinishReasonDetails::raw(normalized, reason), self.usage.clone(), metadata);
        events
    }
}

impl StreamParser for GeminiParser {
    fn step(&mut self, frame: &Frame) -> Result<Step, AIError> {
        let event = decode_json_frame(ADAPTER, &self.route, frame)?;
        Ok(Step::events(self.handle(&event)?))
    }
    fn on_halt(&mut self) -> Result<Vec<LLMEvent>, AIError> {
        Ok(self.finish())
    }
}
