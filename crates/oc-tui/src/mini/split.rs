//! The split-footer terminal renderer (OpenTUI 0.7.0 `screenMode: "split-footer"` with
//! `externalOutputMode: "capture-stdout"`): scrollback commits are appended above a footer region
//! that is repainted in place at the bottom of the main screen. Ports `split-scrollback.zig`, the
//! split paths of `renderer.zig` (`appendSplitFooterSnapshotCommit`, `writeSnapshotCommit`, footer
//! transitions) and the split bookkeeping of `renderer.ts` (`applyScreenMode`, `processResize`,
//! `resetSplitFooterForReplay`, pending tail columns).
//!
//! Output is collected in [`Split::out`]; the caller writes it to the terminal.

use super::cell::{Buffer, Color, Glyph, Intent, attr};
use super::scrollback::{Commit, row_widths, tail_after_commit};
use std::fmt::Write as _;

/// `SplitScrollback` (native): rows published above the footer and the output column of the last one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SplitScrollback {
    pub published_rows: u32,
    pub tail_column: u32,
}

impl SplitScrollback {
    pub fn reset(&mut self, seed_rows: u32) {
        self.published_rows = seed_rows;
        self.tail_column = 0;
    }

    pub fn render_offset(&self, surface_offset: u32) -> u32 {
        if surface_offset == 0 { 0 } else { self.published_rows.min(surface_offset) }
    }

    pub fn note_viewport_scroll(&mut self, lines: u32) {
        self.published_rows -= lines.min(self.published_rows);
        if self.published_rows == 0 {
            self.tail_column = 0;
        }
    }

    pub fn note_newline(&mut self) {
        if self.published_rows == 0 {
            self.published_rows = 1;
        }
        self.published_rows += 1;
        self.tail_column = 0;
    }

    pub fn publish_row(&mut self, columns: u32, width: u32, trailing_newline: bool) {
        self.publish_columns(columns, width);
        if trailing_newline {
            self.note_newline();
        }
    }

