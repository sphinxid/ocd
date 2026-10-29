//! Hand-written handlers for operations the generic dispatcher does not cover.

use axum::extract::ws::WebSocketUpgrade;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use serde_json::{Value, json};

use crate::app::App;
use crate::auth::{PAIRING_TTL_SECONDS, TICKET_TTL_SECONDS, TicketScope, session_cookie_name};
use crate::backend::{
    CursorDirection, EventSubscribeRequest, FsReadRequest, MessageCursor, ParentFilter, SessionListAnchor,
    SessionListQuery, SessionListRequest, SessionMessageListRequest,
};
use crate::cors::is_allowed_request_origin;
use crate::error::ApiError;
use crate::generated::{
    ExperimentalMediaGetRequest, ExperimentalPersistentPtyGetRequest, ExperimentalSessionLogRequest, Op, OpMeta,
    PairingCode, PairingSession, PtyGetRequest, PtyTicketConnectToken, SessionInfo, SessionMessageListOrder,
};
use crate::location::decode_uri_component;
use crate::router::{Call, respond};

pub(crate) async fn special(
    app: &App,
    meta: &'static OpMeta,
    call: Call,
    uri: Uri,
    ws: Option<WebSocketUpgrade>,
) -> Result<Response, ApiError> {
    match meta.op {
        Op::ServerPair => {
            let code = app.pairing.issue();
            Ok(respond::bare(&PairingCode { code, expires_in: PAIRING_TTL_SECONDS }))
        }
        Op::ServerConnect => server_connect(app, &call),
        Op::SessionList => session_list(app, &call).await,
        Op::SessionMessageList => session_message_list(app, &call).await,
        Op::ExperimentalSessionLog => {
            let req = ExperimentalSessionLogRequest {
                session_id: call.path_str("sessionID", Some("^ses"))?,
                after: call.query_int("after", Some(0), None)?,
                follow: call.query_bool("follow")?,
            };
            let stream = app.backend.experimental_session_log(&call.ctx, req).await?;
            Ok(crate::sse::session_log_response(stream))
        }
        Op::EventSubscribe => {
            let types = event_types(&call)?;
            let sub = app.backend.event_subscribe(&call.ctx, EventSubscribeRequest { location: None }).await?;
            Ok(crate::sse::event_response(sub, types))
        }
        Op::ExperimentalMediaGet => {
            let hash = call.path_str("hash", Some("^[0-9a-f]{64}$"))?;
            let blob = app.backend.experimental_media_get(&call.ctx, ExperimentalMediaGetRequest { hash: hash.clone() }).await?;
            Ok(media_response(&hash, blob, &call.headers))
        }
        Op::FsRead => {
            let raw = uri.path().get("/api/fs/read/".len()..).unwrap_or("").to_string();
            let path = decode_uri_component(&raw).ok_or_else(|| ApiError::file_not_found(raw.clone()))?;
            let file = app.backend.fs_read(&call.ctx, FsReadRequest { path }).await?;
            Ok(crate::fsread::serve(&call.method, &call.headers, file).await)
        }
        Op::PtyConnectToken => {
            let pty_id = call.path_str("ptyID", Some("^pty"))?;
            check_ticket_request(app, &call, "Invalid PTY connect token request")?;
            app.backend.pty_get(&call.ctx, PtyGetRequest { pty_id: pty_id.clone() }).await?;
            let ticket = app.tickets.issue(TicketScope {
                pty_id,
                directory: Some(call.ctx.location.directory.clone()),
                workspace_id: call.ctx.location.workspace_id.clone(),
            });
            Ok(respond::location(&call.ctx, &PtyTicketConnectToken { ticket, expires_in: TICKET_TTL_SECONDS }))
        }
        Op::ExperimentalPersistentPtyConnectToken => {
            let pty_id = call.path_str("ptyID", Some("^pty"))?;
            check_ticket_request(app, &call, "Invalid persistent PTY connect token request")?;
            app.backend
                .experimental_persistent_pty_get(&call.ctx, ExperimentalPersistentPtyGetRequest { pty_id: pty_id.clone() })
                .await?;
            let ticket = app.tickets.issue(TicketScope { pty_id, directory: None, workspace_id: None });
            Ok(respond::data(&PtyTicketConnectToken { ticket, expires_in: TICKET_TTL_SECONDS }))
        }
        Op::PtyConnect => crate::pty::connect(app, call, ws).await,
        Op::ExperimentalPersistentPtyConnect => crate::pty::persistent_connect(app, call, ws).await,
        other => Err(ApiError::defect(format!("no special handler for {other:?}"))),
    }
}

