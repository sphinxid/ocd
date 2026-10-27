//! Bridge wire format (`@opentunnel/protocol/bridge-protocol`): JSON control messages in WebSocket text frames,
//! connection data in binary frames (`u32` big-endian connection id, then the payload).

use serde_json::{Value, json};

/// The WebSocket subprotocol.
pub const WEBSOCKET_SUBPROTOCOL: &str = "opentunnel";
/// The client version sent on attach (the SDK's bridge implementation version, not its package version).
pub const CLIENT_VERSION: &str = "0.1.0";
/// Connections the client accepts at once (advertised on attach).
pub const MAX_CONNS: usize = 256;

pub mod code {
    pub const BAD_ATTACH: &str = "bad_attach";
    pub const BAD_TOKEN: &str = "bad_token";
    pub const TUNNEL_NOT_BOUND: &str = "tunnel_not_bound";
    pub const CERT_NOT_READY: &str = "cert_not_ready";
    pub const REPLACED: &str = "replaced";
    pub const UPSTREAM_CONNECT_FAILED: &str = "upstream_connect_failed";
    pub const UPSTREAM_IO_ERROR: &str = "upstream_io_error";
    pub const CLIENT_IO_ERROR: &str = "client_io_error";
    pub const IDLE_TIMEOUT: &str = "idle_timeout";
    pub const ROUTE_CONFLICT: &str = "route_conflict";
    pub const INVALID_ROUTE: &str = "invalid_route";
    pub const UNKNOWN_ROUTE: &str = "unknown_route";
    pub const TOO_MANY_CONNECTIONS: &str = "too_many_connections";
}

pub mod timing {
    pub const HEARTBEAT_MS: u64 = 15_000;
    pub const IDLE_TIMEOUT_MS: u64 = 45_000;
    pub const CONNECT_TIMEOUT_MS: u64 = 10_000;
    pub const RECONNECT_BACKOFF_MIN_MS: u64 = 250;
    pub const RECONNECT_BACKOFF_MAX_MS: u64 = 30_000;
}

pub const CONN_ID_SIZE: usize = 4;
/// Recommended maximum payload per data frame (32 KiB).
pub const MAX_PAYLOAD_SIZE: usize = 32_768;

/// A decoded server control message. Unknown or invalid messages decode to `None`.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    Attached { session: String, routes: Vec<String>, heartbeat_ms: f64, idle_timeout_ms: f64 },
    AttachError { code: String },
    Ping { time_sent: Value },
    Pong { time_sent: Value },
    Drain { reason: String },
    Open { conn: u32, peer: String, sni: String, alpn: String },
    End { conn: u32 },
    Reset { conn: u32, code: String },
}

fn string(v: &Value, key: &str) -> Option<String> {
    v.get(key)?.as_str().map(String::from)
}

fn number(v: &Value, key: &str) -> Option<f64> {
    v.get(key)?.as_f64()
}

fn conn(v: &Value) -> Option<u32> {
    let n = v.get("conn")?.as_f64()?;
    (n.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(&n)).then_some(n as u32)
}

/// `decodeServerMessage`
pub fn decode_server_message(text: &str) -> Option<ServerMessage> {
    let v: Value = serde_json::from_str(text).ok()?;
    if !v.is_object() {
        return None;
    }
    Some(match v.get("type")?.as_str()? {
        "attached" => {
            let routes =
                v.get("routes")?.as_array()?.iter().map(|r| r.as_str().map(String::from)).collect::<Option<_>>()?;
            ServerMessage::Attached {
                session: string(&v, "session")?,
                routes,
                heartbeat_ms: number(&v, "heartbeat_ms")?,
                idle_timeout_ms: number(&v, "idle_timeout_ms")?,
            }
        }
        "attach_error" => ServerMessage::AttachError { code: string(&v, "code")? },
        "ping" => {
            number(&v, "time_sent")?;
            ServerMessage::Ping { time_sent: v["time_sent"].clone() }
        }
        "pong" => {
            number(&v, "time_sent")?;
            ServerMessage::Pong { time_sent: v["time_sent"].clone() }
        }
        "drain" => ServerMessage::Drain { reason: string(&v, "reason")? },
        "open" => ServerMessage::Open {
            conn: conn(&v)?,
            peer: string(&v, "peer")?,
            sni: string(&v, "sni")?,
            alpn: string(&v, "alpn")?,
        },
        "end" => ServerMessage::End { conn: conn(&v)? },
        "reset" => ServerMessage::Reset { conn: conn(&v)?, code: string(&v, "code")? },
        _ => return None,
    })
}

