//! Byte stream → protocol frames (`packages/ai/src/route/framing.ts`,
//! `protocols/bedrock-event-stream.ts`).

use crate::error::AIError;
use crate::json::{encode, Object};
use base64::Engine;
use serde_json::Value;

/// Framing kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum Framing {
    /// SSE, drops `[DONE]`.
    Sse,
    /// SSE retaining `[DONE]`.
    SseWithDone,
    /// SSE restricted to the given event names.
    SseEvents(Vec<String>),
    /// NDJSON lines.
    Lines,
    /// Whole body as one frame.
    Document,
    /// AWS event stream (Bedrock). The string is the route id used in diagnostics.
    AwsEventStream(String),
}

/// One decoded frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// Text frame (SSE data / NDJSON line / document).
    Text(String),
    /// Structured frame (AWS event stream: `{<eventType>: payload, rawBody}` or `{exception, rawBody}`).
    Json(Value),
}

impl Frame {
    /// Original wire representation used for error bodies.
    pub fn raw(&self) -> String {
        match self {
            Frame::Text(s) => s.clone(),
            Frame::Json(v) => v.get("rawBody").and_then(Value::as_str).map(String::from).unwrap_or_else(|| encode(v)),
        }
    }
}

/// Incremental framer.
pub enum Framer {
    Sse(SseParser),
    Lines(LineBuffer),
    Document(Vec<u8>),
    Aws(AwsEventStreamDecoder),
}

impl Framer {
    pub fn new(framing: &Framing) -> Self {
        match framing {
            Framing::Sse => Framer::Sse(SseParser::new(None, false)),
            Framing::SseWithDone => Framer::Sse(SseParser::new(None, true)),
            Framing::SseEvents(events) => Framer::Sse(SseParser::new(Some(events.clone()), false)),
            Framing::Lines => Framer::Lines(LineBuffer::default()),
            Framing::Document => Framer::Document(Vec::new()),
            Framing::AwsEventStream(route) => Framer::Aws(AwsEventStreamDecoder::new(route)),
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, AIError> {
        match self {
            Framer::Sse(p) => Ok(p.push(bytes).into_iter().map(Frame::Text).collect()),
            Framer::Lines(l) => Ok(l.push(bytes).into_iter().map(Frame::Text).collect()),
            Framer::Document(buf) => {
                buf.extend_from_slice(bytes);
                Ok(vec![])
            }
            Framer::Aws(d) => d.push(bytes),
        }
    }

    pub fn finish(&mut self) -> Result<Vec<Frame>, AIError> {
        match self {
            Framer::Sse(p) => Ok(p.finish().into_iter().map(Frame::Text).collect()),
            Framer::Lines(l) => Ok(l.finish().into_iter().map(Frame::Text).collect()),
            Framer::Document(buf) => Ok(vec![Frame::Text(String::from_utf8_lossy(buf).into_owned())]),
            Framer::Aws(d) => d.finish(),
        }
    }
}

/// Decode a full body with the framing (tests / non-streaming use).
pub fn frame_all(framing: &Framing, body: &[u8]) -> Result<Vec<Frame>, AIError> {
    let mut framer = Framer::new(framing);
    let mut frames = framer.push(body)?;
    frames.extend(framer.finish()?);
    Ok(frames)
}

// ---------------------------------------------------------------------------------------------
// SSE (WHATWG event stream parsing)
// ---------------------------------------------------------------------------------------------

/// Incremental UTF-8 decoder that keeps split multibyte sequences across chunks.
#[derive(Default)]
struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        match std::str::from_utf8(&self.pending) {
            Ok(s) => {
                let out = s.to_string();
                self.pending.clear();
                out
            }
            Err(e) => {
                let valid = e.valid_up_to();
                if e.error_len().is_none() {
                    // incomplete sequence at the end
                    let out = String::from_utf8_lossy(&self.pending[..valid]).into_owned();
                    self.pending.drain(..valid);
                    out
                } else {
                    let out = String::from_utf8_lossy(&self.pending).into_owned();
                    self.pending.clear();
                    out
                }
            }
        }
    }
    fn finish(&mut self) -> String {
        let out = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        out
    }
}

pub struct SseParser {
    decoder: Utf8Decoder,
    buffer: String,
    data: String,
    has_data: bool,
    event: String,
    events: Option<Vec<String>>,
    include_done: bool,
    started: bool,
    last_cr: bool,
}

