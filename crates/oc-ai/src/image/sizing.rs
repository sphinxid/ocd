//! `protocols/utils/image-sizing.ts`: lowering the shared `aspect` and `resolution` fields to pixel sizes.

use crate::error::AIError;
use crate::route::media::Identity;
use once_cell::sync::Lazy;
use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

impl Size {
    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
    /// `WIDTHxHEIGHT` (or another separator).
    pub fn render(&self, separator: &str) -> String {
        format!("{}{separator}{}", self.width as i64, self.height as i64)
    }
}

/// The shared `aspect` and `resolution` fields of an image request.
#[derive(Debug, Clone, Copy, Default)]
pub struct Shape<'a> {
    pub aspect: Option<&'a str>,
    pub resolution: Option<&'a str>,
}

/// What a provider accepts for free width and height.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub multiple: f64,
    pub min_edge: Option<f64>,
    pub max_edge: Option<f64>,
    pub min_pixels: Option<f64>,
    pub max_pixels: Option<f64>,
    /// Long edge over short edge.
    pub max_ratio: Option<f64>,
}

impl Limits {
    pub const fn multiple(multiple: f64) -> Self {
        Self { multiple, min_edge: None, max_edge: None, min_pixels: None, max_pixels: None, max_ratio: None }
    }
}

pub fn is_requested(shape: Shape<'_>) -> bool {
    shape.aspect.is_some() || shape.resolution.is_some()
}

static TIER: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^(\d+(?:\.\d+)?)k$").unwrap());

/// `1K` is 1, `2K` is 2, `0.5K` is 0.5; anything else is not a tier.
pub fn tier(resolution: &str) -> Option<f64> {
    TIER.captures(resolution).and_then(|c| c[1].parse().ok())
}

fn aspect_ratio(shape: Shape<'_>) -> f64 {
    let aspect = shape.aspect.unwrap_or("1:1");
    let mut parts = aspect.split(':').map(|p| js_number(p));
    let width = parts.next().unwrap_or(f64::NAN);
    let height = parts.next().unwrap_or(f64::NAN);
    width / height
}

/// `Number(text)` for the decimal strings an aspect holds.
fn js_number(text: &str) -> f64 {
    let t = text.trim();
    if t.is_empty() {
        return 0.0;
    }
    t.parse().unwrap_or(f64::NAN)
}

fn target_pixels(shape: Shape<'_>) -> f64 {
    let t = tier(shape.resolution.unwrap_or("1K")).unwrap_or(1.0);
    (t * 1024.0).powi(2)
}

/// `Number(value.toFixed(6))`.
fn to_fixed6(value: f64) -> f64 {
    format!("{value:.6}").parse().unwrap_or(value)
}

/// Free width and height: the tier sets the pixel count (`1K` is about 1024²), the aspect sets the proportions.
pub fn fit(shape: Shape<'_>, limits: Limits) -> Size {
    let max_ratio = limits.max_ratio.unwrap_or(f64::INFINITY);
    let ratio = max_ratio.min((1.0 / max_ratio).max(aspect_ratio(shape)));
    let area = target_pixels(shape);
    let width = (area * ratio).sqrt();
    let height = (area / ratio).sqrt();
    let up = (limits.min_edge.unwrap_or(0.0) / width.min(height)).max((limits.min_pixels.unwrap_or(0.0) / area).sqrt());
    let scaled = 1f64.max(up);
    let down = (limits.max_edge.unwrap_or(f64::INFINITY) / (width.max(height) * scaled))
        .min((limits.max_pixels.unwrap_or(f64::INFINITY) / area).sqrt() / scaled);
    // Round away from a limit the size reached, including one it lands on exactly, so it never crosses it.
    let round = |x: f64| -> f64 {
        if down <= 1.0 {
            x.floor()
        } else if up >= 1.0 {
            x.ceil()
        } else {
            (x + 0.5).floor()
        }
    };
    let snap = |edge: f64| -> f64 {
        limits.multiple.max(round(to_fixed6((edge * scaled * 1f64.min(down)) / limits.multiple)) * limits.multiple)
    };
    Size { width: snap(width), height: snap(height) }
}

/// The listed size closest to the shape: nearest aspect ratio first, then nearest pixel count.
pub fn nearest(shape: Shape<'_>, sizes: &[Size]) -> Size {
    let ratio = aspect_ratio(shape).ln();
    let area = target_pixels(shape).ln();
    let distance = |size: &Size| ((size.width / size.height).ln() - ratio).abs();
    let area_distance = |size: &Size| ((size.width * size.height).ln() - area).abs();
    let mut best = sizes[0];
    for size in &sizes[1..] {
        let (size_ratio, best_ratio) = (distance(size), distance(&best));
        if (size_ratio - best_ratio).abs() > 1e-9 {
            if size_ratio < best_ratio {
                best = *size;
            }
            continue;
        }
        if area_distance(size) < area_distance(&best) {
            best = *size;
        }
    }
    best
}

