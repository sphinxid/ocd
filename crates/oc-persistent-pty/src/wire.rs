//! Protocol-7 wire types (`persistent-pty/daemon.ts:11-132`).

use serde::{Deserialize, Serialize};

/// `Lifecycle`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Lifecycle {
    Running,
    Exited { exit_code: Option<u64> },
    Failed { message: String },
}

/// `WireTerminal`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireTerminal {
    pub id: u64,
    pub pid: Option<u64>,
    pub title: String,
    pub foreground_process: Option<String>,
    pub group_id: String,
    pub command: Vec<String>,
    pub cwd: String,
    pub cols: u64,
    pub rows: u64,
    pub lifecycle: Lifecycle,
    pub output_head: u64,
    pub output_tail: u64,
}

/// `Registration` (`service.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub instance_id: String,
    pub pid: u64,
    pub protocol: u64,
    pub socket: String,
    pub token: String,
}

/// Attachment role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Controller,
    Observer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Controller => "controller",
            Role::Observer => "observer",
        }
    }
}

/// `WireResponse`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireResponse {
    Pong { instance_id: String, pid: u64, protocol: u64 },
    Created { terminal: WireTerminal },
    Terminals { terminals: Vec<WireTerminal> },
    Ok,
    Owned,
    Handoff { ticket: String, expires_at: u64 },
    Snapshot { terminal: WireTerminal, text: String, checkpoint_base64: String, cursor_x: u64, cursor_y: u64 },
    Rows { terminal: WireTerminal, lines: Vec<String>, cursor_x: u64, cursor_y: u64 },
    Attached {
        terminal: WireTerminal,
        role: Role,
        generation: u64,
        requested_offset: u64,
        available_offset: u64,
        end_offset: u64,
        truncated: bool,
        replay_base64: String,
    },
    Resized { cols: u64, rows: u64, generation: u64, checkpoint_base64: String },
    Exited { exit_code: Option<u64>, final_offset: u64 },
    ControllerChanged { attachment_id: Option<String>, generation: u64 },
    TitleChanged { title: String },
    ForegroundProcessChanged { process: Option<String> },
    Error { message: String },
}

impl WireResponse {
    /// The `type` discriminator.
    pub fn type_name(&self) -> &'static str {
        match self {
            WireResponse::Pong { .. } => "pong",
            WireResponse::Created { .. } => "created",
            WireResponse::Terminals { .. } => "terminals",
            WireResponse::Ok => "ok",
            WireResponse::Owned => "owned",
            WireResponse::Handoff { .. } => "handoff",
            WireResponse::Snapshot { .. } => "snapshot",
            WireResponse::Rows { .. } => "rows",
            WireResponse::Attached { .. } => "attached",
            WireResponse::Resized { .. } => "resized",
            WireResponse::Exited { .. } => "exited",
            WireResponse::ControllerChanged { .. } => "controller_changed",
            WireResponse::TitleChanged { .. } => "title_changed",
            WireResponse::ForegroundProcessChanged { .. } => "foreground_process_changed",
            WireResponse::Error { .. } => "error",
        }
    }

    /// `decode(payload)`: UTF-8 JSON → response.
    pub fn decode(payload: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(payload).map_err(|e| e.to_string())
    }
}

/// Live subscription event (`StreamEvent`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    Output { start: u64, end: u64, data: Vec<u8> },
    Resized { cols: u64, rows: u64, generation: u64, checkpoint: Vec<u8> },
    Exited { exit_code: Option<u64>, final_offset: u64 },
    ControllerChanged { attachment_id: Option<String>, generation: u64 },
    TitleChanged { title: String },
    ForegroundProcessChanged { process: Option<String> },
}

