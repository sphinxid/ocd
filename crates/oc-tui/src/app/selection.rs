//! Mouse text selection and copy (`src/util/selection.ts`, spec §5.15): drag to select over the
//! rendered screen, copy-on-select (`terminal.copy = "select"`) or right-click copy (`"manual"`).

use super::App;
use crate::keymap::Stroke;
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (u16, u16),
    pub focus: (u16, u16),
    /// The pointer moved since the button went down (not a fresh cell selection).
    pub dragged: bool,
    /// The left button is still down.
    pub active: bool,
}

impl Selection {
    /// Ordered `(start, end)` in reading order; `end` is inclusive.
    pub fn range(&self) -> ((u16, u16), (u16, u16)) {
        let a = (self.anchor.1, self.anchor.0);
        let b = (self.focus.1, self.focus.0);
        let (s, e) = if a <= b { (a, b) } else { (b, a) };
        ((s.1, s.0), (e.1, e.0))
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        let ((x0, y0), (x1, y1)) = self.range();
        if y < y0 || y > y1 {
            return false;
        }
        let from = if y == y0 { x0 } else { 0 };
        let to = if y == y1 { x1 } else { u16::MAX };
        x >= from && x <= to
    }

    /// Selected text from a rendered buffer: rows joined with `\n`, trailing blanks trimmed.
    pub fn text(&self, buf: &Buffer) -> String {
        let area = buf.area;
        let ((x0, y0), (x1, y1)) = self.range();
        let mut rows = Vec::new();
        for y in y0..=y1.min(area.bottom().saturating_sub(1)) {
            let from = if y == y0 { x0 } else { area.x };
            let to = if y == y1 { x1 } else { area.right().saturating_sub(1) };
            let mut line = String::new();
            let mut x = from;
            while x <= to && x < area.right() {
                let sym = buf[(x, y)].symbol();
                line.push_str(if sym.is_empty() { "" } else { sym });
                x += 1;
            }
            rows.push(line.trim_end().to_string());
        }
        rows.join("\n")
    }
}

impl App {
    pub fn selection_text(&self) -> Option<String> {
        let s = self.selection.as_ref()?;
        if !s.dragged {
            return None;
        }
        let text = s.text(self.last_frame.as_ref()?);
        if text.trim().is_empty() { None } else { Some(text) }
    }

    /// `copy()`: write the selection to the clipboard; never clears it.
    pub fn copy_selection(&mut self) -> bool {
        let Some(text) = self.selection_text() else { return false };
        if crate::clipboard::write(&text) {
            self.info("Copied to clipboard");
        } else {
            self.toast(super::Toast::new("error", "Failed to copy to clipboard"));
        }
        true
    }

    pub fn clear_selection(&mut self) -> bool {
        let had = self.selection_text().is_some();
        if self.selection.take().is_some() {
            self.dirty = true;
        }
        had
    }

    fn copy_on_select(&self) -> bool {
        self.config.copy_mode() == "select"
    }

    /// Left button down: start a new (empty) selection.
    pub fn selection_down(&mut self, x: u16, y: u16) {
        self.selection = Some(Selection { anchor: (x, y), focus: (x, y), dragged: false, active: true });
    }

    /// Left drag: extend the selection. Returns true when a selection is being dragged.
    pub fn selection_drag(&mut self, x: u16, y: u16) -> bool {
        let Some(s) = self.selection.as_mut().filter(|s| s.active) else { return false };
        if (x, y) != s.focus || s.dragged {
            s.focus = (x, y);
            s.dragged = true;
            self.dirty = true;
        }
        true
    }

    /// Left button up. Returns true when the release ended a drag selection (and was consumed).
    pub fn selection_up(&mut self, x: u16, y: u16) -> bool {
        let Some(s) = self.selection.as_mut() else { return false };
        s.active = false;
        if !s.dragged {
            self.selection = None;
            return false;
        }
        s.focus = (x, y);
        if self.copy_on_select() {
            self.copy_selection();
        }
        self.dirty = true;
        true
    }

    /// Right button down in manual mode copies the selection and swallows the event.
    pub fn selection_right_down(&mut self) -> bool {
        if self.copy_on_select() || self.selection_text().is_none() {
            return false;
        }
        self.copy_selection();
        true
    }

    /// `handleSelectionKey`: keys while a selection exists. Returns true when consumed.
    pub fn text_selection_key(&mut self, s: &Stroke) -> bool {
        if self.selection.is_none() {
            return false;
        }
        if s.ctrl && s.name == "c" {
            if self.copy_on_select() {
                self.clear_selection();
                return false;
            }
            if self.copy_selection() {
                return true;
            }
            self.clear_selection();
            return false;
        }
        if s.name == "escape" && !s.ctrl {
            return self.clear_selection();
        }
        self.clear_selection();
        false
    }

    pub fn draw_selection(&self, buf: &mut Buffer) {
        let Some(s) = self.selection.as_ref().filter(|s| s.dragged) else { return };
        let area = buf.area;
        let ((_, y0), (_, y1)) = s.range();
        for y in y0..=y1.min(area.bottom().saturating_sub(1)) {
            for x in area.x..area.right() {
                if s.contains(x, y) {
                    let cell = &mut buf[(x, y)];
                    cell.modifier.insert(Modifier::REVERSED);
                }
            }
        }
    }
}