/// `?types=` of `event.subscribe`: `Schema.ArrayEnsure(Union(Literals(<public types>), "rpc.${string}"))`.
/// `server.connected` is not a member, so asking for it fails like any unknown type.
fn event_types(call: &Call) -> Result<Option<Vec<String>>, ApiError> {
    let values: Vec<&str> = call.query.iter().filter(|(k, _)| k == "types").map(|(_, v)| v.as_str()).collect();
    if values.is_empty() {
        return Ok(None);
    }
    for (index, v) in values.iter().enumerate() {
        if !(crate::sse::PUBLIC_EVENT_TYPES.contains(v) || v.starts_with("rpc.")) {
            let at = if values.len() == 1 { "[\"types\"]".to_string() } else { format!("[\"types\"][{index}]") };
            let literals = crate::sse::PUBLIC_EVENT_TYPES.iter().map(|t| format!("{t:?}")).collect::<Vec<_>>().join(" | ");
            return Err(ApiError::schema(
                "Query",
                format!("Expected {literals} | `rpc.${{string}}`, got {v:?}\n  at {at}"),
            ));
        }
    }
    Ok(Some(values.into_iter().map(str::to_string).collect()))
}

const MEDIA_INLINE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// `If-None-Match` against a strong ETag: `*`, the tag, or the tag with a `W/` prefix, in a list.
fn matches_if_none_match(value: Option<&str>, etag: &str) -> bool {
    let Some(value) = value else { return false };
    value.split(',').any(|tag| {
        let t = tag.trim();
        let stripped = if t.len() >= 2 && t[..2].eq_ignore_ascii_case("w/") { &t[2..] } else { t };
        t == "*" || stripped == etag
    })
}

/// `makeMediaGet`: immutable caching, a strong ETag, `nosniff` + a sandboxing CSP, and inline only
/// for the image types browsers render safely.
fn media_response(hash: &str, blob: crate::backend::MediaBlob, headers: &axum::http::HeaderMap) -> Response {
    let etag = format!("\"{hash}\"");
    let inline = MEDIA_INLINE_MIMES.contains(&blob.mime.as_str());
    let not_modified = matches_if_none_match(headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()), &etag);
    let mut res = if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let mut res = (StatusCode::OK, blob.data).into_response();
        let ct = if inline { blob.mime.as_str() } else { "application/octet-stream" };
        if let Ok(v) = HeaderValue::from_str(ct) {
            res.headers_mut().insert(header::CONTENT_TYPE, v);
        }
        res
    };
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=31536000, immutable"));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; sandbox"));
    if !inline {
        h.insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("attachment"));
    }
    res
}

/// `x-opencode-ticket: 1` and an allowed (or same-host) Origin, else `403 ForbiddenError`.
fn check_ticket_request(app: &App, call: &Call, message: &str) -> Result<(), ApiError> {
    let header_ok = call.headers.get("x-opencode-ticket").is_some_and(|v| v.as_bytes() == b"1");
    if !header_ok || !is_allowed_request_origin(&call.headers, &app.cors) {
        return Err(ApiError::forbidden(message));
    }
    Ok(())
}

