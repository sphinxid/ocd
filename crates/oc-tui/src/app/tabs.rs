//! Session tabs: horizontal strip, vertical rail (normal + compact), tab context menu and rail
//! resizing (`src/component/session-tabs.tsx`, `src/context/session-tabs-model.ts`).

use super::{App, Route, Toast};
use crate::theme::{Rgba, Theme, tint};
use crate::ui::text::*;
use crate::util::locale::{self, width as sw};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

pub const SESSION_SIDEBAR_WIDTH: u16 = 42;
pub const SESSION_TABS_COMPACT_WIDTH: u16 = 5;
pub const SESSION_SIDEBAR_MAX_WIDTH: u16 = 72;
pub const SESSION_CONTENT_MIN_WIDTH: u16 = 44;
pub const SESSION_TABS_COMPACT_BREAKPOINT: u16 = 12;
pub const NEW_SESSION_TAB_TITLE: &str = "New session";

#[derive(Clone, Debug, PartialEq)]
pub struct TabMenu {
    pub x: u16,
    pub y: u16,
    pub session: Option<String>,
    pub selected: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resizing {
    Rail,
    Pane,
}

#[derive(Clone, Debug, Default)]
pub struct TabStatus {
    pub attention: Option<&'static str>,
    pub busy: bool,
    pub unread: Option<&'static str>,
}

/// `sessionTabDetail(project, current, default, worktree)`.
pub fn session_tab_detail(project: &str, current: Option<&str>, default: Option<&str>, worktree: bool) -> String {
    let branch = if worktree && current != default { current } else { None };
    match branch {
        Some(b) if !project.is_empty() => format!("{project} ⎇ {b}"),
        Some(b) => b.to_string(),
        None => project.to_string(),
    }
}

/// `clampVerticalTabsWidth`: `max(5, min(w, 72, termW - 44))`.
pub fn clamp_rail(w: u16, term_w: u16) -> u16 {
    SESSION_TABS_COMPACT_WIDTH.max(w.min(SESSION_SIDEBAR_MAX_WIDTH).min(term_w.saturating_sub(SESSION_CONTENT_MIN_WIDTH)))
}

/// `adaptiveSessionTabLayout`: (start index, widths of visible tabs).
pub fn tab_layout(n: usize, active: usize, avail: usize) -> (usize, Vec<usize>) {
    const TAB: usize = 22;
    const MAX: usize = 32;
    const MIN: usize = 8;
    if n == 0 {
        return (0, vec![]);
    }
    let mut visible = if avail >= TAB { 1 + (avail - TAB) / MIN } else { 1 }.clamp(1, n);
    let mut start = 0;
    let mut content = avail;
    for _ in 0..3 {
        start = if active + 1 > visible { active + 1 - visible } else { 0 };
        start = start.min(n - visible);
        let before = start;
        let after = n - start - visible;
        let markers = if before > 0 { before.to_string().len() + 2 } else { 0 } + if after > 0 { after.to_string().len() + 2 } else { 0 };
        content = avail.saturating_sub(markers);
        let next = if content >= TAB { 1 + (content - TAB) / MIN } else { 1 }.clamp(1, n);
        if next == visible {
            break;
        }
        visible = next;
    }
    let mut widths = vec![0; visible];
    if content >= TAB * visible {
        let total = content.min(MAX * visible);
        let each = total / visible;
        let rem = total % visible;
        for (i, w) in widths.iter_mut().enumerate() {
            *w = each + usize::from(i < rem);
        }
    } else if visible == 1 {
        widths[0] = content;
    } else {
        let total = content;
        let inactive = ((total - TAB.min(total)) / (visible - 1)).clamp(MIN, TAB);
        for (i, w) in widths.iter_mut().enumerate() {
            *w = inactive;
            if start + i == active {
                *w = total.saturating_sub(inactive * (visible - 1));
            }
        }
    }
    (start, widths)
}

impl App {
    pub fn rail_width(&self) -> u16 {
        clamp_rail(self.rail_pref, self.size.0)
    }