    fn publish_columns(&mut self, columns: u32, width: u32) {
        if columns == 0 {
            return;
        }
        let width = width.max(1);
        let mut remaining = columns;
        while remaining > 0 {
            if self.published_rows == 0 {
                self.published_rows = 1;
            }
            if self.tail_column >= width {
                self.published_rows += 1;
                self.tail_column = 0;
            }
            let step = remaining.min(width - self.tail_column);
            self.tail_column += step;
            remaining -= step;
            if remaining > 0 && self.tail_column >= width {
                self.published_rows += 1;
                self.tail_column = 0;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransitionMode {
    ViewportScroll,
    ClearStaleRows,
}

/// `PendingSplitFooterTransition`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transition {
    pub mode: TransitionMode,
    pub source_top_line: u32,
    pub source_height: u32,
    pub target_top_line: u32,
    pub target_height: u32,
    pub scroll_lines: u32,
}

/// What OpenTUI sends when the renderer stops: the default cursor color and style.
pub const CURSOR_RESET: &str = "\x1b]12;default\x07\x1b]112\x07\x1b[0 q";

/// Cursor request for the footer frame (the focused textarea's cursor).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorReq {
    pub x: u16,
    pub y: u16,
    /// DECSCUSR style number (1 blinking block, 2 steady block, 5/6 bar ...).
    pub style: u8,
    pub color: Option<(u8, u8, u8)>,
}

pub struct Split {
    pub out: Vec<u8>,
    pub term_width: u32,
    pub term_height: u32,
    /// Requested footer height (`renderer.footerHeight`).
    footer_height: u32,
    /// Effective footer height (`_splitHeight`).
    split_height: u32,
    /// Surface offset: the footer's top line is `render_offset + 1`.
    render_offset: u32,
    sb: SplitScrollback,
    /// TS-side tail column after all completed commits.
    split_tail_column: usize,
    queue: Vec<Commit>,
    transition: Option<Transition>,
    force_full: bool,
    current: Option<(u32, Buffer)>,
    cursor_visible: bool,
    cursor_style: Option<u8>,
    cursor_color: Option<(u8, u8, u8)>,
    /// Terminal graphics capabilities (images in scrollback and the footer).
    pub caps: crate::graphics::Caps,
    /// `kittyHistoryNextImageId`: scrollback images get ids above every live one.
    kitty_history: Option<u32>,
    /// Images placed in the footer: the live frame's state.
    pub footer_images: FooterImages,
}

/// `moveToSplitOutputCursor`.
fn move_to_output_cursor(out: &mut String, render_offset: u32, output_column: u32, width: u32) {
    let width = width.max(1);
    let row = if render_offset > 0 { render_offset } else { 1 };
    let column = if render_offset == 0 || output_column == 0 { 1 } else { (output_column + 1).min(width) };
    let _ = write!(out, "\x1b[{row};{column}H");
}

fn emit_color(out: &mut String, c: Color, background: bool) {
    match c.intent {
        Intent::Default => out.push_str(if background { "\x1b[49m" } else { "\x1b[39m" }),
        _ if background && c.a == 0 => out.push_str("\x1b[49m"),
        Intent::Indexed(slot) => {
            let _ = write!(out, "\x1b[{};5;{slot}m", if background { 48 } else { 38 });
        }
        Intent::Rgb => {
            let _ = write!(out, "\x1b[{};2;{};{};{}m", if background { 48 } else { 38 }, c.r, c.g, c.b);
        }
    }
}

fn emit_attrs(out: &mut String, a: u8) {
    for (bit, code) in [(attr::BOLD, "1"), (attr::DIM, "2"), (attr::ITALIC, "3"), (attr::UNDERLINE, "4"), (attr::BLINK, "5"), (attr::INVERSE, "7"), (attr::HIDDEN, "8"), (attr::STRIKETHROUGH, "9")] {
        if a & bit != 0 {
            let _ = write!(out, "\x1b[{code}m");
        }
    }
}

/// Write cells `[0, end)` of row `y` with SGR runs (reset before each style change).
fn write_cells(out: &mut String, buf: &Buffer, y: u16, end: usize) {
    write_cells_with(out, buf, y, end, &|_, _| false);
}

/// [`write_cells`] with the cells `image(x, y)` covers written as plain blanks (a Kitty image shows there).
fn write_cells_with(out: &mut String, buf: &Buffer, y: u16, end: usize, image: &dyn Fn(u16, u16) -> bool) {
    let mut cur: Option<(Color, Color, u8)> = None;
    for x in 0..end {
        let cell = buf.get(x as u16, y);
        if cell.glyph == Glyph::Cont {
            continue;
        }
        if image(x as u16, y) {
            out.push_str("\x1b[0m ");
            cur = None;
            continue;
        }
        // Never-drawn cells keep OpenTUI's cleared foreground (white) when serialized.
        let fg = if cell.glyph == Glyph::Empty { Color::WHITE } else { cell.fg };
        let key = (fg, cell.bg, cell.attrs);
        if cur != Some(key) {
            out.push_str("\x1b[0m");
            emit_color(out, fg, false);
            emit_color(out, cell.bg, true);
            emit_attrs(out, cell.attrs);
            cur = Some(key);
        }
        match &cell.glyph {
            Glyph::Text(t) => out.push_str(t),
            _ => out.push(' '),
        }
    }
}

impl Split {
    /// A split-footer renderer for a `width`×`height` terminal with the cursor on row `cursor_row`
    /// (1-based, from the startup CPR; 0 when the terminal did not answer).
    pub fn new(width: u32, height: u32, footer_height: u32, cursor_row: u32) -> Split {
        let mut s = Split {
            out: Vec::new(),
            term_width: width,
            term_height: height,
            footer_height,
            split_height: footer_height.min(height),
            render_offset: 0,
            sb: SplitScrollback::default(),
            split_tail_column: 0,
            queue: Vec::new(),
            transition: None,
            force_full: true,
            current: None,
            cursor_visible: false,
            cursor_style: None,
            cursor_color: None,
            caps: crate::graphics::Caps::from_env(&|_| None),
            kitty_history: None,
            footer_images: FooterImages::default(),
        };
        let salt = FooterImages::salt();
        s.footer_images.salt = salt;
        s.kitty_history = Some(salt + crate::graphics::kitty::IMAGE_ID_MASK + 1);
        let seed = if cursor_row == 0 { 0 } else { cursor_row.max(1).min(height.max(1)) };
        s.reset_scrollback(seed);
        s
    }

    pub fn width(&self) -> usize {
        self.term_width.max(1) as usize
    }

    pub fn footer_height(&self) -> u32 {
        self.footer_height
    }

    pub fn split_height(&self) -> u32 {
        self.split_height
    }

    /// The footer's top line (1-based).
    pub fn footer_top(&self) -> u32 {
        self.render_offset + 1
    }

    pub fn has_pending(&self) -> bool {
        !self.queue.is_empty()
    }

    fn pinned(&self) -> u32 {
        self.term_height.saturating_sub(self.split_height)
    }

    fn reset_scrollback(&mut self, seed_rows: u32) {
        self.sb.reset(seed_rows);
        self.render_offset = self.sb.render_offset(self.pinned());
        self.split_tail_column = 0;
    }

    fn clamp_surface_offset(&self, surface_offset: u32, pinned: u32) -> u32 {
        let output = self.sb.render_offset(pinned);
        surface_offset.clamp(output.min(pinned), pinned)
    }

    fn sync_scrollback(&mut self) {
        self.render_offset = self.clamp_surface_offset(self.render_offset, self.pinned());
    }

    /// `getPendingSplitTailColumn()`.
    pub fn tail_column(&self) -> usize {
        let width = self.width();
        let mut tail = self.split_tail_column;
        for c in &self.queue {
            tail = tail_after_commit(c, tail, width);
        }
        tail
    }

    /// `writeToScrollback` / scrollback surface commit: queue a snapshot.
    pub fn enqueue(&mut self, commit: Commit) {
        self.queue.push(commit);
    }

    fn emit(&mut self, s: &str) {
        self.out.extend_from_slice(s.as_bytes());
    }

    /// `applyPendingSplitFooterTransition`.
    fn apply_transition(&mut self, out: &mut String) {
        let Some(t) = self.transition.take() else { return };
        if t.source_height == 0 || t.target_height == 0 {
            return;
        }
        match t.mode {
            TransitionMode::ViewportScroll => {
                if t.scroll_lines == 0 {
                    return;
                }
                self.sb.note_viewport_scroll(t.scroll_lines);
                if t.source_top_line < t.target_top_line {
                    let _ = write!(out, "\x1b[{}T", t.scroll_lines);
                } else if t.source_top_line > t.target_top_line {
                    let _ = write!(out, "\x1b[{}S", t.scroll_lines);
                }
            }
            TransitionMode::ClearStaleRows => {
                let source_end = t.source_top_line + t.source_height - 1;
                let target_end = t.target_top_line + t.target_height - 1;
                for line in t.source_top_line..=source_end {
                    if line >= t.target_top_line && line <= target_end {
                        continue;
                    }
                    let _ = write!(out, "\x1b[{line};1H\x1b[2K");
                }
            }
        }
    }

    /// `appendSplitFooterSnapshotCommit`: returns whether the footer must be redrawn.
    fn append_commit(&mut self, out: &mut String, c: &Commit) -> bool {
        let pinned = self.pinned();
        let width = self.term_width;
        let previous_surface = self.render_offset;
        let previous_output = self.sb.render_offset(previous_surface);
        let previous_column = self.sb.tail_column;
        let snap = &c.buf;
        let has_content = snap.width > 0 && snap.height > 0;
        let columns = (c.row_columns as u32).min(snap.width as u32);
        let starts_mid_line = previous_column > 0 && c.start_on_new_line;
        let starts_wrapped_line = previous_column >= width;
        let previous_footer_top = (previous_surface + 1).max(1);
        // `prepareSnapshotImages`: native images, or the text fallback drawn into the snapshot.
        let protocol = crate::graphics::snapshot::prepare(&self.caps, &c.images, columns, c.start_on_new_line, previous_column, pinned);
        let fallback;
        let snap = if protocol.is_none() && !c.images.is_empty() {
            fallback = blocks_fallback(snap, &c.images);
            &fallback
        } else {
            snap
        };
        let history = match protocol {
            Some(crate::graphics::Protocol::Kitty) => match self.kitty_history {
                Some(first) => {
                    self.kitty_history = first.checked_add(c.images.len() as u32);
                    Some(first - 1)
                }
                None => None,
            },
            _ => None,
        };
        let protocol = if protocol == Some(crate::graphics::Protocol::Kitty) && history.is_none() { None } else { protocol };
        let widths = row_widths(snap, columns as usize);
        let mut ends_last_row = c.trailing_newline;
        if has_content {
            if starts_mid_line {
                self.sb.note_newline();
            }
            for (row, w) in widths.iter().enumerate() {
                self.sb.publish_row(*w as u32, width, row + 1 < widths.len() || c.trailing_newline);
            }
            self.sb.published_rows = self.sb.published_rows.min(pinned);
            ends_last_row = c.trailing_newline || (pinned == 0 && self.sb.tail_column >= width);
            if pinned == 0 {
                self.sb.tail_column = 0;
            }
        }
        let next_output = self.sb.render_offset(pinned);
        let next_render = self.clamp_surface_offset(previous_surface, pinned);
        let target_footer_top = (next_render + 1).max(1);
        let bounded = has_content && next_render == pinned && next_output == next_render;
        let repaint_footer = bounded && pinned < 2;
        let redraw = previous_surface != next_render || repaint_footer;
        if has_content {
            if target_footer_top > previous_footer_top {
                for line in previous_footer_top..target_footer_top {
                    let _ = write!(out, "\x1b[{line};1H\x1b[2K");
                }
            }
            if bounded {
                let _ = write!(out, "\x1b[1;{}r", pinned.max(2));
            }
            move_to_output_cursor(out, previous_output, previous_column, width);
            if starts_mid_line || starts_wrapped_line {
                out.push_str("\r\n");
            } else if previous_column + widths.first().copied().unwrap_or(0) as u32 > width {
                out.push_str("\x1b[0m\x1bD\x1b[2K\x1b[A");
            }
            // `writeSnapshotCommit`.
            out.push_str("\x1b[0m\x1b[K");
            let kitty = protocol == Some(crate::graphics::Protocol::Kitty);
            let covered = |x: u16, y: u16| kitty && c.images.iter().any(|i| x >= i.x && x < i.x + i.width && y >= i.y && y < i.y + i.height);
            for y in 0..snap.height {
                write_cells_with(out, snap, y, widths[y as usize], &covered);
                out.push_str("\x1b[0m");
                if let Some(p) = protocol {
                    crate::graphics::snapshot::after_row(out, &self.caps, &c.images, y, p, history.unwrap_or(0));
                }
                let last = y + 1 >= snap.height;
                if !last || ends_last_row {
                    out.push_str("\r\n\x1b[K");
                }
            }
            if bounded {
                let scroll_back = pinned < 2 && pinned + self.term_height > 1;
                out.push_str(if scroll_back { "\x1bD\x1b[r" } else { "\x1b[r" });
            }
        }
        self.render_offset = next_render;
        if redraw {
            let _ = write!(out, "\x1b[{target_footer_top};1H");
        }
        redraw
    }

    /// Paint the footer at the current offset: changed rows only, or every row when forced. Images placed in the
    /// footer follow its rows: a Kitty image is placed again when it or the footer moved, a Sixel painted again
    /// when a row it covers was written.
    fn paint_footer(&mut self, out: &mut String, footer: &Buffer, force: bool) {
        let shown = self.footer_images.layout(&self.caps, (self.term_width as u16, self.term_height as u16));
        let fallback;
        let footer = if shown.is_empty() {
            footer
        } else {
            fallback = self.footer_images.apply(footer, &shown);
            &fallback
        };
        let top = self.render_offset + 1;
        let mut written = vec![false; footer.height as usize];
        let force = force || self.force_full || self.current.as_ref().is_none_or(|(t, b)| *t != top || b.width != footer.width || b.height != footer.height);
        for y in 0..footer.height.min(self.split_height as u16) {
            if !force
                && let Some((_, prev)) = &self.current
                && (0..footer.width).all(|x| prev.get(x, y) == footer.get(x, y))
            {
                continue;
            }
            let _ = write!(out, "\x1b[{};1H", top + y as u32);
            write_cells(out, footer, y, footer.width as usize);
            out.push_str("\x1b[0m");
            written[y as usize] = true;
        }
        self.current = Some((top, footer.clone()));
        self.force_full = false;
        self.footer_images.emit(out, &self.caps, shown, top, &written);
    }

    fn place_cursor(&mut self, out: &mut String, cursor: Option<CursorReq>) {
        match cursor {
            Some(c) if (c.y as u32) < self.split_height => {
                // OpenTUI sends the cursor color (OSC 12) and its style (DECSCUSR) together when either changes.
                if self.cursor_style != Some(c.style) || (c.color.is_some() && self.cursor_color != c.color) {
                    if let Some((r, g, b)) = c.color {
                        let _ = write!(out, "\x1b]12;#{r:02x}{g:02x}{b:02x}\x07");
                        self.cursor_color = c.color;
                    }
                    let _ = write!(out, "\x1b[{} q", c.style);
                    self.cursor_style = Some(c.style);
                }
                let _ = write!(out, "\x1b[{};{}H\x1b[?25h", self.render_offset + 1 + c.y as u32, c.x as u32 + 1);
                self.cursor_visible = true;
            }
            _ => {
                out.push_str("\x1b[?25l");
                self.cursor_visible = false;
            }
        }
    }

    /// Render a frame: flush queued commits (each batch repaints the footer after it), then apply
    /// a pending footer transition and paint `footer` with `cursor`.
    pub fn render(&mut self, footer: &Buffer, cursor: Option<CursorReq>) {
        let mut out = String::new();
        out.push_str("\x1b[?2026h\x1b[?25l");
        if !self.queue.is_empty() {
            self.apply_transition(&mut out);
            let queue = std::mem::take(&mut self.queue);
            let mut redraw = false;
            for c in &queue {
                redraw |= self.append_commit(&mut out, c);
            }
            self.paint_footer(&mut out, footer, redraw);
            // `completeNativeSplitCommits`.
            self.sync_scrollback();
            let width = self.width();
            for c in &queue {
                self.split_tail_column = tail_after_commit(c, self.split_tail_column, width);
            }
            self.transition = None;
        } else {
            // `prepareSplitFooterRepaintFrame`.
            let previous = self.render_offset;
            let next = match self.transition {
                Some(t) if t.mode == TransitionMode::ViewportScroll && t.target_top_line > 0 && t.scroll_lines > 0 => t.target_top_line - 1,
                _ => self.clamp_surface_offset(previous, self.pinned()),
            };
            self.render_offset = next;
            self.apply_transition(&mut out);
            self.paint_footer(&mut out, footer, previous != next);
        }
        self.place_cursor(&mut out, cursor);
        out.push_str("\x1b[?2026l");
        self.emit(&out);
    }

    /// `renderer.footerHeight = height` (`applyScreenMode("split-footer", ..., height)`). Call
    /// [`Split::render`] first when commits are queued: upstream defers the change until they flush.
    pub fn set_footer_height(&mut self, footer_height: u32) {
        let prev_split = self.split_height;
        let next_split = footer_height.min(self.term_height);
        self.footer_height = footer_height;
        if prev_split == next_split {
            return;
        }
        let previous_top = self.render_offset + 1;
        let source_top = self.transition.map(|t| t.source_top_line).unwrap_or(previous_top);
        let source_height = self.transition.map(|t| t.source_height).unwrap_or(prev_split);
        let source_surface = source_top.saturating_sub(1);
        let source_output = self.sb.render_offset(source_surface);
        let next_pinned = self.term_height.saturating_sub(next_split);
        let next_output = self.sb.render_offset(next_pinned);
        let pending = self.transition;
        let pending_return = pending.is_some() && next_split == source_height;
        let viewport_return = pending_return && pending.is_some_and(|t| t.mode == TransitionMode::ViewportScroll && t.scroll_lines > 0);
        let shrinking = next_split > 0 && next_split < source_height;
        let growing = next_split > source_height && source_height > 0;
        let next_surface = if next_split == 0 {
            0
        } else if viewport_return {
            pending.unwrap().target_top_line - 1
        } else if pending_return || (shrinking && source_surface > 0) {
            source_surface
        } else if shrinking {
            next_output
        } else if growing {
            next_output.max(source_surface.min(next_pinned))
        } else {
            next_pinned
        };
        let target_top = next_surface + 1;
        let scroll_lines = if viewport_return {
            pending.unwrap().scroll_lines
        } else if next_split > 0 && !pending_return {
            source_output.saturating_sub(next_output)
        } else {
            0
        };
        let mode = if (!shrinking || viewport_return) && scroll_lines > 0 { TransitionMode::ViewportScroll } else { TransitionMode::ClearStaleRows };
        self.split_height = next_split;
        self.render_offset = next_surface;
        self.transition = Some(Transition { mode, source_top_line: source_top, source_height, target_top_line: target_top, target_height: next_split, scroll_lines });
        self.force_full = true;
    }

    /// `processResize(width, height)`. Call [`Split::render`] first when commits are queued.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == self.term_width && height == self.term_height {
            self.force_full = true;
            return;
        }
        let visible_previous = self.transition.map(|t| t.source_height).unwrap_or(self.split_height);
        let mut clear_start: Option<u32> = None;
        if width < self.term_width && visible_previous > 0 {
            clear_start = Some((self.render_offset + 1).min(self.term_height.saturating_sub(visible_previous * 2).max(1)));
        }
        if let Some(t) = self.transition {
            clear_start = Some(clear_start.map_or(t.source_top_line, |c| c.min(t.source_top_line)));
        }
        if let Some(line) = clear_start {
            let s = format!("\x1b[{line};1H\x1b[J");
            self.emit(&s);
        }
        self.term_width = width;
        self.term_height = height;
        self.split_height = self.footer_height.min(height);
        self.force_full = true;
        self.transition = None;
        self.sync_scrollback();
    }

