//! The plugin manager (`feature-plugins/system/plugins.tsx`) and the plugin error details
//! (`component/dialog-error-details.tsx`).
//!
//! TUI entries are ocd's built-in features under upstream's built-in plugin IDs; they can be
//! disabled and enabled for the running TUI, like upstream's `deactivate`/`activate`. Server
//! entries come from `plugin.list`, with update checks and package updates. ocd loads no external
//! TUI plugins.

use super::{App, Route};
use crate::ui::dialog::{Action, Body, DIALOG_INSET, Dialog, Footer, FooterItem, Opt, Select, Size, TextDialog};
use crate::ui::text::*;
use crate::util::locale;
use serde_json::{Value, json};

/// ocd's built-in features, as upstream's built-in TUI plugins (without the plugin manager itself,
/// and without `opencode.latex`, `opencode.merman` and the experimental `opencode.sidebar.todo`,
/// which ocd does not have).
pub const BUILTINS: &[&str] = &[
    "opencode.btw",
    "opencode.diffs",
    "opencode.home.footer",
    "opencode.notifications",
    "opencode.prompt.footer",
    "opencode.sidebar.context",
    "opencode.sidebar.footer",
    "opencode.sidebar.mcp",
    "opencode.stats",
];

/// Dialog state (`PluginsDialog` signals).
#[derive(Clone, Debug, Default)]
pub struct PluginsState {
    /// `plugin.list` for the location; `None` while loading.
    pub server: Option<Vec<Value>>,
    pub show_internal: bool,
    pub checking: bool,
    /// Keys of server plugins being updated.
    pub pending: Vec<String>,
    pub focused: Option<String>,
}

#[derive(Clone, Debug)]
enum Entry {
    Tui { key: String, id: String, active: bool },
    Server { key: String, internal: bool, plugin: Value },
}

impl Entry {
    fn key(&self) -> &str {
        match self {
            Entry::Tui { key, .. } | Entry::Server { key, .. } => key,
        }
    }
    fn internal(&self) -> bool {
        match self {
            Entry::Tui { .. } => true,
            Entry::Server { internal, .. } => *internal,
        }
    }
    fn status(&self) -> String {
        match self {
            Entry::Tui { active, .. } => (if *active { "active" } else { "inactive" }).into(),
            Entry::Server { plugin, .. } => plugin.pointer("/state/status").and_then(|s| s.as_str()).unwrap_or("active").into(),
        }
    }
    fn package(&self) -> Option<&Value> {
        match self {
            Entry::Server { plugin, .. } => plugin.get("source").filter(|s| s.get("type").and_then(|t| t.as_str()) == Some("package")),
            _ => None,
        }
    }
    fn outdated(&self) -> bool {
        self.package().and_then(|p| p.get("outdated")) == Some(&Value::Bool(true))
    }
    fn error(&self) -> Option<String> {
        match self {
            Entry::Server { plugin, .. } if self.status() == "failed" => Some(plugin.pointer("/state/error").and_then(|e| e.as_str()).unwrap_or("").to_string()),
            _ => None,
        }
    }
    fn error_ref(&self) -> Option<String> {
        match self {
            Entry::Server { plugin, .. } if self.status() == "failed" => plugin.pointer("/state/ref").and_then(|e| e.as_str()).map(str::to_string),
            _ => None,
        }
    }
}

/// `displayVersion`: a commit hash shows its first 7 characters.
fn display_version(v: &str) -> String {
    let hash = (v.len() == 40 || v.len() == 64) && v.bytes().all(|b| b.is_ascii_hexdigit());
    if hash { v[..7].to_string() } else { v.to_string() }
}

