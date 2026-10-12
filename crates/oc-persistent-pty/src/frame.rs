//! Length-prefixed framing of the `opencode-pty` IPC (`persistent-pty/daemon.ts:659-686`).
//!
//! Every frame is a big-endian `u32` length followed by that many payload bytes; payloads
//! are limited to [`MAX_FRAME_BYTES`]. Client → daemon payloads are UTF-8 JSON envelopes
//! `{"token": ..., "request": {...}}`. Daemon → client payloads are JSON responses, except
//! subscription output frames, which start with a `0x00` tag (see [`parse_output_frame`]).

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};

/// `ProtocolVersion` (`daemon.ts:8`).
pub const PROTOCOL_VERSION: u64 = 7;
/// `MaxFrameBytes` (`daemon.ts:9`).
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// First byte of a binary subscription output frame.
pub const OUTPUT_FRAME_TAG: u8 = 0;
/// `[tag u8][start u64 BE][end u64 BE]` header size of output frames.
pub const OUTPUT_HEADER_BYTES: usize = 17;

pub const FRAME_TOO_LARGE: &str = "opencode-pty frame too large";
pub const TRUNCATED_FRAME: &str = "opencode-pty truncated frame";
pub const INVALID_OUTPUT_FRAME: &str = "invalid opencode-pty output frame";

/// Frame raw payload bytes (`encode` after `JSON.stringify`).
pub fn encode_payload(payload: &[u8]) -> Result<Vec<u8>, String> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FRAME_TOO_LARGE.to_string());
    }
    let mut output = Vec::with_capacity(payload.len() + 4);
    output.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    output.extend_from_slice(payload);
    Ok(output)
}

/// `encode(value)`: JSON-serialize and frame.
pub fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, String> {
    let payload = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    encode_payload(&payload)
}

/// Frame an authenticated request envelope `{"token", "request"}`.
pub fn encode_envelope(token: &str, request: &serde_json::Value) -> Result<Vec<u8>, String> {
    encode(&serde_json::json!({ "token": token, "request": request }))
}

/// Incremental decoder (`decoder()` generator state).
#[derive(Debug, Default)]
pub struct FrameDecoder {
    pending: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append received bytes.
    pub fn push(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
    }

    /// Pop the next complete frame, if any. Errors when a header claims more than the limit.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, String> {
        if self.pending.len() < 4 {
            return Ok(None);
        }
        let length = u32::from_be_bytes(self.pending[..4].try_into().expect("4 bytes")) as usize;
        if length > MAX_FRAME_BYTES {
            return Err(FRAME_TOO_LARGE.to_string());
        }
        if self.pending.len() < length + 4 {
            return Ok(None);
        }
        let frame = self.pending[4..length + 4].to_vec();
        self.pending.drain(..length + 4);
        Ok(Some(frame))
    }

    /// Called at EOF: trailing partial data is an error.
    pub fn finish(&self) -> Result<(), String> {
        if self.pending.is_empty() { Ok(()) } else { Err(TRUNCATED_FRAME.to_string()) }
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// Async frame reader over a byte stream. `next` is cancel-safe: dropping its future
/// never loses buffered bytes.
pub struct FrameReader<R> {
    reader: R,
    decoder: FrameDecoder,
    eof: bool,
    buffer: Vec<u8>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self { reader, decoder: FrameDecoder::new(), eof: false, buffer: vec![0u8; 64 * 1024] }
    }

    /// Next frame; `Ok(None)` at a clean EOF.
    pub async fn next(&mut self) -> Result<Option<Vec<u8>>, String> {
        loop {
            if let Some(frame) = self.decoder.next_frame()? {
                return Ok(Some(frame));
            }
            if self.eof {
                self.decoder.finish()?;
                return Ok(None);
            }
            let n = self.reader.read(&mut self.buffer).await.map_err(|e| e.to_string())?;
            if n == 0 {
                self.eof = true;
                continue;
            }
            self.decoder.push(&self.buffer[..n]);
        }
    }

    pub fn get_mut(&mut self) -> &mut R {
        &mut self.reader
    }
}

/// A decoded subscription output frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputFrame<'a> {
    pub start: u64,
    pub end: u64,
    pub data: &'a [u8],
}

/// `None` when the frame is not an output frame (first byte ≠ 0, i.e. JSON);
/// `Some(Err)` when it is tagged but shorter than 17 bytes.
pub fn parse_output_frame(frame: &[u8]) -> Option<Result<OutputFrame<'_>, String>> {
    if frame.first() != Some(&OUTPUT_FRAME_TAG) {
        return None;
    }
    if frame.len() < OUTPUT_HEADER_BYTES {
        return Some(Err(INVALID_OUTPUT_FRAME.to_string()));
    }
    Some(Ok(OutputFrame {
        start: u64::from_be_bytes(frame[1..9].try_into().expect("8 bytes")),
        end: u64::from_be_bytes(frame[9..17].try_into().expect("8 bytes")),
        data: &frame[17..],
    }))
}