    /// `resetSplitFooterForReplay({ clearSavedLines })`. Call [`Split::render`] first when commits
    /// are queued: upstream applies the reset after the commits queued before it.
    pub fn reset_for_replay(&mut self, clear_saved_lines: bool) {
        let s = format!("\x1b[r\x1b[0m\x1b[H\x1b[2J{}\x1b[H", if clear_saved_lines { "\x1b[3J" } else { "" });
        self.emit(&s);
        self.transition = None;
        self.reset_scrollback(0);
        self.force_full = true;
    }

    /// Leave split-footer mode (`screenMode = "main-screen"`): clear the footer rows and leave the
    /// cursor on the footer's top line.
    pub fn leave(&mut self) {
        let mut images = String::new();
        self.footer_images.clear(&mut images, &self.caps);
        self.emit(&images);
        let top = self.transition.map(|t| t.source_top_line).unwrap_or(self.render_offset + 1);
        let s = format!("\x1b[?2026h\x1b[r\x1b[{top};1H\x1b[J\x1b[?25h{CURSOR_RESET}\x1b[?25h\x1b[?2026l");
        self.emit(&s);
    }

    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }
}

/// The images of a snapshot drawn as quadrant characters (`materializeImageFallbacks`).
fn blocks_fallback(snap: &Buffer, images: &[crate::graphics::snapshot::SnapImage]) -> Buffer {
    let mut out = snap.clone();
    for im in images {
        for cy in 0..im.height {
            for cx in 0..im.width {
                let (x, y) = (im.x + cx, im.y + cy);
                if x >= out.width || y >= out.height {
                    continue;
                }
                let (ch, fg, bg) = crate::graphics::snapshot::block_cell(im, cx, cy);
                let i = y as usize * out.width as usize + x as usize;
                let under = out.cells[i].bg;
                let mix = |c: [u8; 4]| if c[3] == 255 { Color::rgb(c[0], c[1], c[2]) } else { super::cell::blend(Color::rgba(c[0], c[1], c[2], c[3]), under) };
                out.cells[i] = super::cell::Cell { glyph: Glyph::Text(ch.to_string()), fg: mix(fg), bg: mix(bg), attrs: 0 };
            }
        }
    }
    out
}