/// A size more than 2% off the requested aspect or more than √2 per edge off the requested tier.
fn missed(shape: Shape<'_>, size: Size) -> Option<String> {
    let aspect_missed = shape.aspect.is_some() && ((size.width / size.height / aspect_ratio(shape)).ln()).abs() > 0.02;
    let requested_tier = shape.resolution.and_then(tier);
    let resolution_missed = shape.resolution.is_some()
        && match requested_tier {
            None => true,
            Some(t) => (((size.width * size.height).sqrt() / (t * 1024.0)).log2()).abs() > 0.5,
        };
    if !aspect_missed && !resolution_missed {
        return None;
    }
    Some([shape.aspect, shape.resolution].into_iter().flatten().collect::<Vec<_>>().join(" at "))
}

fn gcd(a: f64, b: f64) -> f64 {
    if b == 0.0 { a } else { gcd(b, a % b) }
}

fn describe(size: Size) -> String {
    let divisor = gcd(size.width, size.height);
    let width = size.width / divisor;
    let height = size.height / divisor;
    let ratio = if width <= 32.0 && height <= 32.0 {
        format!("{}:{}", width as i64, height as i64)
    } else {
        format!("{:.2}:1", size.width / size.height)
    };
    format!("{}x{} ({ratio})", size.width as i64, size.height as i64)
}

/// Free width and height that must honor the shape; a size that misses it fails.
pub fn exact(route: &Identity, shape: Shape<'_>, limits: Limits) -> Result<Option<Size>, AIError> {
    if !is_requested(shape) {
        return Ok(None);
    }
    let size = fit(shape, limits);
    let Some(requested) = missed(shape, size) else { return Ok(Some(size)) };
    Err(route.unsupported(
        if shape.resolution.is_none() { "media.aspect" } else { "media.resolution" },
        format!(
            "{} can't render {requested}; the closest size it takes is {}. Change aspect or resolution, or set the size in providerOptions",
            route.name,
            describe(size)
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape<'a>(aspect: Option<&'a str>, resolution: Option<&'a str>) -> Shape<'a> {
        Shape { aspect, resolution }
    }

    const GPT_IMAGE_2: Limits =
        Limits { multiple: 16.0, min_edge: None, max_edge: Some(3840.0), min_pixels: Some(655_360.0), max_pixels: Some(8_294_400.0), max_ratio: Some(3.0) };

    fn size(width: f64, height: f64) -> Size {
        Size::new(width, height)
    }

    // `test/image-sizing.test.ts`.
    #[test]
    fn fits_an_aspect_ratio_at_a_resolution_tier() {
        assert_eq!(fit(shape(None, None), Limits::multiple(16.0)), size(1024.0, 1024.0));
        assert_eq!(fit(shape(Some("16:9"), None), GPT_IMAGE_2), size(1360.0, 768.0));
        assert_eq!(fit(shape(Some("9:16"), Some("2K")), GPT_IMAGE_2), size(1536.0, 2736.0));
        assert_eq!(fit(shape(Some("1:1"), Some("0.5K")), Limits::multiple(16.0)), size(512.0, 512.0));
    }

    #[test]
    fn stays_within_the_providers_limits() {
        assert_eq!(fit(shape(Some("16:9"), Some("4K")), GPT_IMAGE_2), size(3840.0, 2160.0));
        assert_eq!(fit(shape(Some("1:1"), Some("4K")), GPT_IMAGE_2), size(2880.0, 2880.0));
        assert_eq!(fit(shape(Some("1:1"), Some("0.5K")), GPT_IMAGE_2), size(816.0, 816.0));
        assert_eq!(fit(shape(Some("4:1"), None), GPT_IMAGE_2), size(1776.0, 592.0));
        let pixels = Limits { max_pixels: Some(2048.0 * 2048.0), ..Limits::multiple(16.0) };
        assert_eq!(fit(shape(Some("16:9"), Some("2K")), pixels), size(2720.0, 1536.0));
        let edges = Limits { min_edge: Some(1024.0), max_edge: Some(2048.0), ..Limits::multiple(32.0) };
        assert_eq!(fit(shape(Some("16:9"), None), edges), size(1824.0, 1024.0));
    }

    #[test]
    fn picks_the_nearest_listed_size_by_aspect_then_by_pixel_count() {
        let standard = [size(1024.0, 1024.0), size(1536.0, 1024.0), size(1024.0, 1536.0)];
        assert_eq!(nearest(shape(Some("16:9"), None), &standard), size(1536.0, 1024.0));
        assert_eq!(nearest(shape(Some("3:4"), None), &standard), size(1024.0, 1536.0));
        let squares: Vec<Size> = [256.0, 512.0, 1024.0].into_iter().map(|e| size(e, e)).collect();
        assert_eq!(nearest(shape(None, Some("0.5K")), &squares), size(512.0, 512.0));
    }
}
