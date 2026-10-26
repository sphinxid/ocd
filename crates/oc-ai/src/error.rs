//! `AIError` and provider-failure classification (`packages/ai/src/schema/errors.ts`,
//! `packages/ai/src/provider-error.ts`).

use indexmap::IndexMap;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::fmt;

/// `ProviderFailureClassification`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureClassification {
    ContextOverflow,
    PayloadTooLarge,
}

impl FailureClassification {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ContextOverflow => "context-overflow",
            Self::PayloadTooLarge => "payload-too-large",
        }
    }
}

/// `HttpContext`: response URL, status and (lower-cased) headers.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HttpContext {
    pub url: String,
    pub status: u16,
    pub headers: IndexMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRateLimitDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset: Option<IndexMap<String, String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportType {
    Http,
    Websocket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportOperation {
    Request,
    Read,
    Write,
}

/// Error reason, mirroring `AIErrorReason` tags.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "_tag")]
pub enum ErrorKind {
    InvalidRequest {
        #[serde(skip_serializing_if = "Option::is_none")]
        parameter: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        classification: Option<FailureClassification>,
    },
    UnsupportedOperation {
        operation: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        route: Option<String>,
    },
    NoRoute {
        route: String,
        provider: String,
        model: String,
    },
    Authentication,
    RateLimit {
        #[serde(rename = "retryAfterMs", skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<f64>,
        #[serde(rename = "rateLimit", skip_serializing_if = "Option::is_none")]
        rate_limit: Option<HttpRateLimitDetails>,
    },
    QuotaExceeded {
        #[serde(rename = "retryAfterMs", skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<f64>,
    },
    ContentPolicy,
    ProviderInternal {
        #[serde(rename = "retryAfterMs", skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<f64>,
    },
    Transport {
        transport: TransportType,
        operation: TransportOperation,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        phase: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        delivery: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        recovery: Option<String>,
    },
    InvalidProviderOutput {
        /// Only `"incomplete-stream"`.
        #[serde(skip_serializing_if = "Option::is_none")]
        classification: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        route: Option<String>,
    },
    UnknownProvider,
    Timeout {
        #[serde(rename = "timeoutMs", skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
}

impl ErrorKind {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::InvalidRequest { .. } => "InvalidRequest",
            Self::UnsupportedOperation { .. } => "UnsupportedOperation",
            Self::NoRoute { .. } => "NoRoute",
            Self::Authentication => "Authentication",
            Self::RateLimit { .. } => "RateLimit",
            Self::QuotaExceeded { .. } => "QuotaExceeded",
            Self::ContentPolicy => "ContentPolicy",
            Self::ProviderInternal { .. } => "ProviderInternal",
            Self::Transport { .. } => "Transport",
            Self::InvalidProviderOutput { .. } => "InvalidProviderOutput",
            Self::UnknownProvider => "UnknownProvider",
            Self::Timeout { .. } => "Timeout",
        }
    }
}

/// The single error type of this crate (`AIError` with its `reason`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AIError {
    #[serde(flatten)]
    pub kind: ErrorKind,
    pub message: String,
    /// Complete original response body or triggering event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpContext>,
    /// Diagnostic rendering of the underlying cause.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
}

/// Alias used by the session runner.
pub type ProviderError = AIError;

impl fmt::Display for AIError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AIError {}

