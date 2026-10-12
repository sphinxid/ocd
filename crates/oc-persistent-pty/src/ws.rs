//! `persistentPty.connect` WebSocket message helpers
//! (`server/src/handlers/persistent-pty.ts:108-207`).
//!
//! Outbound order: text [`attached_message`], binary replay bytes (only when non-empty), text
//! [`replay_complete_message`], then live events ([`outbound`]); close `1000` when the stream
//! ends, `4404 "terminal unavailable"` when attaching fails.

use serde_json::{Value, json};

use crate::Attachment;
use crate::wire::StreamEvent;

/// Close code/reason when attach fails.
pub const CLOSE_UNAVAILABLE: (u16, &str) = (4404, "terminal unavailable");

/// `{"type":"attached","attachmentID","inputProtocol","info","role","generation","replay":{...}}`.
pub fn attached_message(attachment_id: &str, framed_input: bool, attachment: &Attachment) -> Value {
    json!({
        "type": "attached",
        "attachmentID": attachment_id,
        "inputProtocol": if framed_input { 1 } else { 0 },
        "info": attachment.info,
        "role": attachment.role.as_str(),
        "generation": attachment.generation,
        "replay": {
            "requestedOffset": attachment.replay.requested_offset,
            "availableOffset": attachment.replay.available_offset,
            "endOffset": attachment.replay.end_offset,
            "truncated": attachment.replay.truncated,
        },
    })
}

/// `{"type":"replay_complete","endOffset"}`.
pub fn replay_complete_message(end_offset: u64) -> Value {
    json!({ "type": "replay_complete", "endOffset": end_offset })
}

/// One outbound WebSocket frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outbound {
    Binary(Vec<u8>),
    Text(String),
}

/// Live event → frame: output is a binary frame of raw bytes (offsets dropped), everything
/// else a JSON text frame.
pub fn outbound(event: StreamEvent) -> Outbound {
    match event {
        StreamEvent::Output { data, .. } => Outbound::Binary(data),
        other => Outbound::Text(other.to_ws_json().expect("non-output event").to_string()),
    }
}

/// Inbound framed input (`input_protocol=1`): `[type u8][cols u16 BE][rows u16 BE][payload…]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FramedInput<'a> {
    /// type 0 → `control` (claim / resize as controller).
    Control { cols: u16, rows: u16 },
    /// type 1 → `input` with payload.
    Input { cols: u16, rows: u16, data: &'a [u8] },
}

impl<'a> FramedInput<'a> {
    /// `None` (ignored) for < 5 bytes, unknown type, or zero cols/rows.
    pub fn decode(data: &'a [u8]) -> Option<Self> {
        if data.len() < 5 {
            return None;
        }
        let cols = u16::from_be_bytes([data[1], data[2]]);
        let rows = u16::from_be_bytes([data[3], data[4]]);
        if cols == 0 || rows == 0 {
            return None;
        }
        match data[0] {
            0 => Some(FramedInput::Control { cols, rows }),
            1 => Some(FramedInput::Input { cols, rows, data: &data[5..] }),
            _ => None,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let (kind, cols, rows, data): (u8, u16, u16, &[u8]) = match self {
            FramedInput::Control { cols, rows } => (0, *cols, *rows, &[]),
            FramedInput::Input { cols, rows, data } => (1, *cols, *rows, data),
        };
        let mut out = vec![kind];
        out.extend_from_slice(&cols.to_be_bytes());
        out.extend_from_slice(&rows.to_be_bytes());
        out.extend_from_slice(data);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framed_input() {
        assert_eq!(FramedInput::decode(&[0, 0, 80, 0, 24]), Some(FramedInput::Control { cols: 80, rows: 24 }));
        assert_eq!(
            FramedInput::decode(&[1, 0, 80, 0, 24, b'l', b's']),
            Some(FramedInput::Input { cols: 80, rows: 24, data: b"ls" })
        );
        assert_eq!(FramedInput::decode(&[1, 0, 80, 0, 24]), Some(FramedInput::Input { cols: 80, rows: 24, data: b"" }));
        assert_eq!(FramedInput::decode(&[1, 0, 80, 0]), None);
        assert_eq!(FramedInput::decode(&[2, 0, 80, 0, 24]), None);
        assert_eq!(FramedInput::decode(&[1, 0, 0, 0, 24, b'x']), None);
        assert_eq!(FramedInput::decode(&[0, 1, 0, 0, 0]), None);
        let frame = FramedInput::Input { cols: 300, rows: 2, data: b"x" };
        assert_eq!(frame.encode(), vec![1, 1, 44, 0, 2, b'x']);
        assert_eq!(FramedInput::decode(&frame.encode()), Some(frame));
    }

    #[test]
    fn outbound_frames() {
        assert_eq!(outbound(StreamEvent::Output { start: 0, end: 2, data: b"hi".to_vec() }), Outbound::Binary(b"hi".to_vec()));
        assert_eq!(
            outbound(StreamEvent::TitleChanged { title: "x".into() }),
            Outbound::Text(r#"{"type":"title_changed","title":"x"}"#.into())
        );
        assert_eq!(replay_complete_message(9).to_string(), r#"{"type":"replay_complete","endOffset":9}"#);
    }
}