/// `errorDetails`: the copied text and the investigation prompt.
pub fn error_details(title: &str, error: &str, context: Option<&str>, reference: Option<&str>) -> (String, String) {
    let mut parts = vec![title.to_string()];
    if let Some(c) = context {
        parts.push(c.to_string());
    }
    parts.push(format!("Error: {error}"));
    if let Some(r) = reference {
        parts.push(format!("Reference: {r}"));
    }
    let text = parts.join("\n");
    let mut prompt = vec!["Investigate why this OpenCode component failed in the current project.".to_string(), text.clone()];
    if let Some(r) = reference {
        prompt.push(format!(
            "Find the server log entry matching reference {r} and inspect its original cause. If the server logs are not accessible from this session, say so and ask for the matching log entry; do not infer the cause from the reference alone."
        ));
    }
    prompt.push("Inspect the relevant project and global OpenCode configuration, startup or loading behavior, required environment variables or credentials, dependencies, and logs. Identify the root cause and recommend a fix. Do not expose credentials.".to_string());
    (text, prompt.join("\n\n"))
}

impl App {
    /// Whether a built-in feature is enabled (all are, until disabled in `/plugins`).
    pub fn builtin_active(&self, id: &str) -> bool {
        !self.disabled_builtins.contains(id)
    }

