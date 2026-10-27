//! The session list dialog (`component/dialog-session-list.tsx`): sessions of the current project
//! directory or of all projects (`ctrl+a`, kept in the keyed store `session-list`), searched on the
//! server 150 ms after the filter settles, with the local list shown meanwhile.

use super::{App, Route};
use crate::api::types::SessionInfo;
use crate::ui::dialog::{Action, Dialog, Opt, Select, Size};
use crate::ui::text::{bold, fg, seg};
use crate::util::locale;
use serde_json::json;
use std::time::Duration;

const STORAGE: &str = "session-list";
const DEBOUNCE_MS: u64 = 150;

/// A finished server search.
#[derive(Clone, Debug, Default)]
pub struct SearchResult {
    pub query: String,
    pub all: bool,
    pub sessions: Vec<SessionInfo>,
    pub error: bool,
}

/// The debounced search (`createDebouncedSignal` + `createResource`).
#[derive(Clone, Debug, Default)]
pub struct SessionSearch {
    /// The committed (debounced) query.
    pub query: String,
    pub loading: bool,
    /// Bumped per request; only the latest one lands.
    pub seq: u64,
    /// The latest result, kept while a newer search runs.
    pub latest: Option<SearchResult>,
}

/// `path.relative(from, to)` for absolute POSIX paths.
fn relative(from: &str, to: &str) -> String {
    let a: Vec<&str> = from.split('/').filter(|s| !s.is_empty()).collect();
    let b: Vec<&str> = to.split('/').filter(|s| !s.is_empty()).collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<&str> = vec![".."; a.len() - common];
    parts.extend(&b[common..]);
    parts.join("/")
}

fn basename(p: &str) -> &str {
    p.trim_end_matches('/').rsplit('/').next().unwrap_or(p)
}

impl App {
    /// `allProjects`: stored, else on unless sessions tabs are scoped to the directory.
    pub fn session_list_all(&self) -> bool {
        self.keyed.get(STORAGE).and_then(|v| v.get("allProjects").and_then(|a| a.as_bool())).unwrap_or(self.config.tabs_scope() != "cwd")
    }

    /// `pickerLocation`: the viewed session's folder, else the current location.
    fn session_picker_dir(&self) -> String {
        match &self.route {
            Route::Session(id) => self.store.session(id).map(|s| s.location.directory.clone()).filter(|d| !d.is_empty()).unwrap_or_else(|| self.location_dir()),
            _ => self.location_dir(),
        }
    }

    fn session_picker_info(&self) -> Option<crate::api::types::LocationInfo> {
        self.store.locations.get(&self.session_picker_dir()).and_then(|l| l.info.clone())
    }

    /// `projectName`: the project's name, else the basename of its canonical path.
    fn session_project_name(&self) -> String {
        let Some(info) = self.session_picker_info() else { return String::new() };
        if let Some(name) = self.project_names.get(&info.project.id).filter(|n| !n.is_empty()) {
            return name.clone();
        }
        let canonical = if info.project.canonical.is_empty() { info.directory.clone() } else { info.project.canonical.clone() };
        if canonical == "/" {
            return String::new();
        }
        basename(&canonical).to_string()
    }

    pub fn open_session_list(&mut self) {
        let mut sel = Select::new("Sessions", vec![]);
        sel.current = self.session_id().map(|s| s.to_string());
        sel.skip_filter = true;
        let tabs = self.config.tabs_enabled();
        sel.actions = vec![
            Action::new("session.pin.toggle", "pin/unpin").hidden(tabs),
            Action::new("session.delete", "delete"),
            Action::new("session.rename", "rename"),
            Action::key("ctrl+a"),
        ];
        self.session_search = SessionSearch { seq: self.session_search.seq, ..Default::default() };
        self.open_dialog(Dialog::select("session", sel, Size::Large));
        self.search_sessions(String::new());
        if self.project_names.is_empty() {
            let c = self.client.clone();
            self.spawn(async move { c.projects().await }, |app, r| {
                if let Ok(list) = r {
                    app.remember_project_names(&list);
                    app.refresh_session_list();
                }
            });
        }
    }

    /// Project names by ID, from a project listing.
    pub fn remember_project_names(&mut self, projects: &[serde_json::Value]) {
        for p in projects {
            if let Some(id) = p.get("id").and_then(|i| i.as_str()) {
                let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                self.project_names.insert(id.to_string(), name);
            }
        }
    }

