//! Diff viewer (`src/feature-plugins/system/diff-viewer*.tsx`): the full-screen `diff` route on the
//! home screen, and inside a session the side panel, docked beside the transcript or taking over
//! the screen. Docked panels and narrow full screens share the compact layout (no file tree, flush
//! cards and a footer).

use super::{App, Route, Toast};
use crate::api::types::FileDiff;
use crate::keymap::{Match, Stroke};
use crate::theme::{Rgba, Theme, tint};
use crate::ui::dialog::{Dialog, Opt, Select, Size};
use crate::ui::text::*;
use crate::util::locale::{self, width as sw};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use std::collections::{BTreeMap, HashSet};

const MIN_SPLIT_WIDTH: usize = 100;
const FILE_TREE_MIN_WIDTH: u16 = 30;
const FILE_TREE_MAX_WIDTH: u16 = 40;
const FULL_INSET: u16 = 2;
/// The full layout keeps its file tree and inset cards only while its patches stay as wide as the
/// narrowest docked panel, an equal split at the host's split threshold.
pub const FULL_LAYOUT_MIN_WIDTH: u16 = FILE_TREE_MIN_WIDTH + FULL_INSET * 2 + super::panel::SESSION_PANEL_SPLIT_MIN_WIDTH / 2;
const FILE_TREE_STATUS_WIDTH: usize = 1;
const CONTEXT_LINES: u32 = 12;

pub fn source_label(mode: &str) -> (&'static str, &'static str) {
    match mode {
        "committed" => ("Committed", "Branch commits only"),
        "working" => ("Uncommitted", "Local changes only"),
        "turn" => ("Last turn", "Latest session turn"),
        _ => ("All", "Branch + local changes"),
    }
}

fn needs_base(mode: &str) -> bool {
    mode == "branch" || mode == "committed"
}

/// Where a viewer is shown and with how much room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffLayout {
    pub width: u16,
    /// The side panel docked beside the transcript.
    pub docked: bool,
    /// Docked panels only take keys while focused; full screen always does.
    pub focused: bool,
    /// Shown as the session panel (full screen or docked) rather than the home route.
    pub panel: bool,
}

impl Default for DiffLayout {
    fn default() -> Self {
        DiffLayout { width: 100, docked: false, focused: true, panel: false }
    }
}

impl DiffLayout {
    pub fn compact(&self) -> bool {
        self.docked || self.width < FULL_LAYOUT_MIN_WIDTH
    }
    fn inset(&self) -> u16 {
        if self.compact() { 0 } else { FULL_INSET }
    }
    /// The side panel sits on the raised surface; full screen keeps the base background.
    fn surface(&self, t: &Theme) -> Rgba {
        if self.docked { t.background.raised.base } else { t.background.base }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DiffViewer {
    pub mode: String,
    pub session: Option<String>,
    pub location: String,
    pub files: Vec<FileDiff>,
    pub loading: bool,
    pub error: bool,
    pub base: Option<String>,
    pub base_loaded: bool,
    pub selected_base: Option<String>,
    pub reviewed: HashSet<String>,
    pub view: Option<String>,
    pub scroll: usize,
    pub selected: Option<usize>,
    /// Single-patch mode: the last file was marked reviewed with nothing after it, so `m` stays put.
    pub exhausted: Option<usize>,
    pub file_starts: Vec<(usize, usize)>,
    pub hunk_starts: Vec<usize>,
    pub total: usize,
    pub viewport: usize,
    pub return_route: Option<Route>,
    pub request: u64,
    /// The layout of the last frame.
    pub layout: DiffLayout,
}

/// File tree row.
#[derive(Clone, Debug, PartialEq)]
pub struct TreeRow {
    pub depth: usize,
    pub name: String,
    pub file: Option<usize>,
}

/// Build the file tree rows (`buildFileTree` + `flattenFileTree`): directories before files, names
/// in code-unit order, single-child directory chains collapsed.
pub fn tree_rows(files: &[FileDiff]) -> Vec<TreeRow> {
    #[derive(Default)]
    struct Node {
        dirs: BTreeMap<String, Node>,
        files: Vec<(String, usize)>,
    }
    let mut root = Node::default();
    for (i, f) in files.iter().enumerate() {
        let parts: Vec<&str> = f.file.split('/').filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            continue;
        }
        let mut cur = &mut root;
        for p in &parts[..parts.len() - 1] {
            cur = cur.dirs.entry(p.to_string()).or_default();
        }
        cur.files.push((parts.last().unwrap_or(&"").to_string(), i));
    }
    fn walk(node: &Node, depth: usize, out: &mut Vec<TreeRow>) {
        for (name, child) in &node.dirs {
            let mut name = name.clone();
            let mut c = child;
            while c.files.is_empty() && c.dirs.len() == 1 {
                let (n, next) = c.dirs.iter().next().unwrap();
                name = format!("{name}/{n}");
                c = next;
            }
            out.push(TreeRow { depth, name, file: None });
            walk(c, depth + 1, out);
        }
        let mut files = node.files.clone();
        files.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        for (name, i) in files {
            out.push(TreeRow { depth, name, file: Some(i) });
        }
    }
    let mut out = Vec::new();
    walk(&root, 0, &mut out);
    out
}

/// `orderedPatchFileIndexes`: patches follow the tree.
pub fn patch_order(files: &[FileDiff]) -> Vec<usize> {
    tree_rows(files).into_iter().filter_map(|r| r.file).collect()
}

/// Directory chain truncation: `a/b/c/d` → `…/c/d`.
pub fn truncate_chain(name: &str, max: usize) -> String {
    if sw(name) <= max {
        return name.to_string();
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut width = sw("…/");
    for seg in name.split('/').rev() {
        let next = sw(seg) + usize::from(!kept.is_empty());
        if width + next > max {
            break;
        }
        kept.insert(0, seg);
        width += next;
    }
    if kept.is_empty() {
        return locale::truncate_file_path(name, max);
    }
    format!("…/{}", kept.join("/"))
}

fn is_image(file: &str) -> bool {
    matches!(file.rsplit('.').next().map(|e| e.to_lowercase()).as_deref(), Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "bmp" | "ico"))
}

const DIFF_IDS: &[&str] = &[
    "diff.close",
    "diff.down",
    "diff.up",
    "diff.page.down",
    "diff.page.up",
    "diff.half_page.down",
    "diff.half_page.up",
    "diff.first",
    "diff.last",
    "diff.next_hunk",
    "diff.previous_hunk",
    "diff.next_file",
    "diff.previous_file",
    "diff.toggle_file_tree",
    "diff.single_patch",
    "diff.switch_source",
    "diff.toggle_view",
    "diff.toggle_fullscreen",
    "diff.mark_reviewed",
    "diff.help",
];

impl App {
    /// A fresh viewer for `session` (the side panel) or the home route.
    fn new_diff_viewer(&self, session: Option<String>) -> DiffViewer {
        let mut mode = self.diff_memory.clone().or_else(|| self.config.str("diffs.source").map(|s| s.to_string())).unwrap_or_else(|| "branch".into());
        let sources = ["branch", "committed", "working", "turn"];
        if !sources.contains(&mode.as_str()) || (mode == "turn" && session.is_none()) {
            mode = "branch".into();
        }
        DiffViewer { mode, session, location: self.location_dir(), ..Default::default() }
    }