fn server_connect(app: &App, call: &Call) -> Result<Response, ApiError> {
    let code = call.path_str("code", None)?;
    let browser = call
        .headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/html"));
    let token = if app.pairing.consume(&code) { app.auth.issue_session() } else { None };
    let Some(token) = token else {
        if !browser {
            return Err(ApiError::unauthorized("Pairing link expired or already used"));
        }
        return Err(ApiError::Text {
            status: StatusCode::UNAUTHORIZED,
            body: "This pairing link expired or was already used. Run `opencode pair` to get a new one.".into(),
        });
    };
    if !browser {
        return Ok(respond::bare(&PairingSession { token }));
    }
    let host = call.headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let cookie = format!(
        "{}={}; Max-Age={}; Path=/; HttpOnly; SameSite=Lax",
        session_cookie_name(host),
        token,
        crate::auth::SESSION_TTL_SECONDS
    );
    let mut res = StatusCode::FOUND.into_response();
    res.headers_mut().insert(header::LOCATION, HeaderValue::from_static("/"));
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        res.headers_mut().insert(header::SET_COOKIE, v);
    }
    Ok(res)
}

fn b64url_decode(input: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(input.trim_end_matches('=')).or_else(|_| URL_SAFE.decode(input)).ok()
}

/// Decode a `SessionsCursor` (base64url JSON with an `anchor`).
pub(crate) fn decode_sessions_cursor(input: &str) -> Option<SessionListQuery> {
    let bytes = b64url_decode(input)?;
    let q: SessionListQuery = serde_json::from_slice(&bytes).ok()?;
    q.anchor.as_ref()?;
    Some(q)
}

/// Encode a `SessionsCursor` from the page query and an anchor. The query keeps only the
/// fields of the union member it matches (directory > project > all), like upstream.
pub(crate) fn encode_sessions_cursor(query: &SessionListQuery, anchor: SessionListAnchor) -> String {
    let mut q = query.clone();
    if q.directory.is_some() {
        q.project = None;
        q.subpath = None;
    } else if q.project.is_none() {
        q.subpath = None;
    }
    q.anchor = Some(anchor);
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&q).unwrap_or_default())
}

async fn session_list(app: &App, call: &Call) -> Result<Response, ApiError> {
    // Query decode first (all fields), like the schema decoder.
    let limit = call.query_pos_int("limit", None)?.unwrap_or(50);
    let order = call.query_enum("order", r#""asc" | "desc""#)?;
    let search = call.query_str("search", None)?;
    let parent_id = match call.query_raw("parentID") {
        None => None,
        Some(v) => Some(ParentFilter::parse(v).ok_or_else(|| {
            // Union(Session.ID, "null"): a string selects the Session.ID member, whose check fails.
            ApiError::schema("Query", "Expected a string starting with \"ses\"\n  at [\"parentID\"]")
        })?),
    };
    let archived = call.query_bool("archived")?;
    let directory = call.query_str("directory", None)?;
    // `AbsolutePath` has no check: upstream fails later with a defect (500, empty body).
    if directory.as_deref().is_some_and(|d| !std::path::Path::new(d).is_absolute()) {
        return Err(ApiError::defect("relative session list directory"));
    }
    let project = call.query_str("project", None)?;
    let subpath = call.query_str("subpath", None)?;
    let cursor = call.query_str("cursor", None)?;
    let query = match cursor {
        Some(c) => decode_sessions_cursor(&c).ok_or_else(|| ApiError::invalid_cursor("Invalid cursor"))?,
        None => SessionListQuery { order, search, parent_id, archived, directory, project, subpath, anchor: None },
    };
    let sessions = app.backend.session_list(&call.ctx, SessionListRequest { limit, query: query.clone() }).await?;
    let mut base = query;
    base.anchor = None;
    let anchor = |s: &SessionInfo, direction| SessionListAnchor { id: s.id.clone(), time: s.time.updated as i64, direction };
    let previous = sessions.first().map(|s| encode_sessions_cursor(&base, anchor(s, CursorDirection::Previous)));
    let next = sessions.last().map(|s| encode_sessions_cursor(&base, anchor(s, CursorDirection::Next)));
    Ok(respond::bare(&json!({
        "data": sessions,
        "cursor": { "previous": previous, "next": next },
    })))
}

fn encode_message_cursor(id: &str, order: SessionMessageListOrder, direction: CursorDirection) -> String {
    let v = json!({ "id": id, "order": order, "direction": direction });
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&v).unwrap_or_default())
}

