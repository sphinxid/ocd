//! Dialog host and dialogs (`src/ui/dialog*.tsx`): generic fuzzy select list, alert, confirm,
//! prompt and text dialogs, with the exact geometry from spec §2.

use super::text::*;
use crate::keymap::{Keybinds, Match, Stroke};
use crate::theme::Theme;
use crate::util::locale::{self, width as sw};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    Medium,
    Large,
    XLarge,
}

impl Size {
    pub fn width(&self) -> u16 {
        match self {
            Size::Medium => 60,
            Size::Large => 88,
            Size::XLarge => 116,
        }
    }
}

/// `fuzzysort`-like score in 0..=1 (higher is better); `None` if not a subsequence.
pub fn fuzzy(needle: &str, hay: &str) -> Option<f64> {
    if needle.is_empty() {
        return Some(1.0);
    }
    let n = needle.to_lowercase();
    let h = hay.to_lowercase();
    if h.is_empty() {
        return None;
    }
    if h.starts_with(&n) {
        return Some(1.0 - 0.001 * (h.len() - n.len()) as f64 / h.len() as f64);
    }
    if let Some(i) = h.find(&n) {
        let word_start = i == 0 || !h[..i].chars().last().unwrap().is_alphanumeric();
        return Some(if word_start { 0.9 } else { 0.8 } - 0.0001 * i as f64);
    }
    // Subsequence with gap penalty.
    let hc: Vec<char> = h.chars().collect();
    let mut pos = 0;
    let mut gaps = 0usize;
    let mut first = None;
    for c in n.chars() {
        let mut found = false;
        while pos < hc.len() {
            if hc[pos] == c {
                first.get_or_insert(pos);
                found = true;
                pos += 1;
                break;
            }
            if first.is_some() {
                gaps += 1;
            }
            pos += 1;
        }
        if !found {
            return None;
        }
    }
    Some((0.6 - 0.02 * gaps as f64).max(0.05))
}

#[derive(Clone, Debug, Default)]
pub struct Opt {
    pub title: String,
    pub value: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub footer: Option<String>,
    pub footer_color: Option<crate::theme::Rgba>,
    pub search_text: Option<String>,
    pub search_footer: Option<String>,
    pub gutter: Option<Seg>,
    pub disabled: bool,
    /// Override highlight colors (delete-confirm rows).
    pub bg: Option<crate::theme::Rgba>,
    pub fg: Option<crate::theme::Rgba>,
    pub title_width: Option<usize>,
    pub truncate_left: bool,
    pub details: Vec<String>,
}

impl Opt {
    pub fn new(title: impl Into<String>, value: impl Into<String>) -> Opt {
        Opt { title: title.into(), value: value.into(), ..Default::default() }
    }
    pub fn desc(mut self, d: impl Into<String>) -> Opt {
        self.description = Some(d.into());
        self
    }
    pub fn cat(mut self, c: impl Into<String>) -> Opt {
        self.category = Some(c.into());
        self
    }
    pub fn foot(mut self, f: impl Into<String>) -> Opt {
        self.footer = Some(f.into());
        self
    }
}

#[derive(Clone, Debug)]
pub struct Action {
    /// Keybind id (e.g. `session.delete`) or an inline key string prefixed with `key:`.
    pub id: String,
    pub title: String,
    pub right: bool,
    pub disabled: bool,
    pub hidden: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Select {
    pub title: String,
    pub title_suffix: Option<String>,
    pub placeholder: String,
    pub options: Vec<Opt>,
    pub filter: String,
    pub selected: usize,
    pub scroll: usize,
    pub current: Option<String>,
    pub flat: bool,
    pub threshold: f64,
    pub skip_filter: bool,
    pub render_filter: bool,
    pub actions: Vec<Action>,
    pub hints: Vec<(String, String)>,
    pub footer_text: Option<(String, crate::theme::Rgba)>,
    pub action_focus: Option<usize>,
    pub empty: Option<String>,
    pub no_match: Option<String>,
    pub can_back: bool,
    pub focus_current: bool,
}

/// Display row of a select list.
#[derive(Clone, Debug)]
pub enum Item {
    Header(String, bool),
    Opt(usize),
}

impl Select {
    pub fn new(title: impl Into<String>, options: Vec<Opt>) -> Select {
        Select { title: title.into(), placeholder: "Search".into(), options, render_filter: true, focus_current: true, ..Default::default() }
    }

