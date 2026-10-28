//! The split-footer terminal renderer (OpenTUI 0.6.8 `screenMode: "split-footer"` with
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
    let mut cur: Option<(Color, Color, u8)> = None;
    for x in 0..end {
        let cell = buf.get(x as u16, y);
        if cell.glyph == Glyph::Cont {
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
        };
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
            for y in 0..snap.height {
                write_cells(out, snap, y, widths[y as usize]);
                out.push_str("\x1b[0m");
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

    /// Paint the footer at the current offset: changed rows only, or every row when forced.
    fn paint_footer(&mut self, out: &mut String, footer: &Buffer, force: bool) {
        let top = self.render_offset + 1;
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
        }
        self.current = Some((top, footer.clone()));
        self.force_full = false;
    }

    fn place_cursor(&mut self, out: &mut String, cursor: Option<CursorReq>) {
        match cursor {
            Some(c) if (c.y as u32) < self.split_height => {
                if self.cursor_style != Some(c.style) {
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
        let top = self.transition.map(|t| t.source_top_line).unwrap_or(self.render_offset + 1);
        let s = format!("\x1b[?2026h\x1b[r\x1b[{top};1H\x1b[J\x1b[0 q\x1b[?25h\x1b[?2026l");
        self.emit(&s);
    }

    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
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
        Commit { buf, row_columns: width as usize, start_on_new_line: sonl, trailing_newline: tnl }
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
