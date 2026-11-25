//! Composer drawer above the prompt: Subagents / Shell / Terminals tabs
//! (`src/routes/session/composer/*`).

use super::{App, Route, Toast};
use crate::api::types::JobInfo;
use crate::keymap::{Match, Stroke};
use crate::theme::Theme;
use crate::ui::text::*;
use crate::util::locale::{self, titlecase, width as sw};
use ratatui::style::{Modifier, Style};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Composer {
    pub tab: String,
    pub selected: usize,
}

/// One subagent row.
#[derive(Clone, Debug, PartialEq)]
pub struct SubagentEntry {
    pub id: String,
    pub agent: String,
    pub title: String,
    pub running: bool,
    /// Running jobs in its tree (2.4.1): a subagent stays active while any of them runs.
    pub jobs: Vec<String>,
    pub current: bool,
    pub prefix: String,
}

impl SubagentEntry {
    /// Running itself, or owning a running job.
    pub fn active(&self) -> bool {
        self.running || !self.jobs.is_empty()
    }
}

/// `ownedJobs`: the running jobs in a session's tree. A subagent job belongs to the child it runs, which may sit
/// below a parent whose info is not loaded.
pub fn owned_jobs<'a>(jobs: &[&'a JobInfo], session: &str, parent: impl Fn(&str) -> Option<String>) -> Vec<&'a JobInfo> {
    jobs.iter()
        .filter(|job| {
            let subagent = job.kind == "subagent";
            let mut seen = std::collections::HashSet::new();
            let mut current = if subagent { job.child_session_id.clone() } else { Some(job.session_id.clone()) };
            while let Some(c) = current {
                if c == session {
                    return true;
                }
                if !seen.insert(c.clone()) {
                    return false;
                }
                // A subagent job runs in the direct child of the session that started it.
                current = parent(&c).or_else(|| (subagent && job.child_session_id.as_deref() == Some(c.as_str())).then(|| job.session_id.clone()));
            }
            false
        })
        .copied()
        .collect()
}

/// `busyCounts`: what a session has running, for the picker and the prompt footer.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BusyCounts {
    pub scripts: usize,
    pub subagents: usize,
    pub shells: usize,
}

/// `busyTab`: the tab the picker opens on when nothing asks for one: the first that has something running, else
/// Subagents.
pub fn busy_tab(counts: BusyCounts) -> &'static str {
    if counts.scripts > 0 {
        "scripts"
    } else if counts.subagents > 0 {
        "subagents"
    } else if counts.shells > 0 {
        "shell"
    } else {
        "subagents"
    }
}

/// `sessionFamily`: depth-first walk of the family tree under the root with tree prefixes.
pub fn session_family(sessions: &[(String, Option<String>)], session: &str) -> Vec<(String, String)> {
    let by: std::collections::HashMap<&str, Option<&str>> = sessions.iter().map(|(i, p)| (i.as_str(), p.as_deref())).collect();
    if !by.contains_key(session) {
        return vec![];
    }
    let mut root = session;
    let mut seen = std::collections::HashSet::new();
    while let Some(Some(p)) = by.get(root) {
        if !by.contains_key(p) || !seen.insert(*p) {
            break;
        }
        root = p;
    }
    // `visited` stops the recursion on a parent cycle in server data (it would overflow the stack).
    fn walk<'a>(parent: &str, ancestors: &[bool], sessions: &'a [(String, Option<String>)], visited: &mut std::collections::HashSet<&'a str>, out: &mut Vec<(String, String)>) {
        let group: Vec<&String> = sessions.iter().filter(|(i, p)| p.as_deref() == Some(parent) && !visited.contains(i.as_str())).map(|(i, _)| i).collect();
        visited.extend(group.iter().map(|i| i.as_str()));
        for (i, id) in group.iter().enumerate() {
            let last = i == group.len() - 1;
            let prefix = if ancestors.is_empty() {
                String::new()
            } else {
                ancestors[1..].iter().map(|a| if *a { "   " } else { "│  " }).collect::<String>() + if last { "└─ " } else { "├─ " }
            };
            out.push((id.to_string(), prefix));
            let mut next = ancestors.to_vec();
            next.push(last);
            walk(id, &next, sessions, visited, out);
        }
    }
    let mut out = Vec::new();
    let mut visited = std::collections::HashSet::from([root]);
    walk(root, &[], sessions, &mut visited, &mut out);
    out
}

