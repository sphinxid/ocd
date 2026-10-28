//! Footer menus (`src/mini/footer.menu.tsx`): selection state and the row renderer shared by the
//! autocomplete and the command panels.

use super::cell::{Buffer, Color, StyledChunk, attr};
use super::theme::FooterTheme;
use super::util::{move_selection_clamp, move_selection_offset, reconcile_selection, reveal_selection_offset};
use crate::util::locale::{self, width as sw};

pub const FOOTER_MENU_ROWS: usize = 8;
pub const FOOTER_COMPACT_WIDTH: usize = 40;

/// `footerMenuText(text, width, mono)`.
pub fn footer_menu_text(text: &str, width: usize, mono: bool) -> String {
    if !mono {
        return locale::truncate_width(text, width);
    }
    if sw(text) <= width {
        return text.to_string();
    }
    let suffix = ".".repeat(width.min(3));
    locale::take_width(text, width.saturating_sub(suffix.len())) + &suffix
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FooterTone {
    Selection,
    Running,
    Error,
    Success,
}

/// `RunFooterMenuItem`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct MenuItem {
    pub display: String,
    /// One-cell icon glyph and its color (`icon(color)` renders a 2-wide box).
    pub icon: Option<(String, Option<Color>)>,
    pub current: bool,
    pub description: Option<String>,
    pub category: Option<String>,
    pub footer: Option<String>,
    pub footer_tone: Option<FooterTone>,
}

/// `createFooterMenuState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct MenuState {
    pub selected: i64,
    pub offset: i64,
}

impl MenuState {
    pub fn reset(&mut self) {
        self.selected = 0;
        self.offset = 0;
    }

    /// Reconcile after the item count or limit changed.
    pub fn reconcile(&mut self, count: i64, limit: i64) {
        let next = reconcile_selection(self.selected, count);
        self.selected = next;
        self.offset = reveal_selection_offset(self.offset, count, limit.max(1), next);
    }

    pub fn reveal(&mut self, index: i64, count: i64, limit: i64) {
        let next = reconcile_selection(index, count);
        self.selected = next;
        self.offset = reveal_selection_offset(self.offset, count, limit.max(1), next);
    }

    pub fn move_by(&mut self, dir: i64, count: i64, limit: i64) {
        let next = move_selection_clamp(self.selected, count, dir);
        self.selected = next;
        self.offset = move_selection_offset(self.offset, count, limit.max(1), next, dir);
    }

    pub fn rows(&self, count: usize, limit: usize) -> usize {
        limit.max(1).min(count).max(1)
    }
}

enum Row<'a> {
    Header(&'a str),
    Item(&'a MenuItem, usize),
    Spacer,
}

/// Options of `RunFooterMenu`.
pub struct MenuView<'a> {
    pub items: &'a [MenuItem],
    pub selected: i64,
    pub offset: i64,
    pub rows: usize,
    pub limit: usize,
    pub empty: Option<&'a str>,
    pub border: bool,
    pub padding_left: usize,
    pub padding_right: usize,
    pub grouped: bool,
    pub compact: bool,
    pub background: bool,
    pub header_color: Option<Color>,
    pub mono: bool,
    /// Grouped scroll offset (kept by the caller between frames).
    pub group_offset: i64,
}

impl<'a> MenuView<'a> {
    pub fn new(items: &'a [MenuItem], state: MenuState, rows: usize) -> MenuView<'a> {
        MenuView {
            items,
            selected: state.selected,
            offset: state.offset,
            rows,
            limit: FOOTER_MENU_ROWS,
            empty: None,
            border: true,
            padding_left: 1,
            padding_right: 0,
            grouped: false,
            compact: false,
            background: false,
            header_color: None,
            mono: false,
            group_offset: 0,
        }
    }

    fn grouped_rows(&self) -> Vec<Row<'a>> {
        let mut all = Vec::new();
        let mut category = "";
        for (index, item) in self.items.iter().enumerate() {
            if let Some(c) = item.category.as_deref()
                && c != category
            {
                if !all.is_empty() && !self.compact {
                    all.push(Row::Spacer);
                }
                category = c;
                if !self.compact {
                    all.push(Row::Header(c));
                }
            }
            all.push(Row::Item(item, index));
        }
        all
    }

    /// Update the grouped scroll offset like the `createEffect` in `RunFooterMenu`.
    pub fn next_group_offset(&self, previous_selected: i64) -> i64 {
        let all = self.grouped_rows();
        let limit = self.rows.min(self.limit).max(1) as i64;
        let Some(selected) = all.iter().position(|r| matches!(r, Row::Item(_, i) if *i as i64 == self.selected)) else { return 0 };
        let dir = if self.selected == previous_selected + 1 {
            Some(1)
        } else if self.selected == previous_selected - 1 {
            Some(-1)
        } else {
            None
        };
        match dir {
            Some(d) => move_selection_offset(self.group_offset, all.len() as i64, limit, selected as i64, d),
            None => reveal_selection_offset(self.group_offset, all.len() as i64, limit, selected as i64),
        }
    }