impl AIError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), body: None, http: None, cause: None }
    }
    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }
    pub fn with_http(mut self, http: Option<HttpContext>) -> Self {
        if self.http.is_none() {
            self.http = http;
        }
        self
    }
    pub fn with_cause(mut self, cause: impl fmt::Display) -> Self {
        self.cause = Some(cause.to_string());
        self
    }
    pub fn tag(&self) -> &'static str {
        self.kind.tag()
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidRequest { parameter: None, classification: None }, message)
    }
    pub fn unsupported(operation: &str, message: impl Into<String>, provider: Option<&str>, route: Option<&str>) -> Self {
        Self::new(
            ErrorKind::UnsupportedOperation {
                operation: operation.into(),
                provider: provider.map(Into::into),
                route: route.map(Into::into),
            },
            message,
        )
    }
    /// `eventError(route, message, body?)` → InvalidProviderOutput.
    pub fn invalid_output(route: &str, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidProviderOutput { classification: None, route: Some(route.into()) }, message)
    }
    pub fn incomplete_stream(route: &str, message: impl Into<String>) -> Self {
        Self::new(
            ErrorKind::InvalidProviderOutput {
                classification: Some("incomplete-stream".into()),
                route: Some(route.into()),
            },
            message,
        )
    }
    pub fn authentication(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Authentication, message)
    }
    pub fn unknown_provider(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::UnknownProvider, message)
    }
    pub fn provider_internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::ProviderInternal { retry_after_ms: None }, message)
    }
    pub fn transport(
        operation: TransportOperation,
        message: impl Into<String>,
        code: Option<&str>,
        url: Option<&str>,
    ) -> Self {
        Self::new(
            ErrorKind::Transport {
                transport: TransportType::Http,
                operation,
                code: code.map(Into::into),
                url: url.map(Into::into),
                phase: None,
                delivery: None,
                recovery: None,
            },
            message,
        )
    }

    pub fn classification(&self) -> Option<FailureClassification> {
        match &self.kind {
            ErrorKind::InvalidRequest { classification, .. } => *classification,
            _ => None,
        }
    }

    pub fn is_context_overflow(&self) -> bool {
        self.classification() == Some(FailureClassification::ContextOverflow)
    }

    /// `retryAfterMs` for `RateLimit` / `ProviderInternal` reasons (the ones retries wait for; a
    /// `QuotaExceeded` reason also carries it, for the session error).
    pub fn retry_after_ms(&self) -> Option<f64> {
        match &self.kind {
            ErrorKind::RateLimit { retry_after_ms, .. } | ErrorKind::ProviderInternal { retry_after_ms } => {
                *retry_after_ms
            }
            _ => None,
        }
    }

    /// `isRetryable(error)` (`provider-error.ts:81-112`).
    pub fn is_retryable(&self) -> bool {
        if let Some(http) = &self.http {
            match http.headers.get("x-should-retry").map(String::as_str) {
                Some("true") => return true,
                Some("false") => return false,
                _ => {}
            }
        }
        match &self.kind {
            ErrorKind::RateLimit { .. } | ErrorKind::ProviderInternal { .. } => true,
            ErrorKind::Transport { delivery, operation, .. } => {
                delivery.as_deref() != Some("rejected")
                    && (delivery.as_deref() != Some("accepted") || *operation == TransportOperation::Read)
            }
            ErrorKind::InvalidProviderOutput { classification, .. } => {
                classification.as_deref() == Some("incomplete-stream")
            }
            ErrorKind::UnknownProvider => true,
            _ => false,
        }
    }

    /// Interrupted stream: incomplete-stream output or a transport read failure.
    pub fn is_interrupted_stream(&self) -> bool {
        match &self.kind {
            ErrorKind::InvalidProviderOutput { classification, .. } => {
                classification.as_deref() == Some("incomplete-stream")
            }
            ErrorKind::Transport { operation, .. } => *operation == TransportOperation::Read,
            _ => false,
        }
    }
}

/// `ProviderConfigurationError`: thrown while configuring a model, before any request exists.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct ProviderConfigurationError {
    pub provider: String,
    pub message: String,
}

impl ProviderConfigurationError {
    pub fn new(provider: impl Into<String>, message: impl Into<String>) -> Self {
        Self { provider: provider.into(), message: message.into() }
    }
}

// ---------------------------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------------------------

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid regex")
}

static OVERFLOW_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"(?i)prompt is too long",
        r"(?i)input is too long for (?:the )?(?:requested|selected) model",
        // OpenCode Go
        r"(?i)too large for the model'?s configured input limit",
        r"(?i)exceeded this model context window limit",
        r"(?i)exceeds the context window",
        r"(?i)exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
        r"(?i)input token count.*exceeds the maximum",
        r"(?i)number of input tokens exceeds maximum length",
        r"(?i)tokens in request more than max tokens allowed",
        r"(?i)maximum prompt length is \d+",
        r"(?i)reduce the length of the messages",
        r"(?i)requested input length \d+ exceeds maximum input length",
        r"(?i)maximum context length is \d+ tokens",
        r"(?i)exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
        r"(?i)input(?: \(\d+ tokens\))? is longer than the model'?s context length",
        r"(?i)exceeds the limit of \d+",
        r"(?i)exceeds the available context size",
        r"(?i)greater than the context length",
        r"(?i)`inputs` tokens \+ `max_new_tokens` must be <= \d+",
        r"(?i)context window exceeds limit",
        r"(?i)exceeded model token limit",
        r"(?i)context[_ ]length[_ ]exceeded",
        r"(?i)context length is only \d+ tokens",
        r"(?i)input length.*exceeds.*context length",
        // Z.ai code 1261 arrives as `Prompt too long`, `Prompt exceeds max length` or `Prompt 超长`.
        r"(?i)prompt (?:too long|exceeds max length|超长)",
        r"(?i)too large for model with \d+ maximum context length",
        r"(?i)prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
        r"(?i)model_context_window_exceeded",
        r"(?i)range of input length should be",
        r"(?i)too many tokens",
        r"(?i)token limit exceeded",
        r"(?i)request_too_large",
    ]
    .iter()
    .map(|p| re(p))
    .collect()
});

