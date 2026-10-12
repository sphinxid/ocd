//! Image normalization (`core/src/image.ts`, `core/src/image/photon.ts`) using the `image` crate.

use base64::Engine;
use oc_schema::ToolContent;

use crate::tool::ImageLimits;

const JPEG_QUALITIES: [u8; 5] = [80, 85, 70, 55, 40];

#[derive(Debug, Clone, PartialEq)]
pub enum ImageError {
    Decode { resource: String },
    Size { resource: String, width: u32, height: u32, bytes: u64, max_width: u32, max_height: u32, max_bytes: u64 },
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageError::Decode { resource } => write!(f, "Image could not be decoded: {resource}"),
            ImageError::Size { resource, width, height, bytes, max_width, max_height, max_bytes } => write!(
                f,
                "Image {resource} is {width}x{height} with base64 size {bytes}, exceeding configured limits {max_width}x{max_height}/{max_bytes} bytes"
            ),
        }
    }
}

impl std::error::Error for ImageError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalized {
    /// Base64 content.
    pub content: String,
    pub mime: String,
}

/// Candidate sizes: first the scaled size, then repeatedly ×0.75 (floor, min 1), deduped, up to 32.
pub fn candidate_sizes(width: u32, height: u32, max_width: u32, max_height: u32) -> Vec<(u32, u32)> {
    let scale = 1f64.min(max_width as f64 / width as f64).min(max_height as f64 / height as f64);
    let mut sizes: Vec<(u32, u32)> = Vec::new();
    for _ in 0..32 {
        let next = match sizes.last() {
            None => (
                ((width as f64 * scale).round() as u32).max(1),
                ((height as f64 * scale).round() as u32).max(1),
            ),
            Some(&(w, h)) => (
                if w == 1 { 1 } else { ((w as f64 * 0.75).floor() as u32).max(1) },
                if h == 1 { 1 } else { ((h as f64 * 0.75).floor() as u32).max(1) },
            ),
        };
        if !sizes.contains(&next) {
            sizes.push(next);
        }
    }
    sizes
}

fn base64_len(n: usize) -> u64 {
    (n as u64).div_ceil(3) * 4
}

/// `Image.normalize(resource, {content (base64), mime})`.
pub fn normalize(resource: &str, content_b64: &str, mime: &str, limits: &ImageLimits) -> Result<Normalized, ImageError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(content_b64.as_bytes())
        .map_err(|_| ImageError::Decode { resource: resource.to_string() })?;
    let decoded = image::load_from_memory(&bytes).map_err(|_| ImageError::Decode { resource: resource.to_string() })?;
    let (width, height) = (decoded.width(), decoded.height());
    let size = content_b64.len() as u64;
    if width <= limits.max_width && height <= limits.max_height && size <= limits.max_base64_bytes {
        return Ok(Normalized { content: content_b64.to_string(), mime: mime.to_string() });
    }
    let size_error = || ImageError::Size {
        resource: resource.to_string(),
        width,
        height,
        bytes: size,
        max_width: limits.max_width,
        max_height: limits.max_height,
        max_bytes: limits.max_base64_bytes,
    };
    if !limits.auto_resize {
        return Err(size_error());
    }
    for (w, h) in candidate_sizes(width, height, limits.max_width, limits.max_height) {
        let resized = decoded.resize_exact(w, h, image::imageops::FilterType::Lanczos3);
        let mut png = Vec::new();
        if resized.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).is_ok()
            && base64_len(png.len()) <= limits.max_base64_bytes
        {
            return Ok(Normalized { content: base64::engine::general_purpose::STANDARD.encode(&png), mime: "image/png".into() });
        }
        let rgb = image::DynamicImage::ImageRgb8(resized.to_rgb8());
        for quality in JPEG_QUALITIES {
            let mut jpeg = Vec::new();
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality);
            if rgb.write_with_encoder(encoder).is_err() {
                continue;
            }
            if base64_len(jpeg.len()) <= limits.max_base64_bytes {
                return Ok(Normalized {
                    content: base64::engine::general_purpose::STANDARD.encode(&jpeg),
                    mime: "image/jpeg".into(),
                });
            }
        }
    }
    Err(size_error())
}

