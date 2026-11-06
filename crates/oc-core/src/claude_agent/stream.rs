//! Claude Code messages → AI SDK `LanguageModelV3StreamPart`s (`language-model/convert-claude-stream.ts`),
//! with usage accounting (`usage.ts`) and failure detection (`claude-agent-error.ts`).

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use super::convert;
use super::js;

fn s<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn top_level(message: &Value) -> bool {
    message.get("parent_tool_use_id").is_none_or(Value::is_null)
}

// ---------------------------------------------------------------------------------------------
// Usage (`usage.ts`)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct Counts {
    input: f64,
    read: f64,
    write: f64,
    output: f64,
    reasoning: f64,
}

#[derive(Debug, Default)]
pub struct Usage {
    messages: Vec<(String, Counts)>,
    last: Option<String>,
}

fn count(usage: &Value, key: &str) -> f64 {
    usage.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

impl Usage {
    pub fn update(&mut self, id: &str, usage: &Value) {
        let index = match self.messages.iter().position(|(k, _)| k == id) {
            Some(index) => index,
            None => {
                self.messages.push((id.to_string(), Counts::default()));
                self.last = Some(id.to_string());
                self.messages.len() - 1
            }
        };
        let counts = &mut self.messages[index].1;
        counts.input = counts.input.max(count(usage, "input_tokens"));
        counts.read = counts.read.max(count(usage, "cache_read_input_tokens"));
        counts.write = counts.write.max(count(usage, "cache_creation_input_tokens"));
        counts.output = counts.output.max(count(usage, "output_tokens"));
        let thinking = usage.get("output_tokens_details").map(|d| count(d, "thinking_tokens")).unwrap_or(0.0);
        counts.reasoning = counts.reasoning.max(thinking);
    }

    /// Result usage sums the whole turn; only apply it when there was a single model call.
    pub fn result(&mut self, usage: &Value) {
        if self.messages.len() <= 1 {
            let id = self.last.clone().unwrap_or_else(|| "result".into());
            self.update(&id, usage);
        }
    }

    pub fn snapshot(&self) -> Value {
        let output: f64 = self.messages.iter().map(|(_, c)| c.output).sum();
        let reasoning: f64 = self.messages.iter().map(|(_, c)| c.reasoning).sum();
        // Context occupancy is the last API call's input, not the sum of repeated contexts.
        let last = self.last.as_ref().and_then(|l| self.messages.iter().find(|(k, _)| k == l)).map(|(_, c)| *c);
        let mut input = Map::new();
        if let Some(c) = last {
            input.insert("total".into(), num(c.input + c.read + c.write));
            input.insert("noCache".into(), num(c.input));
            input.insert("cacheRead".into(), num(c.read));
            input.insert("cacheWrite".into(), num(c.write));
        }
        json!({
            "inputTokens": input,
            "outputTokens": { "total": num(output), "reasoning": num(reasoning), "text": num(output - reasoning) },
        })
    }
}

fn num(value: f64) -> Value {
    oc_ai::json::num(value)
}

// ---------------------------------------------------------------------------------------------
// Errors (`claude-agent-error.ts`)
// ---------------------------------------------------------------------------------------------

/// Keeps request-local diagnostics until the terminal result. A usage warning alone is not a failed
/// request, and Claude Code owns any retries before this result.
#[derive(Debug, Default)]
pub struct Failure {
    code: Option<String>,
    rejected: Option<Value>,
}

/// `new Date(seconds * 1000).toISOString()`.
fn iso(seconds: f64) -> String {
    let ms = (seconds * 1000.0) as i64;
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()).unwrap_or_default()
}

