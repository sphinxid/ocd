//! Authentication (spec 06 §4): Basic / `auth_token` / session cookie, pairing-code session
//! tokens, and the pairing / PTY-ticket caches.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::error::json_response;

/// Fixed username.
pub const USERNAME: &str = "opencode";
/// Session token lifetime (30 days).
pub const SESSION_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;
/// `www-authenticate` challenge.
pub const WWW_AUTHENTICATE: &str = "Basic realm=\"Secure Area\"";
const UNAUTHORIZED_MESSAGE: &str = "Authentication required";

/// Server credentials. Auth is required iff the password is non-empty.
#[derive(Debug, Clone, Default)]
pub struct Auth {
    pub password: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Credentials {
    pub username: String,
    pub password: String,
}

fn empty() -> Credentials {
    Credentials { username: String::new(), password: String::new() }
}

fn decode_b64(input: &str) -> Option<Vec<u8>> {
    let t = input.trim();
    STANDARD
        .decode(t)
        .or_else(|_| STANDARD_NO_PAD.decode(t))
        .or_else(|_| URL_SAFE.decode(t))
        .or_else(|_| URL_SAFE_NO_PAD.decode(t))
        .ok()
}

pub(crate) fn decode_credential(input: &str) -> Credentials {
    let Some(bytes) = decode_b64(input) else { return empty() };
    let Ok(text) = String::from_utf8(bytes) else { return empty() };
    match text.find(':') {
        Some(i) => Credentials { username: text[..i].to_string(), password: text[i + 1..].to_string() },
        None => empty(),
    }
}

/// `auth_token` query (precedence) or `Authorization: Basic ...`.
pub(crate) fn credential_from_request(query: &[(String, String)], headers: &HeaderMap) -> Credentials {
    if let Some((_, token)) = query.iter().find(|(k, _)| k == "auth_token")
        && !token.is_empty()
    {
        return decode_credential(token);
    }
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("");
    if auth.len() > 5 && auth[..5].eq_ignore_ascii_case("basic") {
        let rest = &auth[5..];
        if rest.starts_with(|c: char| c.is_whitespace()) {
            let value = rest.trim_start();
            if !value.is_empty() {
                return decode_credential(value);
            }
        }
    }
    empty()
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn sign(password: &str, payload: &str) -> String {
    let mut k = Hmac::<Sha256>::new_from_slice(password.as_bytes()).expect("hmac key");
    k.update(b"opencode-session-v1");
    let key = k.finalize().into_bytes();
    let mut m = Hmac::<Sha256>::new_from_slice(&key).expect("hmac key");
    m.update(payload.as_bytes());
    URL_SAFE_NO_PAD.encode(m.finalize().into_bytes())
}

impl Auth {
    pub fn new(password: Option<String>) -> Self {
        Self { password }
    }

    /// Whether credentials are required.
    pub fn required(&self) -> bool {
        self.password.as_deref().is_some_and(|p| !p.is_empty())
    }

    pub(crate) fn authorized(&self, c: &Credentials) -> bool {
        let Some(pw) = &self.password else { return false };
        if c.username != USERNAME {
            return false;
        }
        bool::from(c.password.as_bytes().ct_eq(pw.as_bytes())) || self.verify_session(&c.password)
    }

    /// Issue a pairing session token `<expires>.<sig>` (valid 30 days).
    pub fn issue_session(&self) -> Option<String> {
        self.issue_session_at(now_ms())
    }

    pub(crate) fn issue_session_at(&self, now: u64) -> Option<String> {
        let pw = self.password.as_ref()?;
        let expires = (now / 1000 + SESSION_TTL_SECONDS).to_string();
        let sig = sign(pw, &expires);
        Some(format!("{expires}.{sig}"))
    }

    /// Verify a session token against the current password.
    pub fn verify_session(&self, token: &str) -> bool {
        self.verify_session_at(token, now_ms())
    }

    pub(crate) fn verify_session_at(&self, token: &str, now: u64) -> bool {
        let Some(pw) = &self.password else { return false };
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 2 {
            return false;
        }
        // Number(parts[0]) must be a safe integer.
        let Some(expires) = crate::router::parse_js_number(parts[0]) else { return false };
        if parts[0].trim().is_empty() || expires.fract() != 0.0 || expires.abs() > 9_007_199_254_740_991.0 {
            return false;
        }
        if expires * 1000.0 <= now as f64 {
            return false;
        }
        let expected = sign(pw, parts[0]);
        expected.len() == parts[1].len() && bool::from(expected.as_bytes().ct_eq(parts[1].as_bytes()))
    }

    /// Full request check: credentials or same-origin session cookie. Always true when no
    /// password is configured.
    pub(crate) fn authorized_request(&self, query: &[(String, String)], headers: &HeaderMap) -> bool {
        if !self.required() {
            return true;
        }
        self.authorized(&credential_from_request(query, headers)) || self.authorized_cookie(headers)
    }

    fn authorized_cookie(&self, headers: &HeaderMap) -> bool {
        let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
        let name = session_cookie_name(host);
        let Some(token) = cookie(headers, &name) else { return false };
        if token.is_empty() {
            return false;
        }
        if let Some(origin) = headers.get(header::ORIGIN) {
            let origin = origin.to_str().unwrap_or("");
            if url_host(origin).as_deref() != host {
                return false;
            }
        }
        self.verify_session(&token)
    }
}

/// `URL(origin).host` (host plus non-default port).
pub(crate) fn url_host(input: &str) -> Option<String> {
    let u = url::Url::parse(input).ok()?;
    let host = u.host_str()?;
    if host.is_empty() {
        return None;
    }
    Some(match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    })
}

/// `opencode_session_<port>`, or `opencode_session` when Host has no (non-default) port.
pub fn session_cookie_name(host: Option<&str>) -> String {
    let port = url::Url::parse(&format!("http://{}", host.unwrap_or(""))).ok().and_then(|u| u.port());
    match port {
        Some(p) => format!("opencode_session_{p}"),
        None => "opencode_session".to_string(),
    }
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    for value in headers.get_all(header::COOKIE) {
        let Ok(s) = value.to_str() else { continue };
        for pair in s.split(';') {
            if let Some((k, v)) = pair.split_once('=')
                && k.trim() == name
            {
                let v = v.trim().trim_matches('"');
                return Some(crate::location::decode_uri_component(v).unwrap_or_else(|| v.to_string()));
            }
        }
    }
    None
}

/// Whether a browser would show a native prompt: only when `sec-fetch-mode` is absent or
/// `navigate`.
pub(crate) fn challenge(headers: &HeaderMap) -> bool {
    match headers.get("sec-fetch-mode") {
        None => true,
        Some(v) => v.as_bytes() == b"navigate",
    }
}

/// `401 {"_tag":"UnauthorizedError","message":"Authentication required"}`.
pub(crate) fn unauthorized_response(headers: &HeaderMap) -> Response {
    let mut res = json_response(
        StatusCode::UNAUTHORIZED,
        &serde_json::json!({ "_tag": "UnauthorizedError", "message": UNAUTHORIZED_MESSAGE }),
    );
    if challenge(headers) {
        res.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static(WWW_AUTHENTICATE));
    }
    res
}

