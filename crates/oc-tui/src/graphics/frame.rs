//! Image placements in a frame and what they send to the terminal (OpenTUI `ImageRenderable.renderSelf`,
//! `OptimizedBuffer.drawImage` and the image half of `CliRenderer.prepareRenderFrame`).
//!
//! Drawing an image reserves its cells: they get [`MARKER`] as their symbol and remember the placement. Anything
//! drawn later over a cell (a dialog) replaces the marker, so when the frame is done [`Graphics::finish`] knows
//! which cells still show each image:
//!
//! - text placements (`blocks`) become quadrant characters;
//! - Kitty and Sixel cells become blank (default colors) and are left out of the cell diff, since the image
//!   covers them. Kitty images are sent once per placement and placed again only when their geometry or the
//!   cells they show change; cells a dialog covers split the placement into one placement per uncovered run.
//!   A new or moved placement first blanks its cells. Sixel pixels are the cells, so a Sixel is painted again
//!   (after blanking) when it is new, moved, changed or uncovered, and the dialog cells over it are drawn again
//!   after it.
//!
//! The escapes are written before the cell diff of the same frame.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

use super::caps::{Caps, Protocol, Requested};
use super::decode::Picture;
use super::{blocks, kitty, resize, sixel};

/// The symbol of a reserved cell until the frame is finished (a private-use character nothing else draws).
pub const MARKER: &str = "\u{10FFFD}";

/// How an image fills its box (`ImageFit`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// Whole image, centered, aspect kept.
    Fit,
    /// The whole box, aspect kept, the source cropped around its center.
    Cover,
    /// The whole box, stretched.
    Fill,
}

/// One drawn image (`ImagePlacement`).
#[derive(Clone, Debug)]
pub struct Placement {
    pub id: u32,
    pub picture: Arc<Picture>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub requested: Requested,
}

impl Placement {
    fn src(&self) -> (u32, u32, u32, u32) {
        (self.source_x, self.source_y, self.source_width, self.source_height)
    }
}

/// What the terminal shows for a placement after a frame (`CommittedImage`).
#[derive(Clone, Debug)]
struct Committed {
    id: u32,
    handle: u32,
    protocol: Protocol,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    pixel_width: u32,
    pixel_height: u32,
    src: (u32, u32, u32, u32),
    intact: bool,
    hash: u64,
    runs: u32,
    copied: bool,
    reserved: HashSet<(u16, u16)>,
}

/// Terminal graphics state for one screen.
pub struct Graphics {
    pub caps: Caps,
    /// Terminal size in cells (the resolution's denominator).
    pub term: (u16, u16),
    placements: Vec<Placement>,
    cells: HashMap<(u16, u16), u32>,
    committed: Vec<Committed>,
    /// Kitty image ids are `salt + placement id`, so two programs on one terminal rarely collide.
    pub salt: u32,
    invalidated: bool,
    area: Rect,
    sixels: HashMap<(u32, (u32, u32, u32, u32), u32, u32), Option<String>>,
    /// Bytes of the last finished frame (tests).
    pub last: String,
}

