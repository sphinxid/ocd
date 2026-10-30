//! Incremental transcript rendering (an ocd performance addition; output is identical to
//! [`App::render_transcript_lines`], which stays as the reference implementation).
//!
//! The transcript is a list of rows (`transcript::build_rows`). Each row's rendered lines are
//! cached together with two hashes:
//!
//! - a cheap **fingerprint**: the row structure, the content revisions ([`Rev`]) of every message
//!   the row reads, a signature of the per-row UI state it reads (expanded toggles, the pending
//!   permission, pending inputs, background shells and their output, running subagent sessions)
//!   and, for footers with a scheduled retry, the countdown second;
//! - a **content key**: like the fingerprint, but with the content of each part instead of the
//!   whole message's revision, so the sibling rows of a streaming message (its tools, its reasoning)
//!   are reused while only the part that changed is rendered again.
//!
//! On every frame [`TranscriptCache::sync`] stamps fresh revisions on changed messages, finds the
//! first message that changed and rebuilds the row list only from the start of that turn (a user
//! message right after an `idle` marker), reusing every row whose fingerprint or content key is
//! unchanged. Settings that affect every row (width, theme, config, models, agent colours) drop the
//! rendered rows; the revert boundary, permission blocking and pending inputs rebuild the row list
//! (rendered rows are still reused by fingerprint).
//!
//! Rows are rendered lazily, bottom-up: a frame renders only the rows within the visible window's
//! distance from the bottom, then keeps rendering the rows above it within a small time budget, so
//! a resize or theme switch of a long session shows the visible part at once and completes the
//! rest over the next frames. Until every row is rendered the line count and the message marks are
//! unknown; the window is then addressed from the bottom, and navigation that needs the marks
//! completes the render first ([`App::transcript_complete`]). Drawing reads the visible window by
//! reference.

use super::App;
use crate::api::types::*;
use crate::ui::text::*;
use crate::ui::transcript::{self, Entry, Node, RCtx, Row};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};

static BLANK: SLine = SLine { segs: Vec::new(), bg: None, click: None };

/// Counters for tests and benchmarks.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub syncs: u64,
    /// Rows rendered (cache misses).
    pub rendered: u64,
    /// Full row-list rebuilds.
    pub full: u64,
    /// Row-list rebuilds from a turn start.
    pub partial: u64,
}

/// Inputs of `build_rows` beyond the message list. Any change rebuilds the whole row list (the
/// rendered rows themselves are still reused when their fingerprint matches).
#[derive(Clone, Debug, PartialEq)]
struct BuildKey {
    inputs: Vec<String>,
    compactions: Vec<String>,
    blocked: Vec<String>,
    turn_tokens: bool,
    verbosity: String,
    legacy: bool,
    boundary: Option<String>,
}

/// Per-row UI state, compared each frame.
#[derive(Clone, Debug, Default, PartialEq)]
struct DepSnap {
    expanded: HashMap<String, bool>,
    permission_tool: Option<String>,
    pending_inputs: HashMap<String, String>,
    shells: HashSet<String>,
    shell_output: HashMap<String, String>,
    running: HashSet<String>,
}

/// The same state, borrowed from the app for this frame.
struct DepView<'a> {
    expanded: &'a HashMap<String, bool>,
    permission_tool: Option<String>,
    pending_inputs: HashMap<String, String>,
    shells: HashSet<String>,
    shell_output: &'a HashMap<String, String>,
    running: HashSet<String>,
}

impl DepView<'_> {
    fn matches(&self, s: &DepSnap) -> bool {
        *self.expanded == s.expanded
            && self.permission_tool == s.permission_tool
            && self.pending_inputs == s.pending_inputs
            && self.shells == s.shells
            && self.running == s.running
            && *self.shell_output == s.shell_output
    }
    fn snapshot(&self) -> DepSnap {
        DepSnap {
            expanded: self.expanded.clone(),
            permission_tool: self.permission_tool.clone(),
            pending_inputs: self.pending_inputs.clone(),
            shells: self.shells.clone(),
            shell_output: self.shell_output.clone(),
            running: self.running.clone(),
        }
    }
}

/// What a row reads besides the row structure.
#[derive(Clone, Debug, Default)]
struct RowInfo {
    /// Visible indices of the messages whose content the row reads.
    msgs: Vec<usize>,
    /// Ids the per-row UI state is keyed by (message, tool, shell and subagent session ids).
    tokens: Vec<String>,
    /// Group ids (expanded toggles).
    groups: Vec<String>,
    /// `retry.at` of footers with a scheduled retry (time-dependent).
    retries: Vec<i64>,
}

struct CRow {
    row: Row,
    info: RowInfo,
    fp: u64,
    key: u64,
    deps: u64,
    time: u64,
    /// Rendered lines; `None` until the row is rendered (lazily, bottom-up).
    lines: Option<Vec<SLine>>,
    /// Mark candidate: (message id, user).
    cand: Option<(String, bool)>,
    /// Index of the row's leading blank line in the flattened transcript (valid when complete).
    start: usize,
}

