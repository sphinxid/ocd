//! Dialog host and dialogs (`src/ui/dialog*.tsx`): the shared layout (header, body, footer with
//! `DIALOG_INSET` side padding), the generic fuzzy select list, alert, confirm, help, prompt,
//! export and text dialogs, with the geometry of upstream's legacy (pre "new designs") layout.

use super::text::*;
use crate::keymap::{Keybinds, Match, Stroke};
use crate::theme::{Rgba, Theme};
use crate::util::locale::{self, width as sw};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

/// Side padding of every dialog (`DIALOG_INSET`).
pub const DIALOG_INSET: usize = 3;
/// List rows reserve a 1-cell gutter for the `●` marker (`DIALOG_LIST_INSET`).
pub const DIALOG_LIST_INSET: usize = DIALOG_INSET + 1;

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

/// `dialogSelectContentWidth`: scroll padding, row padding, the gutter, title padding, and the gap.
pub fn select_content_width(dialog_width: usize) -> usize {
    dialog_width.saturating_sub(DIALOG_LIST_INSET * 2 + 4)
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

// ---- footer ----------------------------------------------------------------

/// One footer entry (`DialogFooterAction`): the title, then its key in muted text.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FooterItem {
    pub title: String,
    /// Literal key label, e.g. `enter`; `None` shows the title alone.
    pub key: Option<String>,
    /// Overrides the title and key color (e.g. a "✓ copied" confirmation).
    pub color: Option<Rgba>,
    pub disabled: bool,
}

impl FooterItem {
    pub fn new(title: impl Into<String>, key: impl Into<String>) -> FooterItem {
        let key: String = key.into();
        FooterItem { title: title.into(), key: if key.is_empty() { None } else { Some(key) }, ..Default::default() }
    }
    pub fn color(mut self, c: Rgba) -> FooterItem {
        self.color = Some(c);
        self
    }
}

/// `DialogFooter`: dialog-level content and actions on the left, actions on the selection (or
/// the dialog's main action) on the right.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Footer {
    /// Free content placed first in the left group (a muted note, for example).
    pub children: Vec<Seg>,
    pub secondary: Vec<FooterItem>,
    pub primary: Vec<FooterItem>,
}

impl Footer {
    pub fn is_empty(&self) -> bool {
        self.children.is_empty() && self.secondary.is_empty() && self.primary.is_empty()
    }
}

fn item_segs(i: &FooterItem, t: &Theme) -> Vec<Seg> {
    let title_c = if i.disabled { t.text.action.primary.disabled } else { i.color.unwrap_or(t.text.base) };
    let key_c = if i.disabled { t.text.action.primary.disabled } else { i.color.unwrap_or(t.text.muted) };
    let mut v = vec![seg(i.title.clone(), fg(title_c))];
    if let Some(k) = &i.key {
        v.push(seg(format!(" {k}"), fg(key_c)));
    }
    v
}

/// Footer rows for a content width of `w` (the caller adds the side padding). Below 60 terminal
/// columns the left group stacks into a column.
pub fn footer_lines(f: &Footer, w: usize, narrow: bool, t: &Theme) -> Vec<SLine> {
    let mut left: Vec<Vec<Seg>> = Vec::new();
    if !f.children.is_empty() {
        left.push(f.children.clone());
    }
    for i in &f.secondary {
        left.push(item_segs(i, t));
    }
    let mut right: Vec<Seg> = Vec::new();
    for (n, i) in f.primary.iter().enumerate() {
        if n > 0 {
            right.push(pad(2));
        }
        right.extend(item_segs(i, t));
    }
    let rw: usize = right.iter().map(|s| s.width()).sum();
    let rows: Vec<Vec<Seg>> = if narrow {
        if left.is_empty() { vec![vec![]] } else { left }
    } else {
        let mut row = Vec::new();
        for (n, l) in left.into_iter().enumerate() {
            if n > 0 {
                row.push(pad(2));
            }
            row.extend(l);
        }
        vec![row]
    };
    rows.into_iter()
        .enumerate()
        .map(|(n, row)| {
            if n > 0 || right.is_empty() {
                return SLine::new(clip(&row, w));
            }
            let lw: usize = row.iter().map(|s| s.width()).sum();
            let mut segs = clip(&row, w.saturating_sub(rw));
            segs.push(pad(w.saturating_sub(lw.min(w.saturating_sub(rw)) + rw)));
            segs.extend(right.clone());
            SLine::new(segs)
        })
        .collect()
}

