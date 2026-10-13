//! Full-screen diff viewer (plugin route `diff`, `src/feature-plugins/system/diff-viewer*.tsx`).

use super::{App, Route, Toast};
use crate::api::types::FileDiff;
use crate::keymap::{Match, Stroke};
use crate::theme::{Theme, tint};
use crate::ui::dialog::{Body, Dialog, Opt, Select, Size};
use crate::ui::text::*;
use crate::util::locale::{self, width as sw};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use std::collections::{BTreeMap, HashSet};

const MIN_SPLIT_WIDTH: usize = 100;
const FILE_TREE_MIN_WIDTH: u16 = 30;
const FILE_TREE_MAX_WIDTH: u16 = 40;
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
    pub file_starts: Vec<(usize, usize)>,
    pub hunk_starts: Vec<usize>,
    pub total: usize,
    pub viewport: usize,
    pub return_route: Option<Route>,
    pub request: u64,
}

/// File tree row.
#[derive(Clone, Debug, PartialEq)]
pub struct TreeRow {
    pub depth: usize,
    pub name: String,
    pub file: Option<usize>,
}

/// Build the file tree rows with collapsed single-child directory chains.
pub fn tree_rows(files: &[FileDiff]) -> Vec<TreeRow> {
    #[derive(Default)]
    struct Node {
        dirs: BTreeMap<String, Node>,
        files: Vec<(String, usize)>,
    }
    let mut root = Node::default();
    for (i, f) in files.iter().enumerate() {
        let parts: Vec<&str> = f.file.split('/').collect();
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
        for (name, i) in &node.files {
            out.push(TreeRow { depth, name: name.clone(), file: Some(*i) });
        }
    }
    let mut out = Vec::new();
    walk(&root, 0, &mut out);
    out
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
    "diff.mark_reviewed",
    "diff.help",
];

impl App {
    pub fn open_diff(&mut self) {
        if matches!(&self.route, Route::Plugin(n) if n == "diff") {
            return;
        }
        let session = self.session_id().map(|s| s.to_string());
        let mut mode = self.diff_memory.clone().or_else(|| self.config.str("diffs.source").map(|s| s.to_string())).unwrap_or_else(|| "branch".into());
        if mode == "turn" && session.is_none() {
            mode = "branch".into();
        }
        self.diff = Some(DiffViewer { mode, session, location: self.location_dir(), return_route: Some(self.route.clone()), ..Default::default() });
        self.dialogs.clear();
        self.route = Route::Plugin("diff".into());
        self.load_diff();
        self.dirty = true;
    }