impl CRow {
    /// Lines the row occupies (a blank separator plus its lines; 0 when it renders nothing).
    fn span(&self) -> usize {
        match &self.lines {
            Some(l) if !l.is_empty() => l.len() + 1,
            _ => 0,
        }
    }
    /// The row's lines bottom-up, with its leading blank last.
    fn rev_lines(&self) -> impl Iterator<Item = &SLine> {
        let lines: &[SLine] = self.lines.as_deref().unwrap_or(&[]);
        let lead: &[SLine] = if lines.is_empty() { &[] } else { std::slice::from_ref(&BLANK) };
        lines.iter().rev().chain(lead.iter())
    }
}

/// Time a budgeted sync may spend rendering rows beyond the visible window.
const RENDER_BUDGET: std::time::Duration = std::time::Duration::from_millis(8);

#[derive(Default)]
pub struct TranscriptCache {
    style: Option<u64>,
    built: bool,
    build: Option<BuildKey>,
    deps: Option<DepSnap>,
    revs: Vec<u64>,
    index: HashMap<String, usize>,
    rows: Vec<CRow>,
    /// Turn starts the row list can be rebuilt from: (visible message index, row index).
    restarts: Vec<(usize, usize)>,
    /// Rows with a retry countdown.
    timed: Vec<usize>,
    marks: Vec<(usize, String, bool)>,
    mark_ids: HashSet<String>,
    body: usize,
    extra: Vec<SLine>,
    hint_msg: Option<usize>,
    revert_count: usize,
    /// Rows not rendered yet (ascending).
    pending: Vec<usize>,
    /// First row whose line start (and marks) must be recomputed once every row is rendered.
    layout_from: Option<usize>,
    /// Every row is rendered and the line starts and marks are valid.
    complete: bool,
    sid: String,
    width: usize,
    /// Background render budget per budgeted sync (default [`RENDER_BUDGET`]; tests set it).
    pub budget: Option<std::time::Duration>,
    pub stats: Stats,
}

/// `io::Write` adapter that feeds serialized bytes into a hasher.
struct HashWriter<'a>(&'a mut DefaultHasher);

impl std::io::Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn hash_ser<T: serde::Serialize>(v: &T, h: &mut DefaultHasher) {
    let _ = serde_json::to_writer(HashWriter(h), v);
}

fn walk<'r>(nodes: &'r [Node], level: usize, f: &mut impl FnMut(&'r Entry), g: &mut impl FnMut(transcript::GroupKind, &'r [Node], usize)) {
    for n in nodes {
        match n {
            Node::Entry(e) => f(e),
            Node::Group { kind, children, .. } => {
                g(*kind, children, level);
                walk(children, level + 1, f, g);
            }
        }
    }
}

fn walk_row<'r>(row: &'r Row, f: &mut impl FnMut(&'r Entry), g: &mut impl FnMut(transcript::GroupKind, &'r [Node], usize)) {
    match row {
        Row::Entry(e) => f(e),
        Row::Group { kind, children, .. } => {
            g(*kind, children, 0);
            walk(children, 1, f, g);
        }
    }
}

/// Visible messages: queued prompts and messages past the revert boundary are hidden. Borrowed
/// when that leaves a prefix of the list (the common case).
fn visible<'a>(all: &'a [Message], queued: &HashSet<&str>, boundary: Option<&str>) -> Cow<'a, [Message]> {
    if queued.is_empty() && boundary.is_none() {
        return Cow::Borrowed(all);
    }
    let keep = |m: &Message| !queued.contains(m.id.as_str()) && boundary.is_none_or(|b| m.id.as_str() < b);
    let n = all.iter().position(|m| !keep(m)).unwrap_or(all.len());
    if all[n..].iter().all(|m| !keep(m)) {
        return Cow::Borrowed(&all[..n]);
    }
    Cow::Owned(all.iter().filter(|m| keep(m)).cloned().collect())
}

fn has_text(m: &Message) -> bool {
    m.content.iter().any(|c| matches!(c, Content::Text { text, .. } if !text.trim().is_empty()))
}

/// Everything rendering borrows from the app, built only when a row has to be rendered.
pub(super) struct Env<'a> {
    pub models: &'a [ModelInfo],
    pub colors: HashMap<String, crate::theme::Rgba>,
    pub expanded: &'a HashMap<String, bool>,
    pub permission_tool: Option<String>,
    pub pending_inputs: HashMap<String, String>,
    pub shells: HashSet<String>,
    pub running: HashSet<String>,
    pub session_agent: Option<String>,
    pub home: String,
}

static NO_EXPANDED: std::sync::LazyLock<HashMap<String, bool>> = std::sync::LazyLock::new(HashMap::new);

