pub mod locale;
pub mod jsonc_edit;
pub mod paths;
pub mod system;

use parking_lot::Mutex;

const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const MASK: u64 = 0xFFFF_FFFF_FFFF;
static ID_STATE: Mutex<(u64, u64)> = Mutex::new((0, 0));

/// Identifier body (`schema/src/identifier.ts`): 12 hex + 14 base62.
pub fn id_body(descending: bool) -> String {
    let ts = locale::now_ms() as u64;
    let counter = {
        let mut s = ID_STATE.lock();
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

pub fn session_id() -> String {
    format!("ses_{}", id_body(true))
}

pub fn message_id() -> String {
    format!("msg_{}", id_body(false))
}

#[cfg(test)]
mod tests {
    #[test]
    fn ids() {
        let a = super::message_id();
        let b = super::message_id();
        assert!(a.starts_with("msg_") && a.len() == 30);
        assert!(a < b);
        let s1 = super::session_id();
        let s2 = super::session_id();
        assert!(s1 > s2);
    }
}