    /// `/diff`: inside a session it toggles the side panel; on the home screen it opens the
    /// full-screen route.
    pub fn open_diff(&mut self) {
        self.dialogs.clear();
        if matches!(&self.route, Route::Plugin(n) if n == "diff") {
            return;
        }
        if let Some(sid) = self.session_id().map(|s| s.to_string()) {
            if self.active_panel().is_some_and(|p| p.name == "diff") {
                self.close_panel();
                return;
            }
            let fullscreen = self.config.bool("diffs.fullscreen") == Some(true);
            self.diff = Some(self.new_diff_viewer(Some(sid.clone())));
            self.open_panel("diff", &sid, if fullscreen { super::panel::Presentation::Fullscreen } else { super::panel::Presentation::Panel });
            self.load_diff();
            return;
        }
        let mut d = self.new_diff_viewer(None);
        d.return_route = Some(self.route.clone());
        self.diff = Some(d);
        self.route = Route::Plugin("diff".into());
        self.load_diff();
        self.dirty = true;
    }

    pub fn close_diff(&mut self) {
        self.dialogs.clear();
        if self.panel.as_ref().is_some_and(|p| p.name == "diff") && !matches!(&self.route, Route::Plugin(n) if n == "diff") {
            self.close_panel();
            return;
        }
        let back = self.diff.take().and_then(|d| d.return_route).unwrap_or(Route::Home);
        self.route = back;
        self.dirty = true;
    }

    pub fn load_diff(&mut self) {
        let Some(d) = self.diff.as_mut() else { return };
        d.loading = true;
        d.error = false;
        d.request += 1;
        let req = d.request;
        let c = self.client.clone();
        let mode = d.mode.clone();
        let session = d.session.clone();
        let dir = d.location.clone();
        let selected = d.selected_base.clone();
        self.spawn(
            async move {
                if mode == "turn"
                    && let Some(s) = session
                {
                    return Ok((None, c.session_diff(&s, CONTEXT_LINES).await?));
                }
                let base = if needs_base(&mode) {
                    match selected {
                        Some(b) => Some(b),
                        None => c.vcs_base(Some(&dir)).await?.and_then(|b| b.get("ref").and_then(|r| r.as_str()).map(|s| s.to_string())),
                    }
                } else {
                    None
                };
                if mode == "committed" && base.is_none() {
                    return Ok((None, vec![]));
                }
                let files = c.vcs_diff(&mode, base.as_deref(), CONTEXT_LINES, Some(&dir)).await?;
                Ok((base, files))
            },
            move |app, r| {
                let Some(d) = app.diff.as_mut() else { return };
                if d.request != req {
                    return;
                }
                d.loading = false;
                d.base_loaded = true;
                match r {
                    Ok((base, files)) => {
                        d.base = base;
                        d.files = files;
                        d.reviewed.clear();
                        d.selected = None;
                        d.exhausted = None;
                    }
                    Err(_) => d.error = true,
                }
                app.dirty = true;
            },
        );
    }

    fn diff_tree_enabled(&self) -> bool {
        self.config.bool("diffs.tree").unwrap_or(true)
    }

    fn diff_tree_shown(&self, l: &DiffLayout) -> bool {
        !l.compact() && self.diff_tree_enabled() && self.diff.as_ref().is_some_and(|d| !d.files.is_empty())
    }

    fn diff_tree_width(l: &DiffLayout) -> u16 {
        (l.width / 4).clamp(FILE_TREE_MIN_WIDTH, FILE_TREE_MAX_WIDTH)
    }

    fn diff_detail(&self) -> Option<String> {
        let d = self.diff.as_ref()?;
        if d.mode == "turn" {
            return if d.error { Some("Diff unavailable".into()) } else { None };
        }
        if d.mode == "working" {
            return Some("vs HEAD".into());
        }
        if d.error {
            return Some("Base or diff unavailable".into());
        }
        if d.loading {
            return Some("Resolving diff…".into());
        }
        match &d.base {
            None => Some("Base not reported".into()),
            Some(b) => Some(format!("vs {}", b.trim_start_matches("refs/heads/").trim_start_matches("refs/remotes/"))),
        }
    }

