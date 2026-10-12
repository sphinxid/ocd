//! API shapes (`@opencode/schema/persistent-pty`), serialized exactly like upstream.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Value, json};

use crate::wire::{Lifecycle, WireTerminal};

pub const ID_PREFIX: &str = "pty_persistent_";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    Exited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub cols: u64,
    pub rows: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    pub head: u64,
    pub tail: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub x: u64,
    pub y: u64,
}

/// `PersistentPty.Info` = `Pty.Info` fields + `sessionID`, `foregroundProcess`, `size`, `output`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    pub id: String,
    pub title: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub status: Status,
    pub pid: u64,
    #[serde(rename = "exitCode", skip_serializing_if = "Option::is_none", default)]
    pub exit_code: Option<u64>,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "foregroundProcess")]
    pub foreground_process: Option<String>,
    pub size: Size,
    pub output: Output,
}

impl Info {
    /// `toInfo(value)` (`index.ts:390-408`).
    pub fn from_wire(value: &WireTerminal) -> Self {
        let (status, exit_code) = match &value.lifecycle {
            Lifecycle::Running => (Status::Running, None),
            Lifecycle::Exited { exit_code } => (Status::Exited, *exit_code),
            Lifecycle::Failed { .. } => (Status::Exited, None),
        };
        Info {
            id: to_id(value.id),
            title: value.title.clone(),
            command: value.command.first().cloned().unwrap_or_default(),
            args: value.command.iter().skip(1).cloned().collect(),
            cwd: value.cwd.clone(),
            status,
            pid: value.pid.unwrap_or(0),
            exit_code,
            session_id: value.group_id.clone(),
            foreground_process: value.foreground_process.clone(),
            size: Size { cols: value.cols, rows: value.rows },
            output: Output { head: value.output_head, tail: value.output_tail },
        }
    }
}

/// `toID`.
pub fn to_id(value: u64) -> String {
    format!("{ID_PREFIX}{value}")
}

/// `fromID`: `pty_persistent_<safe int ≥ 1>` (JS `Number()` parse of the suffix).
pub fn from_id(value: &str) -> Result<u64, String> {
    let invalid = || format!("invalid persistent PTY ID: {value}");
    let rest = value.strip_prefix(ID_PREFIX).ok_or_else(invalid)?;
    let parsed = js_number(rest).ok_or_else(invalid)?;
    if parsed.fract() != 0.0 || !(1.0..=9_007_199_254_740_991.0).contains(&parsed) {
        return Err(invalid());
    }
    Ok(parsed as u64)
}

/// Subset of JS `Number(string)` sufficient for ID parsing.
fn js_number(text: &str) -> Option<f64> {
    let t = text.trim();
    if t.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = t.strip_prefix(prefix) {
            return u64::from_str_radix(digits, radix).ok().map(|v| v as f64);
        }
    }
    if t.chars().any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E') {
        return None;
    }
    t.parse::<f64>().ok()
}

fn base64_bytes<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&crate::wire::encode_base64(bytes))
}

/// `PersistentPty.Snapshot` (`checkpoint` is base64 in JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub info: Info,
    pub text: String,
    #[serde(serialize_with = "base64_bytes")]
    pub checkpoint: Vec<u8>,
    pub cursor: Cursor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screen {
    pub text: String,
    pub cols: u64,
    pub rows: u64,
    pub cursor: Cursor,
}

/// `PersistentPty.ReadResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadResult {
    #[serde(rename = "ptyID")]
    pub pty_id: String,
    pub title: String,
    pub cwd: String,
    #[serde(rename = "foregroundProcess")]
    pub foreground_process: Option<String>,
    pub screen: Screen,
}

/// `PersistentPty.CreateInput` (HTTP payload of `persistentPty.create`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CreateInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub title: String,
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<Size>,
}

/// `PersistentPty.UpdateInput` (HTTP payload of `persistentPty.update`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateInput {
    #[serde(rename = "attachmentID", default, skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<String>,
    pub size: Size,
}

/// Ephemeral bus events (`persistent-pty.added` / `persistent-pty.removed`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum BusEvent {
    Added { session_id: String, terminal: Info },
    Removed { session_id: String, pty_id: String },
}

impl BusEvent {
    pub const ADDED: &'static str = "persistent-pty.added";
    pub const REMOVED: &'static str = "persistent-pty.removed";