// ---- select ------------------------------------------------------------------

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
    /// `selection: "none"`: a dialog-level action that works without a selected option. Other
    /// actions are disabled while nothing is selected.
    pub no_selection: bool,
}

impl Action {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Action {
        Action { id: id.into(), title: title.into(), right: false, disabled: false, hidden: false, no_selection: false }
    }
    /// A key handled by the dialog without a footer entry.
    pub fn key(key: &str) -> Action {
        Action { id: format!("key:{key}"), title: String::new(), right: false, disabled: false, hidden: true, no_selection: true }
    }
    pub fn dialog_level(mut self) -> Action {
        self.no_selection = true;
        self
    }
    pub fn hidden(mut self, h: bool) -> Action {
        self.hidden = h;
        self
    }
    pub fn right(mut self) -> Action {
        self.right = true;
        self
    }
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
    /// Footer hints `(title, key)` (`footerHints`), on the left unless in `hints_right`.
    pub hints: Vec<(String, String)>,
    pub hints_right: Vec<(String, String)>,
    /// Free footer content placed before the left group.
    pub footer_text: Option<(String, crate::theme::Rgba)>,
    pub empty: Option<String>,
    pub empty_color: Option<crate::theme::Rgba>,
    pub no_match: Option<String>,
    pub can_back: bool,
    pub focus_current: bool,
    /// `locked`: input is ignored (e.g. while a dialog action runs).
    pub locked: bool,
    /// `moveTo(next, true)`: the next render scrolls the selected row to the middle of the list.
    pub center: bool,
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
        self.center = true;
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
            self.center = true;
        }
    }

    /// `isActionDisabled`: explicitly disabled, or needing a selection while there is none.
    pub fn action_disabled(&self, a: &Action) -> bool {
        a.disabled || (!a.no_selection && self.selected_opt().is_none())
    }

    fn visible_actions(&self, kb: &Keybinds) -> Vec<usize> {
        (0..self.actions.len()).filter(|i| !self.actions[*i].hidden && !action_label(&self.actions[*i], kb).is_empty()).collect()
    }

    /// The legacy footer: actions and hints split by side.
    pub fn footer(&self, kb: &Keybinds) -> Footer {
        let mut f = Footer::default();
        if let Some((text, c)) = &self.footer_text {
            f.children = vec![seg(text.clone(), fg(*c))];
        }
        for i in self.visible_actions(kb) {
            let a = &self.actions[i];
            let item = FooterItem { title: a.title.clone(), key: Some(action_label(a, kb)), color: None, disabled: self.action_disabled(a) };
            if a.right { f.primary.push(item) } else { f.secondary.push(item) }
        }
        f.secondary.extend(self.hints.iter().map(|(t, k)| FooterItem::new(t.clone(), k.clone())));
        f.primary.extend(self.hints_right.iter().map(|(t, k)| FooterItem::new(t.clone(), k.clone())));
        f
    }
}

pub fn action_label(a: &Action, kb: &Keybinds) -> String {
    match a.id.strip_prefix("key:") {
        Some(k) => k.to_string(),
        None => kb.shortcut(&a.id),
    }
}

// ---- other dialogs -------------------------------------------------------------