    /// Whether the vertical rail is shown (`tabs.layout == vertical` and it fits).
    pub fn vertical_tabs(&self) -> bool {
        self.vertical_tabs_fit() && self.tabs_available()
    }

    /// `tabs.layout == vertical` and the rail fits beside the session's preferred width.
    pub fn vertical_tabs_fit(&self) -> bool {
        self.config.tabs_layout() == "vertical" && self.size.0 >= self.rail_width() + 64
    }

    /// Tabs are shown (`tabsVisible`): available, and no full-screen panel covers them.
    pub fn tabs_available(&self) -> bool {
        self.tabs_available_base() && !self.panel_fullscreen()
    }

    /// `tabsAvailable`: tabs enabled with at least one tab, outside plugin routes.
    pub fn tabs_available_base(&self) -> bool {
        self.config.tabs_enabled() && !self.tabs.is_empty() && !matches!(self.route, Route::Plugin(_))
    }

    pub fn tab_status(&self, root: &str) -> TabStatus {
        let mut fam = self.store.family_of(root);
        if !fam.iter().any(|f| f == root) {
            fam.push(root.to_string());
        }
        let attention = if fam.iter().any(|id| self.store.permissions.get(id).is_some_and(|p| !p.is_empty())) {
            Some("permission")
        } else if fam.iter().any(|id| self.store.forms.get(id).is_some_and(|p| !p.is_empty())) {
            Some("question")
        } else {
            None
        };
        let busy = fam.iter().any(|id| self.store.is_running(id));
        let current = self.session_id().map(|s| self.store.root(s));
        let unread = self.store.session(root).and_then(|s| {
            let idle = s.time.idle?;
            if current.as_deref() == Some(root) || idle <= s.time.viewed.unwrap_or(0) {
                return None;
            }
            Some(if s.outcome.as_deref() == Some("failed") { "error" } else { "activity" })
        });
        TabStatus { attention, busy, unread }
    }

    /// Indicator label + color for a tab (`TabIndicator`).
    fn tab_indicator(&self, t: &Theme, id: Option<&str>, index: usize, bg: Rgba, selected: bool) -> (String, Rgba) {
        let numbers = self.config.tabs_indicators() == "numbers";
        let st = id.map(|i| self.tab_status(i)).unwrap_or_default();
        let unread_color = t.accent(200);
        let idle = tint(t.text.formfield.base, bg, 0.55);
        let feedback = if st.attention.is_some() {
            Some(t.accent(200))
        } else if st.unread == Some("error") {
            Some(t.text.feedback.error.base)
        } else {
            None
        };
        let color = if st.busy && st.attention.is_none() {
            t.interactive(200)
        } else if let Some(f) = feedback {
            f
        } else if st.unread.is_some() {
            unread_color
        } else {
            idle
        };
        let _ = selected;
        let label = if numbers {
            (index + 1).to_string()
        } else if st.attention == Some("permission") {
            "!".into()
        } else if st.attention == Some("question") {
            "?".into()
        } else if st.busy {
            if self.config.animations() { SPINNER_GLYPH.to_string() } else { "⠋".into() }
        } else if id.is_none() {
            "+".into()
        } else if st.unread.is_some() {
            "•".into()
        } else {
            String::new()
        };
        (label, color)
    }

    fn tab_title(&self, id: &str) -> String {
        self.store.session(id).and_then(|s| s.title.clone()).filter(|t| !t.is_empty()).unwrap_or_else(|| NEW_SESSION_TAB_TITLE.into())
    }

