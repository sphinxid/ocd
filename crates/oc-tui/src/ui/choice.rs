//! `StringChoiceField` (`component/dialog-integration.tsx`): a string form field with options and a
//! typed answer, as a numbered list (`1. label`), a "Type your own answer" row that turns into an
//! input, and `Skip` for optional fields. Used by the integration sign-in forms.

use super::dialog::{DIALOG_INSET, Footer, FooterItem, footer_lines};
use super::form;
use super::text::*;
use crate::keymap::{Keybinds, Match, Stroke};
use crate::theme::Theme;
use crate::util::locale::{self, width as sw};
use serde_json::Value;

#[derive(Clone, Debug, Default)]
pub struct ChoiceField {
    /// The form field (`FormField` JSON).
    pub field: Value,
    pub selected: usize,
    pub editing: bool,
    pub text: String,
    pub cursor: usize,
    pub error: Option<String>,
    /// First visible row of the scrollbox.
    pub scroll: usize,
}

/// What a key did.
#[derive(Clone, Debug, PartialEq)]
pub enum ChoiceKey {
    None,
    Close,
    /// The answer: a row value, the typed text, or `None` for Skip.
    Submit(Option<Value>),
}

impl ChoiceField {
    pub fn new(field: Value) -> ChoiceField {
        let rows = form::rows(&field);
        let default = field.get("default").filter(|d| !d.is_null()).cloned();
        // `formSelected`: the default's row, the custom row for another string, else the first.
        let selected = match &default {
            Some(d) if !d.is_array() => match rows.iter().position(|(v, _, _)| v == d) {
                Some(i) => i,
                None if d.is_string() && form::custom(&field) => rows.len(),
                None => 0,
            },
            _ => 0,
        };
        // `formInitialValues(...).custom`: a default that is not one of the options.
        let text = match default.as_ref().and_then(|d| d.as_str()) {
            Some(d) if !rows.iter().any(|(v, _, _)| v.as_str() == Some(d)) => d.to_string(),
            _ => String::new(),
        };
        let cursor = text.len();
        ChoiceField { field, selected, editing: false, text, cursor, error: None, scroll: 0 }
    }

    fn required(&self) -> bool {
        self.field.get("required").and_then(|r| r.as_bool()) == Some(true)
    }

    fn count(&self) -> usize {
        form::rows(&self.field).len() + if self.required() { 1 } else { 2 }
    }

    fn submit(&mut self, value: Option<Value>) -> ChoiceKey {
        if let Some(invalid) = form::validate(&self.field, value.as_ref()) {
            self.error = Some(invalid);
            return ChoiceKey::None;
        }
        ChoiceKey::Submit(value)
    }

    /// `select(index)`: a row answers, the custom row starts typing, `Skip` answers nothing.
    pub fn select(&mut self, index: usize) -> ChoiceKey {
        let rows = form::rows(&self.field);
        self.selected = index;
        self.error = None;
        if index == rows.len() {
            self.editing = true;
            self.cursor = self.text.len();
            return ChoiceKey::None;
        }
        // `String(rows[index].value)`.
        let value = rows.get(index).map(|(v, _, _)| Value::String(v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string())));
        self.submit(value)
    }

    fn typed(&self) -> Option<Value> {
        let t = self.text.trim();
        if t.is_empty() { None } else { Some(Value::String(t.to_string())) }
    }

    fn back(&mut self) {
        self.editing = false;
        self.error = None;
    }

    fn changed(&mut self) {
        if self.error.is_some() && form::validate(&self.field, self.typed().as_ref()).is_none() {
            self.error = None;
        }
    }

    pub fn handle_key(&mut self, s: &Stroke, kb: &Keybinds) -> ChoiceKey {
        let is = |id: &str| kb.match_command(id, std::slice::from_ref(s)) == Match::Full;
        if !self.editing {
            if s.name == "escape" || (s.name == "c" && s.ctrl) {
                return ChoiceKey::Close;
            }
            let n = self.count();
            if is("dialog.select.prev") {
                self.selected = (self.selected + n - 1) % n;
            } else if is("dialog.select.next") {
                self.selected = (self.selected + 1) % n;
            } else if is("dialog.select.submit") {
                return self.select(self.selected);
            }
            return ChoiceKey::None;
        }
        if is("dialog.prompt.submit") {
            let v = self.typed();
            return self.submit(v);
        }
        match s.name.as_str() {
            "escape" => self.back(),
            "backspace" => {
                if self.cursor > 0 {
                    let prev = self.text[..self.cursor].char_indices().last().map(|(i, _)| i).unwrap_or(0);
                    self.text.replace_range(prev..self.cursor, "");
                    self.cursor = prev;
                    self.changed();
                }
            }
            "delete" => {
                if self.cursor < self.text.len() {
                    let next = self.text[self.cursor..].char_indices().nth(1).map(|(i, _)| self.cursor + i).unwrap_or(self.text.len());
                    self.text.replace_range(self.cursor..next, "");
                    self.changed();
                }
            }
            "left" => self.cursor = self.text[..self.cursor].char_indices().last().map(|(i, _)| i).unwrap_or(0),
            "right" => self.cursor = self.text[self.cursor..].char_indices().nth(1).map(|(i, _)| self.cursor + i).unwrap_or(self.text.len()),
            "home" => self.cursor = 0,
            "end" => self.cursor = self.text.len(),
            "a" if s.ctrl => self.cursor = 0,
            "e" if s.ctrl => self.cursor = self.text.len(),
            "c" if s.ctrl => {
                if self.text.is_empty() {
                    return ChoiceKey::Close;
                }
                self.text.clear();
                self.cursor = 0;
                self.changed();
            }
            _ => {
                if let Some(c) = s.is_char() {
                    self.text.insert(self.cursor, c);
                    self.cursor += c.len_utf8();
                    self.changed();
                }
            }
        }
        ChoiceKey::None
    }

    pub fn paste(&mut self, text: &str) {
        if !self.editing {
            return;
        }
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.changed();
    }
}