/// A composer tab label, followed by the tab id.
pub const TAB_CLICK: &str = "composer.tab:";

impl App {
    /// `composerVisible` (2.3.1, #64343): a permission prompt wins over the composer, both the one a subagent session
    /// opens by default and one the user opened (`self.composer` is kept, so it comes back once the prompt is
    /// answered).
    pub fn composer_open(&self) -> bool {
        self.active_permission().is_none()
            && (self.composer.is_some() || (self.session().is_some_and(|s| s.parent_id.is_some()) && self.active_form().is_none()))
    }

    pub fn composer_tabs(&self) -> Vec<(&'static str, &'static str)> {
        let mut v = vec![("subagents", "Subagents"), ("shell", "Shell")];
        if self.terminals_available {
            v.push(("terminals", "Terminals"));
        }
        v
    }

    fn composer_state(&self) -> Composer {
        self.composer.clone().unwrap_or(Composer { tab: "subagents".into(), selected: 0 })
    }

    /// The parent of a session in loaded session info.
    fn parent_of(&self, id: &str) -> Option<String> {
        self.store.session(id).and_then(|s| s.parent_id.clone())
    }

    /// `busyCounts({ sessionID })`: running Code Mode scripts the session owns, subagents in its family that run or
    /// own a running job, and its running shell commands (the location's in-flight shells).
    pub fn busy_counts(&self, sid: &str) -> BusyCounts {
        let jobs = self.store.jobs_for(sid);
        let scripts = jobs.iter().filter(|j| j.kind == "codemode" && j.session_id == sid).count();
        let subagents = self
            .store
            .family_of(sid)
            .iter()
            .filter(|id| *id != sid && (self.store.is_running(id) || !owned_jobs(&jobs, id, |s| self.parent_of(s)).is_empty()))
            .count();
        let shells = self
            .loc()
            .map(|l| l.shells.iter().filter(|sh| sh.metadata.as_ref().and_then(|m| m.get("sessionID")).and_then(|v| v.as_str()) == Some(sid)).count())
            .unwrap_or(0);
        BusyCounts { scripts, subagents, shells }
    }

    pub fn subagent_entries(&self) -> Vec<SubagentEntry> {
        let Some(sid) = self.session_id() else { return vec![] };
        let sessions: Vec<(String, Option<String>)> = self.store.session_list().iter().map(|s| (s.id.clone(), s.parent_id.clone())).collect();
        let tree = self.store.jobs_for(&self.store.root(sid));
        let re = regex::Regex::new(r"@(\w+) subagent").unwrap();
        session_family(&sessions, sid)
            .into_iter()
            .filter_map(|(id, prefix)| {
                let s = self.store.session(&id)?;
                let title = s.display_title();
                let m = re.captures(&title).map(|c| (c[0].to_string(), c[1].to_string()));
                let agent = match (&s.agent, &m) {
                    (Some(a), _) => titlecase(a),
                    (None, Some((_, a))) => titlecase(a),
                    _ => "Subagent".into(),
                };
                let title = match &m {
                    Some((all, _)) => {
                        let t = title.replace(all.as_str(), "").trim().to_string();
                        if t.is_empty() { title.clone() } else { t }
                    }
                    None => title,
                };
                let running = self.store.is_running(&id);
                let jobs = owned_jobs(&tree, &id, |s| self.parent_of(s)).into_iter().map(|j| j.id.clone()).collect();
                Some(SubagentEntry { current: id == sid, id, agent, title, running, jobs, prefix })
            })
            .filter(|e| self.subagents_active == e.active())
            .collect()
    }

    fn shell_entries(&self) -> Vec<(String, String)> {
        let sid = self.session_id().unwrap_or("");
        self.store
            .locations
            .values()
            .flat_map(|l| l.shells.iter())
            .filter(|s| s.status == "running" && s.metadata.as_ref().and_then(|m| m.get("sessionID")).and_then(|v| v.as_str()).is_none_or(|x| x == sid))
            .map(|s| (s.id.clone(), s.command.lines().next().unwrap_or("").to_string()))
            .collect()
    }