impl App {
    /// Models of the current location, borrowed.
    pub fn models_ref(&self) -> &[ModelInfo] {
        self.loc().and_then(|l| l.models.as_deref()).unwrap_or(&[])
    }

    fn permission_tool(&self, sid: &str) -> Option<String> {
        if self.autoaccept {
            return None;
        }
        self.store.permissions.get(sid).and_then(|l| l.first()).and_then(|p| p.tool_id()).map(|s| s.to_string())
    }

    fn pending_inputs(&self, sid: &str) -> HashMap<String, String> {
        self.store.pending.get(sid).map(|p| p.iter().filter(|i| i.kind == "user").map(|i| (i.id.clone(), i.delivery.clone())).collect()).unwrap_or_default()
    }

    fn shell_ids(&self) -> HashSet<String> {
        self.store.locations.values().flat_map(|l| l.shells.iter().map(|s| s.id.clone())).collect()
    }

    fn running_ids(&self) -> HashSet<String> {
        self.store.active.iter().filter(|(_, v)| **v).map(|(k, _)| k.clone()).collect()
    }

    pub(super) fn transcript_env(&self, sid: &str) -> Env<'_> {
        Env {
            models: self.models_ref(),
            colors: self.agent_colors(),
            expanded: self.views.get(sid).map(|v| &v.expanded).unwrap_or(&NO_EXPANDED),
            permission_tool: self.permission_tool(sid),
            pending_inputs: self.pending_inputs(sid),
            shells: self.shell_ids(),
            running: self.running_ids(),
            session_agent: self.store.session(sid).and_then(|s| s.agent.clone()),
            home: self.location_dir(),
        }
    }

    pub(super) fn transcript_ctx<'a>(&'a self, env: &'a Env<'a>, messages: &'a [Message], width: usize, index: Option<&'a HashMap<String, usize>>, legacy: Option<bool>) -> RCtx<'a> {
        RCtx {
            theme: self.theme(),
            syntax: &self.syntax,
            thinking_syntax: &self.thinking_syntax,
            config: &self.config,
            width,
            term_width: self.size.0 as usize,
            models: env.models,
            messages,
            session_agent: env.session_agent.as_deref(),
            agent_colors: &env.colors,
            default_agent_color: self.default_agent_color(),
            expanded: env.expanded,
            permission_tool: env.permission_tool.as_deref(),
            pending_inputs: &env.pending_inputs,
            now: self.now(),
            animations: self.config.animations(),
            redo_key: self.keybinds.shortcut("session.redo"),
            running_shells: &env.shells,
            shell_output: &self.shell_output,
            running_sessions: &env.running,
            home: Some(env.home.clone()),
            index,
            legacy,
            providers: self.loc().and_then(|l| l.providers.as_deref()).unwrap_or(&[]),
            usage: self.usage_info(),
            models_key: self.models_key(),
        }
    }

    /// Hash of everything that affects every row.
    fn transcript_style(&self, sid: &str, width: usize) -> u64 {
        let mut h = DefaultHasher::new();
        sid.hash(&mut h);
        width.hash(&mut h);
        self.size.0.hash(&mut h);
        self.themes.epoch.hash(&mut h);
        self.style_epoch.hash(&mut h);
        (self.themes.effective_mode() == crate::theme::Mode::Light).hash(&mut h);
        hash_ser(&self.config.raw, &mut h);
        for m in self.models_ref() {
            (&m.provider_id, &m.id, &m.name).hash(&mut h);
        }
        let mut colors: Vec<(String, crate::theme::Rgba)> = self.agent_colors().into_iter().collect();
        colors.sort_by(|a, b| a.0.cmp(&b.0));
        for (k, c) in &colors {
            (k, c.r, c.g, c.b, c.a, c.src).hash(&mut h);
        }
        let d = self.default_agent_color();
        (d.r, d.g, d.b, d.a, d.src).hash(&mut h);
        self.store.session(sid).and_then(|s| s.agent.as_deref()).hash(&mut h);
        self.location_dir().hash(&mut h);
        // The usage-limit notices read the providers, the workspace name and the `model.list` key.
        for p in self.loc().and_then(|l| l.providers.as_deref()).unwrap_or(&[]) {
            (&p.id, &p.integration_id).hash(&mut h);
        }
        self.usage_info().and_then(|u| u.workspace.name.as_deref()).hash(&mut h);
        self.models_key().hash(&mut h);
        h.finish()
    }

    /// Bring the transcript cache up to date for session `sid` at `width` columns, rendering
    /// every row.
    pub fn transcript_sync(&mut self, sid: &str, width: usize) {
        self.transcript_sync_need(sid, width, None);
    }

    /// Like [`App::transcript_sync`], but with `Some(n)` only the rows within `n` lines of the bottom
    /// must be rendered; more are rendered within [`RENDER_BUDGET`].
    pub fn transcript_sync_need(&mut self, sid: &str, width: usize, need: Option<usize>) {
        if let Some(list) = self.store.messages.get_mut(sid) {
            for m in list.iter_mut().filter(|m| m.rev.0 == 0) {
                m.rev = Rev::next();
            }
        }
        let mut cache = std::mem::take(&mut self.tcache);
        cache.sync(self, sid, width, need);
        self.tcache = cache;
    }

    /// Finish rendering the current session's transcript (line count and marks become valid).
    pub fn transcript_complete(&mut self) {
        if let Some(sid) = self.session_id().map(str::to_string)
            && self.tcache.sid == sid
            && !self.tcache.complete
        {
            let w = self.tcache.width;
            self.transcript_sync(&sid, w);
            self.transcript_lines = self.tcache.len() + self.views.get(&sid).map(|v| v.nav_slack).unwrap_or(0);
        }
    }

    /// The current session's transcript still has rows to render (keep drawing frames).
    pub fn transcript_pending(&self) -> bool {
        !self.tcache.complete && !self.tcache.sid.is_empty() && self.session_id() == Some(self.tcache.sid.as_str())
    }

    /// Marks of the last synced transcript: (line, message id, user).
    pub fn transcript_marks(&self) -> &[(usize, String, bool)] {
        self.tcache.marks()
    }
}

