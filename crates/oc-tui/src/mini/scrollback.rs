//! Scrollback entries (`src/mini/scrollback.surface.ts`, `scrollback.writer.tsx`, `scrollback.shared.ts`):
//! retained streaming surfaces for assistant/reasoning/tool text, static writers for everything else,
//! separators between entry groups and the turn summary. Output is a sequence of [`Commit`]s for the
//! split-footer renderer.

use super::cell::{Buffer, Color, Glyph, StyledChunk, attr};
use super::entry_body::{BodyOptions, entry_body, entry_can_stream, entry_done, entry_flags};
use super::markdown::{self, MdStyle, Rendered, blit};
use super::marked::ParseState;
use super::structured;
use super::text::{WrapMode, layout_first};
use super::theme::{EntryTheme, MiniSyntax, RunTheme};
use super::tool::{EntryBody, tool_structured_final};
use super::types::*;

/// One scrollback commit: rows rendered at the terminal width.
#[derive(Clone, Debug, PartialEq)]
pub struct Commit {
    pub buf: Buffer,
    pub row_columns: usize,
    pub start_on_new_line: bool,
    pub trailing_newline: bool,
}

/// Where commits go: the split-footer renderer.
pub trait Sink {
    fn width(&self) -> usize;
    /// `getPendingSplitTailColumn()`: the output column after everything queued so far.
    fn tail_column(&self) -> usize;
    fn commit(&mut self, c: Commit);
}

/// `entryGroupKey(commit)`.
pub fn entry_group_key(c: &StreamCommit) -> Option<String> {
    let part = c.part_id.as_ref()?;
    let key = format!("{}\u{0}{}", c.message_id.as_deref().unwrap_or(""), part);
    if tool_structured_final(c) {
        return Some(format!("tool:{key}:final"));
    }
    let kind = match c.kind {
        EntryKind::System => "system",
        EntryKind::User => "user",
        EntryKind::Assistant => "assistant",
        EntryKind::Reasoning => "reasoning",
        EntryKind::Tool => "tool",
        EntryKind::Error => "error",
    };
    Some(format!("{kind}:{key}"))
}

