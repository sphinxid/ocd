//! Prompt editor model: multiline textarea with atomic chips (extmarks), undo/redo, history
//! restore, paste collapsing and conversion to the API prompt payload (spec §4).

use super::text::{Seg, Wrap, wrap};
use crate::state::{PastedSource, PastedText, PromptInfo};
use crate::util::locale::{graphemes, width};
use ratatui::style::Style;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChipKind {
    File,
    Agent,
    Skill,
    Paste,
}

/// A chip covering `start..end` (byte offsets) of the text; atomic for the cursor.
#[derive(Clone, Debug, PartialEq)]
pub struct Chip {
    pub start: usize,
    pub end: usize,
    pub kind: ChipKind,
    /// Payload: file attachment / agent / skill object, or the pasted full text.
    pub data: Value,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Snapshot {
    text: String,
    cursor: usize,
    chips: Vec<Chip>,
}

#[derive(Clone, Debug, Default)]
pub struct Editor {
    pub text: String,
    pub cursor: usize,
    pub chips: Vec<Chip>,
    pub shell: bool,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Preferred visual column for up/down.
    goal: Option<usize>,
}

fn prev_boundary(s: &str, i: usize) -> usize {
    let mut j = i.saturating_sub(1);
    while j > 0 && !s.is_char_boundary(j) {
        j -= 1;
    }
    // Step over a whole grapheme.
    let gs: Vec<(usize, &str)> = unicode_segmentation::UnicodeSegmentation::grapheme_indices(s, true).collect();
    gs.iter().rev().find(|(o, _)| *o < i).map(|(o, _)| *o).unwrap_or(j)
}

fn next_boundary(s: &str, i: usize) -> usize {
    let gs: Vec<(usize, &str)> = unicode_segmentation::UnicodeSegmentation::grapheme_indices(s, true).collect();
    gs.iter().find(|(o, _)| *o >= i).map(|(o, g)| o + g.len()).unwrap_or(s.len())
}

impl Editor {
    pub fn new() -> Editor {
        Editor::default()
    }