impl Failure {
    pub fn check(&mut self, message: &Value) -> Option<String> {
        let kind = s(message, "type").unwrap_or_default();
        if kind == "rate_limit_event" {
            let info = message.get("rate_limit_info").cloned().unwrap_or(Value::Null);
            self.rejected = (s(&info, "status") == Some("rejected")).then_some(info);
        }
        if kind == "assistant"
            && top_level(message)
            && let Some(error) = message.get("error").filter(|e| js_truthy(e))
        {
            self.code = Some(error.as_str().map(String::from).unwrap_or_else(|| js::stringify(error)));
        }
        if kind != "result" {
            return None;
        }
        let success = s(message, "subtype") == Some("success");
        let status = if success { message.get("api_error_status").and_then(Value::as_f64) } else { None };
        let http_error = status.is_some_and(|s| s >= 400.0);
        let is_error = message.get("is_error").is_some_and(js_truthy);
        if success && !is_error && !http_error {
            return None;
        }
        let result = if success {
            message.get("result").map(|v| v.as_str().map(String::from).unwrap_or_else(|| js::stringify(v))).unwrap_or_default()
        } else {
            message
                .get("errors")
                .and_then(Value::as_array)
                .map(|e| e.iter().map(|v| v.as_str().map(String::from).unwrap_or_else(|| js::stringify(v))).collect::<Vec<_>>().join("\n"))
                .unwrap_or_default()
        }
        .replace("Please run /login", "Run `claude auth login` in a terminal");
        let failure = match status {
            Some(status) if status >= 500.0 => {
                let lead = if status == 529.0 {
                    "Anthropic is overloaded (529).".to_string()
                } else {
                    format!("Anthropic returned HTTP {}.", js::number(status))
                };
                format!("{lead} Claude Code exhausted its retries. Try again shortly.")
            }
            _ => result,
        };
        let code = self.code.as_deref();
        let limited = code == Some("rate_limit") || (self.rejected.is_some() && status == Some(429.0));
        let window = self.rejected.as_ref().and_then(|r| s(r, "rateLimitType")).map(|t| t.replace('_', " "));
        let limit = if limited && failure.is_empty() {
            format!("Claude Code subscription limit reached{}.", window.filter(|w| !w.is_empty()).map(|w| format!(" ({w})")).unwrap_or_default())
        } else {
            failure
        };
        let resets = self.rejected.as_ref().and_then(|r| r.get("resetsAt")).and_then(Value::as_f64).filter(|v| *v != 0.0);
        let reset = match (limited, resets) {
            (true, Some(at)) => format!(" Resets at {}.", iso(at)),
            _ => String::new(),
        };
        let billing = if code == Some("billing_error") { " Check your Claude plan and extra-usage settings." } else { "" };
        let text = format!("{limit}{reset}{billing}");
        if !text.is_empty() {
            return Some(text);
        }
        if let Some(status) = status.filter(|s| *s != 0.0) {
            return Some(format!("Claude Code could not finish its reply (HTTP {})", js::number(status)));
        }
        Some("Claude Code could not finish its reply".into())
    }
}

fn js_truthy(value: &Value) -> bool {
    oc_ai::json::truthy(value)
}

/// The sidecar's `errorShape` of a plain `Error(message)` (`kind: "unknown"`), as the AI SDK route
/// classifies thrown and streamed errors.
pub fn error_shape(message: &str) -> Value {
    json!({ "kind": "unknown", "message": if message.trim().is_empty() { "Provider request failed" } else { message }, "data": {}, "cause": format!("Error: {message}") })
}

// ---------------------------------------------------------------------------------------------
// Stream conversion
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Block {
    Text,
    Reasoning,
}

/// `ConvertClaudeStream.make(tools)`.
pub struct Converter {
    tools: Vec<Value>,
    usage: Usage,
    failure: Failure,
    streamed: HashSet<String>,
    blocks: HashMap<i64, Block>,
    message_id: String,
    called_tools: bool,
    metadata: Option<Value>,
}

impl Converter {
    pub fn new(tools: Vec<Value>) -> Converter {
        Converter {
            tools,
            usage: Usage::default(),
            failure: Failure::default(),
            streamed: HashSet::new(),
            blocks: HashMap::new(),
            message_id: String::new(),
            called_tools: false,
            metadata: None,
        }
    }

    fn with_metadata(&self, mut part: Map<String, Value>) -> Value {
        if let Some(metadata) = &self.metadata {
            part.insert("providerMetadata".into(), metadata.clone());
        }
        Value::Object(part)
    }

