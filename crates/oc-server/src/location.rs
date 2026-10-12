//! Location selection (`requestRef`, spec 06 §3).

use axum::http::HeaderMap;

use crate::context::LocationRef;

/// Header carrying the location directory (URI-encoded).
pub const DIRECTORY_HEADER: &str = "x-opencode-directory";

/// `?location[directory]` (if non-empty) → `x-opencode-directory` (decodeURIComponent, raw on
/// failure) → server cwd. The string is used verbatim (no normalisation).
pub fn request_ref(query: &[(String, String)], headers: &HeaderMap, cwd: &str) -> LocationRef {
    if let Some((_, v)) = query.iter().find(|(k, _)| k == "location[directory]")
        && !v.is_empty()
    {
        return LocationRef::new(v.clone());
    }
    if let Some(h) = headers.get(DIRECTORY_HEADER) {
        let raw = String::from_utf8_lossy(h.as_bytes()).into_owned();
        if !raw.is_empty() {
            return LocationRef::new(decode_uri_component(&raw).unwrap_or(raw));
        }
    }
    LocationRef::new(cwd)
}

/// JS `decodeURIComponent`: `None` on malformed escapes or invalid UTF-8.
pub fn decode_uri_component(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = input.get(i + 1..i + 3)?;
            if !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            let b = u8::from_str_radix(hex, 16).ok()?;
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn precedence() {
        let mut h = HeaderMap::new();
        h.insert(DIRECTORY_HEADER, HeaderValue::from_static("%2Fh%20dir"));
        assert_eq!(request_ref(&q(&[("location[directory]", "/q")]), &h, "/cwd").directory, "/q");
        assert_eq!(request_ref(&q(&[("location[directory]", "")]), &h, "/cwd").directory, "/h dir");
        assert_eq!(request_ref(&[], &HeaderMap::new(), "/cwd").directory, "/cwd");
        let mut bad = HeaderMap::new();
        bad.insert(DIRECTORY_HEADER, HeaderValue::from_static("/a%zz"));
        assert_eq!(request_ref(&[], &bad, "/cwd").directory, "/a%zz");
    }

    #[test]
    fn decode() {
        assert_eq!(decode_uri_component("a%2Fb").as_deref(), Some("a/b"));
        assert_eq!(decode_uri_component("%E2%82%AC").as_deref(), Some("€"));
        assert_eq!(decode_uri_component("%"), None);
        assert_eq!(decode_uri_component("%FF"), None);
    }
}