/// A number as `JSON.stringify` writes it (integral doubles without a fraction).
fn js_number(value: &Value) -> Value {
    match value.as_f64() {
        Some(f) if !value.is_i64() && !value.is_u64() && f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 => {
            json!(f as i64)
        }
        _ => value.clone(),
    }
}

pub fn attach(token: &str, routes: &[String]) -> String {
    json!({
        "type": "attach",
        "token": token,
        "transport": "ws",
        "routes": routes,
        "client": { "version": CLIENT_VERSION, "max_conns": MAX_CONNS },
    })
    .to_string()
}

pub fn ping(time_sent: i64) -> String {
    json!({ "type": "ping", "time_sent": time_sent }).to_string()
}

pub fn pong(time_sent: &Value) -> String {
    json!({ "type": "pong", "time_sent": js_number(time_sent) }).to_string()
}

pub fn end(conn: u32) -> String {
    json!({ "type": "end", "conn": conn }).to_string()
}

pub fn reset(conn: u32, code: &str) -> String {
    json!({ "type": "reset", "conn": conn, "code": code }).to_string()
}

/// `parseDataFrame`
pub fn parse_data_frame(data: &[u8]) -> Option<(u32, &[u8])> {
    if data.len() < CONN_ID_SIZE {
        return None;
    }
    let conn = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    Some((conn, &data[CONN_ID_SIZE..]))
}

/// `buildDataFrame`
pub fn build_data_frame(conn: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(CONN_ID_SIZE + payload.len());
    out.extend_from_slice(&conn.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_messages_keep_the_sdk_key_order() {
        assert_eq!(
            attach("tok", &["a".into(), "b".into()]),
            r#"{"type":"attach","token":"tok","transport":"ws","routes":["a","b"],"client":{"version":"0.1.0","max_conns":256}}"#
        );
        assert_eq!(ping(5), r#"{"type":"ping","time_sent":5}"#);
        assert_eq!(pong(&json!(1e3)), r#"{"type":"pong","time_sent":1000}"#);
        assert_eq!(pong(&json!(1.5)), r#"{"type":"pong","time_sent":1.5}"#);
        assert_eq!(end(7), r#"{"type":"end","conn":7}"#);
        assert_eq!(reset(7, code::UNKNOWN_ROUTE), r#"{"type":"reset","conn":7,"code":"unknown_route"}"#);
    }

    #[test]
    fn server_messages_are_validated() {
        assert_eq!(
            decode_server_message(
                r#"{"type":"open","conn":4294967295,"peer":"1.2.3.4:5","sni":"a.b","alpn":"","x":1}"#
            ),
            Some(ServerMessage::Open {
                conn: u32::MAX,
                peer: "1.2.3.4:5".into(),
                sni: "a.b".into(),
                alpn: String::new()
            })
        );
        assert_eq!(decode_server_message(r#"{"type":"open","conn":1,"peer":"p","sni":"s"}"#), None);
        assert_eq!(decode_server_message(r#"{"type":"end","conn":-1}"#), None);
        assert_eq!(decode_server_message(r#"{"type":"end","conn":1.5}"#), None);
        assert_eq!(decode_server_message(r#"{"type":"end","conn":2.0}"#), Some(ServerMessage::End { conn: 2 }));
        assert_eq!(decode_server_message(r#"{"type":"ping","time_sent":"x"}"#), None);
        assert_eq!(decode_server_message("not json"), None);
        assert_eq!(decode_server_message(r#"{"type":"nope"}"#), None);
        assert!(matches!(
            decode_server_message(
                r#"{"type":"attached","session":"s","routes":["r"],"heartbeat_ms":15000,"idle_timeout_ms":45000}"#
            ),
            Some(ServerMessage::Attached { .. })
        ));
    }

    #[test]
    fn data_frames() {
        let frame = build_data_frame(0x01020304, b"hi");
        assert_eq!(frame, vec![1, 2, 3, 4, b'h', b'i']);
        assert_eq!(parse_data_frame(&frame), Some((0x01020304, &b"hi"[..])));
        assert_eq!(parse_data_frame(&[1, 2, 3]), None);
        assert_eq!(parse_data_frame(&[0, 0, 0, 9]), Some((9, &b""[..])));
    }
}
