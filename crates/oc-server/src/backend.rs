//! Hand-written request/response types for the operations whose transport is not plain JSON
//! (pagination cursors, SSE, raw files, WebSockets). The [`Backend`] trait itself is generated.

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

pub use crate::context::{LocationRef, RequestContext};
pub use crate::error::ApiError;
pub use crate::generated::Backend;
use crate::generated::{PersistentPtyInfo, SessionListOrder, SessionMessageListOrder, SessionMessageListType};

// ---- session.list ----------------------------------------------------------------------------

/// `session.list` input after cursor decoding. When the client sent a `cursor`, `query` is the
/// query embedded in it (with `anchor` set) and `limit` still comes from the URL.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionListRequest {
    /// Page size (default 50, positive).
    pub limit: i64,
    pub query: SessionListQuery,
}

/// Filters of `GET /api/session` (and the JSON embedded in its cursors).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SessionListQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<SessionListOrder>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    /// `Some(ParentFilter::Root)` = root sessions only (`parentID=null`).
    #[serde(rename = "parentID", default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<ParentFilter>,
    /// Archive state filter, `"true"`/`"false"` on the wire (query and cursor JSON).
    #[serde(default, skip_serializing_if = "Option::is_none", with = "bool_string")]
    pub archived: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
    /// Present when paging from a cursor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<SessionListAnchor>,
}

/// `Schema.Literals(["true", "false"])` decoded to a boolean.
mod bool_string {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<bool>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(b) => s.serialize_str(if *b { "true" } else { "false" }),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
        match String::deserialize(d)?.as_str() {
            "true" => Ok(Some(true)),
            "false" => Ok(Some(false)),
            _ => Err(serde::de::Error::custom("expected \"true\" or \"false\"")),
        }
    }
}

/// `parentID` filter: a session ID, or the literal `"null"` meaning "root sessions only".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentFilter {
    Root,
    Session(String),
}

impl Serialize for ParentFilter {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            ParentFilter::Root => s.serialize_str("null"),
            ParentFilter::Session(id) => s.serialize_str(id),
        }
    }
}

impl<'de> Deserialize<'de> for ParentFilter {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = String::deserialize(d)?;
        Self::parse(&v).ok_or_else(|| serde::de::Error::custom("expected a session ID or \"null\""))
    }
}

impl ParentFilter {
    pub(crate) fn parse(v: &str) -> Option<Self> {
        if v == "null" {
            Some(ParentFilter::Root)
        } else if v.starts_with("ses") {
            Some(ParentFilter::Session(v.to_string()))
        } else {
            None
        }
    }
}

/// Cursor anchor (`Session.ListAnchor`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListAnchor {
    pub id: String,
    /// `time.updated` of the anchor session, epoch ms.
    pub time: i64,
    pub direction: CursorDirection,
}

/// Paging direction relative to the anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorDirection {
    Previous,
    Next,
}

// ---- session.message.list --------------------------------------------------------------------

/// `session.message.list` input after cursor decoding.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionMessageListRequest {
    pub session_id: String,
    /// 1..=200, default 50.
    pub limit: i64,
    /// From the cursor, else the query, else `desc`.
    pub order: SessionMessageListOrder,
    pub r#type: Option<SessionMessageListType>,
    pub cursor: Option<MessageCursor>,
}

/// Decoded message cursor (`{id, direction}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageCursor {
    pub id: String,
    pub direction: CursorDirection,
}

// ---- session.log -------------------------------------------------------------------------------

/// A mid-stream failure of the session log: emitted as the reserved
/// `event: effect/http-api/stream/failure` SSE event whose data is the encoded cause array.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamFailure(pub Value);

impl StreamFailure {
    /// `[{"_tag":"Die","defect":<defect>}]`
    pub fn die(defect: impl Into<Value>) -> Self {
        StreamFailure(serde_json::json!([{ "_tag": "Die", "defect": defect.into() }]))
    }
}

/// Items of the session log: durable session events (with `durable.seq`) or sync markers
/// `{"type":"log.synced","aggregateID":...,"seq":n}` (already JSON-encoded values).
pub type SessionLogStream = BoxStream<'static, Result<Value, StreamFailure>>;

// ---- event.subscribe ---------------------------------------------------------------------------

/// `GET /api/event` subscription request. `location` is `None` for the global feed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EventSubscribeRequest {
    pub location: Option<LocationRef>,
}

