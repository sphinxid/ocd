//! JSON-RPC 2.0 message helpers and the newline-delimited framing used by the stdio transport
//! (`packages/core/src/mcp/stdio.ts` `deliver`/`receive`, SDK `deserializeMessage`/`serializeMessage`).

use serde_json::Value;

use crate::error::McpError;

/// Per-line cap enforced by `packages/core/src/mcp/stdio.ts` (`MAX_FRAME_BYTES`).
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

pub fn is_request(message: &Value) -> bool {
    message.get("method").is_some_and(Value::is_string) && message.get("id").is_some_and(is_id)
}

pub fn is_notification(message: &Value) -> bool {
    message.get("method").is_some_and(Value::is_string) && message.get("id").is_none()
}

pub fn is_result_response(message: &Value) -> bool {
    message.get("method").is_none() && message.get("id").is_some_and(is_id) && message.get("result").is_some()
}

pub fn is_error_response(message: &Value) -> bool {
    message.get("method").is_none() && message.get("error").is_some_and(Value::is_object)
}

pub fn is_response(message: &Value) -> bool {
    is_result_response(message) || is_error_response(message)
}

fn is_id(id: &Value) -> bool {
    id.is_string() || id.is_i64() || id.is_u64()
}

pub fn is_initialize_request(message: &Value) -> bool {
    is_request(message) && message.get("method").and_then(Value::as_str) == Some("initialize")
}

pub fn is_initialized_notification(message: &Value) -> bool {
    is_notification(message) && message.get("method").and_then(Value::as_str) == Some("notifications/initialized")
}

/// Key used to correlate responses with requests (numbers and strings never collide).
pub fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => format!("s:{s}"),
        Value::Number(n) => format!("n:{n}"),
        other => format!("x:{other}"),
    }
}

/// `RequestIdSchema`: a string or an integer.
fn schema_id(id: &Value) -> bool {
    id.is_string() || id.is_i64() || id.is_u64() || id.as_f64().is_some_and(|f| f.is_finite() && f.fract() == 0.0)
}

/// `BaseRequestParamsSchema.loose()` / `NotificationsParamsSchema.loose()`: an object whose `_meta`, when
/// present, is an object with a string or integer `progressToken`.
fn schema_params(params: Option<&Value>) -> bool {
    let Some(params) = params else { return true };
    let Some(params) = params.as_object() else { return false };
    match params.get("_meta") {
        None => true,
        Some(Value::Object(meta)) => meta.get("progressToken").is_none_or(schema_id),
        Some(_) => false,
    }
}

/// `JSONRPCMessageSchema` (`@modelcontextprotocol/core` 2.3.1): a strict request, notification, result
/// response or error response.
fn schema_message(message: &Value) -> bool {
    let Some(obj) = message.as_object() else { return false };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return false;
    }
    let only = |keys: &[&str]| obj.keys().all(|k| keys.contains(&k.as_str()));
    let method = obj.get("method").is_some_and(Value::is_string);
    let id = obj.get("id");
    // Request, then notification.
    if method && schema_params(obj.get("params")) {
        if id.is_some_and(schema_id) && only(&["jsonrpc", "id", "method", "params"]) {
            return true;
        }
        if only(&["jsonrpc", "method", "params"]) {
            return true;
        }
    }
    // Result response: `ResultSchema` is a loose object whose `_meta`, when present, is an object.
    if id.is_some_and(schema_id)
        && only(&["jsonrpc", "id", "result"])
        && obj.get("result").and_then(Value::as_object).is_some_and(|r| r.get("_meta").is_none_or(Value::is_object))
    {
        return true;
    }
    // Error response.
    id.is_none_or(schema_id)
        && only(&["jsonrpc", "id", "error"])
        && obj.get("error").and_then(Value::as_object).is_some_and(|e| {
            e.get("code").is_some_and(|c| c.is_i64() || c.is_u64() || c.as_f64().is_some_and(|f| f.is_finite() && f.fract() == 0.0))
                && e.get("message").is_some_and(Value::is_string)
        })
}

/// `JSONRPCMessageSchema` validation.
pub fn validate_message(message: &Value) -> Result<(), McpError> {
    if schema_message(message) {
        return Ok(());
    }
    Err(McpError::other(format!("Invalid JSON-RPC message: {message}")))
}

