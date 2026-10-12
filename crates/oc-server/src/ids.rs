//! Ascending/descending identifiers (spec 02 §3.1), used for the synthetic `server.connected`
//! event id (`evt_` + ascending).

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
static STATE: Mutex<(u64, u64)> = Mutex::new((0, 0));

/// 26-char id body: 12 hex chars of `(ts_ms << 12) + counter` (low 48 bits) + 14 base62 chars.
pub fn create(descending: bool, timestamp_ms: Option<u64>) -> String {
    let ts = timestamp_ms.unwrap_or_else(|| {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
    });
    let counter = {
        let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
        if st.0 != ts {
            *st = (ts, 0);
        }
        st.1 += 1;
        st.1
    };
    body(descending, ts, counter, &mut |n| {
        let mut b = vec![0u8; n];
        rand::fill(&mut b[..]);
        b
    })
}

fn body(descending: bool, ts: u64, counter: u64, random: &mut dyn FnMut(usize) -> Vec<u8>) -> String {
    let mask = 0xFFFF_FFFF_FFFFu64;
    let current = (ts << 12).wrapping_add(counter) & mask;
    let v = if descending { mask - current } else { current };
    let mut out = format!("{v:012x}");
    for b in random(14) {
        out.push(ALPHABET[(b % 62) as usize] as char);
    }
    out
}

/// `evt_` + ascending id.
pub fn event_id() -> String {
    format!("evt_{}", create(false, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        let mut zero = |n| vec![0u8; n];
        assert_eq!(&body(false, 1760000000000, 1, &mut zero)[..12], "9c82cc000001");
        assert_eq!(&body(true, 1760000000000, 1, &mut zero)[..12], "637d33fffffe");
        assert_eq!(body(false, 1, 1, &mut zero).len(), 26);
    }

    #[test]
    fn ascending_event_ids() {
        let a = event_id();
        let b = event_id();
        assert!(a.starts_with("evt_") && a.len() == 30);
        assert!(a[4..16] < b[4..16] || a[4..16] == b[4..16]);
    }
}