    /// Filtered option indices.
    pub fn filtered(&self) -> Vec<usize> {
        let enabled: Vec<usize> = (0..self.options.len()).filter(|i| !self.options[*i].disabled).collect();
        if self.skip_filter || !self.render_filter || self.filter.is_empty() {
            return enabled;
        }
        let needle = self.filter.to_lowercase();
        let mut scored: Vec<(usize, f64)> = enabled
            .into_iter()
            .filter_map(|i| {
                let o = &self.options[i];
                let t = fuzzy(&needle, &o.title);
                let c = o.category.as_deref().and_then(|c| fuzzy(&needle, c));
                let s = o.search_text.as_deref().and_then(|s| fuzzy(&needle, s));
                if t.is_none() && c.is_none() && s.is_none() {
                    return None;
                }
                let score = t.unwrap_or(0.0) * 2.0 + c.unwrap_or(0.0) + s.unwrap_or(0.0);
                if score < self.threshold {
                    return None;
                }
                Some((i, score))
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().map(|(i, _)| i).collect()
    }

    fn flattened(&self) -> bool {
        self.flat && !self.filter.is_empty()
    }

    pub fn items(&self) -> Vec<Item> {
        let idx = self.filtered();
        if self.flattened() {
            return idx.into_iter().map(Item::Opt).collect();
        }
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        for i in idx {
            let c = self.options[i].category.clone().unwrap_or_default();
            match groups.iter_mut().find(|(k, _)| *k == c) {
                Some((_, v)) => v.push(i),
                None => groups.push((c, vec![i])),
            }
        }
        let mut out = Vec::new();
        for (gi, (c, v)) in groups.into_iter().enumerate() {
            if !c.is_empty() {
                out.push(Item::Header(c, gi == 0));
            }
            out.extend(v.into_iter().map(Item::Opt));
        }
        out
    }

    pub fn visible(&self) -> Vec<usize> {
        self.items().into_iter().filter_map(|i| if let Item::Opt(o) = i { Some(o) } else { None }).collect()
    }

    pub fn selected_opt(&self) -> Option<&Opt> {
        let v = self.visible();
        v.get(self.selected).map(|i| &self.options[*i])
    }

    pub fn selected_value(&self) -> Option<String> {
        self.selected_opt().map(|o| o.value.clone())
    }

    pub fn move_by(&mut self, d: isize) {
        let n = self.visible().len() as isize;
        if n == 0 {
            return;
        }
        self.selected = (((self.selected as isize + d) % n + n) % n) as usize;
        self.action_focus = None;
    }

    pub fn move_to(&mut self, i: usize) {
        let n = self.visible().len();
        if n > 0 {
            self.selected = i.min(n - 1);
        }
    }

    pub fn set_filter(&mut self, f: String) {
        self.filter = f;
        self.selected = 0;
        self.scroll = 0;
        self.action_focus = None;
    }

    /// Jump to the current option.
    pub fn focus_current_option(&mut self) {
        if !self.focus_current {
            return;
        }
        if let Some(cur) = &self.current
            && let Some(p) = self.visible().iter().position(|i| &self.options[*i].value == cur)
        {
            self.selected = p;
        }
    }

    fn visible_actions(&self, kb: &Keybinds) -> Vec<usize> {
        (0..self.actions.len()).filter(|i| !self.actions[*i].hidden && !action_label(&self.actions[*i], kb).is_empty()).collect()
    }
}

pub fn action_label(a: &Action, kb: &Keybinds) -> String {
    match a.id.strip_prefix("key:") {
        Some(k) => k.to_string(),
        None => kb.shortcut(&a.id),
    }
}

/// Single-line text input dialog (`DialogPrompt`).
#[derive(Clone, Debug, Default)]
pub struct PromptDialog {
    pub title: String,
    pub placeholder: String,
    pub value: String,
    pub cursor: usize,
    pub description: Vec<(String, crate::theme::Rgba)>,
    pub busy: Option<String>,
}

impl PromptDialog {
    pub fn new(title: &str, placeholder: &str, value: &str) -> PromptDialog {
        PromptDialog {
            title: title.into(),
            placeholder: if placeholder.is_empty() { "Enter text".into() } else { placeholder.into() },
            value: value.into(),
            cursor: value.len(),
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExportOptions {
    pub json: bool,
    pub thinking: bool,
    pub tools: bool,
    pub sanitize: bool,
    pub focus: usize,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions { json: false, thinking: true, tools: true, sanitize: false, focus: 0 }
    }
}

impl ExportOptions {
    /// Tab order of focusable controls.
    pub fn order(&self) -> Vec<&'static str> {
        if self.json { vec!["markdown", "json", "sanitize", "copy", "export"] } else { vec!["markdown", "json", "thinking", "tools", "copy", "export"] }
    }
    pub fn focused(&self) -> &'static str {
        let o = self.order();
        o[self.focus.min(o.len() - 1)]
    }
}

#[derive(Clone, Debug)]
pub enum Body {
    Select(Box<Select>),
    Alert { title: String, message: String },
    Confirm { title: String, message: String, confirm: bool },
    Prompt(PromptDialog),
    Help,
    /// `DialogExportOptions`.
    Export(ExportOptions),
    /// Free-form lines with a title and optional hint footer.
    Text { title: String, lines: Vec<SLine>, hints: Vec<(String, String)>, scroll: usize },
}

/// One dialog on the host stack.
#[derive(Clone, Debug)]
pub struct Dialog {
    pub body: Body,
    pub size: Size,
    pub centered: bool,
    /// Identifies what the app should do with outcomes.
    pub purpose: String,
    /// Arbitrary purpose payload.
    pub data: serde_json::Value,
}

impl Dialog {
    pub fn select(purpose: &str, s: Select, size: Size) -> Dialog {
        let mut s = s;
        s.focus_current_option();
        Dialog { body: Body::Select(Box::new(s)), size, centered: false, purpose: purpose.into(), data: serde_json::Value::Null }
    }
    pub fn new(purpose: &str, body: Body) -> Dialog {
        Dialog { body, size: Size::Medium, centered: false, purpose: purpose.into(), data: serde_json::Value::Null }
    }
    pub fn sel(&mut self) -> Option<&mut Select> {
        match &mut self.body {
            Body::Select(s) => Some(s),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    None,
    Handled,
    Close,
    Back,
    Select(String),
    Moved(String),
    Filter(String),
    Action(String, Option<String>),
    Submit(String),
    Confirm(bool),
}

fn char_of(s: &Stroke) -> Option<char> {
    s.is_char()
}

/// Handle a key for the top dialog. Unhandled keys return `Outcome::None`.
pub fn handle_key(d: &mut Dialog, s: &Stroke, kb: &Keybinds) -> Outcome {
    let is = |id: &str| kb.match_command(id, std::slice::from_ref(s)) == Match::Full;
    let name = s.name.as_str();
    match &mut d.body {
        Body::Select(sel) => {
            if name == "escape" && !s.ctrl {
                return if sel.can_back { Outcome::Back } else { Outcome::Close };
            }
            if name == "c" && s.ctrl {
                if !sel.filter.is_empty() {
                    sel.set_filter(String::new());
                    return Outcome::Filter(String::new());
                }
                return Outcome::Close;
            }
            // Footer action keys.
            for a in sel.actions.clone() {
                if a.disabled || (a.hidden && !a.id.starts_with("key:")) {
                    continue;
                }
                let hit = match a.id.strip_prefix("key:") {
                    Some(k) => crate::keymap::parse_sequence(k).ok().is_some_and(|seq| seq.len() == 1 && seq[0].matches(s)),
                    None => is(&a.id),
                };
                if hit {
                    return Outcome::Action(a.id.clone(), sel.selected_value());
                }
            }
            if !sel.actions.is_empty() && name == "tab" {
                let vis: Vec<usize> = sel.visible_actions(kb).into_iter().filter(|i| !sel.actions[*i].disabled).collect();
                if !vis.is_empty() {
                    let cur = sel.action_focus.and_then(|f| vis.iter().position(|x| *x == f));
                    sel.action_focus = if s.shift {
                        match cur {
                            None => vis.last().copied(),
                            Some(0) => None,
                            Some(p) => Some(vis[p - 1]),
                        }
                    } else {
                        match cur {
                            None => vis.first().copied(),
                            Some(p) if p + 1 >= vis.len() => None,
                            Some(p) => Some(vis[p + 1]),
                        }
                    };
                    return Outcome::Handled;
                }
            }
            let moved = |sel: &mut Select| sel.selected_value().map(Outcome::Moved).unwrap_or(Outcome::Handled);
            if is("dialog.select.prev") {
                sel.move_by(-1);
                return moved(sel);
            }
            if is("dialog.select.next") {
                sel.move_by(1);
                return moved(sel);
            }
            if is("dialog.select.page_up") {
                sel.move_by(-10);
                return moved(sel);
            }
            if is("dialog.select.page_down") {
                sel.move_by(10);
                return moved(sel);
            }
            if is("dialog.select.home") {
                sel.move_to(0);
                return moved(sel);
            }
            if is("dialog.select.end") {
                sel.move_to(usize::MAX);
                return moved(sel);
            }
            if is("dialog.select.submit") {
                if let Some(f) = sel.action_focus {
                    let a = &sel.actions[f];
                    return Outcome::Action(a.id.clone(), sel.selected_value());
                }
                return match sel.selected_value() {
                    Some(v) => Outcome::Select(v),
                    None => Outcome::Handled,
                };
            }
            if sel.render_filter && !sel.skip_filter || sel.render_filter {
                if name == "backspace" && !s.ctrl {
                    let mut f = sel.filter.clone();
                    f.pop();
                    sel.set_filter(f.clone());
                    return Outcome::Filter(f);
                }
                if let Some(c) = char_of(s) {
                    let f = format!("{}{c}", sel.filter);
                    sel.set_filter(f.clone());
                    return Outcome::Filter(f);
                }
            }
            Outcome::None
        }
        Body::Alert { .. } | Body::Help => {
            if name == "return" || name == "escape" {
                return Outcome::Close;
            }
            if name == "c" && s.ctrl {
                return Outcome::Close;
            }
            Outcome::Handled
        }
        Body::Confirm { confirm, .. } => match name {
            "left" | "right" => {
                *confirm = !*confirm;
                Outcome::Handled
            }
            "return" => Outcome::Confirm(*confirm),
            "escape" => Outcome::Close,
            "c" if s.ctrl => Outcome::Close,
            _ => Outcome::Handled,
        },
        Body::Prompt(p) => {
            if p.busy.is_some() {
                return Outcome::Handled;
            }
            if is("dialog.prompt.submit") {
                return Outcome::Submit(p.value.clone());
            }
            match name {
                "escape" => Outcome::Back,
                "c" if s.ctrl => {
                    if p.value.is_empty() {
                        Outcome::Close
                    } else {
                        p.value.clear();
                        p.cursor = 0;
                        Outcome::Handled
                    }
                }
                "backspace" => {
                    if p.cursor > 0 {
                        let prev = p.value[..p.cursor].char_indices().last().map(|(i, _)| i).unwrap_or(0);
                        p.value.replace_range(prev..p.cursor, "");
                        p.cursor = prev;
                    }
                    Outcome::Handled
                }
                "delete" => {
                    if p.cursor < p.value.len() {
                        let next = p.value[p.cursor..].char_indices().nth(1).map(|(i, _)| p.cursor + i).unwrap_or(p.value.len());
                        p.value.replace_range(p.cursor..next, "");
                    }
                    Outcome::Handled
                }
                "left" => {
                    p.cursor = p.value[..p.cursor].char_indices().last().map(|(i, _)| i).unwrap_or(0);
                    Outcome::Handled
                }
                "right" => {
                    p.cursor = p.value[p.cursor..].char_indices().nth(1).map(|(i, _)| p.cursor + i).unwrap_or(p.value.len());
                    Outcome::Handled
                }
                "home" => {
                    p.cursor = 0;
                    Outcome::Handled
                }
                "end" => {
                    p.cursor = p.value.len();
                    Outcome::Handled
                }
                "a" if s.ctrl => {
                    p.cursor = 0;
                    Outcome::Handled
                }
                "e" if s.ctrl => {
                    p.cursor = p.value.len();
                    Outcome::Handled
                }
                "u" if s.ctrl => {
                    p.value.replace_range(..p.cursor, "");
                    p.cursor = 0;
                    Outcome::Handled
                }
                _ => {
                    if let Some(c) = char_of(s) {
                        p.value.insert(p.cursor, c);
                        p.cursor += c.len_utf8();
                        return Outcome::Handled;
                    }
                    Outcome::None
                }
            }
        }
        Body::Export(e) => match name {
            "tab" => {
                let n = e.order().len();
                e.focus = if s.shift { (e.focus + n - 1) % n } else { (e.focus + 1) % n };
                Outcome::Handled
            }
            "return" => {
                match e.focused() {
                    "markdown" => e.json = false,
                    "json" => {
                        e.json = true;
                        e.focus = 1;
                    }
                    "thinking" => e.thinking = !e.thinking,
                    "tools" => e.tools = !e.tools,
                    "sanitize" => e.sanitize = !e.sanitize,
                    other => return Outcome::Action(other.to_string(), None),
                }
                Outcome::Handled
            }
            "escape" => Outcome::Close,
            "c" if s.ctrl => Outcome::Close,
            _ => Outcome::Handled,
        },
        Body::Text { scroll, .. } => match name {
            "escape" | "return" => Outcome::Close,
            "c" if s.ctrl => Outcome::Close,
            "up" | "k" => {
                *scroll = scroll.saturating_sub(1);
                Outcome::Handled
            }
            "down" | "j" => {
                *scroll += 1;
                Outcome::Handled
            }
            "pageup" => {
                *scroll = scroll.saturating_sub(10);
                Outcome::Handled
            }
            "pagedown" => {
                *scroll += 10;
                Outcome::Handled
            }
            _ => Outcome::None,
        },
    }
}

/// Insert pasted text into a dialog's input.
pub fn paste(d: &mut Dialog, text: &str) {
    let text = text.replace(['\n', '\r'], " ");
    match &mut d.body {
        Body::Select(s) => {
            let f = format!("{}{text}", s.filter);
            s.set_filter(f);
        }
        Body::Prompt(p) => {
            p.value.insert_str(p.cursor, &text);
            p.cursor += text.len();
        }
        _ => {}
    }
}

/// Hit target of a rendered dialog.
#[derive(Clone, Debug, PartialEq)]
pub enum DialogHit {
    Option(usize),
    Action(String),
    Close,
    Ok,
    Confirm(bool),
}

/// Render the dialog host (backdrop + panel). Returns panel rect, hits and cursor position.
pub fn render(buf: &mut Buffer, area: Rect, d: &mut Dialog, base: &Theme, kb: &Keybinds) -> (Rect, Vec<(Rect, DialogHit)>, Option<(u16, u16)>) {
    let t = base.dialog();
    dim(buf, area, 150, base.background.base);
    let w = d.size.width().min(area.width.saturating_sub(2)).max(10);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let mut hits = Vec::new();
    let mut cursor = None;
    let lines: Vec<SLine> = match &mut d.body {
        Body::Select(sel) => select_lines(sel, w as usize, area.height as usize, area.width as usize, t, base, kb, &mut cursor),
        Body::Alert { title, message } => {
            let mut v = header(title, t, 2, w as usize);
            v.push(SLine::blank());
            for l in wrap(&[seg(message.clone(), fg(t.text.muted))], (w as usize).saturating_sub(4), Wrap::Word) {
                v.push(SLine::new(l).prefixed(&[pad(2)]));
            }
            v.push(SLine::blank());
            let ok = Seg::new("   ok   ", fg_bg(t.text.action.primary.focused, t.background.action.primary.focused));
            v.push(SLine::new(vec![pad((w as usize).saturating_sub(10)), ok]).on_click(Some(Click::Custom("ok".into()))));
            v.push(SLine::blank());
            v
        }
        Body::Help => {
            let mut v = header("Help", t, 2, w as usize);
            v[0] = header_with("Help", "esc/enter", t, 2, w as usize);
            v.push(SLine::blank());
            let text = format!("Press {} to see all available actions and commands in any context.", kb.shortcut("command.palette.show"));
            for l in wrap(&[seg(text, fg(t.text.muted))], (w as usize).saturating_sub(4), Wrap::Word) {
                v.push(SLine::new(l).prefixed(&[pad(2)]));
            }
            v.push(SLine::blank());
            let ok = Seg::new("   ok   ", fg_bg(t.text.action.primary.focused, t.background.action.primary.focused));
            v.push(SLine::new(vec![pad((w as usize).saturating_sub(10)), ok]).on_click(Some(Click::Custom("ok".into()))));
            v.push(SLine::blank());
            v
        }
        Body::Confirm { title, message, confirm } => {
            let mut v = header(title, t, 2, w as usize);
            v.push(SLine::blank());
            for l in wrap(&[seg(message.clone(), fg(t.text.muted))], (w as usize).saturating_sub(4), Wrap::Word) {
                v.push(SLine::new(l).prefixed(&[pad(2)]));
            }
            v.push(SLine::blank());
            let btn = |label: &str, active: bool| {
                if active {
                    Seg::new(format!(" {label} "), fg_bg(t.text.action.primary.focused, t.background.action.primary.focused))
                } else {
                    Seg::new(format!(" {label} "), fg(t.text.muted))
                }
            };
            let cancel = btn("Cancel", !*confirm);
            let ok = btn("Confirm", *confirm);
            let used = cancel.width() + ok.width();
            v.push(SLine::new(vec![pad((w as usize).saturating_sub(used + 2)), cancel, ok]));
            v.push(SLine::blank());
            v
        }
        Body::Prompt(p) => {
            let mut v = header(&p.title, t, 2, w as usize);
            v.push(SLine::blank());
            for (text, c) in &p.description {
                for l in wrap(&[seg(text.clone(), fg(*c))], (w as usize).saturating_sub(4), Wrap::Word) {
                    v.push(SLine::new(l).prefixed(&[pad(2)]));
                }
            }
            if !p.description.is_empty() {
                v.push(SLine::blank());
            }
            let iw = (w as usize).saturating_sub(4);
            let line = if p.value.is_empty() {
                vec![pad(2), seg(locale::take_width(&p.placeholder, iw), fg(t.text.muted))]
            } else {
                let start = sw(&p.value[..p.cursor]).saturating_sub(iw.saturating_sub(1));
                let shown: String = locale::take_width(&p.value.chars().skip(start).collect::<String>(), iw);
                vec![pad(2), seg(shown, fg(if p.busy.is_some() { t.text.formfield.disabled } else { t.text.formfield.base }))]
            };
            let row = v.len();
            v.push(SLine::new(line));
            cursor = Some((2 + sw(&p.value[..p.cursor]).min(iw.saturating_sub(1)) as u16, row as u16));
            v.push(SLine::blank());
            match &p.busy {
                Some(b) => v.push(SLine::new(vec![pad(2), seg(format!("{SPINNER_GLYPH} {b}"), fg(t.text.muted))])),
                None => v.push(SLine::new(vec![pad(2), seg(kb.shortcut("dialog.prompt.submit"), fg(t.text.base)), seg(" submit", fg(t.text.muted))])),
            }
            v.push(SLine::blank());
            v
        }
        Body::Export(e) => {
            let mut v = header("Export session", t, 2, w as usize);
            v.push(SLine::blank());
            let ff = &t.text.formfield;
            let bf = &t.background.formfield;
            let ctl = |id: &str, label: String, checked: bool| -> Seg {
                let (f, b) = if e.focused() == id { (ff.focused, bf.focused) } else if checked { (ff.selected, bf.selected) } else { (ff.base, bf.base) };
                Seg::new(format!(" {label} "), fg_bg(f, b))
            };
            v.push(SLine::new(vec![
                pad(2),
                seg("Export as: ", fg(t.text.base)),
                ctl("markdown", format!("{} Markdown", if e.json { "○" } else { "◉" }), !e.json),
                pad(1),
                ctl("json", format!("{} JSON", if e.json { "◉" } else { "○" }), e.json),
            ]));
            v.push(SLine::blank());
            let check = |b: bool| if b { "[x]" } else { "[ ]" };
            if e.json {
                v.push(SLine::new(vec![pad(2), ctl("sanitize", format!("{} Sanitize sensitive data", check(e.sanitize)), e.sanitize)]));
            } else {
                v.push(SLine::new(vec![pad(2), ctl("thinking", format!("{} Include thinking", check(e.thinking)), e.thinking)]));
                v.push(SLine::new(vec![pad(2), ctl("tools", format!("{} Include tools", check(e.tools)), e.tools)]));
            }
            v.push(SLine::blank());
            let copy_focus = e.focused() == "copy";
            let export_focus = e.focused() == "export";
            let copy = Seg::new("    Copy    ", if copy_focus { fg_bg(t.text.action.primary.focused, t.background.action.primary.focused) } else { fg_bg(base.text.base, base.background.raised.high) });
            let export = Seg::new("   Export   ", fg_bg(if export_focus { t.text.action.primary.focused } else { t.text.action.primary.base }, if export_focus { t.background.action.primary.focused } else { t.background.action.primary.base }));
            v.push(SLine::new(vec![pad((w as usize).saturating_sub(12 + 1 + 12 + 2)), copy, pad(1), export]));
            v.push(SLine::blank());
            v
        }
        Body::Text { title, lines, hints, scroll } => {
            let mut v = header(title, t, 2, w as usize);
            v.push(SLine::blank());
            let max = (area.height as usize).saturating_sub(10).max(3);
            *scroll = (*scroll).min(lines.len().saturating_sub(max));
            for l in lines.iter().skip(*scroll).take(max) {
                v.push(l.clone().prefixed(&[pad(2)]));
            }
            if !hints.is_empty() {
                v.push(SLine::blank());
                let mut segs = vec![pad(2)];
                for (i, (a, b)) in hints.iter().enumerate() {
                    if i > 0 {
                        segs.push(pad(2));
                    }
                    segs.push(seg(a.clone(), fg(t.text.base)));
                    segs.push(seg(format!(" {b}"), fg(t.text.muted)));
                }
                v.push(SLine::new(segs));
            }
            v.push(SLine::blank());
            v
        }
    };
    let h = (lines.len() as u16 + 1).min(area.height);
    let y = if d.centered { area.y + area.height.saturating_sub(h) / 2 } else { area.y + ((area.height as f64) / 4.0).round() as u16 };
    let y = y.min(area.bottom().saturating_sub(h));
    let panel = Rect::new(x, y, w, h);
    fill(buf, panel, t.background.base.tui());
    let inner = Rect::new(x, y + 1, w, h.saturating_sub(1));
    for (rect, click) in draw_lines(buf, inner, &lines, 0, SPINNER_FRAMES[0]) {
        let hit = match click {
            Click::Custom(c) if c == "ok" => DialogHit::Ok,
            Click::Custom(c) if c == "esc" => DialogHit::Close,
            Click::Custom(c) if c.starts_with("opt:") => DialogHit::Option(c[4..].parse().unwrap_or(0)),
            Click::Custom(c) if c.starts_with("act:") => DialogHit::Action(c[4..].to_string()),
            _ => continue,
        };
        hits.push((rect, hit));
    }
    let cursor = cursor.map(|(cx, cy)| (x + cx, inner.y + cy));
    (panel, hits, cursor)
}

fn pad(n: usize) -> Seg {
    Seg::new(" ".repeat(n), Style::default())
}

fn header(title: &str, t: &Theme, padding: usize, w: usize) -> Vec<SLine> {
    vec![header_with(title, "esc", t, padding, w)]
}

fn header_with(title: &str, right: &str, t: &Theme, padding: usize, w: usize) -> SLine {
    let avail = w.saturating_sub(padding * 2 + sw(right) + 1);
    let title = locale::truncate_width(title, avail);
    let gap = w.saturating_sub(padding * 2 + sw(&title) + sw(right));
    SLine::new(vec![pad(padding), Seg::new(title, bold(fg(t.text.base))), pad(gap), seg(right, fg(t.text.muted))]).on_click(Some(Click::Custom("esc".into())))
}

#[allow(clippy::too_many_arguments)]
fn select_lines(sel: &mut Select, w: usize, term_h: usize, term_w: usize, t: &Theme, base: &Theme, kb: &Keybinds, cursor: &mut Option<(u16, u16)>) -> Vec<SLine> {
    let mut out = Vec::new();
    // Title row (padding 4).
    let mut title_segs = vec![pad(4), Seg::new(sel.title.clone(), bold(fg(t.text.base)))];
    if let Some(sfx) = &sel.title_suffix {
        title_segs.push(seg(sfx.clone(), fg(t.text.muted)));
    }
    let used: usize = title_segs.iter().map(|s| s.width()).sum();
    title_segs.push(pad(w.saturating_sub(used + 3 + 4)));
    title_segs.push(seg("esc", fg(t.text.muted)));
    out.push(SLine::new(title_segs).on_click(Some(Click::Custom("esc".into()))));
    if sel.render_filter {
        out.push(SLine::blank());
        let iw = w.saturating_sub(8);
        let line = if sel.filter.is_empty() {
            SLine::new(vec![pad(4), seg(locale::take_width(&sel.placeholder, iw), fg(t.text.muted))])
        } else {
            SLine::new(vec![pad(4), seg(locale::take_width(&sel.filter, iw), fg(t.text.formfield.focused))])
        };
        *cursor = Some(((4 + sw(&sel.filter).min(iw)) as u16, out.len() as u16));
        out.push(line);
    }
    out.push(SLine::blank());
    let items = sel.items();
    if items.is_empty() {
        let text = if !sel.filter.is_empty() { sel.no_match.clone().unwrap_or_else(|| "No results found".into()) } else { sel.empty.clone().unwrap_or_else(|| "No items available".into()) };
        out.push(SLine::new(vec![pad(4), seg(text, fg(t.text.muted))]));
    } else {
        // Build all rows, then window to listHeight.
        let mut rows: Vec<(SLine, Option<usize>)> = Vec::new();
        let vis = sel.visible();
        let selected_opt = vis.get(sel.selected).copied();
        let muted_active = sel.action_focus.is_some();
        let flat = sel.flattened();
        for item in &items {
            match item {
                Item::Header(c, first) => {
                    if !first {
                        rows.push((SLine::blank(), None));
                    }
                    rows.push((SLine::new(vec![pad(4), Seg::new(c.clone(), bold(fg(t.accent(200))))]), None));
                }
                Item::Opt(i) => {
                    let o = &sel.options[*i];
                    let active = selected_opt == Some(*i);
                    let is_current = sel.current.as_deref() == Some(o.value.as_str());
                    let (tfg, row_bg, b) = if active && !muted_active {
                        (o.fg.unwrap_or(t.text.action.primary.focused), Some(o.bg.unwrap_or(t.background.action.primary.focused)), true)
                    } else if active {
                        (t.text.muted, Some(base.background.raised.high), false)
                    } else if is_current {
                        (if muted_active { t.text.muted } else { t.text.formfield.selected }, None, false)
                    } else {
                        (t.text.base, None, false)
                    };
                    let mut st = fg(tfg);
                    if b {
                        st = st.add_modifier(Modifier::BOLD);
                    }
                    let mut segs = vec![pad(1)];
                    if let Some(g) = &o.gutter {
                        segs.push(Seg::new(g.text.clone(), g.style.fg(if active && !muted_active { tfg.tui() } else { g.style.fg.unwrap_or(tfg.tui()) })));
                        segs.push(pad(1));
                    } else if is_current {
                        segs.push(Seg::new("●", st));
                        segs.push(pad(1));
                    } else {
                        segs.push(pad(2));
                    }
                    segs.push(pad(3));
                    let footer = if flat { o.search_footer.clone().or(o.category.clone()).or(o.footer.clone()) } else { o.footer.clone() };
                    let footer_w = footer.as_deref().map(sw).unwrap_or(0);
                    let avail = w.saturating_sub(2 + 6 + 3 + if footer_w > 0 { footer_w + 1 } else { 0 });
                    let tw = o.title_width.unwrap_or(61).min(avail.max(1));
                    let title = if o.truncate_left { locale::truncate_left(&o.title, tw) } else { locale::truncate(&o.title, tw) };
                    let title = locale::truncate_width(&title, avail.max(1));
                    let mut text_segs = vec![Seg::new(title.clone(), st)];
                    if let Some(dsc) = &o.description
                        && Some(dsc) != o.category.as_ref()
                    {
                        let dc = if active && !muted_active { tfg } else { t.text.muted };
                        let room = avail.saturating_sub(sw(&title));
                        if room > 1 {
                            text_segs.push(Seg::new(locale::truncate_width(&format!(" {dsc}"), room), fg(dc)));
                        }
                    }
                    segs.extend(text_segs);
                    let used: usize = segs.iter().map(|s| s.width()).sum();
                    if let Some(f) = footer {
                        let fc = if active && !muted_active { tfg } else if muted_active && (active || is_current) { t.text.muted } else { o.footer_color.unwrap_or(t.text.muted) };
                        segs.push(pad(w.saturating_sub(used + footer_w + 4 + 1)));
                        segs.push(seg(f, fg(fc)));
                    }
                    let mut line = SLine::new(segs).on_click(Some(Click::Custom(format!("opt:{}", vis.iter().position(|x| x == i).unwrap_or(0)))));
                    if let Some(bgc) = row_bg {
                        // Row spans x=1..W-2.
                        let mut ss: Vec<Seg> = line.segs.iter().skip(1).cloned().collect();
                        let used: usize = ss.iter().map(|s| s.width()).sum();
                        ss.push(pad(w.saturating_sub(used + 2)));
                        let ss: Vec<Seg> = ss.into_iter().map(|s| Seg::new(s.text, s.style.bg(bgc.tui()))).collect();
                        line.segs = std::iter::once(pad(1)).chain(ss).collect();
                    }
                    rows.push((line, Some(*i)));
                    for dline in &o.details {
                        let max = 1.max(76.min(term_w.saturating_sub(12)));
                        rows.push((SLine::new(vec![pad(4), seg(locale::truncate_middle(dline, max), fg(t.text.muted))]), None));
                    }
                }
            }
        }
        let list_h = rows.len().min((term_h / 2).saturating_sub(6).max(1));
        // Keep the selected row visible with minimal scrolling.
        let sel_row = rows.iter().position(|(_, i)| *i == selected_opt && i.is_some()).unwrap_or(0);
        if sel.selected == 0 {
            sel.scroll = 0;
        }
        if sel_row < sel.scroll {
            sel.scroll = sel_row;
        }
        if sel_row >= sel.scroll + list_h {
            sel.scroll = sel_row + 1 - list_h;
        }
        sel.scroll = sel.scroll.min(rows.len().saturating_sub(list_h));
        out.extend(rows.into_iter().skip(sel.scroll).take(list_h).map(|(l, _)| l));
    }
    out.push(SLine::blank());
    // Footer row.
    let mut left: Vec<Seg> = Vec::new();
    let mut right: Vec<Seg> = Vec::new();
    if let Some((text, c)) = &sel.footer_text {
        left.push(seg(text.clone(), fg(*c)));
    }
    for (title, label) in &sel.hints {
        if !left.is_empty() {
            left.push(pad(2));
        }
        left.push(Seg::new(title.clone(), bold(fg(t.text.base))));
        left.push(seg(format!(" {label}"), fg(t.text.muted)));
    }
    let visible_actions = sel.visible_actions(kb);
    for i in visible_actions {
        let a = &sel.actions[i];
        let label = action_label(a, kb);
        let focused = sel.action_focus == Some(i);
        let (tc, lc, bgc) = if a.disabled {
            (t.text.action.primary.disabled, t.text.action.primary.disabled, None)
        } else if focused {
            (t.text.action.primary.focused, t.text.action.primary.focused, Some(t.background.action.primary.focused))
        } else {
            (t.text.base, t.text.muted, None)
        };
        let mut ts = fg(tc);
        let mut ls = fg(lc);
        if focused {
            ts = ts.add_modifier(Modifier::BOLD);
        }
        if let Some(b) = bgc {
            ts = ts.bg(b.tui());
            ls = ls.bg(b.tui());
        }
        let target = if a.right { &mut right } else { &mut left };
        if !target.is_empty() {
            target.push(pad(2));
        }
        target.push(Seg::new(a.title.clone(), ts));
        target.push(Seg::new(format!(" {label}"), ls));
    }
    if !left.is_empty() || !right.is_empty() {
        let lw: usize = left.iter().map(|s| s.width()).sum();
        let rw: usize = right.iter().map(|s| s.width()).sum();
        let mut segs = vec![pad(4)];
        segs.extend(left);
        segs.push(pad(w.saturating_sub(4 + 2 + lw + rw)));
        segs.extend(right);
        out.push(SLine::new(segs));
    } else {
        out.push(SLine::blank());
    }
    out.push(SLine::blank());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_scores() {
        assert!(fuzzy("mod", "Switch model").unwrap() > 0.8);
        assert!(fuzzy("swm", "Switch model").is_some());
        assert!(fuzzy("xyz", "Switch model").is_none());
        assert!(fuzzy("sw", "Switch") > fuzzy("sw", "aswb"));
    }

    #[test]
    fn select_filter_and_groups() {
        let mut s = Select::new("Commands", vec![
            Opt::new("Switch model", "model.list").cat("Agent"),
            Opt::new("New session", "session.new").cat("Session"),
            Opt::new("Switch session", "session.list").cat("Session"),
        ]);
        s.flat = true;
        assert_eq!(s.items().len(), 5);
        s.set_filter("session".into());
        let v = s.visible();
        assert_eq!(v.len(), 2);
        assert_eq!(s.items().len(), 2);
        s.move_by(1);
        s.move_by(1);
        assert_eq!(s.selected, 0);
    }
}