    fn tab_detail(&self, id: &str) -> String {
        let Some(s) = self.store.session(id) else { return String::new() };
        let dir = s.location.directory.clone();
        let loc = self.store.locations.get(&dir);
        let project = loc.and_then(|l| l.info.as_ref()).map(|i| locale::basename(&i.project.canonical).to_string()).unwrap_or_else(|| locale::basename(&dir).to_string());
        let vcs = loc.and_then(|l| l.vcs.as_ref());
        let current = vcs.and_then(|v| v.pointer("/branch/current")).and_then(|b| b.as_str());
        let default = vcs.and_then(|v| v.pointer("/branch/default")).and_then(|b| b.as_str());
        let worktree = loc.and_then(|l| l.info.as_ref()).is_some_and(|i| i.project.directory != i.project.canonical);
        session_tab_detail(&project, current, default, worktree)
    }

    /// Draw tabs (strip or rail) and return the remaining content area.
    pub fn draw_tabs_frame(&mut self, buf: &mut Buffer, area: Rect, t: &Theme) -> Rect {
        self.tab_hits.clear();
        if !self.tabs_available() {
            return area;
        }
        if self.vertical_tabs() {
            let w = self.rail_width();
            self.draw_rail(buf, Rect::new(area.x, area.y, w, area.height), t);
            return Rect::new(area.x + w, area.y, area.width - w, area.height);
        }
        if area.height <= 4 {
            return area;
        }
        self.draw_strip(buf, Rect::new(area.x, area.y, area.width, 1), t);
        Rect::new(area.x, area.y + 1, area.width, area.height - 1)
    }

    fn draw_strip(&mut self, buf: &mut Buffer, area: Rect, t: &Theme) {
        let current_root = self.session_id().map(|s| self.store.root(s));
        let mut items: Vec<(Option<String>, String)> = self.tabs.iter().map(|id| (Some(id.clone()), self.tab_title(id))).collect();
        let new_tab = current_root.is_none();
        if new_tab {
            items.push((None, "+".into()));
        }
        let n = items.len();
        let active = items.iter().position(|(id, _)| *id == current_root).unwrap_or(n - 1);
        let show_plus = !new_tab;
        let avail = (area.width as usize).saturating_sub(if show_plus { 3 } else { 0 });
        let (start, widths) = tab_layout(n, active, avail);
        let num_w = 2.max(n.to_string().len());
        let mut x = area.x;
        if start > 0 {
            let m = format!("‹{start} ");
            put(buf, x, area.y, &m, fg(t.text.muted), sw(&m));
            x += sw(&m) as u16;
        }
        for (k, wdt) in widths.iter().enumerate() {
            let i = start + k;
            let (id, title) = &items[i];
            let selected = i == active;
            let bgc = if selected { t.dec(t.background.raised.base) } else { t.background.base };
            let fgc = if selected { t.text.base } else { t.text.muted };
            let rect = Rect::new(x, area.y, *wdt as u16, 1);
            fill(buf, rect, bgc.tui());
            let (label, num_c) = self.tab_indicator(t, id.as_deref(), i, bgc, selected);
            let ind = format!("{label:>num_w$} ");
            put(buf, x, area.y, &ind, fg(num_c).bg(bgc.tui()), *wdt);
            let tw = wdt.saturating_sub(1 + num_w);
            let mut st = fg(fgc).bg(bgc.tui());
            if selected {
                st = st.add_modifier(Modifier::BOLD);
            }
            let title = if id.is_none() { NEW_SESSION_TAB_TITLE.to_string() } else { title.clone() };
            put(buf, x + (num_w + 1) as u16, area.y, &title_fade(&title, tw), st, tw);
            self.tab_hits.push((rect, id.clone()));
            x += *wdt as u16;
        }
        let shown_end = start + widths.len();
        if shown_end < n {
            let m = format!(" {}›", n - shown_end);
            put(buf, x, area.y, &m, fg(t.text.muted), sw(&m));
            x += sw(&m) as u16;
        }
        if show_plus {
            put(buf, x, area.y, " + ", fg(t.text.muted), 3);
            self.tab_hits.push((Rect::new(x, area.y, 3, 1), None));
        }
    }

