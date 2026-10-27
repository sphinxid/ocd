//! Pending session state reported to the terminal (`src/context/program-status.ts`, OpenTUI's
//! `setProgramStatus`): OSC 7501 records, one root summary plus one record per pending session
//! (open tabs and the viewed session), sent only when they change.
//!
//! Wire format (as the 2.1.4 build writes it): `ESC ] 7501 ; key=value:… ESC \` with the fields in
//! the order `state`, `id`, `kind`, `app`, `title`, `msg`; `title` and `msg` are base64. The
//! terminal is asked once at startup with `ESC ] 7501 ; ? ESC \`.

use super::App;
use base64::Engine;
use indexmap::IndexMap;

const APP: &str = "opencode";
const ROOT: &str = "";

/// The capability query sent once at startup.
pub const QUERY: &str = "\x1b]7501;?\x1b\\";

#[derive(Clone, Debug, PartialEq)]
pub enum Pending {
    Blocked { id: String, title: Option<String>, kind: String, message: String },
    Working { id: String, title: Option<String> },
}

impl Pending {
    fn blocked(&self) -> bool {
        matches!(self, Pending::Blocked { .. })
    }
}

/// One `ProgramStatus` record.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Record {
    pub state: &'static str,
    pub id: Option<String>,
    pub kind: Option<String>,
    pub app: bool,
    pub title: Option<String>,
    pub message: Option<String>,
}

/// `ESC ] 7501 ; …` for a record.
pub fn encode(r: &Record) -> String {
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
    let mut fields = vec![format!("state={}", r.state)];
    if let Some(id) = &r.id {
        fields.push(format!("id={id}"));
    }
    if let Some(k) = &r.kind {
        fields.push(format!("kind={k}"));
    }
    if r.app {
        fields.push(format!("app={APP}"));
    }
    if let Some(t) = &r.title {
        fields.push(format!("title={}", b64(t)));
    }
    if let Some(m) = &r.message {
        fields.push(format!("msg={}", b64(m)));
    }
    format!("\x1b]7501;{}\x1b\\", fields.join(":"))
}

/// Record IDs must be 1-32 bytes of `[A-Za-z0-9_.+-]`; others are hashed to `ses_<sha256[:16]>`.
pub fn record_id(id: &str) -> String {
    let ok = !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'));
    if ok { id.to_string() } else { format!("ses_{}", &oc_util::hash::sha256(id)[..16]) }
}