/// `normalizeImages` from `core/src/tool.ts`: every `image/*` file part with a base64 data
/// URI is normalized; undecodable/oversized parts are dropped with a trailing note.
pub fn normalize_content(content: Vec<ToolContent>, limits: &ImageLimits) -> Vec<ToolContent> {
    let re = regex::Regex::new(r"(?s)^data:[^,]*;base64,(.*)$").unwrap();
    let mut out = Vec::with_capacity(content.len());
    let mut decode_failures = 0usize;
    let mut size_failures = 0usize;
    for item in content {
        let ToolContent::File { uri, mime, name } = &item else {
            out.push(item);
            continue;
        };
        if !mime.starts_with("image/") {
            out.push(item);
            continue;
        }
        let Some(b64) = re.captures(uri).and_then(|c| c.get(1)).map(|m| m.as_str().to_string()) else {
            out.push(item);
            continue;
        };
        let resource = name.clone().unwrap_or_else(|| format!("{mime} tool output"));
        match normalize(&resource, &b64, mime, limits) {
            Ok(result) => out.push(ToolContent::File {
                uri: format!("data:{};base64,{}", result.mime, result.content),
                mime: result.mime,
                name: name.clone(),
            }),
            Err(ImageError::Decode { .. }) => decode_failures += 1,
            Err(ImageError::Size { .. }) => size_failures += 1,
        }
    }
    let note = |count: usize, text: &str| ToolContent::Text {
        text: format!("[{count} image{} omitted: {text}]", if count == 1 { "" } else { "s" }),
    };
    if decode_failures > 0 {
        out.push(note(decode_failures, "could not be decoded."));
    }
    if size_failures > 0 {
        out.push(note(size_failures, "could not be resized below the image size limit."));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> String {
        let img = image::RgbaImage::from_fn(width, height, |x, y| image::Rgba([(x % 256) as u8, (y % 256) as u8, ((x * y) % 256) as u8, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn sizes_ladder() {
        let sizes = candidate_sizes(4000, 1000, 2000, 2000);
        assert_eq!(sizes[0], (2000, 500));
        assert_eq!(sizes[1], (1500, 375));
        assert!(sizes.len() <= 32);
        assert_eq!(candidate_sizes(1, 1, 2000, 2000), vec![(1, 1)]);
    }

    #[test]
    fn passthrough_and_resize() {
        let limits = ImageLimits { auto_resize: true, max_width: 20, max_height: 20, max_base64_bytes: 1_000_000 };
        let small = png(10, 10);
        assert_eq!(normalize("a.png", &small, "image/png", &limits).unwrap().content, small);
        let big = png(40, 30);
        let result = normalize("b.png", &big, "image/png", &limits).unwrap();
        assert_eq!(result.mime, "image/png");
        let decoded = image::load_from_memory(&base64::engine::general_purpose::STANDARD.decode(result.content).unwrap()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (20, 15));
        let strict = ImageLimits { auto_resize: false, ..limits };
        let err = normalize("b.png", &big, "image/png", &strict).unwrap_err();
        assert_eq!(err.to_string(), format!("Image b.png is 40x30 with base64 size {}, exceeding configured limits 20x20/1000000 bytes", big.len()));
    }

    #[test]
    fn content_notes() {
        let limits = ImageLimits::default();
        let content = vec![
            ToolContent::File { uri: "data:image/png;base64,AAAA".into(), mime: "image/png".into(), name: None },
            ToolContent::File { uri: "data:image/png;base64,AAAA".into(), mime: "image/png".into(), name: Some("x".into()) },
            ToolContent::Text { text: "t".into() },
        ];
        let out = normalize_content(content, &limits);
        assert_eq!(out, vec![ToolContent::Text { text: "t".into() }, ToolContent::Text { text: "[2 images omitted: could not be decoded.]".into() }]);
    }
}