    fn draw_rail(&mut self, buf: &mut Buffer, area: Rect, t: &Theme) {
        let bg = t.background.raised.base;
        fill(buf, area, bg.tui());
        let w = area.width;
        let compact = w < SESSION_TABS_COMPACT_BREAKPOINT;
        let current_root = self.session_id().map(|s| self.store.root(s));
        let new_tab = current_root.is_none();
        let n = self.tabs.len();
        let num_w = 2.max(n.to_string().len());
        let selected_bg = t.background.action.primary.selected;
        let stride: u16 = if compact { 2 } else { 3 };
        let mut y = area.y + 1;
        if compact {
            let x = area.x + (w.saturating_sub(1)) / 2;
            put(buf, x, y, "⌕", fg(t.text.action.secondary.base).bg(bg.tui()), 1);
            self.tab_hits.push((Rect::new(area.x, y, w, 1), Some("__search__".into())));
            y += 2;
        }
        // Keep the active tab in view.
        let active = if new_tab { n } else { self.tabs.iter().position(|id| Some(id) == current_root.as_ref()).unwrap_or(0) };
        let visible_rows = area.bottom().saturating_sub(y);
        let height = if compact { 1 } else { 2 };
        let top = active as u16 * stride;
        if top < self.rail_scroll {
            self.rail_scroll = top;
        }
        if top + height > self.rail_scroll + visible_rows {
            self.rail_scroll = top + height - visible_rows.min(top + height);
        }
        let scroll = self.rail_scroll;
        let tabs = self.tabs.clone();
        for (i, id) in tabs.iter().enumerate() {
            let ty = (i as u16 * stride).checked_sub(scroll).map(|o| y + o);
            let Some(ty) = ty else { continue };
            if ty + height > area.bottom() {
                break;
            }
            let selected = current_root.as_deref() == Some(id.as_str());
            let rowbg = if selected { selected_bg } else { bg };
            let rect = Rect::new(area.x, ty, w, height);
            if rowbg.a > 0 {
                fill(buf, rect, rowbg.tui());
            }
            let effective_bg = if rowbg.a > 0 { rowbg } else { bg };
            let (label, num_c) = self.tab_indicator(t, Some(id), i, effective_bg, selected);
            if compact {
                let label = if label.is_empty() {
                    locale::graphemes(&self.tab_title(id)).first().map(|g| g.to_string()).unwrap_or_else(|| "U".into())
                } else {
                    label
                };
                let x = area.x + (w.saturating_sub(sw(&label) as u16)) / 2;
                put(buf, x, ty, &label, fg(if selected { t.text.base } else { num_c }).bg(effective_bg.tui()), sw(&label));
                if selected {
                    put(buf, area.x, ty.saturating_sub(1), &"▄".repeat(w as usize), Style::default().fg(selected_bg.tui_or_reset()).bg(bg.tui()), w as usize);
                    if ty + 1 < area.bottom() {
                        put(buf, area.x, ty + 1, &"▀".repeat(w as usize), Style::default().fg(selected_bg.tui_or_reset()).bg(bg.tui()), w as usize);
                    }
                }
            } else {
                let ind = format!(" {label:>num_w$} ");
                put(buf, area.x, ty, &ind, fg(num_c).bg(effective_bg.tui()), ind.chars().count());
                let tx = area.x + (num_w + 2) as u16;
                let tw = (w as usize).saturating_sub(num_w + 2 + 1);
                let mut st = fg(t.text.base).bg(effective_bg.tui());
                if selected {
                    st = st.add_modifier(Modifier::BOLD);
                }
                put(buf, tx, ty, &title_fade(&self.tab_title(id), tw), st, tw);
                let detail = self.tab_detail(id);
                let dw = (w as usize).saturating_sub(num_w + 2 + 2);
                put(buf, tx, ty + 1, &locale::take_width(&detail, dw), fg(tint(t.text.muted, effective_bg, 0.35)).bg(effective_bg.tui()), dw);
            }
            self.tab_hits.push((rect, Some(id.clone())));
        }
        // Add slot.
        let ay = (n as u16 * stride).checked_sub(scroll).map(|o| y + o);
        if let Some(ay) = ay
            && ay < area.bottom()
        {
            let rect = Rect::new(area.x, ay, w, 1);
            if new_tab && selected_bg.a > 0 {
                fill(buf, rect, selected_bg.tui());
            }
            let rbg = if new_tab && selected_bg.a > 0 { selected_bg } else { bg };
            let plus_c = if new_tab { t.text.base } else { tint(t.text.formfield.base, rbg, 0.55) };
            if compact {
                put(buf, area.x + (w.saturating_sub(1)) / 2, ay, "+", fg(plus_c).bg(rbg.tui()), 1);
            } else {
                put(buf, area.x + 1, ay, "+ ", fg(plus_c).bg(rbg.tui()), 2);
                let mut st = fg(if new_tab { t.text.base } else { t.text.muted }).bg(rbg.tui());
                if new_tab {
                    st = st.add_modifier(Modifier::BOLD);
                }
                put(buf, area.x + 3, ay, NEW_SESSION_TAB_TITLE, st, (w as usize).saturating_sub(4));
            }
            self.tab_hits.push((rect, None));
        }
        // Resize handle highlight while resizing.
        if self.resizing == Some(Resizing::Rail) {
            for yy in area.top()..area.bottom() {
                buf[(area.right() - 1, yy)].set_bg(t.background.raised.high.tui());
            }
        }
    }

