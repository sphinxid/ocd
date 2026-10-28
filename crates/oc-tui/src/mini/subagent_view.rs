//! The subagent inspector (`src/mini/footer.subagent.tsx`): header with status, the child's
//! transcript as static entries in a bottom-sticky scroll box, and hints.

use super::cell::{Buffer, Color, StyledChunk};
use super::menu::footer_menu_text;
use super::scrollback::{render_entry, separator_rows};
use super::entry_body::{BodyOptions, entry_body};
use super::theme::RunTheme;
use super::types::*;
use crate::keymap::Stroke;
use crate::util::locale::width as sw;

pub const SUBAGENT_INSPECTOR_ROWS: usize = 14;
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Default)]
pub struct SubagentInspector {
    /// Rows scrolled up from the bottom (sticky bottom when 0).
    from_bottom: usize,
    started: Option<std::time::Instant>,
}

fn status_color(t: &super::theme::FooterTheme, s: SubagentStatus) -> Color {
    match s {
        SubagentStatus::Completed => t.success,
        SubagentStatus::Cancelled => t.muted,
        SubagentStatus::Error => t.error,
        SubagentStatus::Running => t.running,
    }
}

fn status_icon(s: SubagentStatus, mono: bool) -> &'static str {
    match (s, mono) {
        (SubagentStatus::Completed, false) => "●",
        (SubagentStatus::Completed, true) => "*",
        (SubagentStatus::Cancelled, false) => "○",
        (SubagentStatus::Cancelled, true) => "-",
        (SubagentStatus::Error, false) => "◍",
        (SubagentStatus::Error, true) => "!",
        (SubagentStatus::Running, false) => "◔",
        (SubagentStatus::Running, true) => ".",
    }
}

impl SubagentInspector {
    pub fn reset(&mut self) {
        self.from_bottom = 0;
        self.started = Some(std::time::Instant::now());
    }