    pub fn event_type(&self) -> &'static str {
        match self {
            BusEvent::Added { .. } => Self::ADDED,
            BusEvent::Removed { .. } => Self::REMOVED,
        }
    }

    /// Event payload: `{sessionID, terminal}` / `{sessionID, ptyID}`.
    pub fn properties(&self) -> Value {
        match self {
            BusEvent::Added { session_id, terminal } => json!({ "sessionID": session_id, "terminal": terminal }),
            BusEvent::Removed { session_id, pty_id } => json!({ "sessionID": session_id, "ptyID": pty_id }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(lifecycle: Lifecycle) -> WireTerminal {
        WireTerminal {
            id: 7,
            pid: None,
            title: "Terminal".into(),
            foreground_process: Some("vim".into()),
            group_id: "ses_abc".into(),
            command: vec!["/bin/zsh".into(), "-i".into()],
            cwd: "/home".into(),
            cols: 80,
            rows: 24,
            lifecycle,
            output_head: 1,
            output_tail: 9,
        }
    }

    #[test]
    fn info_json_key_order() {
        let info = Info::from_wire(&wire(Lifecycle::Exited { exit_code: Some(2) }));
        assert_eq!(
            serde_json::to_string(&info).unwrap(),
            r#"{"id":"pty_persistent_7","title":"Terminal","command":"/bin/zsh","args":["-i"],"cwd":"/home","status":"exited","pid":0,"exitCode":2,"sessionID":"ses_abc","foregroundProcess":"vim","size":{"cols":80,"rows":24},"output":{"head":1,"tail":9}}"#
        );
        let running = Info::from_wire(&wire(Lifecycle::Running));
        let text = serde_json::to_string(&running).unwrap();
        assert!(text.contains(r#""status":"running","pid":0,"sessionID""#), "{text}");
        let failed = Info::from_wire(&wire(Lifecycle::Failed { message: "x".into() }));
        assert_eq!((failed.status, failed.exit_code), (Status::Exited, None));
        let mut empty = wire(Lifecycle::Running);
        empty.command.clear();
        assert_eq!(Info::from_wire(&empty).command, "");
        assert_eq!(serde_json::from_str::<Info>(&text).unwrap(), running);
    }

    #[test]
    fn ids() {
        assert_eq!(to_id(12), "pty_persistent_12");
        assert_eq!(from_id("pty_persistent_12").unwrap(), 12);
        assert_eq!(from_id("pty_persistent_0").unwrap_err(), "invalid persistent PTY ID: pty_persistent_0");
        assert!(from_id("pty_persistent_").is_err());
        assert!(from_id("pty_persistent_1.5").is_err());
        assert!(from_id("pty_persistent_-1").is_err());
        assert!(from_id("pty_persistent_abc").is_err());
        assert!(from_id("pty_1").is_err());
        assert!(from_id("pty_persistent_9007199254740992").is_err());
        assert_eq!(from_id("pty_persistent_9007199254740991").unwrap(), 9_007_199_254_740_991);
    }

    #[test]
    fn snapshot_and_read_json() {
        let snapshot = Snapshot {
            info: Info::from_wire(&wire(Lifecycle::Running)),
            text: "$ ".into(),
            checkpoint: vec![1, 2, 3],
            cursor: Cursor { x: 2, y: 0 },
        };
        let value = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(value["checkpoint"], "AQID");
        assert_eq!(value["cursor"], json!({"x": 2, "y": 0}));
        let read = ReadResult {
            pty_id: "pty_persistent_1".into(),
            title: "t".into(),
            cwd: "/".into(),
            foreground_process: None,
            screen: Screen { text: "a\nb".into(), cols: 80, rows: 24, cursor: Cursor { x: 0, y: 1 } },
        };
        assert_eq!(
            serde_json::to_string(&read).unwrap(),
            r#"{"ptyID":"pty_persistent_1","title":"t","cwd":"/","foregroundProcess":null,"screen":{"text":"a\nb","cols":80,"rows":24,"cursor":{"x":0,"y":1}}}"#
        );
    }

    #[test]
    fn inputs_and_events() {
        let input: CreateInput =
            serde_json::from_str(r#"{"args":[],"title":"Terminal","env":{},"size":{"cols":100,"rows":30}}"#).unwrap();
        assert_eq!(input.size, Some(Size { cols: 100, rows: 30 }));
        assert_eq!(input.command, None);
        let update: UpdateInput = serde_json::from_str(r#"{"attachmentID":"a","size":{"cols":1,"rows":2}}"#).unwrap();
        assert_eq!(update.attachment_id.as_deref(), Some("a"));
        let removed = BusEvent::Removed { session_id: "ses".into(), pty_id: "pty_persistent_1".into() };
        assert_eq!(removed.event_type(), "persistent-pty.removed");
        assert_eq!(removed.properties().to_string(), r#"{"sessionID":"ses","ptyID":"pty_persistent_1"}"#);
    }
}
