//! Screen rendering: root layout, home, session view, prompt box, sidebar, tabs, toasts,
//! permission/form prompts, autocomplete popup and overlays.

use super::{App, Route};
use crate::api::sse::ConnStatus;
use crate::api::types::*;
use crate::theme::{Rgba, Theme, tint};
use crate::ui::dialog;
use crate::ui::form as F;
use crate::ui::prompt::ChipKind;
use crate::ui::text::*;
use crate::ui::transcript::{self, RCtx};
use crate::util::locale::{self, width as sw};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use std::collections::{HashMap, HashSet};

fn pad(n: usize) -> Seg {
    Seg::new(" ".repeat(n), Style::default())
}

fn s(text: impl Into<String>, c: Rgba) -> Seg {
    Seg::new(text, fg(c))
}

impl App {
    /// Render the whole UI.
    pub fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        if self.dirty || (area.width, area.height) != self.size {
            self.render_rev += 1;
        }
        self.size = (area.width, area.height);
        self.hits.clear();
        self.dialog_hits.clear();
        self.dialog_rect = None;
        let t = self.theme().clone();
        let buf = f.buffer_mut();
        fill(buf, area, t.background.base.tui_or_reset());
        let mut cursor: Option<(u16, u16)> = None;
        let main = self.draw_tabs_frame(buf, area, &t);
        match self.route.clone() {
            Route::Home => self.draw_home(buf, main, &t, &mut cursor),
            Route::Session(id) => self.draw_session(buf, main, &t, &id, &mut cursor),
            Route::Plugin(name) => self.draw_plugin(buf, area, &t, &name),
        }
        self.draw_tab_menu(buf, area, &t);
        self.draw_toast(buf, area, &t);
        if let Some(mut d) = self.dialogs.pop() {
            let kb = self.keybinds.clone();
            let (rect, hits, c) = dialog::render(buf, area, &mut d, &t, &kb);
            self.dialogs.push(d);
            self.dialog_hits = hits;
            self.dialog_rect = Some(rect);
            cursor = c;
        }
        self.draw_reconnecting(buf, area, &t);
        let spinner = self.spinner_frame();
        replace_spinners(buf, spinner);
        self.last_frame = Some(buf.clone());
        self.draw_selection(buf);
        if let Some((x, y)) = cursor {
            f.set_cursor_position((x, y));
        }
    }

    fn draw_plugin(&mut self, buf: &mut Buffer, area: Rect, t: &Theme, name: &str) {
        match name {
            "diff" => self.draw_diff(buf, area, t),
            "stats" => self.draw_stats(buf, area, t),
            _ => {
                let msg = format!("Unknown plugin route: {name}");
                let y = area.y + area.height / 2;
                let x = area.x + area.width.saturating_sub(sw(&msg) as u16) / 2;
                put(buf, x, y.saturating_sub(1), &msg, fg(t.text.feedback.warning.base), sw(&msg));
                let go = " go home ";
                let gx = area.x + area.width.saturating_sub(9) / 2;
                put(buf, gx, y + 1, go, fg(t.text.action.primary.hovered).bg(t.background.action.primary.hovered.tui()), 9);
                self.hits.push((Rect::new(gx, y + 1, 9, 1), Click::Custom("session.new".into())));
            }
        }
    }

    // ---- home --------------------------------------------------------------

    fn draw_home(&mut self, buf: &mut Buffer, area: Rect, t: &Theme, cursor: &mut Option<(u16, u16)>) {
        let w = area.width;
        let h = area.height;
        let padx = if w < 44 { 1 } else { 2 };
        let footer_visible = h >= 14 && w >= 44;
        let footer_h = if footer_visible { if h < 16 { 1 } else { 3 } } else { 0 };
        let logo = crate::ui::view_helpers::logo(t, w, h);
        let prompt_w = (w.saturating_sub(padx * 2)).min(75);
        let prompt_lines = self.prompt_lines(t, prompt_w as usize);
        let prompt_h = prompt_lines.0.len() as u16;
        let body_h = h.saturating_sub(footer_h);
        let fixed = logo.len() as u16 + 1 + 1 + prompt_h;
        let spacer3 = 3.min(body_h.saturating_sub(fixed));
        let free = body_h.saturating_sub(fixed + spacer3);
        let top = free / 2;
        let mut y = area.y + top + spacer3;
        let logo_w = logo.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
        let lx = area.x + (w.saturating_sub(logo_w)) / 2;
        for l in &logo {
            draw_lines(buf, Rect::new(lx, y, logo_w.min(w), 1), std::slice::from_ref(l), 0, "");
            y += 1;
        }
        y += 1; // spacer
        y += 1; // prompt paddingTop
        let px = area.x + (w.saturating_sub(prompt_w)) / 2;
        let prect = Rect::new(px, y.min(area.bottom().saturating_sub(prompt_h)), prompt_w, prompt_h.min(area.height));
        self.render_prompt(buf, prect, t, prompt_lines, cursor);
        // Home footer.
        if footer_visible {
            let fy = area.bottom() - footer_h + if h < 16 { 0 } else { 1 };
            let mut left: Vec<Seg> = Vec::new();
            let servers = self.loc().and_then(|l| l.mcp.clone()).unwrap_or_default();
            if !servers.is_empty() {
                let failed = servers.iter().filter(|s| s.status.status == "failed").count();
                let connected = servers.iter().filter(|s| s.status.status == "connected").count();
                if failed > 0 {
                    left.push(s("⊙ ", t.text.feedback.error.base));
                    left.push(s(format!("{failed} MCP failed"), t.text.base));
                } else {
                    left.push(s("⊙ ", if connected > 0 { t.text.feedback.success.base } else { t.text.muted }));
                    left.push(s(format!("{} MCP", servers.len()), t.text.base));
                }
                if w >= 64 {
                    left.push(s(" /mcps", t.text.muted));
                }
            }
            let mut segs = vec![pad(2)];
            segs.extend(left);
            if w >= 64 {
                let version = self.opts.version.clone();
                let used: usize = segs.iter().map(|x| x.width()).sum();
                segs.push(pad((w as usize).saturating_sub(used + sw(&version) + 2)));
                segs.push(s(version, t.text.muted));
            }
            draw_lines(buf, Rect::new(area.x, fy, w, 1), &[SLine::new(segs).on_click(Some(Click::Custom("mcp.list".into())))], 0, "");
        }
        // Global form prompt overlay on home.
        if self.active_form().is_some() {
            let lines = self.form_lines(t, (w as usize).saturating_sub(4));
            let fh = (lines.len() as u16).min(h);
            let r = Rect::new(area.x + 2, area.bottom().saturating_sub(fh + 1), w.saturating_sub(4), fh);
            fill(buf, r, t.background.base.tui());
            draw_lines(buf, r, &lines, 0, "");
        }
    }

    // ---- prompt ------------------------------------------------------------

    fn highlight(&self, t: &Theme) -> Rgba {
        let leader = !self.pending_keys.is_empty();
        if leader {
            return t.border;
        }
        if self.editor.shell {
            return t.text.action.primary.selected;
        }
        self.current_agent().map(|a| self.agent_color(&a.id)).unwrap_or(t.border)
    }

    /// Prompt box lines (bar, textarea, metadata, cap, footer) and cursor offset.
    pub(super) fn prompt_lines(&self, t: &Theme, width: usize) -> (Vec<SLine>, Option<(u16, u16)>) {
        let term_w = self.size.0 as usize;
        let term_h = self.size.1 as usize;
        let prompt_bg = t.dec(t.background.raised.base);
        let bg = prompt_bg.tui();
        let hl = self.highlight(t);
        let padx = if term_w < 44 { 1 } else { 2 };
        let inner = width.saturating_sub(1 + padx * 2).max(1);
        let muted = !self.pending_keys.is_empty();
        let text_c = if muted { t.text.muted } else { t.text.base };
        let bar = Seg::new("┃", Style::default().fg(hl.tui()).bg(bg));
        let mut out = Vec::new();
        out.push(SLine::new(vec![bar.clone()]).with_bg(Some(bg)));
        let max_h = 6.max(term_h / 3);
        let syntax = &self.syntax;
        let chip_style = |k: ChipKind| -> Style {
            let scope = match k {
                ChipKind::File => "extmark.file",
                ChipKind::Agent => "extmark.agent",
                ChipKind::Skill => "extmark.skill",
                ChipKind::Paste => "extmark.paste",
            };
            crate::ui::highlight::token_style(syntax.get(scope))
        };
        let mut cursor = None;
        if self.editor.is_empty() {
            let ph = self.placeholder_text(width);
            let line = vec![bar.clone(), pad(padx), s(ph, t.text.muted)];
            out.push(SLine::new(line).with_bg(Some(bg)));
            cursor = Some(((1 + padx) as u16, 1));
        } else {
            let rows = self.editor.render(inner, fg(text_c), &chip_style);
            let (crow, ccol) = self.editor.cursor_pos(inner);
            let start = (crow + 1).saturating_sub(max_h).min(rows.len().saturating_sub(1));
            for (i, r) in rows.iter().enumerate().skip(start).take(max_h) {
                let mut segs = vec![bar.clone(), pad(padx)];
                segs.extend(r.iter().cloned());
                out.push(SLine::new(segs).with_bg(Some(bg)));
                if i == crow {
                    cursor = Some(((1 + padx + ccol.min(inner)) as u16, (1 + i - start) as u16));
                }
            }
        }
        out.push(SLine::new(vec![bar.clone()]).with_bg(Some(bg)));
        // Metadata row.
        let mut meta = vec![bar.clone(), pad(padx)];
        meta.extend(self.metadata_segs(t, hl, muted));
        out.push(SLine::new(meta).with_bg(Some(bg)));
        // Cap row: `╹` + `▀` in promptBg.
        let cap = vec![s("╹", hl), s("▀".repeat(width.saturating_sub(1)), prompt_bg)];
        out.push(SLine::new(cap));
        // Footer row.
        out.push(self.prompt_footer(t, width));
        (out, cursor)
    }

    fn placeholder_text(&self, width: usize) -> String {
        if self.session_id().is_some() {
            return String::new();
        }
        let w = self.size.0 as usize;
        let normal = ["Fix a TODO in the codebase", "What is the tech stack of this project?", "Fix broken tests"];
        let shell = ["ls -la", "git status", "pwd"];
        let text = if self.editor.shell {
            format!("Run a command… \"{}\"", shell[self.placeholder % shell.len()])
        } else {
            format!("Ask anything… \"{}\"", normal[self.placeholder % normal.len()])
        };
        let tw = if w < 44 { w.saturating_sub(5) } else { 75.min(w.saturating_sub(4)).saturating_sub(5) }.max(1);
        let _ = width;
        locale::take_width(&text, tw).trim_end().to_string()
    }

    fn metadata_segs(&self, t: &Theme, hl: Rgba, muted: bool) -> Vec<Seg> {
        let term_w = self.size.0 as usize;
        if self.editor.shell {
            return vec![s("Shell", hl)];
        }
        let Some(agent) = self.current_agent() else { return vec![] };
        let agent_label = if term_w < 44 { None } else { Some(locale::titlecase(&agent.id)) };
        let model = self.current_model();
        let info = self.current_model_info();
        let (model_name, provider) = match &model {
            None => ("No provider selected".to_string(), "Connect a provider".to_string()),
            Some(m) => {
                let name = info.as_ref().map(|i| i.name.clone()).unwrap_or_else(|| format!("{} (unavailable)", m.id));
                let p = self.providers().iter().find(|p| p.id == m.provider_id).map(|p| p.name.clone()).filter(|n| !n.is_empty()).unwrap_or(m.provider_id.clone());
                (name, p)
            }
        };
        let provider = if term_w < 44 { String::new() } else { provider };
        let variant = self.current_variant();
        let auto = self.autoaccept;
        let width = term_w.saturating_sub(if term_w < 44 { 9 } else { 13 });
        let text = |a: &Option<String>, au: bool, m: &str, p: &str, v: &Option<String>| -> String {
            let mut parts: Vec<String> = Vec::new();
            if let Some(a) = a {
                parts.push(a.clone());
            }
            if au {
                parts.push("auto".into());
            }
            if a.is_some() {
                parts.push("·".into());
            }
            parts.push(m.to_string());
            if !p.is_empty() {
                parts.push(p.to_string());
            }
            if let Some(v) = v {
                parts.push("·".into());
                parts.push(v.clone());
            }
            parts.join(" ")
        };
        let short_provider = provider.rsplit(" / ").next().unwrap_or(&provider).to_string();
        let candidates = [
            (auto, model_name.clone(), provider.clone()),
            (false, model_name.clone(), provider.clone()),
            (false, model_name.clone(), short_provider),
            (false, model_name.clone(), String::new()),
        ];
        let fit = candidates.iter().find(|(au, m, p)| sw(&text(&agent_label, *au, m, p, &variant)) <= width).cloned();
        let (au, m, p) = fit.unwrap_or_else(|| {
            let prefix = agent_label.as_ref().map(|a| sw(&format!("{a} · "))).unwrap_or(0);
            let suffix = variant.as_ref().map(|v| sw(&format!(" · {v}"))).unwrap_or(0);
            let m = locale::truncate_width(&model_name, 9.max(width.saturating_sub(prefix + suffix))).replace(" …", "…");
            (false, m, String::new())
        });
        let mut segs = Vec::new();
        if let Some(a) = &agent_label {
            segs.push(s(a.clone(), hl));
        }
        if au {
            segs.push(pad(1));
            segs.push(s("auto", t.text.muted));
        }
        if agent_label.is_some() {
            segs.push(pad(1));
            segs.push(s("·", t.text.muted));
            segs.push(pad(1));
        }
        segs.push(s(m, if muted { t.text.muted } else { t.text.base }));
        if !p.is_empty() {
            segs.push(pad(1));
            segs.push(s(p, t.text.muted));
        }
        if let Some(v) = variant {
            segs.push(pad(1));
            segs.push(s("·", t.text.muted));
            segs.push(pad(1));
            segs.push(Seg::new(v, bold(fg(t.text.feedback.warning.base))));
        }
        segs
    }

    fn usage_status(&self) -> Vec<String> {
        let Some(sid) = self.session_id() else { return vec![] };
        let Some(session) = self.store.session(sid) else { return vec![] };
        let boundary = session.revert.as_ref().map(|r| r.message_id.clone());
        let last = self
            .store
            .messages(sid)
            .iter()
            .rev()
            .filter(|m| boundary.as_ref().is_none_or(|b| m.id < *b))
            .find(|m| m.kind == "assistant" && m.tokens.as_ref().is_some_and(|t| t.total() > 0.0));
        let mut out = Vec::new();
        if let Some(m) = last {
            let tokens = m.tokens.as_ref().unwrap().total();
            let mr = m.model.clone().unwrap_or_default();
            let ctx = self.model_list().iter().find(|x| x.provider_id == mr.provider_id && x.id == mr.id).map(|x| x.limit.context).filter(|c| *c > 0.0);
            let value = locale::number(tokens);
            out.push(match ctx {
                Some(c) => format!("{value} ({}%)", ((tokens / c) * 100.0).round() as i64),
                None => value,
            });
        }
        let cost = if session.parent_id.is_some() {
            session.cost
        } else {
            let fam = self.store.family.get(sid).cloned().unwrap_or_else(|| vec![sid.to_string()]);
            fam.iter().map(|id| self.store.session(id).map(|s| s.cost).unwrap_or(0.0)).sum()
        };
        if cost > 0.0 {
            out.push(locale::usd(cost));
        }
        out
    }

    fn prompt_footer(&self, t: &Theme, width: usize) -> SLine {
        let term_w = self.size.0 as usize;
        let kb = &self.keybinds;
        // Left: status.
        let mut left: Vec<Seg> = Vec::new();
        if self.running() {
            left.push(pad(1));
            if self.config.animations() {
                left.extend(knight_rider(self.frame_index(), self.highlight(t), t));
            } else {
                left.push(s("[⋯]", t.text.muted));
            }
            left.push(pad(1));
            if self.interrupt > 0 {
                left.push(s("esc again to interrupt", t.text.feedback.warning.base));
            } else {
                left.push(s("esc", t.text.base));
                left.push(s(" interrupt", t.text.muted));
            }
        } else {
            let dir = self.location_dir();
            let branch = self.loc().and_then(|l| l.vcs.as_ref()).and_then(|v| v.pointer("/branch/current")).and_then(|b| b.as_str()).map(|b| b.to_string());
            let label = format!("{}{}", locale::abbreviate_home(&dir), branch.map(|b| format!(":{b}")).unwrap_or_default());
            left.push(s(locale::truncate_file_path(&label, width / 2), t.text.muted));
        }
        // Right: PromptFooter.
        let show_details = self.interrupt == 0 || term_w >= 80;
        let mut right: Vec<Seg> = Vec::new();
        if self.editor.shell {
            right.push(s("esc", t.text.base));
            right.push(s(if term_w < 44 { " shell" } else { " exit shell mode" }, t.text.muted));
        } else {
            let status = self.usage_status();
            let sid = self.session_id().map(|x| x.to_string());
            let subagents = sid.as_ref().map(|s| self.store.family_of(s).iter().filter(|id| *id != s && self.store.is_running(id)).count()).unwrap_or(0);
            let live = subagents > 0;
            let command = kb.shortcut("command.palette.show");
            let (usage_ok, shortcuts_ok) = if status.is_empty() {
                (false, term_w >= 44)
            } else {
                let w = term_w.saturating_sub(8);
                let avail = w.saturating_sub(28.min(w / 2));
                let usage = status.join(" · ");
                let sc = if command.is_empty() { String::new() } else { format!("{command} commands") };
                if !sc.is_empty() && sw(&format!("{usage} · {sc}")) <= avail {
                    (true, true)
                } else if sw(&usage) <= avail {
                    (true, false)
                } else {
                    (false, false)
                }
            };
            if live || !status.is_empty() {
                if live {
                    let key = kb.shortcut("session.child.first");
                    if !key.is_empty() {
                        right.push(s(format!("{key} "), t.text.base));
                    }
                    right.push(s(format!("{subagents} subagent{}", if subagents == 1 { "" } else { "s" }), t.text.muted));
                }
                if show_details && usage_ok {
                    if live {
                        right.push(s(" · ", t.text.muted));
                    }
                    right.push(s(status.join(" · "), t.text.muted));
                }
            } else if show_details && shortcuts_ok {
                right.push(s(kb.shortcut("agent.cycle"), t.text.base));
                right.push(s(" agents", t.text.muted));
            }
            if show_details && shortcuts_ok {
                if !right.is_empty() {
                    right.push(pad(2));
                }
                right.push(s(command, t.text.base));
                right.push(s(" commands", t.text.muted));
            }
        }
        let lw: usize = left.iter().map(|x| x.width()).sum();
        let rw: usize = right.iter().map(|x| x.width()).sum();
        let mut segs = left;
        segs.push(pad(width.saturating_sub(lw + rw)));
        segs.extend(right);
        SLine::new(clip(&segs, width))
    }

    fn frame_index(&self) -> usize {
        (self.start.elapsed().as_millis() / 40) as usize
    }

    pub(super) fn render_prompt(&mut self, buf: &mut Buffer, area: Rect, _t: &Theme, lines: (Vec<SLine>, Option<(u16, u16)>), cursor: &mut Option<(u16, u16)>) {
        let (lines, c) = lines;
        self.prompt_rect = area;
        for (i, l) in lines.iter().enumerate() {
            if i as u16 >= area.height {
                break;
            }
            let row = Rect::new(area.x, area.y + i as u16, area.width, 1);
            if let Some(bg) = l.bg {
                fill(buf, Rect::new(area.x + 1, row.y, area.width.saturating_sub(1), 1), bg);
            }
            let mut ln = l.clone();
            ln.bg = None;
            draw_lines(buf, row, &[ln], 0, "");
        }
        if self.dialogs.is_empty() && self.prompt_focusable() && !self.right_focus {
            *cursor = c.map(|(x, y)| (area.x + x, area.y + y));
        }
        // Autocomplete popup above the prompt.
        if let Some(ac) = self.autocomplete.clone() {
            self.draw_autocomplete(buf, area, &ac);
        }
    }

    fn prompt_focusable(&self) -> bool {
        self.active_permission().is_none() && self.active_form().is_none()
    }

    fn draw_autocomplete(&mut self, buf: &mut Buffer, anchor: Rect, ac: &super::Autocomplete) {
        let t = self.theme().clone();
        let count = ac.items.len();
        let h = 10.min(count.max(1)).min((anchor.y as usize).max(1)) as u16;
        let rect = Rect::new(anchor.x, anchor.y.saturating_sub(h), anchor.width, h);
        let bg = t.background.raised.high.tui();
        fill(buf, rect, bg);
        let inner_w = anchor.width.saturating_sub(2) as usize;
        let mut lines: Vec<SLine> = Vec::new();
        if count == 0 {
            let text = match ac.mode.as_str() {
                "command" => "No matching commands",
                "directory" => {
                    if ac.loading {
                        "Searching…"
                    } else if ac.error {
                        "Could not search directories. Keep typing to try again."
                    } else {
                        "No matching directories"
                    }
                }
                _ => {
                    if ac.loading {
                        "Searching…"
                    } else if ac.error {
                        "Could not search files. Keep typing to try again."
                    } else {
                        "No matching files, agents, or references"
                    }
                }
            };
            let c = if ac.error { t.text.feedback.error.base } else { t.text.muted };
            lines.push(SLine::new(vec![pad(1), s(text, c)]));
        }
        let start = ac.selected.saturating_sub(h as usize - 1);
        for (i, item) in ac.items.iter().enumerate().skip(start).take(h as usize) {
            let selected = i == ac.selected;
            let (fgc, bgc) = if selected { (t.text.action.primary.focused, Some(t.background.action.primary.focused)) } else { (t.text.base, None) };
            let label = item.label.clone();
            let lw = label.as_ref().map(|l| sw(l) + 2).unwrap_or(0);
            let display = locale::truncate_middle_width(&item.display, inner_w.saturating_sub(4 + lw).max(1));
            let mut segs = vec![pad(1), s(display.clone(), fgc)];
            if let Some(d) = &item.description {
                let room = inner_w.saturating_sub(2 + sw(&display) + lw + 2);
                if room > 1 {
                    segs.push(s(locale::truncate_width(&format!(" {}", locale::collapse_ws(d)), room), if selected { fgc } else { t.text.muted }));
                }
            }
            if let Some(l) = label {
                let used: usize = segs.iter().map(|x| x.width()).sum();
                segs.push(pad(inner_w.saturating_sub(used + sw(&l) + 1)));
                segs.push(s(l, if selected { fgc } else { t.text.muted }));
            }
            let mut line = SLine::new(segs);
            if let Some(b) = bgc {
                line = line.with_bg(Some(b.tui()));
            }
            lines.push(line);
        }
        for y in rect.y..rect.bottom() {
            put(buf, rect.x, y, "┃", Style::default().fg(t.border.tui()).bg(bg), 1);
            put(buf, rect.right().saturating_sub(1), y, "┃", Style::default().fg(t.border.tui()).bg(bg), 1);
        }
        draw_lines(buf, Rect::new(rect.x + 1, rect.y, rect.width.saturating_sub(2), rect.height), &lines, 0, "");
    }

    // ---- session -----------------------------------------------------------

    fn draw_session(&mut self, buf: &mut Buffer, area: Rect, t: &Theme, sid: &str, cursor: &mut Option<(u16, u16)>) {
        let wide = area.width > 120;
        let sidebar = self.sidebar_visible();
        let mut pane = area;
        let explicit_sidebar = self.views.get(sid).and_then(|v| v.sidebar_open) == Some(true);
        if let Some(pty) = self.terminal_visible().filter(|_| !explicit_sidebar) {
            let size = self.pane_width(area.width);
            pane = Rect::new(area.x, area.y, area.width - size, area.height);
            let right = Rect::new(area.right() - size, area.y, size, area.height);
            self.terminal_rect = right;
            let c = self.draw_terminal(buf, right, t, &pty);
            if self.right_focus {
                *cursor = c;
            }
            if self.resizing == Some(super::tabs::Resizing::Pane) && area.width >= 3 {
                let hx = right.x.saturating_sub(1);
                for y in right.top()..right.bottom() {
                    buf[(hx, y)].set_bg(t.background.raised.high.tui());
                }
            }
        } else if sidebar && wide {
            let sw = 42.min(area.width);
            pane = Rect::new(area.x, area.y, area.width - sw, area.height);
            self.draw_sidebar(buf, Rect::new(area.right() - sw, area.y, sw, area.height), t, sid);
        }
        let padx = if area.width < 44 { 1 } else { 2 };
        let inner = Rect::new(pane.x + padx, pane.y, pane.width.saturating_sub(padx * 2), pane.height.saturating_sub(1));
        let width = inner.width as usize;
        // Bottom dock.
        let mut dock: Vec<SLine> = Vec::new();
        let queued = self.queued_prompts();
        let permission = self.active_permission();
        let form = self.active_form();
        if !queued.is_empty() && permission.is_none() && form.is_none() {
            let bg = t.background.raised.base.tui();
            let first = queued[0].payload.get("text").and_then(|v| v.as_str()).unwrap_or("").replace('\n', " ");
            let bar = Seg::new("┃", Style::default().fg(t.border.tui()).bg(bg));
            let text = vec![s(format!("{} queued", queued.len()), t.text.base), s(format!(" · {first}"), t.text.muted)];
            dock.push(SLine::new(vec![bar.clone()]).with_bg(Some(bg)).on_click(Some(Click::QueuedDialog)));
            let mut l = vec![bar.clone(), pad(2)];
            l.extend(clip(&text, width.saturating_sub(4)));
            dock.push(SLine::new(l).with_bg(Some(bg)).on_click(Some(Click::QueuedDialog)));
            dock.push(SLine::new(vec![bar]).with_bg(Some(bg)).on_click(Some(Click::QueuedDialog)));
            dock.push(SLine::blank());
        }
        let mut prompt_lines = None;
        if self.composer_open() && form.is_none() {
            dock.extend(self.composer_lines(t, width));
        } else if let Some(p) = &permission {
            dock.extend(self.permission_lines(t, width, p));
        } else if form.is_some() {
            dock.extend(self.form_lines(t, width));
        } else {
            prompt_lines = Some(self.prompt_lines(t, width));
        }
        let prompt_h = prompt_lines.as_ref().map(|p| p.0.len()).unwrap_or(0);
        let max_dock = (inner.height as usize).saturating_sub(4);
        let dock_h = (dock.len() + prompt_h).min(max_dock) as u16;
        let strip_y = inner.bottom().saturating_sub(dock_h + 1);
        let transcript = Rect::new(inner.x, inner.y, inner.width, strip_y.saturating_sub(inner.y));
        self.draw_transcript(buf, transcript, t, sid, width);
        // Status strip.
        let away = self.views.get(sid).is_some_and(|v| v.scroll_from_bottom > 0);
        if away {
            let label = "Jump to latest ↓";
            let x = inner.right().saturating_sub(sw(label) as u16);
            put(buf, x, strip_y, label, fg(t.text.action.secondary.base), sw(label));
            self.hits.push((Rect::new(x, strip_y, sw(label) as u16, 1), Click::JumpBottom));
        }
        // Dock.
        let mut y = strip_y + 1;
        let dock_rect = Rect::new(inner.x, y, inner.width, (dock.len() as u16).min(dock_h));
        let hits = draw_lines(buf, dock_rect, &dock, 0, "");
        self.hits.extend(hits);
        y += dock_rect.height;
        if let Some(pl) = prompt_lines {
            let h = (pl.0.len() as u16).min(inner.bottom().saturating_sub(y));
            self.render_prompt(buf, Rect::new(inner.x, y, inner.width, h), t, pl, cursor);
        }
        // Narrow sidebar overlay.
        if sidebar && !wide && self.views.get(sid).and_then(|v| v.sidebar_open) == Some(true) {
            dim(buf, area, 70, t.background.base);
            let sw = 42.min(area.width);
            self.draw_sidebar(buf, Rect::new(area.right() - sw, area.y, sw, area.height), t, sid);
        }
    }

    pub fn render_transcript_lines(&self, sid: &str, width: usize) -> (Vec<SLine>, Vec<(usize, String, bool)>) {
        let t = self.theme();
        let messages = self.store.messages(sid).to_vec();
        let pending = self.store.pending.get(sid).cloned().unwrap_or_default();
        let queued: HashSet<String> = pending.iter().filter(|i| i.kind == "user" && i.delivery == "queue").map(|i| i.id.clone()).collect();
        let boundary = self.store.session(sid).and_then(|s| s.revert.as_ref().map(|r| r.message_id.clone()));
        let visible: Vec<Message> = messages
            .iter()
            .filter(|m| !queued.contains(&m.id))
            .filter(|m| boundary.as_ref().is_none_or(|b| m.id < *b))
            .cloned()
            .collect();
        let inputs: HashSet<String> = pending.iter().filter(|i| i.kind != "compaction").map(|i| i.id.clone()).collect();
        let compactions: Vec<String> = pending.iter().filter(|i| i.kind == "compaction").map(|i| i.id.clone()).collect();
        let blocked: HashSet<String> = self
            .store
            .permissions
            .get(sid)
            .map(|l| l.iter().filter_map(|p| p.tool_id().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        let rows = transcript::build_rows(&visible, &inputs, &compactions, self.config.turn_tokens().is_some(), self.config.verbosity(), &blocked);
        let models = self.model_list();
        let colors = self.agent_colors();
        let expanded = self.views.get(sid).map(|v| v.expanded.clone()).unwrap_or_default();
        let pending_inputs: HashMap<String, String> = pending.iter().filter(|i| i.kind == "user").map(|i| (i.id.clone(), i.delivery.clone())).collect();
        let shells: HashSet<String> = self.store.locations.values().flat_map(|l| l.shells.iter().map(|s| s.id.clone())).collect();
        let running: HashSet<String> = self.store.active.iter().filter(|(_, v)| **v).map(|(k, _)| k.clone()).collect();
        let session_agent = self.store.session(sid).and_then(|s| s.agent.clone());
        let ctx = RCtx {
            theme: t,
            syntax: &self.syntax,
            thinking_syntax: &self.thinking_syntax,
            config: &self.config,
            width,
            term_width: self.size.0 as usize,
            models: &models,
            messages: &visible,
            session_agent: session_agent.as_deref(),
            agent_colors: &colors,
            default_agent_color: self.default_agent_color(),
            expanded: &expanded,
            permission_tool: if self.autoaccept { None } else { self.store.permissions.get(sid).and_then(|l| l.first()).and_then(|p| p.tool_id()) },
            pending_inputs: &pending_inputs,
            now: self.now(),
            animations: self.config.animations(),
            redo_key: self.keybinds.shortcut("session.redo"),
            running_shells: &shells,
            shell_output: &self.shell_output,
            running_sessions: &running,
            home: Some(self.location_dir()),
        };
        let light = self.themes.effective_mode() == crate::theme::Mode::Light;
        let mut out = Vec::new();
        let mut marks: Vec<(usize, String, bool)> = Vec::new();
        let has_text = |m: &Message| m.content.iter().any(|c| matches!(c, Content::Text { text, .. } if !text.trim().is_empty()));
        for row in &rows {
            let lines = transcript::render_row(&ctx, row, light);
            if lines.is_empty() {
                continue;
            }
            let mid = match row {
                transcript::Row::Entry(transcript::Entry::Message(id)) => {
                    visible.iter().find(|m| &m.id == id).filter(|m| m.kind == "user" && !m.text_str().trim().is_empty()).map(|m| (m.id.clone(), true))
                }
                transcript::Row::Entry(transcript::Entry::Part(r)) => Some(r.message.clone()).map(|m| (m, false)),
                transcript::Row::Entry(transcript::Entry::Footer(id)) => Some((id.clone(), false)),
                transcript::Row::Group { children, .. } => {
                    let mut refs = Vec::new();
                    fn first(nodes: &[transcript::Node], out: &mut Vec<String>) {
                        for n in nodes {
                            match n {
                                transcript::Node::Entry(transcript::Entry::Part(r)) => out.push(r.message.clone()),
                                transcript::Node::Group { children, .. } => first(children, out),
                                _ => {}
                            }
                        }
                    }
                    first(children, &mut refs);
                    refs.into_iter().next().map(|m| (m, false))
                }
                _ => None,
            };
            if let Some((id, user)) = mid
                && (user || visible.iter().any(|m| m.id == id && m.kind == "assistant" && has_text(m)))
                && !marks.iter().any(|(_, x, _)| *x == id)
            {
                marks.push((if user { out.len() + 1 } else { out.len() }, id, user));
            }
            out.push(SLine::blank());
            out.extend(lines);
        }
        // Background tool hint.
        if let Some(m) = visible.iter().rev().find(|m| m.kind == "assistant" && m.time.completed.is_none()) {
            let long = m.content.iter().any(|c| matches!(c, Content::Tool(tp) if tp.state.status == "running"
                && matches!(transcript::canonical_tool(&tp.name), "shell" | "subagent")
                && tp.time.ran.is_some_and(|r| self.now() - r >= 3000)));
            if long {
                out.push(SLine::blank());
                out.push(SLine::new(vec![pad(3), s("Press ", t.text.muted), s(self.keybinds.shortcut("session.background"), t.text.base), s(" to move running work to the background", t.text.muted)]));
            }
        }
        if let Some(b) = boundary {
            let count = messages.iter().filter(|m| m.kind == "user" && m.id >= b).count();
            let files = self.store.session(sid).and_then(|s| s.revert.as_ref().and_then(|r| r.files.clone())).unwrap_or_default();
            out.push(SLine::blank());
            out.extend(transcript::revert_banner(&ctx, count, &files));
        }
        (out, marks)
    }

    fn draw_transcript(&mut self, buf: &mut Buffer, area: Rect, _t: &Theme, sid: &str, width: usize) {
        self.transcript_rect = area;
        if area.height == 0 {
            return;
        }
        let key = format!(
            "{sid}|{width}|{}|{}|{}|{}|{}",
            self.render_rev,
            self.store.generation,
            if self.store.is_running(sid) { self.now() / 1000 } else { 0 },
            self.themes.active,
            self.config.raw
        );
        let mut lines = match &self.transcript_cache {
            Some((k, l)) if *k == key => l.clone(),
            _ => {
                let (l, marks) = self.render_transcript_lines(sid, width);
                self.transcript_marks = marks;
                self.transcript_cache = Some((key, l.clone()));
                l
            }
        };
        let slack = self.views.get(sid).map(|v| v.nav_slack).unwrap_or(0);
        for _ in 0..slack {
            lines.push(SLine::blank());
        }
        self.transcript_lines = lines.len();
        let h = area.height as usize;
        let max = lines.len().saturating_sub(h);
        let from_bottom = self.views.get(sid).map(|v| v.scroll_from_bottom).unwrap_or(0).min(max);
        if let Some(v) = self.views.get_mut(sid) {
            v.scroll_from_bottom = from_bottom;
        }
        let offset = max - from_bottom;
        let hits = draw_lines(buf, area, &lines, offset, SPINNER_FRAMES[0]);
        self.hits.extend(hits);
    }

    fn draw_sidebar(&mut self, buf: &mut Buffer, area: Rect, t: &Theme, sid: &str) {
        let bg = t.background.raised.base.tui();
        fill(buf, area, bg);
        let inner = Rect::new(area.x + 2, area.y + 1, area.width.saturating_sub(4), area.height.saturating_sub(2));
        let w = inner.width as usize;
        let mut lines: Vec<SLine> = Vec::new();
        let title = self.store.session(sid).map(|s| s.display_title()).unwrap_or_default();
        for l in wrap(&[Seg::new(title, bold(fg(t.text.base)))], w.saturating_sub(2).max(1), Wrap::Word) {
            lines.push(SLine::new(l));
        }
        lines.push(SLine::blank());
        // Context block.
        let status = self.usage_status();
        if let Some(s0) = self.store.session(sid)
            && (!status.is_empty() || s0.cost > 0.0)
        {
            lines.push(SLine::new(vec![Seg::new("Context", bold(fg(t.text.base)))]));
            let last = self.store.messages(sid).iter().rev().find(|m| m.kind == "assistant" && m.tokens.as_ref().is_some_and(|t| t.total() > 0.0));
            if let Some(m) = last {
                let tokens = m.tokens.as_ref().unwrap().total();
                lines.push(SLine::new(vec![s(format!("{} tokens", locale::grouped(tokens as i64)), t.text.muted)]));
                let mr = m.model.clone().unwrap_or_default();
                if let Some(c) = self.model_list().iter().find(|x| x.provider_id == mr.provider_id && x.id == mr.id).map(|x| x.limit.context).filter(|c| *c > 0.0) {
                    lines.push(SLine::new(vec![s(format!("{}% used", ((tokens / c) * 100.0).round() as i64), t.text.muted)]));
                }
            }
            lines.push(SLine::new(vec![s(format!("{} spent", locale::usd(s0.cost)), t.text.muted)]));
            lines.push(SLine::blank());
        }
        // MCP block.
        let servers = self.loc().and_then(|l| l.mcp.clone()).unwrap_or_default();
        if !servers.is_empty() {
            lines.push(SLine::new(vec![Seg::new("MCP", bold(fg(t.text.base)))]));
            for sv in &servers {
                let (c, label) = match sv.status.status.as_str() {
                    "connected" => (t.text.feedback.success.base, "Connected"),
                    "pending" => (t.text.muted, "Connecting"),
                    "failed" => (t.text.feedback.error.base, "Error"),
                    "disabled" => (t.text.muted, "Disabled"),
                    "needs_auth" => (t.text.feedback.warning.base, "Sign in"),
                    _ => (t.text.muted, sv.status.status.as_str()),
                };
                let lc = if sv.status.status == "failed" { t.text.feedback.error.base } else { t.text.muted };
                lines.push(SLine::new(vec![s("• ", c), Seg::new(locale::truncate_width(&sv.name, w.saturating_sub(sw(label) + 3)), bold(fg(t.text.base))), s(format!(" {label}"), lc)]));
            }
        }
        draw_lines(buf, inner, &lines, 0, "");
        // Footer: location.
        let dir = self.location_dir();
        let branch = self.loc().and_then(|l| l.vcs.as_ref()).and_then(|v| v.pointer("/branch/current")).and_then(|b| b.as_str()).map(|b| b.to_string());
        let label = format!("{}{}", locale::abbreviate_home(&dir), branch.map(|b| format!(":{b}")).unwrap_or_default());
        let y = inner.bottom().saturating_sub(1);
        put(buf, inner.x, y, &locale::truncate_file_path(&label, 38.min(w)), fg(t.text.muted).bg(bg), w);
    }

    // ---- toasts & overlays -------------------------------------------------

    pub(super) fn draw_toast(&mut self, buf: &mut Buffer, area: Rect, t: &Theme) {
        let Some(toast) = self.toast.clone() else { return };
        let max_w = 60.min(area.width.saturating_sub(6)) as usize;
        let border = match toast.variant.as_str() {
            "error" => t.text.feedback.error.base,
            "warning" => t.text.feedback.warning.base,
            "success" => t.text.feedback.success.base,
            _ => t.text.feedback.info.base,
        };
        let bg = t.background.raised.high;
        let inner = max_w.saturating_sub(6).max(1);
        let affordance = match &toast.action {
            Some((label, _)) => format!("› {label}"),
            None => "x".into(),
        };
        let mut body: Vec<SLine> = Vec::new();
        if let Some(title) = &toast.title {
            let tw = inner.saturating_sub(sw(&affordance) + 2);
            let title = locale::truncate_width(title, tw);
            let gap = inner.saturating_sub(sw(&title) + sw(&affordance));
            body.push(SLine::new(vec![Seg::new(title, bold(fg(t.text.base))), pad(gap), s(affordance.clone(), t.text.muted)]));
            body.push(SLine::blank());
            body.extend(wrap(&[s(toast.message.clone(), t.text.base)], inner, Wrap::Word).into_iter().map(SLine::new));
        } else {
            let mw = inner.saturating_sub(sw(&affordance) + 2).max(1);
            let lines = wrap(&[s(toast.message.clone(), t.text.base)], mw, Wrap::Word);
            for (i, l) in lines.into_iter().enumerate() {
                let mut l = SLine::new(l);
                if i == 0 {
                    let used = l.width();
                    l.segs.push(pad(inner.saturating_sub(used + sw(&affordance))));
                    l.segs.push(s(affordance.clone(), t.text.muted));
                }
                body.push(l);
            }
        }
        if !self.toast_queue.is_empty() {
            body.push(SLine::blank());
            body.push(SLine::new(vec![s(format!("+{} more", self.toast_queue.len()), t.text.muted)]));
        }
        let content_w = body.iter().map(|l| l.width()).max().unwrap_or(1).min(inner);
        let w = (content_w + 6) as u16;
        let h = (body.len() + 2) as u16;
        let x = area.right().saturating_sub(w + 2);
        let y = area.y + 1;
        let rect = Rect::new(x, y, w, h);
        fill(buf, rect, bg.tui());
        for yy in rect.y..rect.bottom() {
            put(buf, rect.x, yy, "┃", fg(border).bg(bg.tui()), 1);
            put(buf, rect.right() - 1, yy, "┃", fg(border).bg(bg.tui()), 1);
        }
        draw_lines(buf, Rect::new(x + 3, y + 1, w.saturating_sub(6), h.saturating_sub(2)), &body, 0, "");
    }

    pub(super) fn draw_reconnecting(&mut self, buf: &mut Buffer, area: Rect, t: &Theme) {
        if self.connection == ConnStatus::Connected {
            return;
        }
        let delay = if self.connection == ConnStatus::Reconnecting && self.ever_connected { 1000 } else { 5000 };
        if self.connection_since.elapsed().as_millis() < delay {
            return;
        }
        dim(buf, area, 150, t.background.base);
        let w = 48.min((area.width as f64 * 0.9) as u16);
        let lines = vec![
            SLine::new(vec![s(format!("{SPINNER_GLYPH} Connection lost…"), t.text.base)]),
            SLine::blank(),
            SLine::new(vec![s("Reconnecting to the server automatically.", t.text.muted)]),
        ];
        let h = 5;
        let rect = Rect::new(area.x + (area.width.saturating_sub(w)) / 2, area.y + (area.height.saturating_sub(h)) / 2, w, h);
        fill(buf, rect, t.background.raised.base.tui());
        draw_lines(buf, Rect::new(rect.x + 2, rect.y + 1, w.saturating_sub(4), 3), &lines, 0, "");
    }

    // ---- permission --------------------------------------------------------

    pub(super) fn permission_lines(&self, t: &Theme, width: usize, req: &PermissionRequest) -> Vec<SLine> {
        let ui = self.permission_ui.clone().filter(|u| u.id == req.id).unwrap_or_default();
        let bg = t.background.raised.base;
        let bar_c = if ui.rejecting { t.text.feedback.error.base } else { t.background.action.primary.focused };
        let bar = Seg::new("┃", Style::default().fg(bar_c.tui()).bg(bg.tui()));
        let inner = width.saturating_sub(5).max(1);
        let mut body: Vec<SLine> = Vec::new();
        let fmt = |p: &str| transcript::format_path(p, Some(&self.location_dir()));
        let (icon, title, lines, diff) = F::permission_presentation(&req.action, &req.resources, req.metadata.as_ref(), &fmt);
        if ui.rejecting {
            body.push(SLine::new(vec![s("△", t.text.feedback.error.base), s(" Reject permission", t.text.base)]));
            body.push(SLine::new(vec![pad(2), s("Tell OpenCode what to do differently", t.text.muted)]));
        } else {
            body.push(SLine::new(vec![s("△", t.text.feedback.warning.base), s(" Permission required", t.text.base)]));
            if req.action != "shell" {
                body.push(SLine::new(vec![pad(2), s(format!("{icon} "), t.text.muted), s(title, t.text.base)]));
            }
        }
        let options: Vec<&str> = if req.save.as_ref().is_some_and(|s| !s.is_empty()) { vec!["once", "always", "reject"] } else { vec!["once", "reject"] };
        if !ui.rejecting {
            body.push(SLine::blank());
            let selected = options.get(ui.selected).copied().unwrap_or("once");
            if selected == "always" {
                for (i, l) in F::always_lines(&req.action, req.save.as_deref().unwrap_or(&[])).into_iter().enumerate() {
                    body.extend(wrap(&[s(l, if i == 0 { t.text.muted } else { t.text.base })], inner, Wrap::Word).into_iter().map(|x| SLine::new(x).prefixed(&[pad(1)])));
                }
            } else if let Some(d) = diff {
                let o = crate::ui::diff::DiffOpts {
                    theme: t,
                    syntax: &self.syntax,
                    filetype: &crate::ui::highlight::filetype(req.resources.first().map(|s| s.as_str())),
                    split: match self.config.diffs_view() {
                        "split" => true,
                        "unified" => false,
                        _ => self.size.0 > 120,
                    },
                    wrap: true,
                };
                let max = if ui.expanded { 200 } else { 8 };
                body.extend(crate::ui::diff::render(&crate::ui::diff::parse(&d), inner, &o).into_iter().take(max));
            } else if req.action == "edit" {
                body.push(SLine::new(vec![pad(1), s("No diff provided", t.text.muted)]));
            } else {
                let base_lines = matches!(req.action.as_str(), "bash" | "shell" | "task" | "subagent");
                for l in lines {
                    body.extend(wrap(&[s(l, if base_lines { t.text.base } else { t.text.muted })], inner, Wrap::Word).into_iter().map(|x| SLine::new(x).prefixed(&[pad(1)])));
                }
            }
        }
        let max_body = if ui.expanded { 1000 } else { 15usize.saturating_sub(5) };
        let mut out: Vec<SLine> = vec![SLine::new(vec![bar.clone()]).with_bg(Some(bg.tui()))];
        for l in body.into_iter().take(max_body) {
            out.push(l.prefixed(&[bar.clone(), pad(1)]).with_bg(Some(bg.tui())));
        }
        out.push(SLine::new(vec![bar.clone()]).with_bg(Some(bg.tui())));
        // Footer band.
        let band = t.dec(bg);
        let mut left: Vec<Seg> = Vec::new();
        let mut right: Vec<Seg> = Vec::new();
        if ui.rejecting {
            let text = if ui.reject_text.is_empty() { String::new() } else { ui.reject_text.clone() };
            left.push(s(text, t.text.base));
            right.push(s("enter", t.text.base));
            right.push(s(" confirm", t.text.muted));
            right.push(pad(2));
            right.push(s("esc", t.text.base));
            right.push(s(" cancel", t.text.muted));
        } else {
            for (i, o) in options.iter().enumerate() {
                let label = match *o {
                    "once" => "Allow once",
                    "always" => "Always allow",
                    _ => "Reject",
                };
                if i == ui.selected {
                    left.push(Seg::new(format!(" {label} "), fg_bg(t.text.action.primary.focused, t.background.action.primary.focused)));
                } else {
                    left.push(s(format!(" {label} "), t.text.action.primary.base));
                }
            }
            let fk = self.keybinds.shortcut("permission.prompt.fullscreen");
            right.push(s(fk, t.text.base));
            right.push(s(if ui.expanded { " minimize" } else { " fullscreen" }, t.text.muted));
            if options.len() > 1 {
                right.push(pad(2));
                right.push(s("⇆", t.text.base));
                right.push(s(" select", t.text.muted));
            }
            right.push(pad(2));
            right.push(s("enter", t.text.base));
            right.push(s(" confirm", t.text.muted));
        }
        let lw: usize = left.iter().map(|x| x.width()).sum();
        let rw: usize = right.iter().map(|x| x.width()).sum();
        let band_line = |segs: Vec<Seg>| SLine::new(segs).prefixed(&[bar.clone(), pad(2)]).with_bg(Some(band.tui()));
        out.push(SLine::new(vec![bar.clone()]).with_bg(Some(band.tui())));
        if self.size.0 < 80 {
            out.push(band_line(left));
            out.push(band_line(right));
        } else {
            let mut segs = left;
            segs.push(pad(width.saturating_sub(3 + 3 + lw + rw)));
            segs.extend(right);
            out.push(band_line(segs));
        }
        out.push(SLine::new(vec![bar]).with_bg(Some(band.tui())));
        out
    }

    // ---- form --------------------------------------------------------------

    pub(super) fn form_lines(&self, t: &Theme, width: usize) -> Vec<SLine> {
        let Some((form, _)) = self.active_form() else { return vec![] };
        let ui = self.form_ui.clone().filter(|u| u.id == form.id).unwrap_or_else(|| super::FormUi { id: form.id.clone(), answers: F::initial(&form.fields), ..Default::default() });
        let fields = F::visible(&form.fields, &ui.answers);
        let single = fields.len() == 1 && (F::kind(&fields[0]) == "boolean" || (F::kind(&fields[0]) == "string" && F::options(&fields[0]).is_some()));
        let tabs = if single { 1 } else { fields.len() + 1 };
        let tab = ui.tab.min(tabs - 1);
        let bg = t.background.raised.base;
        let bar = Seg::new("┃", Style::default().fg(t.background.action.primary.focused.tui()).bg(bg.tui()));
        let inner = width.saturating_sub(5).max(1);
        let mut body: Vec<SLine> = Vec::new();
        body.push(SLine::new(vec![s(form.title.clone(), t.text.muted)]));
        if let Some(m) = form.metadata.as_ref().and_then(|m| m.get("message")).and_then(|m| m.as_str()) {
            body.extend(wrap(&[s(m, t.text.base)], inner, Wrap::Word).into_iter().map(SLine::new));
        }
        if !single {
            let strip: usize = fields.iter().map(|f| sw(&F::label(f)).min(24) + 3).sum::<usize>() + 6 + 3;
            if strip <= (self.size.0 as usize).saturating_sub(8) {
                let mut segs = Vec::new();
                for (i, f) in fields.iter().enumerate() {
                    let l = locale::truncate_width(&F::label(f), 24);
                    let st = if i == tab { bold(fg(t.text.base)) } else { fg(t.text.muted) };
                    segs.push(Seg::new(l, st));
                    segs.push(pad(2));
                }
                segs.push(Seg::new("Submit", if tab == fields.len() { bold(fg(t.text.base)) } else { fg(t.text.muted) }));
                body.push(SLine::new(segs));
            } else {
                let answered = fields.iter().filter(|f| ui.answers.contains_key(&F::key(f))).count();
                let label = if tab == fields.len() { "Review".to_string() } else { format!("Field {} of {}", tab + 1, fields.len()) };
                body.push(SLine::new(vec![s(format!("{label} · {answered}/{} completed", fields.len()), t.text.muted)]));
            }
        }
        body.push(SLine::blank());
        let mut action = "confirm";
        match fields.get(tab) {
            None => {
                action = "submit";
                for f in &fields {
                    let k = F::key(f);
                    let (text, c) = if F::kind(f) == "external" {
                        if ui.acknowledged.contains(&k) { ("Acknowledged".to_string(), t.text.base) } else { ("(acknowledgement required)".to_string(), t.text.feedback.error.base) }
                    } else {
                        let v = ui.answers.get(&k);
                        match F::validate(f, v) {
                            Some(e) => (if v.is_none() { "(required)".into() } else { e }, t.text.feedback.error.base),
                            None => match v {
                                Some(v) => (F::display(f, Some(v), "(none)"), t.text.base),
                                None => ("(not answered)".to_string(), t.text.muted),
                            },
                        }
                    };
                    body.push(SLine::new(vec![s(format!("{}:", locale::truncate(&F::label(f), 40)), t.text.muted), pad(1), s(text, c)]));
                }
            }
            Some(f) => {
                let prompt = f.get("description").and_then(|d| d.as_str()).map(|s| s.to_string()).unwrap_or_else(|| F::label(f));
                body.extend(wrap(&[s(prompt, t.text.base)], inner, Wrap::Word).into_iter().map(SLine::new));
                if F::kind(f) == "external" {
                    let url = f.get("url").and_then(|u| u.as_str()).unwrap_or("");
                    body.push(SLine::new(vec![s(url, t.text.action.primary.base)]));
                    let ack = ui.acknowledged.contains(&F::key(f));
                    body.push(SLine::new(vec![if ack { s("✓ Acknowledged", t.text.feedback.success.base) } else { s("Open or copy the URL, complete the external action, then confirm.", t.text.muted) }]));
                    action = if ack { "continue" } else { "open link" };
                } else if F::textual(f) {
                    let text = if ui.text.is_empty() { s(F::placeholder(f), t.text.muted) } else { s(ui.text.clone(), t.text.formfield.focused) };
                    body.push(SLine::new(vec![pad(1), text]));
                } else {
                    let multi = F::kind(f) == "multiselect";
                    let value = ui.answers.get(&F::key(f));
                    let rows = F::rows(f);
                    for (i, (v, label, desc)) in rows.iter().enumerate() {
                        let active = i == ui.row;
                        let picked = if multi { value.and_then(|x| x.as_array()).is_some_and(|a| a.contains(v)) } else { value == Some(v) };
                        let mut segs = vec![s(format!("{}. ", i + 1), if active { t.text.formfield.focused } else { t.text.muted })];
                        if multi {
                            segs.push(s(if picked { "[✓] " } else { "[ ] " }, if active { t.text.formfield.focused } else if picked { t.text.formfield.selected } else { t.text.muted }));
                        }
                        segs.push(s(label.clone(), if active { t.text.formfield.focused } else { t.text.formfield.base }));
                        if !multi && picked {
                            segs.push(s(" ✓", t.text.formfield.selected));
                        }
                        body.push(SLine::new(segs));
                        if let Some(d) = desc {
                            body.push(SLine::new(vec![pad(if multi { 7 } else { 3 }), s(d.clone(), t.text.muted)]));
                        }
                    }
                    if F::custom(f) {
                        let i = rows.len();
                        let text = if ui.editing || !ui.text.is_empty() { ui.text.clone() } else { "Type your own answer".into() };
                        let active = i == ui.row;
                        body.push(SLine::new(vec![s(format!("{}. ", i + 1), if active { t.text.formfield.focused } else { t.text.muted }), s(text, if ui.editing { t.text.formfield.focused } else { t.text.muted })]));
                    }
                    action = if multi { if ui.row >= rows.len() { "edit" } else { "toggle" } } else { "confirm" };
                }
            }
        }
        if single {
            action = "submit";
        }
        let mut out: Vec<SLine> = vec![SLine::new(vec![bar.clone()]).with_bg(Some(bg.tui()))];
        for l in body {
            out.push(l.prefixed(&[bar.clone(), pad(1)]).with_bg(Some(bg.tui())));
        }
        out.push(SLine::new(vec![bar.clone()]).with_bg(Some(bg.tui())));
        let mut hints: Vec<Seg> = Vec::new();
        let mut hint = |k: &str, label: &str| {
            if !hints.is_empty() {
                hints.push(pad(2));
            }
            hints.push(s(k.to_string(), t.text.base));
            hints.push(s(format!(" {label}"), t.text.muted));
        };
        if !single {
            hint("⇆", "tab");
        }
        if fields.get(tab).is_some_and(|f| !F::textual(f) && F::kind(f) != "external") && !ui.editing {
            hint("↑↓", "select");
        }
        hint("enter", action);
        if fields.get(tab).is_some_and(|f| F::kind(f) == "external") {
            hint("c", "copy");
        }
        hint("esc", if ui.editing { "close" } else { "dismiss" });
        let mut segs = hints;
        if let Some(e) = &ui.error {
            let used: usize = segs.iter().map(|x| x.width()).sum();
            segs.push(pad(width.saturating_sub(used + sw(e) + 6)));
            segs.push(s(e.clone(), t.text.feedback.error.base));
        }
        out.push(SLine::new(segs).prefixed(&[bar.clone(), pad(1)]).with_bg(Some(bg.tui())));
        out.push(SLine::new(vec![bar]).with_bg(Some(bg.tui())));
        out
    }
}

/// KnightRider "blocks" spinner: 8 cells, `■` active / `⬝` inactive.
fn knight_rider(frame: usize, color: Rgba, t: &Theme) -> Vec<Seg> {
    let width = 8usize;
    let cycle = (width - 1) * 2;
    let p = frame % cycle;
    let head = if p < width { p } else { cycle - p };
    (0..width)
        .map(|i| {
            let d = head.abs_diff(i);
            if d == 0 {
                Seg::new("■", fg(color))
            } else if d <= 2 {
                Seg::new("■", fg(tint(t.background.base, color, 0.6_f64.powi(d as i32).max(0.3))))
            } else {
                Seg::new("⬝", fg(tint(t.background.base, color, 0.3)))
            }
        })
        .collect()
}

pub(super) fn replace_spinners(buf: &mut Buffer, frame: &str) {
    let glyph = SPINNER_GLYPH.to_string();
    let area = buf.area;
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if buf[(x, y)].symbol() == glyph {
                buf[(x, y)].set_symbol(frame);
            }
        }
    }
}