    // ---- context menu ------------------------------------------------------

    /// Menu rows: (title, actionable).
    pub fn tab_menu_items(&self, m: &TabMenu) -> Vec<(String, bool)> {
        match &m.session {
            Some(_) => vec![
                (NEW_SESSION_TAB_TITLE.into(), true),
                ("Rename".into(), true),
                ("Copy session ID".into(), true),
                ("Close".into(), true),
            ],
            None => {
                let mut v = vec![("Recently closed tabs".to_string(), false)];
                let closed: Vec<(String, bool)> = self
                    .closed_tabs
                    .iter()
                    .rev()
                    .take(10)
                    .map(|id| (self.store.session(id).and_then(|s| s.title.clone()).filter(|t| !t.is_empty()).unwrap_or_else(|| "Untitled session".into()), true))
                    .collect();
                if closed.is_empty() {
                    v.push(("No recently closed tabs".into(), false));
                }
                v.extend(closed);
                v
            }
        }
    }

    fn tab_menu_rect(&self, m: &TabMenu) -> Rect {
        let items = self.tab_menu_items(m);
        let (tw, th) = self.size;
        let w = (items.iter().map(|(t, _)| 50.min(sw(t) + 2)).max().unwrap_or(0).max(20) as u16).min(tw);
        let h = (items.len() as u16).min(th);
        let top = (m.y + 1).min(th.saturating_sub(h));
        let left = m.x.min(tw.saturating_sub(w));
        Rect::new(left, top, w, h)
    }

    pub fn draw_tab_menu(&mut self, buf: &mut Buffer, _area: Rect, t: &Theme) {
        let Some(m) = self.tab_menu.clone() else { return };
        let rect = self.tab_menu_rect(&m);
        fill(buf, rect, t.background.raised.base.tui());
        for (i, (title, actionable)) in self.tab_menu_items(&m).iter().enumerate() {
            let y = rect.y + i as u16;
            if y >= rect.bottom() {
                break;
            }
            let hovered = *actionable && m.selected == Some(i);
            let bg = if hovered { t.background.raised.high } else { t.background.raised.base };
            fill(buf, Rect::new(rect.x, y, rect.width, 1), bg.tui());
            let c = if *actionable { t.text.base } else { t.text.muted };
            put(buf, rect.x + 1, y, &locale::truncate_width(title, rect.width.saturating_sub(2) as usize), fg(c).bg(bg.tui()), rect.width.saturating_sub(2) as usize);
        }
    }