static PAYLOAD_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [r"(?i)request entity too large", r"(?i)payload too large", r"(?i)request too large"].iter().map(|p| re(p)).collect()
});

static EXCLUSIONS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"(?i)^(throttling error|service unavailable):",
        r"(?i)rate limit",
        r"(?i)too many requests",
        r"(?i)max[_ ]tokens must be less than",
    ]
    .iter()
    .map(|p| re(p))
    .collect()
});

static NO_BODY: Lazy<Regex> = Lazy::new(|| re(r"(?i)^4(?:00|13)\s*(status code)?\s*\(no body\)"));

/// JS regex `.` does not match newlines; Rust's default `.` also excludes `\n` only. JS also excludes
/// `\r`, ` `, ` ` — irrelevant for the patterns used here.
pub fn is_context_overflow(message: &str) -> bool {
    !EXCLUSIONS.iter().any(|p| p.is_match(message))
        && (OVERFLOW_PATTERNS.iter().any(|p| p.is_match(message)) || NO_BODY.is_match(message))
}

pub fn is_payload_too_large(message: &str) -> bool {
    PAYLOAD_PATTERNS.iter().any(|p| p.is_match(message))
}

const QUOTA_CODES: &[&str] = &[
    "insufficient_quota",
    "usage_not_included",
    "billing_error",
    "gousagelimiterror",
    "freeusagelimiterror",
    "creditlimitexceeded",
];
const AUTH_CODES: &[&str] = &["authentication_error", "permission_error", "permission_denied", "api_key_invalid"];
const SERVER_CODES: &[&str] = &[
    "api_error",
    "internal_error",
    "internalserverexception",
    "modelstreamerrorexception",
    "overloaded_error",
    "server_error",
    "server_is_overloaded",
    "slow_down",
    "serviceunavailableexception",
];
const INVALID_REQUEST_CODES: &[&str] =
    &["model_not_found", "invalid_prompt", "invalid_request", "invalid_request_error", "validationexception"];
const CONTENT_POLICY_CODES: &[&str] = &[
    "content_filter",
    "content_moderation",
    "responsibleaipolicyviolation",
    "content_policy_violation",
    "image_content_policy_violation",
    "refusal",
];

static GATEWAY_CODE_LABEL: Lazy<Regex> = Lazy::new(|| re(r"^[^:\n]+: \[([A-Za-z0-9_.-]+)\]"));
static AUTH_TEXT: Lazy<Regex> = Lazy::new(|| re(r"(?i)incorrect api key provided"));
static RATE_LIMIT_TEXT: Lazy<Regex> =
    Lazy::new(|| re(r"(?i)rate increased too quickly|rate[-_\s]?limit|too[_\s]?many[_\s]?requests"));
static QUOTA_TEXT: Lazy<Regex> = Lazy::new(|| {
    re(r"(?i)insufficient[-_\s]?(?:quota|balance)|quota[-_\s]?exceeded|budget exceeded|usage limit|limit exhausted|package has expired|plan does not yet include")
});
static CONTENT_POLICY_TEXT: Lazy<Regex> = Lazy::new(|| {
    re(r"(?i)violating our usage policy|blocked by content filtering policy|content[-_\s]?policy|rejected as a result of our safety system|detected potentially unsafe or sensitive content")
});
static SERVER_ERROR_TEXT: Lazy<Regex> = Lazy::new(|| {
    re(r"(?i)\b(?:try again|(?:please |you can )?retry (?:the |this |your )?request|try (?:the |this |your )?request again|(?:currently |temporarily )?at capacity|overloaded|temporarily unavailable|service[-_\s]?unavailable|(?:server|internal)[-_\s]?error|server (?:is )?busy|provider returned (?:an )?error|resource[-_\s]?exhausted|upstream (?:connect|connection|request)|request buffer limit while retrying upstream)\b")
});

