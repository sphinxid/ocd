//! The Go upsell art (`src/component/go-upsell-art.ts`): rings radiate from the GO logo while a
//! shimmer crosses it. Every cell is a pure function of the elapsed time, the size and the logo
//! position, so a frame is computed into a grid and blitted.

use super::view_helpers::{GO_LEFT, GO_RIGHT};
use crate::theme::Rgba;

const PERIOD: f64 = 4600.0;
const RINGS: usize = 3;
const WIDTH: f64 = 3.8;
const TAIL: f64 = 9.5;
const AMP: f64 = 0.55;
const TAIL_AMP: f64 = 0.16;
const BREATH_AMP: f64 = 0.05;
const BREATH_SPEED: f64 = 0.0008;
/// Offset so the background ring emits from the GO center when the logo shimmer peaks.
const PHASE_OFFSET: f64 = 0.29;
/// The frame shown when animations are off.
pub const STILL: f64 = PERIOD * 0.5;
pub const LOGO_HEIGHT: usize = 4;

#[derive(Clone, Copy, PartialEq)]
enum Glyph {
    None,
    Shadow,
    ShadowTop,
    ShadowBottom,
    Block,
    Other,
}

fn logo_lines() -> Vec<Vec<char>> {
    GO_LEFT.iter().zip(GO_RIGHT.iter()).map(|(l, r)| format!("{l} {r}").chars().collect()).collect()
}

pub fn logo_width() -> usize {
    GO_LEFT[0].chars().count() + 1 + GO_RIGHT[0].chars().count()
}

/// One cell of the art.
#[derive(Clone, Copy, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: Rgba,
    pub bg: Rgba,
    pub bold: bool,
}

type F = (f64, f64, f64);

fn f(c: Rgba) -> F {
    let q = |v: u8| (v as f32 / 255.0) as f64;
    (q(c.r), q(c.g), q(c.b))
}