impl TranscriptCache {
    /// Number of transcript lines (valid when [`TranscriptCache::is_complete`]).
    pub fn len(&self) -> usize {
        self.body + self.extra.len()
    }

    /// Every row is rendered: [`TranscriptCache::len`], the marks and top-based access are valid.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// The `h` lines ending `from_bottom` lines above the bottom, followed by `slack` blank lines
    /// (fewer when the transcript is shorter). Only needs the rows within that distance.
    pub fn window_bottom(&self, from_bottom: usize, h: usize, slack: usize) -> Vec<&SLine> {
        let mut out: Vec<&SLine> = std::iter::repeat_n(&BLANK, slack)
            .chain(self.extra.iter().rev())
            .chain(self.rows.iter().rev().flat_map(|r| r.rev_lines()))
            .skip(from_bottom)
            .take(h)
            .collect();
        out.reverse();
        out
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn marks(&self) -> &[(usize, String, bool)] {
        &self.marks
    }

    /// Lines from `offset` on, by reference.
    pub fn iter_from(&self, offset: usize) -> impl Iterator<Item = &SLine> + '_ {
        let ri = self.rows.partition_point(|r| r.start + r.span() <= offset);
        let first = self.rows.get(ri).map(|r| r.start).unwrap_or(self.body);
        let body = self.rows[ri..].iter().flat_map(|r| {
            let lines: &[SLine] = r.lines.as_deref().unwrap_or(&[]);
            let lead: &[SLine] = if lines.is_empty() { &[] } else { std::slice::from_ref(&BLANK) };
            lead.iter().chain(lines.iter())
        });
        body.chain(self.extra.iter()).skip(offset.saturating_sub(first))
    }

    /// Owned copy of lines `a..b`.
    pub fn range(&self, a: usize, b: usize) -> Vec<SLine> {
        if b <= a {
            return vec![];
        }
        self.iter_from(a).take(b - a).cloned().collect()
    }

    /// The whole transcript (tests).
    pub fn to_vec(&self) -> Vec<SLine> {
        self.range(0, self.len())
    }