    fn diff_single(&self) -> bool {
        self.config.bool("diffs.single").unwrap_or(false)
    }

    /// The file at the top of the patch view.
    fn diff_current_file(&self, d: &DiffViewer) -> Option<usize> {
        d.file_starts.iter().rev().find(|(_, s)| *s <= d.scroll).map(|(f, _)| *f).or_else(|| d.file_starts.first().map(|(f, _)| *f))
    }

    fn visible_files(&self) -> Vec<usize> {
        let Some(d) = &self.diff else { return vec![] };
        let order = patch_order(&d.files);
        if self.diff_single() {
            return d.selected.or_else(|| order.first().copied()).into_iter().collect();
        }
        order
    }

    /// Keys while the diff viewer has focus.
    pub fn diff_key(&mut self, seq: &[Stroke]) -> crate::keymap::Match {
        let kb = self.keybinds.clone();
        let mut prefix = false;
        for id in DIFF_IDS {
            match kb.match_command(id, seq) {
                Match::Full => {
                    if *id == "diff.toggle_fullscreen" && self.active_panel().is_none() {
                        continue;
                    }
                    self.diff_command(id);
                    return Match::Full;
                }
                Match::Prefix => prefix = true,
                Match::None => {}
            }
        }
        if prefix { Match::Prefix } else { Match::None }
    }

    fn diff_scroll_to(&mut self, line: usize) {
        if let Some(d) = self.diff.as_mut() {
            d.scroll = line.min(d.total.saturating_sub(d.viewport));
        }
    }

    pub fn diff_command(&mut self, id: &str) {
        let Some(d) = self.diff.clone() else { return };
        let page = d.viewport.max(1);
        let order = patch_order(&d.files);
        match id {
            "diff.close" => self.close_diff(),
            "diff.down" => self.diff_scroll_to(d.scroll + 1),
            "diff.up" => self.diff_scroll_to(d.scroll.saturating_sub(1)),
            "diff.page.down" => self.diff_scroll_to(d.scroll + page),
            "diff.page.up" => self.diff_scroll_to(d.scroll.saturating_sub(page)),
            "diff.half_page.down" => self.diff_scroll_to(d.scroll + page / 2),
            "diff.half_page.up" => self.diff_scroll_to(d.scroll.saturating_sub(page / 2)),
            "diff.first" => self.diff_scroll_to(0),
            "diff.last" => self.diff_scroll_to(usize::MAX),
            "diff.next_hunk" => {
                if let Some(h) = d.hunk_starts.iter().find(|h| **h > d.scroll) {
                    self.diff_scroll_to(*h);
                }
            }
            "diff.previous_hunk" => {
                if let Some(h) = d.hunk_starts.iter().rev().find(|h| **h < d.scroll) {
                    self.diff_scroll_to(*h);
                }
            }
            "diff.next_file" | "diff.previous_file" => {
                if order.is_empty() {
                    return;
                }
                let cur = d.selected.or_else(|| self.diff_current_file(&d));
                let next = match cur.and_then(|c| order.iter().position(|f| *f == c)) {
                    None => order[0],
                    Some(p) if id == "diff.next_file" => order[(p + 1).min(order.len() - 1)],
                    Some(p) => order[p.saturating_sub(1)],
                };
                if let Some(dv) = self.diff.as_mut() {
                    dv.selected = Some(next);
                    dv.exhausted = None;
                }
                let single = self.diff_single();
                match d.file_starts.iter().find(|(f, _)| *f == next) {
                    Some((_, start)) if !single => self.diff_scroll_to(*start),
                    _ => self.diff_scroll_to(0),
                }
            }
            "diff.toggle_file_tree" => {
                if d.layout.compact() {
                    return;
                }
                let v = !self.diff_tree_enabled();
                self.config.set("diffs.tree", Some(serde_json::json!(v)));
            }
            "diff.single_patch" => {
                let single = self.diff_single();
                let cur = d.selected.or_else(|| self.diff_current_file(&d)).or_else(|| order.first().copied());
                if let Some(dv) = self.diff.as_mut() {
                    dv.selected = cur;
                }
                self.config.set("diffs.single", Some(serde_json::json!(!single)));
                self.diff_scroll_to(0);
            }
            "diff.switch_source" => self.open_diff_source(),
            "diff.toggle_view" => {
                if !self.diff_split_available(&d.layout) {
                    return;
                }
                let current = self.diff_view_split(&d.layout);
                let next = if current { "unified" } else { "split" };
                if let Some(dv) = self.diff.as_mut() {
                    dv.view = Some(next.into());
                }
                self.config.set("diffs.view", Some(serde_json::json!(next)));
            }
            "diff.toggle_fullscreen" => {
                if self.active_panel().is_none() {
                    return;
                }
                let before = self.panel_presentation();
                self.toggle_panel_fullscreen();
                let after = self.panel_presentation();
                // A narrow terminal forces full screen and ignores the toggle; only a real toggle is remembered.
                if after != before {
                    self.config.set("diffs.fullscreen", Some(serde_json::json!(after == super::panel::Presentation::Fullscreen)));
                }
            }
            "diff.mark_reviewed" => {
                let cur = d.selected.or_else(|| self.diff_current_file(&d));
                if cur.is_some() && cur == d.exhausted {
                    return;
                }
                if let Some(i) = cur
                    && let Some(f) = d.files.get(i)
                {
                    let name = f.file.clone();
                    let single = self.diff_single();
                    if let Some(dv) = self.diff.as_mut() {
                        let reviewed = dv.reviewed.contains(&name);
                        if reviewed {
                            dv.reviewed.remove(&name);
                        } else {
                            dv.reviewed.insert(name);
                        }
                        let p = order.iter().position(|x| *x == i);
                        let next = if single && !reviewed { p.and_then(|p| order.get(p + 1)).copied().unwrap_or(i) } else { i };
                        dv.selected = Some(next);
                        dv.exhausted = if single && !reviewed && next == i { Some(i) } else { None };
                    }
                }
            }
            "diff.help" => self.open_diff_help(),
            _ => {}
        }
        self.dirty = true;
    }

