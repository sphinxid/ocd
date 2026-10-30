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
    if let Some(mime) = iso_bmff(bytes) {
        return mime;
    }
    if is_text(bytes) { "text/plain" } else { "application/octet-stream" }
}

/// An `ftyp` box: AVIF, HEIC, QuickTime and M4A share MP4's container; the major brand tells them apart, and
/// generic HEIF brands list `avif`/`avis` among the compatible brands that follow the minor version. (The same
/// rule as `oc_ai::media::iso_bmff_media_type`.)
fn iso_bmff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return None;
    }
    let brand = &bytes[8..12];
    let box_end = (u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize).min(bytes.len());
    let compatible: Vec<&[u8]> =
        if box_end > 16 { (0..(box_end - 16) / 4).map(|i| &bytes[16 + i * 4..20 + i * 4]).collect() } else { vec![] };
    let generic = brand == b"mif1" || brand == b"msf1";
    if brand == b"avif" || brand == b"avis" || (generic && compatible.iter().any(|b| *b == b"avif" || *b == b"avis")) {
        return Some("image/avif");
    }
    if generic || [b"heic", b"heix", b"hevc", b"hevx"].iter().any(|b| brand == *b) {
        return Some("image/heic");
    }
    Some(match brand {
        b"qt  " => "video/quicktime",
        b"M4A " => "audio/mp4",
        _ => "video/mp4",
    })
}

/// `Mime.isTextual`: text the model can read as-is (shared by webfetch and MCP results).
pub fn is_textual(mime: &str) -> bool {
    mime.is_empty()
        || mime.starts_with("text/")
        || mime == "application/json"
        || mime.ends_with("+json")
        || mime == "application/xml"
        || mime.ends_with("+xml")
        || mime == "application/javascript"
        || mime == "application/x-javascript"
}

/// `mime-types.extension(type)`: the first extension mime-db lists for the type (parameters and case
/// ignored), or `None` when it lists none.
pub fn extension(mime: &str) -> Option<&'static str> {
    let trimmed = mime.trim_start();
    let end = trimmed.find(|c: char| c == ';' || c.is_whitespace()).unwrap_or(trimmed.len());
    let key = trimmed[..end].to_lowercase();
    crate::mime_db::EXTENSIONS.binary_search_by(|(k, _)| (*k).cmp(key.as_str())).ok().map(|i| crate::mime_db::EXTENSIONS[i].1)
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

    // core test/mime.test.ts "tells ISO-BMFF brands apart"
    #[test]
    fn iso_bmff_brands() {
        let ftyp = |hex: &str| (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect::<Vec<u8>>();
        for (hex, mime) in [
            ("000000206674797061766966000000004d695072617669666d6961666d696631", "image/avif"),
            ("000000186674797061766973000000006d73663161766973", "image/avif"),
            ("00000018667479706d69663100000000617669666d69663100000000", "image/avif"),
            ("000000246674797068656963000000006d6966314d6950726d6961664d694842", "image/heic"),
            ("00000018667479706d69663100000000686569636d69663100000000", "image/heic"),
            ("000000206674797069736f6d0000020069736f6d69736f32617663316d703431", "video/mp4"),
            ("00000014667479707174202000000200717420200000000877696465", "video/quicktime"),
            ("0000001c667479704d344120000002004d34412069736f6d69736f3200000008", "audio/mp4"),
        ] {
            assert_eq!(detect(&ftyp(hex)), mime, "{hex}");
        }
    }

    #[test]
    fn textual_and_extensions() {
        for mime in ["", "text/markdown", "application/json", "application/ld+json", "application/xml", "image/svg+xml", "application/javascript", "application/x-javascript"] {
            assert!(is_textual(mime), "{mime}");
        }
        for mime in ["application/pdf", "image/png", "application/octet-stream", "application/vnd.apache.parquet"] {
            assert!(!is_textual(mime), "{mime}");
        }
        assert_eq!(extension("text/markdown"), Some("md"));
        assert_eq!(extension("audio/mpeg"), Some("mpga"));
        assert_eq!(extension("Application/Octet-Stream; x=1"), Some("bin"));
        assert_eq!(extension("application/vnd.apache.parquet"), None);
        assert_eq!(extension("x/unknown"), None);
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