    pub(super) fn sync(&mut self, app: &App, sid: &str, width: usize, need: Option<usize>) {
        self.stats.syncs += 1;
        let now = app.now();
        let style = app.transcript_style(sid, width);
        if self.style != Some(style) {
            let (stats, budget) = (self.stats, self.budget);
            *self = TranscriptCache { style: Some(style), stats, budget, sid: sid.to_string(), width, ..Default::default() };
        }
        let all = app.store.messages(sid);
        let pending = app.store.pending.get(sid).map(|p| p.as_slice()).unwrap_or(&[]);
        let queued: HashSet<&str> = pending.iter().filter(|i| i.kind == "user" && i.delivery == "queue").map(|i| i.id.as_str()).collect();
        let boundary = app.store.session(sid).and_then(|s| s.revert.as_ref().map(|r| r.message_id.clone()));
        let vis_cow = visible(all, &queued, boundary.as_deref());
        let vis: &[Message] = &vis_cow;
        let legacy = transcript::is_legacy(vis);
        let turn_tokens = app.config.turn_tokens().is_some();
        let verbosity = app.config.verbosity().to_string();
        let tps = app.config.tps();
        let inputs: HashSet<String> = pending.iter().filter(|i| i.kind != "compaction").map(|i| i.id.clone()).collect();
        let compactions: Vec<String> = pending.iter().filter(|i| i.kind == "compaction").map(|i| i.id.clone()).collect();
        let blocked: HashSet<String> = app.store.permissions.get(sid).map(|l| l.iter().filter_map(|p| p.tool_id().map(|s| s.to_string())).collect()).unwrap_or_default();
        let sorted = |s: &HashSet<String>| {
            let mut v: Vec<String> = s.iter().cloned().collect();
            v.sort();
            v
        };
        let build = BuildKey { inputs: sorted(&inputs), compactions: compactions.clone(), blocked: sorted(&blocked), turn_tokens, verbosity: verbosity.clone(), legacy, boundary: boundary.clone() };
        let mut full = !self.built || self.build.as_ref() != Some(&build);
        if full {
            self.build = Some(build);
        }

        // First message whose revision changed, and running compactions (they reorder rows).
        let mut k = None;
        let mut running_compactions = Vec::new();
        for (i, m) in vis.iter().enumerate() {
            if k.is_none() && self.revs.get(i) != Some(&m.rev.0) {
                k = Some(i);
            }
            if m.kind == "compaction" && m.status.as_deref() == Some("running") {
                running_compactions.push(i);
            }
        }
        if k.is_none() && self.revs.len() != vis.len() {
            k = Some(vis.len());
        }

        let view = DepView {
            expanded: app.views.get(sid).map(|v| &v.expanded).unwrap_or(&NO_EXPANDED),
            permission_tool: app.permission_tool(sid),
            pending_inputs: app.pending_inputs(sid),
            shells: app.shell_ids(),
            shell_output: &app.shell_output,
            running: app.running_ids(),
        };
        let deps_changed = self.deps.as_ref().is_none_or(|d| !view.matches(d));
        if deps_changed {
            self.deps = Some(view.snapshot());
        }

        // Where to rebuild the row list from: (visible message index, row index).
        let mut from: Option<(usize, usize)> = None;
        if !full && let Some(k) = k {
            from = self.restart_for(k, vis, &inputs, &running_compactions, turn_tokens, legacy);
            if from.is_none() {
                full = true;
            }
        }
        if full {
            from = Some((0, 0));
            self.stats.full += 1;
        } else if from.is_some() {
            self.stats.partial += 1;
        }

        let mut index = std::mem::take(&mut self.index);
        if let Some((u, _)) = from {
            // Index of the rebuilt part (stale entries elsewhere are verified on use).
            if u == 0 {
                index.clear();
            }
            for (i, m) in vis.iter().enumerate().skip(u) {
                index.insert(m.id.clone(), i);
            }
            self.revs.truncate(u);
            self.revs.extend(vis[u..].iter().map(|m| m.rev.0));
        }
        let env_cell: std::cell::OnceCell<Env> = std::cell::OnceCell::new();
        let light = app.themes.effective_mode() == crate::theme::Mode::Light;
        let mut first_dirty = self.rows.len();
        {
            let mut cx = Cx { app, sid, width, vis, now, legacy, tps, view: &view, env: &env_cell, index: &index, light, stats: &mut self.stats };
            if let Some((u, r)) = from {
                let rows = transcript::build_rows_with(&vis[u..], &inputs, &compactions, turn_tokens, &verbosity, &blocked, legacy);
                let old: Vec<CRow> = self.rows.drain(r..).collect();
                let mut pool = Pool::new(old);
                for row in rows {
                    let c = cx.refresh(row, &mut pool);
                    self.rows.push(c);
                }
                // Restart points of the rebuilt part.
                self.restarts.retain(|(mu, _)| *mu < u);
                for (ri, c) in self.rows.iter().enumerate().skip(r) {
                    if let Row::Entry(Entry::Message(id)) = &c.row
                        && let Some(i) = cx.pos(id)
                        && qualifies(vis, i, &inputs, turn_tokens, legacy)
                    {
                        self.restarts.push((i, ri));
                    }
                }
                self.timed.retain(|i| *i < r);
                self.timed.extend((r..self.rows.len()).filter(|i| !self.rows[*i].info.retries.is_empty()));
                self.pending.retain(|i| *i < r);
                self.pending.extend((r..self.rows.len()).filter(|i| self.rows[*i].lines.is_none()));
                first_dirty = r;
                self.hint_msg = vis.iter().rposition(|m| m.kind == "assistant" && m.time.completed.is_none());
                self.revert_count = boundary.as_ref().map(|b| all.iter().filter(|m| m.kind == "user" && m.id >= *b).count()).unwrap_or(0);
            }
            // Rows kept from before whose per-row state or countdown changed.
            let kept = first_dirty;
            let candidates: Vec<usize> = if deps_changed { (0..kept).collect() } else { self.timed.iter().copied().filter(|i| *i < kept).collect() };
            for i in candidates {
                if cx.revisit(&mut self.rows[i]) {
                    first_dirty = first_dirty.min(i);
                }
            }
        }
        if first_dirty < self.rows.len() || from.is_some() {
            self.layout_from = Some(self.layout_from.map_or(first_dirty, |l| l.min(first_dirty)));
        }

        // Trailing lines: the background-work hint (time-dependent) and the revert banner.
        self.extra.clear();
        let t = app.theme();
        if let Some(m) = self.hint_msg.and_then(|i| vis.get(i)).filter(|m| m.kind == "assistant" && m.time.completed.is_none()) {
            let long = m.content.iter().any(|c| {
                matches!(c, Content::Tool(tp) if tp.state.status == "running"
                && matches!(transcript::canonical_tool(&tp.name), "shell" | "subagent")
                && tp.time.ran.is_some_and(|r| now - r >= 3000))
            });
            if long {
                self.extra.push(SLine::blank());
                self.extra.push(SLine::new(vec![
                    Seg::new("   ", ratatui::style::Style::default()),
                    Seg::new("Press ", fg(t.text.muted)),
                    Seg::new(app.keybinds.shortcut("session.background"), fg(t.text.base)),
                    Seg::new(" to move running work to the background", fg(t.text.muted)),
                ]));
            }
        }
        if boundary.is_some() {
            let env = env_cell.get_or_init(|| app.transcript_env(sid));
            let ctx = app.transcript_ctx(env, vis, width, None, Some(legacy));
            let files = app.store.session(sid).and_then(|s| s.revert.as_ref().and_then(|r| r.files.clone())).unwrap_or_default();
            self.extra.push(SLine::blank());
            self.extra.extend(transcript::revert_banner(&ctx, self.revert_count, &files));
        }

        // Render what is still missing: the rows within `need` lines of the bottom, then more
        // bottom-up within the budget (everything when `need` is `None`).
        if !self.pending.is_empty() {
            let mut cx = Cx { app, sid, width, vis, now, legacy, tps, view: &view, env: &env_cell, index: &index, light, stats: &mut self.stats };
            let deadline = std::time::Instant::now() + self.budget.unwrap_or(RENDER_BUDGET);
            let mut lowest = usize::MAX;
            if let Some(need) = need {
                let mut covered = self.extra.len();
                let mut i = self.rows.len();
                while i > 0 && covered < need {
                    i -= 1;
                    if self.rows[i].lines.is_none() {
                        let row = self.rows[i].row.clone();
                        self.rows[i].lines = Some(cx.render(&row));
                        lowest = lowest.min(i);
                    }
                    covered += self.rows[i].span();
                }
            }
            while let Some(&i) = self.pending.last() {
                if self.rows[i].lines.is_none() {
                    if need.is_some() && std::time::Instant::now() >= deadline {
                        break;
                    }
                    let row = self.rows[i].row.clone();
                    self.rows[i].lines = Some(cx.render(&row));
                    lowest = lowest.min(i);
                }
                self.pending.pop();
            }
            if lowest != usize::MAX {
                self.layout_from = Some(self.layout_from.map_or(lowest, |l| l.min(lowest)));
            }
        }
        self.index = index;
        self.built = true;
        self.complete = self.pending.is_empty();

        // Line starts and marks from the first changed row on (once every row is rendered).
        if self.complete
            && let Some(first_dirty) = self.layout_from.take()
        {
            let first_dirty = first_dirty.min(self.rows.len());
            let start = if first_dirty == 0 { 0 } else { self.rows[first_dirty - 1].start + self.rows[first_dirty - 1].span() };
            let cut = self.marks.partition_point(|(l, _, _)| *l < start);
            for (_, id, _) in self.marks.drain(cut..) {
                self.mark_ids.remove(&id);
            }
            let mut total = start;
            for c in &mut self.rows[first_dirty..] {
                c.start = total;
                if c.span() > 0
                    && let Some((id, user)) = &c.cand
                    && !self.mark_ids.contains(id)
                {
                    self.marks.push((if *user { total + 1 } else { total }, id.clone(), *user));
                    self.mark_ids.insert(id.clone());
                }
                total += c.span();
            }
            self.body = total;
        }
    }