    fn diff_pane_width(&self, l: &DiffLayout) -> usize {
        let tree = if self.diff_tree_shown(l) { Self::diff_tree_width(l) } else { 0 };
        l.width.saturating_sub(tree + l.inset() * 2) as usize
    }

    fn diff_split_available(&self, l: &DiffLayout) -> bool {
        self.diff_pane_width(l) >= MIN_SPLIT_WIDTH
    }

    fn diff_view_split(&self, l: &DiffLayout) -> bool {
        if !self.diff_split_available(l) {
            return false;
        }
        let stored = self.diff.as_ref().and_then(|d| d.view.clone()).or_else(|| self.config.str("diffs.view").map(|s| s.to_string()));
        stored.as_deref() != Some("unified")
    }

    pub fn open_diff_source(&mut self) {
        let Some(d) = &self.diff else { return };
        let mut sources = vec!["branch", "committed", "working"];
        if d.session.is_some() {
            sources.push("turn");
        }
        let mut opts: Vec<Opt> = sources
            .iter()
            .map(|s| {
                let (l, desc) = source_label(s);
                Opt::new(format!("{l:<11}"), *s).desc(desc)
            })
            .collect();
        let base = d.selected_base.clone().or(d.base.clone());
        opts.push(Opt::new(format!("{:<11}", "Base"), "base").desc(base.unwrap_or_else(|| "Choose…".into())));
        let mut s = Select::new("Diff source", opts);
        s.render_filter = false;
        s.skip_filter = true;
        s.current = Some(d.mode.clone());
        self.open_dialog(Dialog::select("diff-source", s, Size::Medium));
    }

    pub fn diff_source_selected(&mut self, v: &str) {
        self.dialogs.clear();
        if v == "base" {
            let dir = self.diff.as_ref().map(|d| d.location.clone()).unwrap_or_default();
            let c = self.client.clone();
            let mut s = Select::new("Base branch", vec![]);
            s.placeholder = "Search local and remote branches".into();
            s.skip_filter = true;
            s.footer_text = Some(("Remembered until the TUI exits".into(), self.theme().dialog().text.muted));
            s.empty = Some("Loading branches…".into());
            s.current = self.diff.as_ref().and_then(|d| d.selected_base.clone());
            self.open_dialog(Dialog::select("diff-base", s, Size::Medium));
            self.spawn(async move { c.vcs_branches("", 100, Some(&dir)).await }, |app, r| {
                let t = app.theme().dialog().clone();
                let Some(sel) = app.dialogs.last_mut().filter(|d| d.purpose == "diff-base").and_then(|d| d.sel()) else { return };
                match r {
                    Ok(list) => {
                        sel.options = list.into_iter().map(|b| Opt::new(b.clone(), b)).collect();
                        sel.empty = Some("No branches found".into());
                    }
                    Err(_) => {
                        sel.empty = Some("Could not load branches. Reopen the picker to try again.".into());
                        sel.empty_color = Some(t.text.feedback.error.base);
                    }
                }
                app.dirty = true;
            });
            return;
        }
        self.diff_memory = Some(v.to_string());
        if let Some(d) = self.diff.as_mut() {
            d.mode = v.to_string();
            d.scroll = 0;
        }
        self.load_diff();
    }

    pub fn diff_base_selected(&mut self, v: &str) {
        self.dialogs.clear();
        if let Some(d) = self.diff.as_mut() {
            d.selected_base = Some(v.to_string());
        }
        self.load_diff();
    }

    fn open_diff_help(&mut self) {
        let kb = &self.keybinds;
        let t = self.theme().dialog().clone();
        let short = |ids: &[&str]| ids.iter().map(|i| kb.shortcut(i)).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" / ");
        let all = |id: &str| kb.get(id).iter().map(|s| crate::keymap::format_sequence(s, &kb.leader_text)).collect::<Vec<_>>().join(" / ");
        let single = self.diff_single();
        let layout = self.diff.as_ref().map(|d| d.layout).unwrap_or_default();
        let mut view = vec![(short(&["diff.toggle_view"]), "Split / unified"), (short(&["diff.single_patch"]), "All files / single file")];
        if !layout.compact() {
            view.push((short(&["diff.toggle_file_tree"]), "Show / hide file tree"));
        }
        view.push((short(&["diff.switch_source"]), "Switch diff source"));
        if layout.panel {
            view.push((short(&["diff.toggle_fullscreen"]), "Full screen / split view"));
        }
        view.push((all("diff.close"), "Close diff viewer"));
        let groups: Vec<(&str, Vec<(String, &str)>)> = vec![
            (
                "Review",
                vec![
                    (all("diff.next_file"), "Next file"),
                    (all("diff.previous_file"), "Previous file"),
                    (short(&["diff.mark_reviewed"]), if single { "Review + next / reopen" } else { "Review + collapse / reopen" }),
                    (short(&["diff.next_hunk", "diff.previous_hunk"]), "Next / previous change"),
                    ("right-click".into(), "File menu (heading or tree)"),
                ],
            ),
            (
                "Scroll",
                vec![
                    (short(&["diff.down", "diff.up"]), "Down / up"),
                    (short(&["diff.half_page.down", "diff.half_page.up"]), "Half page down / up"),
                    (short(&["diff.page.down", "diff.page.up"]), "Page down / up"),
                    (short(&["diff.first", "diff.last"]), "First / last"),
                ],
            ),
            ("View", view),
        ];
        let mut lines = Vec::new();
        for (gi, (title, rows)) in groups.into_iter().enumerate() {
            if gi > 0 {
                lines.push(SLine::blank());
            }
            lines.push(SLine::new(vec![Seg::new(title, bold(fg(t.text.base)))]));
            for (k, label) in rows {
                let k = if k.is_empty() { "unbound".to_string() } else { k };
                lines.push(SLine::new(vec![seg(format!("{k:<17}"), fg(t.text.base)), seg("  ", fg(t.text.base)), seg(label, fg(t.text.muted))]));
            }
        }
        let mut td = crate::ui::dialog::TextDialog::new("Diff shortcuts", lines);
        td.close_label = "esc close".into();
        td.height = Some(td.lines.len().min((self.size.1 as usize).saturating_sub(6)).max(1));
        let mut d = Dialog::text("text", td);
        d.centered = true;
        self.open_dialog(d);
    }

