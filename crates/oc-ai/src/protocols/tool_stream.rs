//! Streaming tool-call accumulator (`protocols/utils/tool-stream.ts`).

use super::partial_json;
use super::shared::parse_tool_input;
use crate::error::AIError;
use crate::types::{LLMEvent, ProviderMetadata};
use indexmap::IndexMap;

/// Stream-local tool key: content index or item id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StreamKey {
    Index(i64),
    Id(String),
}

impl From<i64> for StreamKey {
    fn from(v: i64) -> Self {
        Self::Index(v)
    }
}
impl From<usize> for StreamKey {
    fn from(v: usize) -> Self {
        Self::Index(v as i64)
    }
}
impl From<&str> for StreamKey {
    fn from(v: &str) -> Self {
        Self::Id(v.into())
    }
}
impl From<String> for StreamKey {
    fn from(v: String) -> Self {
        Self::Id(v)
    }
}

/// One pending streamed tool call; `input` is the raw JSON text so far.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingTool {
    pub id: String,
    pub name: String,
    pub namespace: Option<String>,
    pub input: String,
    pub provider_executed: bool,
    pub provider_metadata: Option<ProviderMetadata>,
}

impl PendingTool {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self { id: id.into(), name: name.into(), namespace: None, input: String::new(), provider_executed: false, provider_metadata: None }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolStream {
    pub route: String,
    pub tools: IndexMap<StreamKey, PendingTool>,
}

fn input_start(tool: &PendingTool) -> LLMEvent {
    LLMEvent::ToolInputStart {
        id: tool.id.clone(),
        name: tool.name.clone(),
        namespace: tool.namespace.clone(),
        provider_executed: if tool.provider_executed { Some(true) } else { None },
        provider_metadata: tool.provider_metadata.clone(),
    }
}

fn input_delta(tool: &PendingTool, text: &str) -> LLMEvent {
    LLMEvent::ToolInputDelta {
        id: tool.id.clone(),
        name: tool.name.clone(),
        namespace: tool.namespace.clone(),
        text: text.into(),
        input: Some(partial_json::parse(&tool.input)),
    }
}

impl ToolStream {
    pub fn new(route: &str) -> Self {
        Self { route: route.into(), tools: IndexMap::new() }
    }

    pub fn get(&self, key: &StreamKey) -> Option<&PendingTool> {
        self.tools.get(key)
    }

