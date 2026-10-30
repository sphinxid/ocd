//! Media assets (`packages/ai/src/media.ts`, `utils/media-type.ts`).

use crate::json::Object;
use base64::Engine;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

const OCTET_STREAM: &str = "application/octet-stream";

/// `Media.Source`. Bytes are kept base64-encoded in their JSON form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", rename_all_fields = "camelCase")]
pub enum MediaSource {
    /// Raw bytes (JSON: base64 string).
    Bytes {
        #[serde(with = "b64")]
        data: Vec<u8>,
        media_type: String,
    },
    Base64 {
        data: String,
        media_type: String,
    },
    Url {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at: Option<f64>,
    },
    Ref {
        provider: String,
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
    },
}

mod b64 {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(data: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(data))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
    Audio,
    Document,
    Other,
}

/// Inline view of a bytes/base64 asset.
#[derive(Debug, Clone, PartialEq)]
pub struct Inline {
    pub mime: String,
    pub base64: String,
    pub data_url: String,
}

/// `Media.Asset`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaAsset {
    pub source: MediaSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<MediaInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<Object>,
}

pub fn kind_of(media_type: &str) -> MediaKind {
    let lower = media_type.to_lowercase();
    if lower.starts_with("image/") {
        MediaKind::Image
    } else if lower.starts_with("video/") {
        MediaKind::Video
    } else if lower.starts_with("audio/") {
        MediaKind::Audio
    } else if lower == "application/pdf" || lower.starts_with("text/") {
        MediaKind::Document
    } else {
        MediaKind::Other
    }
}

impl MediaAsset {
    pub fn from_source(source: MediaSource) -> Self {
        Self { source, info: None, provider_metadata: None }
    }
    pub fn bytes(data: Vec<u8>, media_type: Option<&str>) -> Self {
        let media_type = media_type.map(String::from).or_else(|| detect_media_type(&data).map(String::from)).unwrap_or_else(|| OCTET_STREAM.into());
        Self::from_source(MediaSource::Bytes { data, media_type })
    }
    pub fn base64(data: impl Into<String>, media_type: impl Into<String>) -> Self {
        Self::from_source(MediaSource::Base64 { data: data.into(), media_type: media_type.into() })
    }
    pub fn url(url: impl Into<String>, media_type: Option<String>) -> Self {
        Self::from_source(MediaSource::Url { url: url.into(), media_type, expires_at: None })
    }
    pub fn reference(provider: impl Into<String>, id: impl Into<String>, media_type: Option<String>) -> Self {
        Self::from_source(MediaSource::Ref { provider: provider.into(), id: id.into(), media_type })
    }

    /// Declared media type, else sniffed from bytes, else `application/octet-stream`.
    pub fn media_type(&self) -> String {
        match &self.source {
            MediaSource::Bytes { media_type, .. } | MediaSource::Base64 { media_type, .. } => media_type.clone(),
            MediaSource::Url { media_type, .. } | MediaSource::Ref { media_type, .. } => {
                media_type.clone().unwrap_or_else(|| OCTET_STREAM.into())
            }
        }
    }
    pub fn kind(&self) -> MediaKind {
        kind_of(&self.media_type())
    }
    pub fn source_type(&self) -> &'static str {
        match &self.source {
            MediaSource::Bytes { .. } => "bytes",
            MediaSource::Base64 { .. } => "base64",
            MediaSource::Url { .. } => "url",
            MediaSource::Ref { .. } => "ref",
        }
    }
    /// `inline()`: only for bytes/base64 sources.
    pub fn inline(&self) -> Option<Inline> {
        let base64 = match &self.source {
            MediaSource::Base64 { data, .. } => data.clone(),
            MediaSource::Bytes { data, .. } => base64::engine::general_purpose::STANDARD.encode(data),
            _ => return None,
        };
        let mime = self.media_type().to_lowercase();
        let data_url = format!("data:{mime};base64,{base64}");
        Some(Inline { mime, base64, data_url })
    }
    pub fn url_value(&self) -> Option<&str> {
        match &self.source {
            MediaSource::Url { url, .. } => Some(url),
            _ => None,
        }
    }
    /// Decoded bytes for inline sources.
    pub fn decoded_bytes(&self) -> Option<Vec<u8>> {
        match &self.source {
            MediaSource::Bytes { data, .. } => Some(data.clone()),
            MediaSource::Base64 { data, .. } => base64::engine::general_purpose::STANDARD.decode(data.trim()).ok(),
            _ => None,
        }
    }
}

static DATA_URL: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)^data:([^;,]+)(?:;[^,]*)*;base64,(.*)$").unwrap());

/// `Media.parseDataUrl`.
pub fn parse_data_url(value: &str) -> Option<MediaAsset> {
    let caps = DATA_URL.captures(value)?;
    Some(MediaAsset::base64(caps[2].to_string(), caps[1].to_string()))
}

/// The media type of an ISO-BMFF (`ftyp`) file, shared by `detectMediaType` and core's `Mime.detect`: AVIF, HEIC,
/// QuickTime and M4A share MP4's container; the major brand tells them apart, and generic HEIF brands list
/// `avif`/`avis` among the compatible brands that follow the minor version.
pub fn iso_bmff_media_type(bytes: &[u8]) -> Option<&'static str> {
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

pub fn detect_media_type(bytes: &[u8]) -> Option<&'static str> {
    let starts = |prefix: &[u8]| bytes.len() >= prefix.len() && &bytes[..prefix.len()] == prefix;
    if starts(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if starts(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if starts(&[0x47, 0x49, 0x46, 0x38]) {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" {
        if &bytes[8..12] == b"WEBP" {
            return Some("image/webp");
        }
        if &bytes[8..12] == b"WAVE" {
            return Some("audio/wav");
        }
    }
    if starts(&[0x25, 0x50, 0x44, 0x46]) {
        return Some("application/pdf");
    }
    if let Some(mime) = iso_bmff_media_type(bytes) {
        return Some(mime);
    }
    if starts(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Some("video/webm");
    }
    if starts(&[0x49, 0x44, 0x33]) {
        return Some("audio/mpeg");
    }
    if bytes.len() >= 2 && bytes[0] == 0xff && (bytes[1] & 0xe0) == 0xe0 {
        return Some(if (bytes[1] & 0x06) == 0 { "audio/aac" } else { "audio/mpeg" });
    }
    if starts(&[0x4f, 0x67, 0x67, 0x53]) {
        return Some("audio/ogg");
    }
    if starts(&[0x66, 0x4c, 0x61, 0x43]) {
        return Some("audio/flac");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_url_roundtrip() {
        let asset = parse_data_url("data:image/png;base64,AAAA").unwrap();
        assert_eq!(asset.media_type(), "image/png");
        assert_eq!(asset.inline().unwrap().data_url, "data:image/png;base64,AAAA");
        assert_eq!(kind_of("application/pdf"), MediaKind::Document);
    }

    // media.test.ts / core mime.test.ts: "tells ISO-BMFF brands apart".
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
            assert_eq!(detect_media_type(&ftyp(hex)), Some(mime), "{hex}");
        }
    }

    #[test]
    fn serde_shape() {
        let asset = MediaAsset::bytes(vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a], None);
        let json = serde_json::to_value(&asset).unwrap();
        assert_eq!(json["source"]["type"], "bytes");
        assert_eq!(json["source"]["mediaType"], "image/png");
    }
}
