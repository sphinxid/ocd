//! Open (`dialog-open.tsx`), Worktrees (`dialog-workspaces.tsx`), worktree naming and Pair
//! (`dialog-pair.tsx`) dialogs.

use super::{App, Route, Toast};
use crate::ui::dialog::{Action, Body, Dialog, Opt, PromptDialog, Select, Size};
use crate::ui::text::*;
use crate::util::locale::{self, abbreviate_home, basename, width as sw};
use serde_json::{Value, json};

/// `timeAgo` for the Open dialog: `now`, `{m}m`, `{h}h`, `{d}d`, `{mo}mo`, `{y}y`.
pub fn time_ago(ms: i64, now: i64) -> String {
    let s = (now - ms).max(0) / 1000;
    let m = s / 60;
    let h = m / 60;
    let d = h / 24;
    if m < 1 {
        "now".into()
    } else if h < 1 {
        format!("{m}m")
    } else if d < 1 {
        format!("{h}h")
    } else if d < 30 {
        format!("{d}d")
    } else if d < 365 {
        format!("{}mo", d / 30)
    } else {
        format!("{}y", d / 365)
    }
}

/// Worktree slug: lowercase, `[^a-z0-9]+` → `-`, edges trimmed.
pub fn slugify(s: &str) -> String {
    let lower = s.to_lowercase();
    let re = regex::Regex::new(r"[^a-z0-9]+").unwrap();
    re.replace_all(&lower, "-").trim_matches('-').to_string()
}

pub fn random_slug() -> String {
    const ADJ: [&str; 12] = ["brave", "calm", "eager", "fancy", "gentle", "happy", "jolly", "kind", "lively", "nimble", "proud", "quiet"];
    const NOUN: [&str; 12] = ["cabin", "comet", "falcon", "forest", "harbor", "island", "meadow", "otter", "planet", "river", "summit", "willow"];
    format!("{}-{}", ADJ[rand::random_range(0..ADJ.len())], NOUN[rand::random_range(0..NOUN.len())])
}

