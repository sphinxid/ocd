//! Cell buffers with OpenTUI color semantics (`lib/RGBA.ts`, native `buffer.zig`): colors carry an
//! intent (rgb, indexed palette slot, terminal default) and drawing alpha-blends like `setCellWithAlphaBlending`.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Intent {
    Rgb,
    Indexed(u8),
    Default,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
    pub intent: Intent,
}

impl Color {
    pub const TRANSPARENT: Color = Color { r: 0, g: 0, b: 0, a: 0, intent: Intent::Rgb };
    pub const WHITE: Color = Color { r: 255, g: 255, b: 255, a: 255, intent: Intent::Rgb };

    pub fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { r, g, b, a: 255, intent: Intent::Rgb }
    }
    pub fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color { r, g, b, a, intent: Intent::Rgb }
    }
    pub fn indexed(slot: u8, snapshot: Color) -> Color {
        Color { intent: Intent::Indexed(slot), ..snapshot }
    }
    pub fn default_fg(snapshot: Color) -> Color {
        Color { intent: Intent::Default, ..snapshot }
    }
    pub fn default_bg(snapshot: Color) -> Color {
        Color { intent: Intent::Default, ..snapshot }
    }
    pub fn from_theme(c: crate::theme::Rgba) -> Color {
        Color { r: c.r, g: c.g, b: c.b, a: c.a, intent: Intent::Rgb }
    }
    pub fn from_hex(hex: &str) -> Option<Color> {
        crate::theme::Rgba::from_hex(hex).map(Color::from_theme)
    }
    pub fn with_alpha(self, a: u8) -> Color {
        Color { a, ..self }
    }
    pub fn same_rgb(&self, o: &Color) -> bool {
        self.r == o.r && self.g == o.g && self.b == o.b && self.a == o.a
    }
}

/// Text attribute bits (`TextAttributes`).
pub mod attr {
    pub const BOLD: u8 = 1;
    pub const DIM: u8 = 2;
    pub const ITALIC: u8 = 4;
    pub const UNDERLINE: u8 = 8;
    pub const BLINK: u8 = 16;
    pub const INVERSE: u8 = 32;
    pub const HIDDEN: u8 = 64;
    pub const STRIKETHROUGH: u8 = 128;
}

/// What a cell holds: nothing (never written), a grapheme, or the continuation of a wide grapheme.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Glyph {
    Empty,
    Text(String),
    Cont,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub glyph: Glyph,
    pub fg: Color,
    pub bg: Color,
    pub attrs: u8,
}

impl Cell {
    pub fn empty() -> Cell {
        Cell { glyph: Glyph::Empty, fg: Color::TRANSPARENT, bg: Color::TRANSPARENT, attrs: 0 }
    }
    pub fn text(&self) -> &str {
        match &self.glyph {
            Glyph::Text(s) => s,
            _ => "",
        }
    }
    pub fn is_default_space(&self) -> bool {
        matches!(&self.glyph, Glyph::Text(s) if s == " ")
    }
}

fn mul_div255(a: u32, b: u32) -> u32 {
    (a * b + 127) / 255
}

/// `blendColors(src, dst)`.
pub fn blend(src: Color, dst: Color) -> Color {
    let sa = src.a as u32;
    if sa == 0 {
        return dst;
    }
    if sa == 255 {
        return Color::rgb(src.r, src.g, src.b);
    }
    let da = dst.a as u32;
    let inv = 255 - sa;
    let out_a = sa + mul_div255(da, inv);
    if out_a == 0 {
        return Color::TRANSPARENT;
    }
    if da == 255 {
        let c = |s: u8, d: u8| ((s as u32 * sa + d as u32 * inv + 127) / 255) as u8;
        return Color::rgb(c(src.r, dst.r), c(src.g, dst.g), c(src.b, dst.b));
    }
    let c = |s: u8, d: u8| ((s as u32 * sa + mul_div255(d as u32 * da, inv) + out_a / 2) / out_a) as u8;
    Color::rgba(c(src.r, dst.r), c(src.g, dst.g), c(src.b, dst.b), out_a as u8)
}

pub fn grapheme_width(g: &str) -> usize {
    if g == "\t" {
        return 2;
    }
    if g.chars().all(|c| c.is_control()) {
        return 0;
    }
    UnicodeWidthStr::width(g)
}

