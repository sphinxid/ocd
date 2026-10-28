//! Footer panels (`src/mini/footer.command.tsx`): the command palette and the agent, model,
//! variant, settings, subagent and pending-prompt pickers, all built on the searchable panel
//! controller and `PanelShell`.

use super::cell::{Buffer, Color, StyledChunk, attr};
use super::fuzzysort;
use super::menu::{FOOTER_COMPACT_WIDTH, FooterTone, MenuItem, MenuState, MenuView, footer_menu_text};
use super::motion;
use super::textarea::TextArea;
use super::theme::FooterTheme;
use super::types::*;
use super::verbosity::{WORK_SPINNER_NAMES, match_mini_verbosity, verbosity_change, verbosity_label};
use crate::util::locale::{self, width as sw};

const PANEL_PAD: usize = 2;
pub const PANEL_LIST_ROWS: usize = 10;
pub const PANEL_FRAME_ROWS: usize = 6;
pub const RUN_COMMAND_PANEL_ROWS: usize = PANEL_LIST_ROWS + PANEL_FRAME_ROWS;
pub const SUBAGENT_LIST_ROWS: usize = 12;
pub const RUN_SUBAGENT_PANEL_ROWS: usize = SUBAGENT_LIST_ROWS + PANEL_FRAME_ROWS;

