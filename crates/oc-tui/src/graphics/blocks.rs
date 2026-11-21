//! The text fallback (OpenTUI `materializeImageFallback` / `renderQuadrantBlock`): each cell samples four pixels
//! (a 2 x 2 grid over its share of the source rectangle, nearest pixel), splits them into the two most different
//! colors and draws the quadrant character whose dark quadrants match, the darker color as foreground.

/// `quadrantChars`, indexed by the dark-quadrant bits (TL 8, TR 4, BL 2, BR 1).
pub const QUADRANTS: [char; 16] = [
    ' ', '\u{2597}', '\u{2596}', '\u{2584}', '\u{259D}', '\u{2590}', '\u{259E}', '\u{259F}', '\u{2598}', '\u{259A}', '\u{258C}', '\u{2599}', '\u{2580}', '\u{259C}', '\u{259B}', '\u{2588}',
];

pub type Rgba8 = [u8; 4];

fn distance(a: Rgba8, b: Rgba8) -> f32 {
    let d = |i: usize| a[i] as f32 - b[i] as f32;
    d(0) * d(0) + d(1) * d(1) + d(2) * d(2)
}

fn luminance(c: Rgba8) -> f32 {
    0.2126 * (c[0] as f32 / 255.0) + 0.7152 * (c[1] as f32 / 255.0) + 0.0722 * (c[2] as f32 / 255.0)
}

fn average(p: &[Rgba8; 4]) -> Rgba8 {
    let mut out = [0u8; 4];
    for c in 0..4 {
        let sum: u32 = p.iter().map(|v| v[c] as u32).sum();
        out[c] = ((sum + 2) / 4) as u8;
    }
    out
}

/// `renderQuadrantBlock(pixels)` for pixels TL, TR, BL, BR: `(char, fg, bg)`.
pub fn quadrant(p: [Rgba8; 4]) -> (char, Rgba8, Rgba8) {
    let (mut a, mut b) = (0, 1);
    let mut max = distance(p[0], p[1]);
    for i in 0..4 {
        for j in i + 1..4 {
            let d = distance(p[i], p[j]);
            if d > max {
                a = i;
                b = j;
                max = d;
            }
        }
    }
    let (dark, light) = if luminance(p[a]) <= luminance(p[b]) { (p[a], p[b]) } else { (p[b], p[a]) };
    let mut bits = 0usize;
    for (i, v) in [8, 4, 2, 1].into_iter().enumerate() {
        if distance(p[i], dark) <= distance(p[i], light) {
            bits |= v;
        }
    }
    match bits {
        0 => (' ', dark, average(&p)),
        15 => (QUADRANTS[15], average(&p), light),
        _ => (QUADRANTS[bits], dark, light),
    }
}

/// The four samples of the cell `(cx, cy)` of a `width` x `height` cell placement showing the source rectangle.
pub fn samples(image: &super::decode::Picture, src: (u32, u32, u32, u32), width: u32, height: u32, cx: u32, cy: u32) -> [Rgba8; 4] {
    let (sx0, sy0, sw, sh) = src;
    let mut out = [[0u8; 4]; 4];
    for (q, slot) in out.iter_mut().enumerate() {
        let x = cx * 2 + (q as u32 & 1);
        let y = cy * 2 + (q as u32 >> 1);
        let sx = sx0 + (sw - 1).min(((x as u64 * sw as u64) / (width as u64 * 2)) as u32);
        let sy = sy0 + (sh - 1).min(((y as u64 * sh as u64) / (height as u64 * 2)) as u32);
        *slot = image.pixel(sx, sy);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_quadrants_and_colors() {
        let k = [0, 0, 0, 255];
        let w = [255, 255, 255, 255];
        assert_eq!(quadrant([k, w, k, w]), ('\u{258C}', k, w));
        assert_eq!(quadrant([w, w, k, k]), ('\u{2584}', k, w));
        assert_eq!(quadrant([w, w, w, w]), ('\u{2588}', w, w));
        let r = [200, 10, 10, 255];
        assert_eq!(quadrant([r, r, r, k]), ('\u{2597}', k, r));
    }
}