    /// The latest turn start at or before message `k` the row list can be rebuilt from.
    fn restart_for(&self, k: usize, vis: &[Message], inputs: &HashSet<String>, running: &[usize], turn_tokens: bool, legacy: bool) -> Option<(usize, usize)> {
        let &(u, r) = self.restarts.iter().rev().find(|(u, _)| *u <= k && *u < vis.len())?;
        if !qualifies(vis, u, inputs, turn_tokens, legacy) || running.iter().any(|i| *i < u) {
            return None;
        }
        if !matches!(self.rows.get(r).map(|c| &c.row), Some(Row::Entry(Entry::Message(id))) if *id == vis[u].id) {
            return None;
        }
        // Pending inputs are ordered last; one before `u` would move rows across the restart.
        if inputs.iter().any(|id| self.index.get(id).is_some_and(|&i| i < u && vis.get(i).is_some_and(|m| &m.id == id))) {
            return None;
        }
        Some((u, r))
    }
}

/// A user message the row list can be split at: rows before it never depend on what follows.
/// It must not be a pending input, and with turn-token rows on it must directly follow an `idle`
/// marker (the turn-usage accumulator is empty there).
fn qualifies(vis: &[Message], i: usize, inputs: &HashSet<String>, turn_tokens: bool, legacy: bool) -> bool {
    let Some(m) = vis.get(i) else { return false };
    if m.kind != "user" || inputs.contains(&m.id) {
        return false;
    }
    !turn_tokens || i == 0 || (!legacy && vis[i - 1].kind == "idle")
}