    fn terminal_entries(&self) -> Vec<(String, String)> {
        let Some(sid) = self.session_id() else { return vec![] };
        let mut v: Vec<(String, String)> = self
            .session_terminals(sid)
            .0
            .iter()
            .filter_map(|t| {
                let id = t.get("id")?.as_str()?.to_string();
                let title = t.get("foregroundProcess").and_then(|x| x.as_str()).or(t.get("title").and_then(|x| x.as_str())).unwrap_or("Terminal").to_string();
                Some((id, title))
            })
            .collect();
        v.push(("new".into(), "+ New terminal".into()));
        v
    }

    /// `session.child.first` with a tab: open the composer on that tab (never closes it).
    pub fn open_composer_tab(&mut self, tab: &str) {
        // A tab the picker lacks (Scripts, opt-in) opens it on its first tab.
        let tab = if self.composer_tabs().iter().any(|(id, _)| *id == tab) { tab } else { "subagents" };
        if self.composer.as_ref().is_some_and(|c| c.tab == tab) {
            return;
        }
        let selected = if tab == "subagents" { self.subagent_entries().iter().position(|e| e.current).unwrap_or(0) } else { 0 };
        self.composer = Some(Composer { tab: tab.into(), selected });
        self.dirty = true;
    }

    /// A click on a composer tab label.
    pub fn composer_tab_click(&mut self, tab: &str) {
        if !self.composer_open() || !self.composer_tabs().iter().any(|(id, _)| *id == tab) {
            return;
        }
        let mut st = self.composer_state();
        if st.tab != tab {
            st.tab = tab.into();
            st.selected = 0;
        }
        self.composer = Some(st);
        self.dirty = true;
    }

    /// Without a tab: close an open picker, else open it on the first busy tab (2.4.1, #64972).
    pub fn toggle_composer(&mut self) {
        if self.composer.is_some() {
            self.composer = None;
        } else {
            let requested = self.session_id().map(|sid| busy_tab(self.busy_counts(sid))).unwrap_or("subagents");
            // Without the Scripts tab (opt-in) the picker opens on its first tab.
            let tab = if self.composer_tabs().iter().any(|(id, _)| *id == requested) { requested } else { "subagents" };
            let selected = if tab == "subagents" { self.subagent_entries().iter().position(|e| e.current).unwrap_or(0) } else { 0 };
            self.composer = Some(Composer { tab: tab.into(), selected });
        }
        self.dirty = true;
    }

    fn close_composer(&mut self) {
        self.composer = None;
        if let Some(p) = self.session().and_then(|s| s.parent_id.clone())
            && self.active_form().is_none()
        {
            self.navigate(Route::Session(p));
        }
    }

