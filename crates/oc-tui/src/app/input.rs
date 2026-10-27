//! Keyboard/mouse/paste dispatch (spec §5.7–5.8), prompt submission (§4.9), autocomplete (§4.7),
//! permission (§2.8) and form (§2.9) interaction.

use super::{AcItem, App, Autocomplete, FormUi, PermissionUi, Route, Toast};
use crate::api::types::*;
use crate::keymap::{Match, Stroke};
use crate::ui::dialog::{self, Body, DialogHit, Outcome};
use crate::ui::form as F;
use crate::ui::prompt::ChipKind;
use crate::ui::text::Click;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// Global-mode command ids (active in every mode).
const GLOBAL: &[&str] = &[
    "session.sidebar.toggle",
    "pane.focus.left",
    "pane.focus.right",
    "terminal.select",
    "terminal.toggle",
    "terminal.close",
    "session.new",
    "session.list",
    "open.menu",
    "session.tab.next",
    "session.tab.previous",
    "session.tab.next_unread",
    "session.tab.previous_unread",
    "session.tab.close",
    "session.tab.reopen",
    "session.page.up",
    "session.page.down",
    "session.line.up",
    "session.line.down",
    "session.half.page.up",
    "session.half.page.down",
    "session.aside",
    "plugins.list",
];

/// Base-mode command ids, in precedence order (session route, prompt, app).
const BASE: &[&str] = &[
    "session.queued_prompts",
    "session.interrupt",
    "session.background",
    "prompt.queue",
    "prompt.history.previous",
    "prompt.history.next",
    "prompt.clear",
    "prompt.paste",
    "prompt.editor",
    "prompt.images.view",
    "session.timeline",
    "session.rename",
    "session.compact",
    "session.undo",
    "session.redo",
    "session.export",
    "messages.copy",
    "session.child.first",
    "session.parent",
    "session.first",
    "session.last",
    "session.messages_last_user",
    "command.palette.show",
    "model.list",
    "model.cycle_recent",
    "model.cycle_recent_reverse",
    "model.cycle_favorite",
    "model.cycle_favorite_reverse",
    "agent.list",
    "agent.cycle",
    "agent.cycle.reverse",
    "variant.cycle",
    "variant.list",
    "mcp.list",
    "provider.connect",
    "opencode.settings",
    "opencode.status",
    "opencode.debug",
    "theme.switch",
    "theme.switch_mode",
    "theme.mode.lock",
    "help.show",
    "docs.open",
    "app.toggle.animations",
    "app.toggle.file_context",
    "app.toggle.diffwrap",
    "app.toggle.paste_summary",
    "terminal.suspend",
    "terminal.title.toggle",
    "session.toggle.thinking",
    "session.toggle.scrollbar",
    "session.toggle.exploration_grouping",
    "session.verbosity.cycle",
    "session.share",
    "session.copy",
    "session.copy.id",
    "session.fork",
    "session.message.next",
    "session.message.previous",
    "session.message.user.next",
    "session.message.user.previous",
    "app.exit",
];

const INPUT: &[&str] = &[
    "input.submit",
    "input.newline",
    "input.move.left",
    "input.move.right",
    "input.move.up",
    "input.move.down",
    "input.line.home",
    "input.line.end",
    "input.visual.line.home",
    "input.visual.line.end",
    "input.buffer.home",
    "input.buffer.end",
    "input.delete.line",
    "input.delete.to.line.end",
    "input.delete.to.line.start",
    "input.backspace",
    "input.delete",
    "input.undo",
    "input.redo",
    "input.word.forward",
    "input.word.backward",
    "input.delete.word.forward",
    "input.delete.word.backward",
];

impl App {
    fn prompt_width(&self) -> usize {
        let w = self.size.0 as usize;
        let pad = if w < 44 { 1 } else { 2 };
        let base = if self.session_id().is_some() { w.saturating_sub(if w < 44 { 2 } else { 4 }) } else { w.min(75 + 4).saturating_sub(4).min(75) };
        base.saturating_sub(1 + pad * 2).max(1)
    }

    fn prompt_active(&self) -> bool {
        !self.panel_has_focus()
            && !self.composer_open()
            && self.dialogs.is_empty() && self.permission_ui.is_none() && self.form_ui.is_none() && self.active_permission().is_none() && self.active_form().is_none()
    }

    pub fn active_permission(&self) -> Option<PermissionRequest> {
        if self.autoaccept {
            return None;
        }
        let sid = self.session_id()?;
        for id in self.store.family_of(sid).iter().chain(std::iter::once(&sid.to_string())) {
            if let Some(p) = self.store.permissions.get(id).and_then(|l| l.first()) {
                return Some(p.clone());
            }
        }
        None
    }

    pub fn active_form(&self) -> Option<(FormInfo, Option<LocationRef>)> {
        if let Some(sid) = self.session_id() {
            for id in self.store.family_of(sid).iter().chain(std::iter::once(&sid.to_string())) {
                if let Some(f) = self.store.forms.get(id).and_then(|l| l.first()) {
                    return Some(f.clone());
                }
            }
        }
        self.store.forms.get("global").and_then(|l| l.first()).cloned()
    }

    /// Entry point for a key press.
    pub fn on_key(&mut self, s: Stroke) {
        self.dirty = true;
        // Selection keys are intercepted before all bindings (priority 101).
        if self.text_selection_key(&s) {
            return;
        }
        if self.tab_menu.is_some() && self.tab_menu_key(&s.name, s.ctrl) {
            return;
        }
        if self.dialogs.is_empty() && self.terminal_key(&s) {
            return;
        }
        // Pending sequence handling.
        if !self.pending_keys.is_empty() {
            if s.name == "escape" && !s.ctrl {
                self.pending_keys.clear();
                self.pending_deadline = None;
                return;
            }
            if s.name == "backspace" && !s.ctrl {
                self.pending_keys.pop();
                if self.pending_keys.is_empty() {
                    self.pending_deadline = None;
                }
                return;
            }
        }
        let mut seq = self.pending_keys.clone();
        seq.push(s.clone());

        // Modal dialogs.
        if !self.dialogs.is_empty() && self.pending_keys.is_empty() {
            if s.name == "tab"
                && !s.ctrl
                && let Some(d) = self.dialogs.last_mut().filter(|d| d.purpose == "worktree-name" || d.purpose == "new-worktree")
                && let dialog::Body::Prompt(p) = &mut d.body
                && p.busy.is_none()
            {
                p.value = super::workspace::random_slug();
                p.cursor = p.value.len();
                self.dirty = true;
                return;
            }
            let kb = self.keybinds.clone();
            let outcome = {
                let d = self.dialogs.last_mut().unwrap();
                dialog::handle_key(d, &s, &kb)
            };
            if outcome != Outcome::None {
                self.dialog_outcome(outcome);
                return;
            }
        }
        // Plugin routes and a focused side panel own the keyboard (diff viewer bindings beat app.exit).
        let keyboard_owner = match self.route.clone() {
            Route::Plugin(name) => Some(name),
            _ if self.panel_has_focus() => self.active_panel().map(|p| p.name.clone()),
            _ => None,
        };
        if self.dialogs.is_empty()
            && let Some(name) = keyboard_owner
        {
            match name.as_str() {
                "diff" => match self.diff_key(&seq) {
                    Match::Full => {
                        self.pending_keys.clear();
                        self.pending_deadline = None;
                        return;
                    }
                    Match::Prefix => {
                        self.pending_keys = seq;
                        return;
                    }
                    Match::None => {
                        self.pending_keys.clear();
                        if s.name == "c" && s.ctrl {
                            self.close_diff();
                            return;
                        }
                    }
                },
                "stats" if s.name == "escape" => {
                    self.close_stats();
                    return;
                }
                _ => {}
            }
        }
        if self.pending_keys.is_empty() && self.dialogs.is_empty() && self.session_id().is_some() && self.composer_key(&s) {
            return;
        }
        if self.pending_keys.is_empty() && self.dialogs.is_empty() {
            // Autocomplete layer.
            if self.autocomplete.is_some() && self.autocomplete_key(&s) {
                return;
            }
            // Permission / form layers.
            if self.active_permission().is_some() && self.permission_key(&s) {
                return;
            }
            if self.active_form().is_some() && self.form_key(&s) {
                return;
            }
            // Shell mode inline keys.
            if self.editor.shell {
                if s.name == "escape" && !s.ctrl {
                    self.editor.shell = false;
                    return;
                }
                if (s.name == "c" && s.ctrl && self.editor.is_empty()) || (s.name == "backspace" && !s.ctrl && self.editor.cursor == 0) {
                    self.editor.shell = false;
                    return;
                }
            }
            if s.name == "!" && !self.editor.shell && self.editor.cursor == 0 && self.prompt_active() && s.is_char().is_some() {
                self.editor.shell = true;
                self.placeholder = rand::random_range(0..3);
                return;
            }
            // Narrow sidebar overlay.
            if (s.name == "escape" || (s.name == "c" && s.ctrl))
                && let Some(sid) = self.session_id().map(|x| x.to_string())
                && self.views.get(&sid).and_then(|v| v.sidebar_open) == Some(true)
                && self.size.0 <= 120
            {
                self.view(&sid).sidebar_open = Some(false);
                return;
            }
        }
        // Keybinding dispatch.
        let mut ids: Vec<&str> = GLOBAL.to_vec();
        if self.dialogs.is_empty() {
            ids.extend_from_slice(BASE);
            for i in 1..=10 {
                ids.push(Box::leak(format!("session.tab.select.{i}").into_boxed_str()));
            }
            for i in 1..=9 {
                ids.push(Box::leak(format!("session.quick_switch.{i}").into_boxed_str()));
            }
            if self.prompt_active() {
                ids.extend_from_slice(INPUT);
            }
        }
        let mut prefix = false;
        let kb = self.keybinds.clone();
        for id in ids {
            match kb.match_command(id, &seq) {
                Match::Full => {
                    if self.dispatch(id) {
                        self.pending_keys.clear();
                        self.pending_deadline = None;
                        return;
                    }
                }
                Match::Prefix => prefix = true,
                Match::None => {}
            }
        }
        if prefix {
            if self.pending_keys.is_empty() && kb.is_leader(&s) {
                // A huge `leader.timeout` from a hand-edited config never expires (instead of overflowing `Instant`).
                self.pending_deadline = Instant::now().checked_add(Duration::from_millis(kb.leader_timeout_ms));
            }
            self.pending_keys = seq;
            return;
        }
        // Sequence miss: clear and fall through (typed into the focused input).
        self.pending_keys.clear();
        self.pending_deadline = None;
        if let Some(c) = s.is_char() {
            if !self.dialogs.is_empty() {
                return;
            }
            if self.prompt_active() {
                self.editor.insert(&c.to_string());
                self.after_edit();
            }
        }
    }