/// Input to [`classify_provider_failure`] (`ProviderFailure`).
#[derive(Debug, Clone, Default)]
pub struct ProviderFailure {
    pub message: String,
    pub status: Option<u16>,
    pub raw_body: Option<String>,
    pub data: Option<Value>,
    pub http: Option<HttpContext>,
    pub cause: Option<String>,
    pub retry_after_ms: Option<f64>,
    pub rate_limit: Option<HttpRateLimitDetails>,
}

impl ProviderFailure {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), ..Default::default() }
    }
    pub fn status(mut self, status: Option<u16>) -> Self {
        self.status = status;
        self
    }
    pub fn raw_body(mut self, body: impl Into<String>) -> Self {
        self.raw_body = Some(body.into());
        self
    }
}

fn as_obj(v: Option<&Value>) -> Option<&serde_json::Map<String, Value>> {
    v.and_then(|v| v.as_object())
}

fn provider_codes_value(decoded: &Value) -> Vec<String> {
    let Some(obj) = decoded.as_object() else { return vec![] };
    let error = as_obj(obj.get("error"));
    let inner = error.and_then(|e| as_obj(e.get("innererror")));
    let metadata = error.and_then(|e| as_obj(e.get("metadata")));
    let response = as_obj(obj.get("response"));
    let response_error = response.and_then(|r| as_obj(r.get("error")));
    let exception = as_obj(obj.get("exception"));
    let mut values: Vec<Option<&Value>> = vec![
        obj.get("code"),
        if obj.get("errors").map(Value::is_array).unwrap_or(false) { obj.get("name") } else { None },
        obj.get("error_type"),
        error.and_then(|e| e.get("code")),
        error.and_then(|e| e.get("type")),
        error.and_then(|e| e.get("status")),
        error.and_then(|e| e.get("error_type")),
    ];
    if let Some(details) = error.and_then(|e| e.get("details")).and_then(Value::as_array) {
        for detail in details {
            values.push(detail.as_object().and_then(|d| d.get("reason")));
        }
    }
    values.push(inner.and_then(|i| i.get("code")));
    values.push(metadata.and_then(|m| m.get("error_type")));
    values.push(response_error.and_then(|r| r.get("code")));
    values.push(response.and_then(|r| r.get("error_type")));
    values.push(exception.and_then(|e| e.get("type")));
    values.into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect()
}

fn provider_codes_str(text: &str) -> Vec<String> {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => provider_codes_value(&v),
        Err(_) => vec![],
    }
}