/// A small TTL cache with bounded capacity (oldest entries evicted first).
#[derive(Debug)]
pub(crate) struct TtlCache<V> {
    entries: HashMap<String, (Instant, V)>,
    order: std::collections::VecDeque<String>,
    capacity: usize,
    ttl: Duration,
}

impl<V> TtlCache<V> {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self { entries: HashMap::new(), order: Default::default(), capacity, ttl }
    }

    fn purge(&mut self) {
        let now = Instant::now();
        self.entries.retain(|_, (exp, _)| *exp > now);
        let entries = &self.entries;
        self.order.retain(|k| entries.contains_key(k));
    }

    pub fn insert(&mut self, key: String, value: V) {
        self.purge();
        while self.entries.len() >= self.capacity {
            match self.order.pop_front() {
                Some(old) => {
                    self.entries.remove(&old);
                }
                None => break,
            }
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, (Instant::now() + self.ttl, value));
    }

    /// Remove and return a live entry (single use).
    pub fn take(&mut self, key: &str) -> Option<V> {
        self.purge();
        let (_, v) = self.entries.remove(key)?;
        self.order.retain(|k| k != key);
        Some(v)
    }
}

/// Pairing codes: 16 random bytes base64url, single-use, 5 min, capacity 1000.
pub(crate) struct Pairing {
    cache: parking_lot::Mutex<TtlCache<()>>,
}

pub(crate) const PAIRING_TTL_SECONDS: i64 = 300;

impl Pairing {
    pub fn new() -> Self {
        Self { cache: parking_lot::Mutex::new(TtlCache::new(1_000, Duration::from_secs(PAIRING_TTL_SECONDS as u64))) }
    }

    pub fn issue(&self) -> String {
        let mut bytes = [0u8; 16];
        rand::fill(&mut bytes);
        let code = URL_SAFE_NO_PAD.encode(bytes);
        self.cache.lock().insert(code.clone(), ());
        code
    }

    pub fn consume(&self, code: &str) -> bool {
        self.cache.lock().take(code).is_some()
    }
}

/// Scope stored with a PTY ticket; consumption must match exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TicketScope {
    pub pty_id: String,
    /// `None` for persistent PTYs.
    pub directory: Option<String>,
    pub workspace_id: Option<String>,
}

/// PTY WebSocket tickets: UUID v4, single use, 60 s, capacity 10 000.
pub(crate) struct Tickets {
    cache: parking_lot::Mutex<TtlCache<TicketScope>>,
}