impl StreamEvent {
    pub fn type_name(&self) -> &'static str {
        match self {
            StreamEvent::Output { .. } => "output",
            StreamEvent::Resized { .. } => "resized",
            StreamEvent::Exited { .. } => "exited",
            StreamEvent::ControllerChanged { .. } => "controller_changed",
            StreamEvent::TitleChanged { .. } => "title_changed",
            StreamEvent::ForegroundProcessChanged { .. } => "foreground_process_changed",
        }
    }

    /// The JSON text frame the `persistentPty.connect` WebSocket sends for this event
    /// (`server/src/handlers/persistent-pty.ts:136-145`): `JSON.stringify(event)` with
    /// undefined keys dropped, and `resized.checkpoint` as base64. `None` for `output`,
    /// which is sent as a raw binary frame.
    pub fn to_ws_json(&self) -> Option<serde_json::Value> {
        use base64::Engine;
        use serde_json::{Map, Value, json};
        Some(match self {
            StreamEvent::Output { .. } => return None,
            StreamEvent::Resized { cols, rows, generation, checkpoint } => json!({
                "type": "resized",
                "cols": cols,
                "rows": rows,
                "generation": generation,
                "checkpoint": base64::engine::general_purpose::STANDARD.encode(checkpoint),
            }),
            StreamEvent::Exited { exit_code, final_offset } => {
                let mut map = Map::new();
                map.insert("type".into(), "exited".into());
                if let Some(code) = exit_code {
                    map.insert("exitCode".into(), (*code).into());
                }
                map.insert("finalOffset".into(), (*final_offset).into());
                Value::Object(map)
            }
            StreamEvent::ControllerChanged { attachment_id, generation } => {
                let mut map = Map::new();
                map.insert("type".into(), "controller_changed".into());
                if let Some(id) = attachment_id {
                    map.insert("attachmentID".into(), id.clone().into());
                }
                map.insert("generation".into(), (*generation).into());
                Value::Object(map)
            }
            StreamEvent::TitleChanged { title } => json!({ "type": "title_changed", "title": title }),
            StreamEvent::ForegroundProcessChanged { process } => {
                json!({ "type": "foreground_process_changed", "process": process })
            }
        })
    }
}

/// Map a JSON subscription response to an event (`daemon.ts:599-624`). Response types that
/// are not stream events yield `None` and are ignored by the pump.
pub fn stream_event(response: WireResponse) -> Result<Option<StreamEvent>, String> {
    Ok(Some(match response {
        WireResponse::Resized { cols, rows, generation, checkpoint_base64 } => StreamEvent::Resized {
            cols,
            rows,
            generation,
            checkpoint: decode_base64(&checkpoint_base64),
        },
        WireResponse::ControllerChanged { attachment_id, generation } => {
            StreamEvent::ControllerChanged { attachment_id, generation }
        }
        WireResponse::TitleChanged { title } => StreamEvent::TitleChanged { title },
        WireResponse::ForegroundProcessChanged { process } => StreamEvent::ForegroundProcessChanged { process },
        WireResponse::Exited { exit_code, final_offset } => StreamEvent::Exited { exit_code, final_offset },
        _ => return Ok(None),
    }))
}

/// Node's `Buffer.from(s, "base64")`: lenient (ignores invalid characters and padding).
pub fn decode_base64(value: &str) -> Vec<u8> {
    use base64::Engine;
    let cleaned: String = value
        .chars()
        .filter_map(|c| match c {
            '-' => Some('+'),
            '_' => Some('/'),
            c if c.is_ascii_alphanumeric() || c == '+' || c == '/' => Some(c),
            _ => None,
        })
        .collect();
    // A single dangling sextet carries no full byte; drop it like Node does.
    let usable = if cleaned.len() % 4 == 1 { &cleaned[..cleaned.len() - 1] } else { &cleaned[..] };
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(usable)
        .or_else(|_| {
            base64::engine::GeneralPurpose::new(
                &base64::alphabet::STANDARD,
                base64::engine::GeneralPurposeConfig::new()
                    .with_decode_allow_trailing_bits(true)
                    .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
            )
            .decode(usable)
        })
        .unwrap_or_default()
}

