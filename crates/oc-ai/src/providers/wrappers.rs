//! Provider-level protocol wrappers (`providers/{groq,digitalocean,openrouter,moonshot,minimax}.ts`,
//! `protocols/utils/gateway-protocol.ts`, `providers/vercel-ai-gateway.ts`, `amazon-bedrock-mantle.ts`).

use crate::error::AIError;
use crate::json::{merge_records, num, Object};
use crate::protocols::anthropic_messages as am;
use crate::protocols::openai_chat::{self, body_fields, finalize, ChatOptions, ChatParser};
use crate::protocols::shared::{claude_version, fit_thinking_budget};
use crate::protocols::{Protocol, Step, StreamParser};
use crate::route::framing::Frame;
use crate::types::*;
use serde_json::{json, Value};
use std::sync::Arc;

fn invalid(route: &str, key: &str) -> AIError {
    AIError::invalid_request(format!("Invalid {route} provider option `{key}`"))
}

fn typed(request: &LLMRequest, key: &str, check: fn(&Value) -> bool, route: &str) -> Result<Option<Value>, AIError> {
    match request.po(key) {
        None => Ok(None),
        Some(v) if check(v) => Ok(Some(v.clone())),
        Some(_) => Err(invalid(route, key)),
    }
}

// ---------------------------------------------------------------------------------------------
// groq-chat
// ---------------------------------------------------------------------------------------------

pub struct GroqChat;

impl Protocol for GroqChat {
    fn id(&self) -> &str {
        "groq-chat"
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        // `parallelToolCalls` and `serviceTier` are the shared OpenAI Chat options now.
        let include = typed(request, "includeReasoning", Value::is_boolean, "Groq")?;
        let user = typed(request, "user", Value::is_string, "Groq")?;
        let gpt_oss = request.model.id.starts_with("openai/gpt-oss-");
        let mut f = body_fields(request, &ChatOptions::default())?;
        f.insert("reasoning_format".into(), if gpt_oss { None } else { Some(json!("parsed")) });
        f.insert("include_reasoning".into(), if gpt_oss { include } else { None });
        f.insert("user".into(), user);
        Ok(finalize(f))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(ChatParser::new(request))
    }
}

// ---------------------------------------------------------------------------------------------
// moonshot-chat / minimax-chat
// ---------------------------------------------------------------------------------------------

pub struct MoonshotChat;

impl Protocol for MoonshotChat {
    fn id(&self) -> &str {
        "moonshot-chat"
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        typed(request, "reasoningEffort", Value::is_string, "Moonshot Chat")?;
        let thinking = match request.po("thinking") {
            None => None,
            Some(v) => {
                let kind = v.get("type").and_then(Value::as_str).ok_or_else(|| invalid("Moonshot Chat", "thinking"))?;
                let mut o = Object::new();
                o.insert("type".into(), json!(kind));
                if let Some(keep) = v.get("keep") {
                    if !(keep.is_string() || keep.is_null()) {
                        return Err(invalid("Moonshot Chat", "thinking"));
                    }
                    o.insert("keep".into(), keep.clone());
                }
                Some(Value::Object(o))
            }
        };
        let mut f = body_fields(request, &ChatOptions::default())?;
        f.insert("thinking".into(), thinking);
        Ok(finalize(f))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(ChatParser::new(request))
    }
}

pub struct MinimaxChat;

impl Protocol for MinimaxChat {
    fn id(&self) -> &str {
        "minimax-chat"
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        let thinking = match request.po("thinking") {
            None => None,
            Some(v) => Some(json!({"type": v.get("type").and_then(Value::as_str).ok_or_else(|| invalid("MiniMax Chat", "thinking"))?})),
        };
        let split = typed(request, "reasoningSplit", Value::is_boolean, "MiniMax Chat")?;
        let mut f = body_fields(request, &ChatOptions::default())?;
        f.insert("thinking".into(), thinking);
        f.insert("reasoning_split".into(), Some(split.unwrap_or(json!(true))));
        Ok(finalize(f))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(ChatParser::new(request))
    }
}

// ---------------------------------------------------------------------------------------------
// openrouter
// ---------------------------------------------------------------------------------------------

pub struct OpenRouter;

/// The provider options OpenRouter decodes itself (`Options.fields`); everything else except
/// `promptCacheKey` passes through.
const OPENROUTER_KNOWN: &[&str] = &["usage", "models", "provider", "plugins", "web_search_options", "debug", "user", "reasoning"];