/// `classifyProviderFailure` (`provider-error.ts`).
pub fn classify_provider_failure(input: ProviderFailure) -> AIError {
    let body = input.raw_body.clone().unwrap_or_default();
    let mut codes: Vec<String> = Vec::new();
    if let Some(data) = &input.data {
        match data {
            Value::String(s) => codes.extend(provider_codes_str(s)),
            other => codes.extend(provider_codes_value(other)),
        }
    }
    codes.extend(provider_codes_str(&body));
    codes.extend(provider_codes_str(&input.message));
    if let Some(cap) = GATEWAY_CODE_LABEL.captures(&input.message) {
        codes.push(cap[1].to_string());
    }
    let codes: Vec<String> = codes.into_iter().map(|c| c.to_lowercase()).collect();
    let has = |set: &[&str]| codes.iter().any(|c| set.contains(&c.as_str()));
    let text = [input.message.as_str(), body.as_str()].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join("\n");
    let status = input.status;
    let client_scoped = status.map(|s| (400..500).contains(&s)).unwrap_or(true);

    let make = |kind: ErrorKind| AIError {
        kind,
        message: input.message.clone(),
        body: input.raw_body.clone(),
        http: input.http.clone(),
        cause: input.cause.clone(),
    };

    if client_scoped
        && (codes.iter().any(|c| c == "context_length_exceeded" || c == "model_context_window_exceeded" || c == "request_too_large")
            || is_context_overflow(&text))
    {
        return make(ErrorKind::InvalidRequest {
            parameter: None,
            classification: Some(FailureClassification::ContextOverflow),
        });
    }
    if status == Some(413) || is_payload_too_large(&text) {
        return make(ErrorKind::InvalidRequest {
            parameter: None,
            classification: Some(FailureClassification::PayloadTooLarge),
        });
    }
    if has(CONTENT_POLICY_CODES) || (client_scoped && CONTENT_POLICY_TEXT.is_match(&input.message)) {
        return make(ErrorKind::ContentPolicy);
    }
    if status == Some(402) || has(QUOTA_CODES) || (status == Some(429) && QUOTA_TEXT.is_match(&text)) {
        return make(ErrorKind::QuotaExceeded { retry_after_ms: input.retry_after_ms });
    }
    if status == Some(401) || status == Some(403) || has(AUTH_CODES) || (status == Some(400) && AUTH_TEXT.is_match(&text)) {
        return make(ErrorKind::Authentication);
    }
    if status == Some(429)
        || codes.iter().any(|c| c.contains("rate_limit") || c == "too_many_requests" || c == "throttlingexception")
        || RATE_LIMIT_TEXT.is_match(&text)
    {
        return make(ErrorKind::RateLimit { retry_after_ms: input.retry_after_ms, rate_limit: input.rate_limit.clone() });
    }
    let below_400 = status.map(|s| s < 400).unwrap_or(true);
    if status == Some(408)
        || status == Some(409)
        || status.map(|s| s >= 500).unwrap_or(false)
        || (below_400
            && ((!has(INVALID_REQUEST_CODES) && SERVER_ERROR_TEXT.is_match(&text))
                || codes.iter().any(|c| SERVER_CODES.contains(&c.as_str()) || c.contains("exhausted") || c.contains("unavailable"))))
    {
        return make(ErrorKind::ProviderInternal { retry_after_ms: input.retry_after_ms });
    }
    if has(INVALID_REQUEST_CODES) {
        return make(ErrorKind::InvalidRequest { parameter: None, classification: None });
    }
    if status.map(|s| (400..500).contains(&s)).unwrap_or(false) {
        return make(ErrorKind::InvalidRequest { parameter: None, classification: None });
    }
    make(ErrorKind::UnknownProvider)
}

fn non_blank(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).filter(|s| s.chars().any(|c| !c.is_whitespace())).map(String::from)
}