    pub fn open_tab_menu(&mut self, x: u16, y: u16, session: Option<String>) {
        self.tab_menu = Some(TabMenu { x, y, session, selected: None });
        self.dirty = true;
    }

    /// Run a menu row.
    pub fn tab_menu_run(&mut self, index: usize) {
        let Some(m) = self.tab_menu.take() else { return };
        let items = self.tab_menu_items(&m);
        if !items.get(index).is_some_and(|(_, a)| *a) {
            self.tab_menu = Some(m);
            return;
        }
        match &m.session {
            Some(id) => match index {
                0 => {
                    self.navigate(Route::Home);
                }
                1 => {
                    let title = self.store.session(id).map(|s| s.display_title()).unwrap_or_default();
                    let mut d = crate::ui::dialog::Dialog::new("rename", crate::ui::dialog::Body::Prompt(crate::ui::dialog::PromptDialog::new("Rename session", "Session title", &title)));
                    d.data = serde_json::json!(id);
                    self.open_dialog(d);
                }
                2 => {
                    crate::clipboard::write(id);
                    self.toast(Toast::new("info", "Session ID copied to clipboard"));
                }
                _ => {
                    let id = id.clone();
                    self.close_tab(&id);
                }
            },
            None => {
                let closed: Vec<String> = self.closed_tabs.iter().rev().take(10).cloned().collect();
                if let Some(id) = closed.get(index.saturating_sub(1)).cloned() {
                    self.closed_tabs.retain(|c| *c != id);
                    self.navigate(Route::Session(id));
                }
            }
        }
        self.dirty = true;
    }

    /// Keys while the tab menu is open (`menu` mode).
    pub fn tab_menu_key(&mut self, name: &str, ctrl: bool) -> bool {
        let Some(m) = self.tab_menu.clone() else { return false };
        let items = self.tab_menu_items(&m);
        let actionable: Vec<usize> = items.iter().enumerate().filter(|(_, (_, a))| *a).map(|(i, _)| i).collect();
        match (name, ctrl) {
            ("escape", _) | ("c", true) => self.tab_menu = None,
            ("down", false) | ("up", false) => {
                if actionable.is_empty() {
                    return true;
                }
                let cur = m.selected.and_then(|s| actionable.iter().position(|x| *x == s));
                let next = match (cur, name) {
                    (None, "down") => 0,
                    (None, _) => actionable.len() - 1,
                    (Some(c), "down") => (c + 1) % actionable.len(),
                    (Some(c), _) => (c + actionable.len() - 1) % actionable.len(),
                };
                if let Some(menu) = self.tab_menu.as_mut() {
                    menu.selected = Some(actionable[next]);
                }
            }
            ("return", false) => {
                if let Some(s) = m.selected {
                    self.tab_menu_run(s);
                }
            }
            _ => {}
        }
        self.dirty = true;
        true
    }