    pub fn close_diff(&mut self) {
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
                    }
                    Err(_) => d.error = true,
                }
                app.dirty = true;
            },
        );
    }

    fn diff_tree_shown(&self) -> bool {
        self.size.0 >= 90 && self.config.bool("diffs.tree").unwrap_or(true) && self.diff.as_ref().is_some_and(|d| !d.files.is_empty())
    }

    fn diff_tree_width(&self) -> u16 {
        (self.size.0 / 4).clamp(FILE_TREE_MIN_WIDTH, FILE_TREE_MAX_WIDTH)
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

    fn visible_files(&self) -> Vec<usize> {
        let Some(d) = &self.diff else { return vec![] };
        if self.config.bool("diffs.single").unwrap_or(false) {
            return d.selected.or(if d.files.is_empty() { None } else { Some(0) }).into_iter().collect();
        }
        (0..d.files.len()).collect()
    }

    /// Keys while the diff viewer is active. Returns true when handled.
    pub fn diff_key(&mut self, seq: &[Stroke]) -> crate::keymap::Match {
        let kb = self.keybinds.clone();
        let mut prefix = false;
        for id in DIFF_IDS {
            match kb.match_command(id, seq) {
                Match::Full => {
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
                let files = (0..d.files.len()).collect::<Vec<_>>();
                if files.is_empty() {
                    return;
                }
                let cur = d.selected.or_else(|| d.file_starts.iter().rev().find(|(_, s)| *s <= d.scroll).map(|(f, _)| *f));
                let next = match (cur, id == "diff.next_file") {
                    (None, _) => 0,
                    (Some(c), true) => (c + 1).min(files.len() - 1),
                    (Some(c), false) => c.saturating_sub(1),
                };
                if let Some(dv) = self.diff.as_mut() {
                    dv.selected = Some(next);
                }
                if let Some((_, start)) = d.file_starts.iter().find(|(f, _)| *f == next) {
                    self.diff_scroll_to(*start);
                } else {
                    self.diff_scroll_to(0);
                }
            }
            "diff.toggle_file_tree" => {
                let v = !self.config.bool("diffs.tree").unwrap_or(true);
                self.config.set("diffs.tree", Some(serde_json::json!(v)));
            }
            "diff.single_patch" => {
                let v = !self.config.bool("diffs.single").unwrap_or(false);
                self.config.set("diffs.single", Some(serde_json::json!(v)));
                self.diff_scroll_to(0);
            }
            "diff.switch_source" => self.open_diff_source(),
            "diff.toggle_view" => {
                let current = self.diff_view_split();
                let next = if current { "unified" } else { "split" };
                if let Some(dv) = self.diff.as_mut() {
                    dv.view = Some(next.into());
                }
                self.config.set("diffs.view", Some(serde_json::json!(next)));
            }
            "diff.mark_reviewed" => {
                let cur = d.selected.or_else(|| d.file_starts.iter().rev().find(|(_, s)| *s <= d.scroll).map(|(f, _)| *f));
                if let Some(i) = cur
                    && let Some(f) = d.files.get(i)
                {
                    let name = f.file.clone();
                    let single = self.config.bool("diffs.single").unwrap_or(false);
                    if let Some(dv) = self.diff.as_mut() {
                        if !dv.reviewed.remove(&name) {
                            dv.reviewed.insert(name);
                            if single && i + 1 < dv.files.len() {
                                dv.selected = Some(i + 1);
                            }
                        }
                    }
                }
            }
            "diff.help" => self.open_diff_help(),
            _ => {}
        }
        self.dirty = true;
    }

    fn diff_view_split(&self) -> bool {
        let pane = (self.size.0 as usize).saturating_sub(if self.diff_tree_shown() { self.diff_tree_width() as usize } else { 0 } + 4);
        if pane < MIN_SPLIT_WIDTH {
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
                let Some(sel) = app.dialogs.last_mut().filter(|d| d.purpose == "diff-base").and_then(|d| d.sel()) else { return };
                match r {
                    Ok(list) => {
                        sel.options = list.into_iter().map(|b| Opt::new(b.clone(), b)).collect();
                        sel.empty = Some("No branches found".into());
                    }
                    Err(_) => sel.empty = Some("Could not load branches. Reopen the picker to try again.".into()),
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
        let single = self.config.bool("diffs.single").unwrap_or(false);
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
            (
                "View",
                vec![
                    (short(&["diff.toggle_view"]), "Split / unified"),
                    (short(&["diff.single_patch"]), "All files / single file"),
                    (short(&["diff.toggle_file_tree"]), "Show / hide file tree"),
                    (short(&["diff.switch_source"]), "Switch diff source"),
                    (all("diff.close"), "Close diff viewer"),
                ],
            ),
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
        let mut d = Dialog::new("text", Body::Text { title: "Diff shortcuts".into(), lines, hints: vec![], scroll: 0 });
        d.centered = true;
        self.open_dialog(d);
    }

    // ---- rendering ---------------------------------------------------------

    pub fn draw_diff(&mut self, buf: &mut Buffer, area: Rect, t: &Theme) {
        fill(buf, area, t.background.base.tui());
        let Some(d) = self.diff.clone() else { return };
        let tree = self.diff_tree_shown();
        let mut content = area;
        if !tree {
            // Source header.
            let (label, _) = source_label(&d.mode);
            let count = format!("{}/{}", d.files.iter().filter(|f| d.reviewed.contains(&f.file)).count(), d.files.len());
            let mut segs = vec![Seg::new(label, bold(fg(t.text.action.secondary.base)))];
            if let Some(detail) = self.diff_detail() {
                segs.push(seg(format!(" · {detail}"), fg(t.text.muted)));
            }
            let x = area.x + 2;
            let w = area.width.saturating_sub(4) as usize;
            let segs = clip(&segs, w.saturating_sub(sw(&count) + 1));
            put_segs(buf, x, area.y, &segs, w);
            put(buf, area.x + area.width.saturating_sub(2 + sw(&count) as u16), area.y, &count, fg(t.text.muted), sw(&count));
            put(buf, area.right().saturating_sub(1), area.y, "?", fg(t.text.base), 1);
            self.hits.push((Rect::new(area.x, area.y, area.width.saturating_sub(4 + sw(&count) as u16), 1), Click::Custom("diff.switch_source".into())));
            content = Rect::new(area.x, area.y + 1, area.width, area.height.saturating_sub(1));
        }
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
            let lines = wrap(&[seg(m, fg(c))], content.width.saturating_sub(4) as usize, Wrap::Word);
            for (i, l) in lines.into_iter().enumerate() {
                put_segs(buf, content.x + 2, content.y + 2 + i as u16, &l, content.width.saturating_sub(4) as usize);
            }
            return;
        }
        let mut pane = content;
        if tree {
            let tw = self.diff_tree_width();
            self.draw_diff_tree(buf, Rect::new(content.x, content.y, tw, content.height), t, &d);
            pane = Rect::new(content.x + tw, content.y, content.width - tw, content.height);
        }
        let inner = Rect::new(pane.x + 2, pane.y, pane.width.saturating_sub(4), pane.height);
        // Top edge.
        fill(buf, Rect::new(inner.x, inner.y, inner.width, 1), t.diff.bg_context.tui());
        let view = Rect::new(inner.x, inner.y + 1, inner.width, inner.height.saturating_sub(1));
        let (lines, starts, hunks, headers) = self.diff_lines(t, &d, inner.width as usize);
        let total = lines.len();
        let scroll = d.scroll.min(total.saturating_sub(view.height as usize));
        if let Some(dv) = self.diff.as_mut() {
            dv.total = total;
            dv.viewport = view.height as usize;
            dv.file_starts = starts.clone();
            dv.hunk_starts = hunks;
            dv.scroll = scroll;
        }
        draw_lines(buf, view, &lines, scroll, "");
        // Sticky header of the card containing the top line.
        if let Some((fi, start)) = starts.iter().rev().find(|(_, s)| *s <= scroll).copied()
            && scroll > start
            && let Some((h0, h1)) = headers.iter().find(|(f, _, _)| *f == fi).map(|(_, a, b)| (a.clone(), b.clone()))
        {
            let end = starts.iter().find(|(_, s)| *s > start).map(|(_, s)| *s).unwrap_or(total);
            if scroll + 2 <= end {
                draw_lines(buf, Rect::new(view.x, view.y, view.width, 2), &[h0, h1], 0, "");
            }
        }
    }

    /// Lines of the patch pane plus file starts, hunk starts and header rows.
    #[allow(clippy::type_complexity)]
    fn diff_lines(&self, t: &Theme, d: &DiffViewer, width: usize) -> (Vec<SLine>, Vec<(usize, usize)>, Vec<usize>, Vec<(usize, SLine, SLine)>) {
        let mut out = Vec::new();
        let mut starts = Vec::new();
        let mut hunks = Vec::new();
        let mut headers = Vec::new();
        let split = self.diff_view_split();
        for (k, fi) in self.visible_files().into_iter().enumerate() {
            let Some(f) = d.files.get(fi) else { continue };
            let reviewed = d.reviewed.contains(&f.file);
            let card = if reviewed { t.background.raised.high } else { t.diff.bg_context };
            if k > 0 {
                out.push(SLine::new(vec![seg("▄".repeat(width), Style::default().fg(card.tui()).bg(t.background.base.tui()))]));
            }
            starts.push((fi, out.len()));
            let image = matches!(f.file.rsplit('.').next().map(|e| e.to_lowercase()).as_deref(), Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "bmp" | "ico"));
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
            let rw: usize = right.iter().map(|s| s.width()).sum();
            let pw = width.saturating_sub(rw + 3).max(1);
            let path = locale::truncate_file_path(&f.file, pw);
            let (dir, base) = match path.rfind('/') {
                Some(i) => (path[..=i].to_string(), path[i + 1..].to_string()),
                None => (String::new(), path.clone()),
            };
            let mut hs = vec![seg(" ", Style::default()), seg(dir, fg(t.text.muted)), seg(base, fg(if reviewed { t.text.muted } else { t.text.base }))];
            let used: usize = hs.iter().map(|s| s.width()).sum();
            hs.push(seg(" ".repeat(width.saturating_sub(used + rw + 1)), Style::default()));
            hs.extend(right);
            let h0 = SLine::new(hs).with_bg(Some(card.tui())).on_click(Some(Click::Custom(format!("diff.file:{fi}"))));
            let h1 = if reviewed {
                SLine::new(vec![seg("▀".repeat(width), Style::default().fg(card.tui()).bg(t.background.base.tui()))])
            } else {
                SLine::blank().with_bg(Some(card.tui()))
            };
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
            } else if f.patch.is_empty() {
                Some("No patch available for this file.")
            } else {
                None
            };
            if let Some(m) = fallback {
                for l in wrap(&[seg(m, fg(t.text.muted))], width.saturating_sub(2), Wrap::Word) {
                    out.push(SLine::new(l).prefixed(&[seg(" ", Style::default())]).with_bg(Some(card.tui())));
                }
                out.push(SLine::blank().with_bg(Some(card.tui())));
                continue;
            }
            let ft = crate::ui::highlight::filetype(Some(&f.file));
            let o = crate::ui::diff::DiffOpts { theme: t, syntax: &self.syntax, filetype: &ft, split: split && f.status.as_deref() == Some("modified"), wrap: true };
            let base_line = out.len();
            // Rendered (highlighted) diffs are cached per file: this runs on every frame.
            let key = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                (&f.file, &f.patch, o.split, width, self.themes.epoch, self.style_epoch).hash(&mut h);
                h.finish()
            };
            let cached = self.diff_cache.borrow().get(&key).cloned();
            let rendered = match cached {
                Some(r) => r,
                None => {
                    let r = crate::ui::diff::render(&crate::ui::diff::parse(&f.patch), width.saturating_sub(2), &o);
                    let mut c = self.diff_cache.borrow_mut();
                    if c.len() >= 256 {
                        c.clear();
                    }
                    c.insert(key, r.clone());
                    r
                }
            };
            hunks.push(base_line);
            for (i, l) in rendered.into_iter().enumerate() {
                if i > 0 && l.segs.first().is_some_and(|s| s.text.starts_with(" @@")) {
                    hunks.push(out.len());
                }
                out.push(l.prefixed(&[seg(" ", Style::default().bg(card.tui()))]).with_bg(Some(card.tui())));
            }
            out.push(SLine::blank().with_bg(Some(card.tui())));
        }
        (out, starts, hunks, headers)
    }

    fn draw_diff_tree(&mut self, buf: &mut Buffer, area: Rect, t: &Theme, d: &DiffViewer) {
        let bg = t.background.raised.base;
        fill(buf, area, bg.tui());
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
        let content_w = w.saturating_sub(1 + 1);
        let list_h = inner.height.saturating_sub(4) as usize;
        let selected_row = d.selected.and_then(|s| rows.iter().position(|r| r.file == Some(s))).unwrap_or(0);
        let start = selected_row.saturating_sub(list_h.saturating_sub(1));
        for (i, r) in rows.iter().enumerate().skip(start).take(list_h) {
            let y = inner.y + 2 + (i - start) as u16;
            let indent = "│ ".repeat(r.depth.min(content_w.saturating_sub(3) / 2));
            let marker = if r.file.is_some() { "≡ " } else { "▾ " };
            let reviewed = r.file.and_then(|f| d.files.get(f)).is_some_and(|f| d.reviewed.contains(&f.file));
            let fgc = if r.file.is_none() || reviewed { t.text.muted } else { t.text.base };
            let nw = content_w.saturating_sub(sw(&indent) + sw(marker));
            let name = if r.file.is_none() { truncate_chain(&r.name, nw) } else { locale::truncate_file_path(&r.name, nw) };
            let mut st = fg(fgc).bg(bg.tui());
            if r.file.is_some() && r.file == d.selected {
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
                put(buf, inner.right().saturating_sub(1), y, status, fg(sc).bg(bg.tui()), 1);
                self.hits.push((Rect::new(inner.x, y, inner.width, 1), Click::Custom(format!("diff.file:{fi}"))));
            }
        }
        // Footer help shortcut.
        let help = self.keybinds.shortcut("diff.help");
        if !help.is_empty() {
            let y = inner.bottom().saturating_sub(1);
            let x = inner.x + put(buf, inner.x, y, &help, fg(t.text.base).bg(bg.tui()), w) as u16;
            put(buf, x, y, " help", fg(t.text.muted).bg(bg.tui()), w);
        }
    }

    pub fn diff_click(&mut self, what: &str) {
        if what == "diff.switch_source" {
            self.open_diff_source();
            return;
        }
        if let Some(i) = what.strip_prefix("diff.file:").and_then(|s| s.parse::<usize>().ok()) {
            let start = self.diff.as_ref().and_then(|d| d.file_starts.iter().find(|(f, _)| *f == i).map(|(_, s)| *s));
            if let Some(dv) = self.diff.as_mut() {
                dv.selected = Some(i);
            }
            if let Some(s) = start {
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
        let files: Vec<FileDiff> = ["a/b/c/x.ts", "a/b/c/y.ts", "z.md"].iter().map(|f| FileDiff { file: f.to_string(), ..Default::default() }).collect();
        let rows = tree_rows(&files);
        assert_eq!(rows[0], TreeRow { depth: 0, name: "a/b/c".into(), file: None });
        assert_eq!(rows[1].name, "x.ts");
        assert_eq!(rows[3], TreeRow { depth: 0, name: "z.md".into(), file: Some(2) });
        assert_eq!(truncate_chain("a/b/c/d", 6), "…/c/d");
        assert_eq!(source_label("working").0, "Uncommitted");
    }
}
