//! The prompt composer (`src/mini/footer.prompt.tsx`): `createPromptState` (draft, mention parts
//! tracked by virtual extmarks, history, `@`/`/` autocomplete, shell mode, submit rules) and
//! `RunPromptBody` (the `┃` rail and the textarea).

use super::cell::{Buffer, Color, StyledChunk};
use super::fuzzysort;
use super::menu::{FOOTER_COMPACT_WIDTH, FOOTER_MENU_ROWS, MenuItem, MenuState};
use super::prompt_shared::*;
use super::text::{WrapMode, layout};
use super::textarea::TextArea;
use super::types::*;
use super::util::{mention_trigger_index, parse_file_line_range, parse_slash_head, slash_trigger_index, strip_file_line_range};
use crate::util::locale;

pub const TEXTAREA_MIN_ROWS: usize = 1;
pub const TEXTAREA_MAX_ROWS: usize = 6;
pub const PROMPT_MAX_ROWS: usize = TEXTAREA_MAX_ROWS + FOOTER_MENU_ROWS;
const MARK_TYPE: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct PromptLayout {
    pub padding: usize,
    pub textarea: usize,
    pub menu: usize,
    pub images: usize,
    pub rows: usize,
}

/// `footerPromptLayout(height, lines, options, statusRows, images)`.
pub fn footer_prompt_layout(height: usize, lines: usize, options: usize, status_rows: usize, images: bool) -> PromptLayout {
    let padding = if height >= PROMPT_MAX_ROWS + 4 { 1 } else { 0 };
    let available = (height as i64 - 1 - status_rows as i64 - padding as i64 * 2).max(1) as usize;
    let preview = if images && options == 0 { 4.min(available as i64 - TEXTAREA_MAX_ROWS.min(lines.max(1)) as i64) } else { 0 };
    let image_rows = if preview >= 3 { preview as usize } else { 0 };
    let textarea = 1.max(TEXTAREA_MAX_ROWS.min((available as i64 - image_rows as i64 - if options > 0 { 1 } else { 0 }).max(1) as usize));
    let rows = textarea.min(lines.max(1));
    let menu = if options > 0 { 1.max(FOOTER_MENU_ROWS.min(options).min(available.saturating_sub(rows))) } else { 0 };
    PromptLayout { padding, textarea, menu, images: image_rows, rows: rows + menu + image_rows }
}

