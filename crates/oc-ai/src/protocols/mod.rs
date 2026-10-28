//! Wire protocols: request lowering and stream raising.

pub mod cloud_env;
pub mod lifecycle;
pub mod partial_json;
pub mod options;
pub mod shared;
pub mod tool_stream;

pub mod anthropic_messages;
pub mod bedrock_converse;
pub mod cohere_chat;
pub mod gemini;
pub mod google_interactions;
pub mod mistral_chat;
pub mod open_responses;
pub mod open_responses_channel;
pub mod openai_chat;
pub mod openai_chat_variants;
pub mod openai_responses;
pub mod responses_variants;

use crate::error::AIError;
use crate::route::framing::Frame;
use crate::types::{LLMEvent, LLMRequest};
use serde_json::Value;

/// Output of one parser step.
#[derive(Debug, Default)]
pub struct Step {
    pub events: Vec<LLMEvent>,
    /// The frame was terminal: stop consuming after it (`protocol.stream.terminal`).
    pub terminal: bool,
}

impl Step {
    pub fn events(events: Vec<LLMEvent>) -> Self {
        Self { events, terminal: false }
    }
    pub fn terminal(events: Vec<LLMEvent>) -> Self {
        Self { events, terminal: true }
    }
}

/// Streaming state machine for one response.
pub trait StreamParser: Send {
    /// Decode and handle one frame.
    fn step(&mut self, frame: &Frame) -> Result<Step, AIError>;
    /// Flush at end of the framed stream.
    fn on_halt(&mut self) -> Result<Vec<LLMEvent>, AIError> {
        Ok(vec![])
    }
    /// The transport starts another request for the same response (Gemini continuation).
    fn next_request(&mut self) {}
}

/// The semantic API contract of one model server family.
pub trait Protocol: Send + Sync {
    fn id(&self) -> &str;
    /// Lower the (compiled) request to the provider-native JSON body.
    fn body(&self, request: &LLMRequest) -> Result<Value, AIError>;
    /// Create the stream parser for one response.
    fn parser(&self, request: &LLMRequest) -> Box<dyn StreamParser>;
    /// Whether `body` lowers `Message::effort` markers.
    fn supports_effort_updates(&self, _request: &LLMRequest) -> bool {
        false
    }
    /// Tool schema sanitizer for every model on this protocol (`"gemini"`, `"moonshot"`).
    fn sanitizer(&self) -> Option<&'static str> {
        None
    }
}

/// `"<provider>/<route>"` label used in stream diagnostics.
pub fn route_label(request: &LLMRequest) -> String {
    format!("{}/{}", request.model.provider, request.model.route.id)
}

/// Decode a JSON text frame; failure → `Invalid <label> stream event`.
pub fn decode_json_frame(route: &str, label: &str, frame: &Frame) -> Result<Value, AIError> {
    match frame {
        Frame::Json(v) => Ok(v.clone()),
        Frame::Text(text) => serde_json::from_str::<Value>(text)
            .map_err(|e| AIError::invalid_output(route, format!("Invalid {label} stream event")).with_body(text.clone()).with_cause(e)),
    }
}