/// OpenTUI keeps colors as 32-bit floats and rounds them to bytes on output.
fn to(c: F) -> Rgba {
    let q = |v: f64| ((v as f32).clamp(0.0, 1.0) * 255.0).round() as u8;
    Rgba::rgb(q(c.0), q(c.1), q(c.2))
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

/// `mix` into an RGBA whose channels are stored as 32-bit floats.
fn mix(a: F, b: F, amount: f64) -> F {
    let t = clamp01(amount);
    let c = |x: f64, y: f64| (x + (y - x) * t) as f32 as f64;
    (c(a.0, b.0), c(a.1, b.1), c(a.2, b.2))
}

fn ease(phase: f64) -> f64 {
    let e = (phase * std::f64::consts::PI).sin();
    e * e * (3.0 - 2.0 * e)
}

/// The art for a `width`×`height` area at `t` ms, the logo's top row at `logo_y` (clamped).
#[allow(clippy::too_many_arguments)]
pub fn frame(width: usize, height: usize, t: f64, logo_y: i64, background: Rgba, primary: Rgba, logo_base: Rgba, rgb: bool) -> Vec<Cell> {
    let lines = logo_lines();
    let lw = logo_width();
    let lh = lines.len();
    let left_w = GO_LEFT[0].chars().count();
    let logo_reach = ((lw as f64).hypot(lh as f64 * 2.0)) + 3.0;
    let logo_x = width.saturating_sub(lw) / 2;
    let logo_y = logo_y.clamp(0, height.saturating_sub(lh) as i64) as usize;
    let center_x = logo_x as f64 + lw as f64 / 2.0;
    let center_y = logo_y as f64 + lh as f64 / 2.0;
    let reach = (center_x.max(width as f64 - center_x)).hypot(center_y.max(height as f64 - center_y) * 2.0) + TAIL;
    let (background, primary, logo_base) = (f(background), f(primary), f(logo_base));
    let white = (1.0, 1.0, 1.0);
    let breath = (0.5 + 0.5 * (t * BREATH_SPEED).sin()) * BREATH_AMP;
    let mut ring_heads = [0.0; RINGS];
    let mut ring_eased = [0.0; RINGS];
    for ring in 0..RINGS {
        let phase = (t / PERIOD + ring as f64 / RINGS as f64 - PHASE_OFFSET + 1.0) % 1.0;
        ring_heads[ring] = phase * reach;
        ring_eased[ring] = ease(phase);
    }
    let p0 = (t / PERIOD) % 1.0;
    let p1 = (t / PERIOD + 0.5) % 1.0;
    let logo_heads = [p0 * logo_reach, p1 * logo_reach];
    let logo_eased = [ease(p0), ease(p1)];
    let shadow = mix(background, logo_base, 0.25);
    let logo_pulse = |dist: f64| -> (f64, f64) {
        let mut peak = 0.04;
        let mut glow = 0.0;
        for i in 0..2 {
            let delta = dist - logo_heads[i];
            let core = (-(delta / 1.2).abs().powf(1.8)).exp();
            let soft = (-(delta / 7.0).abs().powf(1.6)).exp();
            let tail = if delta < 0.0 && delta > -7.0 { (1.0 + delta / 7.0).powf(2.6) } else { 0.0 };
            peak += core * 0.65 * logo_eased[i];
            glow += (soft * 0.16 + tail * 0.22) * logo_eased[i];
        }
        (peak.min(1.0), glow.min(1.0))
    };
    let tint_logo = |glow: f64, peak: f64| mix(mix(logo_base, primary, glow), white, peak);
    let mut out = Vec::with_capacity(width * height);
    for y in 0..height {
        let ly = y as i64 - logo_y as i64;
        let in_logo_row = ly >= 0 && (ly as usize) < lh;
        for x in 0..width {
            let dist = (x as f64 + 0.5 - center_x).hypot((y as f64 + 0.5 - center_y) * 2.0);
            let falloff = (1.0 - (dist / (reach * 0.85)).powi(2)).max(0.0);
            let mut level = 0.0;
            for ring in 0..RINGS {
                let delta = dist - ring_heads[ring];
                let crest = if delta.abs() < WIDTH { 0.5 + 0.5 * ((delta / WIDTH) * std::f64::consts::PI).cos() } else { 0.0 };
                let tail = if delta < 0.0 && delta > -TAIL { (1.0 + delta / TAIL).powf(2.3) } else { 0.0 };
                level += (crest * AMP + tail * TAIL_AMP) * ring_eased[ring];
            }
            let pulse = mix(background, primary, ((level / RINGS as f64 + breath) * falloff).min(1.0) * 0.7);
            let lx = x as i64 - logo_x as i64;
            let glyph_char = if in_logo_row && lx >= 0 && (lx as usize) < lw { Some(lines[ly as usize][lx as usize]) } else { None };
            let glyph = match glyph_char {
                None | Some(' ') => Glyph::None,
                Some('_') => Glyph::Shadow,
                Some('^') => Glyph::ShadowTop,
                Some('~') => Glyph::ShadowBottom,
                Some('█') => Glyph::Block,
                Some(_) => Glyph::Other,
            };
            if glyph == Glyph::None {
                out.push(Cell { ch: ' ', fg: to(pulse), bg: to(pulse), bold: false });
                continue;
            }
            let lx = lx as usize;
            let bold = lx > left_w;
            let top_d = (lx as f64 + 0.5 - lw as f64 / 2.0).hypot(ly as f64 * 2.0 - lh as f64);
            let bottom_d = (lx as f64 + 0.5 - lw as f64 / 2.0).hypot(ly as f64 * 2.0 + 1.0 - lh as f64);
            let (top_peak, top_glow) = logo_pulse(top_d);
            let (bottom_peak, bottom_glow) = logo_pulse(bottom_d);
            let cell = match glyph {
                Glyph::Shadow => {
                    let bg = mix(shadow, white, top_peak.max(bottom_peak) * 0.18);
                    Cell { ch: ' ', fg: to(bg), bg: to(bg), bold }
                }
                Glyph::ShadowTop => Cell { ch: '▀', fg: to(tint_logo(top_glow, top_peak)), bg: to(mix(shadow, white, bottom_peak * 0.18)), bold },
                Glyph::ShadowBottom => Cell { ch: '▀', fg: to(mix(shadow, white, top_peak * 0.18)), bg: to(pulse), bold },
                Glyph::Block if rgb => Cell { ch: '▀', fg: to(tint_logo(top_glow, top_peak)), bg: to(tint_logo(bottom_glow, bottom_peak)), bold },
                _ => Cell { ch: glyph_char.unwrap_or(' '), fg: to(tint_logo((top_glow + bottom_glow) / 2.0, (top_peak + bottom_peak) / 2.0)), bg: to(pulse), bold },
            };
            out.push(cell);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn still_frame_draws_the_logo_with_half_blocks() {
        let bg = crate::theme::Rgba::rgb(10, 10, 10);
        let cells = super::frame(60, 17, super::STILL, 7, bg, crate::theme::Rgba::rgb(250, 178, 131), crate::theme::Rgba::rgb(160, 160, 160), true);
        let row = |y: usize| cells[y * 60..(y + 1) * 60].iter().map(|c| c.ch).collect::<String>();
        // Logo 9 wide, centered at x = 25; its first row is blank.
        assert_eq!(row(8).trim(), "▀▀▀▀ ▀▀▀▀");
        assert_eq!(row(8).find('▀').map(|b| row(8)[..b].chars().count()), Some(25));
        assert!(row(10).trim().starts_with("▀▀▀▀"));
        // Away from the logo every cell is a blank pulse.
        assert!(cells[0].ch == ' ' && cells[0].fg == cells[0].bg);
    }
}