/// `providerErrorMessage(body)`: the human-readable message in common error body layouts.
pub fn provider_error_message(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let obj = value.as_object()?;
    if let Some(m) = obj.get("error").and_then(|e| e.as_object()).and_then(|e| non_blank(e.get("message"))) {
        return Some(m);
    }
    if let Some(m) = non_blank(obj.get("error")) {
        return Some(m);
    }
    if let Some(m) = non_blank(obj.get("message")) {
        return Some(m);
    }
    if let Some(m) = non_blank(obj.get("Message")) {
        return Some(m);
    }
    if let Some(m) = non_blank(obj.get("detail")) {
        return Some(m);
    }
    if let Some(first) = obj.get("errors").and_then(Value::as_array).and_then(|a| a.first()) {
        if let Some(m) = first.as_object().and_then(|f| non_blank(f.get("message"))) {
            return Some(m);
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// HTTP failure helpers (`route/executor.ts`)
// ---------------------------------------------------------------------------------------------

/// A non-negative whole number of milliseconds (`Math.min(MAX_SAFE_INTEGER, Math.max(0, Math.ceil(ms)))`).
fn clamp_retry_after_ms(millis: f64) -> f64 {
    millis.ceil().max(0.0).min(9_007_199_254_740_991.0)
}

/// Parse `retry-after-ms` / `retry-after` (seconds or HTTP date) from lower-cased headers.
pub fn retry_after_ms(headers: &IndexMap<String, String>) -> Option<f64> {
    if let Some(ms) = headers.get("retry-after-ms").and_then(|v| v.trim().parse::<f64>().ok()) {
        if ms.is_finite() {
            return Some(clamp_retry_after_ms(ms));
        }
    }
    let value = headers.get("retry-after")?;
    if value.is_empty() {
        return None;
    }
    if let Ok(seconds) = value.trim().parse::<f64>() {
        if seconds.is_finite() {
            return Some(clamp_retry_after_ms(seconds * 1000.0));
        }
    }
    if let Ok(date) = chrono::DateTime::parse_from_rfc2822(value.trim()) {
        let diff = date.timestamp_millis() - chrono::Utc::now().timestamp_millis();
        return Some(clamp_retry_after_ms(diff as f64));
    }
    None
}

static OPENAI_LIMIT: Lazy<Regex> = Lazy::new(|| re(r"^x-ratelimit-limit-(.+)$"));
static OPENAI_REMAINING: Lazy<Regex> = Lazy::new(|| re(r"^x-ratelimit-remaining-(.+)$"));
static OPENAI_RESET: Lazy<Regex> = Lazy::new(|| re(r"^x-ratelimit-reset-(.+)$"));
static ANTHROPIC_RL: Lazy<Regex> = Lazy::new(|| re(r"^anthropic-ratelimit-(.+)-(limit|remaining|reset)$"));

pub fn rate_limit_details(headers: &IndexMap<String, String>, retry_after: Option<f64>) -> Option<HttpRateLimitDetails> {
    let mut limit = IndexMap::new();
    let mut remaining = IndexMap::new();
    let mut reset = IndexMap::new();
    for (name, value) in headers {
        if let Some(c) = OPENAI_LIMIT.captures(name) {
            limit.insert(c[1].to_string(), value.clone());
            continue;
        }
        if let Some(c) = OPENAI_REMAINING.captures(name) {
            remaining.insert(c[1].to_string(), value.clone());
            continue;
        }
        if let Some(c) = OPENAI_RESET.captures(name) {
            reset.insert(c[1].to_string(), value.clone());
            continue;
        }
        if let Some(c) = ANTHROPIC_RL.captures(name) {
            let target = match &c[2] {
                "limit" => &mut limit,
                "remaining" => &mut remaining,
                _ => &mut reset,
            };
            target.insert(c[1].to_string(), value.clone());
        }
    }
    if retry_after.is_none() && limit.is_empty() && remaining.is_empty() && reset.is_empty() {
        return None;
    }
    let opt = |m: IndexMap<String, String>| if m.is_empty() { None } else { Some(m) };
    Some(HttpRateLimitDetails { retry_after_ms: retry_after, limit: opt(limit), remaining: opt(remaining), reset: opt(reset) })
}

const MAX_BODY_CHARS: usize = 2000;
static HTML_START: Lazy<Regex> = Lazy::new(|| re(r"(?i)^<(?:!doctype|html)"));

/// `providerMessage(status, body)` for HTTP error responses.
pub fn http_status_message(status: u16, body: Option<&str>) -> String {
    let fallback = format!("Provider request failed with HTTP {status}");
    let text = body.map(str::trim).unwrap_or("");
    if let Some(message) = provider_error_message(text) {
        return message;
    }
    if text.is_empty() || HTML_START.is_match(text) {
        return fallback;
    }
    let chars: Vec<char> = text.chars().collect();
    // JS `slice` counts UTF-16 units; char count is a close approximation.
    if chars.len() > MAX_BODY_CHARS {
        format!("{fallback}: {}…", chars[..MAX_BODY_CHARS].iter().collect::<String>())
    } else {
        format!("{fallback}: {text}")
    }
}

/// `httpFailure`: classify a failed HTTP response.
pub fn http_failure(
    message: String,
    url: Option<&str>,
    status: Option<u16>,
    headers: &IndexMap<String, String>,
    body: Option<String>,
) -> AIError {
    let headers: IndexMap<String, String> = headers.iter().map(|(k, v)| (k.to_lowercase(), v.clone())).collect();
    let retry_after = retry_after_ms(&headers);
    let rate_limit = rate_limit_details(&headers, retry_after);
    let http = match (status, url) {
        (Some(status), Some(url)) => Some(HttpContext { url: url.to_string(), status, headers: headers.clone() }),
        _ => None,
    };
    classify_provider_failure(ProviderFailure {
        message,
        status,
        raw_body: body,
        data: None,
        http,
        cause: None,
        retry_after_ms: retry_after,
        rate_limit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(message: &str, status: Option<u16>, body: Option<&str>) -> AIError {
        classify_provider_failure(ProviderFailure {
            message: message.into(),
            status,
            raw_body: body.map(String::from),
            ..Default::default()
        })
    }

    #[test]
    fn overflow_detection() {
        assert!(is_context_overflow("prompt is too long: 210000 tokens > 200000 maximum"));
        assert!(is_context_overflow("This model's maximum context length is 128000 tokens"));
        assert!(is_context_overflow("400 status code (no body)"));
        assert!(!is_context_overflow("Rate limit: too many tokens"));
        let e = classify("prompt is too long", Some(400), None);
        assert!(e.is_context_overflow());
        let e = classify("x", Some(400), Some(r#"{"error":{"code":"context_length_exceeded","message":"x"}}"#));
        assert!(e.is_context_overflow());
    }

    #[test]
    fn status_classes() {
        assert_eq!(classify("bad key", Some(401), None).tag(), "Authentication");
        assert_eq!(classify("slow down", Some(429), None).tag(), "RateLimit");
        assert_eq!(classify("You exceeded your current quota", Some(429), Some(r#"{"error":{"code":"insufficient_quota"}}"#)).tag(), "QuotaExceeded");
        assert_eq!(classify("boom", Some(500), None).tag(), "ProviderInternal");
        assert_eq!(classify("Overloaded", None, Some(r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#)).tag(), "ProviderInternal");
        assert_eq!(classify("nope", Some(404), None).tag(), "InvalidRequest");
        assert_eq!(classify("weird", None, None).tag(), "UnknownProvider");
        assert_eq!(classify("too big", Some(413), None).classification(), Some(FailureClassification::PayloadTooLarge));
    }

    #[test]
    fn retryable() {
        assert!(classify("x", Some(429), None).is_retryable());
        assert!(!classify("x", Some(401), None).is_retryable());
        assert!(AIError::incomplete_stream("r", "x").is_retryable());
    }

    #[test]
    fn error_messages() {
        assert_eq!(provider_error_message(r#"{"error":{"message":"boom"}}"#).as_deref(), Some("boom"));
        assert_eq!(http_status_message(500, Some("<html>bad</html>")), "Provider request failed with HTTP 500");
        assert_eq!(http_status_message(500, Some("plain")), "Provider request failed with HTTP 500: plain");
    }

    #[test]
    fn rate_limit_headers() {
        let mut h = IndexMap::new();
        h.insert("retry-after".to_string(), "2".to_string());
        h.insert("anthropic-ratelimit-requests-remaining".to_string(), "0".to_string());
        let d = rate_limit_details(&h, retry_after_ms(&h)).unwrap();
        assert_eq!(d.retry_after_ms, Some(2000.0));
        assert_eq!(d.remaining.unwrap().get("requests").unwrap(), "0");
    }

    #[test]
    fn overflow_messages_added_in_2_1() {
        for message in [
            "Prompt exceeds max length",
            "The input is too long for the selected model.",
            "This text request is too large for the model's configured input limit.",
            "input is too long for requested model",
        ] {
            assert!(is_context_overflow(message), "{message}");
        }
    }

    #[test]
    fn retry_after_is_a_clamped_whole_number() {
        let mut h = IndexMap::new();
        h.insert("Retry-After".to_string(), "1e20".to_string());
        let e = http_failure("Huge retry-after".into(), Some("https://example.test"), Some(429), &h, None);
        assert!(matches!(e.kind, ErrorKind::RateLimit { retry_after_ms: Some(ms), .. } if ms == 9_007_199_254_740_991.0));
        let mut h = IndexMap::new();
        h.insert("retry-after-ms".to_string(), "1500.2".to_string());
        assert_eq!(retry_after_ms(&h), Some(1501.0));
        h.insert("retry-after-ms".to_string(), "-5".to_string());
        assert_eq!(retry_after_ms(&h), Some(0.0));
    }

    #[test]
    fn quota_exceeded_carries_retry_after() {
        let mut h = IndexMap::new();
        h.insert("retry-after".to_string(), "30".to_string());
        let body = r#"{"error":{"code":"insufficient_quota","message":"quota"}}"#;
        let e = http_failure("quota".into(), Some("https://example.test"), Some(429), &h, Some(body.into()));
        assert!(matches!(e.kind, ErrorKind::QuotaExceeded { retry_after_ms: Some(ms) } if ms == 30_000.0));
        assert_eq!(e.retry_after_ms(), None);
    }
}
