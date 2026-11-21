//! Image sources and decoding (OpenTUI `NativeImage.load` / `image.zig` `decode`).
//!
//! An `<image>` source is a URI (`data:`, a file path or `file:` URL, `http(s):`) or bytes. Decoding gives
//! straight-alpha RGBA8 pixels; a PNG also keeps its encoded bytes (cut at `IEND`) so Kitty can send them as they
//! are (`f=100`), like OpenTUI's `encoded_png`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use base64::Engine as _;

/// A decoded image.
#[derive(Debug)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    /// Straight-alpha RGBA8, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
    /// Any pixel with alpha below 255 (`pixelsHaveTransparency`).
    pub has_alpha: bool,
    /// The PNG as sent (`encoded_png`), when the source was a PNG shown in its stored orientation.
    pub png: Option<Vec<u8>>,
    /// A process-unique id: equal handles are the same pixels (OpenTUI's image handle).
    pub handle: u32,
}

static HANDLES: AtomicU32 = AtomicU32::new(1);

impl Picture {
    pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Picture {
        let has_alpha = rgba.chunks_exact(4).any(|p| p[3] != 255);
        Picture { width, height, rgba, has_alpha, png: None, handle: HANDLES.fetch_add(1, Ordering::Relaxed) }
    }

    /// The pixel at `(x, y)` as `[r, g, b, a]`.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let o = (y as usize * self.width as usize + x as usize) * 4;
        [self.rgba[o], self.rgba[o + 1], self.rgba[o + 2], self.rgba[o + 3]]
    }

    /// `extract`: a copy of a source rectangle.
    pub fn extract(&self, x: u32, y: u32, w: u32, h: u32) -> Picture {
        let mut out = Vec::with_capacity(w as usize * h as usize * 4);
        for row in y..y + h {
            let start = (row as usize * self.width as usize + x as usize) * 4;
            out.extend_from_slice(&self.rgba[start..start + w as usize * 4]);
        }
        Picture::from_rgba(w, h, out)
    }
}

/// Where an image's bytes come from.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Bytes(Vec<u8>),
    /// A `media:<hash>` ref, read from the server.
    Media(String),
    File(std::path::PathBuf),
    Remote(String),
    /// A URI OpenTUI refuses (`unsupported-url-scheme`) or an unreadable `data:` URI.
    Invalid,
}

/// Classify a source URI like `NativeImage.load`.
pub fn source_of(uri: &str) -> Source {
    if crate::app::media::media_hash(uri).is_some() {
        return Source::Media(uri.to_string());
    }
    let lower = uri.get(..uri.find(':').map(|i| i + 1).unwrap_or(0)).unwrap_or("").to_ascii_lowercase();
    let drive = uri.len() >= 3 && uri.as_bytes()[0].is_ascii_alphabetic() && uri.as_bytes()[1] == b':' && matches!(uri.as_bytes()[2], b'\\' | b'/');
    let scheme_url = (matches!(lower.as_str(), "http:" | "https:" | "file:" | "blob:" | "data:") || scheme_slashes(uri)) && !drive;
    if !scheme_url {
        return Source::File(uri.into());
    }
    match lower.as_str() {
        "data:" => data_uri_bytes(uri).map(Source::Bytes).unwrap_or(Source::Invalid),
        "file:" => file_url_path(uri).map(Source::File).unwrap_or(Source::Invalid),
        "http:" | "https:" => Source::Remote(uri.to_string()),
        _ => Source::Invalid,
    }
}

/// `/^[a-z][a-z0-9+.-]*:\/\//i`.
fn scheme_slashes(uri: &str) -> bool {
    let Some(i) = uri.find("://") else { return false };
    let scheme = &uri[..i];
    !scheme.is_empty() && scheme.as_bytes()[0].is_ascii_alphabetic() && scheme.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-'))
}