    /// Run a keybinding-dispatched command; false means "not handled, continue".
    fn dispatch(&mut self, id: &str) -> bool {
        let w = self.prompt_width();
        if id.starts_with("input.") {
            if !self.prompt_active() {
                return false;
            }
            match id {
                "input.submit" => {
                    self.submit("steer");
                    return true;
                }
                "input.newline" => self.editor.insert("\n"),
                "input.move.left" => self.editor.left(),
                "input.move.right" => self.editor.right(),
                "input.move.up" => {
                    self.editor.up(w);
                }
                "input.move.down" => {
                    self.editor.down(w);
                }
                "input.line.home" | "input.visual.line.home" => self.editor.home(),
                "input.line.end" | "input.visual.line.end" => self.editor.end(),
                "input.buffer.home" => self.editor.buffer_home(),
                "input.buffer.end" => self.editor.buffer_end(),
                "input.delete.line" => self.editor.delete_line(),
                "input.delete.to.line.end" => self.editor.delete_to_line_end(),
                "input.delete.to.line.start" => self.editor.delete_to_line_start(),
                "input.backspace" => self.editor.backspace(),
                "input.delete" => self.editor.delete(),
                "input.undo" => self.editor.undo(),
                "input.redo" => self.editor.redo(),
                "input.word.forward" => self.editor.word_right(),
                "input.word.backward" => self.editor.word_left(),
                "input.delete.word.forward" => self.editor.delete_word_forward(),
                "input.delete.word.backward" => self.editor.delete_word_back(),
                _ => return false,
            }
            self.after_edit();
            return true;
        }
        match id {
            "prompt.history.previous" | "prompt.history.next" => {
                if !self.prompt_active() || self.autocomplete.is_some() {
                    return false;
                }
                let up = id.ends_with("previous");
                let (row, _) = self.editor.cursor_pos(w);
                let rows = self.editor.layout(w).len();
                if up {
                    if self.editor.cursor != 0 {
                        if row == 0 {
                            self.editor.buffer_home();
                        } else {
                            self.editor.up(w);
                        }
                        return true;
                    }
                } else if self.editor.cursor != self.editor.text.len() {
                    if row + 1 >= rows {
                        self.editor.buffer_end();
                    } else {
                        self.editor.down(w);
                    }
                    return true;
                }
                let text = self.editor.text.clone();
                match self.history.step(if up { -1 } else { 1 }, &text) {
                    Some(p) => {
                        self.editor.set(&p);
                        if up {
                            self.editor.cursor = 0;
                        }
                        true
                    }
                    None => false,
                }
            }
            "prompt.clear" => {
                if self.editor.is_empty() || !self.prompt_active() {
                    return false;
                }
                self.run_command("prompt.clear", None)
            }
            "prompt.paste" => {
                if !self.prompt_active() {
                    return false;
                }
                self.run_command("prompt.paste", None)
            }
            "prompt.queue" | "prompt.editor" | "prompt.images.view" => {
                if !self.prompt_active() {
                    return false;
                }
                self.run_command(id, None)
            }
            "app.exit" => {
                if !self.editor.is_empty() && self.prompt_active() {
                    return false;
                }
                self.quit = true;
                true
            }
            "session.child.first" => {
                if self.session_id().is_none() || !self.prompt_active() {
                    return false;
                }
                self.run_command(id, None)
            }
            "session.first" | "session.last" => {
                if self.prompt_active() && !self.editor.is_empty() {
                    return false;
                }
                self.run_command(id, None)
            }
            "session.parent" => {
                if !self.prompt_active() || !self.editor.is_empty() {
                    return false;
                }
                self.run_command(id, None)
            }
            "session.redo" => {
                if self.session().is_none_or(|s| s.revert.is_none()) {
                    return false;
                }
                self.run_command(id, None)
            }
            "session.queued_prompts" => {
                if self.queued_prompts().is_empty() {
                    return false;
                }
                self.run_command(id, None)
            }
            "session.tab.next" | "session.tab.previous" | "session.tab.close" | "session.tab.reopen" | "session.tab.next_unread" | "session.tab.previous_unread" => {
                if !self.config.tabs_enabled() {
                    return false;
                }
                self.run_command(id, None)
            }
            _ if id.starts_with("session.tab.select.") => self.config.tabs_enabled() && self.run_command(id, None),
            _ if id.starts_with("session.quick_switch.") => !self.config.tabs_enabled() && self.run_command(id, None),
            _ => self.run_command(id, None),
        }
    }

    pub fn interrupt_key(&mut self) -> bool {
        if !self.running() || self.autocomplete.is_some() || !self.prompt_active() {
            return false;
        }
        if self.editor.shell {
            self.editor.shell = false;
            return true;
        }
        self.interrupt += 1;
        self.interrupt_at = Some(Instant::now());
        if self.interrupt >= 2 {
            self.interrupt = 0;
            if let Some(sid) = self.session_id().map(|s| s.to_string()) {
                let c = self.client.clone();
                self.call(async move { c.interrupt(&sid, true).await }, |_, _| {});
            }
        }
        true
    }

    pub fn scroll(&mut self, up: bool, amount: usize) {
        let Some(sid) = self.session_id().map(|s| s.to_string()) else { return };
        let slack = self.views.get(&sid).map(|v| v.nav_slack).unwrap_or(0);
        let max = self.transcript_lines.saturating_sub(self.transcript_rect.height as usize + slack);
        let v = self.view(&sid);
        v.nav_slack = 0;
        if up {
            v.scroll_from_bottom = (v.scroll_from_bottom.saturating_add(amount)).min(max);
        } else {
            v.scroll_from_bottom = v.scroll_from_bottom.saturating_sub(amount);
        }
        let at_top = self.views.get(&sid).is_some_and(|v| v.scroll_from_bottom >= max);
        if up && at_top {
            self.fetch_older(&sid);
        }
        self.dirty = true;
    }

    /// Scroll so transcript line `top` is at the viewport top (adds navigation slack).
    pub fn jump_to_line(&mut self, top: usize) {
        self.transcript_complete();
        let Some(sid) = self.session_id().map(|s| s.to_string()) else { return };
        let h = self.transcript_rect.height as usize;
        let slack_before = self.views.get(&sid).map(|v| v.nav_slack).unwrap_or(0);
        let lines = self.transcript_lines.saturating_sub(slack_before);
        let slack = (top + h).saturating_sub(lines);
        let total = lines + slack;
        let v = self.view(&sid);
        v.nav_slack = slack;
        v.scroll_from_bottom = total.saturating_sub(h).saturating_sub(top);
        self.dirty = true;
    }

    fn current_top(&self) -> usize {
        let Some(sid) = self.session_id() else { return 0 };
        let h = self.transcript_rect.height as usize;
        let from_bottom = self.views.get(sid).map(|v| v.scroll_from_bottom).unwrap_or(0);
        self.transcript_lines.saturating_sub(h).saturating_sub(from_bottom)
    }

    /// `session.message.*` navigation between message boundaries.
    pub fn navigate_message(&mut self, id: &str) -> bool {
        self.transcript_complete();
        let top = self.current_top();
        let user_only = id.contains(".user.") || id == "session.messages_last_user";
        let marks: Vec<usize> = self.transcript_marks().iter().filter(|(_, _, u)| !user_only || *u).map(|(l, _, _)| *l).collect();
        let target = match id {
            "session.messages_last_user" => marks.last().copied(),
            _ if id.ends_with("next") => marks.iter().copied().find(|l| *l > top),
            _ => marks.iter().rev().copied().find(|l| *l < top),
        };
        match target {
            Some(t) => {
                self.jump_to_line(t);
                true
            }
            None => {
                if id.ends_with("next") {
                    if let Some(sid) = self.session_id().map(|s| s.to_string()) {
                        let v = self.view(&sid);
                        v.nav_slack = 0;
                        v.scroll_from_bottom = 0;
                    }
                }
                false
            }
        }
    }

    /// Jump to a specific message (timeline / "Jump to").
    pub fn jump_to_message(&mut self, message: &str) {
        self.transcript_complete();
        if let Some((l, _, _)) = self.transcript_marks().iter().find(|(_, m, _)| m == message).cloned() {
            self.jump_to_line(l);
        }
    }

    // ---- paste -------------------------------------------------------------

    pub fn paste(&mut self, text: &str) {
        self.dirty = true;
        if let Some(d) = self.dialogs.last_mut() {
            dialog::paste(d, text);
            return;
        }
        if let Some(f) = self.form_ui.as_mut()
            && f.editing
        {
            f.text.push_str(text);
            return;
        }
        if !self.prompt_active() {
            return;
        }
        let normalized = crate::util::locale::strip_ansi(text).replace("\r\n", "\n").replace('\r', "\n");
        if self.paste_files(&normalized) {
            return;
        }
        let compact = self.config.prompt_paste() != "full";
        self.editor.paste(&normalized, compact);
        self.after_edit();
    }

