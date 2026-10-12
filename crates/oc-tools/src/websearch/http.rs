//! Shared HTTP helpers: `tool/http-body.ts` (`collectBoundedResponseBody`) and
//! `plugin/websearch/mcp.ts` (single JSON-RPC `tools/call` POST, JSON or SSE response).

use super::ProviderError;
use futures::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;

/// MCP response cap (`WebSearchMcp.MAX_RESPONSE_BYTES`).
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;
pub(crate) const TIMEOUT: Duration = Duration::from_secs(25);

pub(crate) fn transport_error(method: &str, url: &str, error: &reqwest::Error) -> ProviderError {
    ProviderError::new(format!("Transport: {error} ({method} {url})"))
}

/// Fail on non-2xx (`HttpClient.filterStatusOk`).
pub(crate) fn check_status(response: &reqwest::Response, method: &str, url: &str) -> Result<(), ProviderError> {
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(());
    }
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    Err(ProviderError::status(status, method, url, retry_after))
}

/// `collectBoundedResponseBody`: declared Content-Length over the cap fails early, streamed
/// overflow fails while reading.
pub(crate) async fn collect_bounded(
    response: reqwest::Response,
    maximum: usize,
    too_large: impl Fn() -> ProviderError,
    method: &str,
    url: &str,
) -> Result<Vec<u8>, ProviderError> {
    let declared = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    if declared.is_some_and(|size| size > maximum as u64) {
        return Err(too_large());
    }
    let mut body = Vec::with_capacity(maximum.min(declared.map(|d| d as usize).filter(|d| *d > 0).unwrap_or(64 * 1024)));
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| transport_error(method, url, &e))?;
        if chunk.is_empty() {
            continue;
        }
        if body.len() + chunk.len() > maximum {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// `WebSearchMcp.parseResponse`: the body is a JSON-RPC response either directly or in an SSE
/// `data: ` line. A payload that does not start with `{` is skipped; one that does must decode
/// to `{result}` (validated by `decode`) or the call fails. `Ok(None)` when nothing matched.
pub fn parse_response<T>(body: &str, decode: impl Fn(&Value) -> Result<T, String>) -> Result<Option<T>, String> {
    let parse = |payload: &str| -> Result<Option<T>, String> {
        let trimmed = payload.trim();
        if !trimmed.starts_with('{') {
            return Ok(None);
        }
        let value: Value = serde_json::from_str(trimmed).map_err(|e| format!("SyntaxError: {e}"))?;
        let Some(object) = value.as_object() else {
            return Err("Expected object".into());
        };
        let Some(result) = object.get("result") else {
            return Err("[\"result\"]\n  Missing key".into());
        };
        decode(result).map(Some)
    };
    let trimmed = body.trim();
    if !trimmed.is_empty()
        && let Some(direct) = parse(trimmed)?
    {
        return Ok(Some(direct));
    }
    for line in body.split('\n') {
        let Some(data) = line.strip_prefix("data: ") else { continue };
        if let Some(found) = parse(data)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// `WebSearchMcp.call`.
pub(crate) async fn mcp_call<T>(
    client: &reqwest::Client,
    url: &str,
    tool: &str,
    arguments: Value,
    headers: &[(&str, String)],
    decode: impl Fn(&Value) -> Result<T, String>,
) -> Result<Option<T>, ProviderError> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    });
    let mut request = client
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let request = request.body(serde_json::to_vec(&body).unwrap_or_default());
    let run = async {
        let response = request.send().await.map_err(|e| transport_error("POST", url, &e))?;
        check_status(&response, "POST", url)?;
        let bytes = collect_bounded(
            response,
            MAX_RESPONSE_BYTES,
            || ProviderError::new(format!("{tool} response exceeded {MAX_RESPONSE_BYTES} bytes")),
            "POST",
            url,
        )
        .await?;
        parse_response(&String::from_utf8_lossy(&bytes), decode).map_err(ProviderError::new)
    };
    match tokio::time::timeout(TIMEOUT, run).await {
        Ok(result) => result,
        Err(_) => Err(ProviderError::new(format!("{tool} request timed out"))),
    }
}

/// Validate `{content: Array<{type:"text", text: string}>}` and return the first non-empty text.
pub(crate) fn first_text_content(result: &Value, extra: impl Fn(&serde_json::Map<String, Value>) -> Result<(), String>) -> Result<Option<String>, String> {
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| "[\"content\"]\n  Expected array".to_string())?;
    for item in content {
        let object = item.as_object().ok_or("Expected object")?;
        if object.get("type").and_then(Value::as_str) != Some("text") {
            return Err("[\"type\"]\n  Expected \"text\"".into());
        }
        if !object.get("text").is_some_and(Value::is_string) {
            return Err("[\"text\"]\n  Expected string".into());
        }
        extra(object)?;
    }
    Ok(content
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .find(|text| !text.is_empty())
        .map(str::to_string))
}

/// `NullOr(String).pipe(optional)` field: absent/null → None, string → Some, else error.
pub(crate) fn optional_string(object: &serde_json::Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("[\"{key}\"]\n  Expected string | null")),
    }
}

pub(crate) fn required_string(object: &serde_json::Map<String, Value>, key: &str) -> Result<String, String> {
    match object.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(format!("[\"{key}\"]\n  Expected string")),
        None => Err(format!("[\"{key}\"]\n  Missing key")),
    }
}