/// `JSON.parse` failures with JavaScriptCore's common messages (as `${error}` prints them).
fn json_error(text: &str, error: &serde_json::Error) -> String {
    let trimmed = text.trim_start_matches([' ', '\t', '\n', '\r']);
    let message = match trimmed.chars().next() {
        None => "Unexpected EOF".to_string(),
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {
            let word: String = trimmed.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$').collect();
            format!("Unexpected identifier \"{word}\"")
        }
        Some(c) if !(c == '{' || c == '[' || c == '"' || c == '-' || c.is_ascii_digit()) => format!("Unrecognized token '{c}'"),
        Some('"') if error.is_eof() => "Unterminated string".to_string(),
        Some('{') if error.is_eof() => "Expected '}'".to_string(),
        _ => "Unable to parse JSON string".to_string(),
    };
    format!("SyntaxError: JSON Parse error: {message}")
}

/// SDK `deserializeMessage`: `JSON.parse` then `JSONRPCMessageSchema.parse`; the error is the cause text.
pub fn deserialize_message(text: &str) -> Result<Value, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| json_error(text, &e))?;
    validate_message(&value).map_err(|e| e.to_string())?;
    Ok(value)
}

/// `serializeMessage`: `JSON.stringify(message) + "\n"`.
pub fn serialize_line(message: &Value) -> String {
    let mut line = serde_json::to_string(message).unwrap_or_default();
    line.push('\n');
    line
}

/// Newline-delimited framing (`stdio.ts` `deliver`): bytes of the unterminated line are kept until its newline
/// arrives; there is no overall buffer cap, only the 16 MiB per-line limit.
#[derive(Debug, Default)]
pub struct LineBuffer {
    pending: Vec<u8>,
}

/// A line exceeded [`MAX_FRAME_BYTES`] without a newline.
#[derive(Debug, PartialEq, Eq)]
pub struct Oversized;

impl LineBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Splits a stdout chunk into lines and hands each complete one (without its `\n`) to `line`, which
    /// returns false once the transport is closed: delivery then stops and the rest is dropped. Fails when
    /// a line exceeds 16 MiB; the lines before it in the chunk have been delivered.
    pub fn push(&mut self, chunk: &[u8], mut line: impl FnMut(&[u8]) -> bool) -> Result<(), Oversized> {
        let mut start = 0;
        while let Some(offset) = chunk[start..].iter().position(|b| *b == b'\n') {
            let end = start + offset;
            if self.pending.len() + (end - start) > MAX_FRAME_BYTES {
                self.clear();
                return Err(Oversized);
            }
            let open = if self.pending.is_empty() {
                line(&chunk[start..end])
            } else {
                self.pending.extend_from_slice(&chunk[start..end]);
                let joined = std::mem::take(&mut self.pending);
                line(&joined)
            };
            start = end + 1;
            if !open {
                self.clear();
                return Ok(());
            }
        }
        if start == chunk.len() {
            return Ok(());
        }
        if self.pending.len() + (chunk.len() - start) > MAX_FRAME_BYTES {
            self.clear();
            return Err(Oversized);
        }
        self.pending.extend_from_slice(&chunk[start..]);
        Ok(())
    }

    pub fn clear(&mut self) {
        self.pending = Vec::new();
    }
}

/// `stdio.ts` `receive`: a trailing `\r` is stripped, whitespace-only lines are skipped (`None`), and
/// anything else is deserialized.
pub fn receive_line(line: &[u8]) -> Option<Result<Value, String>> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let text = String::from_utf8_lossy(line);
    if text.trim_matches(is_js_whitespace).is_empty() {
        return None;
    }
    Some(deserialize_message(&text))
}