    // ---- rendering ---------------------------------------------------------

    /// The full-screen route on the home screen.
    pub fn draw_diff(&mut self, buf: &mut Buffer, area: Rect, t: &Theme) {
        let l = DiffLayout { width: area.width, docked: false, focused: true, panel: false };
        self.draw_diff_in(buf, area, t, l);
    }

    /// One viewer in `area` with layout `l` (`DiffViewerContent`).
    pub fn draw_diff_in(&mut self, buf: &mut Buffer, area: Rect, t: &Theme, l: DiffLayout) {
        let surface = l.surface(t);
        fill(buf, area, surface.tui());
        let Some(d) = self.diff.clone() else { return };
        if let Some(dv) = self.diff.as_mut() {
            dv.layout = l;
        }
        let compact = l.compact();
        let tree = self.diff_tree_shown(&l);
        let footer_h: u16 = if compact {
            if l.docked { 3 } else { 1 }
        } else {
            0
        };
        let mut y = area.y;
        let mut h = area.height.saturating_sub(footer_h);
        if !tree {
            let top = if l.docked { 1 } else { 0 };
            let header_h = top + 1;
            let x = area.x + 2;
            let right = area.right().saturating_sub(if compact { 1 } else { 2 });
            let count = format!("{}/{}", d.files.iter().filter(|f| d.reviewed.contains(&f.file)).count(), d.files.len());
            let mut segs = vec![Seg::new(source_label(&d.mode).0, bold(fg(t.text.action.secondary.base)))];
            if let Some(detail) = self.diff_detail() {
                segs.push(seg(format!(" · {detail}"), fg(t.text.muted)));
            }
            let w = right.saturating_sub(x) as usize;
            let room = w.saturating_sub(sw(&count) + 1);
            put_segs(buf, x, area.y + top, &clip(&segs, room), room);
            put(buf, right.saturating_sub(sw(&count) as u16), area.y + top, &count, fg(t.text.muted), sw(&count));
            self.hits.push((Rect::new(x, area.y + top, room as u16, 1), Click::Custom("diff.switch_source".into())));
            y += header_h;
            h = h.saturating_sub(header_h);
        }
        let content = Rect::new(area.x, y, area.width, h);
        let message = if d.loading {
            Some(("Loading diff…".to_string(), t.text.muted))
        } else if d.error {
            let text = if d.base.is_none() && needs_base(&d.mode) {
                "Could not load diff. Choose a base branch from Diff source, or select Uncommitted."
            } else {
                "Could not load diff. Reopen the diff viewer to try again."
            };
            Some((text.to_string(), t.text.feedback.error.base))
        } else if d.mode == "committed" && d.base.is_none() && d.base_loaded {
            Some(("Committed comparison unavailable without base metadata. Choose a base branch from Diff source.".to_string(), t.text.muted))
        } else if d.files.is_empty() {
            Some(("No changes to show".to_string(), t.text.muted))
        } else {
            None
        };
        if let Some((m, c)) = message {
            let top = if compact { 1 } else { 2 };
            let lines = wrap(&[seg(m, fg(c))], content.width.saturating_sub(4) as usize, Wrap::Word);
            for (i, line) in lines.into_iter().enumerate() {
                let yy = content.y + top + i as u16;
                if yy >= content.bottom() {
                    break;
                }
                put_segs(buf, content.x + 2, yy, &line, content.width.saturating_sub(4) as usize);
            }
        } else {
            let mut pane = content;
            if tree {
                let tw = Self::diff_tree_width(&l);
                self.draw_diff_tree(buf, Rect::new(content.x, content.y, tw, content.height), t, &d);
                pane = Rect::new(content.x + tw, content.y, content.width - tw, content.height);
            }
            self.draw_diff_patches(buf, pane, t, &d, &l, surface);
        }
        if compact {
            self.draw_diff_footer(buf, Rect::new(area.x, area.bottom().saturating_sub(footer_h), area.width, footer_h), t, &l, surface);
        } else if !tree {
            let help = self.keybinds.shortcut("diff.help");
            if !help.is_empty() {
                put(buf, area.right().saturating_sub(1), area.y, "?", fg(t.text.base), 1);
                self.hits.push((Rect::new(area.right().saturating_sub(1), area.y, 1, 1), Click::Custom("diff.help".into())));
            }
        }
    }