    /// Draw at row `y` of `buf` (full buffer width).
    pub fn render(&self, buf: &mut Buffer, y: usize, term_width: usize, theme: &FooterTheme) {
        let width_total = buf.width as usize;
        let limit = self.rows.min(self.limit).max(1);
        let pl = if term_width < FOOTER_COMPACT_WIDTH { self.padding_left.min(1) } else { self.padding_left };
        let pr = if term_width < FOOTER_COMPACT_WIDTH { self.padding_right.min(1) } else { self.padding_right };
        let width = term_width.saturating_sub(usize::from(self.border) + pl + pr);
        let shade = |active: bool| if active { theme.action_focused_bg } else if self.background { theme.shade } else { Color::TRANSPARENT };
        if self.background {
            buf.fill_rect(0, y as i32, width_total as i32, self.rows as i32, theme.shade);
        }
        let rows: Vec<Row> = if !self.grouped {
            self.items.iter().enumerate().skip(self.offset.max(0) as usize).take(limit).map(|(i, it)| Row::Item(it, i)).collect()
        } else {
            let all = self.grouped_rows();
            let start = (self.group_offset.min(all.len() as i64 - limit as i64)).max(0) as usize;
            all.into_iter().skip(start).take(limit).collect()
        };
        if rows.is_empty() {
            let mut x = 0;
            if self.border {
                buf.draw_chunks(0, y as i32, &[StyledChunk::plain(if self.mono { "|" } else { "┃" }, theme.border)], 1);
                x = 1;
            }
            let avail = width_total.saturating_sub(x + pl + pr);
            let text = locale::truncate_width(self.empty.unwrap_or("No matching items"), avail);
            buf.draw_chunks((x + pl) as i32, y as i32, &[StyledChunk::plain(text, theme.muted)], avail);
            return;
        }
        let desc_col = {
            let w = self.items.iter().filter(|i| i.description.is_some()).map(|i| sw(&i.display)).max().unwrap_or(0);
            if w == 0 { 0 } else { w + 2 }
        };
        for (r, row) in rows.iter().enumerate() {
            let ry = (y + r) as i32;
            match row {
                Row::Spacer => {}
                Row::Header(label) => {
                    let avail = width_total.saturating_sub(pl + pr);
                    let lines = super::text::layout(&[StyledChunk { text: label.to_string(), fg: self.header_color.unwrap_or(theme.muted), bg: Color::TRANSPARENT, attrs: attr::BOLD }], avail.max(1), super::text::WrapMode::None, true);
                    if let Some(l) = lines.first() {
                        buf.draw_chunks(pl as i32, ry, &l.chunks, avail);
                    }
                }
                Row::Item(item, index) => {
                    let active = *index as i64 == self.selected;
                    let bg = shade(active);
                    buf.fill_rect(0, ry, width_total as i32, 1, bg);
                    let available = width.saturating_sub(if item.icon.is_some() { 2 } else { 0 });
                    let attrs = if active { attr::BOLD | if self.mono { attr::INVERSE } else { 0 } } else { 0 };
                    let mut x = 0usize;
                    if self.border {
                        buf.draw_chunks(0, ry, &[StyledChunk { text: if active { if self.mono { ">" } else { "▌" } } else { " " }.into(), fg: theme.action_focused_text, bg, attrs: 0 }], 1);
                        x = 1;
                    }
                    x += pl;
                    let footer = item.footer.as_ref().filter(|f| {
                        let title = sw(&item.display);
                        let primary = item.footer_tone.is_some() && !(item.current && item.footer_tone == Some(FooterTone::Selection));
                        (if primary { title.min(if item.icon.is_some() { 4 } else { 8 }) } else { title }) + 1 + sw(f) <= available
                    });
                    let footer_w = footer.map(|f| sw(f) + 1).unwrap_or(0);
                    let fg = if active { theme.action_focused_text } else { theme.formfield_text };
                    if let Some((glyph, color)) = &item.icon {
                        let c = color.unwrap_or(fg);
                        buf.draw_chunks(x as i32, ry, &[StyledChunk { text: glyph.clone(), fg: if active { theme.action_focused_text } else { c }, bg, attrs: 0 }], 2);
                        x += 2;
                    }
                    let title = footer_menu_text(&item.display, available.saturating_sub(footer_w), self.mono);
                    let tw = sw(&title);
                    buf.draw_chunks(x as i32, ry, &[StyledChunk { text: title, fg, bg, attrs }], available);
                    let mut cx = x + tw;
                    let description = item.description.as_ref().and_then(|d| {
                        let remaining = available as i64 - desc_col as i64 - footer_w as i64;
                        if remaining < 12.min(sw(d)) as i64 {
                            return None;
                        }
                        Some(footer_menu_text(d, remaining.max(0) as usize, self.mono))
                    });
                    if let Some(d) = description {
                        let dfg = if active { theme.action_focused_text } else { theme.muted };
                        let pad = desc_col.saturating_sub(sw(&item.display)).max(1);
                        buf.draw_chunks(cx as i32, ry, &[StyledChunk { text: " ".repeat(pad), fg: dfg, bg, attrs: 0 }], pad);
                        cx += pad;
                        let room = (x + available).saturating_sub(cx).saturating_sub(footer_w);
                        buf.draw_chunks(cx as i32, ry, &[StyledChunk { text: d, fg: dfg, bg, attrs: 0 }], room);
                    }
                    if let Some(f) = footer {
                        let ffg = if active {
                            theme.action_focused_text
                        } else {
                            match item.footer_tone {
                                Some(FooterTone::Selection) => theme.selection,
                                Some(FooterTone::Running) => theme.running,
                                Some(FooterTone::Error) => theme.error,
                                Some(FooterTone::Success) => theme.success,
                                None => theme.muted,
                            }
                        };
                        let fx = term_width.saturating_sub(pr + sw(f));
                        buf.draw_chunks(fx as i32, ry, &[StyledChunk { text: f.clone(), fg: ffg, bg, attrs }], sw(f));
                    }
                }
            }
        }
    }
}
