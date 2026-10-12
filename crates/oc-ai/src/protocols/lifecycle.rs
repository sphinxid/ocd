//! Step/text/reasoning block lifecycle (`protocols/utils/lifecycle.ts`).

use crate::types::{FinishReasonDetails, LLMEvent, ProviderMetadata, Usage};
use indexmap::IndexSet;

#[derive(Debug, Default, Clone)]
pub struct Lifecycle {
    pub step_started: bool,
    pub text: IndexSet<String>,
    pub reasoning: IndexSet<String>,
}

impl Lifecycle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn step_start(&mut self, events: &mut Vec<LLMEvent>) {
        if self.step_started {
            return;
        }
        events.push(LLMEvent::StepStart { index: 0 });
        self.step_started = true;
    }

    pub fn text_start(&mut self, events: &mut Vec<LLMEvent>, id: &str, provider_metadata: Option<ProviderMetadata>) {
        if self.text.contains(id) {
            return;
        }
        self.step_start(events);
        events.push(LLMEvent::TextStart { id: id.into(), provider_metadata });
        self.text.insert(id.into());
    }

    pub fn text_delta(&mut self, events: &mut Vec<LLMEvent>, id: &str, text: &str, provider_metadata: Option<ProviderMetadata>) {
        self.text_start(events, id, None);
        events.push(LLMEvent::TextDelta { id: id.into(), text: text.into(), provider_metadata });
    }

    pub fn reasoning_start(&mut self, events: &mut Vec<LLMEvent>, id: &str, provider_metadata: Option<ProviderMetadata>) {
        if self.reasoning.contains(id) {
            return;
        }
        self.step_start(events);
        events.push(LLMEvent::ReasoningStart { id: id.into(), provider_metadata });
        self.reasoning.insert(id.into());
    }

    pub fn reasoning_delta(
        &mut self,
        events: &mut Vec<LLMEvent>,
        id: &str,
        text: &str,
        provider_metadata: Option<ProviderMetadata>,
    ) {
        self.reasoning_start(events, id, provider_metadata.clone());
        events.push(LLMEvent::ReasoningDelta { id: id.into(), text: text.into(), provider_metadata });
    }

    pub fn reasoning_end(
        &mut self,
        events: &mut Vec<LLMEvent>,
        id: &str,
        provider_metadata: Option<ProviderMetadata>,
        text: Option<String>,
    ) {
        if !self.reasoning.contains(id) {
            return;
        }
        self.step_start(events);
        events.push(LLMEvent::ReasoningEnd { id: id.into(), text, provider_metadata });
        self.reasoning.shift_remove(id);
    }

    pub fn text_end(&mut self, events: &mut Vec<LLMEvent>, id: &str, provider_metadata: Option<ProviderMetadata>, text: Option<String>) {
        if !self.text.contains(id) {
            return;
        }
        self.step_start(events);
        events.push(LLMEvent::TextEnd { id: id.into(), text, provider_metadata });
        self.text.shift_remove(id);
    }

    pub fn is_text_open(&self, id: &str) -> bool {
        self.text.contains(id)
    }

    pub fn is_reasoning_open(&self, id: &str) -> bool {
        self.reasoning.contains(id)
    }

    fn close_open_blocks(&mut self, events: &mut Vec<LLMEvent>) {
        for id in self.reasoning.drain(..) {
            events.push(LLMEvent::ReasoningEnd { id, text: None, provider_metadata: None });
        }
        for id in self.text.drain(..) {
            events.push(LLMEvent::TextEnd { id, text: None, provider_metadata: None });
        }
    }

    /// Emit step-start if needed, close open blocks, then `step-finish` + `finish`.
    pub fn finish(
        &mut self,
        events: &mut Vec<LLMEvent>,
        reason: FinishReasonDetails,
        usage: Option<Usage>,
        provider_metadata: Option<ProviderMetadata>,
    ) {
        self.step_start(events);
        self.close_open_blocks(events);
        events.push(LLMEvent::StepFinish {
            index: 0,
            reason: reason.clone(),
            usage: usage.clone(),
            provider_metadata: provider_metadata.clone(),
        });
        events.push(LLMEvent::Finish { reason, usage, provider_metadata });
        self.step_started = false;
    }
}