    /// Keys while the composer is open (`composer` mode, priority 1).
    pub fn composer_key(&mut self, s: &Stroke) -> bool {
        if !self.composer_open() {
            return false;
        }
        let mut st = self.composer_state();
        let kb = self.keybinds.clone();
        let is = |id: &str| kb.match_command(id, std::slice::from_ref(s)) == Match::Full;
        let tabs = self.composer_tabs();
        match (s.name.as_str(), s.ctrl) {
            ("left", false) | ("right", false) if tabs.len() > 1 => {
                let i = tabs.iter().position(|(id, _)| *id == st.tab).unwrap_or(0);
                let d: isize = if s.name == "left" { -1 } else { 1 };
                let n = tabs.len() as isize;
                st.tab = tabs[(((i as isize + d) % n + n) % n) as usize].0.to_string();
                st.selected = 0;
                self.composer = Some(st);
                return true;
            }
            ("escape", _) | ("c", true) => {
                self.close_composer();
                return true;
            }
            ("a", true) if st.tab == "subagents" => {
                self.subagents_active = !self.subagents_active;
                st.selected = 0;
                self.composer = Some(st);
                return true;
            }
            _ => {}
        }
        let count = match st.tab.as_str() {
            "subagents" => self.subagent_entries().len(),
            "shell" => self.shell_entries().len(),
            _ => self.terminal_entries().len(),
        };
        let (up, down, select, kill) = match st.tab.as_str() {
            "subagents" => ("composer.subagent.up", "composer.subagent.down", "composer.subagent.select", "composer.subagent.interrupt"),
            "shell" => ("composer.shell.up", "composer.shell.down", "composer.shell.select", "composer.shell.kill"),
            _ => ("composer.terminal.up", "composer.terminal.down", "composer.terminal.select", ""),
        };
        if is(up) {
            if st.selected == 0 && st.tab != "terminals" {
                self.close_composer();
            } else {
                st.selected = if st.selected == 0 { count.saturating_sub(1) } else { st.selected - 1 };
                self.composer = Some(st);
            }
            return true;
        }
        if is(down) {
            st.selected = if count == 0 { 0 } else if st.tab == "subagents" { (st.selected + 1).min(count - 1) } else { (st.selected + 1) % count };
            self.composer = Some(st);
            return true;
        }
        if is(select) {
            match st.tab.as_str() {
                "subagents" => {
                    if let Some(e) = self.subagent_entries().get(st.selected).cloned() {
                        self.composer = None;
                        self.navigate(Route::Session(e.id));
                    }
                }
                "shell" => {
                    if let Some((id, cmd)) = self.shell_entries().get(st.selected).cloned() {
                        self.open_shell_output(&id, &cmd);
                    }
                }
                _ => {
                    if let (Some((id, _)), Some(sid)) = (self.terminal_entries().get(st.selected).cloned(), self.session_id().map(|x| x.to_string())) {
                        self.composer = None;
                        self.terminal_open = true;
                        if id == "new" {
                            self.new_terminal(&sid);
                        } else {
                            self.select_terminal(&sid, Some(&id));
                        }
                    }
                }
            }
            return true;
        }
        if !kill.is_empty() && is(kill) {
            match st.tab.as_str() {
                "subagents" => {
                    // A running subagent is interrupted, and the jobs in its tree are cancelled.
                    if let Some(e) = self.subagent_entries().get(st.selected).cloned() {
                        if e.running {
                            let c = self.client.clone();
                            let id = e.id.clone();
                            self.call(async move { c.interrupt(&id, false).await }, |_, _| {});
                        }
                        for job in e.jobs {
                            let c = self.client.clone();
                            self.call(async move { c.job_cancel(&job).await }, |_, _| {});
                        }
                    }
                }
                _ => {
                    if let Some((id, _)) = self.shell_entries().get(st.selected).cloned() {
                        let c = self.client.clone();
                        let dir = self.location_dir();
                        self.call(async move { c.shell_remove(&id, Some(&dir)).await }, |_, _| {});
                    }
                }
            }
            return true;
        }
        false
    }