    /// The list for the current filter and search state, and the empty-state message.
    pub(super) fn refresh_session_list(&mut self) {
        let Some(d) = self.dialogs.iter().rev().find(|d| d.purpose == "session") else { return };
        let armed = d.data.as_str().map(|s| s.to_string());
        let filter = match &d.body {
            crate::ui::dialog::Body::Select(s) => s.filter.trim().to_string(),
            _ => return,
        };
        let all = self.session_list_all();
        let info = self.session_picker_info();
        let search = &self.session_search;
        // `localSessions`: the scope, then a title match.
        let needle = filter.to_lowercase();
        let local: Vec<SessionInfo> = self
            .store
            .session_list()
            .into_iter()
            .filter(|s| !s.id.starts_with("dummy"))
            .filter(|s| all || info.as_ref().is_some_and(|i| s.project_id == i.project.id && s.location.directory == i.directory))
            .filter(|s| needle.is_empty() || (s.parent_id.is_none() && s.display_title().to_lowercase().contains(&needle)))
            .cloned()
            .collect();
        let latest = search.latest.as_ref().map(|r| r.sessions.clone());
        let sessions = if filter != search.query {
            latest.unwrap_or(local)
        } else if search.loading {
            latest.unwrap_or_default()
        } else {
            match &search.latest {
                Some(r) if r.query == filter && r.all == all && !r.error => r.sessions.clone(),
                _ => local,
            }
        };
        let (message, error) = if filter != search.query || search.loading {
            (if filter.is_empty() { "Loading sessions…" } else { "Searching sessions…" }, false)
        } else if search.latest.as_ref().is_some_and(|r| r.query == filter && r.error) {
            (if filter.is_empty() { "Could not load sessions." } else { "Could not search sessions. Change the search to try again." }, true)
        } else {
            (if filter.is_empty() { "No sessions available" } else { "No sessions found" }, false)
        };
        let opts = self.session_options(&sessions, armed.as_deref());
        let t = self.theme().dialog().clone();
        let name = self.session_project_name();
        let tabs = self.config.tabs_enabled();
        let slots = self.session_slots();
        let first = self.keybinds.shortcut("session.quick_switch.1");
        let last = self.keybinds.shortcut("session.quick_switch.9");
        let Some(sel) = self.dialogs.iter_mut().rev().find(|d| d.purpose == "session").and_then(|d| d.sel()) else { return };
        let selected = sel.selected_value();
        sel.options = opts;
        sel.title_suffix = (!all && !name.is_empty()).then(|| format!(" for {name}"));
        sel.empty = Some(message.into());
        sel.no_match = Some(message.into());
        sel.empty_color = error.then_some(t.text.feedback.error.base);
        sel.hints.clear();
        if !tabs && !slots.is_empty() && !first.is_empty() && !last.is_empty() {
            let prefix = &first[..first.len() - 1];
            let range = if first.ends_with('1') && last == format!("{prefix}9") { format!("{prefix}1-9") } else { format!("{first} through {last}") };
            sel.hints.push(("switch".into(), range));
        }
        sel.hints_right = vec![((if all { "current directory" } else { "all projects" }).into(), "ctrl+a".into())];
        match selected.and_then(|v| sel.visible().iter().position(|i| sel.options[*i].value == v)) {
            Some(i) => sel.selected = i,
            None => {
                sel.selected = 0;
                sel.focus_current_option();
            }
        }
        self.dirty = true;
    }

    /// Pinned root sessions, the first 9 being the quick-switch slots.
    fn session_slots(&self) -> Vec<String> {
        self.pins.pinned.iter().filter(|p| self.store.sessions.get(*p).is_some_and(|s| s.parent_id.is_none())).take(9).cloned().collect()
    }

    fn session_options(&self, sessions: &[SessionInfo], armed: Option<&str>) -> Vec<Opt> {
        let t = self.theme().dialog().clone();
        let tabs = self.config.tabs_enabled();
        let today = locale::date_string(locale::now_ms());
        let info = self.session_picker_info();
        let roots: Vec<&SessionInfo> = sessions.iter().filter(|s| s.parent_id.is_none()).collect();
        let pinned: Vec<String> = if tabs { vec![] } else { self.pins.pinned.iter().filter(|p| roots.iter().any(|s| &s.id == *p)).cloned().collect() };
        let slots = if tabs { vec![] } else { self.session_slots() };
        let key = self.keybinds.shortcut("session.delete");
        let option = |s: &SessionInfo, category: String| -> Opt {
            let deleting = armed == Some(s.id.as_str());
            let title = if deleting { format!("Press {key} again to confirm") } else { s.display_title() };
            let mut o = Opt::new(title, s.id.clone()).cat(category);
            // A session rooted outside its project (a worktree) shows the folder's name.
            let dir = &s.location.directory;
            let root = match s.subpath.as_deref().filter(|p| !p.is_empty()) {
                Some(sub) => {
                    let mut r = dir.trim_end_matches('/').to_string();
                    for _ in sub.split('/') {
                        r = r.rsplit_once('/').map(|(a, _)| if a.is_empty() { "/".to_string() } else { a.to_string() }).unwrap_or(r);
                    }
                    r
                }
                None => dir.clone(),
            };
            let canonical = info.as_ref().filter(|i| i.project.id == s.project_id && !i.project.canonical.is_empty()).map(|i| i.project.canonical.clone()).unwrap_or_else(|| root.clone());
            let rel = relative(&canonical, &root);
            if rel.starts_with("..") {
                o.footer = Some(locale::truncate(basename(&rel), 25));
            }
            if deleting {
                o.bg = Some(t.background.action.destructive.focused);
                o.fg = Some(t.text.action.destructive.focused);
            }
            let status = self.tab_status(&s.id);
            o.gutter = match status.attention {
                Some(a) => Some(seg(if a == "permission" { "!" } else { "?" }, bold(fg(t.text.feedback.warning.base)))),
                None if self.store.family_of(&s.id).iter().chain(std::iter::once(&s.id)).any(|id| self.store.is_running(id)) => {
                    Some(seg(crate::ui::text::SPINNER_GLYPH.to_string(), fg(t.text.muted)))
                }
                None => slots.iter().position(|x| *x == s.id).map(|i| seg((i + 1).to_string(), fg(t.accent(200)))),
            };
            o
        };
        let mut out: Vec<Opt> = pinned.iter().filter_map(|id| roots.iter().find(|s| &s.id == id)).map(|s| option(s, "Pinned".into())).collect();
        for s in roots.iter().filter(|s| !pinned.contains(&s.id)) {
            let date = locale::date_string(s.time.updated);
            out.push(option(s, if date == today { "Today".into() } else { date }));
        }
        out
    }

