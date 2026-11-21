//! Image strips: `SessionImages` (transcript), the prompt's attachment previews and mini mode's footer previews.
//!
//! A strip shows up to three images in boxes twice as wide as they are tall (`height * 2` columns, shrinking
//! when the row is too narrow, one column apart) and a `+N more` box eight columns wide when there are more. Rows
//! of a strip carry an [`ImageRow`]; whoever draws the rows draws the images over them.

use std::sync::Arc;

use super::text::{Click, SLine, Seg};

/// One strip: its images and where their boxes are, relative to the row's first column.
#[derive(Clone, Debug, PartialEq)]
pub struct Strip {
    /// Every image of the strip (the preview dialog pages through them all).
    pub uris: Vec<String>,
    /// `(x, width)` of the visible images' boxes.
    pub boxes: Vec<(u16, u16)>,
    /// `(x, width, label)` of the `+N more` box.
    pub more: Option<(u16, u16, String)>,
    pub height: u16,
    /// Clicking the `+N more` box opens the dialog at the first hidden image (the prompt's strip).
    pub more_opens: bool,
    /// The images' mention texts (the dialog's footer label), when known.
    pub mentions: Vec<Option<String>>,
}

/// A row of a strip.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageRow {
    pub strip: Arc<Strip>,
    pub row: u16,
}

/// The click that opens the preview dialog: `image.preview:<index>` with the strip's images in the hit table.
pub const CLICK: &str = "image.preview:";

/// `Math.max(4, Math.min(8, Math.floor(terminalHeight / 4)))`.
pub fn preview_height(term_height: u16) -> u16 {
    (term_height / 4).clamp(4, 8)
}

/// Yoga's row layout: items with their `basis` widths (all `flexShrink: 1`), `gap` apart, shrunk in proportion to
/// their basis when the row is narrower than they are; edges rounded to whole cells.
pub fn row_layout(basis: &[f64], gap: f64, avail: f64) -> Vec<(u16, u16)> {
    let total: f64 = basis.iter().sum::<f64>() + gap * basis.len().saturating_sub(1) as f64;
    let shrink = (avail - total).min(0.0);
    let scaled: f64 = basis.iter().sum();
    let mut x: f64 = 0.0;
    let mut out = vec![];
    for b in basis {
        let w = if scaled > 0.0 { (b + shrink * b / scaled).max(0.0) } else { 0.0 };
        let (l, r) = (x.round(), (x + w).round());
        out.push((l.max(0.0) as u16, (r - l).max(0.0) as u16));
        x += w + gap;
    }
    out
}

/// The strip for `uris` with boxes of `height` rows in `avail` columns.
pub fn strip(uris: Vec<String>, height: u16, avail: u16, more_opens: bool) -> Strip {
    let visible = uris.len().min(3);
    let mut basis: Vec<f64> = vec![height as f64 * 2.0; visible];
    let more = uris.len() > visible;
    if more {
        basis.push(8.0);
    }
    let mut boxes = row_layout(&basis, 1.0, avail as f64);
    let more = more.then(|| {
        let (x, w) = boxes.pop().unwrap_or((0, 0));
        (x, w, format!("+{} more", uris.len() - visible))
    });
    Strip { uris, boxes, more, height, more_opens, mentions: vec![] }
}

/// `SessionImages`: `paddingTop 1`, the boxes, `paddingBottom 1`, every row starting with `prefix` and the boxes
/// `left` columns in. `bg` fills the rows (the user message's background).
pub fn session_lines(uris: Vec<String>, height: u16, left: u16, avail: u16, prefix: &[Seg], bg: Option<ratatui::style::Color>, click: Option<Click>) -> Vec<SLine> {
    if uris.is_empty() {
        return vec![];
    }
    let mut s = strip(uris, height, avail, false);
    for b in &mut s.boxes {
        b.0 += left;
    }
    if let Some(m) = &mut s.more {
        m.0 += left;
    }
    let s = Arc::new(s);
    let blank = || SLine::new(prefix.to_vec()).with_bg(bg).on_click(click.clone());
    let mut out = vec![blank()];
    for row in 0..height {
        let mut l = blank();
        l.image = Some(ImageRow { strip: s.clone(), row });
        out.push(l);
    }
    out.push(blank());
    out
}

/// `DialogImagePreview`: one image at a time, `←`/`→` page through them.
#[derive(Clone, Debug, PartialEq)]
pub struct ImagePreview {
    pub uris: Vec<String>,
    /// `mention.text` of each image (the footer label), when it has one.
    pub mentions: Vec<Option<String>>,
    pub index: usize,
    /// The current image could not be read or decoded (set by the app before each frame).
    pub failed: bool,
}

/// The dialog's purpose.
pub const PREVIEW_PURPOSE: &str = "image-preview";

impl ImagePreview {
    pub fn new(uris: Vec<String>, mentions: Vec<Option<String>>, initial: usize) -> ImagePreview {
        let index = initial.min(uris.len().saturating_sub(1));
        ImagePreview { uris, mentions, index, failed: false }
    }

    /// `move(direction)`: wraps around; does nothing with one image.
    pub fn step(&mut self, direction: i64) {
        let n = self.uris.len() as i64;
        if n < 2 {
            return;
        }
        self.index = ((self.index as i64 + direction + n) % n) as usize;
    }

    pub fn current(&self) -> Option<&str> {
        self.uris.get(self.index).map(|s| s.as_str())
    }

    /// The footer label: the mention, `Image N`, or `No preview`.
    pub fn label(&self) -> String {
        if self.failed {
            return "No preview".into();
        }
        self.mentions.get(self.index).cloned().flatten().unwrap_or_else(|| format!("Image {}", self.index + 1))
    }
}

/// `Math.max(3, dimensions().height - 8)`: the preview's image rows.
pub fn preview_image_rows(term_height: u16) -> u16 {
    term_height.saturating_sub(8).max(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boxes_shrink_in_proportion() {
        assert_eq!(row_layout(&[16.0, 16.0, 16.0], 1.0, 100.0), vec![(0, 16), (17, 16), (34, 16)]);
        // 3 * 16 + 8 + 3 gaps = 59 in 40: every box gives up 19 * basis / 56.
        let l = row_layout(&[16.0, 16.0, 16.0, 8.0], 1.0, 40.0);
        assert_eq!(l.last().map(|(x, w)| x + w), Some(40));
        assert_eq!(l[0], (0, 11));
        assert_eq!(preview_height(24), 6);
        assert_eq!(preview_height(10), 4);
        assert_eq!(preview_height(60), 8);
    }

    #[test]
    fn more_box() {
        let s = strip((0..5).map(|i| i.to_string()).collect(), 4, 100, true);
        assert_eq!(s.boxes, vec![(0, 8), (9, 8), (18, 8)]);
        assert_eq!(s.more, Some((27, 8, "+2 more".into())));
    }
}