    fn snapshot(&mut self) {
        self.undo.push(Snapshot { text: self.text.clone(), cursor: self.cursor, chips: self.chips.clone() });
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn undo(&mut self) {
        if let Some(s) = self.undo.pop() {
            self.redo.push(Snapshot { text: self.text.clone(), cursor: self.cursor, chips: self.chips.clone() });
            self.text = s.text;
            self.cursor = s.cursor;
            self.chips = s.chips;
        }
    }

    pub fn redo(&mut self) {
        if let Some(s) = self.redo.pop() {
            self.undo.push(Snapshot { text: self.text.clone(), cursor: self.cursor, chips: self.chips.clone() });
            self.text = s.text;
            self.cursor = s.cursor;
            self.chips = s.chips;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        if !self.text.is_empty() {
            self.snapshot();
        }
        self.text.clear();
        self.cursor = 0;
        self.chips.clear();
        self.goal = None;
    }

    fn chip_at(&self, pos: usize) -> Option<&Chip> {
        self.chips.iter().find(|c| pos > c.start && pos < c.end)
    }

    /// Shift chips after an edit at `at` of `delta` bytes; drop chips overlapping removed ranges.
    fn shift(&mut self, at: usize, removed: usize, inserted: usize) {
        let end = at + removed;
        self.chips.retain(|c| !(removed > 0 && c.start < end && c.end > at));
        for c in self.chips.iter_mut() {
            if c.start >= end {
                c.start = c.start - removed + inserted;
                c.end = c.end - removed + inserted;
            }
        }
    }

    pub fn insert(&mut self, s: &str) {
        self.snapshot();
        self.insert_raw(s);
    }

    fn insert_raw(&mut self, s: &str) {
        if let Some(c) = self.chip_at(self.cursor) {
            self.cursor = c.end;
        }
        let at = self.cursor;
        self.text.insert_str(at, s);
        self.shift(at, 0, s.len());
        self.cursor = at + s.len();
        self.goal = None;
    }

    /// Insert a chip with its text at the cursor plus one trailing space.
    pub fn insert_chip(&mut self, text: &str, kind: ChipKind, data: Value, trailing_space: bool) {
        self.snapshot();
        if let Some(c) = self.chip_at(self.cursor) {
            self.cursor = c.end;
        }
        let at = self.cursor;
        self.insert_raw(text);
        self.chips.push(Chip { start: at, end: at + text.len(), kind, data });
        self.chips.sort_by_key(|c| c.start);
        if trailing_space && !self.text[self.cursor..].starts_with(' ') {
            self.insert_raw(" ");
        }
    }

    /// Replace `start..end` (bytes) with `text`.
    pub fn replace_range(&mut self, start: usize, end: usize, text: &str) {
        self.snapshot();
        let end = end.min(self.text.len());
        let start = start.min(end);
        self.text.replace_range(start..end, text);
        self.shift(start, end - start, text.len());
        self.cursor = start + text.len();
    }

    pub fn delete_range(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        // Expand to include chips partially covered.
        let mut s = start;
        let mut e = end;
        for c in &self.chips {
            if c.start < e && c.end > s {
                s = s.min(c.start);
                e = e.max(c.end);
            }
        }
        self.snapshot();
        self.text.replace_range(s..e, "");
        self.shift(s, e - s, 0);
        self.cursor = s;
        self.goal = None;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        if let Some(c) = self.chips.iter().find(|c| c.end == self.cursor).cloned() {
            self.delete_range(c.start, c.end);
            return;
        }
        let p = prev_boundary(&self.text, self.cursor);
        self.delete_range(p, self.cursor);
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        if let Some(c) = self.chips.iter().find(|c| c.start == self.cursor).cloned() {
            self.delete_range(c.start, c.end);
            return;
        }
        let n = next_boundary(&self.text, self.cursor);
        self.delete_range(self.cursor, n);
    }

    pub fn left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = prev_boundary(&self.text, self.cursor);
        if let Some(c) = self.chip_at(self.cursor) {
            self.cursor = c.start;
        }
        self.goal = None;
    }

    pub fn right(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        self.cursor = next_boundary(&self.text, self.cursor);
        if let Some(c) = self.chip_at(self.cursor) {
            self.cursor = c.end;
        }
        self.goal = None;
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }
    fn line_end(&self) -> usize {
        self.text[self.cursor..].find('\n').map(|i| self.cursor + i).unwrap_or(self.text.len())
    }
    pub fn home(&mut self) {
        self.cursor = self.line_start();
        self.goal = None;
    }
    pub fn end(&mut self) {
        self.cursor = self.line_end();
        self.goal = None;
    }
    pub fn buffer_home(&mut self) {
        self.cursor = 0;
    }
    pub fn buffer_end(&mut self) {
        self.cursor = self.text.len();
    }

    fn is_word(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    fn word_left_pos(&self) -> usize {
        let before: Vec<(usize, char)> = self.text[..self.cursor].char_indices().collect();
        let mut i = before.len();
        while i > 0 && !Self::is_word(before[i - 1].1) {
            i -= 1;
        }
        while i > 0 && Self::is_word(before[i - 1].1) {
            i -= 1;
        }
        before.get(i).map(|(o, _)| *o).unwrap_or(if i == 0 { 0 } else { self.cursor })
    }

    fn word_right_pos(&self) -> usize {
        let mut pos = self.cursor;
        let rest: Vec<(usize, char)> = self.text[self.cursor..].char_indices().collect();
        let mut i = 0;
        while i < rest.len() && !Self::is_word(rest[i].1) {
            i += 1;
        }
        while i < rest.len() && Self::is_word(rest[i].1) {
            i += 1;
        }
        pos += rest.get(i).map(|(o, _)| *o).unwrap_or(self.text.len() - self.cursor);
        pos
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_left_pos();
        if let Some(c) = self.chip_at(self.cursor) {
            self.cursor = c.start;
        }
    }
    pub fn word_right(&mut self) {
        self.cursor = self.word_right_pos();
        if let Some(c) = self.chip_at(self.cursor) {
            self.cursor = c.end;
        }
    }
    pub fn delete_word_back(&mut self) {
        let p = self.word_left_pos();
        self.delete_range(p, self.cursor);
    }
    pub fn delete_word_forward(&mut self) {
        let p = self.word_right_pos();
        let c = self.cursor;
        self.delete_range(c, p);
    }
    pub fn delete_to_line_end(&mut self) {
        let e = self.line_end();
        let c = self.cursor;
        if e == c && c < self.text.len() {
            self.delete_range(c, c + 1);
        } else {
            self.delete_range(c, e);
        }
    }
    pub fn delete_to_line_start(&mut self) {
        let s = self.line_start();
        let c = self.cursor;
        self.delete_range(s, c);
    }
    pub fn delete_line(&mut self) {
        let s = self.line_start();
        let e = self.line_end();
        let e = if e < self.text.len() { e + 1 } else { e };
        self.delete_range(s, e);
    }

    /// Visual layout: lines of (byte start, byte end) at the given width (char wrap like the
    /// OpenTUI textarea in word mode is approximated by word wrap).
    pub fn layout(&self, w: usize) -> Vec<(usize, usize)> {
        let w = w.max(1);
        let mut out = Vec::new();
        let mut line_start = 0;
        for (li, line) in self.text.split('\n').enumerate() {
            let _ = li;
            let base = line_start;
            let gs: Vec<(usize, &str)> = unicode_segmentation::UnicodeSegmentation::grapheme_indices(line, true).collect();
            if gs.is_empty() {
                out.push((base, base));
            }
            let mut start = 0usize; // index into gs
            while start < gs.len() {
                let mut used = 0;
                let mut end = start;
                let mut last_space: Option<usize> = None;
                while end < gs.len() {
                    let gw = width(gs[end].1);
                    if used + gw > w {
                        break;
                    }
                    if gs[end].1 == " " {
                        last_space = Some(end);
                    }
                    used += gw;
                    end += 1;
                }
                if end < gs.len()
                    && let Some(sp) = last_space
                    && sp > start
                {
                    end = sp + 1;
                }
                if end == start {
                    end = start + 1;
                }
                let bs = base + gs[start].0;
                let be = if end < gs.len() { base + gs[end].0 } else { base + line.len() };
                out.push((bs, be));
                start = end;
            }
            line_start += line.len() + 1;
        }
        if out.is_empty() {
            out.push((0, 0));
        }
        out
    }

    /// (visual row, column) of the cursor.
    pub fn cursor_pos(&self, w: usize) -> (usize, usize) {
        let lay = self.layout(w);
        for (i, (s, e)) in lay.iter().enumerate() {
            let next_start = lay.get(i + 1).map(|n| n.0);
            let at_end_belongs = next_start.is_none_or(|ns| ns > *e);
            if self.cursor >= *s && (self.cursor < *e || (self.cursor == *e && at_end_belongs)) {
                return (i, width(&self.text[*s..self.cursor]));
            }
        }
        let last = lay.len() - 1;
        (last, width(&self.text[lay[last].0..self.cursor.min(self.text.len())]))
    }

    fn offset_at(&self, w: usize, row: usize, col: usize) -> usize {
        let lay = self.layout(w);
        let (s, e) = lay[row.min(lay.len() - 1)];
        let mut used = 0;
        for (o, g) in unicode_segmentation::UnicodeSegmentation::grapheme_indices(&self.text[s..e], true) {
            let gw = width(g);
            if used + gw > col || g == "\n" {
                return s + o;
            }
            used += gw;
        }
        e
    }

    /// Move one visual line up; returns false at the first row.
    pub fn up(&mut self, w: usize) -> bool {
        let (row, col) = self.cursor_pos(w);
        if row == 0 {
            return false;
        }
        let goal = *self.goal.get_or_insert(col);
        self.cursor = self.offset_at(w, row - 1, goal);
        if let Some(c) = self.chip_at(self.cursor) {
            self.cursor = c.start;
        }
        true
    }

    pub fn down(&mut self, w: usize) -> bool {
        let (row, col) = self.cursor_pos(w);
        if row + 1 >= self.layout(w).len() {
            return false;
        }
        let goal = *self.goal.get_or_insert(col);
        self.cursor = self.offset_at(w, row + 1, goal);
        if let Some(c) = self.chip_at(self.cursor) {
            self.cursor = c.end;
        }
        true
    }

    /// Display-width offset (`promptOffsetWidth`) of a byte position.
    pub fn display_offset(&self, byte: usize) -> usize {
        let mut n = 0;
        for g in graphemes(&self.text[..byte.min(self.text.len())]) {
            n += if g == "\n" { 1 } else { width(g) };
        }
        n
    }

    fn byte_of_display(&self, off: usize) -> usize {
        let mut n = 0;
        for (o, g) in unicode_segmentation::UnicodeSegmentation::grapheme_indices(self.text.as_str(), true) {
            if n >= off {
                return o;
            }
            n += if g == "\n" { 1 } else { width(g) };
        }
        self.text.len()
    }

    /// Styled segments per visual line, with chip styles.
    pub fn render(&self, w: usize, base: Style, chip_style: &dyn Fn(ChipKind) -> Style) -> Vec<Vec<Seg>> {
        let lay = self.layout(w);
        lay.iter()
            .map(|(s, e)| {
                let mut segs: Vec<Seg> = Vec::new();
                let mut pos = *s;
                let slice_end = *e;
                while pos < slice_end {
                    let chip = self.chips.iter().find(|c| pos >= c.start && pos < c.end);
                    let (end, style) = match chip {
                        Some(c) => (c.end.min(slice_end), chip_style(c.kind)),
                        None => {
                            let next = self.chips.iter().filter(|c| c.start > pos).map(|c| c.start).min().unwrap_or(slice_end).min(slice_end);
                            (next, base)
                        }
                    };
                    let t = self.text[pos..end].trim_end_matches('\n');
                    if !t.is_empty() {
                        segs.push(Seg::new(t.to_string(), style));
                    }
                    pos = end;
                }
                segs
            })
            .collect()
    }

    /// Text with pasted chips expanded (`inputText`).
    pub fn input_text(&self) -> String {
        let mut out = self.text.clone();
        let mut chips: Vec<&Chip> = self.chips.iter().filter(|c| c.kind == ChipKind::Paste && c.data.get("text").is_some()).collect();
        chips.sort_by_key(|c| std::cmp::Reverse(c.start));
        for c in chips {
            let t = c.data.get("text").and_then(|t| t.as_str()).unwrap_or("");
            out.replace_range(c.start..c.end, t);
        }
        out
    }

    fn mention(&self, c: &Chip) -> Value {
        json!({ "start": self.display_offset(c.start), "end": self.display_offset(c.end), "text": &self.text[c.start..c.end] })
    }

    /// Export to `PromptInfo` (history/stash format).
    pub fn info(&self) -> PromptInfo {
        let mut p = PromptInfo::empty();
        p.text = self.text.clone();
        p.mode = Some(if self.shell { "shell".into() } else { "normal".into() });
        for c in &self.chips {
            let mention = self.mention(c);
            match c.kind {
                ChipKind::File => {
                    let mut v = c.data.clone();
                    v["mention"] = mention;
                    p.files.push(v);
                }
                ChipKind::Agent => {
                    let mut v = c.data.clone();
                    v["mention"] = mention;
                    p.agents.push(v);
                }
                ChipKind::Skill => {
                    let mut v = c.data.clone();
                    v["mention"] = mention;
                    p.skills.get_or_insert_default().push(v);
                }
                ChipKind::Paste => {
                    if let Some(t) = c.data.get("text").and_then(|t| t.as_str()) {
                        p.pasted.push(PastedText {
                            text: t.to_string(),
                            source: PastedSource {
                                start: self.display_offset(c.start) as f64,
                                end: self.display_offset(c.end) as f64,
                                text: self.text[c.start..c.end].to_string(),
                            },
                        });
                    } else {
                        // Attachment chip ([Image N] / [PDF N]) stored as a file part.
                        let mut v = c.data.clone();
                        v["mention"] = mention;
                        p.files.push(v);
                    }
                }
            }
        }
        p
    }

    /// Restore from `PromptInfo`; chips are rebuilt from mention ranges.
    pub fn set(&mut self, p: &PromptInfo) {
        self.snapshot();
        self.text = p.text.clone();
        self.chips.clear();
        self.shell = p.mode.as_deref() == Some("shell");
        let range = |m: &Value| -> Option<(usize, usize)> {
            let s = m.get("start")?.as_f64()? as usize;
            let e = m.get("end")?.as_f64()? as usize;
            Some((s, e))
        };
        let add = |kind: ChipKind, v: &Value, ed: &mut Editor| {
            if let Some(m) = v.get("mention")
                && let Some((s, e)) = range(m)
            {
                let bs = ed.byte_of_display(s);
                let be = ed.byte_of_display(e);
                if bs < be && be <= ed.text.len() {
                    let mut data = v.clone();
                    if let Some(o) = data.as_object_mut() {
                        o.remove("mention");
                    }
                    let kind = if kind == ChipKind::File && data.get("uri").and_then(|u| u.as_str()).is_some_and(|u| u.starts_with("data:")) { ChipKind::Paste } else { kind };
                    ed.chips.push(Chip { start: bs, end: be, kind, data });
                }
            }
        };
        for f in &p.files {
            add(ChipKind::File, f, self);
        }
        for a in &p.agents {
            add(ChipKind::Agent, a, self);
        }
        for s in p.skills.iter().flatten() {
            add(ChipKind::Skill, s, self);
        }
        for pt in &p.pasted {
            let bs = self.byte_of_display(pt.source.start as usize);
            let be = self.byte_of_display(pt.source.end as usize);
            if bs < be {
                self.chips.push(Chip { start: bs, end: be, kind: ChipKind::Paste, data: json!({ "text": pt.text }) });
            }
        }
        self.chips.sort_by_key(|c| c.start);
        self.cursor = self.text.len();
    }

    /// API payload parts: (files, agents, skills).
    pub fn api_parts(&self) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
        let (mut files, mut agents, mut skills) = (Vec::new(), Vec::new(), Vec::new());
        for c in &self.chips {
            let mention = self.mention(c);
            match c.kind {
                ChipKind::File => {
                    let mut v = json!({ "uri": c.data.get("uri").cloned().unwrap_or(Value::Null), "mention": mention });
                    if let Some(n) = c.data.get("name") {
                        v["name"] = n.clone();
                    }
                    files.push(v);
                }
                ChipKind::Paste if c.data.get("uri").is_some() => {
                    let mut v = json!({ "uri": c.data["uri"], "mention": mention });
                    if let Some(n) = c.data.get("name") {
                        v["name"] = n.clone();
                    }
                    files.push(v);
                }
                ChipKind::Agent => agents.push(json!({ "name": c.data.get("name").cloned().unwrap_or(Value::Null), "mention": mention })),
                ChipKind::Skill => skills.push(json!({ "id": c.data.get("id").cloned().unwrap_or(Value::Null), "mention": mention })),
                ChipKind::Paste => {}
            }
        }
        (files, agents, skills)
    }

    /// Paste a large block as a collapsed chip, or expand an identical adjacent paste chip.
    pub fn paste(&mut self, raw: &str, compact: bool) {
        let text = raw.replace("\r\n", "\n").replace('\r', "\n");
        let trimmed = text.trim();
        let lines = trimmed.matches('\n').count() + 1;
        if compact && (lines >= 3 || trimmed.chars().count() > 150) {
            // Expand an identical paste chip right before the cursor.
            if let Some(c) = self
                .chips
                .iter()
                .find(|c| c.kind == ChipKind::Paste && (c.end == self.cursor || c.end + 1 == self.cursor) && c.data.get("text").and_then(|t| t.as_str()) == Some(text.as_str()))
                .cloned()
            {
                self.chips.retain(|x| *x != c);
                self.replace_range(c.start, c.end, &text);
                return;
            }
            let label = format!("[Pasted ~{lines} lines]");
            self.insert_chip(&label, ChipKind::Paste, json!({ "text": text }), true);
            return;
        }
        self.insert(&text);
    }

    /// Label for the next attachment chip of a kind (`[Image N]`).
    pub fn next_attachment_label(&self, kind: &str) -> String {
        let re = regex::Regex::new(&format!(r"^\[{kind} (\d+)\]$")).unwrap();
        let max = self
            .chips
            .iter()
            .filter_map(|c| re.captures(&self.text[c.start..c.end]).and_then(|m| m[1].parse::<usize>().ok()))
            .max()
            .unwrap_or(0);
        format!("[{kind} {}]", max + 1)
    }

    /// Expand a paste chip containing the cursor (click-to-expand).
    pub fn expand_chip_at_cursor(&mut self) -> bool {
        if let Some(c) = self.chips.iter().find(|c| c.kind == ChipKind::Paste && self.cursor >= c.start && self.cursor <= c.end && c.data.get("text").is_some()).cloned() {
            let t = c.data["text"].as_str().unwrap_or("").to_string();
            self.chips.retain(|x| *x != c);
            self.replace_range(c.start, c.end, &t);
            return true;
        }
        false
    }
}

/// Wrap helper for placeholder lines.
pub fn placeholder_lines(text: &str, w: usize, style: Style) -> Vec<Vec<Seg>> {
    wrap(&[Seg::new(text, style)], w, Wrap::Word)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chips_are_atomic() {
        let mut e = Editor::new();
        e.insert("see ");
        e.insert_chip("@src/a.ts", ChipKind::File, json!({"uri": "file:///x/src/a.ts"}), true);
        assert_eq!(e.text, "see @src/a.ts ");
        e.left(); // before space
        e.left(); // jumps over chip
        assert_eq!(e.cursor, 4);
        e.right();
        assert_eq!(e.cursor, 13);
        e.backspace(); // deletes whole chip
        assert_eq!(e.text, "see  ");
        assert!(e.chips.is_empty());
        e.undo();
        assert_eq!(e.text, "see @src/a.ts ");
        assert_eq!(e.chips.len(), 1);
    }

    #[test]
    fn paste_collapse_and_roundtrip() {
        let mut e = Editor::new();
        e.paste("a\nb\nc", true);
        assert_eq!(e.text, "[Pasted ~3 lines] ");
        assert_eq!(e.input_text(), "a\nb\nc ");
        let info = e.info();
        assert_eq!(info.pasted.len(), 1);
        assert_eq!(info.pasted[0].source.end, 17.0);
        let mut f = Editor::new();
        f.set(&info);
        assert_eq!(f.chips.len(), 1);
        assert_eq!(f.input_text(), "a\nb\nc ");
        // Pasting the same text right after the chip expands it.
        let mut g = Editor::new();
        g.paste("x\ny\nz", true);
        g.paste("x\ny\nz", true);
        assert_eq!(g.text, "x\ny\nz ");
        let mut h = Editor::new();
        h.paste("short", true);
        assert_eq!(h.text, "short");
    }

    #[test]
    fn visual_movement() {
        let mut e = Editor::new();
        e.insert("hello world foo");
        assert_eq!(e.layout(6).len(), 3);
        assert_eq!(e.cursor_pos(6), (2, 3));
        assert!(e.up(6));
        assert_eq!(e.cursor_pos(6), (1, 3));
        e.buffer_home();
        assert!(!e.up(6));
        e.insert("x\n");
        assert_eq!(e.cursor_pos(40), (1, 0));
        e.word_right();
        assert_eq!(&e.text[..e.cursor], "x\nhello");
        e.delete_word_back();
        assert_eq!(e.text, "x\n world foo");
    }

    #[test]
    fn attachment_numbering() {
        let mut e = Editor::new();
        e.insert_chip("[Image 1]", ChipKind::Paste, json!({"uri": "data:image/png;base64,AA"}), true);
        assert_eq!(e.next_attachment_label("Image"), "[Image 2]");
        let (files, _, _) = e.api_parts();
        assert_eq!(files.len(), 1);
    }
}