impl SseParser {
    pub fn new(events: Option<Vec<String>>, include_done: bool) -> Self {
        Self {
            decoder: Utf8Decoder::default(),
            buffer: String::new(),
            data: String::new(),
            has_data: false,
            event: String::new(),
            events,
            include_done,
            started: false,
            last_cr: false,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let text = self.decoder.push(bytes);
        self.feed(&text)
    }

    pub fn finish(&mut self) -> Vec<String> {
        let rest = self.decoder.finish();
        let mut out = self.feed(&rest);
        // A trailing line without terminator is processed; an incomplete event is dropped (WHATWG).
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            self.line(&line, &mut out);
        }
        out
    }

    fn feed(&mut self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut chars = text;
        if !self.started && !chars.is_empty() {
            self.started = true;
            if let Some(stripped) = chars.strip_prefix('\u{feff}') {
                chars = stripped;
            }
        }
        for ch in chars.chars() {
            if self.last_cr {
                self.last_cr = false;
                if ch == '\n' {
                    continue;
                }
            }
            match ch {
                '\r' => {
                    self.last_cr = true;
                    let line = std::mem::take(&mut self.buffer);
                    self.line(&line, &mut out);
                }
                '\n' => {
                    let line = std::mem::take(&mut self.buffer);
                    self.line(&line, &mut out);
                }
                c => self.buffer.push(c),
            }
        }
        out
    }