pub(crate) const TICKET_TTL_SECONDS: i64 = 60;

impl Tickets {
    pub fn new() -> Self {
        Self { cache: parking_lot::Mutex::new(TtlCache::new(10_000, Duration::from_secs(TICKET_TTL_SECONDS as u64))) }
    }

    pub fn issue(&self, scope: TicketScope) -> String {
        let ticket = uuid::Uuid::new_v4().to_string();
        self.cache.lock().insert(ticket.clone(), scope);
        ticket
    }

    pub fn consume(&self, ticket: &str, scope: &TicketScope) -> bool {
        let mut cache = self.cache.lock();
        match cache.take(ticket) {
            Some(stored) => stored == *scope,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> Auth {
        Auth::new(Some("secret".into()))
    }

    #[test]
    fn basic_and_query_credentials() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_str(&format!("basic   {}", STANDARD.encode("opencode:secret"))).unwrap());
        assert!(auth().authorized_request(&[], &h));
        let q = vec![("auth_token".to_string(), STANDARD.encode("opencode:wrong"))];
        // query takes precedence over the header
        assert!(!auth().authorized_request(&q, &h));
        let q = vec![("auth_token".to_string(), STANDARD.encode("opencode:secret"))];
        assert!(auth().authorized_request(&q, &HeaderMap::new()));
        let q = vec![("auth_token".to_string(), STANDARD.encode("other:secret"))];
        assert!(!auth().authorized_request(&q, &HeaderMap::new()));
        assert!(!auth().authorized_request(&[], &HeaderMap::new()));
        assert!(Auth::new(None).authorized_request(&[], &HeaderMap::new()));
    }

    #[test]
    fn session_tokens() {
        let a = auth();
        let now = 1_760_000_000_000;
        let t = a.issue_session_at(now).unwrap();
        assert!(t.starts_with(&format!("{}.", now / 1000 + SESSION_TTL_SECONDS)));
        assert!(a.verify_session_at(&t, now));
        assert!(!a.verify_session_at(&t, (now / 1000 + SESSION_TTL_SECONDS) * 1000));
        assert!(!Auth::new(Some("rotated".into())).verify_session_at(&t, now));
        assert!(!a.verify_session_at("abc", now));
        assert!(!a.verify_session_at("1.2.3", now));
        // a token works as the Basic password
        assert!(a.authorized(&Credentials { username: USERNAME.into(), password: a.issue_session().unwrap() }));
    }

    #[test]
    fn cookie_name_and_same_origin() {
        assert_eq!(session_cookie_name(Some("127.0.0.1:4096")), "opencode_session_4096");
        assert_eq!(session_cookie_name(Some("localhost")), "opencode_session");
        assert_eq!(session_cookie_name(None), "opencode_session");
        let a = auth();
        let token = a.issue_session().unwrap();
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("127.0.0.1:4096"));
        h.insert(header::COOKIE, HeaderValue::from_str(&format!("x=1; opencode_session_4096={token}")).unwrap());
        assert!(a.authorized_request(&[], &h));
        h.insert(header::ORIGIN, HeaderValue::from_static("http://127.0.0.1:4096"));
        assert!(a.authorized_request(&[], &h));
        h.insert(header::ORIGIN, HeaderValue::from_static("http://127.0.0.1:5000"));
        assert!(!a.authorized_request(&[], &h));
    }

    #[test]
    fn challenge_only_for_navigation() {
        let mut h = HeaderMap::new();
        assert!(challenge(&h));
        h.insert("sec-fetch-mode", HeaderValue::from_static("navigate"));
        assert!(challenge(&h));
        h.insert("sec-fetch-mode", HeaderValue::from_static("cors"));
        assert!(!challenge(&h));
    }

    #[test]
    fn pairing_codes_are_single_use() {
        let p = Pairing::new();
        let code = p.issue();
        assert_eq!(code.len(), 22);
        assert!(p.consume(&code));
        assert!(!p.consume(&code));
    }

    #[test]
    fn tickets_match_scope() {
        let t = Tickets::new();
        let scope = TicketScope { pty_id: "pty_1".into(), directory: Some("/d".into()), workspace_id: None };
        let k = t.issue(scope.clone());
        let other = TicketScope { directory: Some("/e".into()), ..scope.clone() };
        assert!(!t.consume(&k, &other));
        // consumed even on mismatch (single use)
        assert!(!t.consume(&k, &scope));
        let k = t.issue(scope.clone());
        assert!(t.consume(&k, &scope));
    }

    #[test]
    fn ttl_cache_capacity() {
        let mut c = TtlCache::new(2, Duration::from_secs(60));
        c.insert("a".into(), 1);
        c.insert("b".into(), 2);
        c.insert("c".into(), 3);
        assert!(c.take("a").is_none());
        assert_eq!(c.take("c"), Some(3));
    }
}
