//! Response assembly (`LLMResponse`, `schema/events.ts:405-742`).

use crate::types::*;
use indexmap::IndexMap;

#[derive(Debug, Clone)]
struct ContentAssembly {
    content_index: usize,
    text: String,
    provider_metadata: Option<ProviderMetadata>,
}

#[derive(Debug, Clone)]
struct ToolInputAssembly {
    name: String,
    namespace: Option<String>,
    text: String,
    provider_metadata: Option<ProviderMetadata>,
}

/// Incremental reducer state for one provider attempt.
#[derive(Debug, Clone)]
pub struct ResponseState {
    pub events: Vec<LLMEvent>,
    pub message: Message,
    pub usage: Option<Usage>,
    pub finish_reason: Option<FinishReasonDetails>,
    text_parts: IndexMap<String, ContentAssembly>,
    reasoning_parts: IndexMap<String, ContentAssembly>,
    tool_inputs: IndexMap<String, ToolInputAssembly>,
}

impl Default for ResponseState {
    fn default() -> Self {
        Self {
            events: vec![],
            message: Message::assistant(vec![]),
            usage: None,
            finish_reason: None,
            text_parts: IndexMap::new(),
            reasoning_parts: IndexMap::new(),
            tool_inputs: IndexMap::new(),
        }
    }
}

fn text_content(text: String, pm: Option<ProviderMetadata>) -> ContentPart {
    ContentPart::Text(TextPart { text, cache: None, metadata: None, provider_metadata: pm })
}

fn reasoning_content(text: String, pm: Option<ProviderMetadata>) -> ContentPart {
    ContentPart::Reasoning(ReasoningPart { text, encrypted: None, cache: None, metadata: None, provider_metadata: pm })
}

impl ResponseState {
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure(&mut self, reasoning: bool, id: &str, pm: Option<ProviderMetadata>) {
        let map = if reasoning { &self.reasoning_parts } else { &self.text_parts };
        if map.contains_key(id) {
            return;
        }
        let index = self.message.content.len();
        self.message.content.push(if reasoning { reasoning_content(String::new(), pm.clone()) } else { text_content(String::new(), pm.clone()) });
        let entry = ContentAssembly { content_index: index, text: String::new(), provider_metadata: pm };
        if reasoning {
            self.reasoning_parts.insert(id.into(), entry);
        } else {
            self.text_parts.insert(id.into(), entry);
        }
    }

    fn delta(&mut self, reasoning: bool, id: &str, text: &str, pm: Option<ProviderMetadata>) {
        self.ensure(reasoning, id, pm.clone());
        let map = if reasoning { &mut self.reasoning_parts } else { &mut self.text_parts };
        let Some(current) = map.get_mut(id) else { return };
        current.text.push_str(text);
        if pm.is_some() {
            current.provider_metadata = pm;
        }
        let part = if reasoning {
            reasoning_content(current.text.clone(), current.provider_metadata.clone())
        } else {
            text_content(current.text.clone(), current.provider_metadata.clone())
        };
        self.message.content[current.content_index] = part;
    }

    fn end(&mut self, reasoning: bool, id: &str, text: Option<&String>, pm: Option<ProviderMetadata>) {
        let map = if reasoning { &mut self.reasoning_parts } else { &mut self.text_parts };
        let Some(current) = map.get_mut(id) else { return };
        if let Some(t) = text {
            current.text = t.clone();
        }
        if pm.is_some() {
            current.provider_metadata = pm;
        }
        let part = if reasoning {
            reasoning_content(current.text.clone(), current.provider_metadata.clone())
        } else {
            text_content(current.text.clone(), current.provider_metadata.clone())
        };
        self.message.content[current.content_index] = part;
    }