    pub fn convert(&mut self, message: &Value) -> Vec<Value> {
        let mut parts = vec![];
        if let Some(error) = self.failure.check(message) {
            return vec![json!({ "type": "error", "error": error_shape(&error) })];
        }
        let kind = s(message, "type").unwrap_or_default();
        if kind == "stream_event" && top_level(message) {
            let event = message.get("event").cloned().unwrap_or(Value::Null);
            let index = event.get("index").and_then(Value::as_i64).unwrap_or(0);
            let id = || format!("{}:{}", self.message_id, index);
            match s(&event, "type").unwrap_or_default() {
                "message_start" => {
                    let start = event.get("message").cloned().unwrap_or(Value::Null);
                    self.message_id = s(&start, "id").unwrap_or_default().to_string();
                    self.metadata = None;
                    self.streamed.insert(self.message_id.clone());
                    self.usage.update(&self.message_id.clone(), start.get("usage").unwrap_or(&Value::Null));
                    let mut part = Map::new();
                    part.insert("type".into(), json!("response-metadata"));
                    part.insert("id".into(), json!(self.message_id));
                    if let Some(model) = start.get("model") {
                        part.insert("modelId".into(), model.clone());
                    }
                    parts.push(Value::Object(part));
                }
                "content_block_start" => {
                    let block = event.get("content_block").cloned().unwrap_or(Value::Null);
                    let id = id();
                    match s(&block, "type") {
                        Some("text") => {
                            self.blocks.insert(index, Block::Text);
                            parts.push(json!({ "type": "text-start", "id": id }));
                            if let Some(text) = s(&block, "text").filter(|t| !t.is_empty()) {
                                parts.push(json!({ "type": "text-delta", "id": id, "delta": text }));
                            }
                        }
                        Some("thinking") => {
                            self.blocks.insert(index, Block::Reasoning);
                            parts.push(json!({ "type": "reasoning-start", "id": id }));
                            if let Some(text) = s(&block, "thinking").filter(|t| !t.is_empty()) {
                                parts.push(json!({ "type": "reasoning-delta", "id": id, "delta": text }));
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_delta" => {
                    let delta = event.get("delta").cloned().unwrap_or(Value::Null);
                    match s(&delta, "type") {
                        Some("text_delta") => {
                            parts.push(json!({ "type": "text-delta", "id": id(), "delta": delta.get("text").cloned().unwrap_or(Value::Null) }))
                        }
                        Some("thinking_delta") => {
                            if let Some(text) = s(&delta, "thinking").filter(|t| !t.is_empty()) {
                                parts.push(json!({ "type": "reasoning-delta", "id": id(), "delta": text }));
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    if let Some(block) = self.blocks.remove(&index) {
                        let mut part = Map::new();
                        part.insert("type".into(), json!(if block == Block::Text { "text-end" } else { "reasoning-end" }));
                        part.insert("id".into(), json!(id()));
                        parts.push(self.with_metadata(part));
                    }
                }
                "message_delta" => self.usage.update(&self.message_id.clone(), event.get("usage").unwrap_or(&Value::Null)),
                // Must match the session's tool boundary, or OpenCode's step ends without a finish event.
                "message_stop" if self.called_tools => parts.push(json!({
                    "type": "finish",
                    "finishReason": { "unified": "tool-calls", "raw": "tool_use" },
                    "usage": self.usage.snapshot(),
                })),
                _ => {}
            }
        }
        if kind == "assistant" && top_level(message) {
            // Synthetic API errors are diagnostics, not assistant answers.
            if message.get("error").is_some_and(js_truthy) {
                return parts;
            }
            let body = message.get("message").cloned().unwrap_or(Value::Null);
            let uuid = s(message, "uuid").unwrap_or_default().to_string();
            let mut meta = Map::new();
            meta.insert("sessionID".into(), message.get("session_id").cloned().unwrap_or(Value::Null));
            meta.insert("messageID".into(), json!(uuid));
            self.metadata = Some(json!({ super::PROVIDER_ID: meta }));
            let api_id = s(&body, "id").unwrap_or_default().to_string();
            self.usage.update(&api_id, body.get("usage").unwrap_or(&Value::Null));
            let content = body.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
            for block in &content {
                if s(block, "type") != Some("tool_use") {
                    continue;
                }
                let Some(name) = convert::original(s(block, "name").unwrap_or_default(), &self.tools) else { continue };
                self.called_tools = true;
                let mut part = Map::new();
                part.insert("type".into(), json!("tool-call"));
                part.insert("toolCallId".into(), block.get("id").cloned().unwrap_or(Value::Null));
                part.insert("toolName".into(), json!(name));
                part.insert("input".into(), json!(js::stringify(block.get("input").unwrap_or(&Value::Null))));
                parts.push(self.with_metadata(part));
            }
            // Complete assistant messages repeat the streamed content. Only use them when nothing was streamed.
            if !self.streamed.contains(&api_id) {
                for (i, block) in content.iter().enumerate() {
                    let id = format!("{uuid}:{i}");
                    match s(block, "type") {
                        Some("text") => {
                            parts.push(json!({ "type": "text-start", "id": id }));
                            parts.push(json!({ "type": "text-delta", "id": id, "delta": block.get("text").cloned().unwrap_or(Value::Null) }));
                            let mut end = Map::new();
                            end.insert("type".into(), json!("text-end"));
                            end.insert("id".into(), json!(id));
                            parts.push(self.with_metadata(end));
                        }
                        Some("thinking") => {
                            parts.push(json!({ "type": "reasoning-start", "id": id }));
                            parts
                                .push(json!({ "type": "reasoning-delta", "id": id, "delta": block.get("thinking").cloned().unwrap_or(Value::Null) }));
                            parts.push(json!({ "type": "reasoning-end", "id": id }));
                        }
                        _ => {}
                    }
                }
            }
        }
        if kind == "result" {
            if message.get("num_turns").and_then(Value::as_f64) == Some(1.0) {
                self.usage.result(message.get("usage").unwrap_or(&Value::Null));
            }
            let stop = s(message, "stop_reason");
            let mut reason = Map::new();
            reason.insert("unified".into(), json!(if stop == Some("max_tokens") { "length" } else { "stop" }));
            if let Some(raw) = message.get("stop_reason").filter(|v| !v.is_null()) {
                reason.insert("raw".into(), raw.clone());
            }
            parts.push(json!({ "type": "finish", "finishReason": reason, "usage": self.usage.snapshot() }));
        }
        parts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(extra: Value) -> Value {
        let mut base = json!({ "type": "result", "subtype": "success", "is_error": false, "result": "", "num_turns": 1, "stop_reason": "end_turn", "usage": {} });
        for (k, v) in extra.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    }

    #[test]
    fn login_text_points_at_the_cli() {
        let mut f = Failure::default();
        let error = f.check(&result(json!({ "is_error": true, "result": "Invalid API key · Please run /login" })));
        assert_eq!(error.as_deref(), Some("Invalid API key · Run `claude auth login` in a terminal"));
    }

    #[test]
    fn server_errors_and_limits() {
        let mut f = Failure::default();
        assert_eq!(
            f.check(&result(json!({ "is_error": true, "api_error_status": 529 }))).as_deref(),
            Some("Anthropic is overloaded (529). Claude Code exhausted its retries. Try again shortly.")
        );
        let mut f = Failure::default();
        f.check(
            &json!({ "type": "rate_limit_event", "rate_limit_info": { "status": "rejected", "rateLimitType": "five_hour", "resetsAt": 1792213800 } }),
        );
        assert_eq!(
            f.check(&result(json!({ "is_error": true, "api_error_status": 429 }))).as_deref(),
            Some("Claude Code subscription limit reached (five hour). Resets at 2026-10-17T05:10:00.000Z.")
        );
        let mut f = Failure::default();
        f.check(&json!({ "type": "assistant", "parent_tool_use_id": null, "error": "billing_error", "message": {} }));
        assert_eq!(
            f.check(&result(json!({ "is_error": true, "result": "Credit balance is too low" }))).as_deref(),
            Some("Credit balance is too low Check your Claude plan and extra-usage settings.")
        );
        let mut f = Failure::default();
        assert_eq!(f.check(&result(json!({ "is_error": true }))).as_deref(), Some("Claude Code could not finish its reply"));
        let mut f = Failure::default();
        assert_eq!(
            f.check(&json!({ "type": "result", "subtype": "error_during_execution", "errors": ["a", "b"], "is_error": true })).as_deref(),
            Some("a\nb")
        );
        let mut f = Failure::default();
        assert!(f.check(&result(json!({ "result": "ok" }))).is_none());
    }

    #[test]
    fn usage_takes_the_last_call_input_and_sums_output() {
        let mut u = Usage::default();
        u.update("a", &json!({ "input_tokens": 2, "cache_read_input_tokens": 10, "cache_creation_input_tokens": 5, "output_tokens": 20 }));
        u.update("a", &json!({ "output_tokens": 30, "output_tokens_details": { "thinking_tokens": 4 } }));
        u.update("b", &json!({ "input_tokens": 3, "cache_read_input_tokens": 1, "output_tokens": 7 }));
        u.result(&json!({ "input_tokens": 999 }));
        assert_eq!(
            u.snapshot(),
            json!({ "inputTokens": { "total": 4, "noCache": 3, "cacheRead": 1, "cacheWrite": 0 }, "outputTokens": { "total": 37, "reasoning": 4, "text": 33 } })
        );
    }
}