/// Old rows available for reuse, by fingerprint and by content key.
struct Pool {
    rows: Vec<Option<CRow>>,
    by_fp: HashMap<u64, usize>,
    by_key: HashMap<u64, usize>,
}

impl Pool {
    fn new(rows: Vec<CRow>) -> Pool {
        let by_fp = rows.iter().enumerate().map(|(i, c)| (c.fp, i)).collect();
        let by_key = rows.iter().enumerate().map(|(i, c)| (c.key, i)).collect();
        Pool { rows: rows.into_iter().map(Some).collect(), by_fp, by_key }
    }
    fn take(&mut self, i: usize) -> Option<CRow> {
        let c = self.rows.get_mut(i)?.take()?;
        self.by_fp.remove(&c.fp);
        self.by_key.remove(&c.key);
        Some(c)
    }
    fn by_fp(&mut self, fp: u64) -> Option<CRow> {
        let i = *self.by_fp.get(&fp)?;
        self.take(i)
    }
    fn by_key(&mut self, key: u64) -> Option<CRow> {
        let i = *self.by_key.get(&key)?;
        self.take(i)
    }
}

/// Per-sync context.
struct Cx<'a, 'b> {
    app: &'a App,
    sid: &'b str,
    width: usize,
    vis: &'b [Message],
    now: i64,
    legacy: bool,
    tps: bool,
    view: &'b DepView<'a>,
    env: &'b std::cell::OnceCell<Env<'a>>,
    index: &'b HashMap<String, usize>,
    light: bool,
    stats: &'b mut Stats,
}

impl Cx<'_, '_> {
    fn pos(&self, id: &str) -> Option<usize> {
        if let Some(&i) = self.index.get(id)
            && self.vis.get(i).is_some_and(|m| m.id == id)
        {
            return Some(i);
        }
        self.vis.iter().position(|m| m.id == id)
    }

    fn info(&self, row: &Row) -> RowInfo {
        let mut info = RowInfo::default();
        let mut groups = Vec::new();
        walk_row(
            row,
            &mut |e| match e {
                Entry::Message(id) => {
                    info.tokens.push(id.clone());
                    if let Some(i) = self.pos(id) {
                        info.msgs.push(i);
                        if let Some(s) = &self.vis[i].shell_id {
                            info.tokens.push(s.clone());
                        }
                    }
                }
                Entry::Part(r) => {
                    info.tokens.push(r.part.clone());
                    if let Some(i) = self.pos(&r.message) {
                        info.msgs.push(i);
                        if let Some(Content::Tool(t)) = transcript::resolve_part(&self.vis[i], &r.part)
                            && let Some(meta) = &t.state.metadata
                        {
                            for k in ["shellID", "sessionID", "sessionId"] {
                                if let Some(s) = meta.get(k).and_then(|v| v.as_str()) {
                                    info.tokens.push(s.to_string());
                                }
                            }
                        }
                    }
                }
                Entry::Footer(id) => {
                    if let Some(i) = self.pos(id) {
                        // The footer's duration and speed read the whole turn.
                        let lo = match transcript::turn_bounds(self.vis, i, self.legacy) {
                            Some(s) => s.min(i),
                            None if self.tps => 0,
                            None => i,
                        };
                        info.msgs.extend(lo..=i);
                        if let Some(r) = &self.vis[i].retry {
                            info.retries.push(r.at);
                        }
                    }
                }
                Entry::TurnUsage(ids) => info.msgs.extend(ids.iter().filter_map(|id| self.pos(id))),
                Entry::CompactionQueued(_) => {}
            },
            &mut |kind, children, level| groups.push(transcript::group_id(kind, children, level)),
        );
        info.groups = groups;
        info
    }

    fn deps(&self, info: &RowInfo) -> u64 {
        let v = self.view;
        let mut h = DefaultHasher::new();
        let mut exp: Vec<(&String, &bool)> = v
            .expanded
            .iter()
            .filter(|(k, _)| match k.strip_prefix("error:").or_else(|| k.strip_prefix("shell:")).or_else(|| k.strip_prefix("tool:")) {
                Some(id) => info.tokens.iter().any(|t| t == id),
                None => info.groups.iter().any(|g| g == *k),
            })
            .collect();
        exp.sort();
        exp.hash(&mut h);
        for t in &info.tokens {
            (v.permission_tool.as_deref() == Some(t.as_str())).hash(&mut h);
            v.pending_inputs.get(t).hash(&mut h);
            v.shells.contains(t).hash(&mut h);
            v.shell_output.get(t).hash(&mut h);
            v.running.contains(t).hash(&mut h);
        }
        h.finish()
    }