#[derive(Clone, Debug, PartialEq)]
pub enum OptionKind {
    Mention { value: String, part: PromptPart, directory: bool },
    Slash { name: String, action: Option<&'static str> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PromptOption {
    pub item: MenuItem,
    pub kind: OptionKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuMode {
    Off,
    Mention,
    Slash,
}

/// What the composer asks of its host (the footer).
pub trait Host {
    fn agents(&self) -> &[RunAgent];
    fn references(&self) -> &[RunReference];
    fn commands(&self) -> Option<&[RunCommand]>;
    fn state(&self) -> &FooterState;
    fn queued_prompts(&self) -> Vec<QueuedPrompt>;
    fn directory(&self) -> String;
    fn term_height(&self) -> usize;
    fn mono(&self) -> bool;
    /// `onSubmit`: true when the prompt was accepted.
    fn submit(&mut self, prompt: RunPrompt) -> bool;
    fn cycle(&mut self);
    fn interrupt(&mut self) -> bool;
    fn editor_open(&mut self, value: String);
    fn input_clear(&mut self);
    fn exit_request(&mut self) -> bool;
    fn exit(&mut self);
    fn settings(&mut self);
    fn status(&mut self, text: String);
    fn queued_steer(&mut self, inbox_id: String);
    fn find_files(&mut self, query: String);
    fn paste_clipboard(&mut self);
}

pub struct Composer {
    pub area: TextArea,
    pub shell: bool,
    history: PromptHistory,
    draft: RunPrompt,
    stash: RunPrompt,
    parts: Vec<PromptPart>,
    marks: Vec<(u32, usize)>,
    pub mode: MenuMode,
    at: usize,
    pub query: String,
    pub menu: MenuState,
    files: Vec<String>,
    files_query: Option<String>,
    submitting: bool,
    lines: usize,
    pub status_rows: usize,
    /// The textarea width of the last render.
    pub width: usize,
    prev_view: String,
    /// A draft for the editor round trip.
    editing: Option<RunPrompt>,
    /// The placeholder shown while empty (its wrapped rows count as textarea lines).
    pub placeholder: String,
}

fn empty_prompt(shell: bool) -> RunPrompt {
    RunPrompt { shell, ..Default::default() }
}

/// `parseSlashCommand(text, commands)`.
enum Parsed {
    None,
    Pending,
    Command(PromptCommand),
}

fn parse_slash_command(text: &str, commands: Option<&[RunCommand]>) -> Parsed {
    let Some(head) = parse_slash_head(text) else { return Parsed::None };
    if head.name.is_empty() {
        return Parsed::None;
    }
    let Some(commands) = commands else { return Parsed::Pending };
    let Some(item) = commands.iter().find(|c| c.name == head.name) else { return Parsed::None };
    Parsed::Command(PromptCommand { name: head.name, arguments: head.arguments, source: item.source.clone() })
}

/// `selectedCommand(text, command)`.
pub fn selected_command(text: &str, command: Option<&PromptCommand>) -> Option<PromptCommand> {
    let command = command?;
    let head = parse_slash_head(text)?;
    if head.name != command.name {
        return None;
    }
    Some(PromptCommand { name: command.name.clone(), arguments: head.arguments, source: command.source.clone() })
}

fn file_url(path: &str) -> String {
    let mut out = String::from("file://");
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' | b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=' | b':' | b'@' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl Default for Composer {
    fn default() -> Self {
        Composer::new(&[])
    }
}

impl Composer {
    pub fn new(history: &[RunPrompt]) -> Composer {
        Composer {
            area: TextArea::new(),
            shell: false,
            history: create_prompt_history(history),
            draft: RunPrompt::default(),
            stash: RunPrompt::default(),
            parts: Vec::new(),
            marks: Vec::new(),
            mode: MenuMode::Off,
            at: 0,
            query: String::new(),
            menu: MenuState::default(),
            files: Vec::new(),
            files_query: None,
            submitting: false,
            lines: 1,
            status_rows: 1,
            width: 1,
            prev_view: "prompt".into(),
            editing: None,
            placeholder: String::new(),
        }
    }

    pub fn set_history(&mut self, history: &[RunPrompt]) {
        self.history = create_prompt_history(history);
    }

    pub fn visible(&self) -> bool {
        self.mode != MenuMode::Off
    }

    /// `placeholder()`: text (muted) or empty.
    pub fn placeholder(&self, first: bool, mono: bool) -> String {
        if self.shell {
            return "Run a command… \"git status\"".into();
        }
        if !first {
            return String::new();
        }
        format!("Ask anything, / for commands, @ for context{}", if mono { "..." } else { "…" })
    }

    /// Menu width for display trimming (`width()` in createPromptState).
    fn menu_width(term_width: usize) -> usize {
        term_width.saturating_sub(if term_width < FOOTER_COMPACT_WIDTH { 2 } else { 4 })
    }

    fn agent_options(&self, host: &dyn Host) -> Vec<PromptOption> {
        host.agents()
            .iter()
            .filter(|a| !a.hidden && a.mode != "primary")
            .map(|a| PromptOption {
                item: MenuItem { display: format!("@{}", a.name), ..Default::default() },
                kind: OptionKind::Mention { value: a.id.clone(), part: PromptPart::Agent { name: a.id.clone(), source: Some(MentionRange::default()) }, directory: false },
            })
            .collect()
    }

    fn reference_options(&self, host: &dyn Host) -> Vec<PromptOption> {
        host.references()
            .iter()
            .map(|r| PromptOption {
                item: MenuItem { display: format!("@{}", r.name), description: Some(r.description.clone().unwrap_or_else(|| r.source_detail.clone())), ..Default::default() },
                kind: OptionKind::Mention {
                    value: r.name.clone(),
                    part: PromptPart::File { url: file_url(&r.path), filename: Some(r.name.clone()), mime: Some("application/x-directory".into()), description: None, source: Some((r.name.clone(), MentionRange::default())) },
                    directory: false,
                },
            })
            .collect()
    }

    fn file_options(&self, host: &dyn Host, term_width: usize) -> Vec<PromptOption> {
        let (_, range) = parse_file_line_range(&self.query);
        let width = Self::menu_width(term_width);
        let dir = host.directory();
        self.files
            .iter()
            .map(|item| {
                let directory = item.ends_with('/');
                let mut filename = item.clone();
                let mut url = file_url(&super::util::resolve(&dir, item));
                if let Some((start, end)) = range
                    && !directory
                {
                    filename = format!("{item}#{start}{}", end.map(|e| format!("-{e}")).unwrap_or_default());
                    url = format!("{url}?start={start}{}", end.map(|e| format!("&end={e}")).unwrap_or_default());
                }
                let parts: Vec<&str> = filename.split('/').collect();
                let take = if directory { parts.len().saturating_sub(1) } else { parts.len() };
                let paths: Vec<String> = (0..take).map(|i| format!("@{}", parts[i..].join("/"))).collect();
                let display = paths.iter().find(|p| locale::width(p) <= width).cloned().or_else(|| paths.last().cloned()).unwrap_or(format!("@{filename}"));
                PromptOption {
                    item: MenuItem { display, ..Default::default() },
                    kind: OptionKind::Mention {
                        value: filename.clone(),
                        part: PromptPart::File { url, filename: Some(filename.clone()), mime: Some(if directory { "application/x-directory" } else { "text/plain" }.into()), description: None, source: Some((item.clone(), MentionRange::default())) },
                        directory,
                    },
                }
            })
            .collect()
    }

    fn slash_options(&self, host: &dyn Host) -> Vec<PromptOption> {
        let slash = |name: &str, action: Option<&'static str>, description: &str| PromptOption {
            item: MenuItem { display: format!("/{name}"), description: Some(description.into()), ..Default::default() },
            kind: OptionKind::Slash { name: name.into(), action },
        };
        let mut builtins = vec![
            slash("editor", Some("editor"), "compose in your external editor"),
            slash("settings", Some("settings"), "configure Mini transcript output"),
            slash("new", None, "start a new session"),
            slash("compact", None, "compact older session context to free space"),
        ];
        for name in EXIT_COMMANDS {
            builtins.push(slash(name, None, "close OpenCode"));
        }
        let hidden: Vec<String> = builtins.iter().map(|b| match &b.kind {
            OptionKind::Slash { name, .. } => name.clone(),
            _ => String::new(),
        }).collect();
        let mut all: Vec<PromptOption> = host
            .commands()
            .unwrap_or(&[])
            .iter()
            .filter(|c| !hidden.contains(&c.name))
            .map(|c| PromptOption {
                item: MenuItem { display: format!("/{}{}", c.name, if c.source.as_deref() == Some("mcp") { ":mcp" } else { "" }), description: c.description.clone(), ..Default::default() },
                kind: OptionKind::Slash { name: c.name.clone(), action: None },
            })
            .collect();
        all.extend(builtins);
        all.sort_by(|a, b| locale::compare(&a.item.display, &b.item.display));
        all
    }

    /// `options()`.
    pub fn options(&self, host: &dyn Host, term_width: usize) -> Vec<PromptOption> {
        let mixed = if self.mode == MenuMode::Slash {
            self.slash_options(host)
        } else {
            let mut v = self.agent_options(host);
            v.extend(self.file_options(host, term_width));
            v.extend(self.reference_options(host));
            v
        };
        if self.query.is_empty() {
            return mixed;
        }
        let next = strip_file_line_range(&self.query);
        let keys = |o: &PromptOption| -> Vec<Option<String>> {
            let value = match &o.kind {
                OptionKind::Mention { value, .. } => value.clone(),
                OptionKind::Slash { name, .. } => name.clone(),
            };
            vec![Some(value), Some(o.item.display.clone()), o.item.description.clone()]
        };
        if self.mode == MenuMode::Mention {
            let agents = self.agent_options(host);
            let refs = self.reference_options(host);
            let mut out: Vec<PromptOption> = fuzzysort::go(&next, &agents, keys).into_iter().map(|i| agents[i].clone()).collect();
            out.extend(self.file_options(host, term_width));
            out.extend(fuzzysort::go(&next, &refs, keys).into_iter().map(|i| refs[i].clone()));
            return out;
        }
        let slash_keys = |o: &PromptOption| -> Vec<Option<String>> {
            let value = match &o.kind {
                OptionKind::Mention { value, .. } => value.trim_end().to_string(),
                OptionKind::Slash { name, .. } => name.trim_end().to_string(),
            };
            vec![Some(value), Some(o.item.display.clone()), o.item.description.clone()]
        };
        fuzzysort::go(&next, &mixed, slash_keys).into_iter().map(|i| mixed[i].clone()).collect()
    }

    /// `layout()`.
    pub fn layout(&self, host: &dyn Host, term_width: usize) -> PromptLayout {
        let options = if self.visible() { self.options(host, term_width).len().max(1) } else { 0 };
        footer_prompt_layout(host.term_height(), self.lines, options, self.status_rows, false)
    }

    fn menu_limit(&self, host: &dyn Host, term_width: usize) -> usize {
        self.layout(host, term_width).menu.max(1)
    }

    pub fn hide(&mut self) {
        self.mode = MenuMode::Off;
        self.query.clear();
        self.menu.reset();
    }

    /// `syncRows`: the composer's row count after an edit.
    pub fn sync_rows(&mut self) {
        self.lines = self.area.line_count().max(self.visual_rows());
    }

    /// Visual rows of the textarea: the wrapped text, or the wrapped placeholder while empty.
    fn visual_rows(&self) -> usize {
        if self.area.plain_text().is_empty() && !self.placeholder.is_empty() {
            return layout(&[StyledChunk::plain(self.placeholder.clone(), Color::WHITE)], self.width.max(1), WrapMode::Word, false).len().max(1);
        }
        self.area.virtual_line_count(self.width)
    }

    fn sync_parts(&mut self) {
        let mut next: Vec<Option<PromptPart>> = self.parts.iter().map(|p| match p {
            PromptPart::File { source: None, .. } => Some(p.clone()),
            _ => None,
        }).collect();
        let mut tracked = 0;
        for m in self.area.marks.clone() {
            if m.type_id != MARK_TYPE {
                continue;
            }
            let Some(&(_, idx)) = self.marks.iter().find(|(id, _)| *id == m.id) else { continue };
            let Some(part) = self.parts.get(idx) else { continue };
            let text = self.area.get_text_range(m.start, m.end);
            let prev = match part {
                PromptPart::Agent { name, source } => source.as_ref().map(|s| s.value.clone()).unwrap_or(format!("@{name}")),
                PromptPart::Skill { id, source } => source.as_ref().map(|s| s.value.clone()).unwrap_or(format!("/{id}")),
                PromptPart::File { filename, source, .. } => source.as_ref().map(|s| s.1.value.clone()).unwrap_or(format!("@{}", filename.clone().unwrap_or_default())),
            };
            if text != prev {
                continue;
            }
            let mut copy = part.clone();
            match &mut copy {
                PromptPart::Agent { source, .. } | PromptPart::Skill { source, .. } => *source = Some(MentionRange { start: m.start, end: m.end, value: text }),
                PromptPart::File { source: Some(s), .. } => s.1 = MentionRange { start: m.start, end: m.end, value: text },
                _ => {}
            }
            tracked += 1;
            next[idx] = Some(copy);
        }
        let retained: Vec<PromptPart> = next.into_iter().flatten().collect();
        let stale = tracked != self.marks.len() || retained.len() != self.parts.len();
        self.parts = retained.clone();
        if stale {
            self.restore_parts(&retained);
        }
    }

    fn clear_parts(&mut self) {
        self.area.clear_marks();
        self.parts.clear();
        self.marks.clear();
    }

    fn restore_parts(&mut self, value: &[PromptPart]) {
        self.clear_parts();
        self.parts = value.to_vec();
        for (idx, item) in self.parts.clone().iter().enumerate() {
            let range = match item {
                PromptPart::File { source, .. } => source.as_ref().map(|s| (s.1.start, s.1.end)),
                PromptPart::Agent { source, .. } | PromptPart::Skill { source, .. } => source.as_ref().map(|s| (s.start, s.end)),
            };
            let Some((start, end)) = range else { continue };
            let id = self.area.create_mark(start, end, true, MARK_TYPE);
            self.marks.push((id, idx));
        }
    }

    /// `restore(value, cursor)`.
    pub fn restore(&mut self, value: &RunPrompt, cursor: Option<usize>) {
        self.draft = prompt_copy(value);
        self.shell = value.shell;
        self.hide();
        self.area.set_text(&value.text);
        self.restore_parts(&value.parts);
        let end = measure_edit_text(&value.text);
        self.area.set_cursor_offset(cursor.unwrap_or(end).min(end));
        self.sync_rows();
    }

    fn reset_draft(&mut self) {
        self.area.set_text("");
        self.clear_parts();
        self.hide();
        self.draft = empty_prompt(self.shell);
        self.sync_rows();
    }

    fn set_shell_mode(&mut self, value: bool) {
        self.shell = value;
        if value {
            self.draft.shell = true;
        } else {
            self.draft = RunPrompt { text: self.draft.text.clone(), parts: self.draft.parts.clone(), ..Default::default() };
        }
    }

    /// `refresh()`: open, update or close the autocomplete after an edit.
    pub fn refresh(&mut self, host: &mut dyn Host) {
        let cursor = self.area.cursor_offset();
        let before = self.area.get_text_range(0, cursor);
        let slash = slash_trigger_index(&before).map(|at| (at, self.area.get_text_range(at + 1, cursor)));
        if self.mode == MenuMode::Slash {
            match slash {
                None => self.hide(),
                Some((at, value)) => {
                    self.at = at;
                    self.set_query(host, value);
                }
            }
            return;
        }
        if let Some((at, value)) = slash {
            self.at = at;
            self.menu.reset();
            self.mode = MenuMode::Slash;
            self.set_query(host, value);
            return;
        }
        if self.visible() && self.mode == MenuMode::Mention {
            let query = self.area.get_text_range(self.at, cursor);
            if cursor <= self.at || query.chars().any(char::is_whitespace) {
                self.hide();
                return;
            }
            let q = self.area.get_text_range(self.at + 1, cursor);
            self.set_query(host, q);
            return;
        }
        if cursor == 0 {
            return;
        }
        if let Some(idx) = mention_trigger_index(&before) {
            self.at = idx;
            self.menu.reset();
            self.mode = MenuMode::Mention;
            let q = self.area.get_text_range(idx + 1, cursor);
            self.set_query(host, q);
        }
    }

    fn set_query(&mut self, host: &mut dyn Host, value: String) {
        if value != self.query || self.files_query.is_none() {
            self.menu.reset();
        }
        self.query = value;
        if self.mode == MenuMode::Mention && self.files_query.as_deref() != Some(self.query.as_str()) {
            self.files_query = Some(self.query.clone());
            let (base, _) = parse_file_line_range(&self.query);
            host.find_files(base);
        }
    }

    /// File search results for `query` (the resource behind `files()`).
    pub fn set_files(&mut self, query: &str, files: Vec<String>) {
        if self.files_query.as_deref() == Some(query) {
            self.files = files;
        }
    }

    /// `syncDraft()`.
    pub fn sync_draft(&mut self) {
        self.sync_parts();
        let text = self.area.plain_text().to_string();
        let command = if self.shell { None } else { selected_command(&text, self.draft.command.as_ref()) };
        self.draft = RunPrompt { text, parts: self.parts.clone(), shell: self.shell, command, ..Default::default() };
    }

    pub fn current(&mut self) -> RunPrompt {
        self.sync_draft();
        prompt_copy(&self.draft)
    }

    /// `requestExit()` (ctrl+c): clear a draft first, else request exit.
    pub fn request_exit(&mut self, host: &mut dyn Host, prompt_active: bool) -> bool {
        let text = self.area.plain_text().to_string();
        if prompt_active && (!text.is_empty() || self.draft.parts.iter().any(|p| p.is_file())) {
            host.input_clear();
            self.reset_draft();
            return true;
        }
        host.exit_request()
    }

    fn cancel_autocomplete(&mut self) {
        let cursor = self.area.cursor_offset();
        let start = self.at;
        self.area.delete_range(start, cursor);
        self.area.set_cursor_offset(start);
        self.hide();
        self.sync_draft();
        self.sync_rows();
    }

    /// `openEditor({ value })`.
    pub fn open_editor(&mut self, host: &mut dyn Host, value: Option<String>) {
        host.input_clear();
        self.sync_draft();
        self.hide();
        let current = prompt_copy(&self.draft);
        let value = value.unwrap_or_else(|| current.text.clone());
        self.editing = Some(current);
        host.editor_open(value);
    }

    /// The editor closed with `content` (`None`: cancelled, `Err`: failed).
    pub fn editor_result(&mut self, host: &mut dyn Host, content: Result<Option<String>, ()>) {
        let Some(current) = self.editing.take() else { return };
        match content {
            Ok(None) => {}
            Ok(Some(content)) => {
                let normalized = normalize_prompt_content(&content);
                let parts = realign_editor_prompt_parts(&normalized, &current.parts);
                self.restore(&RunPrompt { text: normalized, parts, shell: current.shell, command: current.command.clone(), ..Default::default() }, None);
            }
            Err(()) => {
                self.restore(&current, None);
                host.status("failed to open editor".into());
            }
        }
    }

    /// `select(item, delivery)`.
    pub fn select(&mut self, host: &mut dyn Host, term_width: usize, item: Option<PromptOption>, delivery: Delivery) {
        let options = self.options(host, term_width);
        let Some(next) = item.or_else(|| options.get(self.menu.selected as usize).cloned()) else { return };
        match &next.kind {
            OptionKind::Slash { name, action } => {
                if *action == Some("editor") {
                    let value = resolve_editor_slash_value(self.area.plain_text());
                    self.open_editor(host, Some(value));
                    return;
                }
                if *action == Some("settings") && !self.shell {
                    self.cancel_autocomplete();
                    host.settings();
                    return;
                }
                let cursor = self.area.cursor_offset();
                let plain = self.area.plain_text().to_string();
                let head = parse_slash_head(&plain);
                let local = !self.shell && (name == "new" || is_exit_command(&format!("/{name}")));
                let separator = if !self.shell && !local && head.as_ref().is_some_and(|h| plain[h.end..].chars().next().is_some_and(char::is_whitespace)) { "" } else { " " };
                let text = format!("/{name}{separator}");
                let end = match &head {
                    Some(h) if !self.shell => measure_edit_text(if local { &plain } else { &plain[..h.end] }),
                    _ => cursor,
                };
                self.area.delete_range(0, end);
                self.area.set_cursor_offset(0);
                self.area.insert_text(&text);
                self.area.set_cursor_offset(measure_edit_text(&text));
                self.hide();
                self.sync_draft();
                if !self.shell {
                    let draft = prompt_copy(&self.draft);
                    self.submit_prompt(host, term_width, draft, delivery);
                    return;
                }
                self.sync_rows();
            }
            OptionKind::Mention { value, part, .. } => {
                let cursor = self.area.cursor_offset();
                let following = self.area.get_text_range(cursor, cursor + 1);
                let append = format!("@{value}{}", if following == " " { "" } else { " " });
                let start_offset = self.at;
                self.area.delete_range(start_offset, cursor);
                self.area.set_cursor_offset(start_offset);
                self.area.insert_text(&append);
                let text = format!("@{value}");
                let end_offset = start_offset + measure_edit_text(&text);
                let mut part = part.clone();
                match &mut part {
                    PromptPart::Agent { source, .. } => *source = Some(MentionRange { start: start_offset, end: end_offset, value: text.clone() }),
                    PromptPart::File { source: Some(s), .. } => s.1 = MentionRange { start: start_offset, end: end_offset, value: text.clone() },
                    _ => {}
                }
                if let PromptPart::File { url, .. } = &part
                    && let Some(prev) = self.parts.iter().position(|p| matches!(p, PromptPart::File { url: u, .. } if u == url))
                {
                    if let Some(pos) = self.marks.iter().position(|(_, idx)| *idx == prev) {
                        let (mark, _) = self.marks.remove(pos);
                        self.area.delete_mark(mark);
                    }
                    self.parts.remove(prev);
                    for m in self.marks.iter_mut() {
                        if m.1 > prev {
                            m.1 -= 1;
                        }
                    }
                }
                let id = self.area.create_mark(start_offset, end_offset, true, MARK_TYPE);
                self.marks.push((id, self.parts.len()));
                self.parts.push(part);
                self.hide();
                self.sync_draft();
                self.sync_rows();
            }
        }
    }

    /// `expand()`: descend into a directory mention.
    pub fn expand(&mut self, host: &mut dyn Host, term_width: usize) {
        let options = self.options(host, term_width);
        let Some(PromptOption { kind: OptionKind::Mention { value, directory: true, .. }, .. }) = options.get(self.menu.selected as usize).cloned() else { return };
        let cursor = self.area.cursor_offset();
        self.area.delete_range(self.at, cursor);
        self.area.set_cursor_offset(self.at);
        self.area.insert_text(&format!("@{value}"));
        self.sync_draft();
        self.refresh(host);
    }

    /// `submitPrompt(next, delivery)`.
    pub fn submit_prompt(&mut self, host: &mut dyn Host, term_width: usize, next: RunPrompt, delivery: Delivery) {
        if self.visible() {
            let options = self.options(host, term_width);
            if self.mode != MenuMode::Slash || !options.is_empty() {
                self.select(host, term_width, None, delivery);
                return;
            }
            self.hide();
        }
        if self.submitting {
            return;
        }
        if next.text.trim().is_empty() && !next.parts.iter().any(|p| p.is_file()) {
            let queued = if delivery == Delivery::Steer { host.queued_prompts().into_iter().find(|q| q.delivery == Delivery::Queue) } else { None };
            if let Some(q) = queued {
                host.queued_steer(q.message_id);
                return;
            }
            let running = host.state().phase == FooterPhase::Running;
            host.status(if running { "waiting for current response" } else { "empty prompt ignored" }.into());
            return;
        }
        let command = if next.shell { None } else { selected_command(&next.text, next.command.as_ref()) };
        let lower = next.text.trim().to_lowercase();
        if delivery == Delivery::Queue && (next.shell || is_new_command(&next.text) || is_compact_command(&next.text) || is_exit_command(&next.text) || lower == "/settings") {
            host.status("this prompt cannot be queued".into());
            return;
        }
        if command.is_none() && !next.shell && is_exit_command(&next.text) {
            host.exit();
            return;
        }
        if command.is_none() && !next.shell && lower == "/settings" {
            self.reset_draft();
            host.settings();
            return;
        }
        let parsed = if command.is_some() || next.parts.iter().any(|p| matches!(p, PromptPart::Skill { .. })) || next.shell || is_new_command(&next.text) {
            Parsed::None
        } else {
            parse_slash_command(&next.text, host.commands())
        };
        if matches!(parsed, Parsed::Pending) {
            host.status("loading commands".into());
            return;
        }
        let mut submit = next.clone();
        submit.delivery = Some(delivery);
        if let Some(c) = command {
            submit.command = Some(c);
        } else if let Parsed::Command(c) = parsed {
            submit.command = Some(c);
        }
        let shell_mode = next.shell;
        self.submitting = true;
        self.reset_draft();
        let accepted = host.submit(submit);
        if accepted {
            self.history = push_prompt_history(&self.history, &next);
            if shell_mode {
                self.set_shell_mode(false);
                self.draft = empty_prompt(false);
            }
        } else {
            self.restore(&next, None);
        }
        self.submitting = false;
    }

    /// `onSubmit(delivery)` (enter).
    pub fn on_submit(&mut self, host: &mut dyn Host, term_width: usize, delivery: Delivery) {
        self.sync_draft();
        let draft = prompt_copy(&self.draft);
        self.submit_prompt(host, term_width, draft, delivery);
    }

    pub fn submit_text(&mut self, host: &mut dyn Host, term_width: usize, text: &str) {
        self.submit_prompt(host, term_width, RunPrompt::text(text), Delivery::Steer);
    }

    /// `onContentChange` after a textarea edit.
    pub fn content_changed(&mut self, host: &mut dyn Host) {
        host.input_clear();
        self.sync_draft();
        self.refresh(host);
        self.sync_rows();
    }

    /// History navigation (`historyCommand`): returns false when the key falls through.
    pub fn history_command(&mut self, dir: i32) -> bool {
        if self.history.index.is_none() && dir == -1 {
            self.stash = prompt_copy(&self.draft);
        }
        let next = move_prompt_history(&self.history, dir, self.area.plain_text(), self.area.cursor_offset());
        if let (true, Some(_), Some(cursor)) = (next.apply, next.text.clone(), next.cursor) {
            self.history = next.state.clone();
            let value = match next.state.index {
                None => self.stash.clone(),
                Some(i) => next.state.items.get(i).cloned().unwrap_or(RunPrompt::text(next.text.clone().unwrap_or_default())),
            };
            self.restore(&value, Some(cursor));
            return true;
        }
        let end = self.area.end_offset();
        if dir == -1 {
            if self.area.cursor_offset() == 0 {
                return false;
            }
            if self.area.visual_cursor(self.width).0 == 0 {
                self.area.set_cursor_offset(0);
                return true;
            }
            self.area.move_up(self.width);
            return true;
        }
        if self.area.cursor_offset() == end {
            return false;
        }
        let rows = self.area.virtual_line_count(self.width);
        if self.area.visual_cursor(self.width).0 + 1 >= rows {
            self.area.set_cursor_offset(end);
            return true;
        }
        self.area.move_down(self.width);
        true
    }

    /// Shell mode toggle on `!` at offset 0.
    pub fn enter_shell(&mut self) -> bool {
        if self.shell || self.area.cursor_offset() != 0 {
            return false;
        }
        self.set_shell_mode(true);
        true
    }

    pub fn leave_shell(&mut self, backspace: bool) -> bool {
        if !self.shell {
            return false;
        }
        if backspace && self.area.cursor_offset() != 0 {
            return false;
        }
        self.set_shell_mode(false);
        true
    }

    pub fn menu_move(&mut self, host: &dyn Host, term_width: usize, dir: i64) {
        let count = self.options(host, term_width).len() as i64;
        let limit = self.menu_limit(host, term_width) as i64;
        self.menu.move_by(dir, count, limit);
    }

    pub fn autocomplete_hide(&mut self) {
        self.cancel_autocomplete();
    }

    /// `prompt.autocomplete.select` / `.complete`.
    pub fn autocomplete_select(&mut self, host: &mut dyn Host, term_width: usize, complete: bool) {
        let options = self.options(host, term_width);
        if self.mode == MenuMode::Slash && options.is_empty() {
            self.hide();
            return;
        }
        if complete
            && let Some(PromptOption { kind: OptionKind::Mention { directory: true, .. }, .. }) = options.get(self.menu.selected as usize)
        {
            self.expand(host, term_width);
            return;
        }
        self.select(host, term_width, None, Delivery::Steer);
    }

    /// Paste text (bracketed paste).
    pub fn paste(&mut self, host: &mut dyn Host, text: &str) {
        let normalized = locale::strip_ansi(text).replace("\r\n", "\n").replace('\r', "\n");
        self.area.insert_text(&normalized);
        self.hide();
        self.sync_draft();
        self.sync_rows();
        let _ = host;
    }

    /// Track view changes (`createEffect` on `input.view()`): leaving the prompt saves the draft,
    /// returning restores it.
    pub fn view_changed(&mut self, kind: &str) {
        if kind == self.prev_view {
            return;
        }
        if self.prev_view == "prompt" {
            self.sync_draft();
        }
        self.hide();
        self.prev_view = kind.to_string();
        if kind == "prompt" {
            let draft = self.draft.clone();
            self.restore(&draft, None);
        }
    }

    /// `replacePrompt` (undo of a queued prompt).
    pub fn replace_prompt(&mut self, prompt: &RunPrompt) {
        self.restore(prompt, None);
    }

    /// Render `RunPromptBody` at (0, y) of `buf`: padding, the rail and the textarea. Returns the
    /// cursor position when the composer is focused.
    #[allow(clippy::too_many_arguments)]
    pub fn render(&mut self, buf: &mut Buffer, y: usize, width: usize, l: &PromptLayout, rail: Color, text: Color, muted: Color, mono: bool, placeholder: &str) -> (usize, usize) {
        let inner = width.saturating_sub(4).max(1);
        self.width = inner;
        let rows = self.area.rows(inner, text);
        let visible = l.textarea.min(self.visual_rows().max(1)).max(TEXTAREA_MIN_ROWS);
        let (crow, ccol) = self.area.visual_cursor(inner);
        let scroll = if crow >= visible { crow + 1 - visible } else { 0 };
        let top = y + l.padding;
        for r in 0..visible {
            buf.draw_chunks(0, (top + r) as i32, &[StyledChunk::plain(if mono { "|" } else { "┃" }, rail)], 1);
        }
        if self.area.plain_text().is_empty() && !placeholder.is_empty() {
            let lines = layout(&[StyledChunk::plain(placeholder, muted)], inner, WrapMode::Word, false);
            for (i, l) in lines.iter().take(visible).enumerate() {
                buf.draw_chunks(2, (top + i) as i32, &l.chunks, inner);
            }
        } else {
            for (i, r) in rows.iter().skip(scroll).take(visible).enumerate() {
                buf.draw_chunks(2, (top + i) as i32, &r.chunks, inner);
            }
        }
        (2 + ccol.min(inner), top + crow - scroll)
    }

    /// Lines used for footer height (`lines()`).
    pub fn lines(&self) -> usize {
        self.lines
    }

    /// Rows the textarea shows at layout `l`.
    pub fn shown_rows(&self, l: &PromptLayout) -> usize {
        l.textarea.min(self.visual_rows().max(1)).max(TEXTAREA_MIN_ROWS)
    }
}
