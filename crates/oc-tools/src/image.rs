//! Image normalization (`core/src/image.ts`, `core/src/image/photon.ts`) using the `image` crate.

use base64::Engine;
use oc_schema::ToolContent;

use crate::tool::ImageLimits;

const JPEG_QUALITIES: [u8; 4] = [80, 70, 55, 40];
/// A fitting PNG at most this large (base64) passes through unchanged.
const PNG_PASSTHROUGH_MAX_BASE64_BYTES: u64 = 384 * 1024;
/// A PNG with at most this many pixels passes through unchanged, and is tried as PNG first.
const SMALL_PNG_MAX_PIXELS: u64 = 256 * 256;

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

/// Samples an evenly spaced grid of up to 64×64 pixels; transparent when more than 5% of the
/// samples have alpha below 250.
fn has_transparency(pixels: &image::RgbaImage) -> bool {
    let (width, height) = (pixels.width(), pixels.height());
    let columns = width.min(64);
    let rows = height.min(64);
    let mut transparent = 0u64;
    for row in 0..rows {
        let y = (((row as f64 + 0.5) * height as f64) / rows as f64).floor() as u32;
        for column in 0..columns {
            let x = (((column as f64 + 0.5) * width as f64) / columns as f64).floor() as u32;
            if pixels.get_pixel(x.min(width - 1), y.min(height - 1))[3] < 250 {
                transparent += 1;
            }
        }
    }
    transparent as f64 / (columns as u64 * rows as u64) as f64 > 0.05
}

fn encode_png(image: &image::DynamicImage) -> Option<Vec<u8>> {
    let mut png = Vec::new();
    image.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    Some(png)
}

fn encode_jpeg(image: &image::DynamicImage, quality: u8) -> Option<Vec<u8>> {
    let rgb = image::DynamicImage::ImageRgb8(image.to_rgb8());
    let mut jpeg = Vec::new();
    rgb.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality)).ok()?;
    Some(jpeg)
}

/// `Image.normalize(resource, {content (base64), mime})` (`image/photon-normalize.ts`).
pub fn normalize(resource: &str, content_b64: &str, mime: &str, limits: &ImageLimits) -> Result<Normalized, ImageError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(content_b64.as_bytes())
        .map_err(|_| ImageError::Decode { resource: resource.to_string() })?;
    let decoded = image::load_from_memory(&bytes).map_err(|_| ImageError::Decode { resource: resource.to_string() })?;
    let (width, height) = (decoded.width(), decoded.height());
    let size = content_b64.len() as u64;
    let unchanged = || Ok(Normalized { content: content_b64.to_string(), mime: mime.to_string() });
    let fits = width <= limits.max_width && height <= limits.max_height && size <= limits.max_base64_bytes;
    let small_png = size <= PNG_PASSTHROUGH_MAX_BASE64_BYTES || width as u64 * height as u64 <= SMALL_PNG_MAX_PIXELS;
    if fits && (!limits.auto_resize || mime != "image/png" || small_png) {
        return unchanged();
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
    let transparent = mime != "image/jpeg" && has_transparency(&decoded.to_rgba8());
    let encoded = |content: Vec<u8>, mime: &str| Normalized {
        content: base64::engine::general_purpose::STANDARD.encode(content),
        mime: mime.to_string(),
    };
    // An image that already fits only changes when JPEG makes it smaller.
    if fits {
        if transparent {
            return unchanged();
        }
        return match encode_jpeg(&decoded, JPEG_QUALITIES[0]) {
            Some(jpeg) if base64_len(jpeg.len()) < size => Ok(encoded(jpeg, "image/jpeg")),
            _ => unchanged(),
        };
    }
    for (w, h) in candidate_sizes(width, height, limits.max_width, limits.max_height) {
        let resized = if (w, h) == (width, height) {
            decoded.clone()
        } else {
            decoded.resize_exact(w, h, image::imageops::FilterType::Lanczos3)
        };
        let png_first = transparent || w as u64 * h as u64 <= SMALL_PNG_MAX_PIXELS;
        let jpegs = JPEG_QUALITIES.iter().map(|quality| ("image/jpeg", Some(*quality)));
        let encoders: Vec<(&str, Option<u8>)> = if png_first {
            std::iter::once(("image/png", None)).chain(jpegs).collect()
        } else {
            jpegs.chain(std::iter::once(("image/png", None))).collect()
        };
        for (mime, quality) in encoders {
            let candidate = match quality {
                Some(quality) => encode_jpeg(&resized, quality),
                None => encode_png(&resized),
            };
            if let Some(candidate) = candidate.filter(|c| base64_len(c.len()) <= limits.max_base64_bytes) {
                return Ok(encoded(candidate, mime));
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

    fn png_from(width: u32, height: u32, pixel: impl Fn(u32, u32, u32) -> u8) -> String {
        let img = image::RgbaImage::from_fn(width, height, |x, y| image::Rgba([0, 1, 2, 3].map(|c| pixel(x, y, c))));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    // tool-read.test.ts "stores large opaque PNG screenshots as JPEG"
    #[test]
    fn stores_large_opaque_png_screenshots_as_jpeg() {
        let (width, height) = (2000u32, 1125u32);
        let content = png_from(width, height, |x, y, c| {
            let index = ((y * width + x) * 4 + c) as u64;
            if c == 3 { 255 } else { ((index * 31 + (index >> 7)) % 256) as u8 }
        });
        let result = normalize("screenshot.png", &content, "image/png", &ImageLimits::default()).unwrap();
        assert_eq!(result.mime, "image/jpeg");
        assert!(result.content.len() < content.len());
        let decoded =
            image::load_from_memory(&base64::engine::general_purpose::STANDARD.decode(result.content).unwrap()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (width, height));
    }

    // tool-read.test.ts "keeps fitting PNGs that are small, transparent, or smaller than their JPEG"
    #[test]
    fn keeps_fitting_pngs_that_are_small_transparent_or_smaller_than_their_jpeg() {
        let mut seed: u32 = 1;
        let blocks: Vec<u8> = (0..500 * 375)
            .map(|_| {
                seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
                ((seed >> 31) * 255) as u8
            })
            .collect();
        let transparent = png_from(600, 600, |x, y, c| {
            if c == 3 { if y < 120 { 0 } else { 255 } } else { ((((y * 600 + x) * 4 + c) * 31 + x) % 256) as u8 }
        });
        let small = png_from(1000, 1000, |_, y, c| if c == 3 { 255 } else { (y % 256) as u8 });
        let diagram =
            png_from(2000, 1500, |x, y, c| if c == 3 { 255 } else { blocks[((y / 4) * 500 + x / 4) as usize] });
        assert!(transparent.len() > 384 * 1024);
        assert!(small.len() <= 384 * 1024);
        assert!(diagram.len() > 384 * 1024);
        for content in [transparent, small, diagram] {
            let result = normalize("image.png", &content, "image/png", &ImageLimits::default()).unwrap();
            assert_eq!(result, Normalized { content, mime: "image/png".into() });
        }
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