    /// The compact footer: `<key> focus prompt` (docked) or `<key> close`, and `? see all`; dimmed
    /// while the docked panel is not focused.
    fn draw_diff_footer(&mut self, buf: &mut Buffer, area: Rect, t: &Theme, l: &DiffLayout, surface: Rgba) {
        if area.height == 0 {
            return;
        }
        fill(buf, area, t.background.base.tui());
        let mut y = area.y;
        if l.docked {
            put(buf, area.x, y, &"▀".repeat(area.width as usize), Style::default().fg(surface.tui()).bg(t.background.base.tui()), area.width as usize);
            y += 1;
        }
        let focused = !l.docked || l.focused;
        let dim = |s: Style| s.add_modifier(Modifier::DIM);
        let key_style = if focused { fg(t.background.action.primary.focused) } else { dim(fg(t.text.action.primary.disabled)) };
        let label_style = if focused { fg(t.text.muted) } else { key_style };
        let help_style = if focused { fg(t.text.base) } else { dim(fg(t.text.action.primary.disabled)) };
        let help_label = if focused { fg(t.text.muted) } else { help_style };
        let bg = t.background.base.tui();
        let key = self.keybinds.shortcut(if l.docked { "pane.focus.left" } else { "diff.close" });
        let label = if l.docked { " focus prompt" } else { " close" };
        let help = self.keybinds.shortcut("diff.help");
        let right: Vec<Seg> = if help.is_empty() { vec![] } else { vec![Seg::new(help, help_style.bg(bg)), Seg::new(" see all", help_label.bg(bg))] };
        let rw: usize = right.iter().map(|s| s.width()).sum();
        let x = area.x + 2;
        let room = (area.width as usize).saturating_sub(2 + 1 + rw);
        let left = vec![Seg::new(key, key_style.bg(bg)), Seg::new(label, label_style.bg(bg))];
        put_segs(buf, x, y, &clip(&left, room), room);
        put_segs(buf, area.right().saturating_sub(1 + rw as u16), y, &right, rw);
        self.hits.push((Rect::new(x, y, room as u16, 1), Click::Custom(if l.docked { "diff.focus".into() } else { "diff.close".into() })));
        if rw > 0 {
            self.hits.push((Rect::new(area.right().saturating_sub(1 + rw as u16), y, rw as u16, 1), Click::Custom("diff.help".into())));
        }
    }

    /// The patch column: the fixed top edge, then the cards.
    fn draw_diff_patches(&mut self, buf: &mut Buffer, pane: Rect, t: &Theme, d: &DiffViewer, l: &DiffLayout, surface: Rgba) {
        if pane.height == 0 {
            return;
        }
        let inset = l.inset();
        let view = Rect::new(pane.x, pane.y + 1, pane.width, pane.height.saturating_sub(1));
        let (lines, starts, hunks, headers) = self.diff_lines(t, d, pane.width as usize, l, surface);
        let total = lines.len();
        let scroll = d.scroll.min(total.saturating_sub(view.height as usize));
        if let Some(dv) = self.diff.as_mut() {
            dv.total = total;
            dv.viewport = view.height as usize;
            dv.file_starts = starts.clone();
            dv.hunk_starts = hunks;
            dv.scroll = scroll;
        }
        // The fixed edge belongs to the card at the top of the view.
        let top_card = starts.iter().rev().find(|(_, s)| *s <= scroll).map(|(f, _)| *f);
        let edge = match top_card.and_then(|f| d.files.get(f)) {
            Some(f) if d.reviewed.contains(&f.file) => t.background.raised.high,
            _ => t.diff.bg_context,
        };
        fill(buf, Rect::new(pane.x + inset, pane.y, pane.width.saturating_sub(inset * 2), 1), edge.tui());
        draw_lines(buf, view, &lines, scroll, "");
        // The header of the card containing the top line sticks to the top, bounded by its card.
        if let Some((fi, start)) = starts.iter().rev().find(|(_, s)| *s <= scroll).copied()
            && scroll > start
            && let Some((h0, h1)) = headers.iter().find(|(f, _, _)| *f == fi).map(|(_, a, b)| (a.clone(), b.clone()))
        {
            let end = starts.iter().find(|(_, s)| *s > start).map(|(_, s)| s.saturating_sub(1)).unwrap_or(total);
            if scroll + 2 <= end {
                draw_lines(buf, Rect::new(view.x, view.y, view.width, 2), &[h0, h1], 0, "");
            }
        }
    }