    /// Fold one event (`LLMResponse.reduce`).
    pub fn reduce(&mut self, event: LLMEvent) {
        match &event {
            LLMEvent::Finish { usage, reason, .. } => {
                if usage.is_some() {
                    self.usage = usage.clone();
                }
                self.finish_reason = Some(reason.clone());
            }
            LLMEvent::ProviderError { .. } => {
                if self.finish_reason.is_none() {
                    self.finish_reason = Some(FinishReasonDetails::new(FinishReason::Error));
                }
            }
            other => {
                if let Some(u) = other.usage() {
                    self.usage = Some(u.clone());
                }
            }
        }
        match &event {
            LLMEvent::Compaction(part) => self.message.content.push(ContentPart::Compaction(part.clone())),
            LLMEvent::TextStart { id, provider_metadata } => self.ensure(false, id, provider_metadata.clone()),
            LLMEvent::TextDelta { id, text, provider_metadata } => self.delta(false, id, text, provider_metadata.clone()),
            LLMEvent::TextEnd { id, text, provider_metadata } => self.end(false, id, text.as_ref(), provider_metadata.clone()),
            LLMEvent::ReasoningStart { id, provider_metadata } => self.ensure(true, id, provider_metadata.clone()),
            LLMEvent::ReasoningDelta { id, text, provider_metadata } => self.delta(true, id, text, provider_metadata.clone()),
            LLMEvent::ReasoningEnd { id, text, provider_metadata } => self.end(true, id, text.as_ref(), provider_metadata.clone()),
            LLMEvent::ToolInputStart { id, name, namespace, provider_metadata, .. } => {
                self.tool_inputs.insert(
                    id.clone(),
                    ToolInputAssembly { name: name.clone(), namespace: namespace.clone(), text: String::new(), provider_metadata: provider_metadata.clone() },
                );
            }
            LLMEvent::ToolInputDelta { id, name, namespace, text, .. } => {
                let entry = self.tool_inputs.entry(id.clone()).or_insert_with(|| ToolInputAssembly {
                    name: name.clone(),
                    namespace: namespace.clone(),
                    text: String::new(),
                    provider_metadata: None,
                });
                entry.text.push_str(text);
            }
            LLMEvent::ToolInputEnd { id, name, namespace, provider_metadata } => {
                let entry = self.tool_inputs.entry(id.clone()).or_insert_with(|| ToolInputAssembly {
                    name: name.clone(),
                    namespace: namespace.clone(),
                    text: String::new(),
                    provider_metadata: None,
                });
                entry.name = name.clone();
                entry.namespace = namespace.clone();
                if provider_metadata.is_some() {
                    entry.provider_metadata = provider_metadata.clone();
                }
            }
            LLMEvent::ToolInputError { id, .. } => {
                self.tool_inputs.shift_remove(id);
            }
            LLMEvent::ToolCall { id, name, namespace, input, provider_executed, provider_metadata } => {
                self.tool_inputs.shift_remove(id);
                self.message.content.push(ContentPart::ToolCall(ToolCallPart {
                    id: id.clone(),
                    name: name.clone(),
                    namespace: namespace.clone(),
                    input: input.clone(),
                    provider_executed: *provider_executed,
                    cache: None,
                    metadata: None,
                    provider_metadata: provider_metadata.clone(),
                }));
            }
            LLMEvent::ToolResult { id, name, namespace, result, provider_executed, provider_metadata, .. } => {
                self.message.content.push(ContentPart::ToolResult(ToolResultPart {
                    id: id.clone(),
                    name: name.clone(),
                    namespace: namespace.clone(),
                    result: result.clone(),
                    provider_executed: *provider_executed,
                    cache: None,
                    metadata: None,
                    provider_metadata: provider_metadata.clone(),
                }));
            }
            LLMEvent::Media { media, provider_metadata } => {
                self.message.content.push(ContentPart::Media(MediaPart {
                    media: media.clone(),
                    filename: None,
                    cache: None,
                    metadata: None,
                    provider_metadata: provider_metadata.clone(),
                }));
            }
            _ => {}
        }
        self.events.push(event);
    }

    /// Completed response only after a terminal finish or provider error.
    pub fn complete(self) -> Option<LLMResponse> {
        let finish_reason = self.finish_reason?;
        Some(LLMResponse { message: self.message, events: self.events, usage: self.usage, finish_reason })
    }
}

/// Folded response of one call.
#[derive(Debug, Clone, PartialEq)]
pub struct LLMResponse {
    pub message: Message,
    pub events: Vec<LLMEvent>,
    pub usage: Option<Usage>,
    pub finish_reason: FinishReasonDetails,
}

fn join_fragments(events: &[LLMEvent], reasoning: bool) -> String {
    let mut parts: IndexMap<String, String> = IndexMap::new();
    for event in events {
        match (event, reasoning) {
            (LLMEvent::TextDelta { id, text, .. }, false) | (LLMEvent::ReasoningDelta { id, text, .. }, true) => {
                parts.entry(id.clone()).or_default().push_str(text);
            }
            (LLMEvent::TextEnd { id, text: Some(text), .. }, false) | (LLMEvent::ReasoningEnd { id, text: Some(text), .. }, true) => {
                parts.insert(id.clone(), text.clone());
            }
            _ => {}
        }
    }
    parts.into_values().collect()
}

impl LLMResponse {
    pub fn from_events(events: Vec<LLMEvent>) -> Option<LLMResponse> {
        let mut state = ResponseState::new();
        for e in events {
            state.reduce(e);
        }
        state.complete()
    }
    pub fn text(&self) -> String {
        join_fragments(&self.events, false)
    }
    pub fn reasoning(&self) -> String {
        join_fragments(&self.events, true)
    }
    pub fn tool_calls(&self) -> Vec<&LLMEvent> {
        self.events.iter().filter(|e| matches!(e, LLMEvent::ToolCall { .. })).collect()
    }
}
