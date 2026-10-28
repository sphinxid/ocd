//! The prompt textarea (OpenTUI `TextareaRenderable` over `EditBuffer`/`EditorView` with the
//! `ExtmarksController`): text, a cursor and extmarks addressed by *cursor offsets* (display cells,
//! one per line break, as `measureEditText` counts them), word-wrapped visual rows, virtual extmarks
//! that the cursor skips and that delete as a unit, and undo/redo snapshots.

use super::cell::{Color, StyledChunk, grapheme_width};
use super::text::{VLine, WrapMode, layout};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extmark {
    pub id: u32,
    pub start: usize,
    pub end: usize,
    pub virt: bool,
    pub type_id: u32,
}

#[derive(Clone, Debug, PartialEq)]
struct Snap {
    text: String,
    cursor: usize,
    marks: Vec<Extmark>,
}

#[derive(Clone, Debug, Default)]
pub struct TextArea {
    text: String,
    /// Cursor as a byte offset into `text` (always on a grapheme boundary).
    cursor: usize,
    pub marks: Vec<Extmark>,
    next_id: u32,
    undo: Vec<Snap>,
    redo: Vec<Snap>,
    /// Wrap width of the last layout (for visual movement).
    pub wrap_width: usize,
}

fn unit(g: &str) -> usize {
    if g == "\n" || g == "\r\n" || g == "\r" { 1 } else { grapheme_width(g) }
}

impl TextArea {
    pub fn new() -> TextArea {
        TextArea { next_id: 1, wrap_width: 1, ..Default::default() }
    }

    pub fn plain_text(&self) -> &str {
        &self.text
    }

    /// Cursor offset of byte `b`.
    pub fn offset_of(&self, b: usize) -> usize {
        self.text[..b.min(self.text.len())].graphemes(true).map(unit).sum()
    }

    /// Byte of cursor offset `o` (the first grapheme boundary at or after it).
    pub fn byte_of(&self, o: usize) -> usize {
        let mut n = 0;
        for (i, g) in self.text.grapheme_indices(true) {
            if n >= o {
                return i;
            }
            n += unit(g);
        }
        self.text.len()
    }

    pub fn end_offset(&self) -> usize {
        self.offset_of(self.text.len())
    }

    pub fn cursor_offset(&self) -> usize {
        self.offset_of(self.cursor)
    }

    fn snapshot(&mut self) {
        self.undo.push(Snap { text: self.text.clone(), cursor: self.cursor, marks: self.marks.clone() });
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn undo(&mut self) {
        if let Some(s) = self.undo.pop() {
            self.redo.push(Snap { text: self.text.clone(), cursor: self.cursor, marks: self.marks.clone() });
            self.text = s.text;
            self.cursor = s.cursor;
            self.marks = s.marks;
        }
    }

    pub fn redo(&mut self) {
        if let Some(s) = self.redo.pop() {
            self.undo.push(Snap { text: self.text.clone(), cursor: self.cursor, marks: self.marks.clone() });
            self.text = s.text;
            self.cursor = s.cursor;
            self.marks = s.marks;
        }
    }

    /// `setText`: replaces the content, clears extmarks and history, cursor at the end.
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.len();
        self.marks.clear();
        self.undo.clear();
        self.redo.clear();
    }

    pub fn get_text_range(&self, start: usize, end: usize) -> String {
        let (a, b) = (self.byte_of(start), self.byte_of(end));
        if a >= b { String::new() } else { self.text[a..b].to_string() }
    }

    // --- extmarks ------------------------------------------------------------------------------

