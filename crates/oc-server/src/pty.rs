//! PTY WebSocket transports (spec 06 §9).

use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::Response;
use base64::Engine;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde_json::json;

use crate::app::App;
use crate::auth::TicketScope;
use crate::backend::{
    PersistentPtyAttachRequest, PersistentPtyEvent, PtyAttachRequest, PtyRole,
};
use crate::context::RequestContext;
use crate::cors::is_allowed_request_origin;
use crate::error::ApiError;
use crate::generated::{Backend, PtyGetRequest};
use crate::router::{Call, parse_js_number};

/// Replay frames carry at most this many UTF-16 code units.
pub const REPLAY_CHUNK: usize = 64 * 1024;

/// Split replay text into chunks of ≤ `REPLAY_CHUNK` UTF-16 code units (never splitting a
/// character).
pub fn replay_chunks(data: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut units = 0;
    for (i, ch) in data.char_indices() {
        let n = ch.len_utf16();
        if units + n > REPLAY_CHUNK {
            out.push(&data[start..i]);
            start = i;
            units = 0;
        }
        units += n;
    }
    if start < data.len() {
        out.push(&data[start..]);
    }
    out
}

/// `0x00` + UTF-8 JSON `{"cursor":n}`.
pub fn meta_frame(cursor: i64) -> Vec<u8> {
    let mut out = vec![0u8];
    out.extend_from_slice(json!({ "cursor": cursor }).to_string().as_bytes());
    out
}

/// Framed persistent-PTY input: `[type:u8][cols:u16 BE][rows:u16 BE][payload…]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FramedInput<'a> {
    Control { cols: u16, rows: u16 },
    Input { cols: u16, rows: u16, payload: &'a [u8] },
}

/// Decode a framed input message; `None` = ignore.
pub fn decode_framed(data: &[u8]) -> Option<FramedInput<'_>> {
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
        1 => Some(FramedInput::Input { cols, rows, payload: &data[5..] }),
        _ => None,
    }
}

fn close(code: u16, reason: &str) -> Message {
    Message::Close(Some(CloseFrame { code, reason: reason.into() }))
}

fn ticket(call: &Call) -> Option<&str> {
    call.query_raw("ticket").filter(|t| !t.is_empty())
}

/// `GET /api/pty/{ptyID}/connect`.
pub(crate) async fn connect(app: &App, call: Call, ws: Option<WebSocketUpgrade>) -> Result<Response, ApiError> {
    if !is_allowed_request_origin(&call.headers, &app.cors) {
        return Err(ApiError::Empty(StatusCode::FORBIDDEN));
    }
    let pty_id = call.path_str("ptyID", Some("^pty"))?;
    let location = call.ctx.location.clone();
    if let Some(t) = ticket(&call) {
        let scope = TicketScope {
            pty_id: pty_id.clone(),
            directory: Some(location.directory.clone()),
            workspace_id: location.workspace_id.clone(),
        };
        if !app.tickets.consume(t, &scope) {
            return Err(ApiError::Empty(StatusCode::FORBIDDEN));
        }
    }
    app.check_location(&location)?;
    if app.backend.pty_get(&call.ctx, PtyGetRequest { pty_id: pty_id.clone() }).await.is_err() {
        return Err(ApiError::Empty(StatusCode::NOT_FOUND));
    }
    let cursor = call
        .query_raw("cursor")
        .and_then(parse_js_number)
        .filter(|n| n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0 && *n >= -1.0)
        .map(|n| n as i64);
    let Some(ws) = ws else { return Err(ApiError::defect("pty.connect requires a WebSocket upgrade")) };
    let backend = app.backend.clone();
    let ctx = call.ctx.clone();
    Ok(ws.on_upgrade(move |socket| run_pty(socket, backend, ctx, PtyAttachRequest { pty_id, cursor })))
}

async fn run_pty(socket: WebSocket, backend: Arc<dyn Backend>, ctx: RequestContext, req: PtyAttachRequest) {
    let (mut sink, mut stream) = socket.split();
    let attachment = match backend.pty_connect(&ctx, req).await {
        Ok(a) => a,
        Err(e) => {
            let reason = if e == ApiError::pty_exited() { "session exited" } else { "session not found" };
            let _ = sink.send(close(4404, reason)).await;
            return;
        }
    };
    let control = attachment.control.clone();
    let mut output = attachment.output;
    let ok = async {
        for chunk in replay_chunks(&attachment.replay) {
            sink.send(Message::Text(chunk.into())).await.ok()?;
        }
        sink.send(Message::Binary(Bytes::from(meta_frame(attachment.cursor)))).await.ok()?;
        Some(())
    }
    .await;
    if ok.is_some() {
        loop {
            tokio::select! {
                out = output.next() => match out {
                    Some(text) => if sink.send(Message::Text(text.into())).await.is_err() { break },
                    None => { let _ = sink.send(close(1000, "")).await; break }
                },
                msg = stream.next() => match msg {
                    Some(Ok(Message::Text(t))) => control.write(t.as_str()),
                    Some(Ok(Message::Binary(b))) => if let Ok(s) = std::str::from_utf8(&b) { control.write(s) },
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    _ => break,
                },
            }
        }
    }
    control.detach();
}