/// An image box in the footer (`fit: "fit"`), in footer cells.
#[derive(Clone, Debug)]
pub struct FooterImage {
    pub picture: std::sync::Arc<crate::graphics::Picture>,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// A fitted footer image and how it shows.
#[derive(Clone, Debug)]
pub struct Shown {
    handle: u32,
    protocol: crate::graphics::Protocol,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    pixel_width: u32,
    pixel_height: u32,
    picture: Option<std::sync::Arc<crate::graphics::Picture>>,
}

impl PartialEq for Shown {
    fn eq(&self, o: &Self) -> bool {
        (self.handle, self.protocol, self.x, self.y, self.width, self.height, self.pixel_width, self.pixel_height)
            == (o.handle, o.protocol, o.x, o.y, o.width, o.height, o.pixel_width, o.pixel_height)
    }
}

/// Images in the footer (the prompt's previews): what the terminal shows and what to send when they change.
#[derive(Default)]
pub struct FooterImages {
    pub salt: u32,
    /// The images the next frame shows.
    pub next: Vec<FooterImage>,
    /// What the terminal shows, with the footer's top line.
    committed: Vec<Shown>,
    committed_top: u32,
}

impl FooterImages {
    /// Fit each box's image (`renderSelf` with `fit: "fit"`) and pick its protocol.
    fn layout(&self, caps: &crate::graphics::Caps, term: (u16, u16)) -> Vec<Shown> {
        let mut g = crate::graphics::Graphics::new(caps.clone());
        g.term = term;
        self.next
            .iter()
            .filter_map(|b| {
                let p = &b.picture;
                let (fw, fh) = g.fitted_size(crate::graphics::Fit::Fit, b.width as u32, b.height as u32, p.width, p.height);
                if fw == 0 || fh == 0 {
                    return None;
                }
                let res = caps.resolution.filter(|_| term.0 > 0 && term.1 > 0);
                let px = |cells: u32, total: u16, r: Option<u32>| r.map(|r| ((cells as f64 * r as f64 / total as f64).round() as u32).max(1)).unwrap_or(0);
                let (pw, ph) = (px(fw, term.0, res.map(|r| r.0)), px(fh, term.1, res.map(|r| r.1)));
                Some(Shown {
                    handle: p.handle,
                    protocol: caps.resolve(crate::graphics::Requested::Auto, (pw, ph)),
                    x: b.x + ((b.width as u32 - fw) / 2) as u16,
                    y: b.y + ((b.height as u32 - fh) / 2) as u16,
                    width: fw as u16,
                    height: fh as u16,
                    pixel_width: pw,
                    pixel_height: ph,
                    picture: Some(p.clone()),
                })
            })
            .collect()
    }