    /// Mouse handling for tabs/menu; returns true when consumed.
    pub fn tab_mouse(&mut self, kind: crossterm::event::MouseEventKind, x: u16, y: u16) -> bool {
        use crossterm::event::{MouseButton, MouseEventKind};
        let inside = |r: &Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        if let Some(m) = self.tab_menu.clone() {
            let rect = self.tab_menu_rect(&m);
            match kind {
                MouseEventKind::Moved => {
                    let row = if inside(&rect) { Some((y - rect.y) as usize) } else { None };
                    let items = self.tab_menu_items(&m);
                    if let Some(menu) = self.tab_menu.as_mut() {
                        menu.selected = row.filter(|r| items.get(*r).is_some_and(|(_, a)| *a));
                    }
                }
                MouseEventKind::Down(MouseButton::Left) if !inside(&rect) => self.tab_menu = None,
                MouseEventKind::Down(MouseButton::Right) => self.tab_menu = None,
                MouseEventKind::Up(MouseButton::Left) if inside(&rect) => self.tab_menu_run((y - rect.y) as usize),
                _ => {}
            }
            self.dirty = true;
            return true;
        }
        // Rail resize.
        if self.vertical_tabs() {
            let w = self.rail_width();
            match kind {
                MouseEventKind::Down(MouseButton::Left) if x + 1 >= w && x <= w => {
                    let now = std::time::Instant::now();
                    if self.last_handle_click.is_some_and(|t| now.duration_since(t).as_millis() < 300) {
                        self.rail_pref = SESSION_SIDEBAR_WIDTH;
                        self.save_rail_width();
                        self.last_handle_click = None;
                    } else {
                        self.last_handle_click = Some(now);
                        self.resizing = Some(Resizing::Rail);
                    }
                    self.dirty = true;
                    return true;
                }
                MouseEventKind::Drag(MouseButton::Left) if self.resizing == Some(Resizing::Rail) => {
                    self.rail_pref = clamp_rail(x + 1, self.size.0);
                    self.dirty = true;
                    return true;
                }
                MouseEventKind::Up(MouseButton::Left) if self.resizing == Some(Resizing::Rail) => {
                    self.resizing = None;
                    self.save_rail_width();
                    self.dirty = true;
                    return true;
                }
                _ => {}
            }
        }
        let hit = self.tab_hits.iter().find(|(r, _)| inside(r)).cloned();
        let Some((rect, id)) = hit else { return false };
        match kind {
            MouseEventKind::Down(MouseButton::Right) => {
                if id.as_deref() != Some("__search__") {
                    self.open_tab_menu(x, y, id);
                }
            }
            MouseEventKind::Down(MouseButton::Middle) => {
                if let Some(id) = id.filter(|i| i != "__search__") {
                    self.close_tab(&id);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => match id.as_deref() {
                Some("__search__") => {
                    self.run_command("session.list", None);
                }
                Some(id) => {
                    let id = id.to_string();
                    self.navigate(Route::Session(id));
                }
                None => {
                    self.run_command("session.new", None);
                }
            },
            _ => return inside(&rect) && matches!(kind, MouseEventKind::Down(_)),
        }
        self.dirty = true;
        true
    }

    pub fn save_rail_width(&self) {
        let w = self.rail_pref;
        self.keyed.update("layout", move |v| {
            if !v.is_object() {
                *v = serde_json::json!({});
            }
            v["verticalTabsWidth"] = serde_json::json!(w);
        });
    }
}

/// Title fade: when a title overflows, the last 4 cells are cut with an ellipsis-free fade
/// (approximated by a hard cut).
fn title_fade(title: &str, w: usize) -> String {
    locale::take_width(title, w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_fit() {
        let (start, w) = tab_layout(3, 0, 100);
        assert_eq!(start, 0);
        assert_eq!(w.iter().sum::<usize>(), 96);
        let (start, w) = tab_layout(20, 19, 80);
        assert!(start > 0);
        assert!(w.iter().all(|x| *x >= 8));
    }

    #[test]
    fn detail_and_clamp() {
        assert_eq!(session_tab_detail("proj", Some("feat"), Some("main"), true), "proj ⎇ feat");
        assert_eq!(session_tab_detail("proj", Some("main"), Some("main"), true), "proj");
        assert_eq!(session_tab_detail("proj", Some("feat"), Some("main"), false), "proj");
        assert_eq!(clamp_rail(42, 200), 42);
        assert_eq!(clamp_rail(100, 200), 72);
        assert_eq!(clamp_rail(42, 60), 16);
        assert_eq!(clamp_rail(1, 200), 5);
    }
}
