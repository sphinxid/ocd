//! `core/src/mime.ts` plus a `mime-types` lookup.

use std::path::Path;

fn starts_with(bytes: &[u8], prefix: &[u8]) -> bool {
    bytes.len() >= prefix.len() && &bytes[..prefix.len()] == prefix
}

/// UTF-8 validity in streaming mode: an incomplete trailing sequence is allowed.
fn valid_utf8_streaming(bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    }
}

fn is_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    if bytes.contains(&0) {
        return false;
    }
    if !valid_utf8_streaming(bytes) {
        return false;
    }
    let controls = bytes.iter().filter(|&&b| b < 9 || (b > 13 && b < 32)).count();
    controls as f64 / bytes.len() as f64 <= 0.3
}

/// `Mime.detect(bytes)`.
pub fn detect(bytes: &[u8]) -> &'static str {
    if starts_with(bytes, &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) {
        return "image/png";
    }
    if starts_with(bytes, &[0xff, 0xd8, 0xff]) {
        return "image/jpeg";
    }
    if starts_with(bytes, &[0x47, 0x49, 0x46, 0x38]) {
        return "image/gif";
    }
    if starts_with(bytes, &[0x42, 0x4d]) {
        return "image/bmp";
    }
    if starts_with(bytes, &[0x25, 0x50, 0x44, 0x46, 0x2d]) {
        return "application/pdf";
    }
    if starts_with(bytes, &[0x52, 0x49, 0x46, 0x46]) && bytes.len() >= 8 && starts_with(&bytes[8..], &[0x57, 0x45, 0x42, 0x50]) {
        return "image/webp";
    }
    if bytes.len() >= 4
        && starts_with(&bytes[4..], &[0x66, 0x74, 0x79, 0x70])
        && bytes.len() >= 8
        && (starts_with(&bytes[8..], &[0x61, 0x76, 0x69, 0x66]) || starts_with(&bytes[8..], &[0x61, 0x76, 0x69, 0x73]))
    {
        return "image/avif";
    }
    if is_text(bytes) { "text/plain" } else { "application/octet-stream" }
}

/// `lookup(path) || "application/octet-stream"` (npm `mime-types@3.0.2`, the table in `mime_db.rs`).
pub fn lookup(path: &Path) -> String {
    lookup_str(&path.to_string_lossy()).unwrap_or("application/octet-stream").to_string()
}

/// `mime-types.lookup(path)`: the extension is `extname("x." + path).toLowerCase().slice(1)`, so a bare
/// name without a dot (`"Makefile"`) is looked up whole, while dotfiles in a subdirectory have none.
pub fn lookup_str(path: &str) -> Option<&'static str> {
    let prefixed = format!("x.{path}");
    let base = prefixed.rsplit(['/', '\\']).next().unwrap_or(&prefixed);
    let dot = base.rfind('.').filter(|&i| i > 0)?;
    let ext = base[dot + 1..].to_ascii_lowercase();
    if ext.is_empty() {
        return None;
    }
    crate::mime_db::TYPES.binary_search_by(|(k, _)| (*k).cmp(ext.as_str())).ok().map(|i| crate::mime_db::TYPES[i].1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects() {
        assert_eq!(detect(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0]), "image/png");
        assert_eq!(detect(b"%PDF-1.4"), "application/pdf");
        assert_eq!(detect(b"RIFF\0\0\0\0WEBPVP8"), "image/webp");
        assert_eq!(detect(b"\0\0\0\x18ftypavif"), "image/avif");
        assert_eq!(detect(b"hello"), "text/plain");
        assert_eq!(detect(b""), "text/plain");
        assert_eq!(detect(b"a\0b"), "application/octet-stream");
        assert_eq!(detect(&[0xe2, 0x82]), "text/plain");
        assert_eq!(detect(&[0xff, 0xfe, 0x41]), "application/octet-stream");
        assert_eq!(detect(&[1, 2, 3, b'a']), "application/octet-stream");
        assert_eq!(lookup(Path::new("a.unknownext")), "application/octet-stream");
        assert_eq!(lookup(Path::new("a.html")), "text/html");
    }

    #[test]
    fn mime_types_lookup() {
        assert_eq!(lookup(Path::new("/a/b/main.rs")), "application/rls-services+xml");
        assert_eq!(lookup(Path::new("x.ts")), "video/mp2t");
        assert_eq!(lookup(Path::new("app.js")), "text/javascript");
        assert_eq!(lookup(Path::new("README.MD")), "text/markdown");
        assert_eq!(lookup(Path::new("/p/Makefile")), "application/octet-stream");
        assert_eq!(lookup_str("json"), Some("application/json"));
        assert_eq!(lookup_str("/p/.env"), None);
        assert_eq!(lookup(Path::new("a/b.unknownext")), "application/octet-stream");
    }
}
