//! Prompt attachment materialization (`core/session/prompt.ts`) and MIME sniffing (`core/mime.ts`).

use std::path::Path;

use base64::Engine;
use oc_schema::types::{AttachmentSource, FileAttachment, Mention};

pub const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct AttachmentError {
    pub uri: String,
    pub message: String,
}

/// `Mime.detect`
pub fn detect(bytes: &[u8]) -> &'static str {
    let at = |off: usize, p: &[u8]| bytes.len() >= off + p.len() && &bytes[off..off + p.len()] == p;
    if at(0, &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) {
        return "image/png";
    }
    if at(0, &[0xff, 0xd8, 0xff]) {
        return "image/jpeg";
    }
    if at(0, b"GIF8") {
        return "image/gif";
    }
    if at(0, b"BM") {
        return "image/bmp";
    }
    if at(0, b"%PDF-") {
        return "application/pdf";
    }
    if at(0, b"RIFF") && at(8, b"WEBP") {
        return "image/webp";
    }
    if at(4, b"ftyp") && (at(8, b"avif") || at(8, b"avis")) {
        return "image/avif";
    }
    if is_text(bytes) { "text/plain" } else { "application/octet-stream" }
}

fn is_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    if bytes.contains(&0) {
        return false;
    }
    // TextDecoder fatal + stream: an incomplete trailing sequence is allowed.
    match std::str::from_utf8(bytes) {
        Ok(_) => {}
        Err(e) if e.error_len().is_none() => {}
        Err(_) => return false,
    }
    let controls = bytes.iter().filter(|b| **b < 9 || (**b > 13 && **b < 32)).count();
    controls as f64 / bytes.len() as f64 <= 0.3
}

pub struct Input {
    pub uri: String,
    /// Source metadata the client already knows (`PromptInput.FileAttachment.source`), kept for inline data.
    pub source: Option<AttachmentSource>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub mention: Option<Mention>,
}

fn label(input: &Input) -> String {
    input.name.clone().unwrap_or_else(|| if input.uri.starts_with("data:") { "inline attachment".into() } else { input.uri.clone() })
}

fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    // decodeURIComponent throws on invalid UTF-8 sequences
    std::str::from_utf8(&out).ok()?;
    Some(out)
}

fn decode_data_url(uri: &str, label: &str) -> Result<Vec<u8>, AttachmentError> {
    let err = || AttachmentError { uri: label.into(), message: format!("Invalid attachment data URL: {label}") };
    let comma = uri.find(',').ok_or_else(err)?;
    let metadata = &uri[5..comma];
    let payload = &uri[comma + 1..];
    if !metadata.split(';').any(|p| p.eq_ignore_ascii_case("base64")) {
        return percent_decode(payload).ok_or_else(err);
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(payload).map_err(|_| err())?;
    if base64::engine::general_purpose::STANDARD.encode(&bytes) != payload {
        return Err(err());
    }
    Ok(bytes)
}

fn positive_int(v: Option<String>) -> Option<usize> {
    let n: f64 = v?.parse().ok()?;
    (n.fract() == 0.0 && n > 0.0).then_some(n as usize)
}

struct Resolved {
    bytes: Vec<u8>,
    source: AttachmentSource,
    start: Option<usize>,
    end: Option<usize>,
    name: Option<String>,
    mime: Option<String>,
}

fn read_file(uri: &str) -> Result<Resolved, AttachmentError> {
    let e = |m: String| AttachmentError { uri: uri.into(), message: m };
    let mut url = url::Url::parse(uri).map_err(|_| e(format!("Invalid attachment URI: {uri}")))?;
    if url.scheme() != "file" {
        return Err(e(format!("Unsupported attachment URI: {uri}")));
    }
    let q = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
    let (start, end) = (positive_int(q("start")), positive_int(q("end")));
    url.set_query(None);
    url.set_fragment(None);
    let target = url.to_file_path().map_err(|_| e(format!("Invalid file URI: {uri}")))?;
    let unable = || e(format!("Unable to read attachment: {uri}"));
    let meta = std::fs::metadata(&target).map_err(|_| unable())?;
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned());
    if meta.is_dir() {
        let mut entries: Vec<(bool, String)> = std::fs::read_dir(&target)
            .map_err(|_| unable())?
            .flatten()
            .filter_map(|en| {
                let t = en.file_type().ok()?;
                (t.is_file() || t.is_dir()).then(|| (t.is_dir(), en.file_name().to_string_lossy().into_owned()))
            })
            .collect();
        entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| crate::instructions::locale_compare(&a.1, &b.1)));
        let sep = std::path::MAIN_SEPARATOR.to_string();
        let listing = entries.iter().map(|(d, n)| format!("{n}{}", if *d { sep.as_str() } else { "" })).collect::<Vec<_>>().join("\n");
        return Ok(Resolved {
            bytes: listing.into_bytes(),
            source: AttachmentSource::Uri { uri: uri.into() },
            start: None,
            end: None,
            name,
            mime: Some("application/x-directory".into()),
        });
    }
    if !meta.is_file() {
        return Err(e(format!("Attachment is not a file: {uri}")));
    }
    if meta.len() as usize > MAX_ATTACHMENT_BYTES {
        return Err(e(format!("Attachment exceeds the {MAX_ATTACHMENT_BYTES} byte limit: {uri}")));
    }
    let bytes = std::fs::read(&target).map_err(|_| unable())?;
    Ok(Resolved { bytes, source: AttachmentSource::Uri { uri: uri.into() }, start, end, name, mime: None })
}