    pub fn open_shell_output(&mut self, id: &str, command: &str) {
        let c = self.client.clone();
        let dir = self.location_dir();
        let shell = id.to_string();
        let cmd = command.to_string();
        let running = self.loc().is_some_and(|l| l.shells.iter().any(|s| s.id == shell && s.status == "running"));
        self.spawn(async move { c.shell_output(&shell, 0, Some(&dir)).await }, move |app, r| {
            use crate::ui::dialog::{Dialog, Footer, FooterItem, Size, TextDialog};
            let t = app.theme().dialog().clone();
            // `DialogShellOutput`: the command (up to 3 rows), the omitted note, then the output.
            let mut pre: Vec<SLine> = wrap(&[seg(cmd.clone(), fg(t.text.muted))], (Size::XLarge.width() as usize).min(app.size.0.saturating_sub(2) as usize).saturating_sub(6), crate::ui::text::Wrap::Word)
                .into_iter()
                .take(3)
                .map(SLine::new)
                .collect();
            pre.push(SLine::blank());
            let mut lines = Vec::new();
            match r {
                Ok(o) => {
                    if o.truncated {
                        pre.push(SLine::new(vec![seg("Earlier output omitted · showing recent output", fg(t.text.muted))]));
                        pre.push(SLine::blank());
                    }
                    let text = locale::strip_ansi(&o.output).replace("\r\n", "\n").replace('\r', "\n");
                    if text.trim().is_empty() {
                        lines.push(SLine::new(vec![seg("No captured output. Output redirected to files is not shown here.", fg(t.text.muted))]));
                    }
                    for l in text.lines() {
                        lines.push(SLine::new(vec![seg(l.to_string(), fg(t.text.base))]));
                    }
                }
                Err(_) => lines.push(SLine::new(vec![seg("Shell output is no longer available.", fg(t.text.feedback.error.base))])),
            }
            let height = ((app.size.1 as f64 * 0.6).floor() as usize).saturating_sub(6).max(3);
            let mut td = TextDialog::new("Shell output", lines);
            td.pre = pre;
            td.title_extra = Some((if running { "Running".to_string() } else { "Exited".to_string() }, false));
            td.header_gap = if app.size.0 < 60 { 1 } else { 2 };
            td.height = Some(height);
            td.scroll = usize::MAX / 2;
            td.footer = Some(Footer { secondary: vec![FooterItem::new("scroll", "↑/↓"), FooterItem::new("follow", "end"), FooterItem::new("back", "esc")], ..Default::default() });
            let mut d = Dialog::text("text", td);
            d.size = Size::XLarge;
            d.centered = true;
            app.open_dialog(d);
        });
    }