/// `GET /api/experimental/persistent-pty/{ptyID}/connect`.
pub(crate) async fn persistent_connect(app: &App, call: Call, ws: Option<WebSocketUpgrade>) -> Result<Response, ApiError> {
    if !is_allowed_request_origin(&call.headers, &app.cors) {
        return Err(ApiError::Empty(StatusCode::FORBIDDEN));
    }
    let pty_id = call.path_str("ptyID", Some("^pty"))?;
    if let Some(t) = ticket(&call) {
        let scope = TicketScope { pty_id: pty_id.clone(), directory: None, workspace_id: None };
        if !app.tickets.consume(t, &scope) {
            return Err(ApiError::Empty(StatusCode::FORBIDDEN));
        }
    }
    let cursor = parse_js_number(call.query_raw("cursor").unwrap_or("0"))
        .filter(|n| n.fract() == 0.0 && *n >= 0.0 && *n <= 9_007_199_254_740_991.0)
        .ok_or(ApiError::Empty(StatusCode::BAD_REQUEST))? as u64;
    let role = if call.query_raw("role") == Some("observer") { PtyRole::Observer } else { PtyRole::Controller };
    let framed = call.query_raw("input_protocol") == Some("1");
    let attachment_id = call.query_raw("attachment_id").map(str::to_string).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let takeover = call.query_raw("takeover") == Some("true");
    let Some(ws) = ws else { return Err(ApiError::defect("persistentPty.connect requires a WebSocket upgrade")) };
    let backend = app.backend.clone();
    let ctx = call.ctx.clone();
    let req = PersistentPtyAttachRequest { pty_id, cursor, attachment_id, role, takeover };
    Ok(ws.on_upgrade(move |socket| run_persistent(socket, backend, ctx, req, framed)))
}

async fn run_persistent(
    socket: WebSocket,
    backend: Arc<dyn Backend>,
    ctx: RequestContext,
    req: PersistentPtyAttachRequest,
    framed: bool,
) {
    let (mut sink, mut stream) = socket.split();
    let attachment_id = req.attachment_id.clone();
    let attachment = match backend.persistent_pty_connect(&ctx, req).await {
        Ok(a) => a,
        Err(_) => {
            let _ = sink.send(close(4404, "terminal unavailable")).await;
            return;
        }
    };
    let control = attachment.control.clone();
    let cols = attachment.info.size.cols as u16;
    let rows = attachment.info.size.rows as u16;
    let attached = json!({
        "type": "attached",
        "attachmentID": attachment_id,
        "inputProtocol": if framed { 1 } else { 0 },
        "info": attachment.info,
        "role": attachment.role,
        "generation": attachment.generation,
        "replay": {
            "requestedOffset": attachment.replay.requested_offset,
            "availableOffset": attachment.replay.available_offset,
            "endOffset": attachment.replay.end_offset,
            "truncated": attachment.replay.truncated,
        },
    });
    let end_offset = attachment.replay.end_offset;
    let replay = attachment.replay.data.clone();
    let mut events = attachment.events;
    let ok = async {
        sink.send(Message::Text(attached.to_string().into())).await.ok()?;
        if !replay.is_empty() {
            sink.send(Message::Binary(replay)).await.ok()?;
        }
        sink.send(Message::Text(json!({ "type": "replay_complete", "endOffset": end_offset }).to_string().into()))
            .await
            .ok()?;
        Some(())
    }
    .await;
    if ok.is_some() {
        loop {
            tokio::select! {
                ev = events.next() => {
                    let msg = match ev {
                        Some(PersistentPtyEvent::Output(b)) => Message::Binary(b),
                        Some(PersistentPtyEvent::Resized { cols, rows, generation, checkpoint }) => Message::Text(
                            json!({
                                "type": "resized",
                                "cols": cols,
                                "rows": rows,
                                "generation": generation,
                                "checkpoint": base64::engine::general_purpose::STANDARD.encode(&checkpoint),
                            })
                            .to_string()
                            .into(),
                        ),
                        Some(PersistentPtyEvent::Json(v)) => Message::Text(crate::support::to_js_string(&v).unwrap_or_default().into()),
                        None => { let _ = sink.send(close(1000, "")).await; break }
                    };
                    if sink.send(msg).await.is_err() { break }
                }
                msg = stream.next() => {
                    let data: Bytes = match msg {
                        Some(Ok(Message::Text(t))) => Bytes::copy_from_slice(t.as_str().as_bytes()),
                        Some(Ok(Message::Binary(b))) => b,
                        Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                        _ => break,
                    };
                    if !framed {
                        control.input(cols, rows, data).await;
                        continue;
                    }
                    match decode_framed(&data) {
                        Some(FramedInput::Control { cols, rows }) => control.control(cols, rows).await,
                        Some(FramedInput::Input { cols, rows, payload }) => {
                            control.input(cols, rows, Bytes::copy_from_slice(payload)).await
                        }
                        None => {}
                    }
                }
            }
        }
    }
    control.detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunking_counts_utf16_units() {
        let s = "a".repeat(REPLAY_CHUNK + 1);
        let c = replay_chunks(&s);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].len(), REPLAY_CHUNK);
        // astral chars count 2 units and are never split
        let s = "😀".repeat(REPLAY_CHUNK / 2 + 1);
        let c = replay_chunks(&s);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].chars().count(), REPLAY_CHUNK / 2);
        assert!(replay_chunks("").is_empty());
    }

    #[test]
    fn meta_and_framed() {
        assert_eq!(meta_frame(42), b"\x00{\"cursor\":42}".to_vec());
        assert_eq!(decode_framed(&[0, 0, 80, 0, 24]), Some(FramedInput::Control { cols: 80, rows: 24 }));
        assert_eq!(decode_framed(&[1, 0, 80, 0, 24, b'x']), Some(FramedInput::Input { cols: 80, rows: 24, payload: b"x" }));
        assert_eq!(decode_framed(&[1, 0, 0, 0, 24, b'x']), None);
        assert_eq!(decode_framed(&[2, 0, 80, 0, 24]), None);
        assert_eq!(decode_framed(&[1, 0, 80, 0]), None);
    }
}
