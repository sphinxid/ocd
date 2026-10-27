//! The OpenCode Go offer shown when the free limit is hit (`src/component/dialog-go-upsell.tsx`):
//! "Free limit reached", the reset time, the animated GO art behind the text, and
//! `don't show again` / `dismiss` / `subscribe`.

use super::go_art;
use super::text::*;
use crate::keymap::Stroke;
use crate::theme::{Rgba, Theme, tint};
use crate::util::locale::width as sw;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use std::time::Instant;

pub const GO_URL: &str = "https://opencode.ai/go";
pub const ACTIONS: [&str; 3] = ["don't show again", "dismiss", "subscribe"];
/// Text sits on a translucent panel (`background.base` at alpha 186) so the rings stay legible.
const TEXT_ALPHA: f64 = 186.0 / 255.0;

#[derive(Clone, Debug)]
pub struct GoUpsell {
    pub reset: Option<String>,
    pub selected: usize,
    pub started: Instant,
    pub animate: bool,
}

impl GoUpsell {
    pub fn new(reset: Option<String>, animate: bool) -> GoUpsell {
        GoUpsell { reset, selected: 2, started: Instant::now(), animate }
    }
    /// Milliseconds into the animation (the still frame when animations are off).
    fn elapsed(&self) -> f64 {
        if self.animate { (self.started.elapsed().as_millis() % 4600) as f64 } else { go_art::STILL }
    }
}