/// Optional image normalizer (resize/re-encode); returns (base64, mime).
pub type ImageNormalizer<'a> = &'a (dyn Fn(&str, &str, &str) -> Result<(String, String), String> + Sync);

/// Reads a stored media blob by hash (`SessionBlob.read`): `(mime, bytes)` while some message references it.
pub type BlobReader<'a> = &'a (dyn Fn(&str) -> Option<(String, Vec<u8>)> + Sync);

/// Image types the resizer can decode; others such as HEIC and AVIF go to the provider unchanged.
pub const RESIZABLE_IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp", "image/bmp"];

pub fn materialize(input: &Input, normalize: Option<ImageNormalizer>) -> Result<FileAttachment, AttachmentError> {
    materialize_with(input, normalize, None)
}

/// `materializeAttachment`, with stored blobs for `media:` refs (a ref a client got from an
/// `experimental-media-uri` read, sent back as an attachment; its bytes were normalized when first admitted).
pub fn materialize_with(input: &Input, normalize: Option<ImageNormalizer>, blobs: Option<BlobReader>) -> Result<FileAttachment, AttachmentError> {
    if oc_storage::stored::is_media_ref(&input.uri) {
        let blob = blobs.and_then(|read| read(&input.uri["media:".len()..]));
        let Some((mime, bytes)) = blob else {
            return Err(AttachmentError { uri: input.uri.clone(), message: format!("Attachment is no longer stored: {}", input.uri) });
        };
        return Ok(FileAttachment {
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
            mime,
            source: input.source.clone().unwrap_or(AttachmentSource::Inline),
            name: input.name.clone(),
            description: input.description.clone(),
            mention: input.mention.clone(),
        });
    }
    let label = label(input);
    let resolved = if input.uri.starts_with("data:") {
        Resolved {
            bytes: decode_data_url(&input.uri, &label)?,
            source: input.source.clone().unwrap_or(AttachmentSource::Inline),
            start: None,
            end: None,
            name: None,
            mime: None,
        }
    } else {
        read_file(&input.uri)?
    };
    if resolved.bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err(AttachmentError { uri: label.clone(), message: format!("Attachment exceeds the {MAX_ATTACHMENT_BYTES} byte limit: {label}") });
    }
    let mime = resolved.mime.clone().unwrap_or_else(|| detect(&resolved.bytes).to_string());
    let content = match (mime.as_str(), resolved.start) {
        ("text/plain", Some(start)) => {
            let text = String::from_utf8_lossy(&resolved.bytes);
            let lines: Vec<&str> = text.split('\n').collect();
            let from = (start - 1).min(lines.len());
            let to = resolved.end.unwrap_or(lines.len()).min(lines.len()).max(from);
            lines[from..to].join("\n").into_bytes()
        }
        _ => resolved.bytes,
    };
    let mut data = base64::engine::general_purpose::STANDARD.encode(&content);
    let mut mime = mime;
    if RESIZABLE_IMAGE_MIMES.contains(&mime.as_str()) {
        if let Some(n) = normalize {
            let (d, m) = n(&label, &data, &mime).map_err(|message| AttachmentError { uri: label.clone(), message })?;
            data = d;
            mime = m;
        }
    }
    Ok(FileAttachment {
        data,
        mime,
        source: resolved.source,
        name: input.name.clone().or(resolved.name),
        description: input.description.clone(),
        mention: input.mention.clone(),
    })
}

#[allow(dead_code)]
fn _p(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn data_urls() {
        let i = Input { uri: "data:text/plain,hello%20world".into(), source: None, name: None, description: None, mention: None };
        let a = materialize(&i, None).unwrap();
        assert_eq!(a.mime, "text/plain");
        assert_eq!(a.data, base64::engine::general_purpose::STANDARD.encode("hello world"));
        let bad = Input { uri: "data:;base64,aGk".into(), source: None, name: None, description: None, mention: None };
        assert_eq!(materialize(&bad, None).unwrap_err().message, "Invalid attachment data URL: inline attachment");
        assert_eq!(detect(b"\x89PNG\r\n\x1a\n...."), "image/png");
        assert_eq!(detect(&[0xff, 0xfe, 0x00]), "application/octet-stream");
    }
}
