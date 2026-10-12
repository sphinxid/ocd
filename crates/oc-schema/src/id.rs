//! Identifier generation (`schema/src/identifier.ts`), bit-exact with the original,
//! including the 48-bit truncation.

use parking_lot::Mutex;

const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const MASK: u64 = 0xFFFF_FFFF_FFFF;

static STATE: Mutex<(u64, u64)> = Mutex::new((0, 0));

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 26-char body: 12 lowercase hex + 14 base62.
pub fn create(descending: bool, timestamp: Option<u64>) -> String {
    let ts = timestamp.unwrap_or_else(now_ms);
    let counter = {
        let mut s = STATE.lock();
        if s.0 != ts {
            *s = (ts, 0);
        }
        s.1 += 1;
        s.1
    };
    let current = (ts << 12).wrapping_add(counter) & MASK;
    let value = if descending { MASK - current } else { current };
    let mut out = format!("{value:012x}");
    let mut bytes = [0u8; 14];
    rand::fill(&mut bytes);
    out.extend(bytes.iter().map(|b| CHARS[(*b % 62) as usize] as char));
    out
}

pub fn ascending(prefix: &str) -> String {
    format!("{prefix}_{}", create(false, None))
}

pub fn descending(prefix: &str) -> String {
    format!("{prefix}_{}", create(true, None))
}

pub fn session() -> String {
    descending("ses")
}
pub fn message() -> String {
    ascending("msg")
}
pub fn event() -> String {
    ascending("evt")
}
pub fn workspace() -> String {
    ascending("wrk")
}
pub fn permission() -> String {
    ascending("per")
}
pub fn permission_saved() -> String {
    ascending("psv")
}
pub fn credential() -> String {
    ascending("cred")
}
pub fn connection() -> String {
    ascending("con")
}
pub fn shell() -> String {
    ascending("sh")
}
pub fn pty() -> String {
    ascending("pty")
}
pub fn form() -> String {
    ascending("frm")
}
pub fn tool_output() -> String {
    ascending("tool")
}

/// `SessionMessage.ID.fromEvent`: `evt_` → `msg_`.
pub fn message_from_event(event_id: &str) -> String {
    match event_id.strip_prefix("evt_") {
        Some(rest) => format!("msg_{rest}"),
        None => event_id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_reference_values() {
        // Verified against the TS implementation: ts=1760000000000, counter=1.
        let a = create(false, Some(1_760_000_000_000));
        assert_eq!(&a[..12], "9c82cc000001");
        let d = create(true, Some(1_760_000_000_001));
        // counter resets on a new timestamp
        assert_eq!(&d[..12], format!("{:012x}", MASK - (((1_760_000_000_001u64) << 12) + 1 & MASK)));
        assert_eq!(a.len(), 26);
    }
}