fn decode_message_cursor(input: &str) -> Option<(MessageCursor, SessionMessageListOrder)> {
    #[derive(serde::Deserialize)]
    struct Raw {
        id: String,
        order: SessionMessageListOrder,
        direction: CursorDirection,
    }
    let raw: Raw = serde_json::from_slice(&b64url_decode(input)?).ok()?;
    if !raw.id.starts_with("msg_") {
        return None;
    }
    Some((MessageCursor { id: raw.id, direction: raw.direction }, raw.order))
}

async fn session_message_list(app: &App, call: &Call) -> Result<Response, ApiError> {
    let session_id = call.path_str("sessionID", Some("^ses"))?;
    let limit = call.query_int("limit", Some(1), Some(200))?.unwrap_or(50);
    let order_q: Option<SessionMessageListOrder> = call.query_enum("order", r#""asc" | "desc""#)?;
    let cursor = call.query_str("cursor", None)?.filter(|c| !c.is_empty());
    let r#type = call.query_enum(
        "type",
        r#""agent-switched" | "model-switched" | "location-switched" | "user" | "synthetic" | "system" | "skill" | "shell" | "assistant" | "compaction" | "idle""#,
    )?;
    if cursor.is_some() && order_q.is_some() {
        return Err(ApiError::invalid_cursor("Cursor cannot be combined with order"));
    }
    let decoded = match &cursor {
        Some(c) => Some(decode_message_cursor(c).ok_or_else(|| ApiError::invalid_cursor("Invalid cursor"))?),
        None => None,
    };
    let order = decoded.as_ref().map(|d| d.1).or(order_q).unwrap_or(SessionMessageListOrder::Desc);
    let messages = app
        .backend
        .session_message_list(
            &call.ctx,
            SessionMessageListRequest { session_id, limit, order, r#type, cursor: decoded.map(|d| d.0) },
        )
        .await?;
    let id_of = |m: &crate::generated::SessionMessageInfo| -> String {
        serde_json::to_value(m).ok().and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_string)).unwrap_or_default()
    };
    let previous = messages.first().map(|m| encode_message_cursor(&id_of(m), order, CursorDirection::Previous));
    let next = messages.last().map(|m| encode_message_cursor(&id_of(m), order, CursorDirection::Next));
    Ok(respond::bare(&json!({
        "data": messages,
        "cursor": { "previous": previous, "next": next },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_cursor_round_trip_and_key_order() {
        let q = SessionListQuery {
            order: Some(crate::generated::SessionListOrder::Asc),
            parent_id: Some(ParentFilter::Root),
            directory: Some("/d".into()),
            project: Some("p".into()),
            ..Default::default()
        };
        let c = encode_sessions_cursor(&q, SessionListAnchor { id: "ses_1".into(), time: 5, direction: CursorDirection::Next });
        let json = String::from_utf8(URL_SAFE_NO_PAD.decode(&c).unwrap()).unwrap();
        assert_eq!(
            json,
            r#"{"order":"asc","parentID":"null","directory":"/d","anchor":{"id":"ses_1","time":5,"direction":"next"}}"#
        );
        let back = decode_sessions_cursor(&c).unwrap();
        assert_eq!(back.parent_id, Some(ParentFilter::Root));
        assert_eq!(back.anchor.unwrap().time, 5);
        assert!(decode_sessions_cursor("!!").is_none());
        // a cursor without anchor is invalid
        let no_anchor = URL_SAFE_NO_PAD.encode(br#"{"order":"asc"}"#);
        assert!(decode_sessions_cursor(&no_anchor).is_none());
    }

    #[test]
    fn message_cursor_round_trip() {
        let c = encode_message_cursor("msg_1", SessionMessageListOrder::Desc, CursorDirection::Previous);
        assert_eq!(
            String::from_utf8(URL_SAFE_NO_PAD.decode(&c).unwrap()).unwrap(),
            r#"{"id":"msg_1","order":"desc","direction":"previous"}"#
        );
        let (cur, order) = decode_message_cursor(&c).unwrap();
        assert_eq!(cur.id, "msg_1");
        assert_eq!(order, SessionMessageListOrder::Desc);
        let bad = URL_SAFE_NO_PAD.encode(br#"{"id":"x","order":"desc","direction":"next"}"#);
        assert!(decode_message_cursor(&bad).is_none());
    }
}
