//! RGBA colors and OKLCH math (`theme/src/tui/color.ts`, `tui/src/theme/color.ts`).

use ratatui::style::Color as TuiColor;

/// A resolved theme color. `src` carries hue provenance `(hue index, step position)`, the Rust
/// equivalent of OpenTUI object identity used by `increase`/`decrease`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
    pub src: Option<(u16, u8)>,
}

impl Rgba {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Rgba { r, g, b, a: 255, src: None }
    }
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Rgba { r, g, b, a, src: None }
    }
    pub const TRANSPARENT: Rgba = Rgba::rgba(0, 0, 0, 0);

    /// `RGBA.fromHex`: `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`.
    pub fn from_hex(s: &str) -> Option<Rgba> {
        let h = s.strip_prefix('#').unwrap_or(s);
        let p = |x: &str| u8::from_str_radix(x, 16).ok();
        let d = |c: char| -> Option<u8> {
            let v = c.to_digit(16)? as u8;
            Some(v * 17)
        };
        // Byte slicing below needs ASCII (a non-hex character is never a match anyway).
        if !h.is_ascii() {
            return None;
        }
        let cs: Vec<char> = h.chars().collect();
        match cs.len() {
            3 => Some(Rgba::rgb(d(cs[0])?, d(cs[1])?, d(cs[2])?)),
            4 => Some(Rgba::rgba(d(cs[0])?, d(cs[1])?, d(cs[2])?, d(cs[3])?)),
            6 => Some(Rgba::rgb(p(&h[0..2])?, p(&h[2..4])?, p(&h[4..6])?)),
            8 => Some(Rgba::rgba(p(&h[0..2])?, p(&h[2..4])?, p(&h[4..6])?, p(&h[6..8])?)),
            _ => None,
        }
    }

    pub fn is_hex(s: &str) -> bool {
        let Some(h) = s.strip_prefix('#') else { return false };
        matches!(h.len(), 3 | 4 | 6 | 8) && h.chars().all(|c| c.is_ascii_hexdigit())
    }

    pub fn floats(&self) -> (f64, f64, f64, f64) {
        (self.r as f64 / 255.0, self.g as f64 / 255.0, self.b as f64 / 255.0, self.a as f64 / 255.0)
    }

    pub fn same_value(&self, o: &Rgba) -> bool {
        self.r == o.r && self.g == o.g && self.b == o.b && self.a == o.a
    }

    pub fn without_src(mut self) -> Rgba {
        self.src = None;
        self
    }

    pub fn hex(&self) -> String {
        if self.a == 255 {
            format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            format!("#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
        }
    }

    pub fn luminance(&self) -> f64 {
        let (r, g, b, _) = self.floats();
        0.299 * r + 0.587 * g + 0.114 * b
    }

    /// With a new alpha (0..1), used for `fg @ alpha 0.6` effects.
    pub fn with_alpha(mut self, a: f64) -> Rgba {
        self.a = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
        self.src = None;
        self
    }

    /// Convert to a terminal color. Alpha is ignored here; compositing happens before.
    pub fn tui(&self) -> TuiColor {
        TuiColor::Rgb(self.r, self.g, self.b)
    }

    /// Terminal color, or `Reset` (terminal default) for fully transparent colors.
    pub fn tui_or_reset(&self) -> TuiColor {
        if self.a == 0 { TuiColor::Reset } else { self.tui() }
    }
}

/// `tint(base, overlay, t)`: per channel `round((b + (o-b)*t)*255)`, alpha 255.
pub fn tint(base: Rgba, overlay: Rgba, t: f64) -> Rgba {
    let (br, bg, bb, _) = base.floats();
    let (or, og, ob, _) = overlay.floats();
    let mix = |b: f64, o: f64| ((b + (o - b) * t) * 255.0).round().clamp(0.0, 255.0) as u8;
    Rgba::rgb(mix(br, or), mix(bg, og), mix(bb, ob))
}

/// Alpha-composite `top` (using its alpha) over `bottom`.
pub fn blend(bottom: Rgba, top: Rgba) -> Rgba {
    let a = top.a as f64 / 255.0;
    tint(bottom, top.with_alpha(1.0), a)
}

#[derive(Clone, Copy, Debug)]
pub struct Oklch {
    pub l: f64,
    pub c: f64,
    pub h: f64,
}