    /// The footer with the images' cells: quadrants for text images, blanks under native ones.
    fn apply(&self, footer: &Buffer, shown: &[Shown]) -> Buffer {
        let mut out = footer.clone();
        for s in shown {
            let Some(p) = &s.picture else { continue };
            for cy in 0..s.height {
                for cx in 0..s.width {
                    let (x, y) = (s.x + cx, s.y + cy);
                    if x >= out.width || y >= out.height {
                        continue;
                    }
                    let i = y as usize * out.width as usize + x as usize;
                    out.cells[i] = if s.protocol == crate::graphics::Protocol::Blocks {
                        let src = (0, 0, p.width, p.height);
                        let (ch, fg, bg) = crate::graphics::blocks::quadrant(crate::graphics::blocks::samples(p, src, s.width as u32, s.height as u32, cx as u32, cy as u32));
                        let under = out.cells[i].bg;
                        let mix = |c: [u8; 4]| if c[3] == 255 { Color::rgb(c[0], c[1], c[2]) } else { super::cell::blend(Color::rgba(c[0], c[1], c[2], c[3]), under) };
                        super::cell::Cell { glyph: Glyph::Text(ch.to_string()), fg: mix(fg), bg: mix(bg), attrs: 0 }
                    } else {
                        let default = Color { r: 0, g: 0, b: 0, a: 255, intent: Intent::Default };
                        super::cell::Cell { glyph: Glyph::Text(" ".into()), fg: default, bg: default, attrs: 0 }
                    };
                }
            }
        }
        out
    }