impl Graphics {
    pub fn new(caps: Caps) -> Graphics {
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0) as u64 ^ std::process::id() as u64;
        let salt = 1 + (seed % (u32::MAX - kitty::IMAGE_ID_MASK - 1) as u64) as u32;
        Graphics {
            caps,
            term: (0, 0),
            placements: vec![],
            cells: HashMap::new(),
            committed: vec![],
            salt,
            invalidated: true,
            area: Rect::default(),
            sixels: HashMap::new(),
            last: String::new(),
        }
    }

    /// The terminal is `cols` x `rows` cells.
    pub fn set_term(&mut self, cols: u16, rows: u16) {
        if self.caps.measured == (0, 0) {
            self.caps.measured = (cols, rows);
        }
        self.caps.resize(cols, rows);
        self.term = (cols, rows);
    }

    /// The window's pixel size, when known for the current terminal size.
    fn resolution(&self) -> Option<(u32, u32)> {
        let (w, h) = self.caps.resolution?;
        (self.term.0 > 0 && self.term.1 > 0 && w > 0 && h > 0).then_some((w, h))
    }

    /// `cellAspectRatio`: a cell's height over its width (2 without a pixel size).
    pub fn cell_aspect(&self) -> f64 {
        match self.resolution() {
            Some((w, h)) => {
                let (cw, ch) = (w as f64 / self.term.0 as f64, h as f64 / self.term.1 as f64);
                if cw > 0.0 && ch > 0.0 { ch / cw } else { 2.0 }
            }
            None => 2.0,
        }
    }

    /// `getFittedSize`.
    pub fn fitted_size(&self, fit: Fit, target_w: u32, target_h: u32, source_w: u32, source_h: u32) -> (u32, u32) {
        if source_w == 0 || source_h == 0 || target_w == 0 || target_h == 0 {
            return (0, 0);
        }
        if fit == Fit::Fill {
            return (target_w, target_h);
        }
        let display = source_w as f64 / source_h as f64 * self.cell_aspect();
        let (tw, th) = (target_w as f64, target_h as f64);
        let scale = if fit == Fit::Fit { (tw / display).min(th) } else { (tw / display).max(th) };
        (((display * scale).round() as u32).max(1), (scale.round() as u32).max(1))
    }

    /// Pixels for `cells` columns and rows, or 0 without a pixel size.
    fn pixels(&self, cols: u32, rows: u32) -> (u32, u32) {
        match self.resolution() {
            Some((w, h)) => (
                ((cols as f64 * w as f64 / self.term.0 as f64).round() as u32).max(1),
                ((rows as f64 * h as f64 / self.term.1 as f64).round() as u32).max(1),
            ),
            None => (0, 0),
        }
    }

    /// The protocol a placement draws with.
    pub fn protocol(&self, p: &Placement) -> Protocol {
        self.caps.resolve(p.requested, (p.pixel_width, p.pixel_height))
    }

    /// `ImageRenderable.renderSelf`: draw `picture` in the box `area` (clipped to `clip`).
    pub fn draw(&mut self, buf: &mut Buffer, picture: &Arc<Picture>, area: Rect, fit: Fit, clip: Rect) {
        self.draw_at(buf, picture, area.x as i32, area.y as i32, area.width as u32, area.height as u32, fit, clip);
    }

    /// [`Graphics::draw`] for a box that may start above or left of the screen.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_at(&mut self, buf: &mut Buffer, picture: &Arc<Picture>, bx: i32, by: i32, width: u32, height: u32, fit: Fit, clip: Rect) {
        if width == 0 || height == 0 || picture.width == 0 || picture.height == 0 {
            return;
        }
        let (fw, fh) = if fit == Fit::Cover { (width, height) } else { self.fitted_size(fit, width, height, picture.width, picture.height) };
        if fw == 0 || fh == 0 {
            return;
        }
        let x = bx + ((width - fw) / 2) as i32;
        let y = by + ((height - fh) / 2) as i32;
        let (pw, ph) = self.pixels(fw, fh);
        let (mut sx, mut sy, mut sw, mut sh) = (0u32, 0u32, picture.width, picture.height);
        if fit == Fit::Cover {
            let target = width as f64 / (height as f64 * self.cell_aspect());
            let source = sw as f64 / sh as f64;
            if source > target {
                let nw = ((sh as f64 * target).round() as u32).max(1);
                sx += (sw - nw.min(sw)) / 2;
                sw = nw.min(sw);
            } else {
                let nh = ((sw as f64 / target).round() as u32).max(1);
                sy += (sh - nh.min(sh)) / 2;
                sh = nh.min(sh);
            }
        }
        self.draw_image(buf, picture, x, y, fw, fh, pw, ph, (sx, sy, sw, sh), clip);
    }

    /// `OptimizedBuffer.drawImage`: clip, record the placement and reserve its cells.
    #[allow(clippy::too_many_arguments)]
    fn draw_image(&mut self, buf: &mut Buffer, picture: &Arc<Picture>, x: i32, y: i32, width: u32, height: u32, pw: u32, ph: u32, src: (u32, u32, u32, u32), clip: Rect) {
        let (source_x, source_y, source_width, source_height) = src;
        if width == 0 || height == 0 || source_width == 0 || source_height == 0 || self.placements.len() as u32 >= kitty::IMAGE_ID_MASK {
            return;
        }
        let area = buf.area.intersection(clip);
        let cx0 = (x as i64).max(area.x as i64);
        let cy0 = (y as i64).max(area.y as i64);
        let cx1 = (x as i64 + width as i64).min(area.x as i64 + area.width as i64);
        let cy1 = (y as i64 + height as i64).min(area.y as i64 + area.height as i64);
        if cx0 >= cx1 || cy0 >= cy1 {
            return;
        }
        let (left, top, right, bottom) = ((cx0 - x as i64) as u64, (cy0 - y as i64) as u64, (cx1 - x as i64) as u64, (cy1 - y as i64) as u64);
        let (w, h) = (width as u64, height as u64);
        let csx = source_x + (left * source_width as u64 / w) as u32;
        let csy = source_y + (top * source_height as u64 / h) as u32;
        let esx = source_x + ((right * source_width as u64 + w - 1) / w) as u32;
        let esy = source_y + ((bottom * source_height as u64 + h - 1) / h) as u32;
        let (cw, ch) = ((cx1 - cx0) as u32, (cy1 - cy0) as u32);
        let cpw = if pw == 0 { 0 } else { ((cw as u64 * pw as u64 + w - 1) / w) as u32 };
        let cph = if ph == 0 { 0 } else { ((ch as u64 * ph as u64 + h - 1) / h) as u32 };
        let id = self.placements.len() as u32 + 1;
        self.placements.push(Placement {
            id,
            picture: picture.clone(),
            x: cx0 as i32,
            y: cy0 as i32,
            width: cw,
            height: ch,
            pixel_width: cpw,
            pixel_height: cph,
            source_x: csx,
            source_y: csy,
            source_width: esx - csx,
            source_height: esy - csy,
            requested: Requested::Auto,
        });
        for yy in cy0..cy1 {
            for xx in cx0..cx1 {
                let pos = (xx as u16, yy as u16);
                if let Some(cell) = buf.cell_mut(pos) {
                    cell.set_symbol(MARKER);
                    self.cells.insert(pos, id);
                }
            }
        }
    }

    /// The placements drawn so far in this frame.
    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }

    /// The next frame repaints and resends every image (after a resize or a cleared screen).
    pub fn invalidate(&mut self) {
        self.invalidated = true;
    }

    /// Kitty deletes for every image on screen (`writeShutdownImage`), when the program leaves the screen.
    pub fn shutdown(&mut self) -> String {
        let mut out = String::new();
        for c in &self.committed {
            if c.protocol == Protocol::Kitty {
                kitty::delete(&mut out, self.salt + c.id, None, true, self.caps.tmux());
            }
        }
        self.committed.clear();
        out
    }

    fn reserved(&self, buf: &Buffer, p: &Placement, x: u32, y: u32) -> bool {
        let pos = ((p.x + x as i32) as u16, (p.y + y as i32) as u16);
        self.cells.get(&pos) == Some(&p.id) && buf.cell(pos).is_some_and(|c| c.symbol() == MARKER)
    }

    /// Finish the frame: turn reserved cells into what the cell diff writes and return the escapes to send first.
    pub fn finish(&mut self, buf: &mut Buffer) -> String {
        if buf.area != self.area {
            self.area = buf.area;
            self.invalidated = true;
        }
        let tmux = self.caps.tmux();
        let placements = std::mem::take(&mut self.placements);
        let protocols: Vec<Protocol> = placements.iter().map(|p| self.protocol(p)).collect();
        // Which cells still show each placement.
        let reserved: Vec<HashSet<(u16, u16)>> = placements
            .iter()
            .map(|p| {
                let mut set = HashSet::new();
                for y in 0..p.height {
                    for x in 0..p.width {
                        if self.reserved(buf, p, x, y) {
                            set.insert(((p.x + x as i32) as u16, (p.y + y as i32) as u16));
                        }
                    }
                }
                set
            })
            .collect();
        let old = std::mem::take(&mut self.committed);
        let previous: &[Committed] = if self.invalidated { &[] } else { &old };
        let previous_for = |id: u32| previous.iter().find(|c| c.id == id);
        let mut out = String::new();
        let mut committed = vec![];

        // Kitty: free images whose placement is gone or shows something else.
        for c in &old {
            if c.protocol != Protocol::Kitty {
                continue;
            }
            let kept = placements.iter().zip(&protocols).any(|(p, pr)| p.id == c.id && p.picture.handle == c.handle && *pr == Protocol::Kitty);
            if !kept {
                kitty::delete(&mut out, self.salt + c.id, None, true, tmux);
            }
        }

        let mut clears: Vec<(u16, u16)> = vec![];
        let mut sixel_out = String::new();
        let mut repaint: Vec<(u16, u16)> = vec![];
        for (i, p) in placements.iter().enumerate() {
            let protocol = protocols[i];
            let set = &reserved[i];
            let prev = previous_for(p.id).filter(|c| protocol != Protocol::Kitty || (c.protocol == Protocol::Kitty && c.handle == p.picture.handle));
            let intact = set.len() as u32 == p.width * p.height;
            let hash = {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                for y in 0..p.height {
                    for x in 0..p.width {
                        set.contains(&((p.x + x as i32) as u16, (p.y + y as i32) as u16)).hash(&mut h);
                    }
                }
                h.finish()
            };
            let mut entry = Committed {
                id: p.id,
                handle: p.picture.handle,
                protocol,
                x: p.x,
                y: p.y,
                width: p.width,
                height: p.height,
                pixel_width: p.pixel_width,
                pixel_height: p.pixel_height,
                src: p.src(),
                intact,
                hash,
                runs: 0,
                copied: false,
                reserved: set.clone(),
            };
            let geometry_changed = prev.is_none_or(|c| {
                c.protocol != protocol || c.x != p.x || c.y != p.y || c.width != p.width || c.height != p.height || c.pixel_width != p.pixel_width || c.pixel_height != p.pixel_height || c.src != p.src()
            });
            match protocol {
                Protocol::Blocks => {
                    for &(x, y) in set {
                        let (cx, cy) = (x as i32 - p.x, y as i32 - p.y);
                        let (ch, fg, bg) = blocks::quadrant(blocks::samples(&p.picture, p.src(), p.width, p.height, cx as u32, cy as u32));
                        if let Some(cell) = buf.cell_mut((x, y)) {
                            let under = match cell.bg {
                                Color::Rgb(r, g, b) => [r, g, b],
                                _ => [0, 0, 0],
                            };
                            let mix = |c: [u8; 4]| {
                                let a = c[3] as u32;
                                let m = |i: usize| ((c[i] as u32 * a + under[i] as u32 * (255 - a) + 127) / 255) as u8;
                                Color::Rgb(m(0), m(1), m(2))
                            };
                            cell.set_symbol(ch.encode_utf8(&mut [0; 4]));
                            cell.fg = mix(fg);
                            cell.bg = mix(bg);
                            cell.modifier = Modifier::empty();
                        }
                    }
                }
                Protocol::Kitty | Protocol::Sixel => {
                    for &pos in set {
                        if let Some(cell) = buf.cell_mut(pos) {
                            cell.set_symbol(" ");
                            cell.fg = Color::Reset;
                            cell.bg = Color::Reset;
                            cell.modifier = Modifier::empty();
                            cell.set_diff_option(CellDiffOption::Skip);
                        }
                    }
                }
            }
            if protocol == Protocol::Kitty {
                let id = self.salt + p.id;
                let downscaled = downscale(p);
                entry.copied = downscaled;
                let coverage_changed = prev.is_none_or(|c| c.intact != intact || c.hash != hash);
                let retransmit = prev.is_some_and(|c| c.copied != downscaled || (downscaled && (c.src != p.src() || c.pixel_width != p.pixel_width || c.pixel_height != p.pixel_height)));
                if geometry_changed {
                    clears.extend(set.iter().copied());
                }
                if prev.is_some() && !retransmit && !coverage_changed && !geometry_changed {
                    entry.runs = prev.map(|c| c.runs).unwrap_or(0);
                    committed.push(entry);
                    continue;
                }
                if retransmit {
                    kitty::delete(&mut out, id, None, true, tmux);
                } else if let Some(c) = prev {
                    if c.intact {
                        kitty::delete(&mut out, id, Some(p.id), false, tmux);
                    } else {
                        for run in 0..c.runs {
                            kitty::delete(&mut out, id, Some(kitty::RUN_ID_MIN + run), false, tmux);
                        }
                    }
                }
                if prev.is_none() || retransmit {
                    if downscaled {
                        let crop = p.picture.extract(p.source_x, p.source_y, p.source_width, p.source_height);
                        kitty::transmit(&mut out, &resize::area(&crop, p.pixel_width, p.pixel_height), id, tmux);
                    } else {
                        kitty::transmit(&mut out, &p.picture, id, tmux);
                    }
                }
                let (sx0, sy0, sw, sh) = if downscaled { (0, 0, p.pixel_width, p.pixel_height) } else { p.src() };
                if !intact {
                    entry.runs = runs(&mut out, p, set, id, (sx0, sy0, sw, sh), tmux);
                } else {
                    let place = kitty::Place { x: p.x as u32, y: p.y as u32, width: p.width, height: p.height, source_x: sx0, source_y: sy0, source_width: sw, source_height: sh };
                    kitty::placement(&mut out, id, p.id, place, kitty::placement_z(p.id), tmux);
                }
                committed.push(entry);
                continue;
            }
            if protocol == Protocol::Sixel {
                let exposed = prev.is_some_and(|c| set.iter().any(|pos| !c.reserved.contains(pos)));
                let changed = prev.is_some_and(|c| c.handle != p.picture.handle);
                if geometry_changed || changed || exposed {
                    clears.extend(set.iter().copied());
                    let key = (p.picture.handle, p.src(), p.pixel_width, p.pixel_height);
                    let payload = self
                        .sixels
                        .entry(key)
                        .or_insert_with(|| {
                            let crop = if p.src() == (0, 0, p.picture.width, p.picture.height) {
                                None
                            } else {
                                Some(p.picture.extract(p.source_x, p.source_y, p.source_width, p.source_height))
                            };
                            let base = crop.as_ref().unwrap_or(&p.picture);
                            if (p.pixel_width, p.pixel_height) != (base.width, base.height) {
                                sixel::encode(&resize::area(base, p.pixel_width, p.pixel_height))
                            } else {
                                sixel::encode(base)
                            }
                        })
                        .clone();
                    if self.sixels.len() > 256 {
                        self.sixels.clear();
                    }
                    if let Some(payload) = payload {
                        let _ = write!(sixel_out, "\x1b[{};{}H", p.y + 1, p.x + 1);
                        sixel::framed(&mut sixel_out, &payload, tmux);
                    }
                    // Cells drawn over the image are painted again after it.
                    for y in 0..p.height {
                        for x in 0..p.width {
                            let pos = ((p.x + x as i32) as u16, (p.y + y as i32) as u16);
                            if !set.contains(&pos) {
                                repaint.push(pos);
                            }
                        }
                    }
                }
                committed.push(entry);
                continue;
            }
            committed.push(entry);
        }
        // Blank the cells of new or moved native placements, as runs.
        clears.sort_by_key(|&(x, y)| (y, x));
        clears.dedup();
        let mut run: Option<(u16, u16)> = None;
        for (x, y) in clears {
            if run != Some((y, x)) {
                let _ = write!(out, "\x1b[0m\x1b[{};{}H", y + 1, x + 1);
            }
            out.push(' ');
            run = Some((y, x + 1));
        }
        out.push_str(&sixel_out);
        for pos in repaint {
            if let Some(cell) = buf.cell_mut(pos) {
                cell.set_diff_option(CellDiffOption::AlwaysUpdate);
            }
        }
        self.committed = committed;
        self.cells.clear();
        self.invalidated = false;
        self.last = out.clone();
        out
    }
}

