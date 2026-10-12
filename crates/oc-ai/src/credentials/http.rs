//! HTTP helpers shared by the OAuth flows.

use super::integration::FlowError;
use serde_json::Value;

/// Shared context for provider flows: HTTP client and the `User-Agent` (`opencode/<channel>/<version>/<name>`).
#[derive(Clone)]
pub struct FlowContext {
    pub http: reqwest::Client,
    pub user_agent: String,
}

impl FlowContext {
    pub fn new(user_agent: impl Into<String>) -> Self {
        Self { http: reqwest::Client::new(), user_agent: user_agent.into() }
    }

    pub fn with_client(http: reqwest::Client, user_agent: impl Into<String>) -> Self {
        Self { http, user_agent: user_agent.into() }
    }
}

pub(crate) fn transport(error: reqwest::Error) -> FlowError {
    FlowError::new(error.to_string())
}

/// `fetch` + `if (!response.ok) throw new Error("Request failed: <status>")` + `response.json()`.
pub(crate) async fn request_json(request: reqwest::RequestBuilder) -> Result<Value, FlowError> {
    let response = request.send().await.map_err(transport)?;
    let status = response.status();
    if !status.is_success() {
        return Err(FlowError::new(format!("Request failed: {}", status.as_u16())));
    }
    response.json::<Value>().await.map_err(transport)
}

pub(crate) fn str_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}