/// Build an output frame payload (daemon side; used by tests and fakes).
pub fn output_frame_payload(start: u64, end: u64, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(OUTPUT_HEADER_BYTES + data.len());
    out.push(OUTPUT_FRAME_TAG);
    out.extend_from_slice(&start.to_be_bytes());
    out.extend_from_slice(&end.to_be_bytes());
    out.extend_from_slice(data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_prefixes_big_endian_length() {
        let framed = encode(&serde_json::json!({"op": "ping"})).unwrap();
        assert_eq!(&framed[..4], &[0, 0, 0, 13]);
        assert_eq!(&framed[4..], br#"{"op":"ping"}"#);
        let envelope = encode_envelope("tok", &serde_json::json!({"op": "list"})).unwrap();
        assert_eq!(&envelope[4..], br#"{"token":"tok","request":{"op":"list"}}"#);
    }

    #[test]
    fn encode_rejects_oversized_payloads() {
        assert!(encode_payload(&vec![b'a'; MAX_FRAME_BYTES]).is_ok());
        assert_eq!(encode_payload(&vec![b'a'; MAX_FRAME_BYTES + 1]).unwrap_err(), FRAME_TOO_LARGE);
    }

    #[test]
    fn decoder_handles_split_and_coalesced_frames() {
        let mut stream = encode_payload(b"hello").unwrap();
        stream.extend(encode_payload(b"").unwrap());
        stream.extend(encode_payload(b"world!").unwrap());
        // Feed byte by byte.
        let mut decoder = FrameDecoder::new();
        let mut frames = Vec::new();
        for byte in &stream {
            decoder.push(std::slice::from_ref(byte));
            while let Some(frame) = decoder.next_frame().unwrap() {
                frames.push(frame);
            }
        }
        assert_eq!(frames, vec![b"hello".to_vec(), Vec::new(), b"world!".to_vec()]);
        decoder.finish().unwrap();
        // All at once.
        let mut decoder = FrameDecoder::new();
        decoder.push(&stream);
        assert_eq!(decoder.next_frame().unwrap().unwrap(), b"hello");
        assert_eq!(decoder.next_frame().unwrap().unwrap(), b"");
        assert_eq!(decoder.next_frame().unwrap().unwrap(), b"world!");
        assert_eq!(decoder.next_frame().unwrap(), None);
    }

    #[test]
    fn decoder_rejects_large_and_truncated() {
        let mut decoder = FrameDecoder::new();
        decoder.push(&((MAX_FRAME_BYTES as u32) + 1).to_be_bytes());
        assert_eq!(decoder.next_frame().unwrap_err(), FRAME_TOO_LARGE);

        let mut decoder = FrameDecoder::new();
        decoder.push(&[0, 0, 0, 5, b'a']);
        assert_eq!(decoder.next_frame().unwrap(), None);
        assert_eq!(decoder.finish().unwrap_err(), TRUNCATED_FRAME);

        // Fewer than 4 header bytes also count as truncated.
        let mut decoder = FrameDecoder::new();
        decoder.push(&[0, 0]);
        assert_eq!(decoder.finish().unwrap_err(), TRUNCATED_FRAME);
    }

    #[test]
    fn output_frames() {
        let payload = output_frame_payload(10, 13, b"abc");
        assert_eq!(payload.len(), 20);
        let frame = parse_output_frame(&payload).unwrap().unwrap();
        assert_eq!(frame, OutputFrame { start: 10, end: 13, data: b"abc" });
        let empty = output_frame_payload(u64::MAX - 1, u64::MAX, b"");
        let frame = parse_output_frame(&empty).unwrap().unwrap();
        assert_eq!((frame.start, frame.end, frame.data.len()), (u64::MAX - 1, u64::MAX, 0));
        assert_eq!(parse_output_frame(&[0, 1, 2]).unwrap().unwrap_err(), INVALID_OUTPUT_FRAME);
        assert!(parse_output_frame(br#"{"type":"ok"}"#).is_none());
        assert!(parse_output_frame(b"").is_none());
    }

    #[tokio::test]
    async fn reader_reads_frames_then_eof() {
        let mut stream = encode_payload(b"one").unwrap();
        stream.extend(encode_payload(b"two").unwrap());
        let (mut a, b) = tokio::io::duplex(3);
        let writer = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            a.write_all(&stream).await.unwrap();
        });
        let mut reader = FrameReader::new(b);
        assert_eq!(reader.next().await.unwrap().unwrap(), b"one");
        assert_eq!(reader.next().await.unwrap().unwrap(), b"two");
        writer.await.unwrap();
        assert_eq!(reader.next().await.unwrap(), None);
    }

    #[tokio::test]
    async fn reader_reports_truncation() {
        let (mut a, b) = tokio::io::duplex(64);
        use tokio::io::AsyncWriteExt;
        a.write_all(&[0, 0, 0, 9, 1, 2]).await.unwrap();
        drop(a);
        let mut reader = FrameReader::new(b);
        assert_eq!(reader.next().await.unwrap_err(), TRUNCATED_FRAME);
    }
}