/// `kittyDownscaleApplies`: a source with at least four times the placement's pixels is sent at the placement's
/// pixel size.
fn downscale(p: &Placement) -> bool {
    let pixel = p.pixel_width as u64 * p.pixel_height as u64;
    pixel > 0 && p.source_width as u64 * p.source_height as u64 >= pixel * 4
}

/// `kittySourceSpan`.
fn source_span(origin: u32, length: u32, offset: u32, count: u32, span: u32) -> Option<(u32, u32)> {
    if span == 0 || count == 0 || length == 0 || offset > span || count > span - offset {
        return None;
    }
    let start = origin as u64 + (offset as u64 * length as u64) / span as u64;
    let end = (origin as u64 + ((offset + count) as u64 * length as u64 + span as u64 - 1) / span as u64).min(origin as u64 + length as u64);
    (end > start).then_some((start as u32, (end - start) as u32))
}

/// `writeKittyReservationRuns`: one placement per uncovered run, rows with the same coverage sharing one.
fn runs(out: &mut String, p: &Placement, set: &HashSet<(u16, u16)>, id: u32, src: (u32, u32, u32, u32), tmux: bool) -> u32 {
    let at = |x: u32, y: u32| set.contains(&((p.x + x as i32) as u16, (p.y + y as i32) as u16));
    let same = |a: u32, b: u32| (0..p.width).all(|x| at(x, a) == at(x, b));
    let z = kitty::placement_z(p.id);
    let mut index = 0u32;
    let mut band = 0u32;
    while band < p.height {
        let mut end = band + 1;
        while end < p.height && same(band, end) {
            end += 1;
        }
        if let Some((sy, sh)) = source_span(src.1, src.3, band, end - band, p.height) {
            let mut x = 0;
            while x < p.width {
                if !at(x, band) {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < p.width && at(x, band) {
                    x += 1;
                }
                if let Some((sx, sw)) = source_span(src.0, src.2, start, x - start, p.width) {
                    let place = kitty::Place { x: p.x as u32 + start, y: p.y as u32 + band, width: x - start, height: end - band, source_x: sx, source_y: sy, source_width: sw, source_height: sh };
                    kitty::placement(out, id, kitty::RUN_ID_MIN + index, place, z, tmux);
                    index += 1;
                }
            }
        }
        band = end;
    }
    index
}