pub fn str_width(s: &str) -> usize {
    s.graphemes(true).map(grapheme_width).sum()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Buffer {
    pub width: u16,
    pub height: u16,
    pub cells: Vec<Cell>,
    /// Clip rectangle stack (x, y, w, h).
    clips: Vec<(i32, i32, i32, i32)>,
}

impl Buffer {
    pub fn new(width: u16, height: u16) -> Buffer {
        Buffer { width, height, cells: vec![Cell::empty(); width as usize * height as usize], clips: Vec::new() }
    }

    pub fn get(&self, x: u16, y: u16) -> &Cell {
        &self.cells[y as usize * self.width as usize + x as usize]
    }

    fn idx(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return None;
        }
        if let Some((cx, cy, cw, ch)) = self.clips.last()
            && (x < *cx || y < *cy || x >= cx + cw || y >= cy + ch)
        {
            return None;
        }
        Some(y as usize * self.width as usize + x as usize)
    }

    pub fn push_clip(&mut self, x: i32, y: i32, w: i32, h: i32) {
        let (x, y, w, h) = match self.clips.last() {
            Some((px, py, pw, ph)) => {
                let x0 = x.max(*px);
                let y0 = y.max(*py);
                let x1 = (x + w).min(px + pw);
                let y1 = (y + h).min(py + ph);
                (x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
            }
            None => (x, y, w, h),
        };
        self.clips.push((x, y, w, h));
    }

    pub fn pop_clip(&mut self) {
        self.clips.pop();
    }

    /// Set a cell with alpha blending (`setCellWithAlphaBlending`).
    pub fn set_blend(&mut self, x: i32, y: i32, glyph: Glyph, fg: Color, bg: Color, attrs: u8) {
        let Some(i) = self.idx(x, y) else { return };
        if fg.a == 0 && bg.a == 0 {
            return;
        }
        let opaque = fg.a == 255 && bg.a == 255;
        if opaque {
            self.cells[i] = Cell { glyph, fg, bg, attrs };
            return;
        }
        let dest = self.cells[i].clone();
        let blended_bg = if bg.a < 255 { blend(bg, dest.bg) } else { bg };
        let is_space = matches!(&glyph, Glyph::Text(s) if s == " ");
        let dest_width_one = match &dest.glyph {
            Glyph::Text(s) => grapheme_width(s) == 1,
            _ => false,
        };
        let preserve = is_space && !matches!(dest.glyph, Glyph::Empty) && !dest.is_default_space() && dest_width_one;
        let (glyph, fg, attrs) = if preserve {
            (dest.glyph.clone(), blend(bg, dest.fg), dest.attrs)
        } else {
            (glyph, if fg.a < 255 { blend(fg, blended_bg) } else { fg }, attrs)
        };
        self.cells[i] = Cell { glyph, fg, bg: blended_bg, attrs };
    }

    /// Fill a rectangle's background (box `backgroundColor`): spaces with the color.
    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, bg: Color) {
        if bg.a == 0 {
            return;
        }
        for yy in y..y + h {
            for xx in x..x + w {
                self.set_blend(xx, yy, Glyph::Text(" ".into()), Color::WHITE.with_alpha(255), bg, 0);
            }
        }
    }

    /// Draw one line of styled chunks at (x, y) within `max` columns. Returns columns used.
    pub fn draw_chunks(&mut self, x: i32, y: i32, chunks: &[StyledChunk], max: usize) -> usize {
        let mut col = 0usize;
        for ch in chunks {
            for g in ch.text.graphemes(true) {
                let w = grapheme_width(g);
                if g == "\t" {
                    for _ in 0..2 {
                        if col >= max {
                            return col;
                        }
                        self.set_blend(x + col as i32, y, Glyph::Text(" ".into()), ch.fg, ch.bg, ch.attrs);
                        col += 1;
                    }
                    continue;
                }
                if w == 0 {
                    continue;
                }
                if col + w > max {
                    return col;
                }
                self.set_blend(x + col as i32, y, Glyph::Text(g.to_string()), ch.fg, ch.bg, ch.attrs);
                for k in 1..w {
                    if let Some(i) = self.idx(x + (col + k) as i32, y) {
                        self.cells[i].glyph = Glyph::Cont;
                    }
                }
                col += w;
            }
        }
        col
    }

    /// Plain text of a row (continuations skipped, empty cells as spaces), right-trimmed.
    pub fn row_text(&self, y: u16) -> String {
        let mut s = String::new();
        for x in 0..self.width {
            match &self.get(x, y).glyph {
                Glyph::Empty => s.push(' '),
                Glyph::Text(t) => s.push_str(t),
                Glyph::Cont => {}
            }
        }
        s.trim_end().to_string()
    }
}

/// A run of text with resolved colors (`TextChunk` after defaults applied).
#[derive(Clone, Debug, PartialEq)]
pub struct StyledChunk {
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub attrs: u8,
}

impl StyledChunk {
    pub fn new(text: impl Into<String>, fg: Color, bg: Color, attrs: u8) -> StyledChunk {
        StyledChunk { text: text.into(), fg, bg, attrs }
    }
    pub fn plain(text: impl Into<String>, fg: Color) -> StyledChunk {
        StyledChunk { text: text.into(), fg, bg: Color::TRANSPARENT, attrs: 0 }
    }
}

/// `ansi256IndexToRgb(index)`.
pub fn ansi256(index: u8) -> Color {
    const ANSI16: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0x80, 0x00, 0x00),
        (0x00, 0x80, 0x00),
        (0x80, 0x80, 0x00),
        (0x00, 0x00, 0x80),
        (0x80, 0x00, 0x80),
        (0x00, 0x80, 0x80),
        (0xc0, 0xc0, 0xc0),
        (0x80, 0x80, 0x80),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x00, 0x00, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    let i = index as usize;
    if i < 16 {
        let (r, g, b) = ANSI16[i];
        return Color::rgb(r, g, b);
    }
    if i < 232 {
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let c = i - 16;
        return Color::rgb(LEVELS[c / 36], LEVELS[(c / 6) % 6], LEVELS[c % 6]);
    }
    let v = (8 + (i - 232) * 10) as u8;
    Color::rgb(v, v, v)
}