    fn plugin_source(&self, plugin: &Value) -> String {
        let source = plugin.get("source").cloned().unwrap_or(Value::Null);
        match source.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "package" => source.get("target").and_then(|t| t.as_str()).unwrap_or("").to_string(),
            "local" => crate::ui::transcript::format_path(source.get("path").and_then(|t| t.as_str()).unwrap_or(""), Some(&self.location_dir())),
            other => other.to_string(),
        }
    }

    fn entry_label(&self, e: &Entry) -> String {
        match e {
            Entry::Tui { id, .. } => id.clone(),
            Entry::Server { plugin, .. } => plugin.get("id").and_then(|i| i.as_str()).map(str::to_string).unwrap_or_else(|| self.plugin_source(plugin)),
        }
    }

    fn plugin_entries(&self) -> Vec<Entry> {
        let mut tui: Vec<Entry> = BUILTINS.iter().map(|id| Entry::Tui { key: format!("tui:{id}"), id: id.to_string(), active: self.builtin_active(id) }).collect();
        tui.sort_by_key(|a| self.entry_label(a).to_lowercase());
        let mut server: Vec<Entry> = self
            .plugins
            .server
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|p| {
                let key = format!("server:{}", p.get("id").and_then(|i| i.as_str()).map(str::to_string).unwrap_or_else(|| self.plugin_source(&p)));
                let internal = p.pointer("/source/type").and_then(|t| t.as_str()) == Some("builtin");
                Entry::Server { key, internal, plugin: p }
            })
            .collect();
        server.sort_by_key(|a| self.entry_label(a).to_lowercase());
        tui.extend(server);
        tui
    }

    fn visible_plugin_entries(&self) -> Vec<Entry> {
        let show = self.plugins.show_internal;
        self.plugin_entries().into_iter().filter(|e| show || !e.internal() || e.status() == "failed").collect()
    }

    fn plugin_updating(&self, e: &Entry) -> bool {
        self.plugins.pending.iter().any(|k| k == e.key()) || e.package().and_then(|p| p.get("updating")) == Some(&Value::Bool(true))
    }

    pub fn open_plugins(&mut self) {
        self.plugins = Default::default();
        let mut sel = Select::new("Plugins", vec![]);
        sel.empty = Some("No items available".into());
        sel.actions = vec![Action::key("ctrl+a")];
        self.open_dialog(Dialog::select("plugins", sel, Size::Large));
        self.refresh_plugins_dialog();
        self.load_plugins();
    }

    fn load_plugins(&mut self) {
        let c = self.client.clone();
        let dir = self.location_dir();
        self.spawn(
            async move { c.plugin_list(Some(&dir)).await },
            |app, r| {
                app.plugins.server = Some(r.unwrap_or_default());
                app.refresh_plugins_dialog();
            },
        );
    }

    /// Options, footer hints and actions for the focused entry.
    pub(super) fn refresh_plugins_dialog(&mut self) {
        if !self.dialogs.iter().any(|d| d.purpose == "plugins") {
            return;
        }
        let t = self.theme().dialog().clone();
        let visible = self.visible_plugin_entries();
        // The focused entry stays; else the first TUI entry, else the first one.
        if !visible.iter().any(|e| Some(e.key()) == self.plugins.focused.as_deref()) {
            self.plugins.focused = visible.iter().find(|e| matches!(e, Entry::Tui { .. })).or(visible.first()).map(|e| e.key().to_string());
        }
        let opts: Vec<Opt> = visible
            .iter()
            .map(|e| {
                let mut o = Opt::new(self.entry_label(e), e.key().to_string()).cat(if matches!(e, Entry::Tui { .. }) { "TUI" } else { "Server" });
                o.search_text = Some(match e {
                    Entry::Tui { id, .. } => id.clone(),
                    Entry::Server { plugin, .. } => self.plugin_source(plugin),
                });
                let status = e.status();
                let mut details: Vec<String> = Vec::new();
                if status != "active" {
                    details.push(status.clone());
                }
                if matches!(e, Entry::Server { plugin, .. } if plugin.pointer("/source/type").and_then(|t| t.as_str()) == Some("local")) {
                    details.push("local".into());
                }
                if let Some(v) = e.package().and_then(|p| p.get("version")).and_then(|v| v.as_str()) {
                    details.push(display_version(v));
                }
                if e.outdated() {
                    details.push("update available".into());
                }
                let updating = self.plugin_updating(e);
                o.footer = if updating { Some("updating".into()) } else { (!details.is_empty()).then(|| details.join(", ")) };
                o.footer_color = Some(if status == "failed" {
                    t.text.feedback.error.base
                } else if e.outdated() {
                    t.text.feedback.info.base
                } else {
                    t.text.muted
                });
                o.gutter = if updating {
                    Some(seg(SPINNER_GLYPH.to_string(), fg(t.text.muted)))
                } else if status == "failed" {
                    Some(seg("x", fg(t.text.feedback.error.base)))
                } else {
                    None
                };
                o
            })
            .collect();
        let focused = self.plugin_entries().into_iter().find(|e| Some(e.key()) == self.plugins.focused.as_deref());
        let focused_tui = focused.as_ref().filter(|e| matches!(e, Entry::Tui { .. }));
        let toggle_title = match focused_tui {
            Some(Entry::Tui { active: true, .. }) => "disable",
            Some(_) => "enable",
            None => "toggle",
        };
        let has_package = self.plugin_entries().iter().any(|e| e.package().is_some());
        let updatable = focused.as_ref().is_some_and(|e| e.outdated() && !self.plugin_updating(e));
        let error_hint = focused.as_ref().is_some_and(|e| e.error().is_some()) && focused_tui.is_none();
        let checking = self.plugins.checking;
        let show = self.plugins.show_internal;
        let focus_key = self.plugins.focused.clone();
        let Some(sel) = self.dialogs.iter_mut().rev().find(|d| d.purpose == "plugins").and_then(|d| d.sel()) else { return };
        sel.options = opts;
        sel.hints = vec![((if show { "hide internal" } else { "show internal" }).into(), "ctrl+a".into())];
        sel.hints_right = if error_hint { vec![("view error".into(), "enter".into())] } else { vec![] };
        let mut check = Action::new("dialog.plugins.check", if checking { "checking for updates" } else { "check for updates" }).dialog_level().hidden(!has_package);
        check.disabled = checking;
        sel.actions = vec![
            check,
            Action::new("dialog.plugins.error", "view error").hidden(true),
            Action::new("plugins.toggle", toggle_title).right().hidden(focused_tui.is_none()),
            Action::new("dialog.plugins.update", "update").right().hidden(!updatable),
            Action::key("ctrl+a"),
        ];
        if let Some(k) = focus_key
            && let Some(i) = sel.visible().iter().position(|i| sel.options[*i].value == k)
        {
            sel.selected = i;
        }
        self.dirty = true;
    }

    pub(super) fn plugins_moved(&mut self, value: &str) {
        self.plugins.focused = Some(value.to_string());
        self.refresh_plugins_dialog();
    }

    pub(super) fn plugins_toggle_internal(&mut self) {
        self.plugins.show_internal = !self.plugins.show_internal;
        self.refresh_plugins_dialog();
    }

    /// Enter on a row: toggle a TUI entry, or open a failed plugin's error.
    pub(super) fn plugins_select(&mut self, value: &str) {
        let Some(e) = self.plugin_entries().into_iter().find(|e| e.key() == value) else { return };
        match &e {
            Entry::Tui { id, .. } => self.plugins_toggle(id),
            Entry::Server { .. } if e.error().is_some() => self.open_plugin_error(&e),
            _ => {}
        }
    }

    fn plugins_toggle(&mut self, id: &str) {
        if !self.disabled_builtins.remove(id) {
            self.disabled_builtins.insert(id.to_string());
        }
        self.refresh_plugins_dialog();
    }

    pub(super) fn plugins_action(&mut self, action: &str, value: Option<String>) {
        match action {
            "plugins.toggle" => {
                if let Some(v) = value {
                    self.plugins_select(&v);
                }
            }
            "dialog.plugins.check" => {
                if self.plugins.checking {
                    return;
                }
                self.plugins.checking = true;
                self.refresh_plugins_dialog();
                let c = self.client.clone();
                let dir = self.location_dir();
                self.spawn(
                    async move { c.plugin_check(Some(&dir)).await },
                    |app, r| {
                        app.plugins.checking = false;
                        match r {
                            Ok(list) => app.plugins.server = Some(list),
                            Err(e) => app.toast(super::Toast::new("error", crate::api::error_message(&e))),
                        }
                        app.refresh_plugins_dialog();
                    },
                );
            }
            "dialog.plugins.update" => {
                let Some(e) = value.and_then(|v| self.plugin_entries().into_iter().find(|e| e.key() == v)) else { return };
                if !e.outdated() || self.plugin_updating(&e) {
                    return;
                }
                let Some(target) = e.package().and_then(|p| p.get("target")).and_then(|t| t.as_str()).map(str::to_string) else { return };
                let key = e.key().to_string();
                self.plugins.pending.push(key.clone());
                self.refresh_plugins_dialog();
                let c = self.client.clone();
                let dir = self.location_dir();
                self.spawn(
                    async move { c.plugin_update(&[target], Some(&dir)).await },
                    move |app, r| {
                        app.plugins.pending.retain(|k| *k != key);
                        match r {
                            Ok(()) => app.load_plugins(),
                            Err(e) => app.toast(super::Toast::new("error", crate::api::error_message(&e))),
                        }
                        app.refresh_plugins_dialog();
                    },
                );
            }
            "dialog.plugins.error" => {
                if let Some(e) = value.and_then(|v| self.plugin_entries().into_iter().find(|e| e.key() == v))
                    && e.error().is_some()
                {
                    self.open_plugin_error(&e);
                }
            }
            "key:ctrl+a" => self.plugins_toggle_internal(),
            _ => {}
        }
    }

    /// `plugin.updated`: refetch while the dialog is open.
    pub(super) fn plugins_updated(&mut self) {
        if self.dialogs.iter().any(|d| d.purpose == "plugins") {
            self.load_plugins();
        }
    }

    fn open_plugin_error(&mut self, e: &Entry) {
        let runtime = if matches!(e, Entry::Tui { .. }) { "tui" } else { "server" };
        let title = format!("{} plugin error", if runtime == "tui" { "TUI" } else { "Server" });
        let source = match e {
            Entry::Tui { id, .. } => id.clone(),
            Entry::Server { plugin, .. } => self.plugin_source(plugin),
        };
        let error = e.error().filter(|x| !x.is_empty()).unwrap_or_else(|| "Unknown plugin error".into());
        let context = format!("Plugin: {}\nStatus: failed\nRuntime: {runtime}\nSource: {source}", self.entry_label(e));
        let mut d = Dialog::text("plugin-error", TextDialog::new(title.clone(), vec![]));
        d.size = Size::Large;
        d.data = json!({ "title": title, "source": source, "error": error, "context": context, "ref": e.error_ref(), "copied": false });
        // Over the list: `esc` goes back to it.
        self.dialogs.push(d);
        self.dirty = true;
        self.refresh_error_details();
    }

    /// `DialogErrorDetails`: the title, the source path, the error, and its reference.
    pub(super) fn refresh_error_details(&mut self) {
        let Some(d) = self.dialogs.last().filter(|d| d.purpose == "plugin-error") else { return };
        let data = d.data.clone();
        let scroll = match &d.body {
            Body::Text(td) => td.scroll,
            _ => 0,
        };
        let t = self.theme().dialog().clone();
        let width = (Size::Large.width().min(self.size.0.saturating_sub(2)) as usize).saturating_sub(DIALOG_INSET * 2);
        let get = |k: &str| data.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let mut lines: Vec<SLine> = wrap(&[seg(get("error"), fg(t.text.base))], width.max(1), Wrap::Word).into_iter().map(SLine::new).collect();
        let body_rows = lines.len().min(20);
        if let Some(r) = data.get("ref").and_then(|r| r.as_str()) {
            lines.push(SLine::blank());
            lines.push(SLine::new(vec![seg(format!("Reference: {r}"), fg(t.text.muted))]));
        }
        let copied = data.get("copied") == Some(&Value::Bool(true));
        let mut td = TextDialog::new(get("title"), lines);
        td.header_gap = 2;
        td.under_header = vec![SLine::new(vec![seg(locale::truncate_file_path(&get("source"), width), fg(t.text.muted))])];
        td.height = Some(body_rows + if data.get("ref").is_some_and(|r| r.is_string()) { 2 } else { 0 });
        td.scroll = scroll;
        td.footer = Some(Footer {
            secondary: vec![
                FooterItem::new("investigate", "i"),
                if copied { FooterItem::new("✓ copied", "").color(t.text.feedback.success.base) } else { FooterItem::new("copy details", "c") },
            ],
            primary: vec![FooterItem::new("scroll", "↑/↓")],
            ..Default::default()
        });
        if let Some(d) = self.dialogs.last_mut() {
            d.body = Body::Text(Box::new(td));
        }
    }

    /// `c` copies the details; `i` starts a new prompt investigating the error.
    pub(super) fn error_details_key(&mut self, key: &str) {
        let Some(d) = self.dialogs.last().filter(|d| d.purpose == "plugin-error") else { return };
        let get = |k: &str| d.data.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let (text, prompt) = error_details(&get("title").unwrap_or_default(), &get("error").unwrap_or_default(), get("context").as_deref(), get("ref").as_deref());
        match key {
            "c" => {
                crate::clipboard::write(&text);
                if let Some(d) = self.dialogs.last_mut() {
                    d.data["copied"] = json!(true);
                }
                self.refresh_error_details();
            }
            "i" => {
                self.dialogs.clear();
                self.navigate(Route::Home);
                let mut p = crate::state::PromptInfo::empty();
                p.text = prompt;
                self.editor.set(&p);
                self.dirty = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_details() {
        assert_eq!(display_version("1.2.3"), "1.2.3");
        assert_eq!(display_version(&"a".repeat(40)), "aaaaaaa");
        let (text, prompt) = error_details("Server plugin error", "boom", Some("Plugin: x"), Some("ref_1"));
        assert_eq!(text, "Server plugin error\nPlugin: x\nError: boom\nReference: ref_1");
        assert!(prompt.starts_with("Investigate why this OpenCode component failed in the current project.\n\nServer plugin error"));
        assert!(prompt.contains("matching reference ref_1"));
    }
}
