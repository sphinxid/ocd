//! Test helpers: recorded HTTP fixtures (`packages/http-recorder` cassette format) and stream decoding.

use crate::client::StreamDriver;
use crate::error::AIError;
use crate::types::{LLMEvent, LLMRequest};
use base64::Engine;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct Recording {
    pub version: u32,
    #[serde(default)]
    pub metadata: Value,
    pub interactions: Vec<Interaction>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Interaction {
    #[serde(default)]
    pub transport: Option<String>,
    pub request: RecordedRequest,
    pub response: RecordedResponse,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecordedRequest {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: serde_json::Map<String, Value>,
    #[serde(default)]
    pub body: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedResponse {
    pub status: u16,
    #[serde(default)]
    pub headers: serde_json::Map<String, Value>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub body_encoding: Option<String>,
}

impl RecordedResponse {
    /// Response body bytes (base64-decoded when `bodyEncoding == "base64"`).
    pub fn bytes(&self) -> Vec<u8> {
        if self.body_encoding.as_deref() == Some("base64") {
            base64::engine::general_purpose::STANDARD.decode(&self.body).expect("valid base64 body")
        } else {
            self.body.as_bytes().to_vec()
        }
    }
}

impl RecordedRequest {
    pub fn json_body(&self) -> Value {
        serde_json::from_str(&self.body).expect("recorded request body is JSON")
    }
}

/// Parse a recording from JSON text (use with `include_str!`).
pub fn recording(text: &str) -> Recording {
    serde_json::from_str(text).expect("valid recording")
}

/// Decode a full response body through the request's route framing + protocol parser.
pub fn decode(request: &LLMRequest, body: &[u8]) -> Vec<Result<LLMEvent, AIError>> {
    StreamDriver::decode_all(request, body)
}

/// Decode and unwrap every event (panics on error).
pub fn decode_ok(request: &LLMRequest, body: &[u8]) -> Vec<LLMEvent> {
    decode(request, body).into_iter().map(|r| r.expect("stream event")).collect()
}

/// Event type names, for compact assertions.
pub fn types(events: &[LLMEvent]) -> Vec<&'static str> {
    events.iter().map(LLMEvent::type_name).collect()
}

/// Compile a request (offline) and return the JSON body after overlay.
pub async fn compile_body(request: &LLMRequest) -> Result<Value, AIError> {
    Ok(crate::client::Client::new().compile(request).await?.prepared.json_body)
}

/// Compile a request (offline) and return the exact body text.
pub async fn compile_text(request: &LLMRequest) -> Result<String, AIError> {
    Ok(crate::client::Client::new().compile(request).await?.prepared.body)
}