/// Project OpenRouter's reasoning fields onto a lowered assistant message.
fn openrouter_assistant(source: &Message, o: &mut Object) {
    let reasoning: String =
        source.content.iter().filter_map(|p| if let ContentPart::Reasoning(r) = p { Some(r.text.as_str()) } else { None }).collect();
    let details = o.get("reasoning_details").and_then(Value::as_array).cloned();
    o.shift_remove("reasoning_content");
    o.shift_remove("reasoning_text");
    let keep = !reasoning.is_empty() && details.as_ref().map(|d| !d.is_empty()).unwrap_or(false);
    if keep {
        o.insert("reasoning".into(), json!(reasoning));
    } else {
        o.shift_remove("reasoning");
    }
    match details {
        Some(d) => {
            o.insert("reasoning_details".into(), Value::Array(d));
        }
        None => {
            o.shift_remove("reasoning_details");
        }
    }
}

impl Protocol for OpenRouter {
    fn id(&self) -> &str {
        "openrouter-chat"
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        let body = body_fields(request, &ChatOptions { cache_control: true, assistant: Some(Arc::new(openrouter_assistant)) })?;
        let po = request.provider_options.clone().unwrap_or_default();
        // Pass-through options come first, so they can never override protocol fields.
        let mut f: openai_chat::Fields = po
            .iter()
            .filter(|(k, _)| !OPENROUTER_KNOWN.contains(&k.as_str()) && k.as_str() != "promptCacheKey")
            .map(|(k, v)| (k.clone(), Some(v.clone())))
            .collect();
        for (k, v) in body {
            f.insert(k, v);
        }
        // The known options decode leniently: a malformed value is dropped.
        let usage = match po.get("usage") {
            None | Some(Value::Bool(true)) => Some(json!({"include": true})),
            Some(Value::Bool(false)) => Some(json!({"include": false})),
            Some(v @ Value::Object(_)) => Some(v.clone()),
            _ => None,
        };
        if let Some(usage) = usage {
            f.insert("usage".into(), Some(usage));
        }
        let strings = |v: &Value| v.as_array().is_some_and(|a| a.iter().all(Value::is_string));
        let objects = |v: &Value| v.as_array().is_some_and(|a| a.iter().all(Value::is_object));
        for (k, check) in [
            ("models", &strings as &dyn Fn(&Value) -> bool),
            ("provider", &Value::is_object),
            ("plugins", &objects),
            ("web_search_options", &Value::is_object),
            ("debug", &Value::is_object),
            ("user", &Value::is_string),
        ] {
            if let Some(v) = po.get(k).filter(|v| check(v)) {
                f.insert(k.into(), Some(v.clone()));
            }
        }
        if let Some(Value::Object(reasoning)) = po.get("reasoning") {
            let mut r = reasoning.clone();
            if let Some(m) = r.get("max_tokens").and_then(Value::as_f64) {
                let max = request.generation.as_ref().and_then(|g| g.max_tokens);
                r.insert("max_tokens".into(), num(fit_thinking_budget(m, max, 1024.0)));
            }
            f.insert("reasoning".into(), Some(Value::Object(r)));
        }
        Ok(finalize(f))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(ChatParser::new(request))
    }
}

/// DigitalOcean: OpenAI Chat with cache markers.
pub fn digitalocean_protocol() -> Arc<dyn Protocol> {
    openai_chat::protocol_with(ChatOptions { cache_control: true, assistant: None })
}

// ---------------------------------------------------------------------------------------------
// Bedrock Mantle messages: effort updates only for Claude >= 5.1 unless overridden
// ---------------------------------------------------------------------------------------------

pub struct MantleMessages;