/// Single-line text input dialog (`DialogPrompt`).
#[derive(Clone, Debug, Default)]
pub struct PromptDialog {
    pub title: String,
    pub placeholder: String,
    pub value: String,
    pub cursor: usize,
    pub description: Vec<(String, crate::theme::Rgba)>,
    /// While set, the input is locked and the body shows a spinner with this text.
    pub busy: Option<String>,
    /// Extra footer actions `(title, keybind id)` after "submit".
    pub actions: Vec<(String, String)>,
    /// The description is an (empty) box: it still takes a row above the input.
    pub description_gap: bool,
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

/// A text dialog in the shared layout: a header, free-form lines and an optional footer.
#[derive(Clone, Debug, Default)]
pub struct TextDialog {
    pub title: String,
    /// Text after the title (`DialogHeader` children), muted: `(text, grows)`. A growing text fills the
    /// row and is clipped (the `/btw` question); otherwise it sits right-aligned before `esc` (a status).
    pub title_extra: Option<(String, bool)>,
    /// Header gap between title, extra text and the close label (`gap`, 2 for headers with extras).
    pub header_gap: usize,
    /// Close label, `esc` by default.
    pub close_label: String,
    /// Fixed rows above the scrolling body.
    pub pre: Vec<SLine>,
    pub lines: Vec<SLine>,
    pub scroll: usize,
    /// Fixed height of the scrolling body (else up to the terminal height - 10).
    pub height: Option<usize>,
    /// Raised band behind the body that bleeds over the side padding (`DialogBleed`), with one blank
    /// row above and below the text.
    pub band: Option<Rgba>,
    pub footer: Option<Footer>,
    /// A single right-aligned button in the legacy footer (`Close` in the export result).
    pub button: Option<String>,
    /// Side padding, `DIALOG_INSET` by default (the fork dialog's pending view uses the list inset).
    pub inset: Option<usize>,
    /// The band's blank rows are part of `lines` and scroll with them (a scrollbox's padding).
    pub band_scrolls: bool,
    /// Rows inside the header box, under the title row (a source path).
    pub under_header: Vec<SLine>,
    /// Color of `title_extra` (muted by default).
    pub title_extra_color: Option<Rgba>,
    /// Fixed rows after the scrolling body (and its band), each group after a blank row.
    pub post: Vec<SLine>,
    /// A footer that renders nothing still takes its gap row (a legacy `DialogFooter` without actions).
    pub empty_footer_gap: bool,
}

impl TextDialog {
    pub fn new(title: impl Into<String>, lines: Vec<SLine>) -> TextDialog {
        TextDialog { title: title.into(), lines, close_label: "esc".into(), header_gap: 0, ..Default::default() }
    }
    pub fn footer(mut self, f: Footer) -> TextDialog {
        self.footer = Some(f);
        self
    }
}

#[derive(Clone, Debug)]
pub enum Body {
    Select(Box<Select>),
    Alert { title: String, message: String },
    Confirm { title: String, message: String, confirm: bool },
    Prompt(PromptDialog),
    Help,
    /// `DialogExportOptions` (legacy layout).
    Export(ExportOptions),
    /// Free-form lines in the shared layout.
    Text(Box<TextDialog>),
    /// The OpenCode Go offer (`DialogGoUpsell`).
    GoUpsell(Box<super::go_upsell::GoUpsell>),
    /// A string form field with options and a typed answer (`StringChoiceField`).
    Choice(Box<super::choice::ChoiceField>),
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
    pub fn text(purpose: &str, t: TextDialog) -> Dialog {
        Dialog::new(purpose, Body::Text(Box::new(t)))
    }
    pub fn sel(&mut self) -> Option<&mut Select> {
        match &mut self.body {
            Body::Select(s) => Some(s),
            _ => None,
        }
    }
    pub fn text_body(&mut self) -> Option<&mut TextDialog> {
        match &mut self.body {
            Body::Text(t) => Some(t),
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
    /// An unhandled key on a text dialog (purpose-specific bindings such as `c` to copy).
    Key(String),
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
            if sel.locked {
                return Outcome::Handled;
            }
            // Footer action keys.
            for a in sel.actions.clone() {
                if sel.action_disabled(&a) || (a.hidden && !a.id.starts_with("key:")) {
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
                return match sel.selected_value() {
                    Some(v) => Outcome::Select(v),
                    None => Outcome::Handled,
                };
            }
            if sel.render_filter {
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
            for (_, id) in p.actions.clone() {
                if is(&id) {
                    return Outcome::Action(id, Some(p.value.clone()));
                }
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
        Body::GoUpsell(g) => match super::go_upsell::handle_key(g, s) {
            super::go_upsell::Key::Close => Outcome::Close,
            super::go_upsell::Key::Run(action) => Outcome::Action(action.to_string(), None),
            super::go_upsell::Key::None => Outcome::Handled,
        },
        Body::Choice(c) => match c.handle_key(s, kb) {
            super::choice::ChoiceKey::Close => Outcome::Close,
            super::choice::ChoiceKey::Submit(v) => Outcome::Submit(serde_json::to_string(&v).unwrap_or_default()),
            super::choice::ChoiceKey::None => Outcome::Handled,
        },
        Body::Text(td) => match name {
            "escape" => Outcome::Close,
            "c" if s.ctrl => Outcome::Close,
            "up" | "k" if !s.ctrl => {
                td.scroll = td.scroll.saturating_sub(1);
                Outcome::Handled
            }
            "down" | "j" if !s.ctrl => {
                td.scroll += 1;
                Outcome::Handled
            }
            "pageup" => {
                td.scroll = td.scroll.saturating_sub(td.height.unwrap_or(10));
                Outcome::Handled
            }
            "pagedown" => {
                td.scroll += td.height.unwrap_or(10);
                Outcome::Handled
            }
            "home" => {
                td.scroll = 0;
                Outcome::Handled
            }
            "end" => {
                td.scroll = usize::MAX / 2;
                Outcome::Handled
            }
            _ => Outcome::Key(s.name.clone()),
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
        Body::Choice(c) => c.paste(&text),
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
    if let Body::GoUpsell(g) = &d.body {
        let (panel, buttons) = super::go_upsell::render(buf, area, g, base);
        let hits = buttons.into_iter().map(|(r, i)| (r, DialogHit::Action(super::go_upsell::ACTIONS[i].to_string()))).collect();
        return (panel, hits, None);
    }
    let w = d.size.width().min(area.width.saturating_sub(2)).max(10);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let mut hits = Vec::new();
    let mut cursor = None;
    let wu = w as usize;
    let inner = wu.saturating_sub(DIALOG_INSET * 2);
    let narrow = area.width < 60;
    let ins = |l: SLine| l.prefixed(&[pad(DIALOG_INSET)]);
    // The legacy single focused button, right-aligned (`paddingLeft/Right` around the label).
    let button = |label: &str, padding: usize| -> SLine {
        let text = format!("{}{label}{}", " ".repeat(padding), " ".repeat(padding));
        let bw = sw(&text);
        SLine::new(vec![pad(DIALOG_INSET + inner.saturating_sub(bw)), Seg::new(text, fg_bg(t.text.action.primary.focused, t.background.action.primary.focused))])
            .on_click(Some(Click::Custom("ok".into())))
    };
    let lines: Vec<SLine> = match &mut d.body {
        Body::Select(sel) => select_lines(sel, wu, area.height as usize, area.width as usize, d.size, t, kb, &mut cursor),
        Body::Alert { title, message } => {
            let mut v = vec![header(title, "esc", t, wu)];
            v.push(SLine::blank());
            for l in wrap(&[seg(message.clone(), fg(t.text.muted))], inner, Wrap::Word) {
                v.push(ins(SLine::new(l)));
            }
            v.push(SLine::blank());
            v.push(button("ok", 3));
            v.push(SLine::blank());
            v
        }
        Body::Help => {
            let mut v = vec![header("Help", "esc/enter", t, wu)];
            v.push(SLine::blank());
            let text = format!("Press {} to see all available actions and commands in any context.", kb.shortcut("command.palette.show"));
            for l in wrap(&[seg(text, fg(t.text.muted))], inner, Wrap::Word) {
                v.push(ins(SLine::new(l)));
            }
            v.push(SLine::blank());
            v.push(button("ok", 3));
            v.push(SLine::blank());
            v
        }
        Body::Confirm { title, message, confirm } => {
            let mut v = vec![header(title, "esc", t, wu)];
            v.push(SLine::blank());
            for l in wrap(&[seg(message.clone(), fg(t.text.muted))], inner, Wrap::Word) {
                v.push(ins(SLine::new(l)));
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
            v.push(SLine::new(vec![pad(DIALOG_INSET + inner.saturating_sub(used)), cancel, ok]));
            v.push(SLine::blank());
            v
        }
        Body::Prompt(p) => {
            let mut v = vec![header(&p.title, "esc", t, wu)];
            v.push(SLine::blank());
            for (text, c) in &p.description {
                for l in wrap(&[seg(text.clone(), fg(*c))], inner, Wrap::Word) {
                    v.push(ins(SLine::new(l)));
                }
            }
            if !p.description.is_empty() || p.description_gap {
                v.push(SLine::blank());
            }
            let iw = inner;
            let line = if p.value.is_empty() {
                vec![pad(DIALOG_INSET), seg(locale::take_width(&p.placeholder, iw), fg(t.text.muted))]
            } else {
                let start = sw(&p.value[..p.cursor]).saturating_sub(iw.saturating_sub(1));
                let shown: String = locale::take_width(&p.value.chars().skip(start).collect::<String>(), iw);
                vec![pad(DIALOG_INSET), seg(shown, fg(if p.busy.is_some() { t.text.formfield.disabled } else { t.text.formfield.base }))]
            };
            let row = v.len();
            v.push(SLine::new(line));
            if p.busy.is_none() {
                cursor = Some(((DIALOG_INSET + sw(&p.value[..p.cursor]).min(iw.saturating_sub(1))) as u16, row as u16));
            }
            if let Some(b) = &p.busy {
                v.push(SLine::blank());
                v.push(ins(SLine::new(vec![seg(format!("{SPINNER_GLYPH} {b}"), fg(t.text.muted))])));
            }
            v.push(SLine::blank());
            match &p.busy {
                Some(_) => v.push(ins(SLine::new(vec![seg("processing…", fg(t.text.muted))]))),
                None => {
                    let mut f = Footer::default();
                    let submit = kb.shortcut("dialog.prompt.submit");
                    if !submit.is_empty() {
                        f.secondary.push(FooterItem::new("submit", submit));
                    }
                    for (title, id) in &p.actions {
                        let key = kb.shortcut(id);
                        if !key.is_empty() {
                            f.secondary.push(FooterItem::new(title.clone(), key));
                        }
                    }
                    v.extend(footer_lines(&f, inner, narrow, t).into_iter().map(ins));
                }
            }
            v.push(SLine::blank());
            v
        }
        Body::Export(e) => {
            let mut v = vec![header("Export session", "esc", t, wu)];
            v.push(SLine::blank());
            let ff = &t.text.formfield;
            // Only the focused control is highlighted; the mark alone shows what is checked.
            let mark = |id: &str, checked: bool| if e.focused() == id { ff.focused } else if checked { ff.selected } else { t.text.muted };
            let label = |id: &str| if e.focused() == id { ff.focused } else { ff.base };
            v.push(ins(SLine::new(vec![
                seg("Export as:", fg(t.text.base)),
                pad(2),
                seg(if e.json { "○" } else { "◉" }, fg(mark("markdown", !e.json))),
                pad(1),
                seg("Markdown", fg(label("markdown"))),
                pad(3),
                seg(if e.json { "◉" } else { "○" }, fg(mark("json", e.json))),
                pad(1),
                seg("JSON", fg(label("json"))),
            ])));
            let check = |b: bool| if b { "[x]" } else { "[ ]" };
            let row = |id: &str, checked: bool, text: &str| ins(SLine::new(vec![seg(check(checked), fg(mark(id, checked))), pad(1), seg(text.to_string(), fg(label(id)))]));
            if e.json {
                v.push(SLine::blank());
                v.push(row("sanitize", e.sanitize, "Sanitize sensitive data"));
            } else {
                v.push(SLine::blank());
                v.push(row("thinking", e.thinking, "Include thinking"));
                v.push(SLine::blank());
                v.push(row("tools", e.tools, "Include tools"));
            }
            v.push(SLine::blank());
            let btn = |id: &str, text: &str| {
                let focused = e.focused() == id;
                let (f, b) = if focused { (t.text.action.primary.focused, t.background.action.primary.focused) } else { (t.text.action.primary.base, t.background.action.primary.base) };
                Seg::new(format!("    {text}    "), fg_bg(f, b))
            };
            let copy = btn("copy", "Copy");
            let export = btn("export", "Export");
            let used = copy.width() + 1 + export.width();
            v.push(SLine::new(vec![pad(DIALOG_INSET + inner.saturating_sub(used)), copy, pad(1), export]));
            v.push(SLine::blank());
            v
        }
        Body::Text(td) => text_lines(td, wu, area, narrow, t),
        Body::Choice(c) => {
            let (lines, at) = super::choice::lines(c, wu, area.height as usize, narrow, t);
            cursor = at;
            lines
        }
        Body::GoUpsell(_) => Vec::new(),
    };
    let h = (lines.len() as u16 + 1).min(area.height);
    let y = if d.centered { area.y + area.height.saturating_sub(h) / 2 } else { area.y + ((area.height as f64) / 4.0).round() as u16 };
    // A dialog taller than the room below its top (a quarter down) runs off the bottom, as upstream's
    // `paddingTop: height / 4` box does, rather than moving up.
    let y = y.min(area.bottom().saturating_sub(1));
    let h = h.min(area.bottom().saturating_sub(y));
    let panel = Rect::new(x, y, w, h);
    fill(buf, panel, t.background.base.tui());
    let inner_rect = Rect::new(x, y + 1, w, h.saturating_sub(1));
    for (rect, click) in draw_lines(buf, inner_rect, &lines, 0, SPINNER_FRAMES[0]) {
        let hit = match click {
            Click::Custom(c) if c == "ok" => DialogHit::Ok,
            Click::Custom(c) if c == "esc" => DialogHit::Close,
            Click::Custom(c) if c.starts_with("opt:") => DialogHit::Option(c[4..].parse().unwrap_or(0)),
            Click::Custom(c) if c.starts_with("act:") => DialogHit::Action(c[4..].to_string()),
            _ => continue,
        };
        hits.push((rect, hit));
    }
    let cursor = cursor.map(|(cx, cy)| (x + cx, inner_rect.y + cy));
    (panel, hits, cursor)
}

fn text_lines(td: &mut TextDialog, w: usize, area: Rect, narrow: bool, t: &Theme) -> Vec<SLine> {
    let ins = td.inset.unwrap_or(DIALOG_INSET);
    let inner = w.saturating_sub(ins * 2);
    let mut v = vec![text_header(td, t, w)];
    for l in &td.under_header {
        v.push(l.clone().prefixed(&[pad(ins)]));
    }
    v.push(SLine::blank());
    for l in &td.pre {
        v.push(l.clone().prefixed(&[pad(ins)]));
    }
    let max = td.height.unwrap_or_else(|| (area.height as usize).saturating_sub(10).max(3));
    let total = td.lines.len();
    td.scroll = td.scroll.min(total.saturating_sub(max));
    let shown: Vec<SLine> = td.lines.iter().skip(td.scroll).take(max).cloned().collect();
    match td.band {
        Some(band) => {
            let row = |l: SLine| l.prefixed(&[pad(ins)]).with_bg(Some(band.tui()));
            if !td.band_scrolls {
                v.push(SLine::blank().with_bg(Some(band.tui())));
            }
            for l in shown {
                v.push(row(l));
            }
            if !td.band_scrolls {
                v.push(SLine::blank().with_bg(Some(band.tui())));
            }
        }
        None => {
            for l in shown {
                v.push(l.prefixed(&[pad(ins)]));
            }
            if let Some(h) = td.height {
                for _ in total.saturating_sub(td.scroll).min(h)..h {
                    v.push(SLine::blank());
                }
            }
        }
    }
    if !td.post.is_empty() {
        v.push(SLine::blank());
        for l in &td.post {
            v.push(l.clone().prefixed(&[pad(ins)]));
        }
    }
    if let Some(label) = &td.button {
        v.push(SLine::blank());
        let text = format!("   {label}   ");
        let bw = sw(&text);
        v.push(
            SLine::new(vec![pad(ins + inner.saturating_sub(bw)), Seg::new(text, fg_bg(t.text.action.primary.focused, t.background.action.primary.focused))])
                .on_click(Some(Click::Custom("ok".into()))),
        );
    } else if let Some(f) = td.footer.as_ref().filter(|f| !f.is_empty()) {
        v.push(SLine::blank());
        v.extend(footer_lines(f, inner, narrow, t).into_iter().map(|l| l.prefixed(&[pad(ins)])));
    } else if td.empty_footer_gap {
        v.push(SLine::blank());
    }
    v.push(SLine::blank());
    v
}

fn pad(n: usize) -> Seg {
    Seg::new(" ".repeat(n), Style::default())
}

/// `DialogHeader` at `DIALOG_INSET`: bold title, `esc` (or another close label) at the right.
fn header(title: &str, right: &str, t: &Theme, w: usize) -> SLine {
    header_at(title, right, t, w, DIALOG_INSET)
}

fn header_at(title: &str, right: &str, t: &Theme, w: usize, inset: usize) -> SLine {
    let avail = w.saturating_sub(inset * 2 + sw(right));
    let title = locale::take_width(title, avail);
    let gap = w.saturating_sub(inset * 2 + sw(&title) + sw(right));
    SLine::new(vec![pad(inset), Seg::new(title, bold(fg(t.text.base))), pad(gap), seg(right, fg(t.text.muted))]).on_click(Some(Click::Custom("esc".into())))
}

fn text_header(td: &TextDialog, t: &Theme, w: usize) -> SLine {
    let ins = td.inset.unwrap_or(DIALOG_INSET);
    let Some((extra, grows)) = &td.title_extra else {
        return header_at(&td.title, &td.close_label, t, w, ins);
    };
    let inner = w.saturating_sub(ins * 2);
    let gap = td.header_gap.max(1);
    let close = td.close_label.as_str();
    let tw = sw(&td.title);
    let mut segs = vec![pad(ins), Seg::new(td.title.clone(), bold(fg(t.text.base)))];
    if *grows {
        // Title, gap, the extra text clipped to the room left, gap, close label.
        let room = inner.saturating_sub(tw + gap * 2 + sw(close));
        let text = locale::take_width(extra, room);
        segs.push(pad(gap));
        segs.push(seg(text.clone(), fg(t.text.muted)));
        segs.push(pad(gap + room.saturating_sub(sw(&text))));
    } else {
        let room = inner.saturating_sub(tw + gap * 2 + sw(close));
        let text = locale::take_width(extra, room);
        segs.push(pad(gap + room.saturating_sub(sw(&text))));
        segs.push(seg(text, fg(td.title_extra_color.unwrap_or(t.text.muted))));
        segs.push(pad(gap));
    }
    segs.push(seg(close, fg(t.text.muted)));
    SLine::new(segs).on_click(Some(Click::Custom("esc".into())))
}

#[allow(clippy::too_many_arguments)]
fn select_lines(sel: &mut Select, w: usize, term_h: usize, term_w: usize, size: Size, t: &Theme, kb: &Keybinds, cursor: &mut Option<(u16, u16)>) -> Vec<SLine> {
    let li = DIALOG_LIST_INSET;
    let mut out = Vec::new();
    // Header: the title (and suffix) at the list inset, `esc` at the right.
    let mut title_segs = vec![pad(li), Seg::new(sel.title.clone(), bold(fg(t.text.base)))];
    if let Some(sfx) = &sel.title_suffix {
        title_segs.push(seg(sfx.clone(), fg(t.text.muted)));
    }
    let used: usize = title_segs.iter().map(|s| s.width()).sum();
    title_segs.push(pad(w.saturating_sub(used + 3 + li)));
    title_segs.push(seg("esc", fg(t.text.muted)));
    out.push(SLine::new(title_segs).on_click(Some(Click::Custom("esc".into()))));
    if sel.render_filter {
        out.push(SLine::blank());
        let iw = w.saturating_sub(li * 2);
        let line = if sel.filter.is_empty() {
            SLine::new(vec![pad(li), seg(locale::take_width(&sel.placeholder, iw), fg(t.text.muted))])
        } else {
            SLine::new(vec![pad(li), seg(locale::take_width(&sel.filter, iw), fg(t.text.formfield.focused))])
        };
        *cursor = Some(((li + sw(&sel.filter).min(iw)) as u16, out.len() as u16));
        out.push(line);
    }
    out.push(SLine::blank());
    let items = sel.items();
    if items.is_empty() {
        let text = if !sel.filter.is_empty() && sel.render_filter { sel.no_match.clone().unwrap_or_else(|| "No results found".into()) } else { sel.empty.clone().unwrap_or_else(|| "No items available".into()) };
        let c = sel.empty_color.unwrap_or(t.text.muted);
        for l in wrap(&[seg(text, fg(c))], w.saturating_sub(li * 2).max(1), Wrap::Word) {
            out.push(SLine::new(l).prefixed(&[pad(li)]));
        }
    } else {
        // Build all rows, then window to the list height.
        let mut rows: Vec<(SLine, Option<usize>)> = Vec::new();
        let vis = sel.visible();
        let selected_opt = vis.get(sel.selected).copied();
        let flat = sel.flattened();
        let details_w = select_content_width((size.width() as usize).min(term_w)).max(1);
        for item in &items {
            match item {
                Item::Header(c, first) => {
                    if !first {
                        rows.push((SLine::blank(), None));
                    }
                    rows.push((SLine::new(vec![pad(li), Seg::new(c.clone(), bold(fg(t.accent(200))))]), None));
                }
                Item::Opt(i) => {
                    let o = &sel.options[*i];
                    let active = selected_opt == Some(*i);
                    let is_current = sel.current.as_deref() == Some(o.value.as_str());
                    let text_c = if active {
                        o.fg.unwrap_or(t.text.action.primary.focused)
                    } else if is_current {
                        t.text.formfield.selected
                    } else {
                        t.text.base
                    };
                    let secondary = if active { text_c } else { t.text.muted };
                    let mut st = fg(text_c);
                    if active {
                        st = st.add_modifier(Modifier::BOLD);
                    }
                    // The row spans 1..w-1 (scroll padding); a marker or gutter takes its cell plus
                    // the row gap out of the left padding, so titles line up at the list inset.
                    let mut segs = vec![pad(1)];
                    if let Some(g) = &o.gutter {
                        segs.push(pad(li - 3));
                        let gs = if g.style.fg.is_none() { g.style.fg(text_c.tui()) } else { g.style };
                        segs.push(Seg::new(g.text.clone(), if active { gs.fg(text_c.tui()) } else { gs }));
                        segs.push(pad(1));
                    } else if is_current {
                        segs.push(pad(li - 3));
                        segs.push(Seg::new("●", fg(text_c)));
                        segs.push(pad(1));
                    } else {
                        segs.push(pad(li - 1));
                    }
                    let footer = if flat { o.search_footer.clone().or(o.category.clone()).or(o.footer.clone()) } else { o.footer.clone() };
                    let footer_w = footer.as_deref().map(sw).unwrap_or(0);
                    let row_right = w.saturating_sub(1 + (li - 1));
                    let used: usize = segs.iter().map(|s| s.width()).sum();
                    let avail = row_right.saturating_sub(used + if footer_w > 0 { footer_w + 1 } else { 0 });
                    let tw = o.title_width.unwrap_or(61);
                    let title = if o.truncate_left { locale::truncate_left(&o.title, tw) } else { locale::truncate(&o.title, tw) };
                    let mut text = vec![Seg::new(title, st)];
                    if let Some(dsc) = &o.description
                        && Some(dsc) != o.category.as_ref()
                    {
                        // The description shares the title's text node, so it is bold on the active row too.
                        let mut ds = fg(secondary);
                        if active {
                            ds = ds.add_modifier(Modifier::BOLD);
                        }
                        text.push(seg(format!(" {dsc}"), ds));
                    }
                    segs.extend(clip(&text, avail));
                    if let Some(f) = footer {
                        let fc = match o.footer_color {
                            Some(c) if !active => c,
                            _ => secondary,
                        };
                        let used: usize = segs.iter().map(|s| s.width()).sum();
                        segs.push(pad(row_right.saturating_sub(used + footer_w)));
                        segs.push(seg(f, fg(fc)));
                    }
                    let mut line = SLine::new(segs).on_click(Some(Click::Custom(format!("opt:{}", vis.iter().position(|x| x == i).unwrap_or(0)))));
                    if active {
                        let bgc = o.bg.unwrap_or(t.background.action.primary.focused);
                        let mut ss: Vec<Seg> = line.segs.iter().skip(1).cloned().collect();
                        let used: usize = ss.iter().map(|s| s.width()).sum();
                        ss.push(pad(w.saturating_sub(used + 2)));
                        let ss: Vec<Seg> = ss.into_iter().map(|s| Seg::new(s.text, s.style.bg(bgc.tui()))).collect();
                        line.segs = std::iter::once(pad(1)).chain(ss).collect();
                    }
                    rows.push((line, Some(*i)));
                    for dline in &o.details {
                        rows.push((SLine::new(vec![pad(li), seg(locale::truncate_middle(dline, details_w), fg(t.text.muted))]), None));
                    }
                }
            }
        }
        let list_h = rows.len().min((term_h / 2).saturating_sub(6).max(1));
        // Keep the selected row visible with minimal scrolling.
        let sel_row = rows.iter().position(|(_, i)| *i == selected_opt && i.is_some()).unwrap_or(0);
        if std::mem::take(&mut sel.center) {
            // `scrollToSelection(true)`: the selected row goes to the middle of the list.
            sel.scroll = sel_row.saturating_sub(list_h / 2);
        } else if sel.selected == 0 {
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
    // Footer row (`DialogFooter` at the list inset); without one, an empty box keeps the gap.
    let footer = sel.footer(kb);
    if !footer.is_empty() {
        out.extend(footer_lines(&footer, w.saturating_sub(li * 2), term_w < 60, t).into_iter().map(|l| l.prefixed(&[pad(li)])));
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

    #[test]
    fn footer_label_first_and_sides() {
        let t = Theme::default();
        let f = Footer { children: vec![seg("note", Style::default())], secondary: vec![FooterItem::new("copy", "c")], primary: vec![FooterItem::new("all projects", "ctrl+a")] };
        let rows = footer_lines(&f, 50, false, &t);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text(), format!("note  copy c{}all projects ctrl+a", " ".repeat(50 - 12 - 19)));
        let rows = footer_lines(&f, 50, true, &t);
        assert_eq!(rows.iter().map(|r| r.text().trim_end().to_string()).collect::<Vec<_>>(), vec![format!("note{}all projects ctrl+a", " ".repeat(50 - 4 - 19)), "copy c".to_string()]);
    }
}