    /// Send what changed after the footer's rows were painted at line `top`.
    fn emit(&mut self, out: &mut String, caps: &crate::graphics::Caps, shown: Vec<Shown>, top: u32, written: &[bool]) {
        use crate::graphics::{Protocol, kitty, resize, sixel};
        let tmux = caps.tmux();
        let moved = top != self.committed_top;
        let kitty_now: Vec<&Shown> = shown.iter().filter(|s| s.protocol == Protocol::Kitty).collect();
        let kitty_before: Vec<&Shown> = self.committed.iter().filter(|s| s.protocol == Protocol::Kitty).collect();
        if moved || kitty_now != kitty_before {
            for (i, _) in self.committed.iter().enumerate().filter(|(_, s)| s.protocol == Protocol::Kitty) {
                kitty::delete(out, self.salt + i as u32 + 1, None, true, tmux);
            }
            for (i, s) in shown.iter().enumerate().filter(|(_, s)| s.protocol == Protocol::Kitty) {
                let Some(p) = &s.picture else { continue };
                let id = self.salt + i as u32 + 1;
                let pixels = s.pixel_width as u64 * s.pixel_height as u64;
                let downscale = pixels > 0 && p.width as u64 * p.height as u64 >= pixels * 4;
                let (sw, sh) = if downscale {
                    kitty::transmit(out, &resize::area(p, s.pixel_width, s.pixel_height), id, tmux);
                    (s.pixel_width, s.pixel_height)
                } else {
                    kitty::transmit(out, p, id, tmux);
                    (p.width, p.height)
                };
                let place = kitty::Place { x: s.x as u32, y: top - 1 + s.y as u32, width: s.width as u32, height: s.height as u32, source_x: 0, source_y: 0, source_width: sw, source_height: sh };
                kitty::placement(out, id, i as u32 + 1, place, kitty::placement_z(i as u32 + 1), tmux);
            }
        }
        for s in shown.iter().filter(|s| s.protocol == Protocol::Sixel) {
            let repainted = (s.y..s.y + s.height).any(|y| written.get(y as usize).copied().unwrap_or(false));
            if !repainted && !moved && self.committed.contains(s) {
                continue;
            }
            let Some(p) = &s.picture else { continue };
            let resized = ((s.pixel_width, s.pixel_height) != (p.width, p.height)).then(|| resize::area(p, s.pixel_width, s.pixel_height));
            if let Some(payload) = sixel::encode(resized.as_ref().unwrap_or(p)) {
                let _ = write!(out, "\x1b[{};{}H", top + s.y as u32, s.x as u32 + 1);
                sixel::framed(out, &payload, tmux);
            }
        }
        self.committed = shown;
        self.committed_top = top;
    }