/// Source of bus events (`Event.Payload` JSON: `{id, created, metadata?, type, durable?,
/// location?, data}`). The server filters to public types, frames them as `data: <json>`, and
/// drops a subscriber that falls 4096 frames behind (or lags its broadcast channel).
pub enum EventSubscription {
    Broadcast(broadcast::Receiver<Value>),
    Stream(BoxStream<'static, Value>),
}

impl From<broadcast::Receiver<Value>> for EventSubscription {
    fn from(rx: broadcast::Receiver<Value>) -> Self {
        EventSubscription::Broadcast(rx)
    }
}

// ---- fs.read -------------------------------------------------------------------------------

/// `fs.read` input: the URL path after `/api/fs/read/`, percent-decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsReadRequest {
    pub path: String,
}

/// A file to stream (the server implements ETag / Last-Modified / Range).
#[derive(Debug, Clone)]
pub struct FsFile {
    pub size: u64,
    /// Modification time, epoch ms.
    pub mtime_ms: Option<i64>,
    pub mime: String,
    pub source: FsSource,
}

/// Where the bytes of an [`FsFile`] come from.
#[derive(Debug, Clone)]
pub enum FsSource {
    Path(PathBuf),
    Bytes(Bytes),
}

// ---- experimental.media.get ------------------------------------------------------------------

/// A content-addressed media blob (`SessionBlob.read`): its stored mime and raw bytes. The server
/// adds the cache/ETag/security headers and answers `If-None-Match` with 304.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaBlob {
    pub mime: String,
    pub data: Bytes,
}

// ---- pty.connect -------------------------------------------------------------------------------

/// `pty.connect` attach input. `cursor`: absolute output cursor (`-1` = tail only, `None` = the
/// whole retained buffer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyAttachRequest {
    pub pty_id: String,
    pub cursor: Option<i64>,
}

/// Input side of a location PTY attachment.
pub trait PtyControl: Send + Sync {
    /// Write terminal input (text frames verbatim; binary frames after strict UTF-8 decode).
    fn write(&self, data: &str);
    /// Called once when the socket closes.
    fn detach(&self) {}
}

/// A live attachment to a location PTY.
///
/// Return `Err(ApiError::pty_not_found(..))` from `pty_connect` for close `4404 "session not
/// found"` and `Err(ApiError::pty_exited())` for `4404 "session exited"`.
pub struct PtyAttachment {
    /// Retained output from the cursor (sent as text frames of ≤ 65 536 UTF-16 units).
    pub replay: String,
    /// Absolute cursor after the replay (sent in the `0x00` meta frame).
    pub cursor: i64,
    /// Live output chunks; the socket closes with `1000` when the stream ends.
    pub output: BoxStream<'static, String>,
    pub control: Arc<dyn PtyControl>,
}

// ---- persistentPty.connect ---------------------------------------------------------------------

/// Attachment role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PtyRole {
    Controller,
    Observer,
}

/// `persistentPty.connect` attach input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistentPtyAttachRequest {
    pub pty_id: String,
    pub cursor: u64,
    pub attachment_id: String,
    pub role: PtyRole,
    pub takeover: bool,
}

/// Replay metadata + bytes for a persistent PTY attachment.
#[derive(Debug, Clone, PartialEq)]
pub struct PersistentPtyReplay {
    pub requested_offset: u64,
    pub available_offset: u64,
    pub end_offset: u64,
    pub truncated: bool,
    pub data: Bytes,
}

/// Live events of a persistent PTY.
#[derive(Debug, Clone, PartialEq)]
pub enum PersistentPtyEvent {
    /// Raw output bytes → binary frame.
    Output(Bytes),
    /// → text `{"type":"resized","cols","rows","generation","checkpoint":"<base64>"}`.
    Resized { cols: u16, rows: u16, generation: i64, checkpoint: Bytes },
    /// Any other event (`exited`, `controller_changed`, `title_changed`,
    /// `foreground_process_changed`) → text frame with this JSON.
    Json(Value),
}

/// Control side of a persistent PTY attachment.
#[async_trait::async_trait]
pub trait PersistentPtyControl: Send + Sync {
    async fn input(&self, cols: u16, rows: u16, data: Bytes);
    /// Take control / resize (framed input type 0).
    async fn control(&self, cols: u16, rows: u16);
    /// Called once when the socket closes.
    fn detach(&self) {}
}

/// A live attachment to a persistent PTY. Any `Err` from `persistent_pty_connect` closes the
/// socket with `4404 "terminal unavailable"`.
pub struct PersistentPtyAttachment {
    pub info: PersistentPtyInfo,
    pub role: PtyRole,
    pub generation: i64,
    pub replay: PersistentPtyReplay,
    /// Live events; the socket closes with `1000` when the stream ends.
    pub events: BoxStream<'static, PersistentPtyEvent>,
    pub control: Arc<dyn PersistentPtyControl>,
}

impl ApiError {
    /// Signals "PTY exited" from `pty_connect` (close `4404 "session exited"`).
    pub fn pty_exited() -> Self {
        ApiError::Empty(axum::http::StatusCode::GONE)
    }
}