    pub fn key(&mut self, s: &Stroke) {
        match s.name.as_str() {
            "up" | "k" => self.from_bottom += 1,
            "down" | "j" => self.from_bottom = self.from_bottom.saturating_sub(1),
            "pageup" => self.from_bottom += 6,
            "pagedown" => self.from_bottom = self.from_bottom.saturating_sub(6),
            _ => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(&mut self, buf: &mut Buffer, y: usize, width: usize, height: usize, theme: &RunTheme, tab: Option<&SubagentTab>, index: usize, total: usize, detail: &[StreamCommit], interrupt: Option<&str>, shell_output: bool, mono: bool) {
        let t = &theme.footer;
        // Left border.
        for r in 0..height {
            buf.draw_chunks(0, (y + r) as i32, &[StyledChunk::plain(if mono { "|" } else { "┃" }, t.border)], 1);
        }
        let x0 = 1;
        let w = width.saturating_sub(1);
        buf.fill_rect(x0 as i32, y as i32, w as i32, height as i32, t.surface);
        let compact = w < 56 || height < 12;
        let (pt, pl, pr, pb) = if compact { (0, 0, 0, 0) } else { (1, 1, 3, 1) };
        let inner = w.saturating_sub(pl + pr);
        let mut row = y + pt;
        let bottom = y + height - pb;
        let title = tab.map(|t| if !t.description.is_empty() { t.description.clone() } else { t.title.clone().filter(|s| !s.is_empty()).unwrap_or(t.label.clone()) }).unwrap_or_default();
        let subtitle = tab.filter(|t| title != t.label).map(|t| t.label.clone()).unwrap_or_default();
        let hint = interrupt.filter(|_| tab.is_some_and(|t| t.status == SubagentStatus::Running));
        let count = if total > 1 && index > 0 { format!("{index} of {total}") } else { String::new() };
        let controls_w = hint.map(|h| sw(&format!("{h} interrupt")) + 1).unwrap_or(0) + if count.is_empty() { 0 } else { sw(&count) + 1 };
        let header_controls = !compact && sw(&title) + 2 + controls_w <= w.saturating_sub(4);
        let title_w = w.saturating_sub(if compact { 2 } else { 6 } + if header_controls { controls_w } else { 0 }).max(1);
        if let Some(tab) = tab {
            let mut x = x0 + pl;
            let glyph = if tab.status == SubagentStatus::Running {
                let frames: Vec<&str> = if mono { vec!["-", "\\", "|", "/"] } else { SPINNER_FRAMES.to_vec() };
                let interval = if mono { 160 } else { 80 };
                let age = self.started.map(|s| s.elapsed().as_millis() as usize).unwrap_or(0);
                frames[(age / interval) % frames.len()].to_string()
            } else {
                status_icon(tab.status, mono).to_string()
            };
            buf.draw_chunks(x as i32, row as i32, &[StyledChunk::plain(glyph, status_color(t, tab.status))], 1);
            x += 2;
            let mut chunks = vec![StyledChunk::plain(footer_menu_text(&title, title_w, mono), t.text)];
            if !subtitle.is_empty() && title_w >= sw(&title) + sw(&subtitle) + 2 {
                chunks.push(StyledChunk::plain(format!("  {subtitle}"), t.muted));
            }
            buf.draw_chunks(x as i32, row as i32, &chunks, inner.saturating_sub(2));
            if header_controls {
                let mut parts = Vec::new();
                if let Some(h) = hint {
                    parts.push(format!("{h} interrupt"));
                }
                if !count.is_empty() {
                    parts.push(count.clone());
                }
                let text = parts.join(" ");
                let tw = sw(&text);
                let rx = (x0 + pl + inner).saturating_sub(tw);
                buf.draw_chunks(rx as i32, row as i32, &[StyledChunk::plain(text, t.muted)], tw);
            }
            row += if compact { 1 } else { 2 };
        }
        // Hints at the bottom.
        let mut hint_rows = 0;
        if !header_controls {
            let mut items: Vec<(String, Color)> = vec![("esc back".into(), t.action_secondary_text)];
            if let Some(h) = hint {
                items.push((format!("{h} {}", if w >= sw(h) + 10 { "interrupt" } else { "stop" }), t.action_secondary_text));
            }
            if w >= 56 {
                items.push(("pgup/pgdn scroll".into(), t.muted));
                if total > 1 && index > 0 {
                    items.push((format!("tab next {index}/{total}"), t.action_secondary_text));
                }
            }
            let mut lines: Vec<Vec<StyledChunk>> = vec![vec![]];
            let mut lw = 0;
            for (text, c) in items {
                if lw > 0 && lw + 1 + sw(&text) > inner {
                    lines.push(vec![]);
                    lw = 0;
                }
                if lw > 0 {
                    lines.last_mut().unwrap().push(StyledChunk::plain(" ", t.muted));
                    lw += 1;
                }
                lw += sw(&text);
                lines.last_mut().unwrap().push(StyledChunk::plain(text, c));
            }
            hint_rows = lines.len();
            for (i, l) in lines.iter().enumerate() {
                buf.draw_chunks((x0 + pl) as i32, (bottom - hint_rows + i) as i32, l, inner);
            }
        }
        // Transcript.
        let view_h = bottom.saturating_sub(hint_rows).saturating_sub(row);
        let cw = inner.saturating_sub(if mono { 0 } else { 1 });
        let mut parts: Vec<Buffer> = Vec::new();
        if detail.is_empty() {
            let rows = super::text::layout(&[StyledChunk::plain("No subagent activity yet", t.muted)], cw.max(1), super::text::WrapMode::Word, false);
            let mut b = Buffer::new(cw.max(1) as u16, rows.len() as u16);
            for (i, r) in rows.iter().enumerate() {
                b.draw_chunks(0, i as i32, &r.chunks, cw);
            }
            parts.push(b);
        } else {
            for (i, c) in detail.iter().enumerate() {
                let body = entry_body(c, BodyOptions { shell_output: Some(shell_output), mono });
                if i > 0 && separator_rows(Some(&detail[i - 1]), c, &body) > 0 {
                    parts.push(Buffer::new(cw.max(1) as u16, 1));
                }
                parts.push(render_entry(c, &body, theme, cw.max(1), 0, mono, false));
            }
        }
        let total_h: usize = parts.iter().map(|b| b.height as usize).sum();
        let mut canvas = Buffer::new(cw.max(1) as u16, total_h.max(1) as u16);
        let mut cy = 0;
        for p in &parts {
            super::markdown::blit(&mut canvas, p, 0, cy);
            cy += p.height as i32;
        }
        let max_scroll = total_h.saturating_sub(view_h);
        self.from_bottom = self.from_bottom.min(max_scroll);
        let start = max_scroll - self.from_bottom;
        for r in 0..view_h.min(total_h) {
            for cx in 0..cw {
                let cell = canvas.get(cx as u16, (start + r) as u16).clone();
                if cell.glyph == super::cell::Glyph::Empty {
                    continue;
                }
                let i = (row + r) * buf.width as usize + x0 + pl + cx;
                if i < buf.cells.len() {
                    let bg = buf.cells[i].bg;
                    buf.cells[i] = cell;
                    if buf.cells[i].bg.a == 0 {
                        buf.cells[i].bg = bg;
                    }
                }
            }
        }
        if !mono && view_h > 0 {
            super::blocker::draw_scrollbar(buf, x0 + pl + inner - 1, row, view_h, total_h, view_h, start, t.surface, t.line);
        }
    }
}