/// One line within `bytes`: control characters and whitespace runs collapse to a space; an
/// oversized text keeps whole characters and ends with `…`.
pub fn clip(input: Option<&str>, bytes: usize) -> Option<String> {
    let text = input.unwrap_or("");
    let mut out = String::new();
    let mut space = false;
    for c in text.chars() {
        let blank = c.is_whitespace() || (c as u32) <= 0x1f || (0x7f..=0x9f).contains(&(c as u32));
        if blank {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    if out.is_empty() {
        return None;
    }
    if out.len() <= bytes {
        return Some(out);
    }
    let mut end = bytes.saturating_sub(3);
    while !out.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}…", &out[..end]))
}

fn record(p: &Pending, id: Option<String>) -> Record {
    match p {
        Pending::Working { title, .. } => Record { state: "working", id, app: true, title: clip(title.as_deref(), 192), ..Default::default() },
        Pending::Blocked { title, kind, message, .. } => Record { state: "blocked", id, kind: Some(kind.clone()), app: true, title: clip(title.as_deref(), 192), message: clip(Some(message), 2048) },
    }
}

fn root(sessions: &[Pending]) -> Record {
    let blocked: Vec<&Pending> = sessions.iter().filter(|s| s.blocked()).collect();
    let working: Vec<&Pending> = sessions.iter().filter(|s| !s.blocked()).collect();
    if blocked.len() == 1 {
        return record(blocked[0], None);
    }
    if blocked.len() > 1 {
        let kind = |p: &Pending| match p {
            Pending::Blocked { kind, .. } => kind.clone(),
            _ => String::new(),
        };
        let first = kind(blocked[0]);
        let same = blocked.iter().all(|b| kind(b) == first);
        return Record { state: "blocked", kind: same.then_some(first), app: true, message: Some(format!("{} sessions need input", blocked.len())), ..Default::default() };
    }
    if working.len() == 1 {
        return record(working[0], None);
    }
    if working.len() > 1 {
        return Record { state: "working", app: true, message: Some(format!("{} sessions working", working.len())), ..Default::default() };
    }
    Record { state: "idle", app: true, ..Default::default() }
}

/// `programStatusReports`: the root summary first, then one record per session.
pub fn reports(sessions: &[Pending]) -> IndexMap<String, Record> {
    let mut out = IndexMap::new();
    out.insert(ROOT.to_string(), root(sessions));
    for s in sessions {
        let id = match s {
            Pending::Blocked { id, .. } | Pending::Working { id, .. } => record_id(id),
        };
        out.insert(id.clone(), record(s, Some(id)));
    }
    out
}

/// What was last sent per record.
#[derive(Clone, Debug, Default)]
pub struct ProgramStatus {
    sent: IndexMap<String, String>,
}

impl ProgramStatus {
    /// The sequences that bring the terminal up to date: clears for records that went away, then
    /// every record whose content changed.
    pub fn update(&mut self, sessions: &[Pending]) -> String {
        let reports = reports(sessions);
        let mut out = String::new();
        let stale: Vec<String> = self.sent.keys().filter(|k| k.as_str() != ROOT && !reports.contains_key(*k)).cloned().collect();
        for key in stale {
            out.push_str(&encode(&Record { state: "clear", id: Some(key.clone()), ..Default::default() }));
            self.sent.shift_remove(&key);
        }
        for (key, r) in reports {
            let seq = encode(&r);
            if self.sent.get(&key) != Some(&seq) {
                out.push_str(&seq);
                self.sent.insert(key, seq);
            }
        }
        out
    }

    /// The root clear sent on exit.
    pub fn clear() -> String {
        encode(&Record { state: "clear", ..Default::default() })
    }
}

impl App {
    /// Pending open tabs plus the viewed session (`createProgramStatus` in `session-tabs.tsx`).
    pub fn program_status_sessions(&self) -> Vec<Pending> {
        let viewed = self.session_id().filter(|s| *s != "dummy").map(|s| self.store.root(s));
        let mut ids: Vec<String> = if self.config.tabs_enabled() { self.tabs.clone() } else { vec![] };
        if let Some(v) = viewed
            && !ids.contains(&v)
        {
            ids.push(v);
        }
        ids.into_iter()
            .filter_map(|id| {
                let st = self.tab_status(&id);
                let title = self.store.session(&id).map(|s| s.display_title());
                match st.attention {
                    Some(kind) => {
                        let message = if kind == "permission" { "Permission needs input" } else { "Input needs response" };
                        Some(Pending::Blocked { id, title, kind: kind.to_string(), message: message.to_string() })
                    }
                    None if st.busy => Some(Pending::Working { id, title }),
                    None => None,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn working(id: &str, title: &str) -> Pending {
        Pending::Working { id: id.into(), title: Some(title.into()) }
    }

    #[test]
    fn records_match_the_2_1_4_wire_format() {
        let mut ps = ProgramStatus::default();
        assert_eq!(ps.update(&[]), "\x1b]7501;state=idle:app=opencode\x1b\\");
        assert_eq!(ps.update(&[]), "");
        let id = "ses_e83d34f5affeJQDaYu0ZnGP7X2";
        let out = ps.update(&[working(id, "Smoke test")]);
        assert_eq!(out, format!("\x1b]7501;state=working:app=opencode:title=U21va2UgdGVzdA==\x1b\\\x1b]7501;state=working:id={id}:app=opencode:title=U21va2UgdGVzdA==\x1b\\"));
        let blocked = Pending::Blocked { id: id.into(), title: Some("Smoke test".into()), kind: "permission".into(), message: "Permission needs input".into() };
        let out = ps.update(&[blocked]);
        assert_eq!(
            out,
            format!(
                "\x1b]7501;state=blocked:kind=permission:app=opencode:title=U21va2UgdGVzdA==:msg=UGVybWlzc2lvbiBuZWVkcyBpbnB1dA==\x1b\\\x1b]7501;state=blocked:id={id}:kind=permission:app=opencode:title=U21va2UgdGVzdA==:msg=UGVybWlzc2lvbiBuZWVkcyBpbnB1dA==\x1b\\"
            )
        );
        assert_eq!(ps.update(&[]), format!("\x1b]7501;state=clear:id={id}\x1b\\\x1b]7501;state=idle:app=opencode\x1b\\"));
        assert_eq!(ProgramStatus::clear(), "\x1b]7501;state=clear\x1b\\");
    }

    #[test]
    fn summaries_ids_and_clipping() {
        let r = reports(&[working("a", "A"), working("b", "B")]);
        assert_eq!(encode(&r[""]), format!("\x1b]7501;state=working:app=opencode:msg={}\x1b\\", base64::engine::general_purpose::STANDARD.encode("2 sessions working")));
        let blocked = |id: &str, kind: &str| Pending::Blocked { id: id.into(), title: None, kind: kind.into(), message: "m".into() };
        let r = reports(&[blocked("a", "question"), blocked("b", "permission")]);
        assert_eq!(r[""].kind, None);
        assert_eq!(r[""].message.as_deref(), Some("2 sessions need input"));
        assert_eq!(record_id("ses_ok"), "ses_ok");
        let long = "x".repeat(40);
        assert_eq!(record_id(&long), format!("ses_{}", &oc_util::hash::sha256(&long)[..16]));
        assert_eq!(clip(Some("  a\n\tb\u{7}c  "), 192).as_deref(), Some("a b c"));
        assert_eq!(clip(Some("   "), 192), None);
        let clipped = clip(Some(&"é".repeat(200)), 192).unwrap();
        assert!(clipped.len() <= 192 && clipped.ends_with('…'));
    }
}