    /// Rows of the patch column (full width, insets included) plus file starts, hunk starts and
    /// each file's header rows.
    #[allow(clippy::type_complexity)]
    fn diff_lines(&self, t: &Theme, d: &DiffViewer, width: usize, l: &DiffLayout, surface: Rgba) -> (Vec<SLine>, Vec<(usize, usize)>, Vec<usize>, Vec<(usize, SLine, SLine)>) {
        let compact = l.compact();
        let inset = l.inset() as usize;
        let cw = width.saturating_sub(inset * 2);
        let mut out = Vec::new();
        let mut starts = Vec::new();
        let mut hunks = Vec::new();
        let mut headers = Vec::new();
        let split = self.diff_view_split(l);
        let base = t.background.base.tui();
        let side = |text: &str| Seg::new(text.repeat(inset), Style::default().fg(surface.tui()).bg(base));
        // A card row: surface insets around the card's cells.
        let row = |segs: Vec<Seg>, card: Rgba| -> SLine {
            let mut v = vec![Seg::new(" ".repeat(inset), Style::default().bg(surface.tui()))];
            let clipped = clip(&segs, cw);
            let used: usize = clipped.iter().map(|s| s.width()).sum();
            v.extend(clipped.into_iter().map(|s| if s.style.bg.is_none() { Seg::new(s.text, s.style.bg(card.tui())) } else { s }));
            v.push(Seg::new(" ".repeat(cw.saturating_sub(used)), Style::default().bg(card.tui())));
            v.push(Seg::new(" ".repeat(inset), Style::default().bg(surface.tui())));
            SLine::new(v)
        };
        // `CardGap`: a half row of base background, the card's color over its width.
        let gap = |glyph: &str, card: Rgba| -> SLine { SLine::new(vec![side(glyph), Seg::new(glyph.repeat(cw), Style::default().fg(card.tui()).bg(base)), side(glyph)]) };
        let left_pad = if compact { 2 } else { 1 };
        for (k, fi) in self.visible_files().into_iter().enumerate() {
            let Some(f) = d.files.get(fi) else { continue };
            let reviewed = d.reviewed.contains(&f.file);
            let card = if reviewed { t.background.raised.high } else { t.diff.bg_context };
            if k > 0 {
                out.push(gap("▄", card));
            }
            starts.push((fi, out.len()));
            let image = is_image(&f.file);
            let mut right: Vec<Seg> = Vec::new();
            if reviewed {
                right.push(seg("✓ ", fg(t.text.muted)));
            }
            if image {
                right.push(seg("Image", fg(t.text.muted)));
            } else {
                right.push(seg(format!("+{}", f.additions), fg(if reviewed { t.text.muted } else { t.diff.text_added })));
                right.push(seg(format!(" -{}", f.deletions), fg(if reviewed { t.text.muted } else { t.diff.text_removed })));
            }
            let counts_w = (if image { 6 } else { f.additions.to_string().len() + f.deletions.to_string().len() + 5 }) + if reviewed { 2 } else { 0 };
            let path = locale::truncate_file_path(&f.file, cw.saturating_sub(counts_w + 2).max(1));
            let (dir, name) = match path.rfind('/') {
                Some(i) => (path[..=i].to_string(), path[i + 1..].to_string()),
                None => (String::new(), path.clone()),
            };
            let rw: usize = right.iter().map(|s| s.width()).sum();
            let mut hs = vec![seg(" ".repeat(left_pad), Style::default()), seg(dir, fg(t.text.muted)), seg(name, fg(if reviewed { t.text.muted } else { t.text.base }))];
            let used: usize = hs.iter().map(|s| s.width()).sum();
            hs.push(seg(" ".repeat(cw.saturating_sub(used + rw + 1)), Style::default()));
            hs.extend(right);
            let h0 = row(hs, card).on_click(Some(Click::Custom(format!("diff.file:{fi}"))));
            let h1 = if reviewed { gap("▀", card) } else { row(vec![], card) };
            headers.push((fi, h0.clone(), h1.clone()));
            out.push(h0);
            out.push(h1);
            if reviewed {
                continue;
            }
            let fallback = if d.mode == "committed" && image {
                Some("Committed image preview unavailable. The working-tree image is not shown.")
            } else if f.status.as_deref() == Some("deleted") && image {
                Some("Deleted image. The previous revision is not available for preview.")
            } else if f.patch.is_empty() || image {
                Some("No patch available for this file.")
            } else {
                None
            };
            if let Some(m) = fallback {
                for line in wrap(&[seg(m, fg(t.text.muted))], cw.saturating_sub(left_pad + 1).max(1), Wrap::Word) {
                    let mut segs = vec![seg(" ".repeat(left_pad), Style::default())];
                    segs.extend(line);
                    out.push(row(segs, card));
                }
                out.push(row(vec![], card));
                continue;
            }
            let ft = crate::ui::highlight::filetype(Some(&f.file));
            let o = crate::ui::diff::DiffOpts { theme: t, syntax: &self.syntax, filetype: &ft, split: split && f.status.as_deref() == Some("modified"), wrap: true };
            let gutter = if compact { 1 } else { 0 };
            let base_line = out.len();
            // Rendered (highlighted) cards are cached per file: this runs on every frame.
            let key = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                (&f.file, &f.patch, o.split, cw, gutter, self.themes.epoch, self.style_epoch).hash(&mut h);
                h.finish()
            };
            let cached = self.diff_cache.borrow().get(&key).cloned();
            let (rendered, hunk_rows) = match cached {
                Some(r) => r,
                None => {
                    let r = crate::ui::diff::render_viewer(&crate::ui::diff::parse(&f.patch), cw, o.split, gutter, &o);
                    let mut c = self.diff_cache.borrow_mut();
                    if c.len() >= 256 {
                        c.clear();
                    }
                    c.insert(key, r.clone());
                    r
                }
            };
            hunks.extend(hunk_rows.into_iter().map(|r| base_line + r));
            for line in rendered {
                out.push(row(line.segs, card));
            }
        }
        (out, starts, hunks, headers)
    }

    fn draw_diff_tree(&mut self, buf: &mut Buffer, area: Rect, t: &Theme, d: &DiffViewer) {
        let bg = t.background.raised.base;
        fill(buf, area, bg.tui());
        // Top edge row, then the box with padding 2 left/right and 1 at the bottom.
        let inner = Rect::new(area.x + 2, area.y + 1, area.width.saturating_sub(4), area.height.saturating_sub(2));
        let (label, _) = source_label(&d.mode);
        let count = format!("{}/{}", d.files.iter().filter(|f| d.reviewed.contains(&f.file)).count(), d.files.len());
        let mut segs = vec![Seg::new(label, bold(fg(t.text.action.secondary.base)).bg(bg.tui()))];
        if let Some(detail) = self.diff_detail() {
            segs.push(seg(format!(" · {detail}"), fg(t.text.muted).bg(bg.tui())));
        }
        let w = inner.width as usize;
        put_segs(buf, inner.x, inner.y, &clip(&segs, w.saturating_sub(sw(&count) + 1)), w);
        put(buf, inner.right().saturating_sub(sw(&count) as u16), inner.y, &count, fg(t.text.muted).bg(bg.tui()), sw(&count));
        self.hits.push((Rect::new(inner.x, inner.y, inner.width.saturating_sub(sw(&count) as u16 + 1), 1), Click::Custom("diff.switch_source".into())));
        let rows = tree_rows(&d.files);
        let faint = tint(t.text.muted, bg, 0.45);
        let rail = tint(t.text.muted, bg, 0.7);
        let content_w = w.saturating_sub(FILE_TREE_STATUS_WIDTH + 1);
        // Rows from below the header's bottom margin to above the footer (help + its padding).
        let help = self.keybinds.shortcut("diff.help");
        let footer_h = if help.is_empty() { 0 } else { 3 };
        let list_h = inner.height.saturating_sub(2 + footer_h) as usize;
        let selected = d.selected.or_else(|| if self.diff_single() { self.visible_files().first().copied() } else { None });
        let selected_row = selected.and_then(|s| rows.iter().position(|r| r.file == Some(s))).unwrap_or(0);
        let start = selected_row.saturating_sub(list_h.saturating_sub(1));
        for (i, r) in rows.iter().enumerate().skip(start).take(list_h) {
            let y = inner.y + 2 + (i - start) as u16;
            let indent = "│ ".repeat(r.depth.min(content_w.saturating_sub(3) / 2));
            let marker = if r.file.is_some() { "≡ " } else { "▾ " };
            let reviewed = r.file.and_then(|f| d.files.get(f)).is_some_and(|f| d.reviewed.contains(&f.file));
            let is_selected = r.file.is_some() && r.file == selected;
            let fgc = if r.file.is_none() {
                t.text.muted
            } else if is_selected {
                t.text.action.primary.selected
            } else if reviewed {
                t.text.muted
            } else {
                t.text.base
            };
            let nw = content_w.saturating_sub(sw(&indent) + sw(marker));
            let name = if r.file.is_none() { truncate_chain(&r.name, nw) } else { locale::truncate_file_path(&r.name, nw) };
            let mut st = fg(fgc).bg(bg.tui());
            if is_selected {
                st = st.add_modifier(Modifier::BOLD);
            }
            let mut x = inner.x;
            x += put(buf, x, y, &indent, fg(rail).bg(bg.tui()), w) as u16;
            x += put(buf, x, y, marker, fg(faint).bg(bg.tui()), w) as u16;
            put(buf, x, y, &name, st, nw);
            if let Some(fi) = r.file {
                let status = if reviewed {
                    "✓"
                } else {
                    match d.files[fi].status.as_deref() {
                        Some("modified") => "M",
                        Some("added") => "A",
                        Some("deleted") => "D",
                        _ => "?",
                    }
                };
                let sc = if reviewed {
                    t.text.muted
                } else {
                    match d.files[fi].status.as_deref() {
                        Some("added") => t.diff.text_added,
                        Some("deleted") => t.diff.text_removed,
                        _ => t.text.muted,
                    }
                };
                put(buf, inner.right().saturating_sub(FILE_TREE_STATUS_WIDTH as u16), y, status, fg(sc).bg(bg.tui()), 1);
                self.hits.push((Rect::new(inner.x, y, inner.width, 1), Click::Custom(format!("diff.file:{fi}"))));
            }
        }
        // Footer: the help shortcut with a blank row above and below it.
        if !help.is_empty() {
            let y = inner.bottom().saturating_sub(2);
            let x = inner.x + put(buf, inner.x, y, &help, fg(t.text.base).bg(bg.tui()), w) as u16;
            put(buf, x, y, " help", fg(t.text.muted).bg(bg.tui()), w);
            self.hits.push((Rect::new(inner.x, y, (sw(&help) + 5) as u16, 1), Click::Custom("diff.help".into())));
        }
    }

    pub fn diff_click(&mut self, what: &str) {
        match what {
            "diff.switch_source" => {
                self.focus_panel();
                self.open_diff_source();
                return;
            }
            "diff.help" => {
                self.open_diff_help();
                return;
            }
            "diff.close" => {
                self.close_diff();
                return;
            }
            "diff.focus" => {
                self.focus_panel();
                return;
            }
            _ => {}
        }
        if let Some(i) = what.strip_prefix("diff.file:").and_then(|s| s.parse::<usize>().ok()) {
            self.focus_panel();
            let start = self.diff.as_ref().and_then(|d| d.file_starts.iter().find(|(f, _)| *f == i).map(|(_, s)| *s));
            if let Some(dv) = self.diff.as_mut() {
                dv.selected = Some(i);
                dv.exhausted = None;
            }
            if self.diff_single() {
                self.diff_scroll_to(0);
            } else if let Some(s) = start {
                self.diff_scroll_to(s);
            }
        }
    }

    pub fn diff_toast_unavailable(&mut self) {
        self.toast(Toast::new("warning", "Diff unavailable"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_and_chains() {
        let files: Vec<FileDiff> = ["z.md", "a/b/c/y.ts", "a/b/c/x.ts"].iter().map(|f| FileDiff { file: f.to_string(), ..Default::default() }).collect();
        let rows = tree_rows(&files);
        assert_eq!(rows[0], TreeRow { depth: 0, name: "a/b/c".into(), file: None });
        assert_eq!(rows[1].name, "x.ts");
        assert_eq!(rows[2].name, "y.ts");
        assert_eq!(rows[3], TreeRow { depth: 0, name: "z.md".into(), file: Some(0) });
        assert_eq!(patch_order(&files), vec![2, 1, 0]);
        assert_eq!(truncate_chain("a/b/c/d", 6), "…/c/d");
        assert_eq!(source_label("working").0, "Uncommitted");
        assert_eq!(FULL_LAYOUT_MIN_WIDTH, 88);
    }
}