    /// Debounce a filter change; the search runs once the filter has stayed the same for 150 ms.
    pub(super) fn session_filter_changed(&mut self, filter: &str) {
        self.refresh_session_list();
        let q = filter.trim().to_string();
        self.spawn(
            async move {
                tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS)).await;
                Ok(q)
            },
            |app, r| {
                let Ok(q) = r else { return };
                let current = app.dialogs.iter().rev().find(|d| d.purpose == "session").and_then(|d| match &d.body {
                    crate::ui::dialog::Body::Select(s) => Some(s.filter.trim().to_string()),
                    _ => None,
                });
                if current.as_deref() == Some(q.as_str()) && (q != app.session_search.query || app.session_search.latest.is_none() && !app.session_search.loading) {
                    app.search_sessions(q);
                }
            },
        );
    }

    /// `client.api.session.list` for the scope and query (50 newest root sessions).
    pub(super) fn search_sessions(&mut self, query: String) {
        let all = self.session_list_all();
        self.session_search.seq += 1;
        self.session_search.query = query.clone();
        self.session_search.loading = true;
        let seq = self.session_search.seq;
        let dir = self.session_picker_dir();
        let info = self.session_picker_info();
        let c = self.client.clone();
        let q = query.clone();
        self.spawn(
            async move {
                let info = match info {
                    Some(i) => i,
                    None => c.location_get(Some(&dir)).await?,
                };
                let mut params: Vec<(&str, String)> = Vec::new();
                if !all {
                    if info.project.id == "global" {
                        params.push(("directory", info.directory.clone()));
                    } else {
                        params.push(("project", info.project.id.clone()));
                        params.push(("subpath", relative(&info.project.directory, &info.directory)));
                    }
                }
                if !q.is_empty() {
                    params.push(("search", q.clone()));
                }
                params.push(("limit", "50".into()));
                params.push(("order", "desc".into()));
                params.push(("parentID", "null".into()));
                Ok(c.session_list(&params).await?.data)
            },
            move |app, r| {
                if app.session_search.seq != seq {
                    return;
                }
                app.session_search.loading = false;
                app.session_search.latest = Some(match r {
                    Ok(sessions) => SearchResult { query, all, sessions, error: false },
                    Err(_) => SearchResult { query, all, sessions: vec![], error: true },
                });
                app.refresh_session_list();
            },
        );
        self.refresh_session_list();
    }

    /// `ctrl+a`: toggle between the current directory's sessions and all projects.
    pub(super) fn toggle_session_scope(&mut self) {
        let all = !self.session_list_all();
        self.keyed.update(STORAGE, |v| {
            if !v.is_object() {
                *v = json!({});
            }
            v["allProjects"] = json!(all);
        });
        let q = self.session_search.query.clone();
        self.search_sessions(q);
    }

    /// A deleted session leaves the search results too.
    pub(super) fn session_list_removed(&mut self, id: &str) {
        if let Some(r) = self.session_search.latest.as_mut() {
            r.sessions.retain(|s| s.id != id);
        }
        self.refresh_session_list();
    }
}

#[cfg(test)]
mod tests {
    use super::relative;

    #[test]
    fn relative_paths() {
        assert_eq!(relative("/a/b", "/a/b"), "");
        assert_eq!(relative("/a/b", "/a/b/c/d"), "c/d");
        assert_eq!(relative("/a/b", "/a/x"), "../x");
        assert_eq!(relative("/a/b/c", "/d"), "../../../d");
    }
}