    /// Composer lines (rendered above the prompt).
    pub fn composer_lines(&self, t: &Theme, width: usize) -> Vec<SLine> {
        let st = self.composer_state();
        let kb = &self.keybinds;
        let bg = t.background.raised.base;
        let bar = Seg::new("┃", Style::default().fg(t.border.tui()).bg(bg.tui()));
        let inner = width.saturating_sub(4);
        let mut body: Vec<SLine> = Vec::new();
        let tabs = self.composer_tabs();
        let mut head: Vec<Seg> = vec![seg(" ", fg(t.text.base))];
        if tabs.len() > 1 {
            for (i, (id, label)) in tabs.iter().enumerate() {
                if i > 0 {
                    head.push(seg("  ", fg(t.text.base)));
                }
                // A tab label switches to its tab on click (2.3.0).
                let click = Click::Custom(format!("{TAB_CLICK}{id}"));
                if *id == st.tab {
                    head.push(Seg::new(*label, bold(fg(t.text.base))).on_click(click));
                } else {
                    head.push(seg(*label, fg(t.text.muted)).on_click(click));
                }
            }
        } else {
            head.push(Seg::new(tabs[0].1, bold(fg(t.text.base))));
        }
        let used: usize = head.iter().map(|s| s.width()).sum();
        head.push(seg(" ".repeat(inner.saturating_sub(used + 3)), fg(t.text.base)));
        head.push(seg("esc", fg(t.text.muted)));
        body.push(SLine::new(head));
        body.push(SLine::blank());
        let row = |text: String, right: Option<&str>, selected: bool, current: bool| -> SLine {
            let (f, b, bold_) = if selected {
                (t.text.action.primary.focused, t.background.action.primary.focused, true)
            } else if current {
                (t.text.action.primary.selected, t.background.action.primary.selected, false)
            } else {
                (t.text.action.primary.base, t.background.action.primary.base, false)
            };
            let mut st = fg(f);
            if b.a > 0 {
                st = st.bg(b.tui());
            }
            if bold_ {
                st = st.add_modifier(Modifier::BOLD);
            }
            let rw = right.map(sw).unwrap_or(0);
            let text = locale::truncate_width(&text, inner.saturating_sub(2 + rw + 1));
            let mut segs = vec![Seg::new(" ", st), Seg::new(text.clone(), st)];
            let used = 1 + sw(&text);
            segs.push(Seg::new(" ".repeat(inner.saturating_sub(used + rw + 1)), st));
            if let Some(r) = right {
                segs.push(Seg::new(r.to_string(), if selected { st } else { fg(t.text.muted) }));
            }
            segs.push(Seg::new(" ", st));
            SLine::new(segs)
        };
        let mut list: Vec<SLine> = Vec::new();
        let mut hints: Vec<(String, String)> = Vec::new();
        match st.tab.as_str() {
            "subagents" => {
                let entries = self.subagent_entries();
                if entries.is_empty() {
                    list.push(SLine::new(vec![seg(if self.subagents_active { " No active subagents" } else { " No inactive subagents" }, fg(t.text.muted))]));
                }
                for (i, e) in entries.iter().enumerate() {
                    list.push(row(format!("{}{}: {}", e.prefix, e.agent, e.title), if e.running { Some("Running") } else { None }, i == st.selected, e.current));
                }
                if entries.get(st.selected).is_some_and(|e| e.active()) {
                    hints.push(("interrupt".into(), kb.shortcut("composer.subagent.interrupt")));
                }
                hints.push((format!("show {}", if self.subagents_active { "inactive" } else { "active" }), "ctrl+a".into()));
            }
            "shell" => {
                let entries = self.shell_entries();
                if entries.is_empty() {
                    list.push(SLine::new(vec![seg(" No shell commands", fg(t.text.muted))]));
                }
                for (i, (_, cmd)) in entries.iter().enumerate() {
                    list.push(row(cmd.clone(), None, i == st.selected, false));
                }
                if !entries.is_empty() {
                    hints.push(("output".into(), kb.shortcut("composer.shell.select")));
                    hints.push(("kill".into(), kb.shortcut("composer.shell.kill")));
                }
            }
            _ => {
                let visible = self.terminal_visible();
                for (i, (id, title)) in self.terminal_entries().iter().enumerate() {
                    list.push(row(title.clone(), None, i == st.selected, visible.as_deref() == Some(id.as_str())));
                }
            }
        }
        let start = st.selected.saturating_sub(4);
        body.extend(list.into_iter().skip(start).take(5));
        body.push(SLine::blank());
        let mut foot: Vec<Seg> = vec![seg(" ", fg(t.text.base))];
        for (label, key) in &hints {
            if foot.len() > 1 {
                foot.push(seg("  ", fg(t.text.base)));
            }
            foot.push(Seg::new(label.clone(), bold(fg(t.text.base))));
            foot.push(seg(format!(" {key}"), fg(t.text.muted)));
        }
        if tabs.len() > 1 {
            if foot.len() > 1 {
                foot.push(seg("  ", fg(t.text.base)));
            }
            foot.push(Seg::new("tabs", bold(fg(t.text.base))));
            foot.push(seg(" ←/→", fg(t.text.muted)));
        }
        body.push(SLine::new(foot));
        let mut out = vec![SLine::new(vec![bar.clone()]).with_bg(Some(bg.tui()))];
        for l in body {
            out.push(l.prefixed(&[bar.clone(), seg(" ", fg(t.text.base))]).with_bg(Some(bg.tui())));
        }
        out.push(SLine::new(vec![bar]).with_bg(Some(bg.tui())));
        out
    }

    pub fn composer_toast_unavailable(&mut self) {
        self.toast(Toast::new("error", "Unable to load terminal"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_prefixes() {
        let s = vec![
            ("root".to_string(), None),
            ("a".to_string(), Some("root".to_string())),
            ("a1".to_string(), Some("a".to_string())),
            ("b".to_string(), Some("root".to_string())),
        ];
        let f = session_family(&s, "a1");
        assert_eq!(f, vec![("a".into(), "".into()), ("a1".into(), "└─ ".into()), ("b".into(), "".into())]);
    }

    /// A parent cycle in server data (or a session that is its own parent) used to recurse until
    /// the stack overflowed, which aborts the process.
    #[test]
    fn family_parent_cycle_terminates() {
        let s = vec![("a".to_string(), Some("b".to_string())), ("b".to_string(), Some("a".to_string()))];
        assert_eq!(session_family(&s, "a"), vec![("b".to_string(), "".to_string())]);
        let s = vec![("a".to_string(), Some("a".to_string())), ("c".to_string(), Some("a".to_string()))];
        assert_eq!(session_family(&s, "a"), vec![("c".to_string(), "".to_string())]);
    }
}