/// The dialog body: header, description, the numbered rows (a scrollbox), the error and the footer.
/// Returns the lines and the cursor while typing.
pub fn lines(c: &mut ChoiceField, w: usize, term_h: usize, narrow: bool, t: &Theme) -> (Vec<SLine>, Option<(u16, u16)>) {
    let inner = w.saturating_sub(DIALOG_INSET * 2).max(1);
    let ins = |l: SLine| l.prefixed(&[Seg::new(" ".repeat(DIALOG_INSET), Default::default())]);
    let title = form::label(&c.field);
    let close = "esc";
    let title_w = inner.saturating_sub(sw(close));
    let title = locale::take_width(&title, title_w);
    let gap = inner.saturating_sub(sw(&title) + sw(close));
    let mut out = vec![
        ins(SLine::new(vec![Seg::new(title, bold(fg(t.text.base))), seg(" ".repeat(gap), Default::default()), seg(close, fg(t.text.muted))]))
            .on_click(Some(Click::Custom("esc".into()))),
    ];
    out.push(SLine::blank());
    if let Some(d) = c.field.get("description").and_then(|d| d.as_str()).filter(|d| !d.is_empty()) {
        for l in wrap(&[seg(d.to_string(), fg(t.text.muted))], inner, Wrap::Word) {
            out.push(ins(SLine::new(l)));
        }
        out.push(SLine::blank());
    }
    let rows = form::rows(&c.field);
    let color = |i: usize| if c.selected == i { t.text.formfield.focused } else { t.text.formfield.base };
    // Entries of the scrollbox: (lines, cursor column within the first line when typing).
    let mut entries: Vec<(Vec<SLine>, Option<usize>)> = Vec::new();
    for (i, (_, label, desc)) in rows.iter().enumerate() {
        let click = Some(Click::Custom(format!("opt:{i}")));
        let mut lines: Vec<SLine> = wrap(&[seg(format!("{}. {label}", i + 1), fg(color(i)))], inner, Wrap::Word).into_iter().map(|l| SLine::new(l).on_click(click.clone())).collect();
        if let Some(d) = desc {
            for l in wrap(&[seg(d.clone(), fg(t.text.muted))], inner.saturating_sub(3).max(1), Wrap::Word) {
                lines.push(SLine::new(l).prefixed(&[seg("   ", Default::default())]).on_click(click.clone()));
            }
        }
        entries.push((lines, None));
    }
    let n = rows.len();
    let number = format!("{}.", n + 1);
    let room = inner.saturating_sub(sw(&number) + 1).max(1);
    let custom_click = Some(Click::Custom(format!("opt:{n}")));
    if c.editing {
        let (segs, col) = if c.text.is_empty() {
            let ph = c.field.get("placeholder").and_then(|p| p.as_str()).unwrap_or("Type your own answer");
            (vec![seg(locale::take_width(ph, room), fg(t.text.muted))], 0)
        } else {
            let start = sw(&c.text[..c.cursor]).saturating_sub(room.saturating_sub(1));
            let shown = locale::take_width(&c.text.chars().skip(start).collect::<String>(), room);
            (vec![seg(shown, fg(t.text.formfield.focused))], sw(&c.text[..c.cursor]).min(room.saturating_sub(1)))
        };
        let mut line = vec![seg(number.clone(), fg(color(n))), seg(" ", Default::default())];
        line.extend(segs);
        entries.push((vec![SLine::new(line).on_click(custom_click)], Some(sw(&number) + 1 + col)));
    } else {
        let text = if c.text.is_empty() { "Type your own answer".to_string() } else { c.text.clone() };
        let line = vec![seg(number.clone(), fg(color(n))), seg(" ", Default::default()), seg(locale::take_width(&text, room), fg(color(n)))];
        entries.push((vec![SLine::new(line).on_click(custom_click)], None));
    }
    if !c.required() {
        entries.push((vec![SLine::new(vec![seg(format!("{}. Skip", n + 2), fg(color(n + 1)))]).on_click(Some(Click::Custom(format!("opt:{}", n + 1))))], None));
    }
    // The scrollbox: `min(count + rows with a description, max(3, floor(height / 2) - 6))` rows tall.
    let described = rows.iter().filter(|(_, _, d)| d.is_some()).count();
    let count = c.count();
    let height = (count + described).min(((term_h / 2) as isize - 6).max(3) as usize);
    let mut content: Vec<(SLine, Option<usize>)> = Vec::new();
    let mut tops = Vec::new();
    // The scrollbox's `gap` stays on its own box: its content rows sit back to back.
    for (lines, cur) in entries {
        tops.push((content.len(), lines.len()));
        for (j, l) in lines.into_iter().enumerate() {
            content.push((l, if j == 0 { cur } else { None }));
        }
    }
    // `scrollChildIntoView` of the selected entry.
    if let Some((top, len)) = tops.get(c.selected).copied() {
        if top < c.scroll {
            c.scroll = top;
        } else if top + len > c.scroll + height {
            c.scroll = (top + len).saturating_sub(height);
        }
    }
    c.scroll = c.scroll.min(content.len().saturating_sub(height));
    let mut cursor = None;
    for (l, cur) in content.into_iter().skip(c.scroll).take(height) {
        if let Some(col) = cur {
            cursor = Some(((DIALOG_INSET + col) as u16, out.len() as u16));
        }
        out.push(ins(l));
    }
    if let Some(e) = &c.error {
        out.push(SLine::blank());
        for l in wrap(&[seg(e.clone(), fg(t.text.feedback.error.base))], inner, Wrap::Word) {
            out.push(ins(SLine::new(l)));
        }
    }
    out.push(SLine::blank());
    let footer = if c.editing {
        Footer { secondary: vec![FooterItem::new("submit", "enter"), FooterItem::new("back", "esc")], ..Default::default() }
    } else {
        Footer { secondary: vec![FooterItem::new("select", "↑/↓"), FooterItem::new("confirm", "enter"), FooterItem::new("cancel", "esc")], ..Default::default() }
    };
    out.extend(footer_lines(&footer, inner, narrow, t).into_iter().map(ins));
    out.push(SLine::blank());
    (out, cursor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn location() -> Value {
        json!({"custom": true, "default": "global", "description": "global serves the most models.", "key": "location", "minLength": 1,
               "options": [{"label": "global", "value": "global"}, {"label": "us", "value": "us"}, {"label": "eu", "value": "eu"}],
               "pattern": "\\S", "placeholder": "Location", "required": true, "title": "Location", "type": "string"})
    }

    #[test]
    fn starts_on_the_default_and_answers_with_a_row() {
        let kb = Keybinds::new(None, 2000, true);
        let mut c = ChoiceField::new(location());
        assert_eq!(c.selected, 0);
        assert_eq!(c.handle_key(&Stroke::key("down"), &kb), ChoiceKey::None);
        assert_eq!(c.handle_key(&Stroke::key("return"), &kb), ChoiceKey::Submit(Some(json!("us"))));
    }

    #[test]
    fn a_custom_default_selects_the_typed_row_and_validates() {
        let kb = Keybinds::new(None, 2000, true);
        let mut field = location();
        field["default"] = json!("asia-1");
        let mut c = ChoiceField::new(field);
        assert_eq!(c.selected, 3);
        assert_eq!(c.text, "asia-1");
        c.handle_key(&Stroke::key("return"), &kb);
        assert!(c.editing);
        for _ in 0..6 {
            c.handle_key(&Stroke::key("backspace"), &kb);
        }
        assert_eq!(c.handle_key(&Stroke::key("return"), &kb), ChoiceKey::None);
        assert_eq!(c.error.as_deref(), Some("Answer required"));
        c.handle_key(&Stroke::key("x"), &kb);
        assert_eq!(c.error, None);
        assert_eq!(c.handle_key(&Stroke::key("return"), &kb), ChoiceKey::Submit(Some(json!("x"))));
        c.handle_key(&Stroke::key("escape"), &kb);
        assert!(!c.editing);
        assert_eq!(c.handle_key(&Stroke::key("escape"), &kb), ChoiceKey::Close);
    }

    #[test]
    fn rows_are_numbered_and_the_list_scrolls() {
        let t = Theme::default();
        let mut c = ChoiceField::new(location());
        let (out, _) = lines(&mut c, 60, 30, false, &t);
        let text: Vec<String> = out.iter().map(|l| l.text().trim_end().to_string()).collect();
        assert_eq!(&text[4..9], &["   1. global", "   2. us", "   3. eu", "   4. Type your own answer", ""], "{text:#?}");
        assert!(text.iter().any(|l| l == "   select ↑/↓  confirm enter  cancel esc"), "{text:#?}");
        // Five rows with Skip, on a short terminal the list keeps 3 and follows the selection.
        let mut field = location();
        field["required"] = json!(false);
        let mut c = ChoiceField::new(field);
        c.selected = 4;
        let (out, _) = lines(&mut c, 60, 12, false, &t);
        let text: Vec<String> = out.iter().map(|l| l.text().trim_end().to_string()).collect();
        assert_eq!(&text[4..8], &["   3. eu", "   4. Type your own answer", "   5. Skip", ""], "{text:#?}");
    }
}