    pub fn contains(&self, key: &StreamKey) -> bool {
        self.tools.contains_key(key)
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Register a tool whose start event arrived before argument deltas (no events emitted).
    pub fn start(&mut self, key: impl Into<StreamKey>, tool: PendingTool) {
        self.tools.insert(key.into(), tool);
    }

    /// Start event for a registered tool.
    pub fn start_event(tool: &PendingTool) -> LLMEvent {
        input_start(tool)
    }

    fn append_tool(&mut self, key: StreamKey, tool: PendingTool, text: &str, events: &mut Vec<LLMEvent>) {
        if !self.tools.contains_key(&key) {
            events.push(input_start(&tool));
        }
        if !text.is_empty() {
            events.push(input_delta(&tool, text));
        }
        self.tools.insert(key, tool);
    }

    /// `appendOrStart`: start the tool from delta identity when unknown.
    pub fn append_or_start(
        &mut self,
        key: impl Into<StreamKey>,
        id: Option<&str>,
        name: Option<&str>,
        text: &str,
        provider_metadata: Option<ProviderMetadata>,
        missing_tool_message: &str,
        events: &mut Vec<LLMEvent>,
    ) -> Result<PendingTool, AIError> {
        let key = key.into();
        let current = self.tools.get(&key).cloned();
        let id = current.as_ref().map(|c| c.id.clone()).or_else(|| id.filter(|s| !s.is_empty()).map(String::from));
        let name = current.as_ref().map(|c| c.name.clone()).or_else(|| name.filter(|s| !s.is_empty()).map(String::from));
        let (Some(id), Some(name)) = (id, name) else {
            return Err(AIError::invalid_output(&self.route, missing_tool_message));
        };
        if let Some(current) = &current {
            if text.is_empty() && current.id == id && current.name == name {
                return Ok(current.clone());
            }
        }
        let tool = PendingTool {
            id,
            name,
            namespace: current.as_ref().and_then(|c| c.namespace.clone()),
            input: format!("{}{}", current.as_ref().map(|c| c.input.as_str()).unwrap_or(""), text),
            provider_executed: current.as_ref().map(|c| c.provider_executed).unwrap_or(false),
            provider_metadata: current.as_ref().and_then(|c| c.provider_metadata.clone()).or(provider_metadata),
        };
        self.append_tool(key, tool.clone(), text, events);
        Ok(tool)
    }

    /// `append`: returns `None` when no tool is open under `key`.
    pub fn append(&mut self, key: impl Into<StreamKey>, text: &str, events: &mut Vec<LLMEvent>) -> Option<PendingTool> {
        let key = key.into();
        let current = self.tools.get(&key)?.clone();
        if text.is_empty() {
            return Some(current);
        }
        let mut tool = current;
        tool.input.push_str(text);
        self.append_tool(key, tool.clone(), text, events);
        Some(tool)
    }

    /// `appendExisting`: error when the tool was never started.
    pub fn append_existing(
        &mut self,
        key: impl Into<StreamKey>,
        text: &str,
        missing_tool_message: &str,
        events: &mut Vec<LLMEvent>,
    ) -> Result<PendingTool, AIError> {
        let route = self.route.clone();
        self.append(key, text, events).ok_or_else(|| AIError::invalid_output(&route, missing_tool_message))
    }

    fn finish_tool(route: &str, tool: &PendingTool, input_override: Option<&str>, events: &mut Vec<LLMEvent>) -> Result<(), AIError> {
        let raw = input_override.unwrap_or(&tool.input);
        let event = match parse_tool_input(route, &tool.name, raw) {
            Ok(input) => LLMEvent::ToolCall {
                id: tool.id.clone(),
                name: tool.name.clone(),
                namespace: tool.namespace.clone(),
                input,
                provider_executed: if tool.provider_executed { Some(true) } else { None },
                provider_metadata: tool.provider_metadata.clone(),
            },
            Err(error) => {
                if tool.provider_executed {
                    return Err(error);
                }
                LLMEvent::ToolInputError {
                    id: tool.id.clone(),
                    name: tool.name.clone(),
                    namespace: tool.namespace.clone(),
                    raw: raw.to_string(),
                }
            }
        };
        events.push(LLMEvent::ToolInputEnd {
            id: tool.id.clone(),
            name: tool.name.clone(),
            namespace: tool.namespace.clone(),
            provider_metadata: tool.provider_metadata.clone(),
        });
        events.push(event);
        Ok(())
    }

    /// Finalize one tool (no-op when missing). Returns whether a tool was finished.
    pub fn finish(&mut self, key: impl Into<StreamKey>, events: &mut Vec<LLMEvent>) -> Result<bool, AIError> {
        let key = key.into();
        let Some(tool) = self.tools.shift_remove(&key) else { return Ok(false) };
        Self::finish_tool(&self.route, &tool, None, events)?;
        Ok(true)
    }

    /// Finalize with an authoritative final input string.
    pub fn finish_with_input(&mut self, key: impl Into<StreamKey>, input: &str, events: &mut Vec<LLMEvent>) -> Result<bool, AIError> {
        let key = key.into();
        let Some(tool) = self.tools.shift_remove(&key) else { return Ok(false) };
        Self::finish_tool(&self.route, &tool, Some(input), events)?;
        Ok(true)
    }

    /// Finalize all pending tools (JS object key order: integer keys ascending, then strings by insertion).
    pub fn finish_all(&mut self, events: &mut Vec<LLMEvent>) -> Result<(), AIError> {
        let tools = std::mem::take(&mut self.tools);
        let mut indexed: Vec<(i64, PendingTool)> = vec![];
        let mut named: Vec<PendingTool> = vec![];
        for (key, tool) in tools {
            match key {
                StreamKey::Index(i) if i >= 0 => indexed.push((i, tool)),
                _ => named.push(tool),
            }
        }
        indexed.sort_by_key(|(i, _)| *i);
        for tool in indexed.into_iter().map(|(_, t)| t).chain(named) {
            Self::finish_tool(&self.route, &tool, None, events)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stream_and_finish() {
        let mut ts = ToolStream::new("openai-chat");
        let mut events = vec![];
        ts.append_or_start(0i64, Some("call_1"), Some("read"), "{\"pa", None, "missing", &mut events).unwrap();
        ts.append_or_start(0i64, None, None, "th\":\"a\"}", None, "missing", &mut events).unwrap();
        ts.finish_all(&mut events).unwrap();
        let types: Vec<_> = events.iter().map(|e| e.type_name()).collect();
        assert_eq!(types, vec!["tool-input-start", "tool-input-delta", "tool-input-delta", "tool-input-end", "tool-call"]);
        match &events[4] {
            LLMEvent::ToolCall { input, .. } => assert_eq!(input, &json!({"path":"a"})),
            _ => panic!(),
        }
    }

    #[test]
    fn invalid_input_is_tool_input_error() {
        let mut ts = ToolStream::new("r");
        let mut events = vec![];
        ts.start(1i64, PendingTool { input: "{bad".into(), ..PendingTool::new("id", "t") });
        ts.finish(1i64, &mut events).unwrap();
        assert_eq!(events[1].type_name(), "tool-input-error");
        let err = ts.append_or_start(2i64, None, Some("x"), "", None, "missing id", &mut events).unwrap_err();
        assert_eq!(err.message, "missing id");
    }
}