    pub fn create_mark(&mut self, start: usize, end: usize, virt: bool, type_id: u32) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.marks.push(Extmark { id, start, end, virt, type_id });
        id
    }

    pub fn delete_mark(&mut self, id: u32) {
        self.marks.retain(|m| m.id != id);
    }

    pub fn clear_marks(&mut self) {
        self.marks.clear();
    }

    fn virtual_containing(&self, offset: usize) -> Option<Extmark> {
        self.marks.iter().find(|m| m.virt && offset >= m.start && offset < m.end).cloned()
    }

    fn adjust_after_insertion(&mut self, at: usize, len: usize) {
        for m in self.marks.iter_mut() {
            if m.start >= at {
                m.start += len;
                m.end += len;
            } else if m.end > at {
                m.end += len;
            }
        }
    }

    fn adjust_after_deletion(&mut self, at: usize, len: usize) {
        let end = at + len;
        let mut dead = Vec::new();
        for m in self.marks.iter_mut() {
            if m.end <= at {
                continue;
            }
            if m.start >= end {
                m.start -= len;
                m.end -= len;
            } else if m.start >= at && m.end <= end {
                dead.push(m.id);
            } else if m.start < at && m.end > end {
                m.end -= len;
            } else if m.start < at && m.end > at {
                m.end -= m.end.min(end) - at;
            } else if m.start < end && m.end > end {
                m.start = at;
                m.end -= len;
            }
        }
        self.marks.retain(|m| !dead.contains(&m.id));
    }

    // --- raw edits -----------------------------------------------------------------------------

    fn raw_set_cursor(&mut self, offset: usize) {
        self.cursor = self.byte_of(offset.min(self.end_offset()));
    }

    /// `setCursorByOffset` with the virtual-extmark rules.
    pub fn set_cursor_offset(&mut self, offset: usize) {
        let current = self.cursor_offset();
        if offset > current {
            if let Some(m) = self.virtual_containing(offset)
                && current <= m.start
            {
                self.raw_set_cursor(m.end);
                return;
            }
        } else if let Some(m) = self.marks.iter().find(|m| m.virt && current >= m.end && offset < m.end && offset >= m.start) {
            let s = m.start;
            self.raw_set_cursor(s.saturating_sub(1));
            return;
        }
        self.raw_set_cursor(offset);
    }

    pub fn insert_text(&mut self, s: &str) {
        self.snapshot();
        let at = self.cursor_offset();
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        let len = self.cursor_offset() - at;
        if len > 0 {
            self.adjust_after_insertion(at, len);
        }
    }

    /// Delete `[start, end)` (offsets).
    pub fn delete_range(&mut self, start: usize, end: usize) {
        if end <= start {
            return;
        }
        self.snapshot();
        let (a, b) = (self.byte_of(start), self.byte_of(end));
        self.text.replace_range(a..b, "");
        let cur = self.cursor;
        self.cursor = if cur >= b { cur - (b - a) } else if cur > a { a } else { cur };
        self.adjust_after_deletion(start, end - start);
    }

    fn prev_byte(&self, b: usize) -> usize {
        self.text[..b].grapheme_indices(true).next_back().map(|(i, _)| i).unwrap_or(0)
    }

    fn next_byte(&self, b: usize) -> usize {
        self.text[b..].graphemes(true).next().map(|g| b + g.len()).unwrap_or(b)
    }

    pub fn delete_char_backward(&mut self) {
        let current = self.cursor_offset();
        if current == 0 {
            return;
        }
        if let Some(m) = self.virtual_containing(current - 1)
            && current == m.end
        {
            self.delete_range(m.start, m.end);
            self.delete_mark(m.id);
            return;
        }
        let a = self.prev_byte(self.cursor);
        let start = self.offset_of(a);
        self.delete_range(start, current);
    }

    pub fn delete_char(&mut self) {
        let current = self.cursor_offset();
        if let Some(m) = self.virtual_containing(current)
            && current == m.start
        {
            self.delete_range(m.start, m.end);
            self.delete_mark(m.id);
            return;
        }
        let b = self.next_byte(self.cursor);
        let end = self.offset_of(b);
        self.delete_range(current, end);
    }

    pub fn move_left(&mut self) {
        let current = self.cursor_offset();
        if current == 0 {
            return;
        }
        if let Some(m) = self.virtual_containing(current - 1)
            && current >= m.end
        {
            self.set_cursor_offset(m.start.saturating_sub(1));
            return;
        }
        self.cursor = self.prev_byte(self.cursor);
    }

    pub fn move_right(&mut self) {
        let current = self.cursor_offset();
        if self.cursor >= self.text.len() {
            return;
        }
        if let Some(m) = self.virtual_containing(current + 1)
            && current <= m.start
        {
            self.raw_set_cursor(m.end);
            return;
        }
        self.cursor = self.next_byte(self.cursor);
    }

    fn line_bounds(&self) -> (usize, usize) {
        let start = self.text[..self.cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let end = self.text[self.cursor..].find('\n').map(|i| self.cursor + i).unwrap_or(self.text.len());
        (start, end)
    }

    pub fn line_home(&mut self) {
        let (s, _) = self.line_bounds();
        let o = self.offset_of(s);
        self.set_cursor_offset(o);
    }

    pub fn line_end(&mut self) {
        let (_, e) = self.line_bounds();
        let o = self.offset_of(e);
        self.set_cursor_offset(o);
    }

    pub fn buffer_home(&mut self) {
        self.set_cursor_offset(0);
    }

    pub fn buffer_end(&mut self) {
        let o = self.end_offset();
        self.set_cursor_offset(o);
    }

    fn is_word(g: &str) -> bool {
        g.chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_')
    }

    fn word_forward_byte(&self) -> usize {
        let rest: Vec<(usize, &str)> = self.text[self.cursor..].grapheme_indices(true).collect();
        let mut i = 0;
        while i < rest.len() && !Self::is_word(rest[i].1) {
            i += 1;
        }
        while i < rest.len() && Self::is_word(rest[i].1) {
            i += 1;
        }
        rest.get(i).map(|(o, _)| self.cursor + o).unwrap_or(self.text.len())
    }

    fn word_backward_byte(&self) -> usize {
        let before: Vec<(usize, &str)> = self.text[..self.cursor].grapheme_indices(true).collect();
        let mut i = before.len();
        while i > 0 && !Self::is_word(before[i - 1].1) {
            i -= 1;
        }
        while i > 0 && Self::is_word(before[i - 1].1) {
            i -= 1;
        }
        before.get(i).map(|(o, _)| *o).unwrap_or(0)
    }

    pub fn word_forward(&mut self) {
        let o = self.offset_of(self.word_forward_byte());
        self.set_cursor_offset(o);
    }

    pub fn word_backward(&mut self) {
        let o = self.offset_of(self.word_backward_byte());
        self.set_cursor_offset(o);
    }

    pub fn delete_word_forward(&mut self) {
        let (a, b) = (self.cursor_offset(), self.offset_of(self.word_forward_byte()));
        self.delete_range(a, b);
    }

    pub fn delete_word_backward(&mut self) {
        let (a, b) = (self.offset_of(self.word_backward_byte()), self.cursor_offset());
        self.delete_range(a, b);
    }

    pub fn delete_to_line_end(&mut self) {
        let (_, e) = self.line_bounds();
        let (a, mut b) = (self.cursor_offset(), self.offset_of(e));
        if a == b && e < self.text.len() {
            b += 1;
        }
        self.delete_range(a, b);
    }

    pub fn delete_to_line_start(&mut self) {
        let (s, _) = self.line_bounds();
        let (a, b) = (self.offset_of(s), self.cursor_offset());
        self.delete_range(a, b);
    }

    pub fn delete_line(&mut self) {
        let (s, e) = self.line_bounds();
        let a = self.offset_of(s);
        let b = if e < self.text.len() { self.offset_of(e) + 1 } else { self.offset_of(e) };
        let a = if e >= self.text.len() && s > 0 { a - 1 } else { a };
        self.delete_range(a, b);
    }

    pub fn newline(&mut self) {
        self.insert_text("\n");
    }

    // --- layout --------------------------------------------------------------------------------

    /// Word-wrapped visual rows at `width`.
    pub fn rows(&self, width: usize, fg: Color) -> Vec<VLine> {
        layout(&[StyledChunk::plain(self.text.clone(), fg)], width.max(1), WrapMode::Word, false)
    }

    pub fn line_count(&self) -> usize {
        self.text.split('\n').count()
    }

    pub fn virtual_line_count(&self, width: usize) -> usize {
        self.rows(width, Color::WHITE).len()
    }

    /// Visual (row, column) of the cursor at `width`.
    pub fn visual_cursor(&self, width: usize) -> (usize, usize) {
        let rows = self.rows(width, Color::WHITE);
        let mut best = 0;
        for (i, r) in rows.iter().enumerate() {
            let line_start = self.text[..r.start.min(self.text.len())].rfind('\n').map(|j| j + 1).unwrap_or(0);
            let cursor_line = self.text[..self.cursor].rfind('\n').map(|j| j + 1).unwrap_or(0);
            if line_start == cursor_line && r.start <= self.cursor {
                best = i;
            }
        }
        let r = &rows[best];
        let col = self.offset_of(self.cursor).saturating_sub(self.offset_of(r.start.min(self.cursor)));
        (best, col)
    }

    fn offset_at_visual(&self, rows: &[VLine], row: usize, col: usize) -> usize {
        let r = &rows[row.min(rows.len() - 1)];
        let mut used = 0;
        let mut b = r.start;
        for g in self.text[r.start..r.end.max(r.start)].graphemes(true) {
            let w = unit(g);
            if used + w > col {
                break;
            }
            used += w;
            b += g.len();
        }
        self.offset_of(b)
    }

    /// `moveCursorUp` (visual) with the virtual-extmark adjustment; false on the first row.
    pub fn move_up(&mut self, width: usize) -> bool {
        let rows = self.rows(width, Color::WHITE);
        let (row, col) = self.visual_cursor(width);
        if row == 0 {
            return false;
        }
        let o = self.offset_at_visual(&rows, row - 1, col);
        self.raw_set_cursor(o);
        if let Some(m) = self.virtual_containing(o) {
            if o - m.start < m.end - o {
                self.raw_set_cursor(m.start.saturating_sub(1));
            } else {
                self.raw_set_cursor(m.end);
            }
        }
        true
    }

    pub fn move_down(&mut self, width: usize) -> bool {
        let rows = self.rows(width, Color::WHITE);
        let (row, col) = self.visual_cursor(width);
        if row + 1 >= rows.len() {
            return false;
        }
        let current = self.cursor_offset();
        let o = self.offset_at_visual(&rows, row + 1, col);
        self.raw_set_cursor(o);
        if let Some(m) = self.virtual_containing(o) {
            if o - m.start < m.end - o {
                let adjusted = m.start as isize - 1;
                self.raw_set_cursor(if adjusted <= current as isize { m.end } else { adjusted as usize });
            } else {
                self.raw_set_cursor(m.end);
            }
        }
        true
    }

    /// Visual line home/end.
    pub fn visual_home(&mut self, width: usize) {
        let rows = self.rows(width, Color::WHITE);
        let (row, _) = self.visual_cursor(width);
        let o = self.offset_of(rows[row].start);
        self.set_cursor_offset(o);
    }

    pub fn visual_end(&mut self, width: usize) {
        let rows = self.rows(width, Color::WHITE);
        let (row, _) = self.visual_cursor(width);
        let o = self.offset_of(rows[row].end);
        self.set_cursor_offset(o);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_marks_are_atomic() {
        let mut t = TextArea::new();
        t.insert_text("see @a.ts now");
        let id = t.create_mark(4, 9, true, 1);
        t.set_cursor_offset(9);
        t.move_left();
        assert_eq!(t.cursor_offset(), 3);
        t.move_right();
        assert_eq!(t.cursor_offset(), 9);
        t.delete_char_backward();
        assert_eq!(t.plain_text(), "see  now");
        assert!(t.marks.iter().all(|m| m.id != id));
    }

    #[test]
    fn insertion_shifts_marks() {
        let mut t = TextArea::new();
        t.insert_text("ab @x cd");
        t.create_mark(3, 5, true, 1);
        t.set_cursor_offset(0);
        t.insert_text("zz");
        assert_eq!((t.marks[0].start, t.marks[0].end), (5, 7));
        assert_eq!(t.get_text_range(5, 7), "@x");
    }

    #[test]
    fn visual_cursor_wraps() {
        let mut t = TextArea::new();
        t.insert_text("hello world");
        assert_eq!(t.visual_cursor(8), (1, 5));
        t.set_cursor_offset(3);
        assert_eq!(t.visual_cursor(8), (0, 3));
        assert!(t.move_down(8));
        assert_eq!(t.visual_cursor(8), (1, 3));
    }
}