    /// File-path paste: images/PDF become attachment chips, SVG becomes a pasted-text chip.
    fn paste_files(&mut self, text: &str) -> bool {
        let trimmed = text.trim();
        if trimmed.is_empty() || trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            return false;
        }
        let candidates: Vec<String> = {
            let single = trimmed.trim_matches(|c| c == '\'' || c == '"').to_string();
            let single = single.strip_prefix("file://").map(|s| s.to_string()).unwrap_or(single).replace("\\ ", " ");
            if std::path::Path::new(&single).is_file() {
                vec![single]
            } else {
                let parts: Vec<String> = trimmed.split_whitespace().map(|s| s.trim_matches(|c| c == '\'' || c == '"').to_string()).collect();
                if parts.len() <= 32 && parts.iter().all(|p| std::path::Path::new(p).is_file()) { parts } else { vec![] }
            }
        };
        if candidates.is_empty() {
            return false;
        }
        let mut total = 0u64;
        let mut items = Vec::new();
        for p in &candidates {
            let ext = p.rsplit('.').next().unwrap_or("").to_lowercase();
            let mime = match ext.as_str() {
                "avif" => "image/avif",
                "gif" => "image/gif",
                "jpeg" | "jpg" => "image/jpeg",
                "png" => "image/png",
                "webp" => "image/webp",
                "pdf" => "application/pdf",
                "svg" => "image/svg+xml",
                _ => return false,
            };
            let Ok(bytes) = std::fs::read(p) else { return false };
            total += bytes.len() as u64;
            if total > 20 * 1024 * 1024 {
                return false;
            }
            items.push((p.clone(), mime, bytes));
        }
        use base64::Engine;
        for (p, mime, bytes) in items {
            let name = p.rsplit('/').next().unwrap_or(&p).to_string();
            if mime == "image/svg+xml" {
                let label = format!("[SVG: {name}]");
                self.editor.insert_chip(&label, ChipKind::Paste, json!({ "text": String::from_utf8_lossy(&bytes) }), true);
                continue;
            }
            let kind = if mime == "application/pdf" { "PDF" } else { "Image" };
            let label = self.editor.next_attachment_label(kind);
            let uri = format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes));
            self.editor.insert_chip(&label, ChipKind::Paste, json!({ "uri": uri, "name": name, "mime": mime }), true);
        }
        true
    }

    // ---- autocomplete ------------------------------------------------------

    pub fn after_edit(&mut self) {
        self.dirty = true;
        if self.editor.shell {
            self.autocomplete = None;
            return;
        }
        let text = self.editor.text.clone();
        if self.dismissed_text.as_deref() == Some(text.as_str()) {
            return;
        }
        self.dismissed_text = None;
        let cursor = self.editor.cursor;
        let before = &text[..cursor];
        // /cd directory argument mode.
        if let Some(rest) = before.strip_prefix("/cd ") {
            let query = rest.to_string();
            self.open_autocomplete("directory", 4, &query);
            return;
        }
        if let Some(ac) = &self.autocomplete {
            let trig = ac.index;
            if cursor <= trig || text.get(trig..cursor).is_none_or(|s| s.contains(char::is_whitespace)) {
                self.autocomplete = None;
            }
        }
        if cursor == 0 {
            self.autocomplete = None;
            return;
        }
        // `/` trigger: only at the start of the prompt.
        if let Some(q) = before.strip_prefix('/')
            && !q.contains(char::is_whitespace)
            && !q.contains('/')
        {
            self.open_autocomplete("command", 0, q);
            return;
        }
        if let Some(i) = before.rfind('@') {
            let ok_start = i == 0 || before[..i].ends_with(char::is_whitespace);
            let q = &before[i + 1..];
            if ok_start && !q.contains(char::is_whitespace) {
                self.open_autocomplete("reference", i, &q.to_string());
                return;
            }
        }
        self.autocomplete = None;
    }

    fn open_autocomplete(&mut self, mode: &str, index: usize, query: &str) {
        let mut ac = Autocomplete { mode: mode.into(), index, query: query.into(), ..Default::default() };
        match mode {
            "command" => {
                if index == 0 {
                    ac.items = self.slash_items(query);
                }
            }
            "reference" => {
                ac.items = self.reference_items(query);
                ac.loading = true;
                let c = self.client.clone();
                let base = query.split('#').next().unwrap_or("").to_string();
                let dir = self.location_dir();
                let q = query.to_string();
                self.spawn(async move { c.file_find(&base, 20, Some(&dir)).await }, move |app, r| {
                    let dir = app.location_dir();
                    let Some(ac) = app.autocomplete.as_mut() else { return };
                    if ac.mode != "reference" || ac.query != q {
                        return;
                    }
                    ac.loading = false;
                    match r {
                        Ok(entries) => {
                            let range = q.split_once('#').map(|(_, r)| format!("#{r}")).unwrap_or_default();
                            for e in entries {
                                if ac.items.len() >= 10 {
                                    break;
                                }
                                let directory = e.kind == "directory";
                                ac.items.push(AcItem {
                                    display: format!("{}{}", e.path, if directory { "/" } else { "" }),
                                    value: format!("{}{range}", e.path),
                                    kind: "file".into(),
                                    directory,
                                    data: json!({ "uri": format!("file://{}/{}", dir.trim_end_matches('/'), e.path), "name": e.path }),
                                    ..Default::default()
                                });
                            }
                        }
                        Err(_) => ac.error = true,
                    }
                    app.dirty = true;
                });
            }
            "directory" => {
                ac.loading = true;
                let c = self.client.clone();
                let dir = self.location_dir();
                let q = query.to_string();
                let (parent, leaf) = match q.rfind('/') {
                    Some(i) => (q[..=i].to_string(), q[i + 1..].to_string()),
                    None => (String::new(), q.clone()),
                };
                let path = if parent.starts_with('~') {
                    format!("{}{}", dirs::home_dir().map(|h| h.to_string_lossy().to_string()).unwrap_or_default(), &parent[1..])
                } else {
                    parent.clone()
                };
                self.spawn(async move { c.file_list(if path.is_empty() { None } else { Some(&path) }, Some(&dir)).await }, move |app, r| {
                    let Some(ac) = app.autocomplete.as_mut() else { return };
                    if ac.mode != "directory" || ac.query != q {
                        return;
                    }
                    ac.loading = false;
                    match r {
                        Ok((_, entries)) => {
                            for e in entries.into_iter().filter(|e| e.kind == "directory") {
                                let name = e.path.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string();
                                if name.starts_with('.') && leaf.is_empty() {
                                    continue;
                                }
                                if !name.to_lowercase().starts_with(&leaf.to_lowercase()) {
                                    continue;
                                }
                                ac.items.push(AcItem { display: format!("{parent}{name}/"), value: format!("{parent}{name}/"), kind: "directory".into(), directory: true, ..Default::default() });
                            }
                        }
                        Err(_) => ac.error = true,
                    }
                    app.dirty = true;
                });
            }
            _ => {}
        }
        self.autocomplete = Some(ac);
    }

    fn slash_items(&self, query: &str) -> Vec<AcItem> {
        let mut items: Vec<AcItem> = Vec::new();
        for c in self.commands() {
            let Some(name) = c.slash else { continue };
            for n in std::iter::once(name).chain(c.aliases.iter().copied()) {
                items.push(AcItem {
                    display: format!("/{n}"),
                    description: Some(c.description.clone().unwrap_or_else(|| c.title.clone())),
                    value: c.id.clone(),
                    kind: if c.args { "command-args".into() } else { "command".into() },
                    data: json!({ "name": n }),
                    ..Default::default()
                });
            }
        }
        for c in self.loc().and_then(|l| l.commands.clone()).unwrap_or_default() {
            items.push(AcItem { display: format!("/{}", c.name), description: c.description.clone(), value: c.name.clone(), kind: "server".into(), data: json!({ "name": c.name }), ..Default::default() });
        }
        items.sort_by(|a, b| a.display.cmp(&b.display));
        let max = items.iter().map(|i| crate::util::locale::width(&i.display)).max().unwrap_or(0);
        for i in items.iter_mut() {
            i.display = format!("{:<w$}", i.display, w = max + 2);
        }
        if query.is_empty() {
            return items;
        }
        let mut scored: Vec<(f64, AcItem)> = items
            .into_iter()
            .filter_map(|i| {
                let d = dialog::fuzzy(query, i.display.trim().trim_start_matches('/'));
                let s = i.description.as_deref().and_then(|x| dialog::fuzzy(query, x));
                let best = d.unwrap_or(0.0).max(s.unwrap_or(0.0) * 0.5);
                if d.is_none() && s.is_none() {
                    return None;
                }
                let bonus = if i.display.trim().starts_with(&format!("/{query}")) { 2.0 } else { 1.0 };
                Some((best * bonus, i))
            })
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().take(10).map(|(_, i)| i).collect()
    }

    fn reference_items(&self, query: &str) -> Vec<AcItem> {
        let mut items = Vec::new();
        let loc = self.loc();
        for s in loc.and_then(|l| l.skills.clone()).unwrap_or_default() {
            items.push(AcItem { display: format!("@{}", s.id), description: s.description.clone(), label: Some("skill".into()), value: s.id.clone(), kind: "skill".into(), data: json!({ "id": s.id, "name": s.name }), ..Default::default() });
        }
        for r in loc.and_then(|l| l.references.clone()).unwrap_or_default().into_iter().filter(|r| !r.hidden) {
            items.push(AcItem {
                display: format!("@{}", r.name),
                description: Some(r.description.clone().unwrap_or(r.path.clone())),
                label: Some("reference".into()),
                value: r.name.clone(),
                kind: "reference".into(),
                data: json!({ "uri": format!("file://{}", r.path), "name": r.name }),
                ..Default::default()
            });
        }
        for a in self.all_agents().into_iter().filter(|a| !a.hidden && a.mode != "primary") {
            items.push(AcItem { display: format!("@{}", a.id), label: Some("agent".into()), value: a.id.clone(), kind: "agent".into(), data: json!({ "name": a.id }), ..Default::default() });
        }
        if query.is_empty() {
            return items;
        }
        let mut scored: Vec<(f64, AcItem)> = items
            .into_iter()
            .filter_map(|i| {
                let s = dialog::fuzzy(query, &i.value)?;
                if s < 0.5 {
                    return None;
                }
                let bonus = if i.display.starts_with(&format!("@{query}")) { 2.0 } else { 1.0 };
                Some((s * bonus, i))
            })
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().take(10).map(|(_, i)| i).collect()
    }

    fn autocomplete_key(&mut self, s: &Stroke) -> bool {
        let kb = self.keybinds.clone();
        let is = |id: &str| kb.match_command(id, std::slice::from_ref(s)) == Match::Full;
        let n = self.autocomplete.as_ref().map(|a| a.items.len()).unwrap_or(0);
        if is("prompt.autocomplete.prev") {
            if let Some(a) = self.autocomplete.as_mut()
                && n > 0
            {
                a.selected = (a.selected + n - 1) % n;
                a.armed = None;
            }
            return true;
        }
        if is("prompt.autocomplete.next") {
            if let Some(a) = self.autocomplete.as_mut()
                && n > 0
            {
                a.selected = (a.selected + 1) % n;
                a.armed = None;
            }
            return true;
        }
        if is("prompt.autocomplete.hide") {
            self.dismissed_text = Some(self.editor.text.clone());
            self.autocomplete = None;
            return true;
        }
        if is("prompt.clear") {
            let ac = self.autocomplete.take().unwrap();
            if ac.mode == "command" {
                let end = self.editor.cursor;
                self.editor.delete_range(ac.index, end);
            }
            self.dismissed_text = Some(self.editor.text.clone());
            return true;
        }
        if is("prompt.autocomplete.select") {
            self.autocomplete_select(false);
            return true;
        }
        if is("prompt.autocomplete.complete") {
            self.autocomplete_select(true);
            return true;
        }
        if s.name == "d" && s.ctrl {
            return true;
        }
        if is("prompt.queue") {
            let is_server = self.autocomplete.as_ref().and_then(|a| a.items.get(a.selected)).is_some_and(|i| i.kind == "server");
            if is_server {
                self.autocomplete_select(false);
                self.submit("queue");
            }
            return true;
        }
        false
    }

    fn autocomplete_select(&mut self, complete: bool) {
        let Some(ac) = self.autocomplete.clone() else { return };
        let Some(item) = ac.items.get(ac.selected).cloned() else {
            self.autocomplete = None;
            return;
        };
        let cursor = self.editor.cursor;
        match ac.mode.as_str() {
            "command" => {
                self.autocomplete = None;
                self.editor.delete_range(ac.index, cursor);
                let name = item.data.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                if item.kind == "command" {
                    self.run_command(&item.value, None);
                } else {
                    let end = self.editor.cursor;
                    self.editor.replace_range(0, end, &format!("/{name} "));
                    self.dismissed_text = Some(self.editor.text.clone());
                }
            }
            "directory" => {
                self.editor.replace_range(4, cursor, &item.value);
                if complete && item.directory {
                    self.after_edit();
                } else {
                    self.autocomplete = None;
                    self.dismissed_text = Some(self.editor.text.clone());
                }
            }
            _ => {
                if complete && item.directory {
                    self.editor.replace_range(ac.index, cursor, &format!("@{}", item.display));
                    self.after_edit();
                    return;
                }
                self.autocomplete = None;
                self.editor.delete_range(ac.index, cursor);
                let (kind, text) = match item.kind.as_str() {
                    "skill" => {
                        if self.editor.chips.iter().any(|c| c.kind == ChipKind::Skill && c.data.get("id") == item.data.get("id")) {
                            return;
                        }
                        (ChipKind::Skill, format!("@{}", item.value))
                    }
                    "agent" => (ChipKind::Agent, format!("@{}", item.value)),
                    _ => (ChipKind::File, format!("@{}", item.value)),
                };
                let mut data = item.data.clone();
                if let Some((_, range)) = item.value.split_once('#') {
                    let (start, end) = match range.split_once('-') {
                        Some((a, b)) => (a.to_string(), Some(b.to_string())),
                        None => (range.to_string(), None),
                    };
                    let uri = data.get("uri").and_then(|u| u.as_str()).unwrap_or("").to_string();
                    let mut q = format!("{uri}?start={start}");
                    if let Some(e) = end.filter(|e| e.parse::<i64>().unwrap_or(0) > start.parse::<i64>().unwrap_or(0)) {
                        q.push_str(&format!("&end={e}"));
                    }
                    data["uri"] = json!(q);
                }
                if kind == ChipKind::File {
                    crate::state::frecency_touch(&crate::util::paths::state_dir().join("frecency.jsonl"), &item.value);
                }
                self.editor.insert_chip(&text, kind, data, true);
            }
        }
        self.dirty = true;
    }

    // ---- submission --------------------------------------------------------

    pub fn submit(&mut self, delivery: &str) {
        if self.autocomplete.is_some() {
            return;
        }
        let text = self.editor.text.clone();
        let trimmed = text.trim().to_string();
        let shell = self.editor.shell;
        let sid = self.session_id().map(|s| s.to_string());
        if trimmed.is_empty() {
            if sid.is_none() || shell || delivery == "queue" {
                return;
            }
            // Empty Enter steers the first queued prompt.
            if let (Some(s), Some(q)) = (sid.clone(), self.queued_prompts().first().cloned()) {
                let c = self.client.clone();
                self.call(async move { c.inbox_update(&s, &q.id, "steer").await }, |_, _| {});
            }
            return;
        }
        if matches!(trimmed.as_str(), "exit" | "quit" | ":q") {
            if delivery == "queue" {
                self.toast(Toast::new("warning", "This prompt cannot be queued"));
                return;
            }
            self.quit = true;
            return;
        }
        let input_text = self.editor.input_text();
        // Argument slash commands.
        if let Some(rest) = trimmed.strip_prefix('/') {
            let (name, args) = rest.split_once(' ').map(|(a, b)| (a.to_string(), Some(b.to_string()))).unwrap_or((rest.to_string(), None));
            if let Some(c) = self.commands().into_iter().find(|c| c.args && (c.slash == Some(name.as_str()) || c.aliases.contains(&name.as_str()))) {
                if delivery == "queue" {
                    self.toast(Toast::new("warning", "This prompt cannot be queued"));
                    return;
                }
                self.editor.clear();
                self.run_command(&c.id, Some(args.unwrap_or_default()));
                return;
            }
        }
        if shell && delivery == "queue" {
            self.toast(Toast::new("warning", "This prompt cannot be queued"));
            return;
        }
        let Some(agent) = self.current_agent() else {
            self.toast(Toast::new("warning", "Connect an integration to send prompts").duration(3000));
            return;
        };
        let model = self.current_model();
        if model.is_none() && !shell {
            self.toast(Toast::new("warning", "Connect an integration to send prompts").duration(3000));
            return;
        }
        if let Some(m) = &model
            && !shell
            && !self.model_valid(m)
        {
            self.toast(Toast::new("warning", format!("{}/{} is not available in this session's location", m.provider_id, m.id)).title("Model unavailable"));
            return;
        }
        let mut entry = self.editor.info();
        entry.mode = Some(if shell { "shell".into() } else { "normal".into() });
        let (files, agents, skills) = self.editor.api_parts();
        self.history.append(entry);
        self.editor = crate::ui::prompt::Editor::new();
        self.autocomplete = None;
        if let Some(s) = sid.clone() {
            self.view(&s).scroll_from_bottom = 0;
        }
        let variant = self.current_variant();
        let model_ref = model.map(|mut m| {
            m.variant = variant.clone();
            m
        });
        // Create the session first when on home.
        let (sid, created) = match sid {
            Some(s) => (s, false),
            None => {
                let id = crate::util::session_id();
                let location = LocationRef { directory: self.location_dir(), workspace_id: None };
                self.store.admit_session(&id, location.clone(), Some(agent.id.clone()), model_ref.clone());
                (id, true)
            }
        };
        let c = self.client.clone();
        let agent_id = agent.id.clone();
        let session_agent = self.store.session(&sid).and_then(|s| s.agent.clone());
        let session_model = self.store.session(&sid).and_then(|s| s.model.clone());
        let env = self.opts.environment.clone();
        let dir = self.location_dir();
        // Server command?
        let server_cmd = trimmed.strip_prefix('/').and_then(|r| {
            let name = r.split_whitespace().next().unwrap_or("");
            self.loc().and_then(|l| l.commands.clone()).unwrap_or_default().into_iter().find(|c| c.name == name).map(|c| (c.name, r[name.len()..].trim().to_string()))
        });
        let msg_id = crate::util::message_id();
        if !shell && server_cmd.is_none() {
            self.store.admit_prompt(&sid, &msg_id, &input_text, json!(agents), delivery);
        }
        self.turn_begin(&sid);
        let delivery = delivery.to_string();
        let sid2 = sid.clone();
        let sid_nav = sid.clone();
        let mid2 = msg_id.clone();
        let revert = self.store.session(&sid).and_then(|s| s.revert.clone()).is_some();
        let model2 = model_ref.clone();
        self.spawn(
            async move {
                if created {
                    let mut body = json!({ "id": sid2, "agent": agent_id, "location": { "directory": dir } });
                    if let Some(m) = &model2 {
                        body["model"] = serde_json::to_value(m)?;
                    }
                    c.session_create(body).await.map_err(|e| anyhow::anyhow!("Creating a session failed: {}", crate::api::error_message(&e)))?;
                    if !env.is_empty() {
                        c.environment(&sid2, serde_json::to_value(&env)?).await?;
                    }
                }
                if shell {
                    c.shell(&sid2, &input_text).await?;
                    return Ok(());
                }
                if session_agent.as_deref() != Some(agent_id.as_str()) && !created {
                    c.switch_agent(&sid2, &agent_id).await?;
                }
                if let Some(m) = &model2
                    && session_model.as_ref() != Some(m)
                    && !created
                {
                    c.switch_model(&sid2, m).await.map_err(|e| anyhow::anyhow!("Failed to switch model: {}", crate::api::error_message(&e)))?;
                }
                if let Some((name, args)) = server_cmd {
                    let mut body = json!({ "name": name, "text": args, "delivery": delivery });
                    if !files.is_empty() {
                        body["files"] = json!(files);
                    }
                    if !agents.is_empty() {
                        body["agents"] = json!(agents);
                    }
                    if !skills.is_empty() {
                        body["skills"] = json!(skills);
                    }
                    c.command(&sid2, body).await?;
                    return Ok(());
                }
                if revert {
                    c.revert_commit(&sid2).await?;
                }
                let mut body = json!({ "id": mid2, "text": input_text, "delivery": delivery });
                if !files.is_empty() {
                    body["files"] = json!(files);
                }
                if !agents.is_empty() {
                    body["agents"] = json!(agents);
                }
                if !skills.is_empty() {
                    body["skills"] = json!(skills);
                }
                c.prompt(&sid2, body).await?;
                Ok(())
            },
            move |app, r| {
                app.turn_acked(&sid, r.is_ok(), shell);
                if let Err(e) = r {
                    app.store.rollback_prompt(&sid, &msg_id);
                    if created && app.store.outbox.remove(&sid) {
                        app.store.remove_session(&sid);
                        if app.session_id() == Some(sid.as_str()) {
                            app.navigate(Route::Home);
                        }
                    }
                    let title = if shell && !created { "Failed to run shell command" } else { "Failed to send prompt" };
                    app.toast(Toast::new("error", crate::api::error_message(&e)).title(title));
                }
            },
        );
        if let Some(m) = &model_ref {
            self.models.add_recent(&m.provider_id, &m.id);
        }
        if created {
            self.navigate(Route::Session(sid_nav));
        }
    }

    // ---- dialogs -----------------------------------------------------------

    pub fn close_dialog(&mut self) {
        if let Some(d) = self.dialogs.pop()
            && d.purpose == "theme"
            && let Some(initial) = self.theme_preview_initial.take()
        {
            self.set_theme(&initial, false);
        }
        self.dirty = true;
    }

    pub fn dialog_outcome(&mut self, o: Outcome) {
        let Some(d) = self.dialogs.last().cloned() else { return };
        let purpose = d.purpose.clone();
        match o {
            Outcome::None | Outcome::Handled => {}
            Outcome::Back if purpose == "open-worktrees" => {
                self.dialogs.clear();
                self.open_open_dialog();
            }
            Outcome::Close | Outcome::Back => self.close_dialog(),
            Outcome::Filter(f) => {
                if purpose == "palette"
                    && let Some(sel) = self.dialogs.last_mut().and_then(|d| d.sel())
                {
                    for o in sel.options.iter_mut() {
                        if o.value.starts_with("setting:") {
                            o.disabled = f.is_empty();
                        }
                        if o.value.starts_with("suggested:") {
                            o.disabled = !f.is_empty();
                        }
                    }
                }
                if purpose == "model"
                    && let Some(sel) = self.dialogs.last_mut().and_then(|d| d.sel())
                {
                    // Favorites and Recent sections only show without a search; the full list then holds them.
                    let sectioned: Vec<String> = sel.options.iter().filter_map(|o| o.value.strip_prefix("section:").map(|v| v.to_string())).collect();
                    for o in sel.options.iter_mut() {
                        if o.value.starts_with("section:") {
                            o.disabled = !f.is_empty();
                        } else if sectioned.contains(&o.value) {
                            o.disabled = f.is_empty();
                        }
                    }
                    sel.focus_current_option();
                }
                if purpose == "theme" {
                    let first = self.dialogs.last_mut().and_then(|d| d.sel()).and_then(|s| s.selected_value());
                    match (f.is_empty(), first) {
                        (true, _) => {
                            if let Some(i) = self.theme_preview_initial.clone() {
                                self.set_theme(&i, false);
                            }
                        }
                        (false, Some(v)) => {
                            self.set_theme(&v, false);
                        }
                        _ => {}
                    }
                }
            }
            Outcome::Moved(v) => {
                if purpose == "timeline" {
                    self.jump_to_message(&v);
                }
                if purpose == "theme" {
                    self.set_theme(&v, false);
                }
                if let Some(sel) = self.dialogs.last_mut().and_then(|d| d.sel()) {
                    // Any cursor move cancels a pending delete confirmation.
                    for o in sel.options.iter_mut() {
                        o.bg = None;
                        o.fg = None;
                    }
                }
                if purpose == "session" || purpose == "stash" {
                    self.reset_confirm();
                }
                if purpose == "worktrees"
                    && let Some(d) = self.dialogs.last_mut()
                    && let Some(armed) = d.data.get("armed").and_then(|a| a.as_str()).map(|s| s.to_string())
                {
                    if let Some(o) = d.data.as_object_mut() {
                        o.remove("armed");
                    }
                    if let Some(sel) = d.sel()
                        && let Some(o) = sel.options.iter_mut().find(|o| o.value == armed)
                    {
                        o.title = crate::util::locale::abbreviate_home(&armed[2..]);
                    }
                }
            }
            Outcome::Select(v) => self.dialog_select(&purpose, &v, &d),
            Outcome::Submit(text) => self.dialog_submit(&purpose, &text, &d),
            Outcome::Confirm(yes) if purpose == "worktree-force" => {
                self.close_dialog();
                if yes {
                    let project = d.data.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                    let dir = d.data.get("dir").and_then(|i| i.as_str()).unwrap_or("").to_string();
                    let c = self.client.clone();
                    let p = project.clone();
                    self.spawn(async move { c.worktree_remove(&p, &dir, true).await }, move |app, r| match r {
                        Ok(_) => {
                            if app.dialogs.last().is_some_and(|d| d.purpose == "worktrees") {
                                app.worktree_action("dialog.move_session.refresh", None);
                            }
                        }
                        Err(e) => app.toast(Toast::new("error", crate::api::error_message(&e)).title("Failed to delete worktree")),
                    });
                }
            }
            Outcome::Confirm(_) => self.close_dialog(),
            Outcome::Action(id, v) => self.dialog_action(&purpose, &id, v),
            Outcome::Key(k) => self.dialog_key(&purpose, &k, &d),
        }
        self.dirty = true;
    }

    /// Purpose-specific keys of text dialogs (`Outcome::Key`).
    fn dialog_key(&mut self, purpose: &str, key: &str, d: &dialog::Dialog) {
        match (purpose, key) {
            ("export-result", "return") => self.close_dialog(),
            ("debug", "return") => {
                let text = d.data.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                crate::clipboard::write(&text);
                self.toast(Toast::new("info", "Debug info copied to clipboard"));
                let entries: Vec<(String, String)> = d
                    .data
                    .get("entries")
                    .and_then(|e| e.as_array())
                    .map(|a| a.iter().filter_map(|p| Some((p.get(0)?.as_str()?.to_string(), p.get(1)?.as_str()?.to_string()))).collect())
                    .unwrap_or_default();
                let entries: Vec<(&str, String)> = entries.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                let body = super::commands::debug_dialog(self, &entries, true);
                if let Some(top) = self.dialogs.last_mut().filter(|x| x.purpose == "debug") {
                    top.body = Body::Text(Box::new(body));
                }
            }
            ("btw-answer", "c") => {
                let answer = d.data.get("answer").and_then(|t| t.as_str()).unwrap_or("").to_string();
                let question = d.data.get("question").and_then(|t| t.as_str()).unwrap_or("").to_string();
                crate::clipboard::write(&answer);
                let scroll = match &d.body {
                    Body::Text(td) => td.scroll,
                    _ => 0,
                };
                let mut nd = super::commands::btw_answer(self, &question, &answer, true);
                if let Some(td) = nd.text_body() {
                    td.scroll = scroll;
                }
                if let Some(top) = self.dialogs.last_mut().filter(|x| x.purpose == "btw-answer") {
                    *top = nd;
                }
            }
            _ => {}
        }
    }

    fn reset_confirm(&mut self) {
        if let Some(d) = self.dialogs.last_mut() {
            d.data = Value::Null;
        }
    }

    fn dialog_select(&mut self, purpose: &str, v: &str, d: &dialog::Dialog) {
        match purpose {
            "palette" => {
                self.dialogs.clear();
                if let Some(path) = v.strip_prefix("setting:") {
                    let p = path.to_string();
                    self.open_settings(Some(&p));
                    return;
                }
                let id = v.strip_prefix("suggested:").unwrap_or(v).to_string();
                self.run_command(&id, None);
            }
            "agent" => {
                self.dialogs.clear();
                self.set_agent(v);
            }
            "variant" => {
                self.dialogs.clear();
                if let Some(mut m) = self.current_model() {
                    self.models.set_variant(&format!("{}/{}", m.provider_id, m.id), Some(v));
                    m.variant = if v == "default" { None } else { Some(v.into()) };
                    self.set_model(m, false);
                }
            }
            "model" => {
                self.dialogs.clear();
                if let Some((p, m)) = v.strip_prefix("section:").unwrap_or(v).split_once('/') {
                    let mref = ModelRef { provider_id: p.into(), id: m.into(), variant: None };
                    self.set_model(mref, true);
                    let info = self.current_model_info();
                    if let Some(info) = info
                        && !info.variants.is_empty()
                        && self.current_variant().is_none_or(|cv| !info.variants.iter().any(|x| x.id == cv))
                    {
                        self.run_command("variant.list", None);
                    }
                }
            }
            "theme" => {
                self.theme_preview_initial = None;
                self.dialogs.clear();
                self.set_theme(v, true);
            }
            "settings" => self.change_setting(v, 1),
            "session" | "navigate" => {
                self.dialogs.clear();
                self.navigate(Route::Session(v.to_string()));
            }
            "timeline" => {
                self.jump_to_message(v);
                let mut opts = Vec::new();
                for (t, val, desc) in [("Jump to", "jump", "view message in session"), ("Revert", "revert", "undo messages and file changes"), ("Copy", "copy", "message text to clipboard"), ("Fork", "fork", "create a new session")] {
                    opts.push(dialog::Opt::new(t, val).desc(desc));
                }
                let mut nd = dialog::Dialog::select("message", dialog::Select::new("Message Actions", opts), dialog::Size::Medium);
                nd.data = json!(v);
                self.open_dialog(nd);
            }
            "message" => {
                let mid = d.data.as_str().unwrap_or("").to_string();
                let Some(sid) = self.session_id().map(|s| s.to_string()) else { return };
                let msg = self.store.messages(&sid).iter().find(|m| m.id == mid).cloned();
                self.dialogs.clear();
                match v {
                    "jump" => self.jump_to_message(&mid),
                    "copy" => {
                        if let Some(m) = msg {
                            self.copy(m.text_str(), "Message copied to clipboard!");
                        }
                    }
                    "fork" => self.open_fork(Some(mid)),
                    "revert" => {
                        let c = self.client.clone();
                        let text = msg.map(|m| m.text_str().to_string()).unwrap_or_default();
                        self.call(async move { c.revert_stage(&sid, &mid).await }, move |app, _| {
                            let mut p = crate::state::PromptInfo::empty();
                            p.text = text;
                            app.editor.set(&p);
                        });
                    }
                    _ => {}
                }
            }
            "fork" => {
                let Some(sid) = self.session_id().map(|s| s.to_string()) else { return };
                self.fork(&sid, if v == "full" { None } else { Some(v.to_string()) });
            }
            "pending" => {
                let mid = d.data.as_str().unwrap_or("").to_string();
                let Some(sid) = self.session_id().map(|s| s.to_string()) else { return };
                self.dialogs.clear();
                let c = self.client.clone();
                if v == "queue" {
                    self.call(async move { c.inbox_update(&sid, &mid, "queue").await }, |_, _| {});
                } else {
                    self.call(async move { c.inbox_cancel(&sid, &mid).await }, |_, _| {});
                }
            }
            "skill" => {
                self.dialogs.clear();
                let skills = self.loc().and_then(|l| l.skills.clone()).unwrap_or_default();
                if let Some(s) = skills.iter().find(|s| s.id == v)
                    && !self.editor.chips.iter().any(|c| c.kind == ChipKind::Skill && c.data.get("id").and_then(|x| x.as_str()) == Some(v))
                {
                    self.editor.insert_chip(&format!("@{}", s.id), ChipKind::Skill, json!({ "id": s.id, "name": s.name }), true);
                }
            }
            "stash" => {
                self.dialogs.clear();
                if let Ok(i) = v.parse::<usize>()
                    && let Some(e) = self.stash.remove(i)
                {
                    self.editor.set(&e.prompt);
                }
            }
            "queued" => {
                self.dialogs.clear();
                let Some(sid) = self.session_id().map(|s| s.to_string()) else { return };
                let id = v.to_string();
                let c = self.client.clone();
                self.call(async move { c.inbox_update(&sid, &id, "steer").await }, |_, _| {});
            }
            "mcp" => {}
            "diff-source" => self.diff_source_selected(v),
            "terminal" => {
                self.dialogs.clear();
                let Some(sid) = self.session_id().map(|s| s.to_string()) else { return };
                self.terminal_open = true;
                if v == "new" {
                    self.new_terminal(&sid);
                } else {
                    self.select_terminal(&sid, Some(v));
                }
            }
            "diff-base" => self.diff_base_selected(v),
            "open" | "open-worktrees" => self.open_selected(v),
            "worktrees" => {
                let dir = v.get(2..).unwrap_or("").to_string();
                self.dialogs.clear();
                match self.session_id().map(|s| s.to_string()) {
                    Some(sid) => self.move_session(&sid, &dir),
                    None => self.set_home_location(&dir),
                }
            }
            _ => self.close_dialog(),
        }
    }

    fn dialog_submit(&mut self, purpose: &str, text: &str, d: &dialog::Dialog) {
        match purpose {
            "rename" => {
                let sid = d.data.as_str().unwrap_or("").to_string();
                self.dialogs.clear();
                if !text.trim().is_empty() {
                    self.rename(&sid, text.trim());
                }
            }
            "btw" => {
                self.dialogs.clear();
                let sid = d.data.as_str().unwrap_or("").to_string();
                if !text.trim().is_empty() {
                    self.run_command_args_btw(&sid, text);
                }
            }
            "new-worktree" | "worktree-name" => {
                let project = d.data.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                let then_move = purpose == "worktree-name" && self.session_id().is_some();
                self.create_worktree(text, &project, then_move);
            }
            _ => self.close_dialog(),
        }
    }

    fn run_command_args_btw(&mut self, _sid: &str, text: &str) {
        self.run_command("session.aside", Some(text.to_string()));
    }

    fn dialog_action(&mut self, purpose: &str, id: &str, v: Option<String>) {
        match (purpose, id) {
            ("open", "key:right") => {
                if let Some(v) = v {
                    self.open_project_worktrees(&v);
                }
            }
            ("open-worktrees", "key:left") => {
                self.dialogs.clear();
                self.open_open_dialog();
            }
            ("open-worktrees", "key:ctrl+n") => self.open_new_worktree_prompt(),
            ("worktrees", _) => self.worktree_action(id, v),
            ("model", "model.dialog.favorite") => {
                if let Some((p, m)) = v.as_deref().map(|v| v.strip_prefix("section:").unwrap_or(v)).and_then(|v| v.split_once('/')) {
                    let fav = self.models.is_favorite(p, m);
                    self.models.set_favorite(p, m, !fav);
                    let provider = self.dialogs.last().and_then(|d| d.data.get("provider").and_then(|p| p.as_str()).map(|p| p.to_string()));
                    let (filter, selected) = self.dialogs.last_mut().and_then(|d| d.sel()).map(|s| (s.filter.clone(), s.selected_value())).unwrap_or_default();
                    self.open_model_dialog(provider);
                    if let Some(sel) = self.dialogs.last_mut().and_then(|d| d.sel()) {
                        if !filter.is_empty() {
                            sel.set_filter(filter.clone());
                        }
                        if let Some(v) = selected
                            && let Some(i) = sel.visible().iter().position(|i| sel.options[*i].value == v)
                        {
                            sel.selected = i;
                        }
                    }
                    if !filter.is_empty() {
                        self.dialog_outcome(Outcome::Filter(filter));
                    }
                }
            }
            ("model", "model.dialog.provider") => {
                self.run_command("provider.connect", None);
            }
            ("session", "session.pin.toggle") => {
                if let Some(v) = v {
                    self.pins.toggle(&v);
                }
            }
            ("session", "session.rename") => {
                if let Some(v) = v {
                    let title = self.store.session(&v).map(|s| s.display_title()).unwrap_or_default();
                    let mut d = dialog::Dialog::new("rename", Body::Prompt(dialog::PromptDialog::new("Rename session", "Session title", &title)));
                    d.data = json!(v);
                    self.open_dialog(d);
                }
            }
            ("session", "session.delete") | ("stash", "stash.delete") => {
                let Some(v) = v else { return };
                let key = self.keybinds.shortcut(id);
                let armed = self.dialogs.last().map(|d| d.data.as_str() == Some(v.as_str())).unwrap_or(false);
                if !armed {
                    let t = self.theme().clone();
                    if let Some(d) = self.dialogs.last_mut() {
                        d.data = json!(v);
                        if let Some(sel) = d.sel()
                            && let Some(o) = sel.options.iter_mut().find(|o| o.value == v)
                        {
                            o.title = format!("Press {key} again to confirm");
                            o.bg = Some(t.dialog().background.action.destructive.focused);
                            o.fg = Some(t.dialog().text.action.destructive.focused);
                        }
                    }
                    return;
                }
                if purpose == "stash" {
                    if let Ok(i) = v.parse::<usize>() {
                        self.stash.remove(i);
                    }
                    self.run_command("prompt.stash.list", None);
                    return;
                }
                let c = self.client.clone();
                let id2 = v.clone();
                self.spawn(async move { c.session_delete(&id2).await }, move |app, r| match r {
                    Ok(_) => {
                        app.store.remove_session(&v);
                        app.tabs.retain(|t| *t != v);
                        app.save_tabs();
                        if app.dialogs.last().is_some_and(|d| d.purpose == "session") {
                            app.open_session_list();
                        }
                    }
                    Err(e) => app.toast(Toast::new("error", format!("Failed to delete session: {}", crate::api::error_message(&e)))),
                });
            }
            ("queued", "queued_prompt.delete") => {
                let (Some(sid), Some(v)) = (self.session_id().map(|s| s.to_string()), v) else { return };
                let c = self.client.clone();
                self.call(async move { c.inbox_cancel(&sid, &v).await }, |app, _| app.close_dialog());
            }
            ("queued", "queued_prompt.undo") => {
                let (Some(sid), Some(v)) = (self.session_id().map(|s| s.to_string()), v) else { return };
                let text = self.queued_prompts().into_iter().find(|i| i.id == v).and_then(|i| i.payload.get("text").and_then(|t| t.as_str()).map(|s| s.to_string()));
                let c = self.client.clone();
                self.call(async move { c.inbox_cancel(&sid, &v).await }, move |app, _| {
                    app.close_dialog();
                    if let Some(t) = text {
                        let mut p = crate::state::PromptInfo::empty();
                        p.text = t;
                        app.editor.set(&p);
                    }
                });
            }
            ("mcp", "dialog.mcp.toggle") => {
                let Some(name) = v else { return };
                let connected = self.loc().and_then(|l| l.mcp.clone()).unwrap_or_default().iter().any(|s| s.name == name && s.status.status == "connected");
                let c = self.client.clone();
                let dir = self.location_dir();
                self.call(async move { c.mcp_connect(&name, !connected, Some(&dir)).await }, |app, _| {
                    let d = app.location_dir();
                    app.fetch_catalog(&d, crate::store::Catalog::Mcp);
                });
            }
            ("export", "copy") | ("export", "export") => {
                let opts = match self.dialogs.last().map(|d| &d.body) {
                    Some(Body::Export(e)) => e.clone(),
                    _ => return,
                };
                self.export_session(opts, id == "copy");
            }
            ("settings", "key:left") => {
                if let Some(v) = v {
                    self.change_setting(&v, -1);
                }
            }
            ("settings", "key:right") => {
                if let Some(v) = v {
                    self.change_setting(&v, 1);
                }
            }
            _ => {}
        }
    }

    // ---- permission --------------------------------------------------------

    fn permission_key(&mut self, s: &Stroke) -> bool {
        let Some(req) = self.active_permission() else { return false };
        let ui = self.permission_ui.get_or_insert_with(|| PermissionUi { id: req.id.clone(), ..Default::default() });
        if ui.id != req.id {
            *ui = PermissionUi { id: req.id.clone(), ..Default::default() };
        }
        let options: Vec<&str> = if req.save.as_ref().is_some_and(|s| !s.is_empty()) { vec!["once", "always", "reject"] } else { vec!["once", "reject"] };
        let kb = self.keybinds.clone();
        if ui.rejecting {
            match s.name.as_str() {
                "return" if !s.shift && !s.meta => {
                    let msg = ui.reject_text.trim().to_string();
                    self.reply_permission(&req, "reject", if msg.is_empty() { None } else { Some(msg) });
                }
                "escape" => ui.rejecting = false,
                "c" if s.ctrl => {
                    if ui.reject_text.is_empty() {
                        ui.rejecting = false;
                    } else {
                        ui.reject_text.clear();
                    }
                }
                "backspace" => {
                    ui.reject_text.pop();
                }
                _ => {
                    if let Some(c) = s.is_char() {
                        ui.reject_text.push(c);
                    }
                }
            }
            return true;
        }
        let n = options.len();
        match s.name.as_str() {
            "left" | "h" if !s.ctrl => ui.selected = (ui.selected + n - 1) % n,
            "right" | "l" if !s.ctrl => ui.selected = (ui.selected + 1) % n,
            "return" => {
                let choice = options[ui.selected.min(n - 1)];
                self.permission_choose(&req, choice);
            }
            _ if kb.match_command("permission.prompt.fullscreen", std::slice::from_ref(s)) == Match::Full => ui.expanded = !ui.expanded,
            _ if s.name == "escape" || kb.match_command("app.exit", std::slice::from_ref(s)) == Match::Full => {
                if ui.expanded {
                    ui.expanded = false;
                } else {
                    self.permission_choose(&req, "reject");
                }
            }
            _ => return false,
        }
        true
    }

    pub fn permission_choose(&mut self, req: &PermissionRequest, choice: &str) {
        let child = self.store.session(&req.session_id).is_some_and(|s| s.parent_id.is_some());
        if choice == "reject" && child {
            if let Some(ui) = self.permission_ui.as_mut() {
                ui.rejecting = true;
            }
            return;
        }
        self.reply_permission(req, choice, None);
    }

    fn reply_permission(&mut self, req: &PermissionRequest, decision: &str, message: Option<String>) {
        let c = self.client.clone();
        let sid = req.session_id.clone();
        let rid = req.id.clone();
        let d = decision.to_string();
        if let Some(list) = self.store.permissions.get_mut(&sid) {
            list.retain(|p| p.id != rid);
        }
        self.permission_ui = None;
        self.call(async move { c.permission_reply(&sid, &rid, &d, message.as_deref()).await }, |_, _| {});
    }

    // ---- forms -------------------------------------------------------------

    pub fn form_state(&mut self) -> Option<(FormInfo, Option<LocationRef>, Vec<Value>)> {
        let (form, loc) = self.active_form()?;
        let ui = self.form_ui.get_or_insert_with(|| FormUi { id: form.id.clone(), answers: F::initial(&form.fields), ..Default::default() });
        if ui.id != form.id {
            *ui = FormUi { id: form.id.clone(), answers: F::initial(&form.fields), ..Default::default() };
        }
        let fields = F::visible(&form.fields, &ui.answers);
        Some((form, loc, fields))
    }

    fn form_key(&mut self, s: &Stroke) -> bool {
        let Some((form, loc, fields)) = self.form_state() else { return false };
        let single = fields.len() == 1 && (F::kind(&fields[0]) == "boolean" || (F::kind(&fields[0]) == "string" && F::options(&fields[0]).is_some()));
        let tabs = if single { 1 } else { fields.len() + 1 };
        let ui = self.form_ui.as_mut().unwrap();
        let tab = ui.tab.min(tabs - 1);
        let field = fields.get(tab).cloned();
        let review = field.is_none();
        let kb = self.keybinds.clone();
        let exit = kb.match_command("app.exit", std::slice::from_ref(s)) == Match::Full;
        // Editing a textual field or custom answer.
        let textual = field.as_ref().is_some_and(F::textual);
        if textual || ui.editing {
            let f = field.clone().unwrap();
            match s.name.as_str() {
                "tab" => {
                    let commit = commit_text(&f, &ui.text);
                    match commit {
                        Err(e) if !s.shift => ui.error = Some(e),
                        r => {
                            if let Ok(v) = r {
                                set_answer(ui, &f, v);
                            }
                            ui.error = None;
                            ui.editing = false;
                            ui.tab = if s.shift { (tab + tabs - 1) % tabs } else { (tab + 1) % tabs };
                            load_text(ui, fields.get(ui.tab));
                        }
                    }
                    return true;
                }
                "return" if !s.shift && !s.meta => {
                    match commit_text(&f, &ui.text) {
                        Ok(v) => {
                            set_answer(ui, &f, v);
                            ui.error = None;
                            ui.editing = false;
                            if single {
                                self.form_submit(&form, loc.as_ref());
                            } else if F::kind(&f) != "multiselect" {
                                ui.tab = (tab + 1) % tabs;
                                load_text(ui, fields.get(ui.tab));
                            }
                        }
                        Err(e) => ui.error = Some(e),
                    }
                    return true;
                }
                "escape" => {
                    if ui.editing {
                        ui.editing = false;
                    } else {
                        self.form_cancel(&form, loc.as_ref());
                    }
                    return true;
                }
                "c" if s.ctrl => {
                    if !ui.text.is_empty() {
                        ui.text.clear();
                    } else if ui.editing {
                        ui.editing = false;
                    } else {
                        self.form_cancel(&form, loc.as_ref());
                    }
                    return true;
                }
                "backspace" => {
                    ui.text.pop();
                    return true;
                }
                "up" if ui.editing => {
                    ui.editing = false;
                    ui.row = ui.row.saturating_sub(1);
                    return true;
                }
                _ => {
                    if let Some(c) = s.is_char() {
                        ui.text.push(c);
                        return true;
                    }
                    return exit;
                }
            }
        }
        if review {
            match s.name.as_str() {
                "return" => self.form_submit(&form, loc.as_ref()),
                "up" | "k" => ui.review_scroll = ui.review_scroll.saturating_sub(1),
                "down" | "j" => ui.review_scroll += 1,
                "left" | "h" => ui.tab = (tab + tabs - 1) % tabs,
                "right" | "l" | "tab" => ui.tab = if s.shift { (tab + tabs - 1) % tabs } else { (tab + 1) % tabs },
                "escape" => self.form_cancel(&form, loc.as_ref()),
                _ if exit => self.form_cancel(&form, loc.as_ref()),
                _ => return false,
            }
            return true;
        }
        let f = field.unwrap();
        if F::kind(&f) == "external" {
            match s.name.as_str() {
                "return" => {
                    let url = f.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
                    let k = F::key(&f);
                    if ui.acknowledged.contains(&k) {
                        ui.tab = (tab + 1) % tabs;
                    } else {
                        super::commands::open_url(&url);
                        ui.acknowledged.insert(k);
                    }
                }
                "c" => {
                    let url = f.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
                    self.copy(&url, "Copied URL to clipboard");
                }
                "escape" => self.form_cancel(&form, loc.as_ref()),
                "tab" | "right" | "l" => ui.tab = if s.shift { (tab + tabs - 1) % tabs } else { (tab + 1) % tabs },
                "left" | "h" => ui.tab = (tab + tabs - 1) % tabs,
                _ => return exit,
            }
            return true;
        }
        let rows = F::rows(&f);
        let custom = F::custom(&f);
        let count = rows.len() + usize::from(custom);
        let multi = F::kind(&f) == "multiselect";
        let activate = |ui: &mut FormUi, row: usize| -> bool {
            if row < rows.len() {
                let v = rows[row].0.clone();
                if multi {
                    let mut cur: Vec<Value> = ui.answers.get(&F::key(&f)).and_then(|a| a.as_array().cloned()).unwrap_or_default();
                    match cur.iter().position(|x| *x == v) {
                        Some(i) => {
                            cur.remove(i);
                        }
                        None => cur.push(v),
                    }
                    ui.answers.insert(F::key(&f), Value::Array(cur));
                    false
                } else {
                    ui.answers.insert(F::key(&f), v);
                    true
                }
            } else {
                ui.editing = true;
                ui.text.clear();
                false
            }
        };
        match s.name.as_str() {
            "left" | "h" if !s.ctrl => ui.tab = (tab + tabs - 1) % tabs,
            "right" | "l" if !s.ctrl => ui.tab = (tab + 1) % tabs,
            "tab" => ui.tab = if s.shift { (tab + tabs - 1) % tabs } else { (tab + 1) % tabs },
            "up" | "k" => ui.row = (ui.row + count.max(1) - 1) % count.max(1),
            "down" | "j" => ui.row = (ui.row + 1) % count.max(1),
            "space" if multi => {
                activate(ui, ui.row);
            }
            "return" => {
                let row = ui.row;
                if activate(ui, row) {
                    if single {
                        self.form_submit(&form, loc.as_ref());
                    } else {
                        ui.tab = (tab + 1) % tabs;
                        ui.row = 0;
                    }
                }
            }
            "escape" => self.form_cancel(&form, loc.as_ref()),
            n if n.len() == 1 && n.chars().next().unwrap().is_ascii_digit() && n != "0" => {
                let i = n.parse::<usize>().unwrap() - 1;
                if i < count.min(9) {
                    ui.row = i;
                    if activate(ui, i) {
                        if single {
                            self.form_submit(&form, loc.as_ref());
                        } else {
                            ui.tab = (tab + 1) % tabs;
                            ui.row = 0;
                        }
                    }
                }
            }
            _ => {
                if exit {
                    self.form_cancel(&form, loc.as_ref());
                    return true;
                }
                if custom && ui.row == rows.len()
                    && let Some(c) = s.is_char()
                {
                    ui.editing = true;
                    ui.text.push(c);
                    return true;
                }
                return false;
            }
        }
        true
    }

    fn form_submit(&mut self, form: &FormInfo, loc: Option<&LocationRef>) {
        let Some(ui) = self.form_ui.clone() else { return };
        let fields = F::visible(&form.fields, &ui.answers);
        for f in &fields {
            if F::kind(f) == "external" {
                if !ui.acknowledged.contains(&F::key(f)) {
                    if let Some(u) = self.form_ui.as_mut() {
                        u.error = Some(format!("External action must be acknowledged: {}", F::label(f)));
                    }
                    return;
                }
                continue;
            }
            if let Some(e) = F::validate(f, ui.answers.get(&F::key(f))) {
                if let Some(u) = self.form_ui.as_mut() {
                    u.error = Some(e);
                }
                return;
            }
        }
        let mut answer = serde_json::Map::new();
        for f in &fields {
            if let Some(v) = ui.answers.get(&F::key(f)) {
                answer.insert(F::key(f), v.clone());
            }
        }
        let c = self.client.clone();
        let sid = form.session_id.clone();
        let fid = form.id.clone();
        let dir = loc.map(|l| l.directory.clone());
        self.store.remove_form(&sid, &fid);
        self.form_ui = None;
        self.call(async move { c.form_reply(&sid, &fid, Value::Object(answer), dir.as_deref()).await }, |_, _| {});
    }

    fn form_cancel(&mut self, form: &FormInfo, loc: Option<&LocationRef>) {
        let c = self.client.clone();
        let sid = form.session_id.clone();
        let fid = form.id.clone();
        let dir = loc.map(|l| l.directory.clone());
        self.store.remove_form(&sid, &fid);
        self.form_ui = None;
        self.call(async move { c.form_cancel(&sid, &fid, dir.as_deref()).await }, |_, _| {});
    }

    // ---- mouse -------------------------------------------------------------

    pub fn on_mouse(&mut self, m: MouseEvent) {
        if self.dialogs.is_empty() && self.tab_mouse(m.kind, m.column, m.row) {
            return;
        }
        if self.dialogs.is_empty() && (self.terminal_visible().is_some() || (self.active_panel().is_some() && !self.panel_fullscreen())) && self.pane_mouse(m) {
            return;
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => self.selection_down(m.column, m.row),
            MouseEventKind::Drag(MouseButton::Left) => {
                self.selection_drag(m.column, m.row);
            }
            MouseEventKind::Down(MouseButton::Right) => {
                self.selection_right_down();
            }
            MouseEventKind::Up(MouseButton::Left) if self.selection_up(m.column, m.row) => {}
            MouseEventKind::ScrollUp => {
                if let Some(d) = self.dialogs.last_mut().and_then(|d| d.sel()) {
                    d.move_by(-1);
                } else {
                    let n = self.config.scroll_speed().round().max(1.0) as usize;
                    self.scroll(true, n);
                }
            }
            MouseEventKind::ScrollDown => {
                if let Some(d) = self.dialogs.last_mut().and_then(|d| d.sel()) {
                    d.move_by(1);
                } else {
                    let n = self.config.scroll_speed().round().max(1.0) as usize;
                    self.scroll(false, n);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let (x, y) = (m.column, m.row);
                let inside = |r: &ratatui::layout::Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
                if !self.dialogs.is_empty() {
                    let hit = self.dialog_hits.iter().find(|(r, _)| inside(r)).map(|(_, h)| h.clone());
                    match hit {
                        Some(DialogHit::Close) | Some(DialogHit::Ok) => self.close_dialog(),
                        Some(DialogHit::Option(i)) => {
                            let v = self.dialogs.last_mut().and_then(|d| d.sel()).and_then(|s| {
                                s.move_to(i);
                                s.selected_value()
                            });
                            if let Some(v) = v {
                                self.dialog_outcome(Outcome::Select(v));
                            }
                        }
                        Some(DialogHit::Action(a)) => {
                            let v = self.dialogs.last_mut().and_then(|d| d.sel()).and_then(|s| s.selected_value());
                            self.dialog_outcome(Outcome::Action(a, v));
                        }
                        Some(DialogHit::Confirm(_)) => self.close_dialog(),
                        None => {
                            if self.dialog_rect.is_some_and(|r| !inside(&r)) {
                                self.close_dialog();
                            }
                        }
                    }
                    self.dirty = true;
                    return;
                }
                let hit = self.hits.iter().find(|(r, _)| inside(r)).map(|(_, c)| c.clone());
                if let Some(c) = hit {
                    self.on_click(c);
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    fn pane_mouse(&mut self, m: MouseEvent) -> bool {
        let r = self.terminal_rect;
        let (x, y) = (m.column, m.row);
        let in_term = x >= r.x && x < r.right() && y >= r.y && y < r.bottom();
        let on_handle = x + 1 == r.x && y >= r.y && y < r.bottom();
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if on_handle => {
                self.resizing = Some(super::tabs::Resizing::Pane);
                return true;
            }
            MouseEventKind::Drag(MouseButton::Left) if self.resizing == Some(super::tabs::Resizing::Pane) => {
                let w = self.size.0.saturating_sub(x + 1);
                self.pane_pref = Some(w.max(1));
                self.dirty = true;
                return true;
            }
            MouseEventKind::Up(MouseButton::Left) if self.resizing == Some(super::tabs::Resizing::Pane) => {
                self.resizing = None;
                let w = self.pane_pref;
                self.keyed.update("layout", move |v| {
                    if !v.is_object() {
                        *v = serde_json::json!({});
                    }
                    v["paneWidth"] = serde_json::json!(w);
                });
                return true;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.right_focus = in_term;
                self.dirty = true;
                // A panel takes the focus and still handles the click itself.
                if self.active_panel().is_some() {
                    return false;
                }
                if in_term
                    && let Some(p) = self.terminal_visible()
                    && let Some(st) = self.terminal_states.get_mut(&p)
                {
                    st.interact();
                }
                return in_term;
            }
            _ => {}
        }
        false
    }

    pub fn on_click(&mut self, c: Click) {
        if let Click::Custom(x) = &c
            && x.starts_with("diff.")
        {
            let x = x.clone();
            self.diff_click(&x);
            return;
        }
        let Some(sid) = self.session_id().map(|s| s.to_string()) else {
            if let Click::Custom(x) = c {
                self.run_command(&x, None);
            }
            return;
        };
        let toggle = |app: &mut App, key: String, default: bool| {
            let v = app.view(&sid);
            let cur = v.expanded.get(&key).copied().unwrap_or(default);
            v.expanded.insert(key, !cur);
        };
        match c {
            Click::ToggleGroup(id) => {
                let default = self.config.verbosity() == "high" && (id.contains("\"exploration\"") || id.contains("\"instructions\""));
                toggle(self, id, default);
            }
            Click::ToggleTool(id) => toggle(self, id, false),
            Click::ToggleError(id) => toggle(self, format!("error:{id}"), false),
            Click::ToggleThinking(id) => toggle(self, id, false),
            Click::OpenMessage(id) => {
                let opts = [("Jump to", "jump", "view message in session"), ("Revert", "revert", "undo messages and file changes"), ("Copy", "copy", "message text to clipboard"), ("Fork", "fork", "create a new session")]
                    .iter()
                    .map(|(t, v, d)| dialog::Opt::new(*t, *v).desc(*d))
                    .collect();
                let mut d = dialog::Dialog::select("message", dialog::Select::new("Message Actions", opts), dialog::Size::Medium);
                d.data = json!(id);
                self.open_dialog(d);
            }
            Click::PendingSteer(id) => {
                let opts = vec![dialog::Opt::new("Move to queue", "queue"), dialog::Opt::new("Delete", "cancel")];
                let mut d = dialog::Dialog::select("pending", dialog::Select::new("Pending steer", opts), dialog::Size::Medium);
                d.data = json!(id);
                self.open_dialog(d);
            }
            Click::Navigate(id) => self.navigate(Route::Session(id)),
            Click::Execute(_, _) => {}
            Click::RevertClear => {
                let c = self.client.clone();
                self.call(async move { c.revert_clear(&sid).await }, |_, _| {});
            }
            Click::JumpBottom => self.view(&sid).scroll_from_bottom = 0,
            Click::QueuedDialog => {
                self.run_command("session.queued_prompts", None);
            }
            Click::ShellOutput(_) => {}
            Click::Custom(x) => {
                self.run_command(&x, None);
            }
        }
        self.dirty = true;
    }
}

fn commit_text(f: &Value, text: &str) -> Result<Value, String> {
    let v = match F::kind(f) {
        "number" | "integer" => {
            if text.trim().is_empty() {
                return F::validate(f, None).map(Err).unwrap_or(Ok(Value::Null));
            }
            match text.trim().parse::<f64>() {
                Ok(n) => json!(n),
                Err(_) => return Err("Expected a number".into()),
            }
        }
        "multiselect" => {
            return Ok(Value::String(text.to_string()));
        }
        _ => Value::String(text.to_string()),
    };
    match F::validate(f, Some(&v)) {
        Some(e) => Err(e),
        None => Ok(v),
    }
}

fn set_answer(ui: &mut FormUi, f: &Value, v: Value) {
    if v.is_null() {
        return;
    }
    let key = F::key(f);
    if F::kind(f) == "multiselect" {
        let mut cur: Vec<Value> = ui.answers.get(&key).and_then(|a| a.as_array().cloned()).unwrap_or_default();
        if v.as_str().is_some_and(|s| !s.is_empty()) && !cur.contains(&v) {
            cur.push(v);
        }
        ui.answers.insert(key, Value::Array(cur));
        return;
    }
    ui.answers.insert(key, v);
}

fn load_text(ui: &mut FormUi, f: Option<&Value>) {
    ui.text = match f {
        Some(f) if F::textual(f) => match ui.answers.get(&F::key(f)) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        },
        _ => String::new(),
    };
}