/// `String.prototype.trim` whitespace.
fn is_js_whitespace(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000D}' | '\u{0020}' | '\u{00A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classification() {
        assert!(is_request(&json!({"jsonrpc":"2.0","id":1,"method":"x"})));
        assert!(is_notification(&json!({"jsonrpc":"2.0","method":"x"})));
        assert!(is_result_response(&json!({"jsonrpc":"2.0","id":"a","result":{}})));
        assert!(is_error_response(&json!({"jsonrpc":"2.0","id":1,"error":{"code":1,"message":"m"}})));
        assert!(!is_response(&json!({"jsonrpc":"2.0","id":1,"method":"x"})));
        assert_ne!(id_key(&json!(1)), id_key(&json!("1")));
    }

    #[test]
    fn framing() {
        let mut buffer = LineBuffer::new();
        let mut lines: Vec<Option<Result<Value, String>>> = vec![];
        let chunk = b"{\"jsonrpc\":\"2.0\",\"method\":\"a\"}\r\nnot json\n \t\n{\"x\":1}\n{\"jsonrpc\":\"2.0\",";
        buffer.push(chunk, |l| {
            lines.push(receive_line(l));
            true
        })
        .unwrap();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].as_ref().unwrap().as_ref().unwrap()["method"], "a");
        assert_eq!(lines[1].as_ref().unwrap().as_ref().unwrap_err(), "SyntaxError: JSON Parse error: Unexpected identifier \"not\"");
        assert!(lines[2].is_none());
        assert!(lines[3].as_ref().unwrap().as_ref().unwrap_err().starts_with("Invalid JSON-RPC message"));
        lines.clear();
        buffer.push(b"\"id\":1,\"result\":{}}\n", |l| {
            lines.push(receive_line(l));
            true
        })
        .unwrap();
        assert_eq!(lines[0].as_ref().unwrap().as_ref().unwrap(), &json!({"jsonrpc":"2.0","id":1,"result":{}}));
        assert_eq!(serialize_line(&json!({"a":"/é"})), "{\"a\":\"/é\"}\n");
    }

    #[test]
    fn stops_delivering_once_closed() {
        let mut buffer = LineBuffer::new();
        let mut count = 0;
        buffer
            .push(b"{\"jsonrpc\":\"2.0\",\"method\":\"a\"}\n{\"jsonrpc\":\"2.0\",\"method\":\"b\"}\npartial", |_| {
                count += 1;
                false
            })
            .unwrap();
        assert_eq!(count, 1);
        assert!(buffer.pending.is_empty());
    }

    /// mcp-stdio.test.ts: messages larger than the SDK's 10 MiB read buffer are delivered; a line over 16 MiB
    /// without a newline is refused.
    #[test]
    fn frame_limit() {
        let big = format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"text\":\"{}\"}}}}\n", "a".repeat(11 * 1024 * 1024));
        let mut buffer = LineBuffer::new();
        let mut got = vec![];
        for piece in big.as_bytes().chunks(64 * 1024) {
            buffer.push(piece, |l| {
                got.push(receive_line(l));
                true
            })
            .unwrap();
        }
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].as_ref().unwrap().as_ref().unwrap()["result"]["text"].as_str().unwrap().len(), 11 * 1024 * 1024);

        let mut buffer = LineBuffer::new();
        let chunk = vec![b'a'; MAX_FRAME_BYTES];
        assert_eq!(buffer.push(&chunk, |_| true), Ok(()));
        assert_eq!(buffer.push(b"a", |_| true), Err(Oversized));
        // A complete line over the limit in one chunk is refused after the lines before it.
        let mut buffer = LineBuffer::new();
        let mut chunk = b"{\"jsonrpc\":\"2.0\",\"method\":\"a\"}\n".to_vec();
        chunk.extend(vec![b'a'; MAX_FRAME_BYTES + 1]);
        chunk.push(b'\n');
        let mut count = 0;
        assert_eq!(
            buffer.push(&chunk, |_| {
                count += 1;
                true
            }),
            Err(Oversized)
        );
        assert_eq!(count, 1);
    }

    /// `JSONRPCMessageSchema` is strict: unknown keys, non-object params or results, and fractional ids fail.
    #[test]
    fn schema() {
        for ok in [
            json!({"jsonrpc":"2.0","id":1,"method":"x","params":{"a":1,"_meta":{"progressToken":"t"}}}),
            json!({"jsonrpc":"2.0","method":"x"}),
            json!({"jsonrpc":"2.0","id":"a","result":{"x":1}}),
            json!({"jsonrpc":"2.0","error":{"code":1,"message":"m","data":2}}),
        ] {
            assert!(validate_message(&ok).is_ok(), "{ok}");
        }
        for bad in [
            json!({"jsonrpc":"2.0","id":1,"result":{},"extra":1}),
            json!({"jsonrpc":"2.0","id":1,"result":5}),
            json!({"jsonrpc":"2.0","method":"x","extra":1}),
            json!({"jsonrpc":"2.0","id":1.5,"method":"x"}),
            json!({"jsonrpc":"2.0","id":null,"error":{"code":1,"message":"m"}}),
            json!({"jsonrpc":"2.0","id":1,"error":{"code":"x","message":"m"}}),
            json!({"jsonrpc":"2.0","id":1,"method":"x","params":[1]}),
            json!({"jsonrpc":"2.0","id":1,"error":{"code":1}}),
        ] {
            assert!(validate_message(&bad).is_err(), "{bad}");
        }
    }
}
