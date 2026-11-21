//! Sixel encoding (OpenTUI `terminal-image.zig`): Wu's moment-cube quantizer down to at most 255 colors (pixels
//! with alpha under 128 stay transparent), then six-pixel bands with one plane per palette color, run-length
//! encoded from four repeats. The DCS asks for a transparent background (`P2 = 1`) and gives the raster size.

use std::fmt::Write as _;

use super::decode::Picture;

const SIDE: usize = 33;
const LEN: usize = SIDE * SIDE * SIDE;

#[derive(Clone, Copy, Default)]
struct Moment {
    count: u64,
    r: u64,
    g: u64,
    b: u64,
    square: u64,
}

impl Moment {
    fn add(&mut self, v: Moment) {
        self.count += v.count;
        self.r += v.r;
        self.g += v.g;
        self.b += v.b;
        self.square += v.square;
    }
}

#[derive(Clone, Copy)]
struct Bx {
    r0: u8,
    r1: u8,
    g0: u8,
    g1: u8,
    b0: u8,
    b1: u8,
}

impl Default for Bx {
    fn default() -> Self {
        Bx { r0: 0, r1: 32, g0: 0, g1: 32, b0: 0, b1: 32 }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Axis {
    R,
    G,
    B,
}

fn idx(r: usize, g: usize, b: usize) -> usize {
    (r * SIDE + g) * SIDE + b
}

fn field(m: &Moment, f: u8) -> i128 {
    (match f {
        0 => m.count,
        1 => m.r,
        2 => m.g,
        3 => m.b,
        _ => m.square,
    }) as i128
}

fn volume(m: &[Moment], b: Bx, f: u8) -> i128 {
    let (r0, r1, g0, g1, b0, b1) = (b.r0 as usize, b.r1 as usize, b.g0 as usize, b.g1 as usize, b.b0 as usize, b.b1 as usize);
    field(&m[idx(r1, g1, b1)], f) - field(&m[idx(r1, g1, b0)], f) - field(&m[idx(r1, g0, b1)], f) + field(&m[idx(r1, g0, b0)], f)
        - field(&m[idx(r0, g1, b1)], f)
        + field(&m[idx(r0, g1, b0)], f)
        + field(&m[idx(r0, g0, b1)], f)
        - field(&m[idx(r0, g0, b0)], f)
}

#[derive(Clone, Copy)]
struct Stats {
    count: u64,
    r: u64,
    g: u64,
    b: u64,
}

fn stats(m: &[Moment], b: Bx) -> Stats {
    Stats { count: volume(m, b, 0) as u64, r: volume(m, b, 1) as u64, g: volume(m, b, 2) as u64, b: volume(m, b, 3) as u64 }
}

fn energy(s: Stats) -> f64 {
    if s.count == 0 {
        return 0.0;
    }
    let (r, g, b) = (s.r as f64, s.g as f64, s.b as f64);
    (2.0 * r * r + 4.0 * g * g + 3.0 * b * b) / (8.0 * s.count as f64)
}

fn variance(m: &[Moment], b: Bx) -> f64 {
    (volume(m, b, 4) as u64 as f64 - energy(stats(m, b))).max(0.0)
}

fn best_cut(m: &[Moment], b: Bx) -> Option<(Axis, u8, f64)> {
    let mut best: Option<(Axis, u8, f64)> = None;
    for axis in [Axis::R, Axis::G, Axis::B] {
        let (low, high) = match axis {
            Axis::R => (b.r0, b.r1),
            Axis::G => (b.g0, b.g1),
            Axis::B => (b.b0, b.b1),
        };
        let mut at = low + 1;
        while at < high {
            let (mut first, mut second) = (b, b);
            match axis {
                Axis::R => {
                    first.r1 = at;
                    second.r0 = at;
                }
                Axis::G => {
                    first.g1 = at;
                    second.g0 = at;
                }
                Axis::B => {
                    first.b1 = at;
                    second.b0 = at;
                }
            }
            let (fs, ss) = (stats(m, first), stats(m, second));
            if fs.count != 0 && ss.count != 0 {
                let score = energy(fs) + energy(ss);
                if best.is_none_or(|(_, _, s)| score > s) {
                    best = Some((axis, at, score));
                }
            }
            at += 1;
        }
    }
    best
}

/// `QuantizedSixel`: a palette and one index per pixel (255 = transparent).
pub struct Quantized {
    pub palette: Vec<[u8; 3]>,
    pub indices: Vec<u8>,
}

/// `quantizeSixel(image, max_colors)`.
pub fn quantize(image: &Picture, max_colors: usize) -> Quantized {
    let max_colors = max_colors.clamp(1, 255);
    let px = &image.rgba;
    let count = image.width as usize * image.height as usize;
    let mut m = vec![Moment::default(); LEN];
    let mut active = 0usize;
    for p in 0..count {
        let o = p * 4;
        if px[o + 3] < 128 {
            continue;
        }
        let (r, g, b) = (px[o], px[o + 1], px[o + 2]);
        let bin = &mut m[idx((r >> 3) as usize + 1, (g >> 3) as usize + 1, (b >> 3) as usize + 1)];
        if bin.count == 0 {
            active += 1;
        }
        bin.count += 1;
        bin.r += r as u64;
        bin.g += g as u64;
        bin.b += b as u64;
        bin.square += (2 * r as u64 * r as u64 + 4 * g as u64 * g as u64 + 3 * b as u64 * b as u64 + 4) >> 3;
    }
    for r in 1..SIDE {
        let mut area = [Moment::default(); SIDE];
        for g in 1..SIDE {
            let mut line = Moment::default();
            for b in 1..SIDE {
                line.add(m[idx(r, g, b)]);
                area[b].add(line);
                let mut cumulative = m[idx(r - 1, g, b)];
                cumulative.add(area[b]);
                m[idx(r, g, b)] = cumulative;
            }
        }
    }
    if active == 0 {
        return Quantized { palette: vec![], indices: vec![255; count] };
    }
    let mut boxes = vec![Bx::default()];
    let mut variances = vec![variance(&m, boxes[0])];
    while boxes.len() < active.min(max_colors) {
        let mut split: Option<usize> = None;
        let mut highest = 0.0;
        for (i, v) in variances.iter().enumerate() {
            if *v > highest {
                highest = *v;
                split = Some(i);
            }
        }
        let Some(split) = split else { break };
        let Some((axis, at, _)) = best_cut(&m, boxes[split]) else {
            variances[split] = 0.0;
            continue;
        };
        let mut second = boxes[split];
        match axis {
            Axis::R => {
                boxes[split].r1 = at;
                second.r0 = at;
            }
            Axis::G => {
                boxes[split].g1 = at;
                second.g0 = at;
            }
            Axis::B => {
                boxes[split].b1 = at;
                second.b0 = at;
            }
        }
        boxes.push(second);
        variances[split] = variance(&m, boxes[split]);
        variances.push(variance(&m, second));
    }
    // Common colors first: shorter selectors for the colors each band repeats most.
    let mut counts: Vec<u64> = boxes.iter().map(|b| stats(&m, *b).count).collect();
    for i in 1..boxes.len() {
        let (b, c) = (boxes[i], counts[i]);
        let mut d = i;
        while d > 0 && counts[d - 1] < c {
            boxes[d] = boxes[d - 1];
            counts[d] = counts[d - 1];
            d -= 1;
        }
        boxes[d] = b;
        counts[d] = c;
    }
    let mut tags = vec![0u8; 32 * 32 * 32];
    let mut palette = Vec::with_capacity(boxes.len());
    for (i, b) in boxes.iter().enumerate() {
        let s = stats(&m, *b);
        palette.push([((s.r + s.count / 2) / s.count) as u8, ((s.g + s.count / 2) / s.count) as u8, ((s.b + s.count / 2) / s.count) as u8]);
        for r in b.r0..b.r1 {
            for g in b.g0..b.g1 {
                for bb in b.b0..b.b1 {
                    tags[((r as usize) << 10) | ((g as usize) << 5) | bb as usize] = i as u8;
                }
            }
        }
    }
    let indices = (0..count)
        .map(|p| {
            let o = p * 4;
            if px[o + 3] < 128 { 255 } else { tags[((px[o] as usize >> 3) << 10) | ((px[o + 1] as usize >> 3) << 5) | (px[o + 2] as usize >> 3)] }
        })
        .collect();
    Quantized { palette, indices }
}

/// `writeSixelIndexedPayload`: the DCS body (without `ESC P` and the terminator).
pub fn payload(indices: &[u8], palette: &[[u8; 3]], width: u32, height: u32) -> String {
    let (w, h) = (width as usize, height as usize);
    let mut out = String::new();
    let _ = write!(out, "0;1;0q\"1;1;{width};{height}");
    for (i, c) in palette.iter().enumerate() {
        let s = |v: u8| (v as u32 * 100 + 127) / 255;
        let _ = write!(out, "#{i};2;{};{};{}", s(c[0]), s(c[1]), s(c[2]));
    }
    let mut masks = vec![0u8; palette.len() * w];
    let mut generations = [0u32; 256];
    let mut last = [0usize; 256];
    let mut generation = 0u32;
    let mut band = 0usize;
    while band < h {
        generation += 1;
        for bit in 0..6 {
            let y = band + bit;
            if y >= h {
                continue;
            }
            for x in 0..w {
                let pi = indices[y * w + x] as usize;
                if pi == 255 || pi >= palette.len() {
                    continue;
                }
                if generations[pi] != generation {
                    generations[pi] = generation;
                    masks[pi * w..pi * w + w].fill(0);
                    last[pi] = 0;
                }
                masks[pi * w + x] |= 1 << bit;
                last[pi] = last[pi].max(x + 1);
            }
        }
        let mut first = true;
        for pi in 0..palette.len() {
            if generations[pi] != generation {
                continue;
            }
            let plane = &masks[pi * w..pi * w + last[pi]];
            if !first {
                out.push('$');
            }
            first = false;
            let _ = write!(out, "#{pi}");
            let mut x = 0;
            while x < plane.len() {
                let mask = plane[x];
                let ch = (b'?' + mask) as char;
                let mut run = 1;
                while x + run < plane.len() && plane[x + run] == mask {
                    run += 1;
                }
                if run >= 4 {
                    let _ = write!(out, "!{run}{ch}");
                } else {
                    for _ in 0..run {
                        out.push(ch);
                    }
                }
                x += run;
            }
        }
        if band + 6 < h {
            out.push('-');
        }
        band += 6;
    }
    out
}

/// `writeSixelFramedPayload`.
pub fn framed(out: &mut String, payload: &str, tmux: bool) {
    out.push_str(if tmux { "\x1bPtmux;\x1b\x1bP" } else { "\x1bP" });
    out.push_str(payload);
    out.push_str(if tmux { "\x1b\x1b\\\x1b\\" } else { "\x1b\\" });
}

/// The whole sixel image for `image`, or `None` when every pixel is transparent.
pub fn encode(image: &Picture) -> Option<String> {
    let q = quantize(image, 255);
    if q.palette.is_empty() {
        return None;
    }
    Some(payload(&q.indices, &q.palette, image.width, image.height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_colors_two_bands() {
        // 3 x 7: red top row, blue below; the last row is a second band.
        let mut px = Vec::new();
        for y in 0..7 {
            for _ in 0..3 {
                px.extend_from_slice(if y == 0 { &[255, 0, 0, 255] } else { &[0, 0, 255, 255] });
            }
        }
        let image = Picture::from_rgba(3, 7, px);
        let q = quantize(&image, 255);
        // Blue covers more pixels, so it gets index 0.
        assert_eq!(q.palette, vec![[0, 0, 255], [255, 0, 0]]);
        assert_eq!(encode(&image).unwrap(), "0;1;0q\"1;1;3;7#0;2;0;0;100#1;2;100;0;0#0}}}$#1@@@-#0@@@");
    }

    #[test]
    fn runs_and_transparency() {
        let mut px = vec![];
        for x in 0..6 {
            px.extend_from_slice(if x == 5 { &[0, 0, 0, 0] } else { &[10, 200, 30, 255] });
        }
        let image = Picture::from_rgba(6, 1, px);
        assert_eq!(encode(&image).unwrap(), "0;1;0q\"1;1;6;1#0;2;4;78;12#0!5@");
        assert!(encode(&Picture::from_rgba(1, 1, vec![0, 0, 0, 0])).is_none());
        let mut out = String::new();
        framed(&mut out, "x", true);
        assert_eq!(out, "\x1bPtmux;\x1b\x1bPx\x1b\x1b\\\x1b\\");
    }
}