/// `sameEntryGroup(left, right)`.
pub fn same_entry_group(left: Option<&StreamCommit>, right: &StreamCommit) -> bool {
    let Some(left) = left else { return false };
    match (entry_group_key(left), entry_group_key(right)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn entry_layout_block(c: &StreamCommit, body: &EntryBody) -> bool {
    if c.image.is_some() {
        return true;
    }
    if c.kind == EntryKind::Tool {
        if matches!(body, EntryBody::Structured(_) | EntryBody::Markdown(_)) {
            return true;
        }
        if c.phase == Phase::Progress && c.tool_state == Some(ToolStateKind::Completed)
            && let EntryBody::Text(t) = body
            && t.contains('\n')
        {
            return true;
        }
        return false;
    }
    true
}

/// `separatorRows(prev, next, body)`.
pub fn separator_rows(prev: Option<&StreamCommit>, next: &StreamCommit, body: &EntryBody) -> usize {
    let Some(p) = prev else { return 0 };
    if same_entry_group(Some(p), next) {
        return 0;
    }
    let prev_body = entry_body(p, BodyOptions::default());
    if !entry_layout_block(p, &prev_body) && !entry_layout_block(next, body) {
        return 0;
    }
    1
}

fn entry_failed(c: &StreamCommit) -> bool {
    c.kind == EntryKind::Tool && (c.tool_state == Some(ToolStateKind::Error) || c.part.as_ref().is_some_and(|p| p.status == ToolStatus::Error))
}

/// `entryLook(commit, theme)`: `(fg, attrs)`.
pub fn entry_look(c: &StreamCommit, t: &EntryTheme) -> (Color, u8) {
    if c.kind == EntryKind::User {
        return (t.user.body, 0);
    }
    if entry_failed(c) {
        return (t.error.body, attr::BOLD);
    }
    if c.phase == Phase::Final {
        return (t.system.body, 0);
    }
    if c.kind == EntryKind::Tool && c.phase == Phase::Start {
        return (t.tool.start.unwrap_or(t.tool.body), 0);
    }
    match c.kind {
        EntryKind::Assistant => (t.assistant.body, 0),
        EntryKind::Reasoning => (t.reasoning.body, 0),
        EntryKind::Error => (t.error.body, attr::BOLD),
        EntryKind::Tool => (t.tool.body, 0),
        _ => (t.system.body, 0),
    }
}

/// `entryColor(commit, theme)`.
pub fn entry_color(c: &StreamCommit, t: &RunTheme) -> Color {
    match c.kind {
        EntryKind::Assistant => t.entry.assistant.body,
        EntryKind::Reasoning => t.entry.reasoning.body,
        _ if entry_failed(c) => t.entry.error.body,
        EntryKind::Tool => t.block.text,
        _ => entry_look(c, &t.entry).0,
    }
}

/// Empty syntax used when the theme has none (`RUN_THEME_FALLBACK` / mono).
fn syntax_of(t: &RunTheme) -> MiniSyntax {
    t.block.syntax.clone().unwrap_or_default()
}

fn text_rows(content: &str, fg: Color, attrs: u8, width: usize, first: usize) -> Buffer {
    let chunks = vec![StyledChunk { text: content.to_string(), fg, bg: Color::TRANSPARENT, attrs }];
    let lines = layout_first(&chunks, width, WrapMode::Word, first);
    let mut buf = Buffer::new(width as u16, lines.len().max(1) as u16);
    for (i, l) in lines.iter().enumerate() {
        buf.draw_chunks(0, i as i32, &l.chunks, width);
    }
    buf
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActiveKind {
    Text,
    Code,
    Markdown,
}

struct Active {
    kind: ActiveKind,
    filetype: Option<String>,
    commit: StreamCommit,
    content: String,
    committed_rows: usize,
    committed_blocks: usize,
    pending_spacer: usize,
    rendered: bool,
    start_on_new_line: bool,
    first_line_offset: usize,
    commits: usize,
    parse: Option<ParseState>,
}

pub struct ScrollbackStream {
    tail: Option<StreamCommit>,
    rendered: Option<StreamCommit>,
    active: Option<Active>,
    pub wrote: bool,
    pub mono: bool,
    pub image_preview: bool,
    pub shell_output: bool,
    pub theme: RunTheme,
    destroyed: bool,
}

impl ScrollbackStream {
    pub fn new(theme: RunTheme, wrote: bool, mono: bool, shell_output: bool, image_preview: bool) -> Self {
        ScrollbackStream { tail: None, rendered: None, active: None, wrote, mono, image_preview, shell_output, theme, destroyed: false }
    }

    pub fn destroy(&mut self) {
        self.destroyed = true;
        self.active = None;
    }

    pub fn set_theme(&mut self, theme: RunTheme) {
        self.theme = theme;
    }

    fn mark_rendered(&mut self, c: Option<StreamCommit>) {
        if let Some(c) = c {
            self.rendered = Some(c);
        }
    }

    fn write_spacer(&mut self, sink: &mut dyn Sink, rows: usize) {
        if rows == 0 {
            return;
        }
        let w = sink.width().max(1);
        sink.commit(Commit { buf: Buffer::new(w as u16, 1), row_columns: w, start_on_new_line: true, trailing_newline: true });
        self.wrote = false;
    }

    fn create_entry(&self, sink: &dyn Sink, commit: &StreamCommit, body: &EntryBody) -> Active {
        let flags = entry_flags(commit);
        let tail = sink.tail_column();
        let width = sink.width();
        let first_line_offset = if !flags.start_on_new_line && tail > 0 && tail < width { tail } else { 0 };
        let (kind, filetype) = match body {
            EntryBody::Text(_) => (ActiveKind::Text, None),
            EntryBody::Code { filetype, .. } => (ActiveKind::Code, filetype.clone()),
            _ => (ActiveKind::Markdown, None),
        };
        let rows = separator_rows(self.rendered.as_ref(), commit, body);
        Active {
            kind,
            filetype,
            commit: commit.clone(),
            content: String::new(),
            committed_rows: 0,
            committed_blocks: 0,
            pending_spacer: if rows > 0 { rows } else if self.rendered.is_none() && self.wrote { 1 } else { 0 },
            rendered: false,
            start_on_new_line: flags.start_on_new_line,
            first_line_offset,
            commits: 0,
            parse: None,
        }
    }

    fn surface(&self, a: &mut Active, width: usize, done: bool) -> (Buffer, Option<Rendered>) {
        let syntax = syntax_of(&self.theme);
        match a.kind {
            ActiveKind::Text => {
                let (fg, attrs) = entry_look(&a.commit, &self.theme.entry);
                (text_rows(&a.content, fg, attrs, width, a.first_line_offset), None)
            }
            ActiveKind::Code => {
                let st = MdStyle { syntax: &syntax, fg: entry_color(&a.commit, &self.theme), mono: self.mono };
                (markdown::code_block(&a.content, a.filetype.as_deref(), width, &st, true), None)
            }
            ActiveKind::Markdown => {
                let st = MdStyle { syntax: &syntax, fg: entry_color(&a.commit, &self.theme), mono: self.mono };
                let state = super::marked::parse_incremental(&a.content, a.parse.as_ref(), 2);
                let state = if done { super::marked::parse_incremental(&a.content, Some(&state), 0) } else { state };
                let r = markdown::render_state(&state, width, &st);
                a.parse = Some(state);
                (r.compose(width as u16), Some(r))
            }
        }
    }

    fn commit_rows(sink: &mut dyn Sink, a: &mut Active, buf: &Buffer, start: usize, end: usize, trailing: bool) {
        if start == end {
            return;
        }
        let w = buf.width;
        let mut out = Buffer::new(w, (end - start) as u16);
        for y in start..end {
            for x in 0..w as usize {
                let i = (y - start) * w as usize + x;
                out.cells[i] = buf.cells[y * w as usize + x].clone();
            }
        }
        sink.commit(Commit { buf: out, row_columns: w as usize, start_on_new_line: a.commits == 0 && a.start_on_new_line, trailing_newline: trailing });
        a.commits += 1;
    }

    /// `flushActive(done, trailingNewline)`.
    fn flush_active(&mut self, sink: &mut dyn Sink, done: bool, trailing: bool) -> bool {
        let Some(mut a) = self.active.take() else { return false };
        let width = sink.width().max(1);
        let (buf, rendered) = self.surface(&mut a, width, done);
        let result = match rendered {
            None => {
                let height = buf.height as usize;
                let target = if done { height } else { a.committed_rows.max(height.saturating_sub(1)) };
                if target <= a.committed_rows {
                    false
                } else {
                    let spacer = std::mem::take(&mut a.pending_spacer);
                    self.write_spacer(sink, spacer);
                    let t = if done && target == height { trailing } else { false };
                    let from = a.committed_rows;
                    Self::commit_rows(sink, &mut a, &buf, from, target, t);
                    a.committed_rows = target;
                    a.rendered = true;
                    true
                }
            }
            Some(r) => {
                let target = if done { r.blocks.len() } else { r.stable.min(r.blocks.len()) };
                if target <= a.committed_blocks || r.blocks.is_empty() {
                    false
                } else {
                    let ys = r.ys();
                    let start = ys[a.committed_blocks];
                    let end = if target < r.blocks.len() { ys[target] } else { ys[target - 1] + r.blocks[target - 1].buf.height as usize };
                    let t = if done && target == r.blocks.len() { trailing } else { false };
                    let spacer = std::mem::take(&mut a.pending_spacer);
                    self.write_spacer(sink, spacer);
                    Self::commit_rows(sink, &mut a, &buf, start, end, t);
                    a.committed_blocks = target;
                    a.rendered = true;
                    true
                }
            }
        };
        self.active = Some(a);
        result
    }

    /// `finishActive(trailingNewline)`.
    fn finish_active(&mut self, sink: &mut dyn Sink, trailing: bool) -> Option<StreamCommit> {
        self.active.as_ref()?;
        self.flush_active(sink, true, trailing);
        let a = self.active.take()?;
        if a.rendered { Some(a.commit) } else { None }
    }

    fn write_streaming(&mut self, sink: &mut dyn Sink, commit: &StreamCommit, body: &EntryBody) {
        let kind = match body {
            EntryBody::Text(_) => ActiveKind::Text,
            EntryBody::Code { .. } => ActiveKind::Code,
            _ => ActiveKind::Markdown,
        };
        let reuse = self.active.as_ref().is_some_and(|a| same_entry_group(Some(&a.commit), commit) && a.kind == kind);
        if !reuse {
            let done = self.finish_active(sink, false);
            self.mark_rendered(done);
            self.active = Some(self.create_entry(sink, commit, body));
        }
        let add = match body {
            EntryBody::Text(t) | EntryBody::Markdown(t) => t.clone(),
            EntryBody::Code { content, .. } => content.clone(),
            _ => String::new(),
        };
        if let Some(a) = self.active.as_mut() {
            a.commit = commit.clone();
            a.content.push_str(&add);
        }
        self.flush_active(sink, false, false);
        if self.active.as_ref().is_some_and(|a| a.rendered) {
            let c = self.active.as_ref().map(|a| a.commit.clone());
            self.mark_rendered(c);
        }
    }

    /// `append(commit)`.
    pub fn append(&mut self, sink: &mut dyn Sink, commit: StreamCommit) {
        if self.destroyed {
            return;
        }
        let same = same_entry_group(self.tail.as_ref(), &commit);
        if !same || commit.image.is_some() {
            let done = self.finish_active(sink, false);
            self.mark_rendered(done);
        }
        if let Some(summary) = &commit.summary {
            self.write_spacer(sink, 1);
            let c = turn_summary(sink.width(), &summary.agent, &summary.model, &summary.duration, &self.theme, self.mono);
            sink.commit(c);
            self.mark_rendered(Some(commit.clone()));
            self.tail = Some(commit);
            return;
        }
        let body = entry_body(&commit, BodyOptions { shell_output: Some(self.shell_output), mono: self.mono });
        if body == EntryBody::None {
            if entry_done(&commit) {
                let done = self.finish_active(sink, false);
                self.mark_rendered(done);
            }
            self.tail = Some(commit);
            return;
        }
        if !matches!(body, EntryBody::Structured(_)) && (entry_can_stream(&commit, &body) || (commit.kind == EntryKind::Tool && commit.phase == Phase::Final && matches!(body, EntryBody::Markdown(_)))) {
            self.write_streaming(sink, &commit, &body);
            if entry_done(&commit) {
                let done = self.finish_active(sink, entry_flags(&commit).trailing_newline);
                self.mark_rendered(done);
            }
            self.tail = Some(commit);
            return;
        }
        if same {
            let done = self.finish_active(sink, false);
            self.mark_rendered(done);
        }
        let rows = separator_rows(self.rendered.as_ref(), &commit, &body);
        let spaced = if rows > 0 { rows } else if self.rendered.is_none() && self.wrote { 1 } else { 0 };
        self.write_spacer(sink, spaced);
        let body = static_body(&commit, body, spaced);
        let c = self.entry_writer(sink, &commit, &body);
        sink.commit(c);
        self.mark_rendered(Some(commit.clone()));
        self.tail = Some(commit);
    }

    /// `complete(trailingNewline)`.
    pub fn complete(&mut self, sink: &mut dyn Sink, trailing: bool) {
        let done = self.finish_active(sink, trailing);
        self.mark_rendered(done);
    }

    /// `writeTurnSummary`.
    pub fn write_turn_summary(&mut self, sink: &mut dyn Sink, agent: &str, model: &str, duration: &str) {
        let commit = StreamCommit {
            kind: EntryKind::System,
            text: format!("{agent} · {model} · {duration}"),
            phase: Phase::Final,
            source: Source::System,
            summary: Some(TurnSummary { agent: agent.into(), model: model.into(), duration: duration.into() }),
            ..Default::default()
        };
        self.append(sink, commit);
    }

    /// `entryWriter({commit, body, theme, opts})`: a static snapshot.
    fn entry_writer(&self, sink: &dyn Sink, commit: &StreamCommit, body: &EntryBody) -> Commit {
        let width = sink.width().max(1);
        let flags = entry_flags(commit);
        let tail = sink.tail_column();
        let first = if !flags.start_on_new_line && tail > 0 && tail < width { tail } else { 0 };
        let buf = render_entry(commit, body, &self.theme, width, first, self.mono, true);
        Commit { buf, row_columns: width, start_on_new_line: flags.start_on_new_line, trailing_newline: flags.trailing_newline }
    }
}

/// `staticBody(commit, body, spaced)`.
fn static_body(commit: &StreamCommit, body: EntryBody, spaced: usize) -> EntryBody {
    if spaced == 0 {
        return body;
    }
    let EntryBody::Text(t) = &body else { return body };
    if commit.kind != EntryKind::Tool || commit.phase != Phase::Progress || commit.tool_state != Some(ToolStateKind::Completed) {
        return body;
    }
    match t.strip_prefix('\n') {
        Some(rest) => EntryBody::Text(rest.to_string()),
        None => body,
    }
}

/// `RunEntryContent`: render one entry body at `width`.
pub fn render_entry(commit: &StreamCommit, body: &EntryBody, theme: &RunTheme, width: usize, first: usize, mono: bool, suppress_backgrounds: bool) -> Buffer {
    let syntax = syntax_of(theme);
    if commit.compaction {
        return compaction_rule(&commit.text, theme, width);
    }
    match body {
        EntryBody::None => Buffer::new(width as u16, 1),
        EntryBody::Text(t) => {
            let (fg, attrs) = entry_look(commit, &theme.entry);
            text_rows(t, fg, attrs, width, first)
        }
        EntryBody::Code { content, filetype } => {
            let st = MdStyle { syntax: &syntax, fg: entry_color(commit, theme), mono };
            let buf = markdown::code_block(content, filetype.as_deref(), width, &st, false);
            if filetype.is_some() {
                // `drawUnstyledText={false}`: a synchronous snapshot is taken before highlighting
                // finishes, so the rows stay blank.
                Buffer::new(buf.width, buf.height)
            } else {
                buf
            }
        }
        EntryBody::Markdown(t) => {
            let st = MdStyle { syntax: &syntax, fg: entry_color(commit, theme), mono };
            let streaming = commit.phase == Phase::Progress;
            let (_, r) = markdown::render(t, None, streaming, width, &st);
            r.compose(width as u16)
        }
        EntryBody::Structured(snap) => structured::render(snap, theme, &syntax, width, mono, suppress_backgrounds),
    }
}

/// The compaction rule: `──── Compaction ────`.
fn compaction_rule(text: &str, theme: &RunTheme, width: usize) -> Buffer {
    let mut buf = Buffer::new(width as u16, 1);
    let label = super::cell::str_width(text) + 2;
    let rest = width.saturating_sub(label);
    let left = rest / 2 + rest % 2;
    let right = rest / 2;
    let c = theme.block.muted;
    for x in 0..left {
        buf.set_blend(x as i32, 0, Glyph::Text("─".into()), c, Color::TRANSPARENT, 0);
    }
    buf.draw_chunks((left + 1) as i32, 0, &[StyledChunk::plain(text, c)], width.saturating_sub(left + 1));
    for x in 0..right {
        buf.set_blend((left + label + x) as i32, 0, Glyph::Text("─".into()), c, Color::TRANSPARENT, 0);
    }
    buf
}

/// `turnSummaryWriter`: `Agent · model · duration` (truncated, no wrap).
pub fn turn_summary(width: usize, agent: &str, model: &str, duration: &str, theme: &RunTheme, mono: bool) -> Commit {
    let dot = if mono { "-" } else { "·" };
    let chunks = vec![StyledChunk::plain(agent, theme.block.text), StyledChunk::plain(format!(" {dot} {model} {dot} {duration}"), theme.block.muted)];
    let lines = super::text::layout(&chunks, width.max(1), WrapMode::None, true);
    let mut buf = Buffer::new(width.max(1) as u16, 1);
    if let Some(l) = lines.first() {
        buf.draw_chunks(0, 0, &l.chunks, width);
    }
    Commit { buf, row_columns: width.max(1), start_on_new_line: true, trailing_newline: false }
}

/// Blit helper re-export for writers.
pub fn place(dst: &mut Buffer, src: &Buffer, x: i32, y: i32) {
    blit(dst, src, x, y);
}

/// `snapshotRowWidths`: columns of each row up to `row_columns`, without trailing empty cells.
pub fn row_widths(buf: &Buffer, row_columns: usize) -> Vec<usize> {
    (0..buf.height)
        .map(|y| {
            let mut x = row_columns.min(buf.width as usize);
            while x > 0 && buf.get(x as u16 - 1, y).glyph == Glyph::Empty {
                x -= 1;
            }
            x
        })
        .collect()
}

/// `advanceSplitTailColumn`.
fn advance_tail(mut tail: usize, columns: usize, width: usize) -> usize {
    let mut remaining = columns;
    while remaining > 0 {
        if tail >= width {
            tail = 0;
        }
        let step = remaining.min(width - tail);
        tail += step;
        remaining -= step;
        if remaining > 0 && tail >= width {
            tail = 0;
        }
    }
    tail
}

/// `getSplitTailColumnAfterCommit`: the output column after `commit` is appended at `tail`.
pub fn tail_after_commit(commit: &Commit, tail: usize, width: usize) -> usize {
    let mut tail = if commit.start_on_new_line && tail > 0 { 0 } else { tail };
    let widths = row_widths(&commit.buf, commit.row_columns);
    for (i, w) in widths.iter().enumerate() {
        tail = advance_tail(tail, *w, width.max(1));
        if i + 1 < widths.len() || commit.trailing_newline {
            tail = 0;
        }
    }
    tail
}

#[cfg(test)]
#[path = "scrollback_golden.rs"]
mod golden;