    fn line(&mut self, line: &str, out: &mut Vec<String>) {
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.find(':') {
            Some(idx) => {
                let value = &line[idx + 1..];
                (&line[..idx], value.strip_prefix(' ').unwrap_or(value))
            }
            None => (line, ""),
        };
        match field {
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "event" => self.event = value.to_string(),
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<String>) {
        let data = std::mem::take(&mut self.data);
        let has_data = std::mem::replace(&mut self.has_data, false);
        let event = std::mem::take(&mut self.event);
        if !has_data {
            return;
        }
        let event = if event.is_empty() { "message".to_string() } else { event };
        let allowed = self.events.as_ref().map(|set| set.contains(&event)).unwrap_or(true);
        if allowed
            && !data.is_empty()
            && data != "null"
            && data != ": keepalive"
            && (data != "[DONE]" || self.include_done || (self.events.is_some() && event != "message"))
        {
            out.push(data);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Lines
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct LineBuffer {
    decoder: Utf8Decoder,
    buffer: String,
}

impl LineBuffer {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let text = self.decoder.push(bytes);
        self.buffer.push_str(&text);
        let mut out = Vec::new();
        while let Some(pos) = self.buffer.find(['\n', '\r']) {
            let line: String = self.buffer[..pos].to_string();
            let mut skip = pos + 1;
            if self.buffer[pos..].starts_with("\r\n") {
                skip = pos + 2;
            }
            self.buffer.drain(..skip);
            if !line.trim().is_empty() {
                out.push(line);
            }
        }
        out
    }
    pub fn finish(&mut self) -> Vec<String> {
        let rest = self.decoder.finish();
        self.buffer.push_str(&rest);
        let line = std::mem::take(&mut self.buffer);
        if line.trim().is_empty() { vec![] } else { vec![line] }
    }
}

// ---------------------------------------------------------------------------------------------
// AWS event stream
// ---------------------------------------------------------------------------------------------

pub struct AwsEventStreamDecoder {
    route: String,
    buffer: Vec<u8>,
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// One decoded AWS event-stream message.
#[derive(Debug, Clone, PartialEq)]
pub struct AwsMessage {
    /// Header name → `{type, value}` JSON (smithy shape).
    pub headers: Object,
    pub body: Vec<u8>,
}

impl AwsMessage {
    pub fn header_str(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|h| h.get("value")).and_then(Value::as_str)
    }
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Decode one complete message (`@smithy/eventstream-codec` semantics, CRC validated).
pub fn decode_aws_message(message: &[u8]) -> Result<AwsMessage, String> {
    if message.len() < 16 {
        return Err("Provided message too short to accommodate event stream message overhead".into());
    }
    let total = read_u32(message, 0) as usize;
    if total != message.len() {
        return Err("Reported message length does not match received message length".into());
    }
    let headers_len = read_u32(message, 4) as usize;
    let prelude_crc = read_u32(message, 8);
    let expected_prelude = crc32fast::hash(&message[0..8]);
    if prelude_crc != expected_prelude {
        return Err(format!(
            "The prelude checksum specified in the message ({prelude_crc}) does not match the calculated CRC32 checksum ({expected_prelude})"
        ));
    }
    let message_crc = read_u32(message, total - 4);
    let expected = crc32fast::hash(&message[0..total - 4]);
    if message_crc != expected {
        return Err(format!(
            "The message checksum ({expected}) did not match the expected value of {message_crc}"
        ));
    }
    let headers_start = 12;
    let headers_end = headers_start + headers_len;
    if headers_end > total - 4 {
        return Err("Invalid headers length".into());
    }
    let headers = parse_headers(&message[headers_start..headers_end])?;
    Ok(AwsMessage { headers, body: message[headers_end..total - 4].to_vec() })
}

fn parse_headers(bytes: &[u8]) -> Result<Object, String> {
    let mut out = Object::new();
    let mut i = 0;
    let need = |i: usize, n: usize| if i + n > bytes.len() { Err("Truncated event stream header".to_string()) } else { Ok(()) };
    while i < bytes.len() {
        need(i, 1)?;
        let name_len = bytes[i] as usize;
        i += 1;
        need(i, name_len)?;
        let name = String::from_utf8_lossy(&bytes[i..i + name_len]).into_owned();
        i += name_len;
        need(i, 1)?;
        let kind = bytes[i];
        i += 1;
        let (type_name, value): (&str, Value) = match kind {
            0 => ("boolean", Value::Bool(true)),
            1 => ("boolean", Value::Bool(false)),
            2 => {
                need(i, 1)?;
                let v = bytes[i] as i8;
                i += 1;
                ("byte", Value::from(v))
            }
            3 => {
                need(i, 2)?;
                let v = i16::from_be_bytes([bytes[i], bytes[i + 1]]);
                i += 2;
                ("short", Value::from(v))
            }
            4 => {
                need(i, 4)?;
                let v = i32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
                i += 4;
                ("integer", Value::from(v))
            }
            5 => {
                need(i, 8)?;
                let mut b = [0u8; 8];
                b.copy_from_slice(&bytes[i..i + 8]);
                i += 8;
                ("long", Value::from(i64::from_be_bytes(b)))
            }
            6 | 7 => {
                need(i, 2)?;
                let len = u16::from_be_bytes([bytes[i], bytes[i + 1]]) as usize;
                i += 2;
                need(i, len)?;
                let raw = &bytes[i..i + len];
                i += len;
                if kind == 6 {
                    ("binary", Value::String(b64(raw)))
                } else {
                    ("string", Value::String(String::from_utf8_lossy(raw).into_owned()))
                }
            }
            8 => {
                need(i, 8)?;
                let mut b = [0u8; 8];
                b.copy_from_slice(&bytes[i..i + 8]);
                i += 8;
                ("timestamp", Value::from(i64::from_be_bytes(b)))
            }
            9 => {
                need(i, 16)?;
                let raw = &bytes[i..i + 16];
                i += 16;
                let h = hex::encode(raw);
                ("uuid", Value::String(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])))
            }
            other => return Err(format!("Unrecognized header type tag: {other}")),
        };
        let mut header = Object::new();
        header.insert("type".into(), Value::String(type_name.into()));
        header.insert("value".into(), value);
        out.insert(name, Value::Object(header));
    }
    Ok(out)
}

/// Encode an AWS event-stream message with string headers (tests / fixtures).
pub fn encode_aws_message(headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut header_bytes = Vec::new();
    for (name, value) in headers {
        header_bytes.push(name.len() as u8);
        header_bytes.extend_from_slice(name.as_bytes());
        header_bytes.push(7);
        header_bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
        header_bytes.extend_from_slice(value.as_bytes());
    }
    let total = 12 + header_bytes.len() + body.len() + 4;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
    let prelude_crc = crc32fast::hash(&out[0..8]);
    out.extend_from_slice(&prelude_crc.to_be_bytes());
    out.extend_from_slice(&header_bytes);
    out.extend_from_slice(body);
    let crc = crc32fast::hash(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

impl AwsEventStreamDecoder {
    pub fn new(route: &str) -> Self {
        Self { route: route.into(), buffer: Vec::new() }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, AIError> {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut offset = 0;
        while self.buffer.len() - offset >= 4 {
            let total = read_u32(&self.buffer, offset) as usize;
            if self.buffer.len() - offset < total {
                break;
            }
            let raw = self.buffer[offset..offset + total.max(4)].to_vec();
            let decoded = decode_aws_message(&raw).map_err(|message| {
                AIError::invalid_output(&self.route, format!("Failed to decode Bedrock Converse event-stream frame: {message}"))
                    .with_body(b64(&raw))
            });
            let decoded = match decoded {
                Ok(d) => d,
                Err(e) => {
                    self.buffer.drain(..offset);
                    return Err(e);
                }
            };
            offset += total.max(4);
            let payload = String::from_utf8_lossy(&decoded.body).into_owned();
            let mut body_obj = Object::new();
            body_obj.insert("headers".into(), Value::Object(decoded.headers.clone()));
            body_obj.insert("body".into(), Value::String(payload.clone()));
            let body = encode(&Value::Object(body_obj));
            let message_type = decoded.header_str(":message-type");
            if message_type == Some("error") {
                let parts: Vec<&str> =
                    [decoded.header_str(":error-code"), decoded.header_str(":error-message")].into_iter().flatten().collect();
                let message = if parts.is_empty() { "Bedrock Converse event-stream error".to_string() } else { parts.join(": ") };
                self.buffer.drain(..offset);
                return Err(AIError::invalid_output(&self.route, message).with_body(body));
            }
            let event_type = match message_type {
                Some("event") => decoded.header_str(":event-type"),
                Some("exception") => decoded.header_str(":exception-type"),
                _ => None,
            };
            let Some(event_type) = event_type else { continue };
            if payload.is_empty() {
                continue;
            }
            let mut parsed: Value = match serde_json::from_str(&payload) {
                Ok(v) => v,
                Err(e) => {
                    self.buffer.drain(..offset);
                    return Err(AIError::invalid_output(&self.route, "Failed to parse Bedrock Converse event-stream payload")
                        .with_body(body)
                        .with_cause(e));
                }
            };
            if let Some(obj) = parsed.as_object_mut() {
                obj.shift_remove("p");
            }
            let mut frame = Object::new();
            if message_type == Some("exception") {
                let mut exception = Object::new();
                exception.insert("type".into(), Value::String(event_type.into()));
                exception.insert("details".into(), parsed);
                frame.insert("exception".into(), Value::Object(exception));
            } else {
                frame.insert(event_type.into(), parsed);
            }
            frame.insert("rawBody".into(), Value::String(body));
            out.push(Frame::Json(Value::Object(frame)));
        }
        self.buffer.drain(..offset);
        Ok(out)
    }

    pub fn finish(&mut self) -> Result<Vec<Frame>, AIError> {
        if !self.buffer.is_empty() {
            let remaining = std::mem::take(&mut self.buffer);
            return Err(AIError::incomplete_stream(
                &self.route,
                format!(
                    "Incomplete Bedrock Converse event-stream frame: {} buffered bytes remain at end of stream",
                    remaining.len()
                ),
            )
            .with_body(b64(&remaining)));
        }
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse(framing: Framing, input: &str) -> Vec<String> {
        frame_all(&framing, input.as_bytes())
            .unwrap()
            .into_iter()
            .map(|f| match f {
                Frame::Text(s) => s,
                _ => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn sse_basic() {
        let input = "data: {\"a\":1}\n\n: comment\n\ndata: [DONE]\n\ndata: null\n\nevent: ping\ndata: x\r\n\r\ndata: a\ndata: b\n\n";
        assert_eq!(sse(Framing::Sse, input), vec!["{\"a\":1}", "x", "a\nb"]);
        assert_eq!(sse(Framing::SseWithDone, input), vec!["{\"a\":1}", "[DONE]", "x", "a\nb"]);
        assert_eq!(sse(Framing::SseEvents(vec!["ping".into()]), input), vec!["x"]);
    }

    #[test]
    fn sse_split_chunks() {
        let mut parser = SseParser::new(None, false);
        let mut out = parser.push(b"da");
        out.extend(parser.push(b"ta: h\xc3"));
        out.extend(parser.push(b"\xa9llo\n"));
        out.extend(parser.push(b"\n"));
        assert_eq!(out, vec!["héllo"]);
    }

    #[test]
    fn aws_roundtrip() {
        let msg = encode_aws_message(
            &[(":event-type", "messageStop"), (":content-type", "application/json"), (":message-type", "event")],
            br#"{"p":"abc","stopReason":"end_turn"}"#,
        );
        let mut d = AwsEventStreamDecoder::new("bedrock-converse");
        let mut frames = d.push(&msg[..10]).unwrap();
        frames.extend(d.push(&msg[10..]).unwrap());
        frames.extend(d.finish().unwrap());
        assert_eq!(frames.len(), 1);
        let Frame::Json(v) = &frames[0] else { panic!() };
        assert_eq!(v["messageStop"]["stopReason"], "end_turn");
        assert!(v["messageStop"].get("p").is_none());
        let mut d = AwsEventStreamDecoder::new("bedrock-converse");
        d.push(&msg[..10]).unwrap();
        let err = d.finish().unwrap_err();
        assert!(err.message.contains("10 buffered bytes"));
    }
}