    fn time(&self, info: &RowInfo) -> u64 {
        if info.retries.is_empty() {
            return 0;
        }
        let mut h = DefaultHasher::new();
        for at in &info.retries {
            transcript::retry_secs(*at, self.now).hash(&mut h);
        }
        h.finish()
    }

    fn fingerprint(&self, row: &Row, info: &RowInfo, deps: u64, time: u64) -> u64 {
        let mut h = DefaultHasher::new();
        row.hash(&mut h);
        for &i in &info.msgs {
            (&self.vis[i].id, self.vis[i].rev.0).hash(&mut h);
        }
        (deps, time).hash(&mut h);
        h.finish()
    }

    /// Like the fingerprint, but parts are hashed by content, so a row whose message changed
    /// elsewhere (another part streaming) keeps its key.
    fn content_key(&self, row: &Row, info: &RowInfo, deps: u64, time: u64) -> u64 {
        let mut h = DefaultHasher::new();
        row.hash(&mut h);
        (deps, time).hash(&mut h);
        let mut parts_only = true;
        walk_row(
            row,
            &mut |e| match e {
                Entry::Part(r) => {
                    if let Some(i) = self.pos(&r.message) {
                        let m = &self.vis[i];
                        (&m.id, &m.kind, m.time.created, m.time.completed).hash(&mut h);
                        match transcript::resolve_part(m, &r.part) {
                            Some(p) => hash_ser(p, &mut h),
                            None => 0u8.hash(&mut h),
                        }
                    }
                }
                Entry::CompactionQueued(_) => {}
                _ => parts_only = false,
            },
            &mut |_, _, _| {},
        );
        if !parts_only {
            for &i in &info.msgs {
                (&self.vis[i].id, self.vis[i].rev.0).hash(&mut h);
            }
        }
        h.finish()
    }

    fn candidate(&self, row: &Row) -> Option<(String, bool)> {
        let (id, user) = match row {
            Row::Entry(Entry::Message(id)) => {
                let m = self.pos(id).map(|i| &self.vis[i])?;
                if m.kind != "user" || m.text_str().trim().is_empty() {
                    return None;
                }
                (m.id.clone(), true)
            }
            Row::Entry(Entry::Part(r)) => (r.message.clone(), false),
            Row::Entry(Entry::Footer(id)) => (id.clone(), false),
            Row::Group { children, .. } => {
                fn first(nodes: &[Node]) -> Option<&String> {
                    for n in nodes {
                        match n {
                            Node::Entry(Entry::Part(r)) => return Some(&r.message),
                            Node::Group { children, .. } => {
                                if let Some(m) = first(children) {
                                    return Some(m);
                                }
                            }
                            _ => {}
                        }
                    }
                    None
                }
                (first(children)?.clone(), false)
            }
            _ => return None,
        };
        if user || self.pos(&id).is_some_and(|i| self.vis[i].kind == "assistant" && has_text(&self.vis[i])) { Some((id, user)) } else { None }
    }

    fn render(&mut self, row: &Row) -> Vec<SLine> {
        self.stats.rendered += 1;
        let env = self.env.get_or_init(|| self.app.transcript_env(self.sid));
        let ctx = self.app.transcript_ctx(env, self.vis, self.width, Some(self.index), Some(self.legacy));
        transcript::render_row(&ctx, row, self.light)
    }

    /// A row of a rebuilt part of the list: reuse an old row by fingerprint or content key.
    fn refresh(&mut self, row: Row, pool: &mut Pool) -> CRow {
        let info = self.info(&row);
        let deps = self.deps(&info);
        let time = self.time(&info);
        let fp = self.fingerprint(&row, &info, deps, time);
        if let Some(mut c) = pool.by_fp(fp) {
            c.info = info;
            return c;
        }
        let key = self.content_key(&row, &info, deps, time);
        // Rendering is deferred (see `sync`); identical content keeps its lines.
        let lines = pool.by_key(key).and_then(|c| c.lines);
        let cand = self.candidate(&row);
        CRow { row, info, fp, key, deps, time, lines, cand, start: 0 }
    }

    /// A kept row: re-render it when its per-row state or countdown changed. Returns whether its
    /// lines changed.
    fn revisit(&mut self, c: &mut CRow) -> bool {
        let deps = self.deps(&c.info);
        let time = self.time(&c.info);
        if deps == c.deps && time == c.time {
            return false;
        }
        c.deps = deps;
        c.time = time;
        c.fp = self.fingerprint(&c.row, &c.info, deps, time);
        c.key = self.content_key(&c.row, &c.info, deps, time);
        let Some(old) = &c.lines else { return false };
        let lines = self.render(&c.row);
        if lines == *old {
            return false;
        }
        c.lines = Some(lines);
        true
    }
}

#[cfg(test)]
#[path = "tcache_tests.rs"]
mod tests;
