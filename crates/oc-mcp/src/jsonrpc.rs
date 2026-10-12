//! JSON-RPC 2.0 message helpers and the newline-delimited framing used by the stdio transport
//! (SDK `ReadBuffer` / `serializeMessage`, plus the 16 MiB per-frame guard in `stdio.ts`).

use serde_json::Value;

use crate::error::McpError;

/// Per-frame cap enforced by `packages/core/src/mcp/stdio.ts` (`MAX_FRAME_BYTES`).
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// SDK `STDIO_DEFAULT_MAX_BUFFER_SIZE`: the `ReadBuffer` refuses to hold more than this.
pub const MAX_BUFFER_BYTES: usize = 10 * 1024 * 1024;

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

/// `JSONRPCMessageSchema` validation (structural).
pub fn validate_message(message: &Value) -> Result<(), McpError> {
    let Some(obj) = message.as_object() else {
        return Err(McpError::other(format!("Invalid JSON-RPC message: {message}")));
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(McpError::other(format!("Invalid JSON-RPC message: {message}")));
    }
    if is_request(message) || is_notification(message) || is_result_response(message) || is_error_response(message) {
        return Ok(());
    }
    Err(McpError::other(format!("Invalid JSON-RPC message: {message}")))
}

/// `serializeMessage`: `JSON.stringify(message) + "\n"`.
pub fn serialize_line(message: &Value) -> String {
    let mut line = serde_json::to_string(message).unwrap_or_default();
    line.push('\n');
    line
}

/// Newline-delimited JSON framing with upstream's guards.
#[derive(Debug, Default)]
pub struct LineBuffer {
    buffer: Vec<u8>,
    trailing: usize,
}

/// One item read from a [`LineBuffer`].
#[derive(Debug)]
pub enum Frame {
    Message(Value),
    /// A line that parsed as JSON but is not a JSON-RPC message (reported through `onerror`).
    Invalid(McpError),
}

impl LineBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a stdout chunk. Fails when a frame exceeds 16 MiB or the buffer exceeds 10 MiB;
    /// both errors end the stream upstream.
    pub fn append(&mut self, chunk: &[u8]) -> Result<(), McpError> {
        for byte in chunk {
            self.trailing = if *byte == b'\n' { 0 } else { self.trailing + 1 };
            if self.trailing > MAX_FRAME_BYTES {
                return Err(McpError::other("MCP stdio frame exceeded 16 MiB"));
            }
        }
        if self.buffer.len() + chunk.len() > MAX_BUFFER_BYTES {
            self.buffer.clear();
            return Err(McpError::other(format!("ReadBuffer exceeded maximum size of {MAX_BUFFER_BYTES} bytes")));
        }
        self.buffer.extend_from_slice(chunk);
        Ok(())
    }

    /// Reads the next complete frame. Lines that are not JSON are skipped silently (SDK
    /// `ReadBuffer` swallows `SyntaxError`s).
    pub fn next_frame(&mut self) -> Option<Frame> {
        loop {
            let index = self.buffer.iter().position(|b| *b == b'\n')?;
            let line: Vec<u8> = self.buffer.drain(..=index).collect();
            let mut text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            if text.ends_with('\r') {
                text.pop();
            }
            let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
            return Some(match validate_message(&value) {
                Ok(()) => Frame::Message(value),
                Err(error) => Frame::Invalid(error),
            });
        }
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.trailing = 0;
    }
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
        buffer.append(b"{\"jsonrpc\":\"2.0\",\"method\":\"a\"}\r\nnot json\n{\"x\":1}\n{\"jsonrpc\":\"2.0\",").unwrap();
        match buffer.next_frame() {
            Some(Frame::Message(v)) => assert_eq!(v["method"], "a"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(buffer.next_frame(), Some(Frame::Invalid(_))));
        assert!(buffer.next_frame().is_none());
        buffer.append(b"\"id\":1,\"result\":{}}\n").unwrap();
        assert!(matches!(buffer.next_frame(), Some(Frame::Message(_))));
        assert_eq!(serialize_line(&json!({"a":"/é"})), "{\"a\":\"/é\"}\n");
    }

    #[test]
    fn frame_limit() {
        let mut buffer = LineBuffer::new();
        let chunk = vec![b'a'; MAX_FRAME_BYTES + 1];
        assert_eq!(buffer.append(&chunk).unwrap_err().to_string(), "MCP stdio frame exceeded 16 MiB");
        let mut buffer = LineBuffer::new();
        let mut chunk = vec![b'a'; MAX_BUFFER_BYTES];
        chunk[100] = b'\n';
        chunk.push(b'\n');
        assert!(buffer.append(&chunk).unwrap_err().to_string().starts_with("ReadBuffer exceeded maximum size of 10485760"));
    }
}