    /// Free the Kitty images (leaving the screen).
    fn clear(&mut self, out: &mut String, caps: &crate::graphics::Caps) {
        for (i, s) in self.committed.iter().enumerate() {
            if s.protocol == crate::graphics::Protocol::Kitty {
                crate::graphics::kitty::delete(out, self.salt + i as u32 + 1, None, true, caps.tmux());
            }
        }
        self.committed.clear();
    }

    fn salt() -> u32 {
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0) as u64 ^ (std::process::id() as u64) << 7;
        1 + (seed % (u32::MAX - crate::graphics::kitty::IMAGE_ID_MASK - 2) as u64 / 2) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(rows: &[&str], width: u16, sonl: bool, tnl: bool) -> Commit {
        let mut buf = Buffer::new(width, rows.len() as u16);
        for (y, r) in rows.iter().enumerate() {
            buf.draw_chunks(0, y as i32, &[super::super::cell::StyledChunk::plain(*r, Color::WHITE)], width as usize);
        }
        Commit { buf, row_columns: width as usize, start_on_new_line: sonl, trailing_newline: tnl, images: vec![] }
    }

    fn picture() -> std::sync::Arc<crate::graphics::Picture> {
        std::sync::Arc::new(crate::graphics::decode::decode(include_bytes!("../graphics/testdata/landscape.png")).unwrap())
    }