fn panel_pad(mono: bool) -> usize {
    if mono { 1 } else { PANEL_PAD }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanelLayout {
    pub compact: bool,
    pub frame: usize,
    pub limit: usize,
}

/// `footerPanelLayout(height, limit)`.
pub fn footer_panel_layout(height: usize, limit: usize) -> PanelLayout {
    let available = height.saturating_sub(1).max(3);
    let compact = available < limit + PANEL_FRAME_ROWS;
    let frame = if compact { 2 } else { PANEL_FRAME_ROWS };
    PanelLayout { compact, frame, limit: 1.max(limit.min(available.saturating_sub(frame))) }
}

fn count_label(count: usize, total: usize, query: &str) -> String {
    if query.trim().is_empty() { format!("{total}") } else { format!("{count}/{total}") }
}

fn subagent_status_label(s: SubagentStatus) -> &'static str {
    match s {
        SubagentStatus::Completed => "done",
        SubagentStatus::Cancelled => "cancelled",
        SubagentStatus::Error => "error",
        SubagentStatus::Running => "running",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelKind {
    Command,
    Agent,
    Model,
    Variant,
    Settings,
    Subagent,
    Queued,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PanelAction {
    Agent,
    Model,
    Editor,
    Queued,
    Subagent,
    Status,
    VariantCycle,
    VariantList,
    Settings,
    Slash(String),
    Clear,
    Exit,
    PickAgent(String),
    PickModel(ModelSel),
    PickVariant(Option<String>),
    Setting(&'static str),
    PickSubagent(String),
    PickQueued(QueuedPrompt),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PanelEntry {
    pub item: MenuItem,
    pub keywords: String,
    pub action: PanelAction,
    pub current: bool,
    pub provider_id: String,
    pub provider_name: String,
}

impl PanelEntry {
    fn new(category: &str, display: &str, action: PanelAction) -> PanelEntry {
        PanelEntry { item: MenuItem { display: display.into(), category: Some(category.into()), ..Default::default() }, keywords: String::new(), action, current: false, provider_id: String::new(), provider_name: String::new() }
    }
    fn footer(mut self, f: Option<String>) -> PanelEntry {
        self.item.footer = f;
        self
    }
    fn keywords(mut self, k: impl Into<String>) -> PanelEntry {
        self.keywords = k.into();
        self
    }
}

/// Data the panels read from the footer.
pub struct PanelData {
    pub subagents: Vec<SubagentTab>,
    pub queued: Vec<QueuedPrompt>,
    pub variants: Vec<String>,
    pub variant_cycle: String,
    pub clear_shortcut: String,
    pub agents: Vec<RunAgent>,
    pub current_agent: Option<String>,
    pub providers: Option<Vec<RunProvider>>,
    pub current_model: Option<ModelSel>,
    pub current_variant: Option<String>,
    pub settings: MiniSettings,
    pub current_subagent: Option<String>,
    pub undo_shortcut: String,
    pub delete_shortcut: String,
}

/// What a panel key produced.
#[derive(Clone, Debug, PartialEq)]
pub enum PanelOutcome {
    None,
    Close,
    Select(PanelAction),
    /// Settings left/right change (`direction`).
    Change(PanelAction, i32),
    Undo(QueuedPrompt),
    Delete(QueuedPrompt),
}

pub struct Panel {
    pub kind: PanelKind,
    pub input: TextArea,
    pub menu: MenuState,
    group_offset: i64,
    prev_selected: i64,
    /// Subagent picker: show running (true) or finished tabs.
    pub active: bool,
    /// Settings: the key being saved.
    pub saving: Option<&'static str>,
    revealed: bool,
}

impl Panel {
    pub fn new(kind: PanelKind) -> Panel {
        Panel { kind, input: TextArea::new(), menu: MenuState::default(), group_offset: 0, prev_selected: -1, active: true, saving: None, revealed: false }
    }

    pub fn query(&self) -> String {
        self.input.plain_text().to_string()
    }

    fn list_limit(&self) -> usize {
        match self.kind {
            PanelKind::Subagent | PanelKind::Queued => SUBAGENT_LIST_ROWS,
            _ => PANEL_LIST_ROWS,
        }
    }

    pub fn layout(&self, term_height: usize) -> PanelLayout {
        footer_panel_layout(term_height, self.list_limit())
    }

    pub fn entries(&self, d: &PanelData) -> Vec<PanelEntry> {
        match self.kind {
            PanelKind::Command => {
                let active = d.subagents.iter().filter(|s| s.status == SubagentStatus::Running).count();
                let mut v = vec![
                    PanelEntry::new("Session", "Open editor", PanelAction::Editor).footer(Some("/editor".into())).keywords("editor compose draft external editor"),
                    PanelEntry::new("Session", "Show status", PanelAction::Status).keywords("status activity model context usage footer"),
                ];
                if !d.subagents.is_empty() {
                    v.push(
                        PanelEntry::new("Session", "View subagents", PanelAction::Subagent)
                            .footer(Some(if active > 0 { format!("{active} active") } else { format!("{} recent", d.subagents.len()) }))
                            .keywords(d.subagents.iter().map(|s| format!("{} {} {}", s.label, s.description, s.title.clone().unwrap_or_default())).collect::<Vec<_>>().join(" ")),
                    );
                }
                v.push(PanelEntry::new("Session", "Compact session", PanelAction::Slash("compact".into())).footer(Some("/compact".into())).keywords("compact session context"));
                v.push(PanelEntry::new("Session", "New session", PanelAction::Slash("new".into())).footer(Some("/new".into())).keywords("new session clear"));
                v.push(PanelEntry::new("Agent", "Switch agent", PanelAction::Agent));
                v.push(PanelEntry::new("Agent", "Switch model", PanelAction::Model));
                if !d.queued.is_empty() {
                    v.push(
                        PanelEntry::new("Agent", "View pending prompts", PanelAction::Queued)
                            .footer(Some(format!("{} pending", d.queued.len())))
                            .keywords(d.queued.iter().map(|q| q.prompt.text.clone()).collect::<Vec<_>>().join(" ")),
                    );
                }
                v.push(PanelEntry::new("Agent", "Variant cycle", PanelAction::VariantCycle).footer(Some(d.variant_cycle.clone()).filter(|s| !s.is_empty())).keywords("variant cycle"));
                if !d.variants.is_empty() {
                    v.push(PanelEntry::new("Agent", "Switch model variant", PanelAction::VariantList).keywords(format!("variant variants {}", d.variants.join(" "))));
                }
                v.push(PanelEntry::new("System", "Clear screen", PanelAction::Clear).footer(Some(d.clear_shortcut.clone()).filter(|s| !s.is_empty())).keywords("clear screen cls redraw"));
                v.push(PanelEntry::new("System", "Settings", PanelAction::Settings).footer(Some("/settings".into())).keywords("/settings settings preferences configuration"));
                v.push(PanelEntry::new("System", "Exit", PanelAction::Exit).footer(Some("/exit".into())).keywords("/exit exit"));
                v
            }
            PanelKind::Agent => d
                .agents
                .iter()
                .filter(|a| a.mode != "subagent" && !a.hidden)
                .map(|a| {
                    let current = d.current_agent.as_deref() == Some(a.id.as_str());
                    let mut e = PanelEntry::new("", &a.id, PanelAction::PickAgent(a.id.clone())).footer(current.then(|| "current".into())).keywords(format!("{} {} {}", a.id, a.name, a.description.clone().unwrap_or_default()));
                    e.item.description = a.description.clone();
                    e.item.footer_tone = Some(FooterTone::Selection);
                    e.item.current = current;
                    e.current = current;
                    e
                })
                .collect(),
            PanelKind::Variant => {
                let mut v = Vec::new();
                let current = d.current_variant.is_none();
                let mut e = PanelEntry::new("", "Default", PanelAction::PickVariant(None)).footer(current.then(|| "current".into())).keywords("default");
                e.item.footer_tone = Some(FooterTone::Selection);
                e.item.current = current;
                e.current = current;
                v.push(e);
                for variant in &d.variants {
                    let current = d.current_variant.as_deref() == Some(variant.as_str());
                    let mut e = PanelEntry::new("", variant, PanelAction::PickVariant(Some(variant.clone()))).footer(current.then(|| "current".into())).keywords(variant.clone());
                    e.item.footer_tone = Some(FooterTone::Selection);
                    e.item.current = current;
                    e.current = current;
                    v.push(e);
                }
                v
            }
            PanelKind::Settings => {
                let s = &d.settings;
                let show = |b: bool| if b { "show" } else { "hide" }.to_string();
                let row = |cat: &str, display: &str, key: &'static str, value: String, keywords: String| {
                    let saving = self.saving == Some(key);
                    let mut e = PanelEntry::new(cat, display, PanelAction::Setting(key)).footer(Some(if saving { "saving".into() } else { value })).keywords(keywords);
                    e.item.footer_tone = Some(if saving { FooterTone::Running } else { FooterTone::Selection });
                    e
                };
                let level = verbosity_label(match_mini_verbosity(s));
                let mut v = vec![
                    row("Transcript", "Verbosity", "verbosity", level.into(), format!("verbosity quiet default everything custom noise {level}")),
                    row("Transcript", "Thinking", "thinking", show(s.thinking), format!("thinking reasoning {}", show(s.thinking))),
                    row("Transcript", "Tools", "tools", show(s.tools), format!("tools files skills activity work steps intermediate {}", show(s.tools))),
                    row("Transcript", "Shell", "shell_output", show(s.shell_output), format!("shell tool command output {}", show(s.shell_output))),
                    row("Transcript", "Turn summary", "turn_summary", show(s.turn_summary), format!("turn summary agent model duration {}", show(s.turn_summary))),
                    row("Terminal", "Footer details", "footer", show(s.footer), format!("footer status activity model context usage {}", show(s.footer))),
                    row("Terminal", "Splash", "splash", show(s.splash), format!("splash entry exit banner {}", show(s.splash))),
                    row("Terminal", "Monochrome UI", "mono", if s.mono { "on" } else { "off" }.into(), format!("mono monochrome ascii legacy compat terminal {}", if s.mono { "on" } else { "off" })),
                    row("Terminal", "Work spinner", "work_spinner", s.work_spinner.trim_start_matches("block-").replace('-', " "), format!("work spinner animation {}", s.work_spinner)),
                ];
                let spinner = if s.mono { motion::seed_mono() } else { motion::work_spinner(&s.work_spinner) };
                v[8].item.icon = Some((if s.mono { "*".into() } else { spinner.frames[0].clone() }, None));
                v
            }
            PanelKind::Subagent => d
                .subagents
                .iter()
                .filter(|t| if self.active { t.status == SubagentStatus::Running } else { t.status != SubagentStatus::Running })
                .map(|t| {
                    let title = if !t.description.is_empty() { t.description.clone() } else { t.title.clone().filter(|s| !s.is_empty()).unwrap_or(t.label.clone()) };
                    let current = d.current_subagent.as_deref() == Some(t.session_id.as_str());
                    let mut e = PanelEntry::new("", &title, PanelAction::PickSubagent(t.session_id.clone()))
                        .footer(Some(subagent_status_label(t.status).into()))
                        .keywords(format!("{} {} {} {}", t.label, t.description, t.title.clone().unwrap_or_default(), match t.status {
                            SubagentStatus::Running => "running",
                            SubagentStatus::Completed => "completed",
                            SubagentStatus::Cancelled => "cancelled",
                            SubagentStatus::Error => "error",
                        }));
                    e.item.description = (title != t.label).then(|| t.label.clone());
                    e.item.footer_tone = match t.status {
                        SubagentStatus::Running => Some(FooterTone::Running),
                        SubagentStatus::Error => Some(FooterTone::Error),
                        SubagentStatus::Completed => Some(FooterTone::Success),
                        SubagentStatus::Cancelled => None,
                    };
                    e.current = current;
                    e
                })
                .collect(),
            PanelKind::Queued => d
                .queued
                .iter()
                .map(|q| PanelEntry::new("", &q.prompt.text.replace('\n', " "), PanelAction::PickQueued(q.clone())).footer(Some(if q.delivery == Delivery::Queue { "queued" } else { "steering" }.into())).keywords(q.prompt.text.clone()))
                .collect(),
            PanelKind::Model => {
                let mut v: Vec<PanelEntry> = Vec::new();
                for p in d.providers.as_deref().unwrap_or(&[]) {
                    for (model_id, m) in &p.models {
                        if m.status.as_deref() == Some("deprecated") {
                            continue;
                        }
                        let title = m.name.clone().unwrap_or(model_id.clone());
                        let current = d.current_model.as_ref().is_some_and(|c| c.provider_id == p.id && &c.model_id == model_id);
                        let footer = if current {
                            Some("current".to_string())
                        } else if m.cost_input == Some(0.0) && p.id == "opencode" {
                            Some("Free".into())
                        } else if &title != model_id {
                            Some(model_id.clone())
                        } else {
                            None
                        };
                        let mut e = PanelEntry::new(&p.name, &title, PanelAction::PickModel(ModelSel { provider_id: p.id.clone(), model_id: model_id.clone() }))
                            .footer(footer.clone())
                            .keywords(format!("{} {} {} {} {}", p.id, p.name, model_id, title, footer.unwrap_or_default()));
                        e.item.footer_tone = current.then_some(FooterTone::Selection);
                        e.item.current = current;
                        e.current = current;
                        e.provider_id = p.id.clone();
                        e.provider_name = p.name.clone();
                        v.push(e);
                    }
                }
                v.sort_by(|a, b| {
                    let pa = (a.provider_id != "opencode") as i32 - (b.provider_id != "opencode") as i32;
                    if pa != 0 {
                        return pa.cmp(&0);
                    }
                    let n = locale::compare(&a.provider_name, &b.provider_name);
                    if n != std::cmp::Ordering::Equal {
                        return n;
                    }
                    locale::compare(&a.item.display, &b.item.display)
                });
                v
            }
        }
    }

    /// `match(query, entries)`.
    pub fn items(&self, d: &PanelData) -> Vec<PanelEntry> {
        let entries = self.entries(d);
        let q = self.query();
        let text = q.trim();
        if text.is_empty() {
            return entries;
        }
        fuzzysort::go(text, &entries, |e| vec![Some(e.item.display.clone()), e.item.category.clone(), e.item.description.clone(), Some(e.keywords.clone()).filter(|k| !k.is_empty())]).into_iter().map(|i| entries[i].clone()).collect()
    }

    /// Reconcile the selection with the item count (the controller's effects); reveal the current
    /// item once when the query is empty.
    pub fn sync(&mut self, d: &PanelData, term_height: usize) {
        let items = self.items(d);
        let limit = self.layout(term_height).limit as i64;
        self.menu.reconcile(items.len() as i64, limit);
        if !self.revealed && self.query().trim().is_empty() {
            if let Some(i) = items.iter().position(|e| e.current) {
                self.menu.reveal(i as i64, items.len() as i64, limit);
            }
            self.revealed = true;
        }
    }

    /// Rows the panel reports (`menu.rows() + frame`) for the subagent/queued routes.
    pub fn reported_rows(&self, d: &PanelData, term_height: usize) -> usize {
        let l = self.layout(term_height);
        self.menu.rows(self.items(d).len(), l.limit) + l.frame
    }

    /// Key handling (`useKeyboard` in the controller plus the input field). `name`/`ctrl` from the stroke.
    pub fn key(&mut self, d: &PanelData, term_height: usize, s: &crate::keymap::Stroke) -> PanelOutcome {
        let items = self.items(d);
        let limit = self.layout(term_height).limit as i64;
        let count = items.len() as i64;
        let selected = items.get(self.menu.selected as usize).cloned();
        let name = s.name.to_lowercase();
        let ctrl = s.ctrl && !s.meta && !s.shift && !s.super_;
        if self.kind == PanelKind::Settings && (name == "left" || name == "right") && !s.ctrl && !s.meta {
            if let Some(item) = selected {
                return PanelOutcome::Change(item.action, if name == "left" { -1 } else { 1 });
            }
            return PanelOutcome::None;
        }
        if self.kind == PanelKind::Subagent && name == "tab" {
            self.active = !self.active;
            self.menu.reset();
            return PanelOutcome::None;
        }
        if self.kind == PanelKind::Subagent && name == "up" && self.menu.selected == 0 {
            return PanelOutcome::Close;
        }
        if name == "escape" || (ctrl && name == "c") {
            return PanelOutcome::Close;
        }
        if name == "up" || (ctrl && name == "p") {
            self.menu.move_by(-1, count, limit);
            return PanelOutcome::None;
        }
        if name == "down" || (ctrl && name == "n") {
            self.menu.move_by(1, count, limit);
            return PanelOutcome::None;
        }
        if name == "pageup" {
            self.menu.reveal(self.menu.selected - (limit - 1).max(1), count, limit);
            return PanelOutcome::None;
        }
        if name == "pagedown" {
            self.menu.reveal(self.menu.selected + (limit - 1).max(1), count, limit);
            return PanelOutcome::None;
        }
        if name == "home" {
            self.menu.reveal(0, count, limit);
            return PanelOutcome::None;
        }
        if name == "end" {
            self.menu.reveal(i64::MAX / 2, count, limit);
            return PanelOutcome::None;
        }
        if name == "return" {
            return match selected {
                Some(item) if self.kind == PanelKind::Settings => PanelOutcome::Change(item.action, 1),
                Some(item) => PanelOutcome::Select(item.action),
                None => PanelOutcome::None,
            };
        }
        if ctrl && name == "u" {
            self.input.set_text("");
            self.menu.reset();
            return PanelOutcome::None;
        }
        // The search input.
        let before = self.query();
        if let Some(c) = s.is_char() {
            self.input.insert_text(&c.to_string());
        } else if name == "backspace" {
            self.input.delete_char_backward();
        } else if name == "delete" || (ctrl && name == "d") {
            self.input.delete_char();
        } else if name == "left" || (ctrl && name == "b") {
            self.input.move_left();
        } else if name == "right" || (ctrl && name == "f") {
            self.input.move_right();
        } else if ctrl && name == "a" {
            self.input.buffer_home();
        } else if ctrl && name == "e" {
            self.input.buffer_end();
        } else if ctrl && name == "w" {
            self.input.delete_word_backward();
        } else if ctrl && name == "k" {
            self.input.delete_to_line_end();
        }
        if self.query() != before {
            self.menu.reset();
        }
        PanelOutcome::None
    }

    pub fn paste(&mut self, text: &str) {
        let line = text.replace(['\r', '\n'], " ");
        self.input.insert_text(&line);
        self.menu.reset();
    }

    /// `PanelShell` + `RunFooterMenu` at row `y`. Returns the input cursor (x, y).
    #[allow(clippy::too_many_arguments)]
    pub fn render(&mut self, buf: &mut Buffer, y: usize, width: usize, term_height: usize, theme: &FooterTheme, d: &PanelData, mono: bool) -> (usize, usize) {
        let layout = self.layout(term_height);
        let items = self.items(d);
        let total = self.entries(d).len();
        let query = self.query();
        let (title, count_visible, hint, background, empty) = match self.kind {
            PanelKind::Command => ("Commands", false, None, true, "No results found"),
            PanelKind::Agent => ("Select agent", true, None, true, "No agents found"),
            PanelKind::Variant => ("Select variant", true, None, true, "No results found"),
            PanelKind::Settings => ("Settings", false, Some("left/right change".to_string()), true, "No settings found"),
            PanelKind::Subagent => ("Select subagent", true, Some(format!("tab show {}", if self.active { "inactive" } else { "active" })), true, "No subagents found"),
            PanelKind::Queued => {
                let enter = if items.get(self.menu.selected as usize).is_some_and(|e| matches!(&e.action, PanelAction::PickQueued(q) if q.delivery == Delivery::Steer)) { "enter queue" } else { "enter steer" };
                let mut parts = vec![enter.to_string()];
                if !d.delete_shortcut.is_empty() {
                    parts.push(format!("{} delete", d.delete_shortcut));
                }
                if !d.undo_shortcut.is_empty() {
                    parts.push(format!("{} undo", d.undo_shortcut));
                }
                ("Pending prompts", true, Some(parts.join(" · ")), true, "No pending prompts")
            }
            PanelKind::Model => ("Select model", true, None, false, if d.providers.is_some() { "No results found" } else { "Models loading" }),
        };
        let pad = if width < FOOTER_COMPACT_WIDTH { 1 } else { panel_pad(mono) };
        let bg = if background { theme.shade } else { Color::TRANSPARENT };
        let mut row = y;
        let fill = |buf: &mut Buffer, r: usize| buf.fill_rect(0, r as i32, width as i32, 1, bg);
        if !layout.compact {
            fill(buf, row);
            row += 1;
        }
        // Header.
        fill(buf, row);
        let hw = width.saturating_sub(pad * 2 + 4);
        let title_text = footer_menu_text(title, hw, mono);
        let count = count_label(items.len(), total, &query);
        let show_count = count_visible && sw(title) + sw(&count) < hw;
        let hint = hint.filter(|h| sw(title) + if show_count { sw(&count) + 1 } else { 0 } + sw(h) + 3 <= hw);
        let mut x = pad;
        buf.draw_chunks(x as i32, row as i32, &[StyledChunk { text: title_text.clone(), fg: theme.text, bg: Color::TRANSPARENT, attrs: attr::BOLD }], width.saturating_sub(x));
        x += sw(&title_text);
        if show_count {
            let c = format!(" {count}");
            buf.draw_chunks(x as i32, row as i32, &[StyledChunk::plain(c.clone(), theme.muted)], width.saturating_sub(x));
        }
        let right = format!("{}esc", hint.map(|h| format!("{h} {} ", if mono { "-" } else { "·" })).unwrap_or_default());
        let rx = width.saturating_sub(pad + sw(&right));
        buf.draw_chunks(rx as i32, row as i32, &[StyledChunk::plain(right, theme.muted)], width.saturating_sub(rx));
        row += 1;
        if !layout.compact {
            fill(buf, row);
            row += 1;
        }
        // Search input.
        fill(buf, row);
        let iw = width.saturating_sub(pad * 2).max(1);
        let field_bg = if background { theme.formfield_focused_bg } else { Color::TRANSPARENT };
        let field_fg = if background { theme.formfield_focused_text } else { theme.formfield_text };
        buf.fill_rect(pad as i32, row as i32, iw as i32, 1, field_bg);
        let cursor_col = self.input.cursor_offset();
        let scroll = if cursor_col >= iw { cursor_col + 1 - iw } else { 0 };
        if query.is_empty() {
            buf.draw_chunks(pad as i32, row as i32, &[StyledChunk { text: "Search".into(), fg: theme.muted, bg: field_bg, attrs: 0 }], iw);
        } else {
            let visible = locale::take_width(&super::util::skip_width(&query, scroll), iw);
            buf.draw_chunks(pad as i32, row as i32, &[StyledChunk { text: visible, fg: field_fg, bg: field_bg, attrs: 0 }], iw);
        }
        let cursor = (pad + cursor_col - scroll, row);
        row += 1;
        if !layout.compact {
            fill(buf, row);
            row += 1;
        }
        // Menu.
        let menu_items: Vec<MenuItem> = if self.kind == PanelKind::Model && (!query.trim().is_empty() || (layout.compact && items.iter().map(|i| &i.provider_id).collect::<std::collections::HashSet<_>>().len() > 1)) {
            items.iter().map(|e| MenuItem { footer: Some(e.provider_name.clone()), footer_tone: None, ..e.item.clone() }).collect()
        } else {
            items.iter().map(|e| e.item.clone()).collect()
        };
        let rows = match self.kind {
            PanelKind::Subagent | PanelKind::Queued => self.menu.rows(items.len(), layout.limit),
            _ => layout.limit,
        };
        let grouped = matches!(self.kind, PanelKind::Command | PanelKind::Settings | PanelKind::Model) && query.trim().is_empty();
        let mut view = MenuView::new(&menu_items, self.menu, rows);
        view.limit = layout.limit;
        view.compact = layout.compact;
        view.empty = Some(empty);
        view.border = false;
        view.padding_left = panel_pad(mono);
        view.padding_right = panel_pad(mono);
        view.grouped = grouped;
        view.background = background;
        view.header_color = Some(theme.muted);
        view.mono = mono;
        view.group_offset = self.group_offset;
        if grouped {
            self.group_offset = view.next_group_offset(self.prev_selected);
            view.group_offset = self.group_offset;
        }
        self.prev_selected = self.menu.selected;
        view.render(buf, row, width, theme);
        row += rows;
        if !layout.compact && !mono && background {
            buf.draw_chunks(0, row as i32, &[StyledChunk::plain("▀".repeat(width), theme.shade)], width);
        }
        cursor
    }
}

/// Settings change for `key` in `direction` (`change(item, direction)`).
pub fn setting_change(key: &str, settings: &MiniSettings, direction: i32) -> Option<MiniSettingChange> {
    match key {
        "verbosity" => verbosity_change(settings, if direction < 0 { -1 } else { 1 }),
        "work_spinner" => {
            let n = WORK_SPINNER_NAMES.len() as i32;
            let i = WORK_SPINNER_NAMES.iter().position(|s| *s == settings.work_spinner).map(|i| i as i32).unwrap_or(-1);
            Some(MiniSettingChange::WorkSpinner(WORK_SPINNER_NAMES[((i + direction + n) % n) as usize].to_string()))
        }
        "mono" => Some(MiniSettingChange::Mono(!settings.mono)),
        "thinking" => Some(MiniSettingChange::Thinking(!settings.thinking)),
        "tools" => Some(MiniSettingChange::Tools(!settings.tools)),
        "shell_output" => Some(MiniSettingChange::ShellOutput(!settings.shell_output)),
        "turn_summary" => Some(MiniSettingChange::TurnSummary(!settings.turn_summary)),
        "footer" => Some(MiniSettingChange::Footer(!settings.footer)),
        "splash" => Some(MiniSettingChange::Splash(!settings.splash)),
        _ => None,
    }
}