fn srgb_to_linear(v: f64) -> f64 {
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

fn linear_to_srgb(v: f64) -> f64 {
    if v <= 0.0031308 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

pub fn rgb_to_oklch(red: f64, green: f64, blue: f64) -> Oklch {
    let lr = srgb_to_linear(red);
    let lg = srgb_to_linear(green);
    let lb = srgb_to_linear(blue);
    let l_root = (0.4122214708 * lr + 0.5363325363 * lg + 0.0514459929 * lb).cbrt();
    let m_root = (0.2119034982 * lr + 0.6806995451 * lg + 0.1073969566 * lb).cbrt();
    let s_root = (0.0883024619 * lr + 0.2817188376 * lg + 0.6299787005 * lb).cbrt();
    let lightness = 0.2104542553 * l_root + 0.793617785 * m_root - 0.0040720468 * s_root;
    let a = 1.9779984951 * l_root - 2.428592205 * m_root + 0.4505937099 * s_root;
    let b = 0.0259040371 * l_root + 0.7827717662 * m_root - 0.808675766 * s_root;
    let chroma = (a * a + b * b).sqrt();
    let angle = b.atan2(a) * (180.0 / std::f64::consts::PI);
    Oklch { l: lightness, c: chroma, h: if angle < 0.0 { angle + 360.0 } else { angle } }
}

fn oklch_to_rgb(c: Oklch) -> (f64, f64, f64) {
    let a = c.c * (c.h * std::f64::consts::PI / 180.0).cos();
    let b = c.c * (c.h * std::f64::consts::PI / 180.0).sin();
    let l_root = c.l + 0.3963377774 * a + 0.2158037573 * b;
    let m_root = c.l - 0.1055613458 * a - 0.0638541728 * b;
    let s_root = c.l - 0.0894841775 * a - 1.291485548 * b;
    let l = l_root * l_root * l_root;
    let m = m_root * m_root * m_root;
    let s = s_root * s_root * s_root;
    (
        linear_to_srgb(4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s),
        linear_to_srgb(-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s),
        linear_to_srgb(-0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s),
    )
}

fn in_gamut(rgb: (f64, f64, f64)) -> bool {
    let ok = |v: f64| (0.0..=1.0).contains(&v);
    ok(rgb.0) && ok(rgb.1) && ok(rgb.2)
}

fn fit_oklch(c: Oklch) -> Oklch {
    let base = Oklch { l: c.l.clamp(0.0, 1.0), c: c.c.max(0.0), h: ((c.h % 360.0) + 360.0) % 360.0 };
    if in_gamut(oklch_to_rgb(base)) {
        return base;
    }
    for i in 0..24 {
        let next = Oklch { c: base.c * 0.9f64.powi(i + 1), ..base };
        if in_gamut(oklch_to_rgb(next)) {
            return next;
        }
    }
    Oklch { c: 0.0, ..base }
}

/// JS `Math.round` (round half up toward +inf).
pub fn js_round(v: f64) -> f64 {
    (v + 0.5).floor()
}

pub fn oklch_to_hex(c: Oklch) -> String {
    let (r, g, b) = oklch_to_rgb(fit_oklch(c));
    let h = |v: f64| js_round(v.clamp(0.0, 1.0) * 255.0) as u8;
    format!("#{:02x}{:02x}{:02x}", h(r), h(g), h(b))
}

pub fn to_oklch(c: &Rgba) -> Oklch {
    rgb_to_oklch(c.r as f64 / 255.0, c.g as f64 / 255.0, c.b as f64 / 255.0)
}

/// ANSI 256 color index → RGB (`v1-migrate.ts:470-502`).
pub fn ansi(code: i64) -> Rgba {
    const BASE: [&str; 16] = [
        "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0", "#808080", "#ff0000",
        "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
    ];
    if code < 0 {
        return Rgba::rgb(0, 0, 0);
    }
    if code < 16 {
        return Rgba::from_hex(BASE[code as usize]).unwrap();
    }
    if code < 232 {
        let i = code - 16;
        let v = |p: i64| if p == 0 { 0 } else { (p * 40 + 55) as u8 };
        return Rgba::rgb(v(i / 36), v((i / 6) % 6), v(i % 6));
    }
    if code < 256 {
        let g = ((code - 232) * 10 + 8) as u8;
        return Rgba::rgb(g, g, g);
    }
    Rgba::rgb(0, 0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parse() {
        assert_eq!(Rgba::from_hex("#fff").unwrap().hex(), "#ffffff");
        assert_eq!(Rgba::from_hex("#0a0a0a").unwrap().hex(), "#0a0a0a");
        assert_eq!(Rgba::from_hex("#11223344").unwrap().hex(), "#11223344");
        assert!(Rgba::from_hex("#zz0000").is_none());
    }

    /// Six or eight characters with a multibyte one used to slice inside it and panic (agent
    /// colors from the server are parsed on every frame).
    #[test]
    fn hex_parse_non_ascii() {
        assert!(Rgba::from_hex("#aé1234").is_none());
        assert!(Rgba::from_hex("aé1234").is_none());
        assert!(Rgba::from_hex("#1é345678").is_none());
        assert!(Rgba::from_hex("#é1é").is_none());
    }

    #[test]
    fn oklch_roundtrip() {
        let c = Rgba::from_hex("#fab283").unwrap();
        let o = to_oklch(&c);
        assert_eq!(oklch_to_hex(o), "#fab283");
    }

    #[test]
    fn tinting() {
        let a = Rgba::rgb(0, 0, 0);
        let b = Rgba::rgb(255, 255, 255);
        assert_eq!(tint(a, b, 0.5).hex(), "#808080");
        assert_eq!(ansi(196).hex(), "#ff0000");
        assert_eq!(ansi(244).hex(), "#808080");
    }
}