impl Protocol for MantleMessages {
    fn id(&self) -> &str {
        "anthropic-messages"
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        Ok(am::finalize(am::body_fields(request)?))
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(am::Parser::new(request))
    }
    fn supports_effort_updates(&self, request: &LLMRequest) -> bool {
        if !am::supports_effort_updates(request) {
            return false;
        }
        if request.model.compatibility.as_ref().and_then(|c| c.supports_effort_updates).is_some() {
            return true;
        }
        claude_version(&request.model.id).map(|v| v.major > 5 || (v.major == 5 && v.minor >= 1)).unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------------------------
// Vercel AI Gateway (`gatewayProtocol`)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub enum GatewayApi {
    Messages,
    Responses,
    Chat,
}

pub struct Gateway {
    pub id: &'static str,
    pub inner: Arc<dyn Protocol>,
    pub api: GatewayApi,
}

impl Gateway {
    fn prepare(&self, request: &LLMRequest) -> Result<(LLMRequest, Object), AIError> {
        const R: &str = "Vercel AI Gateway";
        let gateway = typed(request, "gateway", Value::is_object, R)?;
        let upstream = typed(request, "upstream", |v| v.as_object().map(|o| o.values().all(Value::is_object)).unwrap_or(false), R)?;
        let effort = typed(request, "reasoningEffort", Value::is_string, R)?;
        let thinking = match request.po("thinking") {
            None => None,
            Some(v) => {
                let o = v.as_object().ok_or_else(|| invalid(R, "thinking"))?;
                let kind = o.get("type").and_then(Value::as_str).ok_or_else(|| invalid(R, "thinking"))?.to_string();
                let budget = o.get("budgetTokens").or_else(|| o.get("budget_tokens")).and_then(Value::as_f64);
                Some((kind, budget))
            }
        };
        let cache_ttl = typed(request, "cacheTTL", Value::is_string, R)?;
        let anchors = typed(request, "cacheAnchorItems", Value::is_number, R)?;
        let provider_options: Option<Value> = match (&upstream, &gateway) {
            (None, None) => None,
            (Some(u), None) => Some(u.clone()),
            (u, Some(g)) => {
                let mut o = u.as_ref().and_then(Value::as_object).cloned().unwrap_or_default();
                o.insert("gateway".into(), g.clone());
                Some(Value::Object(o))
            }
        };
        let mut extra = Object::new();
        if let Some(p) = provider_options {
            extra.insert("providerOptions".into(), p);
        }
        let mut req = request.clone();
        match self.api {
            GatewayApi::Messages => {
                if let Some(effort) = effort.and_then(|e| e.as_str().map(String::from)) {
                    let enabled = effort != "none";
                    let current_thinking = request.po("thinking").cloned();
                    let po = req.provider_options.get_or_insert_with(Object::new);
                    if enabled {
                        po.insert("effort".into(), json!(effort));
                    } else {
                        po.shift_remove("effort");
                    }
                    po.insert(
                        "thinking".into(),
                        current_thinking.unwrap_or_else(|| json!({"type": if enabled { "adaptive" } else { "disabled" }})),
                    );
                }
            }
            GatewayApi::Responses => {
                if let Some(t) = cache_ttl {
                    extra.insert("cache_ttl".into(), t);
                }
                if let Some(a) = anchors {
                    extra.insert("cache_anchor_items".into(), a);
                }
            }
            GatewayApi::Chat => {
                if let Some((kind, budget)) = thinking {
                    let mut r = Object::new();
                    r.insert("enabled".into(), json!(kind != "disabled"));
                    if let Some(b) = budget {
                        r.insert("max_tokens".into(), num(b));
                    }
                    extra.insert("reasoning".into(), Value::Object(r));
                }
            }
        }
        Ok((req, extra))
    }
}

impl Protocol for Gateway {
    fn id(&self) -> &str {
        self.id
    }
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError> {
        let (prepared, extra) = self.prepare(request)?;
        let mut body = self.inner.body(&prepared)?;
        if let Some(o) = body.as_object_mut() {
            for (k, v) in extra {
                o.insert(k, v);
            }
        }
        Ok(body)
    }
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser> {
        Box::new(GatewayParser { inner: self.inner.parser(request), gateway: None })
    }
    fn supports_effort_updates(&self, request: &LLMRequest) -> bool {
        self.inner.supports_effort_updates(request)
    }
    fn sanitizer(&self) -> Option<&'static str> {
        self.inner.sanitizer()
    }
}

struct GatewayParser {
    inner: Box<dyn StreamParser>,
    gateway: Option<Object>,
}

impl GatewayParser {
    fn attach(&self, events: &mut [LLMEvent]) {
        let Some(gateway) = &self.gateway else { return };
        for e in events.iter_mut() {
            if let LLMEvent::Finish { provider_metadata, .. } = e {
                let mut pm = provider_metadata.clone().unwrap_or_default();
                pm.insert("gateway".into(), Value::Object(gateway.clone()));
                *provider_metadata = Some(pm);
            }
        }
    }
}

impl StreamParser for GatewayParser {
    fn step(&mut self, frame: &Frame) -> Result<Step, AIError> {
        if let Frame::Text(text) = frame {
            if let Ok(event) = serde_json::from_str::<Value>(text) {
                let pick = |v: Option<&Value>| v.and_then(|v| v.get("provider_metadata")).and_then(|p| p.get("gateway")).and_then(Value::as_object).cloned();
                let mut sources = vec![pick(Some(&event)), pick(event.get("response"))];
                if let Some(choices) = event.get("choices").and_then(Value::as_array) {
                    for c in choices {
                        sources.push(pick(c.get("delta")));
                    }
                }
                let mut items: Vec<Option<&Object>> = vec![self.gateway.as_ref()];
                items.extend(sources.iter().map(|s| s.as_ref()));
                self.gateway = merge_records(&items);
            }
        }
        let mut step = self.inner.step(frame)?;
        self.attach(&mut step.events);
        Ok(step)
    }
    fn on_halt(&mut self) -> Result<Vec<LLMEvent>, AIError> {
        let mut events = self.inner.on_halt()?;
        self.attach(&mut events);
        Ok(events)
    }
}