    fn image_commit() -> Commit {
        let mut c = commit(&["› shot.png", " ", " "], 20, true, false);
        for y in 1..3 {
            for x in 0..4 {
                c.buf.cells[y * 20 + x].glyph = Glyph::Text(" ".into());
            }
        }
        c.images = vec![crate::graphics::snapshot::SnapImage { picture: picture(), x: 0, y: 1, width: 4, height: 2, pixel_width: 40, pixel_height: 40 }];
        c
    }

    #[test]
    fn scrollback_images_go_out_after_their_last_row() {
        let mut s = Split::new(20, 10, 4, 1);
        s.caps.apply_replies(b"\x1b_Gi=31337;OK\x1b\\\x1b[4;200;200t");
        s.enqueue(image_commit());
        s.render(&Buffer::new(20, 4), None);
        let out = String::from_utf8(s.take_output()).unwrap();
        // The image's cells are plain blanks; after its last row the cursor goes back up to its first row.
        assert!(out.contains("\x1b[0m \x1b[0m \x1b[0m \x1b[0m "), "{out:?}");
        let id = s.footer_images.salt + crate::graphics::kitty::IMAGE_ID_MASK + 1;
        assert!(out.contains(&format!("\x1b[s\x1b[1A\r\x1b_Ga=t,f=100,i={id},m=0,q=2;")), "{out:?}");
        assert!(out.contains(&format!("\x1b_Ga=p,i={id},p=1,c=4,r=2,C=1,z=-1073741823,q=2\x1b\\\x1b[u")), "{out:?}");
        // The next commit's images take the next ids.
        s.enqueue(image_commit());
        s.render(&Buffer::new(20, 4), None);
        let out = String::from_utf8(s.take_output()).unwrap();
        assert!(out.contains(&format!("i={},", id + 1)), "{out:?}");
    }

    #[test]
    fn scrollback_images_without_graphics_are_text() {
        let mut s = Split::new(20, 10, 4, 1);
        s.enqueue(image_commit());
        s.render(&Buffer::new(20, 4), None);
        let out = String::from_utf8(s.take_output()).unwrap();
        assert!(!out.contains("\x1b_G") && !out.contains("\x1bP0;1;0q"), "{out:?}");
        assert!(crate::graphics::blocks::QUADRANTS[1..].iter().any(|q| out.contains(*q)), "{out:?}");
    }

    #[test]
    fn footer_images_follow_the_footer() {
        let mut s = Split::new(20, 10, 4, 10);
        s.caps.apply_replies(b"\x1b[?62;4c\x1b[4;200;200t");
        let footer = Buffer::new(20, 4);
        s.footer_images.next = vec![FooterImage { picture: picture(), x: 2, y: 0, width: 6, height: 3 }];
        s.render(&footer, None);
        let out = String::from_utf8(s.take_output()).unwrap();
        // 96 x 48 fitted in 6 x 3 cells of 10 x 20 pixels: 6 x 2 cells at the box's top, a 60 x 40 sixel.
        let top = s.footer_top();
        assert!(out.contains(&format!("\x1b[{top};3H\x1bP0;1;0q\"1;1;60;40")), "{out:?}");
        // Unchanged: nothing more.
        s.render(&footer, None);
        let out = String::from_utf8(s.take_output()).unwrap();
        assert!(!out.contains("\x1bP0;1;0q"), "{out:?}");
        s.footer_images.next = vec![];
        s.render(&footer, None);
        let out = String::from_utf8(s.take_output()).unwrap();
        assert!(!out.contains("\x1bP0;1;0q"), "{out:?}");
    }

    #[test]
    fn footer_follows_output_until_pinned() {
        let mut s = Split::new(20, 10, 4, 1);
        assert_eq!(s.footer_top(), 2);
        s.enqueue(commit(&["", "splash"], 20, true, true));
        s.render(&Buffer::new(20, 4), None);
        // Seed row 1 + two rows + the trailing newline row.
        assert_eq!(s.footer_top(), 4);
        for _ in 0..5 {
            s.enqueue(commit(&["a", "b"], 20, true, true));
            s.render(&Buffer::new(20, 4), None);
        }
        assert_eq!(s.footer_top(), 7);
    }
}
