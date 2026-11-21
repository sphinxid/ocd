//! Area resampling (OpenTUI `resize(..., .area)`, which runs stb_image_resize2's box filter on sRGB RGBA): each
//! output pixel averages the input pixels its footprint covers, weighted by coverage, in linear light with
//! alpha-weighted colors. The results can differ from stb's by a rounding step.

use super::decode::Picture;

fn to_linear(v: u8) -> f32 {
    let n = v as f32 / 255.0;
    if n <= 0.04045 { n / 12.92 } else { ((n + 0.055) / 1.055).powf(2.4) }
}

fn to_srgb(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let e = if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
    (e * 255.0).round() as u8
}

/// Coverage weights of the input pixels under each output pixel.
fn weights(input: u32, output: u32) -> Vec<Vec<(usize, f32)>> {
    let scale = input as f64 / output as f64;
    (0..output)
        .map(|o| {
            let (start, end) = (o as f64 * scale, (o + 1) as f64 * scale);
            let mut out = vec![];
            let mut i = start.floor() as i64;
            while (i as f64) < end {
                let cover = (end.min(i as f64 + 1.0) - start.max(i as f64)) as f32;
                if cover > 0.0 {
                    out.push((i.clamp(0, input as i64 - 1) as usize, cover));
                }
                i += 1;
            }
            let total: f32 = out.iter().map(|(_, w)| w).sum();
            for (_, w) in &mut out {
                *w /= total;
            }
            out
        })
        .collect()
}

/// Resize to `width` x `height`.
pub fn area(src: &Picture, width: u32, height: u32) -> Picture {
    if width == src.width && height == src.height {
        return Picture::from_rgba(width, height, src.rgba.clone());
    }
    let lut: Vec<f32> = (0..=255u8).map(to_linear).collect();
    // Premultiplied linear floats.
    let pre: Vec<[f32; 4]> = src
        .rgba
        .chunks_exact(4)
        .map(|p| {
            let a = p[3] as f32 / 255.0;
            [lut[p[0] as usize] * a, lut[p[1] as usize] * a, lut[p[2] as usize] * a, a]
        })
        .collect();
    let wx = weights(src.width, width);
    let wy = weights(src.height, height);
    let sw = src.width as usize;
    // Horizontal pass.
    let mut mid = vec![[0f32; 4]; width as usize * src.height as usize];
    for y in 0..src.height as usize {
        for (x, ws) in wx.iter().enumerate() {
            let mut acc = [0f32; 4];
            for &(i, w) in ws {
                let p = pre[y * sw + i];
                for c in 0..4 {
                    acc[c] += p[c] * w;
                }
            }
            mid[y * width as usize + x] = acc;
        }
    }
    let mut out = Vec::with_capacity(width as usize * height as usize * 4);
    for ws in &wy {
        for x in 0..width as usize {
            let mut acc = [0f32; 4];
            for &(i, w) in ws {
                let p = mid[i * width as usize + x];
                for c in 0..4 {
                    acc[c] += p[c] * w;
                }
            }
            let a = acc[3];
            let un = |v: f32| if a > 0.0 { v / a } else { 0.0 };
            out.extend_from_slice(&[to_srgb(un(acc[0])), to_srgb(un(acc[1])), to_srgb(un(acc[2])), (a.clamp(0.0, 1.0) * 255.0).round() as u8]);
        }
    }
    Picture::from_rgba(width, height, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn averages_in_linear_light() {
        let src = Picture::from_rgba(2, 1, vec![0, 0, 0, 255, 255, 255, 255, 255]);
        let out = area(&src, 1, 1);
        // Linear 0.5 is sRGB 188.
        assert_eq!(out.rgba, vec![188, 188, 188, 255]);
        let same = area(&src, 2, 1);
        assert_eq!(same.rgba, src.rgba);
        let up = area(&Picture::from_rgba(1, 1, vec![10, 20, 30, 255]), 3, 2);
        assert!(up.rgba.chunks(4).all(|p| p == [10, 20, 30, 255]));
        // Transparent pixels do not darken their neighbours.
        let half = area(&Picture::from_rgba(2, 1, vec![255, 0, 0, 255, 0, 0, 0, 0]), 1, 1);
        assert_eq!(half.rgba, vec![255, 0, 0, 128]);
    }
}