pub fn encode_base64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn terminal_json() -> serde_json::Value {
        json!({
            "id": 3, "pid": 42, "title": "Terminal", "foreground_process": null, "group_id": "ses_1",
            "command": ["/bin/bash", "-i"], "cwd": "/tmp", "cols": 80, "rows": 24,
            "lifecycle": {"status": "exited", "exit_code": null}, "output_head": 0, "output_tail": 12
        })
    }

    #[test]
    fn decodes_terminal_and_lifecycles() {
        let t: WireTerminal = serde_json::from_value(terminal_json()).unwrap();
        assert_eq!(t.lifecycle, Lifecycle::Exited { exit_code: None });
        assert_eq!(t.command, vec!["/bin/bash", "-i"]);
        let running: Lifecycle = serde_json::from_value(json!({"status": "running"})).unwrap();
        assert_eq!(running, Lifecycle::Running);
        let failed: Lifecycle = serde_json::from_value(json!({"status": "failed", "message": "boom"})).unwrap();
        assert_eq!(failed, Lifecycle::Failed { message: "boom".into() });
    }

    #[test]
    fn decodes_responses() {
        let pong = WireResponse::decode(br#"{"type":"pong","instance_id":"i","pid":7,"protocol":7}"#).unwrap();
        assert_eq!(pong, WireResponse::Pong { instance_id: "i".into(), pid: 7, protocol: 7 });
        assert_eq!(WireResponse::decode(br#"{"type":"ok"}"#).unwrap(), WireResponse::Ok);
        assert_eq!(WireResponse::decode(br#"{"type":"owned"}"#).unwrap(), WireResponse::Owned);
        let handoff = WireResponse::decode(br#"{"type":"handoff","ticket":"t","expires_at":1700000000000}"#).unwrap();
        assert_eq!(handoff.type_name(), "handoff");
        let attached = json!({
            "type": "attached", "terminal": terminal_json(), "role": "observer", "generation": 2,
            "requested_offset": 0, "available_offset": 0, "end_offset": 12, "truncated": false,
            "replay_base64": "aGVsbG8="
        });
        let attached = WireResponse::decode(attached.to_string().as_bytes()).unwrap();
        assert!(matches!(attached, WireResponse::Attached { role: Role::Observer, generation: 2, .. }));
        let error = WireResponse::decode(br#"{"type":"error","message":"authentication failed"}"#).unwrap();
        assert_eq!(error, WireResponse::Error { message: "authentication failed".into() });
        assert!(WireResponse::decode(br#"{"type":"nope"}"#).is_err());
        assert!(WireResponse::decode(b"not json").is_err());
    }

    #[test]
    fn maps_stream_events() {
        let r = WireResponse::decode(br#"{"type":"resized","cols":100,"rows":30,"generation":4,"checkpoint_base64":"AAE="}"#)
            .unwrap();
        assert_eq!(
            stream_event(r).unwrap(),
            Some(StreamEvent::Resized { cols: 100, rows: 30, generation: 4, checkpoint: vec![0, 1] })
        );
        let r = WireResponse::decode(br#"{"type":"exited","exit_code":3,"final_offset":99}"#).unwrap();
        assert_eq!(stream_event(r).unwrap(), Some(StreamEvent::Exited { exit_code: Some(3), final_offset: 99 }));
        assert_eq!(stream_event(WireResponse::Ok).unwrap(), None);
    }

    #[test]
    fn ws_json_matches_json_stringify() {
        let e = StreamEvent::Exited { exit_code: None, final_offset: 5 };
        assert_eq!(e.to_ws_json().unwrap().to_string(), r#"{"type":"exited","finalOffset":5}"#);
        let e = StreamEvent::Exited { exit_code: Some(0), final_offset: 5 };
        assert_eq!(e.to_ws_json().unwrap().to_string(), r#"{"type":"exited","exitCode":0,"finalOffset":5}"#);
        let e = StreamEvent::ControllerChanged { attachment_id: None, generation: 1 };
        assert_eq!(e.to_ws_json().unwrap().to_string(), r#"{"type":"controller_changed","generation":1}"#);
        let e = StreamEvent::ControllerChanged { attachment_id: Some("a".into()), generation: 1 };
        assert_eq!(
            e.to_ws_json().unwrap().to_string(),
            r#"{"type":"controller_changed","attachmentID":"a","generation":1}"#
        );
        let e = StreamEvent::ForegroundProcessChanged { process: None };
        assert_eq!(e.to_ws_json().unwrap().to_string(), r#"{"type":"foreground_process_changed","process":null}"#);
        let e = StreamEvent::TitleChanged { title: "t".into() };
        assert_eq!(e.to_ws_json().unwrap().to_string(), r#"{"type":"title_changed","title":"t"}"#);
        let e = StreamEvent::Resized { cols: 1, rows: 2, generation: 3, checkpoint: vec![255] };
        assert_eq!(
            e.to_ws_json().unwrap().to_string(),
            r#"{"type":"resized","cols":1,"rows":2,"generation":3,"checkpoint":"/w=="}"#
        );
        assert!(StreamEvent::Output { start: 0, end: 0, data: vec![] }.to_ws_json().is_none());
    }

    #[test]
    fn lenient_base64() {
        assert_eq!(decode_base64("aGVsbG8="), b"hello");
        assert_eq!(decode_base64("aGVsbG8"), b"hello");
        assert_eq!(decode_base64(""), b"");
        assert_eq!(decode_base64("aGVs\nbG8="), b"hello");
    }
}