/// `uqr.renderUnicodeCompact(text, {border: 1})`: light modules are drawn filled.
pub fn qr_compact(text: &str) -> Vec<String> {
    let Ok(code) = qrcode::QrCode::new(text.as_bytes()) else { return vec![] };
    let w = code.width();
    let colors = code.to_colors();
    let light = |x: isize, y: isize| -> bool {
        if x < 0 || y < 0 || x >= w as isize || y >= w as isize {
            return true;
        }
        colors[(y as usize) * w + x as usize] == qrcode::Color::Light
    };
    let size = w as isize + 2;
    let mut out = Vec::new();
    let mut y = 0;
    while y < size {
        let mut line = String::new();
        for x in 0..size {
            let top = light(x - 1, y - 1);
            let bottom = if y + 1 < size { light(x - 1, y) } else { false };
            line.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push(line);
        y += 2;
    }
    out
}

fn content_width(w: usize) -> usize {
    w.saturating_sub(6)
}

impl App {
    fn project_id(&self) -> Option<String> {
        self.store.locations.get(&self.location_dir()).and_then(|l| l.info.as_ref()).map(|i| i.project.id.clone()).filter(|p| !p.is_empty())
    }

    // ---- Open dialog -------------------------------------------------------

    pub fn open_open_dialog(&mut self) {
        if self.dialogs.last().is_some_and(|d| d.purpose == "open") {
            return;
        }
        let mut s = Select::new("Open", vec![]);
        s.placeholder = "Search sessions and projects…".into();
        s.empty = Some("No recent sessions or projects".into());
        s.no_match = Some(format!("No matches · search all sessions with {}", self.keybinds.shortcut("session.list")));
        s.footer_text = Some(("Refreshing sessions and projects…".into(), self.theme().dialog().text.muted));
        self.open_dialog(Dialog::select("open", s, Size::Large));
        self.refresh_open_options(vec![]);
        let c = self.client.clone();
        self.spawn(async move { c.projects().await }, |app, r| {
            match r {
                // Archived projects stay out of the project list.
                Ok(p) => app.refresh_open_options(p.into_iter().filter(|p| p.pointer("/time/archived").is_none_or(|a| a.is_null())).collect()),
                Err(_) => {
                    if let Some(sel) = app.dialogs.last_mut().filter(|d| d.purpose == "open").and_then(|d| d.sel()) {
                        sel.footer_text = Some(("Could not refresh projects.".into(), app.themes.current.dialog().text.feedback.error.base));
                    }
                }
            }
            app.dirty = true;
        });
    }

    pub(crate) fn refresh_open_options(&mut self, projects: Vec<Value>) {
        let t = self.theme().dialog().clone();
        let now = locale::now_ms();
        let mut opts = Vec::new();
        let current = self.session_id().map(|s| s.to_string());
        let mut sessions: Vec<_> = self
            .store
            .session_list()
            .into_iter()
            .filter(|s| s.parent_id.is_none())
            .map(|s| {
                let attention = self.store.permissions.get(&s.id).is_some_and(|p| !p.is_empty()) || self.store.forms.get(&s.id).is_some_and(|f| !f.is_empty());
                (attention, s.clone())
            })
            .filter(|(attention, s)| !self.tabs.contains(&s.id) || (*attention && current.as_deref() != Some(s.id.as_str())))
            .collect();
        sessions.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.time.updated.cmp(&a.1.time.updated)));
        let attention_count = sessions.iter().filter(|(a, _)| *a).count();
        for (attention, s) in sessions.into_iter().take(8.max(attention_count)) {
            let dir = &s.location.directory;
            let project = self.store.locations.get(dir).and_then(|l| l.info.as_ref()).map(|i| basename(&i.project.canonical).to_string());
            let label = match project {
                Some(p) if p != basename(dir) => format!("{p} · {}", basename(dir)),
                Some(p) => p,
                None => basename(dir).to_string(),
            };
            let mut o = Opt::new(s.display_title(), format!("session:{}", s.id)).cat("Sessions");
            o.footer = Some(format!("{} · {}", locale::truncate(&label, 30), time_ago(s.time.updated, now)));
            o.search_text = Some(format!("{} {dir}", s.id));
            if attention {
                o.gutter = Some(Seg::new("!", bold(fg(t.text.feedback.warning.base))));
            } else if self.store.is_running(&s.id) {
                o.gutter = Some(seg(SPINNER_GLYPH.to_string(), fg(t.text.muted)));
            }
            opts.push(o);
        }
        let current_project = self.store.locations.get(&self.location_dir()).and_then(|l| l.info.as_ref()).map(|i| i.project.canonical.clone());
        let mut seen = std::collections::HashSet::new();
        for p in &projects {
            let Some(canonical) = p.get("canonical").and_then(|c| c.as_str()) else { continue };
            if canonical == "/" || !seen.insert(canonical.to_string()) {
                continue;
            }
            let name = p.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()).unwrap_or_else(|| basename(canonical).to_string());
            let git = p.get("vcs").and_then(|v| v.as_str()) == Some("git");
            let width = content_width(88.min(self.size.0.saturating_sub(2)) as usize).saturating_sub(sw(&name) + if git { 2 } else { 0 });
            let mut o = Opt::new(name, format!("project:{canonical}")).cat("Projects");
            o.footer = Some(format!("{}{}", locale::truncate_file_path(&abbreviate_home(canonical), width.max(4)), if git { " →" } else { "" }));
            o.search_text = Some(canonical.to_string());
            if current_project.as_deref() == Some(canonical) {
                o.gutter = Some(seg("●", fg(t.text.formfield.selected)));
            }
            o.details = vec![];
            opts.push(o);
        }
        if let Some(sel) = self.dialogs.last_mut().filter(|d| d.purpose == "open").and_then(|d| d.sel()) {
            sel.options = opts;
            if !projects.is_empty() {
                sel.footer_text = None;
            }
            sel.actions = vec![Action::key("right")];
        }
        if let Some(d) = self.dialogs.last_mut().filter(|d| d.purpose == "open") {
            d.data = json!(projects);
        }
    }

    pub fn open_selected(&mut self, value: &str) {
        if let Some(id) = value.strip_prefix("session:") {
            self.dialogs.clear();
            self.navigate(Route::Session(id.to_string()));
        } else if let Some(dir) = value.strip_prefix("project:").or_else(|| value.strip_prefix("worktree:")) {
            self.dialogs.clear();
            self.set_home_location(dir);
        }
    }

    pub fn set_home_location(&mut self, dir: &str) {
        self.opts.directory = dir.to_string();
        self.client.directory = dir.to_string();
        self.store.directory = dir.to_string();
        self.navigate(Route::Home);
        self.sync_location();
        self.dirty = true;
    }

    /// `right` on a git project in the Open dialog: show its worktrees.
    pub fn open_project_worktrees(&mut self, value: &str) {
        let Some(dir) = value.strip_prefix("project:") else { return };
        let projects = self.dialogs.last().map(|d| d.data.clone()).unwrap_or(Value::Null);
        let Some(project) = projects.as_array().and_then(|a| a.iter().find(|p| p.get("canonical").and_then(|c| c.as_str()) == Some(dir))).cloned() else { return };
        if project.get("vcs").and_then(|v| v.as_str()) != Some("git") {
            return;
        }
        let name = project.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()).unwrap_or_else(|| "Project".into());
        let id = project.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
        let mut s = Select::new(format!("{name} / Worktrees"), vec![]);
        s.placeholder = "Search worktrees…".into();
        s.can_back = true;
        s.empty = Some("Loading worktrees…".into());
        s.no_match = Some("No matching worktrees".into());
        s.footer_text = Some(("Loading worktrees…".into(), self.theme().dialog().text.muted));
        s.hints = vec![("new worktree".into(), "ctrl+n".into())];
        s.actions = vec![
            Action::key("left"),
            Action::key("ctrl+n"),
        ];
        let mut d = Dialog::select("open-worktrees", s, Size::Large);
        d.data = json!({ "project": project, "projects": projects, "id": id, "root": dir, "name": name });
        self.dialogs.clear();
        self.dialogs.push(d);
        let c = self.client.clone();
        let root = dir.to_string();
        let cwd = self.location_dir();
        self.spawn(async move { c.worktrees(&id).await }, move |app, r| {
            let Some(sel) = app.dialogs.last_mut().filter(|d| d.purpose == "open-worktrees").and_then(|d| d.sel()) else { return };
            sel.footer_text = None;
            match r {
                Ok(list) => {
                    let mut dirs: Vec<String> = vec![root.clone()];
                    if cwd != root {
                        dirs.push(cwd.clone());
                    }
                    for w in list {
                        if let Some(d) = w.get("directory").and_then(|d| d.as_str())
                            && !dirs.iter().any(|x| x == d)
                        {
                            dirs.push(d.to_string());
                        }
                    }
                    let width = dirs.iter().map(|d| sw(&abbreviate_home(d))).max().unwrap_or(0);
                    sel.options = dirs.iter().map(|d| Opt::new(basename(d).to_string(), format!("worktree:{d}")).foot(format!("{:<width$}", abbreviate_home(d)))).collect();
                    sel.empty = Some("No matching worktrees".into());
                }
                Err(_) => sel.footer_text = Some(("Could not refresh projects.".into(), app.themes.current.dialog().text.feedback.error.base)),
            }
            app.dirty = true;
        });
    }

    pub fn open_new_worktree_prompt(&mut self) {
        let data = self.dialogs.last().map(|d| d.data.clone()).unwrap_or(Value::Null);
        let name = data.get("name").and_then(|n| n.as_str()).unwrap_or("Project").to_string();
        let mut p = PromptDialog::new(&format!("{name} / New worktree"), "Worktree name (optional)", "");
        p.description = vec![("Leave blank for a random name.".into(), self.theme().dialog().text.muted)];
        let mut d = Dialog::new("new-worktree", Body::Prompt(p));
        d.size = Size::Large;
        d.data = data;
        self.open_dialog(d);
    }

    pub fn create_worktree(&mut self, name: &str, project: &str, then_move: bool) {
        let c = self.client.clone();
        let project = project.to_string();
        let name = if name.trim().is_empty() { None } else { Some(slugify(name)) };
        if let Some(Body::Prompt(p)) = self.dialogs.last_mut().map(|d| &mut d.body) {
            p.busy = Some("Creating worktree…".into());
        }
        self.spawn(async move { c.worktree_create(&project, name.as_deref()).await }, move |app, r| match r {
            Ok(info) => {
                app.dialogs.clear();
                if let Some(dir) = info.get("directory").and_then(|d| d.as_str()) {
                    let dir = dir.to_string();
                    if then_move && let Some(sid) = app.session_id().map(|s| s.to_string()) {
                        app.move_session(&sid, &dir);
                    } else {
                        app.set_home_location(&dir);
                    }
                }
            }
            Err(e) => {
                app.dialogs.clear();
                app.toast(Toast::new("error", crate::api::error_message(&e)).title("Creating worktree failed"));
            }
        });
    }

    pub fn move_session(&mut self, sid: &str, dir: &str) {
        let c = self.client.clone();
        let s = sid.to_string();
        let d = dir.to_string();
        self.spawn(async move { c.session_move(&s, &d).await }, |app, r| {
            if let Err(e) = r {
                app.toast(Toast::new("error", crate::api::error_message(&e)).title("Failed to move session"));
            }
        });
    }

    // ---- Worktrees dialog --------------------------------------------------

    pub fn open_worktrees_dialog(&mut self) {
        let Some(project) = self.project_id() else {
            self.toast(Toast::new("error", "Could not load worktrees"));
            return;
        };
        self.open_worktrees_dialog_for(project);
    }

    /// The Worktrees dialog for a project (also the missing-folder screen's "Choose directory").
    pub fn open_worktrees_dialog_for(&mut self, project: String) {
        let mut s = Select::new("Worktrees", vec![]);
        s.flat = true;
        s.empty = Some("Loading worktrees…".into());
        s.no_match = Some("No worktrees found".into());
        let in_session = self.session_id().is_some();
        s.actions = vec![
            Action::new("dialog.move_session.move", "move").hidden(!in_session),
            Action::new("dialog.move_session.new", "new").dialog_level(),
            Action::new("dialog.move_session.delete", "delete"),
            Action::new("dialog.move_session.refresh", "refresh").dialog_level(),
        ];
        let mut d = Dialog::select("worktrees", s, Size::XLarge);
        d.data = json!({ "id": project });
        self.open_dialog(d);
        self.load_worktrees(&project);
    }

    fn load_worktrees(&mut self, project: &str) {
        let c = self.client.clone();
        let p = project.to_string();
        let cwd = self.location_dir();
        let root = self.store.locations.get(&cwd).and_then(|l| l.info.as_ref()).map(|i| i.project.canonical.clone()).unwrap_or(cwd.clone());
        self.spawn(async move { c.worktrees(&p).await }, move |app, r| {
            let tw = content_width(116.min(app.size.0.saturating_sub(2)) as usize);
            let t = app.theme().dialog().clone();
            let Some(sel) = app.dialogs.last_mut().filter(|d| d.purpose == "worktrees").and_then(|d| d.sel()) else { return };
            match r {
                Ok(list) => {
                    let mut roots: Vec<(String, bool)> = vec![(root.clone(), false)];
                    for w in &list {
                        if let Some(d) = w.get("directory").and_then(|d| d.as_str())
                            && d != root
                        {
                            roots.push((d.to_string(), w.get("strategy").and_then(|s| s.as_str()).is_some()));
                        }
                    }
                    let current = roots.iter().filter(|(r, _)| cwd == *r || cwd.starts_with(&format!("{r}/"))).map(|(r, _)| r.clone()).max_by_key(|r| r.len());
                    roots.sort_by(|a, b| {
                        let ac = Some(&a.0) == current.as_ref();
                        let bc = Some(&b.0) == current.as_ref();
                        bc.cmp(&ac).then(a.1.cmp(&b.1)).then(a.0.len().cmp(&b.0.len()))
                    });
                    let mut opts = Vec::new();
                    for (r, worktree) in &roots {
                        let is_current = Some(r) == current.as_ref();
                        let mut o = Opt::new(abbreviate_home(r), format!("{}{r}", if *worktree { "w:" } else { "r:" })).cat(if is_current { "Current" } else { "Other" });
                        o.title_width = Some(tw);
                        o.truncate_left = true;
                        opts.push(o);
                        if is_current && cwd != *r {
                            let rel = cwd[r.len()..].to_string();
                            let mut o = Opt::new(format!("{}{rel}", abbreviate_home(r)), format!("s:{cwd}")).cat("Current");
                            o.title_width = Some(tw);
                            o.truncate_left = true;
                            o.footer = Some(rel);
                            o.footer_color = Some(t.text.muted);
                            opts.push(o);
                        }
                    }
                    sel.options = opts;
                    sel.empty = Some("No worktrees available".into());
                }
                Err(e) => {
                    sel.empty = Some(format!("Could not load worktrees · {} · Close and reopen Worktrees to try again.", crate::api::error_message(&e)));
                }
            }
            app.dirty = true;
        });
    }

    pub fn worktree_action(&mut self, id: &str, value: Option<String>) {
        let project = self.dialogs.last().and_then(|d| d.data.get("id").and_then(|i| i.as_str()).map(|s| s.to_string())).unwrap_or_default();
        let dir = value.as_deref().map(|v| v.get(2..).unwrap_or("").to_string());
        match id {
            "dialog.move_session.move" => {
                if let (Some(sid), Some(d)) = (self.session_id().map(|s| s.to_string()), dir) {
                    self.dialogs.clear();
                    self.move_session(&sid, &d);
                }
            }
            "dialog.move_session.new" => {
                let mut p = PromptDialog::new("Name worktree", "Worktree name", "");
                p.description = vec![];
                let mut d = Dialog::new("worktree-name", Body::Prompt(p));
                d.data = json!({ "id": project });
                self.open_dialog(d);
            }
            "dialog.move_session.refresh" => {
                let c = self.client.clone();
                let p = project.clone();
                self.call(async move { c.worktree_refresh(&p).await }, move |app, _| app.load_worktrees(&project));
            }
            "dialog.move_session.delete" => {
                let Some(v) = value else { return };
                if !v.starts_with("w:") {
                    return;
                }
                let armed = self.dialogs.last().and_then(|d| d.data.get("armed").and_then(|a| a.as_str()).map(|s| s.to_string()));
                if armed.as_deref() != Some(v.as_str()) {
                    let key = self.keybinds.shortcut(id);
                    let t = self.theme().dialog().clone();
                    if let Some(d) = self.dialogs.last_mut() {
                        d.data["armed"] = json!(v);
                        if let Some(sel) = d.sel()
                            && let Some(o) = sel.options.iter_mut().find(|o| o.value == v)
                        {
                            o.title = format!("Press {key} again to confirm");
                            o.fg = Some(t.text.action.destructive.base);
                            o.bg = Some(t.background.action.destructive.base);
                        }
                    }
                    return;
                }
                let d = v[2..].to_string();
                let c = self.client.clone();
                let p = project.clone();
                self.spawn(async move { c.worktree_remove(&p, &d, false).await }, move |app, r| match r {
                    Ok(_) => app.load_worktrees(&project),
                    Err(e) => {
                        let force = e.downcast_ref::<crate::api::ApiError>().is_some_and(|a| a.body.pointer("/data/forceRequired") == Some(&Value::Bool(true)));
                        if force {
                            let mut dlg = Dialog::new(
                                "worktree-force",
                                Body::Confirm { title: "Delete worktree?".into(), message: "This worktree has file changes. Do you want to delete it anyway?".into(), confirm: true },
                            );
                            dlg.data = json!({ "id": project, "dir": v[2..] });
                            app.open_dialog(dlg);
                        } else {
                            app.toast(Toast::new("error", crate::api::error_message(&e)).title("Failed to delete worktree"));
                        }
                    }
                });
            }
            _ => {}
        }
        self.dirty = true;
    }

    // ---- Pair --------------------------------------------------------------

    pub fn open_pair(&mut self) {
        let mut d = Dialog::text("text", crate::ui::dialog::TextDialog::new("Pair", vec![SLine::new(vec![seg("Loading server information…", fg(self.theme().dialog().text.muted))])]));
        d.size = Size::Large;
        d.centered = true;
        d.purpose = "pair".into();
        self.open_dialog(d);
        let c = self.client.clone();
        self.spawn(async move { Ok((c.info().await?, c.pair().await?)) }, |app, r| {
            let t = app.theme().dialog().clone();
            let lines = match r {
                Ok((info, pair)) => {
                    let code = pair.get("code").and_then(|c| c.as_str()).unwrap_or("").to_string();
                    let minutes = (pair.get("expires_in").and_then(|e| e.as_f64()).unwrap_or(300.0) / 60.0).round() as i64;
                    let urls: Vec<String> = info.get("urls").and_then(|u| u.as_array()).map(|a| a.iter().filter_map(|u| u.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
                    let link = |u: &str| format!("{}/auth/connect/{code}", u.trim_end_matches('/'));
                    let links: Vec<String> = urls.iter().map(|u| link(u)).collect();
                    let host_of = |u: &str| u.split("://").nth(1).unwrap_or(u).split('/').next().unwrap_or("").rsplit_once(':').map(|(h, _)| h.to_string()).unwrap_or_default();
                    let localhost = urls.first().map(|u| {
                        let h = host_of(u);
                        link(&u.replacen(&h, "localhost", 1))
                    });
                    let loopback = urls.iter().any(|u| ["localhost", "127.0.0.1", "[::1]"].contains(&host_of(u).as_str()));
                    let mut l = vec![SLine::new(vec![seg(format!("Open a link to connect. Links work once and expire in {minutes} minutes."), fg(t.text.muted))]), SLine::blank()];
                    if let Some(lh) = localhost {
                        l.push(SLine::new(vec![seg("This device", fg(t.text.muted))]));
                        l.push(SLine::new(vec![seg(lh, fg(t.text.base))]));
                        l.push(SLine::blank());
                    }
                    l.push(SLine::new(vec![seg("Links", fg(t.text.muted))]));
                    for k in &links {
                        l.push(SLine::new(vec![seg(k.clone(), fg(t.text.base))]));
                    }
                    if loopback {
                        l.push(SLine::blank());
                        l.push(SLine::new(vec![seg("Run `opencode service set hostname 0.0.0.0` to access the service remotely.", fg(t.text.muted))]));
                    }
                    if let Some(first) = links.first() {
                        l.push(SLine::blank());
                        for q in qr_compact(first) {
                            l.push(SLine::new(vec![seg(q, fg(t.text.base))]));
                        }
                    }
                    l
                }
                Err(e) => vec![
                    SLine::new(vec![Seg::new("Could not load server information", bold(fg(t.text.feedback.error.base)))]),
                    SLine::new(vec![seg(crate::api::error_message(&e), fg(t.text.muted))]),
                    SLine::new(vec![seg("Close and reopen Pair to try again.", fg(t.text.muted))]),
                ],
            };
            if let Some(td) = app.dialogs.last_mut().filter(|d| d.purpose == "pair").and_then(|d| d.text_body()) {
                td.lines = lines;
            }
            app.dirty = true;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(time_ago(0, 30_000), "now");
        assert_eq!(time_ago(0, 5 * 60_000), "5m");
        assert_eq!(time_ago(0, 3 * 3_600_000), "3h");
        assert_eq!(time_ago(0, 40 * 86_400_000), "1mo");
        assert_eq!(slugify("  My New_Feature!! "), "my-new-feature");
        let q = qr_compact("http://localhost:4096/auth/connect/abc");
        assert!(q.len() > 10);
        assert!(q[0].chars().all(|c| c == '█' || c == '▀'));
    }
}