/// What a key does to the offer.
pub enum Key {
    None,
    Close,
    Run(&'static str),
}

pub fn handle_key(g: &mut GoUpsell, s: &Stroke) -> Key {
    let step = |g: &mut GoUpsell, by: isize| g.selected = ((g.selected as isize + by).rem_euclid(ACTIONS.len() as isize)) as usize;
    match s.name.as_str() {
        "return" => Key::Run(ACTIONS[g.selected]),
        "escape" => Key::Close,
        "c" if s.ctrl => Key::Close,
        "left" | "up" => {
            step(g, -1);
            Key::None
        }
        "tab" if s.shift => {
            step(g, -1);
            Key::None
        }
        "right" | "down" | "tab" => {
            step(g, 1);
            Key::None
        }
        _ => Key::None,
    }
}

/// Size class of the offer: narrow below 52 columns, compact (no art) on short terminals.
pub fn layout(area: Rect) -> (bool, bool, bool) {
    let narrow = area.width < 52;
    let short = area.height < 22;
    let compact = short || area.height < if narrow { 26 } else { 22 };
    (narrow, short, compact)
}

/// Whether the offer animates (redraws on its own).
pub fn animating(g: &GoUpsell, area: Rect) -> bool {
    g.animate && !layout(area).2
}

struct Text {
    x: u16,
    y: u16,
    text: String,
    fg: Rgba,
    bold: bool,
    /// `Some`: an opaque background; `None`: the translucent text panel.
    bg: Option<Rgba>,
}

/// Draw the offer as the dialog panel. Returns the panel and the button hit boxes.
pub fn render(buf: &mut Buffer, area: Rect, g: &GoUpsell, base: &Theme) -> (Rect, Vec<(Rect, usize)>) {
    let t = base.dialog();
    let (narrow, short, compact) = layout(area);
    let w = 60u16.min(area.width.saturating_sub(2)).max(10);
    let x0 = area.x + area.width.saturating_sub(w) / 2;
    let inset: u16 = if narrow { 2 } else { 3 };
    let inner = w.saturating_sub(inset * 2) as usize;
    let gap: u16 = if short { 0 } else { 1 };
    let primary = t.background.action.primary.focused;
    let first = format!("You've used today's free messages.{}", g.reset.as_ref().map(|r| format!(" Resets at {r}.")).unwrap_or_default());
    let second = "OpenCode Go gives you reliable access to the best open-source models for $10/month.";
    // A wrapped row keeps the space it broke at (it takes the text background too).
    let wrap_rows = |s: &str| -> Vec<String> {
        let rows: Vec<String> = wrap(&[Seg::new(s, Style::default())], inner.max(1), Wrap::Word).into_iter().map(|l| l.into_iter().map(|s| s.text).collect()).collect();
        let n = rows.len();
        rows.into_iter().enumerate().map(|(i, r)| if i + 1 < n && sw(&r) < inner { format!("{r} ") } else { r }).collect()
    };
    let body: Vec<String> = wrap_rows(&first).into_iter().chain(wrap_rows(second)).collect();
    // Rows relative to the panel top: padding 1, header, gap, body, gap, anchor (7) | link, gap, buttons, padding.
    let mut texts: Vec<Text> = Vec::new();
    let mut row: u16 = 1;
    let header_row = row;
    row += 1 + gap;
    let body_row = row;
    let anchor_row;
    let link_row;
    if compact {
        let top_inset = if short { 1 } else { area.height / 4 + 1 };
        let footer_rows = if narrow { ACTIONS.len() as u16 } else { 1 };
        let gaps = if short { 1 } else { 4 };
        let max_h = area.height.saturating_sub(top_inset + footer_rows + gaps).max(2) as usize;
        let shown = (body.len() + 1).min(max_h);
        row += shown as u16;
        link_row = if shown > body.len() { Some(body_row + body.len() as u16) } else { None };
        anchor_row = None;
    } else {
        row += body.len() as u16 + gap;
        anchor_row = Some(row);
        link_row = Some(row + 5);
        row += 7;
    }
    row += gap;
    let buttons_row = row;
    row += if narrow { ACTIONS.len() as u16 } else { 1 };
    row += if short { 0 } else { 1 };
    let h = row.min(area.height);
    let centered = short;
    let y0 = if centered { area.y + area.height.saturating_sub(h) / 2 } else { area.y + ((area.height as f64) / 4.0).round() as u16 };
    let y0 = y0.min(area.bottom().saturating_sub(h));
    let panel = Rect::new(x0, y0, w, h);
    // Background: the art (or the dialog surface).
    let art: Option<Vec<go_art::Cell>> = if compact {
        None
    } else {
        let logo_base = tint(t.background.base, t.text.base, 0.62);
        let logo_y = anchor_row.map(|r| r as i64).unwrap_or(0);
        Some(go_art::frame(w as usize, h as usize, g.elapsed(), logo_y, t.background.base, primary, logo_base, true))
    };
    let bg_at = |x: u16, y: u16| -> Rgba {
        match &art {
            Some(cells) => cells.get(((y - y0) as usize) * w as usize + (x - x0) as usize).map(|c| c.bg).unwrap_or(t.background.base),
            None => t.background.base,
        }
    };
    match &art {
        Some(cells) => {
            for (i, c) in cells.iter().enumerate() {
                let (x, y) = (x0 + (i % w as usize) as u16, y0 + (i / w as usize) as u16);
                if y >= panel.bottom() {
                    break;
                }
                let mut st = Style::default().fg(c.fg.tui()).bg(c.bg.tui());
                if c.bold {
                    st = st.add_modifier(Modifier::BOLD);
                }
                buf[(x, y)].reset();
                put(buf, x, y, &c.ch.to_string(), st, 1);
            }
        }
        None => fill(buf, panel, t.background.base.tui()),
    }
    // Header.
    texts.push(Text { x: x0 + inset, y: y0 + header_row, text: "Free limit reached".into(), fg: t.text.base, bold: true, bg: None });
    texts.push(Text { x: x0 + w - inset - 3, y: y0 + header_row, text: "esc".into(), fg: t.text.muted, bold: false, bg: None });
    // Body (clipped to the compact scroll height).
    let body_max = if compact { link_row.map(|l| l - body_row).unwrap_or(row) as usize } else { body.len() };
    for (i, line) in body.iter().enumerate().take(body_max) {
        texts.push(Text { x: x0 + inset, y: y0 + body_row + i as u16, text: line.clone(), fg: t.text.muted, bold: false, bg: None });
    }
    if let Some(lr) = link_row {
        let lx = if compact { x0 + inset } else { x0 + inset + (inner as u16).saturating_sub(GO_URL.len() as u16).div_ceil(2) };
        texts.push(Text { x: lx, y: y0 + lr, text: GO_URL.into(), fg: primary, bold: false, bg: None });
    }
    // Buttons: `don't show again` left, `dismiss` and `subscribe` right (a column when narrow).
    let mut hits = Vec::new();
    let mut button = |texts: &mut Vec<Text>, x: u16, y: u16, i: usize| {
        let label = ACTIONS[i];
        let selected = g.selected == i;
        let bw = sw(label) as u16 + 4;
        if selected {
            texts.push(Text { x, y, text: format!("  {label}  "), fg: t.text.action.primary.focused, bold: true, bg: Some(primary) });
        } else {
            let fg = if label == "subscribe" { t.text.base } else { t.text.muted };
            texts.push(Text { x: x + 2, y, text: label.into(), fg, bold: false, bg: None });
        }
        hits.push((Rect::new(x, y, bw, 1), i));
    };
    if narrow {
        for i in 0..ACTIONS.len() {
            button(&mut texts, x0 + inset, y0 + buttons_row + i as u16, i);
        }
    } else {
        button(&mut texts, x0 + inset, y0 + buttons_row, 0);
        let sub_w = sw(ACTIONS[2]) as u16 + 4;
        let dis_w = sw(ACTIONS[1]) as u16 + 4;
        let right = x0 + w - inset;
        button(&mut texts, right - sub_w, y0 + buttons_row, 2);
        button(&mut texts, right - sub_w - 1 - dis_w, y0 + buttons_row, 1);
    }
    for tx in texts {
        if tx.y >= panel.bottom() {
            continue;
        }
        let mut x = tx.x;
        for ch in tx.text.chars() {
            if x >= panel.right() {
                break;
            }
            let bg = tx.bg.unwrap_or_else(|| tint(bg_at(x, tx.y), t.background.base, TEXT_ALPHA));
            let mut st = Style::default().fg(tx.fg.tui()).bg(bg.tui());
            if tx.bold {
                st = st.add_modifier(Modifier::BOLD);
            }
            buf[(x, tx.y)].reset();
            x += put(buf, x, tx.y, &ch.to_string(), st, 2) as u16;
        }
    }
    (panel, hits)
}