/// The bytes of a `data:` URI (base64 or percent-encoded).
pub fn data_uri_bytes(uri: &str) -> Option<Vec<u8>> {
    let rest = uri.get(5..)?;
    let comma = rest.find(',')?;
    let (meta, data) = (&rest[..comma], &rest[comma + 1..]);
    if meta.split(';').any(|p| p.trim().eq_ignore_ascii_case("base64")) {
        let clean: String = data.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        let clean = percent_decode(&clean);
        let text = String::from_utf8_lossy(&clean).into_owned();
        base64::engine::general_purpose::STANDARD
            .decode(text.trim_end_matches('='))
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(text.trim_end_matches('=')))
            .ok()
    } else {
        Some(percent_decode(data))
    }
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn file_url_path(uri: &str) -> Option<std::path::PathBuf> {
    let rest = &uri[5..];
    let rest = rest.strip_prefix("//").map(|r| r.find('/').map(|i| &r[i..]).unwrap_or("")).unwrap_or(rest);
    let decoded = String::from_utf8(percent_decode(rest)).ok()?;
    #[cfg(windows)]
    {
        let d = decoded.strip_prefix('/').filter(|d| d.len() >= 2 && d.as_bytes()[1] == b':').unwrap_or(&decoded);
        return Some(std::path::PathBuf::from(d.replace('/', "\\")));
    }
    #[cfg(not(windows))]
    Some(std::path::PathBuf::from(decoded))
}

/// The length of a PNG up to the end of its `IEND` chunk (OpenTUI's `effective_len`), when it is one.
fn png_len(data: &[u8]) -> Option<usize> {
    const SIG: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
    if data.len() < 8 || data[..8] != SIG {
        return None;
    }
    let mut at = 8usize;
    while at + 12 <= data.len() {
        let len = u32::from_be_bytes(data[at..at + 4].try_into().ok()?) as usize;
        let kind = &data[at + 4..at + 8];
        let end = at.checked_add(12)?.checked_add(len)?;
        if end > data.len() {
            return None;
        }
        if kind == b"IEND" {
            return Some(end);
        }
        at = end;
    }
    None
}

/// `NativeImage.decode`: PNG, JPEG, WebP and the first frame of a GIF, turned to their EXIF orientation.
pub fn decode(data: &[u8]) -> anyhow::Result<Picture> {
    use image::ImageDecoder as _;
    let format = image::guess_format(data)?;
    if !matches!(format, image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::WebP | image::ImageFormat::Gif) {
        anyhow::bail!("unsupported image format");
    }
    let png = if format == image::ImageFormat::Png { png_len(data) } else { None };
    let bytes = png.map(|n| &data[..n]).unwrap_or(data);
    let mut decoder = image::ImageReader::with_format(std::io::Cursor::new(bytes), format).into_decoder()?;
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = image::DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    let mut picture = Picture::from_rgba(width, height, rgba.into_raw());
    if png.is_some() && orientation == image::metadata::Orientation::NoTransforms {
        picture.png = Some(bytes.to_vec());
    }
    Ok(picture)
}

/// A loaded source: decoded, failed, or waiting for bytes.
#[derive(Clone, Debug)]
pub enum Loaded {
    Ready(Arc<Picture>),
    Failed,
    Pending,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_sources_like_native_image_load() {
        assert_eq!(source_of("data:image/png;base64,AQID"), Source::Bytes(vec![1, 2, 3]));
        assert_eq!(source_of("data:text/plain,a%20b"), Source::Bytes(b"a b".to_vec()));
        assert_eq!(source_of("/tmp/a.png"), Source::File("/tmp/a.png".into()));
        assert_eq!(source_of("https://example.com/a.png"), Source::Remote("https://example.com/a.png".into()));
        assert_eq!(source_of("ftp://example.com/a.png"), Source::Invalid);
        let hash = "a".repeat(64);
        assert_eq!(source_of(&format!("media:{hash}")), Source::Media(format!("media:{hash}")));
        #[cfg(not(windows))]
        assert_eq!(source_of("file:///tmp/a%20b.png"), Source::File("/tmp/a b.png".into()));
        assert_eq!(source_of("C:\\a.png"), Source::File("C:\\a.png".into()));
    }

    #[test]
    fn decodes_png_and_keeps_its_bytes_up_to_iend() {
        let png = super::super::tests::png(3, 2, |x, y| [x as u8 * 80, y as u8 * 100, 7, 255]);
        let mut padded = png.clone();
        padded.extend_from_slice(b"trailing");
        let p = decode(&padded).unwrap();
        assert_eq!((p.width, p.height, p.has_alpha), (3, 2, false));
        assert_eq!(p.png.as_deref(), Some(&png[..]));
        assert_eq!(p.pixel(2, 1), [160, 100, 7, 255]);
        assert!(decode(&[1, 2, 3]).is_err());
    }
}
